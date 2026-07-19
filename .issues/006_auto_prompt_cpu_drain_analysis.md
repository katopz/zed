# Issue 006: Zed 1500% CPU after extended use — auto_prompt + retained_threads + zombie subprocess leak

## Status
- [x] Symptom reproduced (live process sampled, log scanned, subprocess tree inspected)
- [x] Root cause identified (multiple compounding causes — see below)
- [x] Fix proposed (see "Recommended fixes" — pick subset per priority)
- [x] P0 fixes landed (see `.docs/006_auto_prompt_cpu_drain_p0_fixes.md`)
- [x] P1 fix landed: zombie reaping in `util::command::darwin::Child::drop` (see `.docs/006_*`)
- [-] P1 investigation: MCP duplicate-spawn (deferred — needs live debug; guards look correct)
- [-] P1 investigation: action_log observer (no change needed — already per-thread, NOT global)
- [ ] P2 fixes (concurrent-stream cap, SSE idle timeout, background decision log)
- [ ] GOAT verified (live CPU measurement after P0+P1 land)

## Correction

Original issue 006 diagnosis contained two errors that the P1 investigation
surfaced:

1. **Finding #1 (zombies)** claimed the agent's MCP servers were not reaped
   by commit `05f20945eb`. **Wrong.** The agent path uses `util::process::Child`
   (reaped). The actual leak was in `util::command::darwin::Child` (used by
   LSP/debugger/SSH/REPL), whose `Drop` never called `waitpid`. Fixed in P1.

2. **Finding #7 (action_log cascade)** claimed every ConversationView
   subscribed to a shared global action_log. **Wrong.** `AcpThread.action_log`
   is per-thread; each ConversationView observes its own. No change needed.

## Symptom

After using Zed for a while (multiple agent sessions, auto_prompt enabled), CPU
usage climbs to ~1500% (15 cores fully pegged). `sample` of the live process
shows 16 `async-std/runtime` threads + 8 rayon workers + 3 PTY readers + many
unnamed worker threads. Activity Monitor shows the Zed process holding 1.6 GB
RSS and ~36% sustained CPU even when idle, with periodic spikes to many cores.

## Evidence (collected 2026-07-19 against running Zed Dev.app, PID 46430)

### 1. Zombie subprocess leak — Zed has 34 unreaped child processes

```
$ ps -A -o pid,ppid,stat,command | awk '$2 == 46430 {print $3}' | sort | uniq -c
  13 Ss       ← living children
  34 Z        ← ZOMBIE children (defunct)
```

PIDs `47890`, `48061`, `48086`, `48391..48422` are all `<defunct>`. These are
MCP servers / agent terminal tools / node helpers that exited but were never
`waitpid`-ed by the parent. Each zombie holds a PID slot and forces the kernel
to keep the process table entry. Under load this contributes to fork failures
and "Too many open files" (errno 24) seen in prior sessions.

Related: `.docs/004_watchdog_timer_reset_fix.md` and commit `8c5e2995cd
fix(remote_server): reap kernel subprocesses via util::process::Child` added
reaping for `remote_server` only. **The agent's MCP server subprocesses and
terminal tool subprocesses are NOT reaped.**

### 2. Duplicate MCP server processes — 4 unique servers × 2 instances each

```
48449 /Users/.../mcp-server-zai-web-reader/.../supergateway ... web_reader/mcp
48450 /Users/.../mcp-server-zai-vision/.../build/index.js                 ← stdio
48451 /Users/.../mcp-server-zai-zread/.../supergateway ... zread/mcp
48452 /Users/.../mcp-server-zai-web-search/.../supergateway ... web_search_prime/mcp
50561 /Users/.../mcp-server-zai-web-reader/.../supergateway ... web_reader/mcp   ← DUPLICATE
50587 /Users/.../mcp-server-zai-vision/.../build/index.js                      ← DUPLICATE
50609 /Users/.../mcp-server-zai-zread/.../supergateway ... zread/mcp             ← DUPLICATE
50621 /Users/.../mcp-server-zai-web-search/.../supergateway ... web_search_prime/mcp ← DUPLICATE
```

All have PPID 46430 (the Zed editor). Each `supergateway` node process runs an
HTTP long-poll + JSON-RPC dispatcher + OAuth token refresher. 8 living node
processes ≈ 8 × ~3 async-std pump threads = ~24 background threads just for
MCP transport.

Likely cause: workspace was reopened or `ContextServerStore::maintain_servers`
re-spawns on settings/worktree events without first killing the prior process
(see `crates/project/src/context_server_store.rs:run_server`). Combined with
the zombie-reaping bug (#1), even kill+respawn cycles leave the old PID around.

### 3. `/tmp/zed_auto_prompt/` has 7811 files (31 MB) — decision logging on by default

```
$ ls /tmp/zed_auto_prompt/ | wc -l
7811

$ ls -lt /tmp/zed_auto_prompt/ | head -3
... 1784437376243_2_pending_question.json
... 1784437376203_1_needs_llm_call.json
... 1784437376091_0_decide_entry.json     ← seq reset to 0 after Zed restart
```

Each decision = 1 synchronous `std::fs::write` from the FOREGROUND thread
(`crates/auto_prompt/src/debug_log.rs:82`). Logging is on by default
(commit `06a609773a feat(auto_prompt): enable decision logging by default`).

Per the seq numbers in the prior session, **1200+ auto_prompt decisions fired**
in one session. Each decision may trigger:

- Plan-file scan: 8 worktrees × N plans = 983 plans scanned, truncated to 10
- Doc-file scan: 22 docs scanned
- Full context serialization: 258,031 chars (≈64K tokens) — though the actual
  LLM call uses a "lightweight" 1.3 KB context (good)
- Up to 2 LLM calls (main + second opinion)
- Markdown parse → `cx.refresh_windows()` → all windows repaint

### 4. `detect_remaining_work` regex safety-net fires false positives, forcing extra LLM calls

`crates/auto_prompt/src/auto_prompt.rs:3096` — `detect_remaining_work()` does
naive substring matching on:

```rust
&["remaining work", "remaining:", "still need", "still needs",
  "next step", "next steps", "todo:", "action items", "left to do"]
```

It also scans every line of the message for unchecked `- [ ]` checkboxes.

Observed in `/Users/katopz/Library/Logs/Zed/Zed.log` (12:03:17):

> Worker LLM confidence=0.1 (wants to stop). Message explicitly says
> "No remaining work" and "All 7 commits landed and pushed successfully".
>
> `detect_remaining_work` matched "remaining work" inside that very sentence
> and forced a `NeedsSecondOpinion` evaluation = a SECOND full LLM streaming
> call (~7s, ~1.5KB output, ~700ms to first byte) just to confirm the stop.

Worse, `decide_with_llm` has **3 separate call sites** that consult
`detect_remaining_work` as a safety net (lines 1665, 1722, plus the main
`evaluate_response` at 647). Each retry path can re-fire the regex.

### 5. Retained continuation threads never auto-prune

`crates/agent_ui/src/agent_panel.rs:1398` — `retained_threads: HashMap<ThreadId, Entity<ConversationView>>`.

Background continuation threads created by `external_thread_background`
(`crates/agent_ui/src/agent_panel.rs:3682`, used by auto_prompt per
`3db73389f8 fix(agent_ui): create auto_prompt continuation threads in background`)
are inserted into `retained_threads` and **stay there forever** unless:

1. The user manually clicks/archives the thread
2. `ThreadMetadataStoreEvent::ThreadArchived` fires (manual action)
3. `try_make_empty_draft_ephemeral` — only when the thread is empty

Each retained `ConversationView` keeps:

- `_subscriptions: Vec<Subscription>` including
  `cx.observe(&action_log, |_, _, cx| cx.notify())`
  (`crates/agent_ui/src/conversation_view.rs:1203`) — this fires on EVERY
  global `action_log` change (every file edit, diff update, keep/reject edit).
  With N retained threads and M file edits, that's N×M `cx.notify()` calls
  per second, each scheduling a ThreadView repaint.
- Markdown parsers (with streaming buffers if still generating)
- A `_turn_timer_task` (1Hz timer — properly stopped on turn end, OK)
- A `_watchdog_task` (10-min timer — properly cancelled on Stopped, OK)

### 6. Streaming-text repaint death spiral (mostly fixed, but still present)

Perf commits `93cf3e0b14`, `945b3c591e`, `ee524a5461`, `aae2198ff9` already
throttled the streaming reveal from 16ms → 300ms and gated markdown
interactivity. But:

- The 300ms reveal tick still triggers `cx.refresh_windows()` per tick per
  streaming thread. If multiple threads stream concurrently (auto_prompt
  continuation + active thread), each contributes its own repaint pressure.
- `crates/agent_ui/src/conversation_view/thread_view.rs:1347-1368` — the
  `_turn_timer_task` skips `cx.notify()` during streaming (good) but only for
  the active thread; parked streaming threads still get repaints from
  `cx.refresh_windows`.

### 7. `cx.observe(&action_log, ...)` is global, not per-thread

```rust
// crates/agent_ui/src/conversation_view.rs:1201
let subscriptions = vec![
    cx.subscribe_in(&thread, window, Self::handle_thread_event),
    cx.observe(&action_log, |_, _, cx| cx.notify()),
];
```

Every `ConversationView` subscribes to the **shared global `action_log`**.
The action_log fires `cx.notify()` on every buffer file change, diff update,
keep/reject edits, undo_last_reject, etc. — most of which are irrelevant to
a parked thread that doesn't own that buffer.

## Root cause summary

The 1500% CPU state is not a single bug — it's a multiplicative interaction:

1. **Auto_prompt fires very frequently** (1200+ decisions/session observed) —
   every thread stop triggers plan/doc scans + 1-2 LLM calls.
2. **`detect_remaining_work` regex** forces extra LLM calls on false-positive
   matches (the word "remaining" appearing anywhere in a stop summary).
3. **Retained continuation threads** accumulate without bound. Each retained
   `ConversationView` subscribes to the global `action_log`, so every file
   edit anywhere notifies every parked thread → repaint cascade.
4. **Subprocess leak (zombies + duplicate MCP servers)** — old MCP processes
   aren't reaped, new ones spawn alongside, each contributes ~3 async-std
   runtime threads + JSON-RPC polling overhead.
5. **Decision logging writes synchronously to `/tmp`** on the foreground
   thread for every decision, 7811 files accumulated.
6. **`cx.refresh_windows()` on every markdown parse completion** — a single
   streaming response still repaints all windows ~3/sec; with N concurrent
   streams, that's 3N repaints/sec across all retained ThreadViews.

When these compound (agent active + multiple parked threads + many recent
file edits + a watchdog firing + 8 MCP servers polling), the foreground
thread saturates and the death spiral described in commit `93cf3e0b14`
("starving the foreground thread so it could no longer pump the incoming
HTTP stream") recurs.

## Recommended fixes (priority order)

### P0 — Quick wins, low risk  ✅ LANDED (see `.docs/006_auto_prompt_cpu_drain_p0_fixes.md`)

- [x] **Default-disable decision logging.** Flip default in
      `crates/auto_prompt/src/debug_log.rs:20` from on → off. The logging is
      structured debug data, not user-facing. Users who need it set
      `ZED_AUTO_PROMPT_LOG=1`. Saves 1 sync fs write per decision.
- [x] **Tighten `detect_remaining_work` patterns.** Require the phrase to
      appear in a *heading* or *list item context* (line starts with `-`, `*`,
      `#`, or a digit), not free-form prose. Drop "remaining:" and
      "remaining work" from the patterns (too generic) — keep "left to do",
      "todo:", "action items". This kills the false-positive → second-opinion
      LLM call cycle.
- [x] **Bound `retained_threads`.** Hard cap at 8 threads via new
      `insert_retained_thread` helper that all insertion paths now route
      through. Existing `cleanup_retained_threads` (cap 5 idle) is invoked
      from the helper so auto_prompt's continuation path no longer bypasses
      it. When the cap is exceeded with all threads busy, the oldest is
      evicted; metadata stays in `ThreadMetadataStore` for reopen.

### P1 — Higher-impact, more code

- [x] **Reap agent-side subprocesses on drop.** Investigated: commit `05f20945eb`
      already covers the agent path via `util::process::Child`. The actual
      leak was in a DIFFERENT `Child` type (`util::command::darwin::Child`,
      used by LSP/debugger/SSH/REPL) whose Drop never called `waitpid`.
      Fixed by adding a detached `waitpid` reap task to that Drop impl.
      See `.docs/006_auto_prompt_cpu_drain_p0_fixes.md` for details.
- [-] **Investigate duplicate MCP server spawns.** Code-review could not
      reproduce. The `ContextServerStore::run_server` lifecycle guards
      look correct (`stop_server` is called before re-spawn for Starting/
      Running/Authenticating states). Deferred — would need live
      instrumentation in `run_server` to diagnose. May also be explained
      by multi-scope settings (same MCP server configured in global +
      project + project-group).
- [-] **Scope `cx.observe(&action_log, ...)` per-thread.** Investigation
      showed the observer IS already per-thread (`AcpThread.action_log` is
      a per-instance `Entity<ActionLog>`, not a shared global). No N×M
      cascade exists. No change needed.

### P2 — Architectural

- [ ] **Cap concurrent streaming threads.** Auto_prompt should refuse to
      create a new continuation thread while N≥2 are already in `Generating`
      state — queue instead. Prevents the "all threads streaming at once →
      3N repaints/sec" death spiral.
- [ ] **Fix the underlying SSE idle hang (issue 003).** The watchdog
      recovers but burns 10 min + a reasoning LLM call per incident. Adding
      the `select! { _ = idle_timer => ... }` branch to
      `agent/src/thread.rs:2353` removes the entire recovery cycle and the
      false-positive scenarios that compound with this issue.
- [ ] **Move auto_prompt decision log off the foreground thread.** Either
      batch-write via a background channel, or move to ring-buffer in memory
      with dump-on-demand. The sync `std::fs::write` per decision is
      unjustifiable on the foreground thread.

## Files to inspect / modify

| File | Concern |
|------|---------|
| `crates/auto_prompt/src/debug_log.rs:20` | Logging default on |
| `crates/auto_prompt/src/auto_prompt.rs:3096` | Naive regex `detect_remaining_work` |
| `crates/auto_prompt/src/auto_prompt.rs:1665, 1722` | Two more safety-net call sites |
| `crates/agent_ui/src/agent_panel.rs:1398` | `retained_threads` unbounded |
| `crates/agent_ui/src/agent_panel.rs:3682` | `external_thread_background` (auto_prompt path) |
| `crates/agent_ui/src/conversation_view.rs:1201` | Blanket `cx.observe(&action_log, ...)` |
| `crates/project/src/context_server_store.rs:673` | `run_server` (duplicate spawn suspicion) |
| `crates/util/src/process.rs` | Where the `Child` reaping impl lives — extend coverage |
| `crates/agent/src/thread.rs:2353` | SSE idle timeout (issue 003) |

## Severity

**HIGH.** This is the user's daily-driver editor; sustained 1500% CPU drains
battery, makes the editor unresponsive, and triggers macOS thermal throttling
that slows down everything else. The compounding nature means it gets worse
the longer the session runs.

## Key Files Reference

- `crates/auto_prompt/src/auto_prompt.rs:3096` — `detect_remaining_work` regex
- `crates/auto_prompt/src/debug_log.rs:82` — sync `std::fs::write` per decision
- `crates/agent_ui/src/agent_panel.rs:1398` — `retained_threads` HashMap
- `crates/agent_ui/src/conversation_view.rs:1201` — `cx.observe(&action_log, ...)`
- `crates/project/src/context_server_store.rs:673` — `run_server` (MCP lifecycle)
- `.docs/002_auto_prompt_stuck_thread_watchdog.md` — watchdog context
- `.issues/003_native_agent_stream_idle_hang.md` — SSE idle hang (the trigger)
