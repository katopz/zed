# 005 — gpui profiler `save_action_timing` panic on concurrent action dispatch (RESOLVED)

## Status

Resolved. Fix in commit `b49cc02630` on `develop`.

## Symptom

~6 tests in `crates/agent_ui/src/conversation_view.rs` failed intermittently
under parallel test execution with:

```
panicked at crates/gpui/src/profiler/actions.rs:87:14
```

Typical failures: `test_granularity_selection_updates_state`,
`test_allow_button_uses_selected_granularity`,
`test_manually_editing_title_updates_acp_thread_title`,
`test_message_editing_regenerate`, `test_message_editing_cancel`,
`test_authorize_tool_call_action_*`, `test_escape_cancels_generation_*`.

Previously misdiagnosed as "scheduler-seed / profiler issues". Not a seed
problem — it is a genuine data race.

## Root Cause

`ACTION_STATISTICS` (`crates/gpui/src/profiler/actions.rs:170`) is a
**process-global** `spin::Mutex<ActionStatistics>`. Action dispatch in
`Window::dispatch_action_on_node_inner` (`crates/gpui/src/window.rs`) does:

1. `update_running_action()` — lock, set `running = Some((action, now))`, unlock
2. run listener
3. `save_action_timing()` — lock, `running.take()`, **`.expect("only called after update_running_action")`**

The three steps hold the lock **separately**, so the update→listener→save
sequence is **not atomic across threads**. With two threads dispatching
concurrently:

| Step | Thread A | Thread B |
|------|----------|----------|
| 1 | update → `running = Some(A)` | |
| 2 | | update → `running = Some(B)` (overwrites A) |
| 3 | save → takes `Some(B)` (wrong data, no panic) | |
| 4 | | save → `running` is `None` → **PANIC** |

In production this is effectively impossible (UI actions dispatch on a single
foreground thread). In parallel test runs it fires ~1 in 3 full-suite runs.

## Fix

`ActionStatistics::save_action_timing` now returns early when `running` is
`None` instead of `.expect()`-ing. Losing one timing sample to a cross-thread
race is acceptable noise for a statistical profiler; the panic was the only
incorrect behavior.

```rust
let Some((action, started)) = self.running.take() else {
    return;
};
```

## Residual limitation (not fixed)

Thread A can still record Thread B's action timing when both update before
either saves — a data-correctness issue inherent to the process-global design.
Switching to `thread_local!` would fix it but changes aggregation semantics
for the hang detector (`crates/zed/src/reliability/hang_detection.rs`), which
samples the global from a monitor thread. Left as-is: the panic is closed,
and statistical noise from rare cross-thread attribution does not mislead the
profiler in practice.

## Validation

- `./script/clippy -p gpui` — clean.
- Full `conversation_view` suite (103 tests):
  - 15/15 clean runs at default parallelism (previously ~1/3 failed).
  - 10/10 clean runs at `--test-threads=32`.
- Watchdog tests (`test_watchdog_*`) still pass.

## Files Changed

| File | Change |
|------|--------|
| `crates/gpui/src/profiler/actions.rs` | `save_action_timing` uses `let-else` instead of `.expect()` |
