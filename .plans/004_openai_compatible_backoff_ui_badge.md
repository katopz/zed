# OpenAI Compatible: Backoff Status Badge in ConfigurationView

## Goal
Surface per-key backoff state in the provider configuration view so users can
see when a key is being rotated out, why, and how long until it re-qualifies.
Closes the loop on the prior work (plan 002 = backoff, plan 003 = rotation) by
making the previously-invisible health state observable.

## Why
The prior two plans made requests reliable (rotation hides failing keys from
the user's request flow), but the user has no way to tell *which* key is
currently backed off or *when* it will recover. The ConfigurationView still
shows a green check next to a configured key even when that key is in a
multi-hour backoff window. This plan replaces the green check with a warning
badge + countdown when the slot is backed off, and adds a periodic refresh so
the countdown ticks down while the settings page is open.

## Design

### `SlotHealthStatus` + `State::slot_health_snapshot()`
Add a small pure-data struct returned from `State` so the ConfigurationView
can render per-slot status without reaching into `KeyHealthTracker` directly:

```rust
#[derive(Clone, Debug, PartialEq)]
struct SlotHealthStatus {
    has_key: bool,
    is_backed_off: bool,
    backoff_remaining: Duration,
    consecutive_failures: u32,
}
```

`State::slot_health_snapshot(&self) -> [SlotHealthStatus; 3]` — one entry per
slot in `[Primary, Secondary, Tertiary]` order. Clones the tracker under the
mutex (same pattern as `snapshot_health`), then computes the per-slot fields
against `Instant::now()`. Uses `saturating_duration_since` to avoid panics
when `backoff_until` is in the past.

### `format_backoff_remaining(Duration) -> String`
Human-readable countdown. Format:
- `>=1h` → `"1h 5m"` (hour precision is enough; the user doesn't need seconds
  at this scale)
- `>=1m` → `"4m 32s"`
- `<1m` → `"45s"`

### ConfigurationView changes
- New field `backoff_refresh_task: Option<Task<()>>`. The task is spawned in
  `new()` and runs for the lifetime of the view. It polls `slot_health_snapshot`
  every 1s; if the snapshot changed since the last poll, it calls `cx.notify()`
  to trigger a re-render. When the view is dropped, the task is dropped with it
  (cancelling the future).
- In `render()`, for each slot where the key is configured:
  - If `is_backed_off` → replace the green check with a warning icon + label
    `"In backoff: 4m 32s"` colored `Color::Warning`, plus a tooltip explaining
    consecutive failures and the auto-recover behavior.
  - Else → keep the existing green check.
- The badge reads from the snapshot taken at the top of `render()`, so the
  countdown string stays in sync with the periodic refresh.

### Polling cadence rationale
1 second is fine because:
- ConfigurationView is short-lived (only rendered while the user is on the
  provider settings page).
- The poll only takes a mutex lock + compares 12 primitive fields; cost is
  negligible.
- Only calls `cx.notify()` when the snapshot actually changed (e.g. the
  countdown ticked, or a slot entered/exited backoff).

## Tasks
- [x] Add `SlotHealthStatus` struct + `PartialEq` derive
- [x] Add `State::slot_health_snapshot()` returning `[SlotHealthStatus; 3]`
- [x] Add `format_backoff_remaining()` helper
- [x] Add `backoff_refresh_task: Option<Task<()>>` field to ConfigurationView
- [x] Spawn periodic refresh task in `ConfigurationView::new`
- [x] Modify `render()` to show backoff badge + countdown when slot is in backoff
- [x] Add tests for `format_backoff_remaining` (hours, minutes, seconds, zero)
- [x] Add test for `slot_health_snapshot` (healthy + backed-off + mixed)
- [x] `cargo check -p language_models`
- [x] `./script/clippy -p language_models` (incl. cargo-machete)
- [x] `cargo test -p language_models --lib`

## Validation
- Badge renders with `Color::Warning` + `IconName::Warning` when slot is backed
  off, green check otherwise.
- Countdown string formats per spec (hours/minutes/seconds).
- Periodic refresh only notifies when snapshot changes (no busy re-render).
- Task is dropped (cancelled) when ConfigurationView is dropped.
- All existing tests continue to pass unchanged.

## Non-goals (still)
- No persistence of backoff state across restarts (separate follow-up).
- No mid-stream retry (still out of scope; would require partial-output
  reconciliation).
- No historical failure log / chart — only current state is shown.
- No user action to manually clear backoff (auto-clear at 5h is the only path).

## Key Files
- `zed/crates/language_models/src/provider/open_ai_compatible.rs` —
  `SlotHealthStatus`, `State::slot_health_snapshot`, `format_backoff_remaining`,
  `ConfigurationView::{new, render}`, tests.

## TL;DR
Add a warning badge + live countdown to each configured key card in the
OpenAI-compatible provider settings when that key is in backoff. Polls every
1s while the settings page is open; only re-renders when state changes.
Makes the previously-invisible backoff system observable to the user.
