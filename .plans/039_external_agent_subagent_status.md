# 039 — Show external agents' background subagents in the thread
Status: in progress — `agent.external_subagents` gate shipped (`39ebd58099`, default on); live GUI check awaiting a Zed restart (2026-10-27)

## Problem

When Claude Code (via ACP) launches background agents ("Two agents are still
working… I'll report when both finish"), the Zed thread shows nothing: no card,
no running/waiting/done state.

## Findings

- The adapter (`@agentclientprotocol/claude-agent-acp` 0.97.0) already speaks
  the ACP subagents RFD: when the client declares the `subagents` capability it
  announces each Claude subagent as its own session with `subagent_update`
  (`state: running → requires_action → idle + stopReason`) and the parent's
  `session_message` (`dist/native-subagents.js`). Background `task_*` SDK events
  drive those states (`subagents.taskStarted` / `finishTask`).
- Zed already renders that: `apply_v1_subagent_update` creates a child
  `AcpThread`, and `acp_subagent_card` draws a card with Running / Completed /
  Failed / Canceled, a stop button and a live preview of the child's entries.
- The only gate: v1 agents are offered `subagents` only behind the `acp-beta`
  feature flag (`agent_servers/src/acp/v1.rs`), which also switches whole
  connections to ACP v2 and is staff-only to override. Without it the adapter
  never announces subagents and Zed drops the updates.
- Rejected: impersonating JetBrains AIR (`asyncTasks`) — brings in AIR-only
  behaviour; raw `_claude/sdkMessage` parsing — duplicates what the RFD path
  already models.

## Approach

Un-gate only the `subagents` capability behind a user setting,
`agent.external_subagents` (default on), mirrored into an `agent_servers`
global the same way `DetachableRemoteAgents` is. `acp-beta` keeps offering it
too. The capability is negotiated at connection start, so a change applies to
new agent connections.

## Tasks

- [x] `ForkAgentSettingsContent::external_subagents` + `ForkAgentSettings` + `default.json`
- [x] `agent_servers::ExternalSubagents` global; `v1.rs` offers `subagents` when it or `acp-beta` is on (`offer_subagents` in `acp/external_subagents.rs`)
- [x] `agent_ui` fork init mirrors the setting into the global and follows settings changes
- [x] Tests: `subagents_follow_the_setting_without_acp_beta` (gate on/off, acp-beta alone); existing `client_capabilities_advertise_subagents_only_when_offered` and `v1_subagent_state_snapshots_drive_child_activity` cover the capability and the card's state path
- [x] `./script/clippy` clean; agent_servers 191, agent_settings + settings_content pass. `settings` has one failure that predates this plan (`test_agent_profile_tool_schema`, `list_threads` since `4765075d04`) → `.issues/041`, fixed in `439944d821` (schema also lacked `send_message`, `request_verdict`)
- [ ] Live check in the GUI with Claude Code background agents: awaiting a rebuild + Zed restart on this build (the running build predates it). Ask Claude Code to launch a background agent; expect a subagent card that spins while it runs and turns to a check when it finishes
