# 037: auto_prompt's max-iterations cap is global, so it never fires with concurrent threads
Status: open — documented, behaviour unchanged on purpose; the no-progress stop (`82c6ffc7d8`) covers the runaway case it was meant for

## Symptom

A Claude ACP thread whose remaining work all waited on the owner (SSH access,
a Zed rebuild, data from real use) was re-dispatched by the summary fast path
for 10+ rounds on 2026-10-27, past `max_iterations = 20` worth of chain time,
without the cap ever stopping it.

## Root cause

`AUTO_PROMPT_ITERATION`, `LAST_ITERATION_SECS` and `VERIFICATION_COUNT`
(`crates/auto_prompt/src/auto_prompt.rs`) are process-wide atomics, but
`reset_iteration_with_session(session_id)` zeroes them on every chain stop of
*any* session. With several agents in one window, some thread stops every few
minutes, so no single thread's count gets near the cap. The same atomics are
also incremented by every thread, so in a quiet window one busy thread can be
cut short by another's turns. `api_unreachable` had the same defect and moved
to per-session streaks for exactly this reason.

## What shipped instead

- `cfb60f7b21`: the terminal-summary check recognises blocked / waiting /
  needs-you items (word-start matching).
- `82c6ffc7d8`: `stall.rs` stops a same-thread chain once its summary's next
  steps are unchanged for 2 repeats, per session. This is a no-progress signal
  rather than a turn count, so long productive chains are unaffected.

## Decision (owner-delegated verdict)

Do not simply make the cap per-session: a real 20-turn limit per thread would
start cutting off long productive chains that today only survive because the
global counter keeps being reset. Options, in order of preference:

- [ ] Make the counters per-session (same `Mutex<HashMap>` pattern as
      `api_unreachable` / `stall`) **and** count only turns without progress
      (no commit, no file edit), so the cap becomes a backstop for the stall
      detector instead of a limit on useful work.
- [-] Keep the global counter as is and rely on `stall.rs` (current state).
