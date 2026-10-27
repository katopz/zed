# 038 — Isolate the fork lane from upstream files (shrink merge-conflict surface)
Status: in progress — phase 1 (whole-item moves) done, conflict replay 177 → 166 hunks; phase 2 (inline seams) next — `script/fork-surface` shipped; acp_thread, agent, thread, thread_view, agent_panel, open_ai_compatible, agent tests fork code extracted + fork settings grouped; surface 16,609 → 8,565 lines (130 files)

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
- [x] `agent/src/tests/mod.rs` → `tests/fork_tests.rs` (8 fork-only tests); fork lines 496 → 2. Second `script/fork-split-items` pass over acp_thread/agent/thread moved 4 more items
- [x] Fork-only settings → `ForkAgentSettingsContent` (`settings_content/src/agent/fork.rs`, `#[serde(flatten)]` so JSON keys stay under `agent`) + `ForkAgentSettings` (`agent_settings/src/fork.rs`, `from_content`); upstream structs carry one `fork` field each, accessors read `settings.fork.<key>`. settings_content/agent.rs fork lines 93 → 6, agent_settings.rs 72 → 29. `claude_md_symlink` defaults now `unwrap_or` (no panic if default.json lacks the key)
- [x] Re-merge dry run. Only 2 new upstream commits (merge clean, 0 conflicts), so the 1038-commit sync was replayed backwards with `script/fork-conflict-sim` (merge-tree, no worktree): **55 files / 177 hunks right after the sync (f4e142ba98) → 55 files / 166 hunks now (−6%)**. Target ≤ 15 files NOT met. Finding: the changed-lines metric (−48%) overstates the win — whole fork-only items moved cleanly, but conflicts come from inline edits inside hot upstream fns, plus Cargo.lock/Cargo.toml. Hunks now: thread_view 25, acp_thread 15, Cargo.lock 14, thread 11, agent 10, conversation_view 8, open_ai_compatible 7
- [ ] Phase 2 — inline seams: turn the inline fork edits in the top conflict files (thread_view render fragments, acp_thread/thread/agent turn-loop edits) into single-line calls to fork fns; measure with `script/fork-conflict-sim` (hunks), not `fork-surface` (lines)
- [-] Cargo.lock conflicts — regenerate instead of merge (`git checkout --theirs Cargo.lock && cargo update -w`) during syncs; document in the sync runbook rather than refactor
