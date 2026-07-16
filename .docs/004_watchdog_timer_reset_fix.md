# 004 — Watchdog timer accumulates across generations (RESOLVED)

## Status

Resolved. Fix in commit `b32d7ea083` on `develop`. Regression + e2e tests in
commit `39fffc0bbe`.

## Symptom

Stuck-thread watchdog reported "approximately 20 minutes ago" for a generation
that started seconds ago. Elapsed time accumulated across generation
boundaries; stale watchdogs from background threads could fire while viewing
other threads.

## Root Cause (two compounding bugs)

1. **`cancel_watchdog_for_thread` cancelled the wrong thread.** It targeted
   `conversation_view.active_thread()` (currently-viewed) instead of the
   thread that emitted `Stopped`/`Error`. Switching away from a generating
   thread left its watchdog running.
2. **`arm_watchdog` refused to re-arm when a stale task existed.** Guard
   `self._watchdog_task.is_some()` caused the next generation to inherit a
   stale task whose `started_at` predated it.

## Fix

- `cancel_watchdog_for_thread` now takes `&SessionId` and uses
  `conversation_view.thread_view(&session_id)`.
- `arm_watchdog` always drops a stale task before re-arming, so every
  generation gets a fresh `Instant::now()` baseline.
- No per-thread `HashMap<SessionId, Instant>` is needed: `started_at` lives
  in the task closure, so fixing the lifecycle bugs makes each generation
  fresh automatically.

## Files Changed

| File | Change |
|------|--------|
| `crates/agent_ui/src/auto_prompt/mod.rs` | `cancel_watchdog_for_thread` takes `&SessionId`, uses `thread_view(session_id)` |
| `crates/agent_ui/src/conversation_view.rs` | Both call sites (Stopped, Error) pass `&session_id` |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `arm_watchdog` drops stale task before re-arming |
| `crates/agent_ui/src/conversation_view.rs` (tests) | `test_watchdog_halts_stuck_thread`, `test_watchdog_resets_across_generations` |

## Tests

- `test_watchdog_halts_stuck_thread` — full HALT flow: stuck worker → 1s
  timer fires (via `advance_clock`) → reasoning returns halt → worker
  cancelled → timeout-notice injected → new generation.
- `test_watchdog_resets_across_generations` — regression for this issue:
  two sequential generations, asserts lifecycle armed → None (cancelled) →
  Some (re-armed). Uses default 600s window (no env var) to avoid global
  state races with the HALT test.

Stable at 30 iterations × 2 tests (60/60 pass).

## Remaining

Runtime smoke test in a live Zed instance (cannot be performed by an agent).
Risk is low: the change is a thread-targeting fix + task-lifecycle correction.
