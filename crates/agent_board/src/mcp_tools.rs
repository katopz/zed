//! MCP tools exposed by the agent board (Phase 2 point 9).
//!
//! `GetAgentRoom` returns the current room snapshot — devices, their agent
//! states, and the last messages — so any agent (native or Claude Code) can
//! query what peers are doing via a standard MCP tool call.
//!
//! The tool reads from the process-global snapshot cache
//! ([`crate::board_state::current_room_snapshot`]), which the feeder updates on
//! each poll round. When no snapshot is available (board not started, or no
//! poll has succeeded yet), the tool returns a "no data yet" message rather
//! than an error — agents should gracefully degrade.

use context_server::listener::{McpServerTool, ToolResponse};
use context_server::types::ToolResponseContent;
use gpui::AsyncApp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::war_room::{WorkItem, build_work_board};

/// Returns the current agent room snapshot: all devices, their latest agent
/// states (what each agent is doing right now), and the last short messages.
/// Call this when you want to know what peer agents are working on.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetAgentRoomInput {}

/// One device's contribution to the room.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoomDevice {
    pub device_id: String,
    pub device_name: String,
    pub states: Vec<RoomDeviceState>,
}

/// A single agent state broadcast, summarized for the MCP output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RoomDeviceState {
    pub session_id: String,
    pub sub_agent_id: Option<String>,
    pub state_text: String,
    pub meta: String,
}

/// The tool's structured output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetAgentRoomOutput {
    pub room: String,
    pub devices: Vec<RoomDevice>,
    /// Pinned work board (Plan 024 P7): who is doing/stale/released on which
    /// plan, race conflicts flagged. Read this before grabbing a plan.
    #[serde(default)]
    pub work_board: Vec<WorkItem>,
}

/// MCP tool: get the current agent room snapshot.
///
/// Registered on a default-on `McpServer` during runtime init so both native
/// and Claude Code agents can call `get_agent_room` to discover what peer
/// agents are doing. No arguments — returns the full room.
#[derive(Clone)]
pub struct GetAgentRoom;

impl McpServerTool for GetAgentRoom {
    type Input = GetAgentRoomInput;
    type Output = GetAgentRoomOutput;
    const NAME: &'static str = "get_agent_room";

    fn annotations(&self) -> context_server::types::ToolAnnotations {
        context_server::types::ToolAnnotations {
            title: Some("Get Agent Room".to_string()),
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }
    }

    fn run(
        &self,
        _input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> impl std::future::Future<Output = anyhow::Result<ToolResponse<Self::Output>>> {
        let result = build_response();
        std::future::ready(result)
    }
}

/// Input for `post_agent_board_message` (Plan 024 P4).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PostAgentBoardMessageInput {
    /// Message text. Use `@device:sess4 <text>` to command another agent
    /// (labels come from `get_agent_room`). Mention cooldowns apply; do not
    /// spam peers.
    pub text: String,
    /// Your own `device:sess4` label when known — self-mentions are dropped
    /// by the routing guard.
    #[serde(default)]
    pub sender: String,
}

/// Output for `post_agent_board_message`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PostAgentBoardMessageOutput {
    pub posted: bool,
}

/// MCP tool: post a message to the shared war-room feed. Gives agents a
/// voice: ask peers, answer the operator, or `@device:sess4`-command another
/// agent through the same mention routing the operator uses.
#[derive(Clone)]
pub struct PostAgentBoardMessage;

impl McpServerTool for PostAgentBoardMessage {
    type Input = PostAgentBoardMessageInput;
    type Output = PostAgentBoardMessageOutput;
    const NAME: &'static str = "post_agent_board_message";

    fn annotations(&self) -> context_server::types::ToolAnnotations {
        context_server::types::ToolAnnotations {
            title: Some("Post Agent Board Message".to_string()),
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }
    }

    fn run(
        &self,
        input: Self::Input,
        _cx: &mut AsyncApp,
    ) -> impl std::future::Future<Output = anyhow::Result<ToolResponse<Self::Output>>> {
        let posted = !input.text.trim().is_empty();
        if posted {
            let sender = if input.sender.trim().is_empty() {
                "agent".to_string()
            } else {
                input.sender.trim().to_string()
            };
            crate::board_state::post_message(input.text.trim(), &sender);
        }
        let output = PostAgentBoardMessageOutput { posted };
        std::future::ready(Ok(ToolResponse {
            content: vec![ToolResponseContent::Text {
                text: if posted {
                    "posted to the war room feed".to_string()
                } else {
                    "not posted: empty text".to_string()
                },
            }],
            structured_content: output,
        }))
    }
}

/// Build the tool response from the global snapshot cache. Returns an error
/// response (not a Rust error) when no snapshot is available, so the agent sees
/// a clean message instead of a tool failure.
fn build_response() -> anyhow::Result<ToolResponse<GetAgentRoomOutput>> {
    match crate::board_state::current_room_snapshot() {
        Some(snapshot) => {
            // Group states by device_id, matching each to its DeviceStatus for
            // the display name.
            let mut devices: Vec<RoomDevice> = Vec::new();
            for status in &snapshot.statuses {
                let states: Vec<RoomDeviceState> = snapshot
                    .states
                    .iter()
                    .filter(|s| s.device_id == status.device_id)
                    .map(|s| RoomDeviceState {
                        session_id: s.session_id.clone(),
                        sub_agent_id: s.sub_agent_id.clone(),
                        state_text: s.state_text.clone(),
                        meta: s.meta.clone(),
                    })
                    .collect();
                devices.push(RoomDevice {
                    device_id: status.device_id.clone(),
                    device_name: status.device_name.clone(),
                    states,
                });
            }
            // Also include states from devices with no active status post.
            for state in &snapshot.states {
                if !devices.iter().any(|d| d.device_id == state.device_id) {
                    devices.push(RoomDevice {
                        device_id: state.device_id.clone(),
                        device_name: state.device_name.clone(),
                        states: vec![RoomDeviceState {
                            session_id: state.session_id.clone(),
                            sub_agent_id: state.sub_agent_id.clone(),
                            state_text: state.state_text.clone(),
                            meta: state.meta.clone(),
                        }],
                    });
                }
            }

            let output = GetAgentRoomOutput {
                room: snapshot.room.clone(),
                devices,
                work_board: build_work_board(
                    &snapshot,
                    &auto_prompt::plan_registry::active_claims(),
                    &crate::board_state::device_name().unwrap_or_default(),
                    now_unix_ms(),
                ),
            };
            let text = format_room_text(&output);
            Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text }],
                structured_content: output,
            })
        }
        None => {
            let text = "Agent board not started or no data yet. \
                        Configure the board in ~/.config/zed/agent_board.json to see peer agents."
                .to_string();
            Ok(ToolResponse {
                content: vec![ToolResponseContent::Text { text }],
                structured_content: GetAgentRoomOutput {
                    room: String::new(),
                    devices: Vec::new(),
                    work_board: Vec::new(),
                },
            })
        }
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Human-readable summary for the text content of the response. Agents that
/// don't parse structured output still get a useful string.
fn format_room_text(output: &GetAgentRoomOutput) -> String {
    if output.room.is_empty() {
        return "No room configured.".to_string();
    }
    let mut lines = Vec::with_capacity(output.devices.len() + output.work_board.len() + 1);
    lines.push(format!("Room: {}", output.room));
    if !output.work_board.is_empty() {
        lines.push("Work board (do NOT pick plans marked doing/stale):".to_string());
        for item in &output.work_board {
            let flag = if item.race { " [RACE]" } else { "" };
            lines.push(format!(
                "  {} {flag} {} — {}",
                match item.state {
                    crate::war_room::WorkState::Doing => "doing",
                    crate::war_room::WorkState::Stale => "stale",
                    crate::war_room::WorkState::Released => "released",
                },
                item.plan_name,
                item.owner_labels.join(", "),
            ));
        }
    }
    for device in &output.devices {
        if device.states.is_empty() {
            lines.push(format!("  {} ({}): no active states", device.device_name, device.device_id));
        } else {
            for state in &device.states {
                let sub = state.sub_agent_id.as_deref().unwrap_or("main");
                lines.push(format!(
                    "  {} [{}/{}]: {}",
                    device.device_name, state.session_id, sub, state.state_text
                ));
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_empty_room() {
        let output = GetAgentRoomOutput {
            room: String::new(),
            devices: Vec::new(),
            work_board: Vec::new(),
        };
        assert_eq!(format_room_text(&output), "No room configured.");
    }

    #[test]
    fn format_room_with_states() {
        let output = GetAgentRoomOutput {
            room: "test-room".to_string(),
            devices: vec![RoomDevice {
                device_id: "dev-a".to_string(),
                device_name: "laptop".to_string(),
                states: vec![RoomDeviceState {
                    session_id: "sess-1".to_string(),
                    sub_agent_id: None,
                    state_text: "debugging".to_string(),
                    meta: String::new(),
                }],
            }],
            work_board: Vec::new(),
        };
        let text = format_room_text(&output);
        assert!(text.contains("test-room"));
        assert!(text.contains("laptop"));
        assert!(text.contains("debugging"));
    }

    // ── build_response tests ──
    // These exercise the full MCP tool pipeline: set a room snapshot via the
    // global cache, call build_response(), and verify the grouped output.
    // Each test clears the cache first and holds `TEST_LOCK` for its whole
    // body so parallel test threads never interleave global writes.

    #[test]
    fn build_response_with_no_snapshot_returns_graceful_message() {
        let _state_guard =
            crate::board_state::TEST_LOCK.lock().expect("board-state test lock poisoned");
        crate::board_state::clear_for_test();
        let result = build_response().unwrap();
        assert_eq!(result.structured_content.room, "");
        assert!(result.structured_content.devices.is_empty());
        let text = result.content.first().and_then(|c| match c {
            ToolResponseContent::Text { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(text.unwrap().contains("not started"));
    }

    #[test]
    fn build_response_groups_states_by_device() {
        let _state_guard =
            crate::board_state::TEST_LOCK.lock().expect("board-state test lock poisoned");
        crate::board_state::clear_for_test();
        let snapshot = crate::types::RoomSnapshot {
            v: 1,
            room: "room-42".to_string(),
            statuses: vec![
                crate::types::DeviceStatus {
                    v: 1,
                    device_id: "dev-a".to_string(),
                    device_name: "laptop".to_string(),
                    location_hash: String::new(),
                    project_path: String::new(),
                    scopes: vec![],
                    updated_at: 0,
                    stale: false,
                },
                crate::types::DeviceStatus {
                    v: 1,
                    device_id: "dev-b".to_string(),
                    device_name: "desktop".to_string(),
                    location_hash: String::new(),
                    project_path: String::new(),
                    scopes: vec![],
                    updated_at: 0,
                    stale: false,
                },
            ],
            messages: vec![],
            states: vec![
                crate::types::AgentStateMessage {
                    v: 1,
                    device_id: "dev-a".to_string(),
                    device_name: "laptop".to_string(),
                    session_id: "s1".to_string(),
                    sub_agent_id: None,
                    state_text: "debugging".to_string(),
                    meta: String::new(),
                    ts: 1000,
                },
                crate::types::AgentStateMessage {
                    v: 1,
                    device_id: "dev-a".to_string(),
                    device_name: "laptop".to_string(),
                    session_id: "s2".to_string(),
                    sub_agent_id: Some("sub-x".to_string()),
                    state_text: "researching".to_string(),
                    meta: String::new(),
                    ts: 2000,
                },
                crate::types::AgentStateMessage {
                    v: 1,
                    device_id: "dev-b".to_string(),
                    device_name: "desktop".to_string(),
                    session_id: "s3".to_string(),
                    sub_agent_id: None,
                    state_text: "building".to_string(),
                    meta: String::new(),
                    ts: 3000,
                },
            ],
            replies: vec![],
        };
        crate::board_state::set_room_snapshot(snapshot);

        let result = build_response().unwrap();
        assert_eq!(result.structured_content.room, "room-42");
        assert_eq!(result.structured_content.devices.len(), 2);

        let dev_a = result
            .structured_content
            .devices
            .iter()
            .find(|d| d.device_id == "dev-a")
            .unwrap();
        assert_eq!(dev_a.device_name, "laptop");
        assert_eq!(dev_a.states.len(), 2, "dev-a has two states");

        let dev_b = result
            .structured_content
            .devices
            .iter()
            .find(|d| d.device_id == "dev-b")
            .unwrap();
        assert_eq!(dev_b.states.len(), 1);
        assert_eq!(dev_b.states[0].state_text, "building");

        // Text output includes all devices.
        let text = result.content.first().and_then(|c| match c {
            ToolResponseContent::Text { text } => Some(text.as_str()),
            _ => None,
        });
        let text = text.unwrap();
        assert!(text.contains("room-42"));
        assert!(text.contains("laptop"));
        assert!(text.contains("desktop"));
        assert!(text.contains("sub-x"));
    }

    #[test]
    fn build_response_includes_orphan_states_without_status() {
        // A device posted a state but never posted a status — the state should
        // still appear in the output as an "orphan" device entry.
        let _state_guard =
            crate::board_state::TEST_LOCK.lock().expect("board-state test lock poisoned");
        crate::board_state::clear_for_test();
        let snapshot = crate::types::RoomSnapshot {
            v: 1,
            room: "room-99".to_string(),
            statuses: vec![], // no statuses at all
            messages: vec![],
            states: vec![crate::types::AgentStateMessage {
                v: 1,
                device_id: "dev-ghost".to_string(),
                device_name: "ghost".to_string(),
                session_id: "s1".to_string(),
                sub_agent_id: None,
                state_text: "haunting".to_string(),
                meta: String::new(),
                ts: 5000,
            }],
            replies: vec![],
        };
        crate::board_state::set_room_snapshot(snapshot);

        let result = build_response().unwrap();
        assert_eq!(result.structured_content.devices.len(), 1);
        assert_eq!(result.structured_content.devices[0].device_id, "dev-ghost");
        assert_eq!(result.structured_content.devices[0].device_name, "ghost");
        assert_eq!(result.structured_content.devices[0].states[0].state_text, "haunting");
    }

    #[test]
    fn build_response_includes_work_board_from_scopes() {
        let _state_guard =
            crate::board_state::TEST_LOCK.lock().expect("board-state test lock poisoned");
        crate::board_state::clear_for_test();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let snapshot = crate::types::RoomSnapshot {
            v: 1,
            room: "room-wb".to_string(),
            statuses: vec![crate::types::DeviceStatus {
                v: 1,
                device_id: "dev-a".to_string(),
                device_name: "laptop".to_string(),
                location_hash: String::new(),
                project_path: String::new(),
                scopes: vec![crate::types::ActiveScope {
                    session_id: "b1c9ffff".to_string(),
                    plan_file: Some("/repo/.plans/024_war.md".to_string()),
                    task_summary: "war room work".to_string(),
                    scope_kind: crate::types::ScopeKind::Plan,
                }],
                updated_at: now - 30_000,
                stale: false,
            }],
            messages: vec![],
            states: vec![],
            replies: vec![],
        };
        crate::board_state::set_room_snapshot(snapshot);

        let result = build_response().unwrap();
        assert_eq!(result.structured_content.work_board.len(), 1);
        let item = &result.structured_content.work_board[0];
        assert_eq!(item.plan_name, "024_war.md");
        assert_eq!(item.state, crate::war_room::WorkState::Doing);
        assert_eq!(item.owner_labels, vec!["laptop:b1c9".to_string()]);

        let text = result.content.first().and_then(|c| match c {
            ToolResponseContent::Text { text } => Some(text.as_str()),
            _ => None,
        });
        let text = text.unwrap();
        assert!(text.contains("Work board"));
        assert!(text.contains("024_war.md"));
    }

    #[test]
    fn build_response_empty_room_has_devices_but_no_states() {
        // Devices with statuses but no state broadcasts: each appears with an
        // empty states vec, and the text says "no active states".
        let _state_guard =
            crate::board_state::TEST_LOCK.lock().expect("board-state test lock poisoned");
        crate::board_state::clear_for_test();
        let snapshot = crate::types::RoomSnapshot {
            v: 1,
            room: "quiet-room".to_string(),
            statuses: vec![crate::types::DeviceStatus {
                v: 1,
                device_id: "dev-silent".to_string(),
                device_name: "silent".to_string(),
                location_hash: String::new(),
                project_path: String::new(),
                scopes: vec![],
                updated_at: 0,
                stale: false,
            }],
            messages: vec![],
            states: vec![],
            replies: vec![],
        };
        crate::board_state::set_room_snapshot(snapshot);

        let result = build_response().unwrap();
        assert_eq!(result.structured_content.devices.len(), 1);
        assert!(result.structured_content.devices[0].states.is_empty());
        let text = result.content.first().and_then(|c| match c {
            ToolResponseContent::Text { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(text.unwrap().contains("no active states"));
    }
}
