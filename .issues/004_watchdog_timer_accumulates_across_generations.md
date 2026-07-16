# Issue 004: Watchdog timer accumulates across generations / fires on stale state

## Status
- [x] Root cause identified
- [x] Fix implemented
- [ ] Tested (unit tests blocked by sibling agent's pin-thread WIP build breakage)
- [ ] Runtime verified

## Symptom

The stuck-thread watchdog yells "approximately 20 minutes ago" even though the
current generation started only seconds ago. The elapsed time appears to
**accumulate across generation boundaries** — and, when switching threads, a
stale watchdog from thread A can still fire while the user is looking at
thread B.

User expectation (all three currently broken):

1. Per-thread watchdog time start.
2. Reset to 0 for each new thread (or use a per-thread mapping).
3. The time should count from the last request — reset to 0 for each new request.

## Root Cause

Two independent bugs combine to cause the accumulation:

### Bug A — `cancel_watchdog_for_thread` cancels the WRONG thread

`cancel_watchdog_for_thread` operates on `conversation_view.active_thread()`
(the currently-viewed thread), NOT on the specific thread that emitted the
`Stopped`/`Error` event:

```rust
// crates/agent_ui/src/auto_prompt/mod.rs
pub fn cancel_watchdog_for_thread(conversation_view, cx) {
    if let Some(active) = conversation_view.active_thread() {  // <-- BUG
        active.update(cx, |active, _| { active.cancel_watchdog(); });
    }
}
```

When the user switches to thread B while thread A is generating, thread A's
`Stopped` event fires but the watchdog is cancelled on thread B (which has no
watchdog). Thread A's `_watchdog_task` is never dropped.

### Bug B — `arm_watchdog` guard silently inherits a stale task

`arm_watchdog` skips re-arming if a task already exists:

```rust
// crates/agent_ui/src/conversation_view/thread_view.rs
pub fn arm_watchdog(&mut self, window, cx) {
    if !self.auto_prompt_enabled || self._watchdog_task.is_some() {
        return;  // <-- BUG: stale task from a prior generation blocks re-arm
    }
    ...
    self._watchdog_task = Some(start_watchdog(...));
}
```

Because Bug A leaves a stale task in `_watchdog_task`, a subsequent generation
on the same thread hits this guard and **never gets a fresh watchdog**. The
stale task — with `started_at` from the FIRST generation — keeps running. Its
`elapsed` and `timeout_number` grow across generation boundaries, producing
the "20 minutes" message for a generation that just started.

### The accumulation scenario

1. T0: Thread A starts generating → watchdog armed, `started_at = T0`,
   `_watchdog_task = Some(task1)`.
2. User switches to Thread B. Thread A is now a background (retained) thread.
3. T0+300s: Thread A stops normally. `Stopped` event →
   `cancel_watchdog_for_thread(active_thread=B)` → cancels **B's** (nonexistent)
   watchdog. Thread A's `task1` is NOT cancelled.
4. T0+350s: auto_prompt dispatches a continuation to Thread A → `send_content`
   → `arm_watchdog` → guard `_watchdog_task.is_some()` is true → **no re-arm**.
   The stale `task1` (started at T0) remains the active watchdog.
5. T0+600s: `task1` wakes for timeout #1, sees Thread A is `Generating` again
   (from step 4), fires with `elapsed = 600s`. Reasoning says Continue.
6. T0+1200s: `task1` wakes for timeout #2 with `elapsed = 1200s` (20 min!) even
   though the current generation started at T0+350s (850s ago, not 1200s).

The timeout message `"approximately {} minutes ago".format(elapsed / 60)` now
reads "20 minutes" — accumulated across two separate generations.

## Fix

1. **`cancel_watchdog_for_thread` takes a `session_id`** and cancels the
   specific thread's watchdog via `conversation_view.thread_view(&session_id)`,
   not `active_thread()`.
2. **`arm_watchdog` always drops a stale task before re-arming.** A new
   generation invalidates the previous watchdog — its `started_at` is no longer
   meaningful. Drop the old task (cancels it) and create a fresh one with
   `started_at = Instant::now()`.

Both call sites of `cancel_watchdog_for_thread` in `conversation_view.rs`
(Stopped handler ~line 1652, Error handler ~line 1760) pass the event's
`session_id`, which is already in scope.

## Why per-thread mapping is not needed

A `HashMap<SessionId, Instant>` of start times is unnecessary because the
watchdog task itself already captures `started_at` locally in its closure. The
bug was not a missing data structure — it was that a stale task was being
reused instead of being replaced. Fixing the two guard/cancellation bugs makes
each generation get a fresh `Instant::now()` automatically.

## Files Changed

| File | Change |
|------|--------|
| `crates/agent_ui/src/auto_prompt/mod.rs` | `cancel_watchdog_for_thread` takes `&SessionId`, uses `thread_view(session_id)` |
| `crates/agent_ui/src/conversation_view.rs` | Both call sites pass `&session_id` |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `arm_watchdog` drops stale task before re-arming |

## TL;DR

Watchdog timer accumulates because (A) cancellation targets the active thread
instead of the event's thread, leaving stale tasks behind, and (B) the arm
guard refuses to replace an existing task. Fix: cancel by session_id and
always replace stale tasks on re-arm.
