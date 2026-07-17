# Issue 004: remote_server `handle_kill_kernel` leaks zombie via raw `smol::process::Child`

## Status
- [x] Fix (commit pending — `headless_project.rs` migrated to `util::process::Child`)
- [ ] Proof (no zombie after kill on remote host — requires remote SSH session to validate)
- [ ] Close-out note in related doc

## Resolution
Applied **Option A** from the fix sketch: swapped `use smol::process::Child;` →
`use util::process::Child;` in `crates/remote_server/src/headless_project.rs`, and
changed the `spawn_kernel` closure to use `util::process::Child::spawn(command,
Stdio::inherit(), Stdio::inherit(), Stdio::inherit())` with a `std::process::Command`.

Behavior changes (both improvements, no regression):
1. Killed kernels are now reaped by `util::process::Child::Drop` — no zombie.
2. Kernels now run in their own session (`set_pre_exec_to_start_new_session`), so
   `kill()` reaches the whole process group including kernel-spawned subprocesses.

`handle_kill_kernel` is unchanged — `child.kill().log_err()` still works; the reap
happens when `child` drops at the end of the if-block via the wrapper's `Drop` impl.

Validation:
- `cargo clippy -p remote_server --no-deps -- --deny warnings` ✅
- `cargo build -p remote_server --tests` ✅
- Runtime proof (no zombie on remote host) requires a live SSH session and is left
  to the user to verify on next remote kernel restart.

## Symptom
When a user kills a Jupyter (or other) kernel on a remote Zed session, the
remote `zed_remote_server` process accumulates a `<defunct>` (zombie) entry in
the remote host's process table for every killed kernel. Over a long SSH
session with many kernel restarts this can exhaust the remote user's process
table (`ulimit -u`) or clutter `ps`.

The leak is **not visible on the user's local machine** — it lives on the SSH
host running `zed_remote_server`. This is why it was missed during the local
startup-zombie investigation (commit `05f20945eb`, issue resolved locally).

## Root Cause
`crates/remote_server/src/headless_project.rs` declares:

```rust
use smol::process::Child;   // line 35
...
pub kernels: HashMap<String, Child>,   // line 74
```

It uses the **raw** `smol::process::Child`, not `util::process::Child` (which
gained a defensive `Drop` impl in `05f20945eb` that SIGKILLs the process group
and detaches a reaper).

`handle_kill_kernel` (line 1040-1051) does:

```rust
let child = this.update(&mut cx, |this, _| this.kernels.remove(&kernel_id));
if let Some(mut child) = child {
    child.kill().log_err();   // SIGKILL but no status() await
}
```

`smol::process::Child::kill()` sends SIGKILL but does not reap. When `child`
is dropped at the end of the if-block, `smol::process::Child::drop` also does
not reap (mirrors `std::process::Child`). Result: kernel stays in the process
table as a zombie until the remote_server process itself exits.

Same pattern applies if `HeadlessProject` is dropped while kernels are still
running — the `HashMap` drops each `Child` without reaping.

## Fix Sketch
Two acceptable approaches:

### Option A (preferred, minimal): migrate to `util::process::Child`
```rust
use util::process::Child;
```
Change the `kernels` field type and the `spawn_kernel` closure to use
`util::process::Child::spawn`. `util::process::Child` already:
- Calls `set_pre_exec_to_start_new_session` (so `killpg` reaches grandchildren)
- Has a `Drop` impl that SIGKILLs the group and reaps via `smol::spawn(status)`

Then `handle_kill_kernel` works as-is (`child.kill().log_err()` — when `child`
drops at block end, our `Drop` reaps).

### Option B (local fix only): explicit reap in `handle_kill_kernel`
```rust
if let Some(mut child) = child {
    let _ = child.kill();
    let _ = smol::spawn(async move { let _ = child.status().await; }).await;
}
```
Worse than A because it doesn't fix the `HeadlessProject::drop` path through
the HashMap.

## Validation Gate
1. `cargo clippy -p remote_server --deny warnings`
2. Spawn a remote kernel, kill it, verify on the remote host:
   ```
   ps -o pid,ppid,state,cmd -u $USER | grep defunct
   ```
   must show no new `<defunct>` entry attributed to the remote_server pid.
3. Drop `HeadlessProject` while a kernel is running; same check.

## Scope
- Single crate: `remote_server`
- Single file change for Option A: `crates/remote_server/src/headless_project.rs`
- No protocol changes, no API changes, no behavior change for live kernels —
  only ensures killed kernels are reaped.

## Out of Scope
- The local startup zombie leak is already fixed (`05f20945eb`).
- The `dap` / `context_server` transport Drop impls are already defensively
  handled by `util::process::Child::Drop` (they use the wrapper).
- Windows: `util::process::Child` on Windows reaps on kill via smol, so the
  migration is a no-op there but still cleaner.

## References
- Fix that surfaced this: `05f20945eb fix(util): reap subprocesses on Child drop to prevent zombie leaks`
- Wrapper: `zed/crates/util/src/process.rs`
- Leak site: `zed/crates/remote_server/src/headless_project.rs:35,74,1040-1051`
