# Plan 027: Key rotation rework + summary-first auto-prompt + auto-allow everywhere

## Problem

1. Key backoff cap is 5h — too long; a stale backoff keeps a recovered key out of
   rotation for hours.
2. Spare-key selection was time-bound (`deterministic_hourly_pick`, wall-clock hour
   modulo) instead of random-per-thread, and had no same-thread cache affinity.
3. New threads never verified whether backed-off keys are *really* still limited.
4. Context-overflow Phase 2 ran an extra LLM "reasoned continuation" pass and a
   detector chain even when the worker already ended with a `## Summary` — wasted
   latency/tokens ("do not process").
5. Same-thread continuation always paid the orchestrator LLM call even when the
   worker's last message was already a self-summary with remaining work.
6. Auto-allow countdown (plan 025) only covered `PermissionGrant`; ActionChoice
   prompts and the sandbox-fallback prompt still parked unattended chains forever.
7. Claude + native agents should share the same summary-first code flow.

## Design

### A. Key rotation (`language_models/src/provider/open_ai_compatible/health.rs`)

- `BACKOFF_MAX`: 5h → **1h** (req 1).
- `select_from_candidates` new policy:
  1. **K1 (Primary) always wins while healthy** (req 2, unchanged stickiness).
  2. **Session affinity**: reuse `last_used_slot` while it stays healthy — same
     thread keeps the same key so the upstream prompt cache stays hot (req 4).
  3. **Random pick** among remaining healthy slots (replaces hourly rotation;
     reqs 4/5 — re-randomizes naturally on retry because the failed slot is no
     longer healthy/in `remaining`).
  4. Fail-open earliest-expiring backed-off enabled key (unchanged).
- `KeyHealthTracker::reset_session()` — clears `last_used_slot` so the next pick
  re-randomizes; called on new-thread creation.

### B. New-thread API check (`open_ai_compatible.rs` + `language_model.rs`)

- New trait method `LanguageModel::reset_key_session(&self, cx: &App)` (default
  no-op; only OpenAI-compatible implements).
- Implementation: clear the session-sticky pick, then background-probe **every**
  configured key (including backed-off ones) via the existing `run_key_probe`
  (1-token ping). `Ok` → clear that slot's backoff (it's not really limited);
  rate-limit/err → leave backoff untouched (matches `ConfigurationView::probe_key`
  semantics). Probing happens only at new-thread time (reqs 3/4).
- `agent_ui::auto_prompt::dispatch_action` calls `model.reset_key_session(cx)`
  right before creating a new thread (manual + automatic paths both funnel here).

### C. Summary-first auto-prompt (`auto_prompt/src/auto_prompt.rs`)

- New `pub const CONTINUE_REMAINS_DECISION = "Continue remains and make decisions
  for best perf/sec prod grade"` (reqs 6/7 decision text).
- New shared `summary_continuation_fast_path(&LlmCallData) -> Option<AutoPromptOutcome>`:
  last message is a voluntary summary (`looks_like_voluntary_summary`) AND the
  summary does not declare nothing-left → `Continue` same-thread action carrying
  the fixed decision text. Called from:
  - `decide_with_llm` (native same-thread, after the pending-question path), and
  - `decide_claude_async` (Claude same-thread) → shared flow (req 9).
  - Nothing-left summaries return `None` → normal flow (stop/housekeeping).
- Overflow Phase 2 (new thread, req 6): no more reasoned pass —
  `phase2_reasoning_attempt*` / `reason_phase2_continuation` /
  `build_phase2_reasoning_input` / `PHASE2_REASONER_*` and the
  `reasoned_phase2_enabled` config + env var are removed ("do not process").
  Continuation is the fixed decision text, except:
  - summary declares nothing-left AND no unclaimed plan tasks (current repo, then
    other repos) → `housekeeping_continuation()` (termination safety).
- No summary at last paragraph → unchanged Phase 1 ("ask AI to summary first")
  then Phase 2 as above.

### D. Auto-allow everywhere (`agent_ui/src/conversation_view.rs`, req 8)

- `fire_auto_allow`: drop the `PermissionGrant`-only guard and the
  sandbox-fallback carve-out. Any pending tool authorization that offers an
  allow option is auto-answered after the countdown with the least-privileged
  allow (first `AllowOnce`, else first `AllowAlways`):
  - ActionChoice prompts with allow options (e.g. save/discard) → allow fires.
  - Sandbox fallback → "Run without sandbox once" (retry/deny never picked).
  - Prompts with no allow option stay with the user.
- Covered UIs after this: terminal/edit/sandbox permission cards, windows fs
  warning, save/discard action choices, sandbox fallback, plus the already-shared
  elicitation auto-answer (ask_user forms) from plan 010. Both native + ACP
  (Claude) threads funnel through `Conversation` (req 9).

## Files

- `crates/language_models/src/provider/open_ai_compatible/health.rs`
- `crates/language_models/src/provider/open_ai_compatible.rs`
- `crates/language_model/src/language_model.rs`
- `crates/agent_ui/src/auto_prompt/mod.rs`
- `crates/agent_ui/src/conversation_view.rs`
- `crates/auto_prompt/src/auto_prompt.rs`
- `crates/auto_prompt/src/config.rs`
- `crates/auto_prompt/src/claude_agent.rs`

## Tasks

- [x] A1 BACKOFF_MAX 5h→1h + doc updates
- [x] A2 select_from_candidates: K1 sticky → session affinity → random → fail-open
- [x] A3 reset_session + tests (random membership, affinity, re-randomize)
- [x] B1 LanguageModel::reset_key_session trait method
- [x] B2 OpenAI-compatible impl: clear sticky + background probe all keys
- [x] B3 dispatch_action hook on new-thread creation
- [x] C1 CONTINUE_REMAINS_DECISION + summary_continuation_fast_path + tests
- [x] C2 decide_with_llm wiring (same-thread fast path + overflow simplification)
- [x] C3 Phase 2 fixed-decision continuation + housekeeping guard + tests
- [x] C4 remove reasoned Phase 2 machinery + config field/env + stale tests
- [x] C5 decide_claude_async fast path (parity)
- [x] D1 fire_auto_allow covers ActionChoice + sandbox fallback + tests
- [x] clippy + targeted tests green (language_models, auto_prompt, agent_ui)
- [x] commit `feat:` on develop

## Results

- `cargo clippy -p auto_prompt -p language_models -p language_model -p agent_ui
  --all-targets --all-features -- --deny warnings`: clean.
- `cargo test -p auto_prompt --all-features`: 428 passed.
- `cargo test -p language_models --lib open_ai_compatible`: 72 passed
  (incl. new random/affinity/reset selection tests).
- `cargo test -p agent_ui --lib conversation_view::`: 157 passed; the 2
  failures (`test_close_session_returns_error_when_unsupported`,
  `test_watchdog_does_not_fire_during_active_stream`) reproduce identically on
  clean HEAD (verified via temp worktree) — pre-existing, not from this plan.

## Follow-up: pre-existing failures fixed (commit `61d9947e23`)

Both failures flagged above (tracked as issue 015) are fixed and the issue
removed:

- `test_close_session_returns_error_when_unsupported` — the
  `StubAgentConnection::close_session` stub returned `Ok` even when
  `supports_close_session()` was false, violating the `AgentConnection`
  contract (default impl returns `Err`). The stub now gates on the
  capability flag, mirroring `load_session`.
- `test_watchdog_does_not_fire_during_active_stream` (+
  `test_watchdog_halts_stuck_thread`, which flaked in the opposite
  direction) — root cause was NOT the fake clock: both tests mutated the
  process-global `ZED_AUTO_PROMPT_WATCHDOG_*` env vars, and parallel test
  threads in one binary race their config loads against a sibling's
  `set_var` (via the shared `CACHED_CONFIG` static), arming the wrong
  timeout (1s vs 2s). Reproduced 5/8 parallel runs pre-fix. Fix: per-app
  `WatchdogConfigOverride` GPUI global consulted by `start_watchdog` —
  per-test state, no process globals. Post-fix: 12/12 parallel watchdog
  runs green, full `agent_ui` suite 465/465 twice, `acp_thread` 124/124,
  `auto_prompt hidden_thread` 12/12.

## Notes / deviations

- Same-thread no-summary case keeps the orchestrator flow instead of forcing a
  summarize round-trip every turn (perf/sec: workers with the mandated `##
  Summary` format hit the fast path anyway; the orchestrator preserves
  stop/clarify detection for the rest).
- Removed `reasoned_phase2_enabled` config + `ZED_AUTO_PROMPT_REASONED_PHASE2_ENABLED`
  env (old config files still parse — unknown serde fields are ignored).
- `truncate_at_char_boundary` kept (now `pub`) — the repo `.rules` names it the
  canonical truncation helper.
- Auto-allow now also answers sandbox-fallback prompts with "Run without
  sandbox once" after the countdown (previously carved out by plan 025).

## Addendum: issue 029 (fair distribution fix)

The plan-027 "session-sticky" pick (`last_used_slot`) was process-global,
 so with K1 unavailable all concurrent agents stuck to whichever spare one
 random roll picked (user report: 6 agents, all on K4). Fixed in `cb99d024cd`:
 selection is keyed by `LanguageModelRequest::thread_id` (per-thread sticky
 map, TTL 30min) and fresh picks advance a round-robin cursor (random start)
 over the healthy spares — 6 agents over K2..K4 now distribute exactly 2/2/2.
 K1-priority, fail-open fallback, and `reset_key_session` probing unchanged;
 `reset_session`/`record_attempt` removed as obsolete. Persist-if-changed now
 compares persisted slot health only (manual `PartialEq`), so per-request
 selection mutations no longer schedule backoff-file writes.
