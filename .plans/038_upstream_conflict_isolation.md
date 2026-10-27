# 038 — Isolate the fork lane from upstream files (shrink merge-conflict surface)
Status: in progress — `script/fork-surface` shipped; acp_thread + agent fork APIs extracted; surface 16,609 → 16,241 lines (130 files)

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
- [ ] `open_ai_compatible.rs` → move multi-key lane into `provider/open_ai_compatible/multi_key.rs` (health.rs already split); upstream file keeps a single `MultiKeyLane` field + delegating calls
- [ ] `thread_view.rs` → extract fork renders (retry callout, key ring, usage countdown, board card) into `conversation_view/fork_render.rs`; thread_view calls `fork_render::*` at 4–5 seams
- [ ] `conversation_view.rs` → auto-allow countdown + watchdog glue into `conversation_view/auto_allow.rs`
- [ ] `agent_panel.rs` → retained-thread cap + war-room into `agent_panel_fork.rs`
- [ ] `agent/src/thread.rs` → idle-timeout + retry-cap policies into `thread_fork_policy.rs` (pure fns, unit-tested)
- [x] `agent/src/agent.rs` → `agent_fork.rs`: CLAUDE.md symlink (+ statics), `seed_history`, verdict subagent, peer-thread list/send (trait methods now one-line delegations), `running_successor_execution`; agent.rs fork lines 565 → 295 (remaining: struct fields, call-site seams, internal_tests regression tests)
- [x] `acp_thread.rs` → fork `impl AcpThread` (board notification, bloat candidates, subagent retry completion, api-error/turn-id accessors) + `tool_call_bloat_label` moved to `acp_thread/src/fork.rs`; acp_thread.rs fork lines 409 → 216 (remaining: enum variant, struct fields, inline turn-truncation note, regression test that needs private `tests` helpers). Also fixed `acp_thread` standalone build: uuid `v7` feature was only satisfied via workspace unification
- [ ] Fork-only settings (`stream_idle_timeout_secs`, `verdict_max_rounds`, …) → one `fork: ForkAgentSettings` sub-struct so upstream settings structs get a single field
- [ ] Re-merge dry run against next upstream: target ≤ 15 conflicted files
