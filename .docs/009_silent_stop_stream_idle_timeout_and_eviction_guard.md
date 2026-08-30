# 009 — Silent agent stops: stream idle timeout + busy-thread eviction guard

Fix commit: `66976d6531` (resolves `.issues/003_native_agent_stream_idle_hang.md`,
removed per convention; also closes the "SSE idle timeout" P2 follow-up listed in
`.docs/006`).

## Incident (2026-08-23)

User report: 20+ sudden silent agent stops. Example: `sed -n '753,815p' …/gzero_loop.rs`
returns, then nothing — no continuation, no auto_prompt trigger, no error. Claude-agent
sessions showed `[Request interrupted by user for tool use]` after long cargo builds
(separate root cause, already diagnosed in `.plans/014`: quota exhaustion /
orchestrator abort races / DNS drops — none are human ESC presses).

## Forensics — why the watchdog "never fired"

The `72883e1538` watchdog raise (10→30 min) was NOT the cause. Across
`Zed.log` + `Zed.log.old` there are **zero watchdog decision lines**. The
watchdog was architecturally unreachable for these stops:

1. **Premature "normal" stops** — the thread stops (EndTurn), the watchdog is
   cancelled ("thread stopped normally") before it could ever fire.
2. **Evicted views** — the retained-threads hard cap (`.docs/006` backstop)
   evicted the oldest thread *blindly* when all 8 were busy. Dropping a
   generating thread's `ConversationView` cancels its `_watchdog_task`,
   `_auto_prompt_task`, and the `AcpThreadEvent` subscription — silently
   (dropped tasks log nothing). Evidence: 32 evictions in the heavy window;
   23 watchdogs started, only 13 cancel lines → ~10 died with their views;
   2 thread stops had no `on_thread_stopped` entry point at all.
3. **Hung streams with no timeout at all** — `.issues/003` evidence: 10/11
   watchdog halts were "tool call completed → provider accepts connection →
   zero SSE events forever". Also hits first-event waits (TTFT) at 350–413k
   token contexts — **not only after tool calls**.

## What landed (`66976d6531`)

### 1. Stream idle timeout in `run_turn_internal`

`crates/agent/src/thread.rs` — the event-loop `select!` now races against a
GPUI timer, recreated each iteration:

- New setting `agent.stream_idle_timeout_secs` (default **120**, `0` disables;
  `crates/settings_content/src/agent.rs`, `crates/agent_settings/src/agent_settings.rs`).
  120s covers time-to-first-token on very large contexts while capping a
  wedged stream at ~2 min instead of forever.
- Timeout fires **only when no tool results are pending** (snapshot
  `has_pending_tool_results` before the `select!`, because its tool branch
  mutably borrows `tool_results`): a silent stream is legitimate while tools
  run — this is what makes long cargo builds safe.
- On fire: `log::warn!` + `LanguageModelCompletionError::Other("stream idle
  timeout: …")` → the existing `retry_strategy_for(Other)` path (2 retries,
  5s fixed delay). Early tool results are still processed before the retry,
  so nothing is lost. After retries exhaust: `send_error` → visible error
  entry in the thread + "Agent stopped due to an error" notification, and
  agent_ui's error arm can chain auto_prompt.
  **Follow-up (2026-08-28, `1bd6fb6383`):** user report showed the provider hang outlasting
  that budget — the turn died with a visible `stream idle timeout` error while
  a later *manual* Retry click (fresh turn, reset budget) succeeded. Idle
  timeouts now carry a typed payload (`Thread::StreamIdleTimeout`, no string
  matching) and `retry_strategy_for` grants them `Fixed { delay: 30s,
  max_attempts: 4 }` (MAX_RETRY_ATTEMPTS) instead of the `Other(..)` fallback:
  ~12 min of automatic recovery (5 attempts × 120s idle + 4 × 30s delays)
  before the error surfaces, mirroring what the manual retry button did by
  hand. Applies to the main turn loop and `stream_compaction` (shared retry
  path). Regression tests:
  `test_stream_idle_timeout_recovers_mid_retry_cascade` (hang, hang, recover →
  turn completes with no error), plus the two exhaustion tests updated to the
  5-attempt cascade.
- Pending tool results + hung stream: the timer re-arms every iteration until
  tools finish, then the timeout applies. No false positives on long tools.

### 2. Busy-thread eviction guard

`crates/agent_ui/src/agent_panel.rs`:

- New `AgentPanel::is_thread_evictable(&self, view, cx)` — the exact
  predicate the soft `cleanup_retained_threads` filter used inline (Idle +
  loadable + not loading contents + not generating title/summary + no queued
  messages; intentionally still NOT guarding on `_auto_prompt_task`, per the
  contract pinned by `test_auto_prompt_task_thread_is_cleanable`).
- Soft cleanup now calls the shared helper.
- Hard-cap backstop (`insert_retained_thread`) now evicts the **oldest
  evictable** thread; when every retained thread is busy it logs a warning
  and **deliberately exceeds the cap** — the cap exists to bound idle-thread
  memory (`.docs/006`), not to kill active work. Generating views keep their
  watchdog/auto_prompt recovery chains alive.

## Verification

```
$ cargo test -p agent --lib thread::tests::test_stream_idle_timeout_recovers_hung_stream
  1 passed   # hung stream retries (3 attempts) then ends with the error, running_turn cleared
$ cargo test -p agent_ui --lib agent_panel::tests::test_hard_cap
  2 passed   # idle evicted first / cap exceeded when all busy
$ cargo test -p agent_ui --lib agent_panel::tests::          92/92
$ cargo test -p agent --lib thread::tests::                   38/38
$ cargo test -p agent_settings --lib test_stream_idle_timeout_secs_parsing  1/1
$ ./script/clippy -p agent -p agent_settings -p settings_content -p agent_ui
  clean (+ cargo machete clean)
```

The idle-timeout test drives the deterministic GPUI scheduler
(`advance_clock`), so the full retry cascade (~375s virtual time) runs in
0.29s wall time.

## Remaining known gaps (updated 2026-08-23, second pass)

- [x] `stream_compaction`'s event loop had the same no-timeout shape — FIXED:
  both the request-establishment `select!` and the event loop now race the
  same `agent.stream_idle_timeout_secs` timer (shared
  `stream_idle_timeout_future` helper); a hung compaction stream retries
  twice via the existing compaction-retry path, then the turn ends with a
  visible `stream idle timeout` error. Regression test:
  `test_compaction_stream_idle_timeout_recovers_hung_stream`.
- [x] Watchdog Claude coverage hole — FIXED: a Claude-agent thread with a
  non-Anthropic default model no longer skips the watchdog (the old
  `return None` would leave hung Claude threads with NO recovery path).
  It logs INFO and reasons with the configured default model; on provider
  failure the decision degrades to `Continue` (re-sleep), so there is no
  call-burn during healthy operation.
- [x] auto_prompt overflow gate 256k → 200k — DONE: `default_max_context_tokens()`
  is now 200_000 (threads were ballooning to 343–413k before the gate could
  fire at turn end); the hardcoded 256k fallback in `dispatch_action` now
  uses the shared default. Override remains
  `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS`.
- [x] `[agent_board]` poll-loop log spam — root-caused as stale binary: the
  running build (Aug 23 07:42) predates the unlogged-restart guard that
  already sits uncommitted in the tree (sibling agent, Aug 23 22:01). The
  spam was `realtime_nudge` → `force_refresh` → `start_poll` logging INFO
  unconditionally every ~2s. No action taken here — the sibling's fix in
  `crates/agent_board/src/runtime.rs` covers it; rebuilding/restarting picks
  it up.
- [ ] `tools::edit_file_tool::tests::test_streaming_authorize` fails
  deterministically — PRE-EXISTING from the Aug 19 upstream merge
  (`8823d2bcea`), verified failing at `HEAD~2` in an isolated worktree
  before any of the 003 fixes. Unrelated to this workstream; needs its own
  issue (agent-skills permission prompt offering "Always allow").
- [ ] Premature "normal" stops where auto_prompt's decide LLM concludes
  NoAction at 350k+ token contexts are a decision-quality problem, not a
  wiring bug — the 200k gate above shrinks those contexts going forward.
