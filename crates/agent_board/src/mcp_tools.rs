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
}

/// MCP tool: get the current agent room snapshot.
///
/// Registered on a default-on `McpServer` during panel init so both native and
/// Claude Code agents can call `get_agent_room` to discover what peer agents are
/// doing. No arguments — returns the full room.
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
                },
            })
        }
    }
}

/// Human-readable summary for the text content of the response. Agents that
/// don't parse structured output still get a useful string.
fn format_room_text(output: &GetAgentRoomOutput) -> String {
    if output.room.is_empty() {
        return "No room configured.".to_string();
    }
    let mut lines = Vec::with_capacity(output.devices.len() + 1);
    lines.push(format!("Room: {}", output.room));
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
        };
        let text = format_room_text(&output);
        assert!(text.contains("test-room"));
        assert!(text.contains("laptop"));
        assert!(text.contains("debugging"));
    }
}
