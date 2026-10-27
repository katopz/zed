# 038 — Isolate the fork lane from upstream files (shrink merge-conflict surface)
Status: in progress — `script/fork-surface` shipped; acp_thread, agent, thread, thread_view, agent_panel, open_ai_compatible fork code extracted; surface 16,609 → 9,166 lines (130 files)

## Why

Syncing 1038 upstream commits (plan 037 P0) produced **54 conflicted files /
~220 hunks**, all from fork edits *inside* upstream-owned files. Fork-owned
crates (`auto_prompt`, `agent_board`, `agentd`, `work_remote`) never conflict.

Baseline (merge-base `3f660a0a60` → `origin/develop` before sync):
116 upstream files modified, **16,959 changed lines**. Top offenders:

| lines | file | fork feature living inline |
|---:|---|---|
| 3377 | agent_ui/src/conversation_view/thread_view.rs | retry callouts, key ring, usage countdown, board cards, prompt jumps |
| 2155 | language_models/src/provider/open_ai_compatible.rs | multi-key lane, backoff, key health UI |
| 1923 | agent_ui/src/conversation_view.rs | auto-allow countdown, watchdog, auto_prompt glue |
| 1551 | agent_ui/src/agent_panel.rs | retained-thread cap, pin, war-room |
| 730 | agent_ui/src/thread_metadata_store.rs | pinning |
| 637 | agent/src/thread.rs | idle timeouts, retry cap, resume guard |
| 565 | agent/src/agent.rs | mailbox, verdict subagent, seed_history |
| 485 | acp_thread/src/acp_thread.rs | board notification entry, bloat labels |

## Rule (SOLID / open-closed against upstream)

Fork logic lives in fork-owned files; upstream files carry only **one-line
seams**: a `mod` line, a call into the fork module, or a trait impl delegating
to it. Pattern proven by plan 037 (`acp/detachable.rs`: 7 one-line upstream touches).

## Tasks (one PR-sized refactor each, measured by re-running the baseline script)

- [x] Script `script/fork-surface` — prints changed-lines-in-upstream-files vs merge-base (the metric above); record before/after per task
  - pre-sync (`origin/develop` vs `upstream/main`): 124 files / 16,995 lines (counts fork deletions of upstream files; excludes Cargo.lock)
  - post-sync (`--worktree`, vs upstream tip `6b985a8b26`): 129 files / 16,609 lines — the baseline every refactor below is measured against
- [x] `open_ai_compatible.rs` → `open_ai_compatible/fork.rs` (fork-only `impl State` multi-key lane, 30 methods, + 10 free items) and `open_ai_compatible/tests/fork_tests.rs` (17 key-health/probe tests); open_ai_compatible.rs fork lines 2172 → 526 (remaining: `State`/`ConfigurationView` multi-slot fields + edits inside upstream fns). `MultiKeyLane` field consolidation stays a possible follow-up
- [x] `thread_view.rs` → `thread_view/fork.rs` (20 fork-only free items/trait impls + 26 fork-only `ThreadView` methods), `thread_view/branch_boundary_tests.rs` (fork-only test module), `thread_view/tests/fork_tests.rs` (6 fork tests); seam = `mod fork; use fork::*;`. thread_view.rs fork lines 3361 → 885 (remaining: inline render fragments + struct fields inside upstream fns). Tool: `script/fork-split-items FILE upstream/main` (generic; reusable for the other files)
- [x] `conversation_view.rs` fork tests (20 items, 1231 lines) → `conversation_view/tests/fork_tests.rs` via one `mod fork_tests;` seam — `efaac2e25b`; file surface 1893 → 285, total 16,241 → 14,800
- [ ] `conversation_view.rs` → remaining auto-allow countdown + watchdog glue (285 lines) into `conversation_view/auto_allow.rs`
- [x] `agent_panel.rs` → `agent_panel/fork.rs` (4 free items + 8 `AgentPanel` methods via `script/fork-split-items`) and `agent_panel/tests/fork_tests.rs` (10 fork tests incl. retained-thread hard cap + stream cap); agent_panel.rs fork lines 1543 → 372
- [x] `agent/src/thread.rs` → `thread/fork.rs` (stream-idle-timeout type + retry consts/`retry_after_delay`, `messages`/`set_messages`, `stream_idle_timeout_future`, metadata-call retry + `strip_unsupported_images`; seam = `mod fork; pub(crate) use fork::*;`) and its 3 fork idle-timeout tests → `thread/tests/fork_tests.rs`; thread.rs fork lines 632 → 220
- [x] `agent/src/agent.rs` → `agent_fork.rs`: CLAUDE.md symlink (+ statics), `seed_history`, verdict subagent, peer-thread list/send (trait methods now one-line delegations), `running_successor_execution`; agent.rs fork lines 565 → 295 (remaining: struct fields, call-site seams, internal_tests regression tests)
- [x] `acp_thread.rs` → fork `impl AcpThread` (board notification, bloat candidates, subagent retry completion, api-error/turn-id accessors) + `tool_call_bloat_label` moved to `acp_thread/src/fork.rs`; acp_thread.rs fork lines 409 → 216 (remaining: enum variant, struct fields, inline turn-truncation note, regression test that needs private `tests` helpers). Also fixed `acp_thread` standalone build: uuid `v7` feature was only satisfied via workspace unification
- [ ] Fork-only settings (`stream_idle_timeout_secs`, `verdict_max_rounds`, …) → one `fork: ForkAgentSettings` sub-struct so upstream settings structs get a single field
- [ ] Re-merge dry run against next upstream: target ≤ 15 conflicted files
