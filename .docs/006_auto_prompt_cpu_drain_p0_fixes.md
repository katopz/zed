# 006 — auto_prompt CPU drain: P0 fixes

## Status
- [x] Diagnosis filed (`.issues/006_auto_prompt_cpu_drain_analysis.md`)
- [x] P0 fixes implemented (this doc)
- [x] `cargo clippy` clean on `auto_prompt` and `agent_ui`
- [x] `cargo test -p auto_prompt --lib` — 250/250 pass (6 new tests added)
- [x] `cargo test -p agent_ui --lib retained` — 7/7 pass
- [x] `cargo test -p sidebar --lib retained` — 3/3 pass
- [x] P1 fix: zombie reaping in `util::command::darwin::Child::drop` (see below)
- [x] `cargo test -p util --lib` — 126/126 pass (+1 new regression test)
- [ ] Verified against live editor (user to confirm CPU drop)
- [ ] P1 follow-ups: MCP duplicate-spawn diagnosis (deferred — needs live debug), action_log observer already per-thread (no change needed)
- [ ] P2 follow-ups (concurrent-stream cap, SSE idle timeout, background decision log)

## What landed (commit reference: see `git log`)

### 1. Default-disable decision logging

**File:** `crates/auto_prompt/src/debug_log.rs`

`ZED_AUTO_PROMPT_LOG` is now **off by default**. Each decision performs a
synchronous `std::fs::write` on the foreground thread — at 1200+ decisions
per session that is a measurable source of foreground stalls, and the
resulting 7811-file `/tmp/zed_auto_prompt/` accumulation was the third
largest CPU contributor in the issue-006 evidence.

Users who want the trace flip it back explicitly:

```sh
export ZED_AUTO_PROMPT_LOG=1     # or "true"
```

`ZED_AUTO_PROMPT_LOG_DIR` still overrides the destination.

### 2. Tighten `detect_remaining_work` patterns

**File:** `crates/auto_prompt/src/auto_prompt.rs`

Old behavior matched these substrings anywhere in the message body:

```
"remaining work", "remaining:", "still need", "still needs",
"next step", "next steps", "todo:", "action items", "left to do"
```

A stop summary containing the literal sentence "No remaining work" matched
`"remaining work"` and forced a second-opinion LLM streaming call (~7s,
~1.5 KB output) just to confirm the stop. With 1200+ decisions/session
these false positives were a significant CPU contributor.

New behavior:
- Pattern set reduced to authoritative task-list markers only: `"todo:"`,
  `"action items"`, `"left to do"`.
- The phrase must appear on a line that starts with a markdown list/heading
  marker (`-`, `*`, `+`, `#`, or `digit.`/`digit)`). Free-form prose
  mentions no longer fire.
- Negation guard: if any of `no `, `none `, `nothing `, `nothing left `,
  `no further `, `already done`, `all done`, `complete`, `finished`,
  `shipped`, `landed`, `resolved` appears within 40 chars before the match
  on the same line, the override is suppressed.
- Unchecked-checkbox detector (`- [ ]` not struck through) is unchanged.

Six new tests cover the regression cases; four existing tests were updated
to use the tightened semantics.

### 3. Bound `retained_threads` via centralized `insert_retained_thread`

**File:** `crates/agent_ui/src/agent_panel.rs`

The existing `cleanup_retained_threads` mechanism (default
`MaxIdleRetainedThreads = 5`) was only invoked from `retain_running_thread`.
The auto_prompt continuation path (`external_thread_background`) and
`create_thread_with_options` inserted directly into `retained_threads`
without invoking cleanup — so under auto_prompt loops the park grew
without bound.

Changes:
- Field type changed from `HashMap<ThreadId, Entity<ConversationView>>`
  to `IndexMap<…>` (preserves insertion order for FIFO eviction;
  `collections::IndexMap` uses the same `FxBuildHasher` as before).
- All `.remove(&id)` call sites updated to `.shift_remove(&id)` to silence
  the deprecation warning and preserve order semantics.
- New helper `AgentPanel::insert_retained_thread(thread_id, view, cx)`:
  1. `shift_remove` + `insert` (refreshes FIFO position on re-park)
  2. calls `cleanup_retained_threads(cx)` (existing logic, respects
     `MaxIdleRetainedThreads`)
  3. enforces a hard cap of `MAX_RETAINED_THREADS = 8` as a backstop for
     the runaway case where every retained thread is mid-generation (so
     cleanup cannot evict any). The oldest is dropped; its metadata stays
     in `ThreadMetadataStore` for sidebar reopen.
- `retain_running_thread`, `external_thread_background`,
  `create_thread_with_options`, and the draft-parking branch of
  `activate_new_thread` all route through the new helper.

The hard cap of 8 (vs. cleanup's 5) is intentionally higher so legitimate
busy threads aren't evicted prematurely; the backstop only fires in the
runaway state the issue-006 investigation surfaced.

## What did NOT change (intentionally)

- The watchdog timer (`auto_prompt/mod.rs:1107`) — already uses GPUI
  timers correctly.
- The streaming-text reveal throttle (300 ms) — already in place from
  `93cf3e0b14`.
- The unchecked-checkbox detector in `detect_remaining_work` — still
  catches `- [ ]` items that aren't struck through.
- The summary-response guard (`is_auto_prompt_summary_response`) — still
  skips Phase 1 ContextOverflow summaries.

## P1 fix: zombie subprocess reaping in `util::command::darwin::Child`

**File:** `crates/util/src/command/darwin.rs`

### Root cause (corrected from issue 006)

Issue 006 originally claimed the 34 zombies came from the agent's MCP
servers not being reaped, and that commit `05f20945eb` only covered
`remote_server`. **That diagnosis was wrong.** Investigation shows:

- The agent's MCP servers use `context_server::StdioTransport` which
  spawns via `util::process::Child` — and that wrapper's `Drop` impl
  (added in `05f20945eb`) already sends SIGKILL + detached reap.
- The agent's external server connections (`AcpConnection`) also use
  `util::process::Child`, same reap path.

The actual leak was in a **different `Child` type**: `util::command::Child`
(on macOS, re-exported from `util::command::darwin`). This is the wrapper
used by **LSP servers** (`crates/lsp/src/lsp.rs`), the **debugger
companion** (`crates/project/src/debugger/session.rs`), **SSH transports**
(`crates/remote/src/transport/`), and **REPL kernels** (`crates/repl/`).

Its `Drop` impl only sent SIGKILL when `kill_on_drop` was set, but **never
reaped**. The LSP shutdown path is the most prolific caller of the leak
pattern:

```rust
// crates/lsp/src/lsp.rs:1139
server.lock().take().map(|mut child| child.kill());
```

`kill()` sends SIGKILL but does not call `waitpid`. The child becomes a
zombie that stays in the kernel process table until the parent (Zed)
itself exits. With ~25 language servers across multiple worktrees and
several LSP restarts over a long session (settings changes, worktree
additions, etc.), this easily produces 30+ zombies.

### Fix

`util::command::darwin::Child::drop` now schedules a detached `waitpid`
on the smol global executor, matching the semantics of
`util::process::Child::drop`. If `kill_on_drop` is set, the SIGKILL is
sent first (existing behavior); the reap task then blocks until the
process exits (typically immediate, since SIGKILL can't be caught) and
reaps the zombie.

If `kill_on_drop` is NOT set, the reap task waits for the process to exit
naturally. This is correct behavior — the alternative is a zombie, and the
reap runs on a background executor thread so it never blocks the foreground.

### Test

`command::darwin::tests::dropping_child_kills_and_reaps_subprocess`
spawns `/bin/sleep 300` with `kill_on_drop(true)`, drops the handle
without awaiting `status()`, and polls `kill -0` until the process is
fully reaped (ESRCH). Mirrors the existing regression test in
`util::process::tests`.

## P1 investigation: action_log observer — NO CHANGE NEEDED

Issue 006 finding #7 claimed every `ConversationView` subscribed to a
"shared global `action_log`", producing an N×M repaint cascade. **This
finding was incorrect.** The action_log is **per-thread**
(`AcpThread::action_log: Entity<ActionLog>` at
`crates/acp_thread/src/acp_thread.rs:1213`), and each `ConversationView`
observes ITS OWN thread's action_log (`conversation_view.rs:1121` reads
`thread.read(cx).action_log().clone()`). There is no global cascade.

The repaint pressure from action_log observers is bounded by the per-thread
edit rate, which is exactly what the P0 `retained_threads` cap already
addresses.

## P1 investigation: duplicate MCP server spawns — DEFERRED

The observed "4 unique MCP servers × 2 instances each" pattern was not
reproduced in code review. The lifecycle guards in
`ContextServerStore::run_server` correctly call `stop_server` before
re-spawning when the server is in `Starting | Running | Authenticating`
state. Possible explanations that require live debugging to confirm:

- The duplicates were left over from a workspace reload that happened
  before process cleanup ran (now fixed by the P0 retained_threads cap
  and this P1 reap fix).
- The user has the same MCP server configured in multiple settings
  scopes (global + project + project-group), each of which legitimately
  spawns its own instance.
- A `maintain_servers` race that only manifests under specific timing.

Adding logging to `ContextServerStore::run_server` to track spawn/stop
events would help diagnose if this recurs. Not addressed here because
the fix is not obvious from static analysis and the P0+P1 fixes already
remove the biggest CPU contributors.


## Verification

```
$ CARGO_TARGET_DIR=/tmp/zed-issue-006 cargo clippy -p auto_prompt --lib --tests
Finished `dev` profile in 1m 59s   # no warnings

$ CARGO_TARGET_DIR=/tmp/zed-issue-006 cargo clippy -p agent_ui --lib --tests
Finished `dev` profile in 4.96s    # no warnings

$ CARGO_TARGET_DIR=/tmp/zed-issue-006 cargo test -p auto_prompt --lib
test result: ok. 250 passed; 0 failed; 0 ignored; 0 measured

$ CARGO_TARGET_DIR=/tmp/zed-issue-006 cargo test -p agent_ui --lib retained
test result: ok. 7 passed; 0 failed

$ CARGO_TARGET_DIR=/tmp/zed-issue-006 cargo test -p sidebar --lib retained
test result: ok. 3 passed; 0 failed
```

Three unrelated agent_ui tests (`test_new_workspace_load_uses_global_terminal_entry_kind`,
`test_restored_terminal_does_not_update_global_entry_kind`,
`test_watchdog_does_not_fire_during_active_stream`) fail when run as
part of the full suite, but **pass in isolation** and **fail identically
on unmodified `develop`** — confirmed pre-existing flakes, not regressions
from these changes.
