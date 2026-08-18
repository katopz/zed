//! Wire types shared between the Rust client and the Cloudflare Worker.
//!
//! Versioned (`v: 1`). Keep this file dependency-free beyond serde so it can be
//! reasoned about in isolation and matched 1:1 with the worker's JSON shapes.

use serde::{Deserialize, Serialize};

/// Schema version for every payload crossing the wire.
pub const SCHEMA_VERSION: u32 = 1;

/// What kind of artifact a scope tracks. Mirrors the operator's `.plans` /
/// `.issues` / `.proposals` convention from the user's AGENTS.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeKind {
    Plan,
    Issue,
    Proposal,
}

/// A single unit of work an agent is currently engaged with, as advertised to
/// other agents/devices so they don't clobber it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveScope {
    /// Opaque session id of the thread doing the work (e.g. an ACP session id).
    pub session_id: String,
    /// Absolute path of the plan/issue/proposal file, when known.
    pub plan_file: Option<String>,
    /// One-line human description ("implementing auth bridge").
    pub task_summary: String,
    /// Categorical kind of the scope, for display.
    pub scope_kind: ScopeKind,
}

/// Latest-wins status posted by one device. Overwritten on every heartbeat,
/// never appended — this is the "who is doing what right now" board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub v: u32,
    /// blake3(raw_ed25519_pubkey_32) hex — set by the worker from auth headers,
    /// but included in the payload for self-description on read.
    #[serde(default)]
    pub device_id: String,
    pub device_name: String,
    /// blake3(hostname + primary_iface_mac) hex — best-effort location fingerprint.
    #[serde(default)]
    pub location_hash: String,
    /// Absolute path of the active project / work dir.
    #[serde(default)]
    pub project_path: String,
    pub scopes: Vec<ActiveScope>,
    /// Unix millis of the last heartbeat that wrote this status.
    #[serde(default)]
    pub updated_at: i64,
    /// Set by the worker on read: true when the status is older than the stale window.
    #[serde(default)]
    pub stale: bool,
}

/// A short notepad message appended to the room feed. Capped at 1024 chars by
/// the worker. Only the most recent 10 are returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardMessage {
    pub v: u32,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    pub text: String,
    /// Unix millis.
    pub ts: i64,
}

/// Maximum length (bytes) of an [`AgentStateMessage::state_text`] and
/// [`AgentStateMessage::meta`]. Enforced client-side before posting and
/// server-side by the worker. Matches the operator's 256-char bound (point 8
/// of the Phase 2 spec).
pub const MAX_STATE_TEXT_BYTES: usize = 256;

/// How many state messages the room retains (ring buffer). Matches the
/// operator's "last 10" bound (point 7).
pub const MAX_ROOM_STATES: usize = 10;

/// A structured agent state broadcast: what an agent is doing/thinking right
/// now. Agents yell these at the board at plan-start and summary-occurrence so
/// peer agents can reason about each other. Both `state_text` and `meta` are
/// capped at [`MAX_STATE_TEXT_BYTES`]; the worker keeps only the last
/// [`MAX_ROOM_STATES`] per room.
///
/// Distinct from [`BoardMessage`] (free-text chat) because states are
/// structured and consumed by auto_prompt's `peer_agent_states` context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStateMessage {
    pub v: u32,
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    /// Opaque session id of the agent thread (ACP session id or native thread id).
    #[serde(default)]
    pub session_id: String,
    /// Optional sub-agent id (e.g. a delegated sub-task). When set, muting can
    /// target this specific sub-agent rather than the whole session.
    #[serde(default)]
    pub sub_agent_id: Option<String>,
    /// The state text (≤256 bytes): what the agent is doing right now.
    #[serde(default)]
    pub state_text: String,
    /// Structured metadata (≤256 bytes): plan name, phase, etc.
    #[serde(default)]
    pub meta: String,
    /// Unix millis.
    #[serde(default)]
    pub ts: i64,
}

/// Full snapshot returned by `GET /v1/rooms/{room}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    pub v: u32,
    pub room: String,
    pub statuses: Vec<DeviceStatus>,
    pub messages: Vec<BoardMessage>,
    /// Agent state broadcasts (Phase 2). Only present in snapshots from workers
    /// that support the `/state` endpoint; older workers omit the field.
    #[serde(default)]
    pub states: Vec<AgentStateMessage>,
    /// Web UI steering replies (Plan 015). Only present in snapshots from
    /// workers that support the `/reply` endpoint; older workers omit the field.
    #[serde(default)]
    pub replies: Vec<WebReply>,
}

/// Body for `POST /v1/rooms/{room}/status` — everything except `v` is
/// author-supplied; the worker stamps `device_id`/`updated_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostStatusBody {
    pub device_name: String,
    pub location_hash: String,
    pub project_path: String,
    pub scopes: Vec<ActiveScope>,
}

/// Body for `POST /v1/rooms/{room}/msg`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostMessageBody {
    #[serde(default)]
    pub device_name: String,
    pub text: String,
}

/// Body for `POST /v1/rooms/{room}/state` — append an agent state broadcast.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostStateBody {
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub sub_agent_id: Option<String>,
    /// Pre-truncated to [`MAX_STATE_TEXT_BYTES`] by the caller (defense in
    /// depth: the worker also truncates).
    pub state_text: String,
    #[serde(default)]
    pub meta: String,
}

/// A steering reply posted from the web UI (Plan 015). Targets a specific
/// device + agent by session-id prefix. The target device picks it up via
/// feeder poll (fallback) or real-time push (SSE/WebSocket) and injects it
/// into the agent thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebReply {
    pub v: u32,
    /// The target device name (e.g. "m3", "SHIKUWA").
    #[serde(default)]
    pub target_device: String,
    /// First 4 chars of the target agent's session_id (routing key).
    #[serde(default)]
    pub target_session_prefix: String,
    /// The reply text (capped at 1024 chars by the worker).
    #[serde(default)]
    pub text: String,
    /// GitHub login of the poster (always the allowlisted login for now).
    #[serde(default)]
    pub author_login: String,
    /// Unix millis.
    #[serde(default)]
    pub ts: i64,
}

/// Truncate a string to at most `max_bytes` without splitting a UTF-8
/// character. Walks backwards from the limit to the nearest char boundary.
/// Matches the inline pattern used throughout the Zed codebase (e.g.
/// `auto_prompt/src/context.rs`, `agent/src/thread.rs`).
pub fn truncate_to_byte_budget(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// A mute target: identifies what to suppress from chat/context injection.
/// Any field set to `None` is a wildcard (match-all). All fields `None` is a
/// catch-all mute (suppress everything). Matches the operator's Phase 2 spec
/// point 5: "foo/bar can select what to mute (per-agent, per-sub-agent,
/// per-device)".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuteKey {
    /// Mute all states from this device id (blake3 hex). None = any device.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Mute all states from this session id. None = any session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Mute a specific sub-agent. None = any (or all) sub-agents.
    #[serde(default)]
    pub sub_agent_id: Option<String>,
}

impl MuteKey {
    /// Returns true if this mute key matches the given state message. A `None`
    /// field acts as a wildcard.
    pub fn matches(&self, state: &AgentStateMessage) -> bool {
        self.device_id.as_deref().is_none_or(|d| d == state.device_id)
            && self.session_id
                .as_deref()
                .is_none_or(|s| s == state.session_id)
            && self
                .sub_agent_id
                .as_deref()
                .is_none_or(|s| state.sub_agent_id.as_deref() == Some(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_within_budget_keeps_all() {
        assert_eq!(truncate_to_byte_budget("hello", 10), "hello");
    }

    #[test]
    fn truncate_ascii_at_budget() {
        assert_eq!(truncate_to_byte_budget("hello", 5), "hello");
        assert_eq!(truncate_to_byte_budget("hello world", 5), "hello");
    }

    #[test]
    fn truncate_multibyte_does_not_split_char() {
        // "é" is 2 bytes (0xC3 0xA9). Truncating at byte 1 must roll back to 0.
        assert_eq!(truncate_to_byte_budget("é", 1), "");
        // "aé" — byte 1 lands mid-"é"; roll back to 1 ("a").
        assert_eq!(truncate_to_byte_budget("aé", 2), "a");
        // "ébc" — byte 2 is the char boundary between "é" and "b"; keep "é".
        assert_eq!(truncate_to_byte_budget("ébc", 2), "é");
        // "ébc" — byte 1 lands mid-"é"; roll back to 0.
        assert_eq!(truncate_to_byte_budget("ébc", 1), "");
    }

    #[test]
    fn truncate_emoji_does_not_split() {
        // "🦀" is 4 bytes. Truncating at byte 3 must roll back to 0.
        assert_eq!(truncate_to_byte_budget("🦀", 3), "");
        // "x🦀" — byte 4 is the start of the emoji; keep "x".
        assert_eq!(truncate_to_byte_budget("x🦀", 4), "x");
        // Full emoji fits at budget 4.
        assert_eq!(truncate_to_byte_budget("🦀", 4), "🦀");
    }

    #[test]
    fn truncate_at_256_char_boundary() {
        // Simulate an agent state text at the 256-byte bound.
        let text = "a".repeat(300);
        let truncated = truncate_to_byte_budget(&text, MAX_STATE_TEXT_BYTES);
        assert_eq!(truncated.len(), MAX_STATE_TEXT_BYTES);
        assert!(truncated.ends_with('a'));
    }

    #[test]
    fn agent_state_message_serializes() {
        let msg = AgentStateMessage {
            v: SCHEMA_VERSION,
            device_id: "abc123".to_string(),
            device_name: "m3-laptop".to_string(),
            session_id: "sess-1".to_string(),
            sub_agent_id: None,
            state_text: "debugging auth bridge".to_string(),
            meta: "plan: 013".to_string(),
            ts: 1700000000_000,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AgentStateMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn room_snapshot_defaults_states_for_old_worker() {
        // An old worker that doesn't know about `states` should deserialize
        // with an empty vec, not fail.
        let json = r#"{
            "v": 1,
            "room": "test",
            "statuses": [],
            "messages": []
        }"#;
        let snap: RoomSnapshot = serde_json::from_str(json).unwrap();
        assert!(snap.states.is_empty());
    }

    fn sample_state(
        device_id: &str,
        session_id: &str,
        sub_agent_id: Option<&str>,
    ) -> AgentStateMessage {
        AgentStateMessage {
            v: SCHEMA_VERSION,
            device_id: device_id.to_string(),
            device_name: "dev".to_string(),
            session_id: session_id.to_string(),
            sub_agent_id: sub_agent_id.map(|s| s.to_string()),
            state_text: "working".to_string(),
            meta: String::new(),
            ts: 0,
        }
    }

    #[test]
    fn mute_key_matches_exact() {
        let state = sample_state("dev-a", "sess-1", Some("sub-2"));
        let key = MuteKey {
            device_id: Some("dev-a".to_string()),
            session_id: Some("sess-1".to_string()),
            sub_agent_id: Some("sub-2".to_string()),
        };
        assert!(key.matches(&state));
    }

    #[test]
    fn mute_key_wildcard_device() {
        let state = sample_state("dev-a", "sess-1", None);
        let key = MuteKey {
            device_id: None,
            session_id: Some("sess-1".to_string()),
            sub_agent_id: None,
        };
        assert!(key.matches(&state));
    }

    #[test]
    fn mute_key_no_match_different_session() {
        let state = sample_state("dev-a", "sess-1", None);
        let key = MuteKey {
            device_id: None,
            session_id: Some("sess-other".to_string()),
            sub_agent_id: None,
        };
        assert!(!key.matches(&state));
    }

    #[test]
    fn mute_key_sub_agent_mismatch() {
        let state = sample_state("dev-a", "sess-1", Some("sub-2"));
        // Muting sub-1 should NOT match a state from sub-2.
        let key = MuteKey {
            device_id: None,
            session_id: None,
            sub_agent_id: Some("sub-1".to_string()),
        };
        assert!(!key.matches(&state));
    }

    #[test]
    fn mute_key_catch_all_matches_everything() {
        let state = sample_state("dev-a", "sess-1", Some("sub-2"));
        let key = MuteKey {
            device_id: None,
            session_id: None,
            sub_agent_id: None,
        };
        assert!(key.matches(&state));
    }

    // ── Worker contract validation ──
    // These verify that the exact JSON shapes produced by the Cloudflare Worker
    // (agent-board-worker/src/index.js) deserialize correctly into the Rust
    // wire types. If the worker's output shape drifts, these catch it without
    // needing a live deployment.

    #[test]
    fn worker_state_output_deserializes() {
        // Exact JSON shape from handlePostState in the worker.
        let json = r#"{
            "v": 1,
            "device_id": "a1b2c3",
            "device_name": "m3-laptop",
            "session_id": "sess-42",
            "sub_agent_id": null,
            "state_text": "debugging auth bridge",
            "meta": "summary",
            "ts": 1700000000000
        }"#;
        let state: AgentStateMessage = serde_json::from_str(json).unwrap();
        assert_eq!(state.device_id, "a1b2c3");
        assert_eq!(state.device_name, "m3-laptop");
        assert_eq!(state.session_id, "sess-42");
        assert!(state.sub_agent_id.is_none());
        assert_eq!(state.state_text, "debugging auth bridge");
        assert_eq!(state.meta, "summary");
    }

    #[test]
    fn worker_state_with_sub_agent_deserializes() {
        let json = r#"{
            "v": 1,
            "device_id": "dev-x",
            "device_name": "desktop",
            "session_id": "sess-1",
            "sub_agent_id": "investigator",
            "state_text": "researching",
            "meta": "plan: 013",
            "ts": 1700000000001
        }"#;
        let state: AgentStateMessage = serde_json::from_str(json).unwrap();
        assert_eq!(state.sub_agent_id.as_deref(), Some("investigator"));
    }

    #[test]
    fn worker_room_snapshot_with_states_deserializes() {
        // Exact JSON shape from handleGetRoom with states populated.
        let json = r#"{
            "v": 1,
            "room": "room-abc",
            "statuses": [
                {
                    "v": 1,
                    "device_id": "dev-a",
                    "device_name": "laptop",
                    "location_hash": "hash1",
                    "project_path": "/home/user/project",
                    "scopes": [],
                    "updated_at": 1700000000000,
                    "stale": false
                }
            ],
            "messages": [
                {
                    "v": 1,
                    "device_id": "dev-a",
                    "device_name": "laptop",
                    "text": "hello world",
                    "ts": 1700000000000
                }
            ],
            "states": [
                {
                    "v": 1,
                    "device_id": "dev-a",
                    "device_name": "laptop",
                    "session_id": "sess-1",
                    "sub_agent_id": null,
                    "state_text": "building feature X",
                    "meta": "plan: 013",
                    "ts": 1700000000001
                }
            ]
        }"#;
        let snap: RoomSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snap.room, "room-abc");
        assert_eq!(snap.statuses.len(), 1);
        assert_eq!(snap.messages.len(), 1);
        assert_eq!(snap.states.len(), 1);
        assert_eq!(snap.states[0].state_text, "building feature X");
    }

    #[test]
    fn worker_post_state_body_serializes_correctly() {
        // Verify that PostStateBody serializes to the JSON shape the worker's
        // handlePostState expects to parse.
        let body = crate::types::PostStateBody {
            device_name: "laptop".to_string(),
            session_id: "sess-1".to_string(),
            sub_agent_id: Some("sub-a".to_string()),
            state_text: "working".to_string(),
            meta: "plan: 013".to_string(),
        };
        let json = serde_json::to_string(&body).unwrap();
        // The worker reads body.device_name, body.session_id,
        // body.sub_agent_id, body.state_text, body.meta.
        assert!(json.contains("\"device_name\":\"laptop\""));
        assert!(json.contains("\"session_id\":\"sess-1\""));
        assert!(json.contains("\"sub_agent_id\":\"sub-a\""));
        assert!(json.contains("\"state_text\":\"working\""));
        assert!(json.contains("\"meta\":\"plan: 013\""));
    }

    // ── WebReply wire contract (Plan 015) ──

    #[test]
    fn web_reply_serializes() {
        let reply = WebReply {
            v: SCHEMA_VERSION,
            target_device: "m3".to_string(),
            target_session_prefix: "f3a2".to_string(),
            text: "stop and commit".to_string(),
            author_login: "katopz".to_string(),
            ts: 1700000000000,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: WebReply = serde_json::from_str(&json).unwrap();
        assert_eq!(reply, back);
    }

    #[test]
    fn worker_reply_output_deserializes() {
        // Exact JSON shape from handlePostReply / handleGetRoom reply objects.
        let json = r#"{
            "v": 1,
            "target_device": "SHIKUWA",
            "target_session_prefix": "b1c9",
            "text": "switch to develop first",
            "author_login": "katopz",
            "ts": 1700000000000
        }"#;
        let reply: WebReply = serde_json::from_str(json).unwrap();
        assert_eq!(reply.target_device, "SHIKUWA");
        assert_eq!(reply.target_session_prefix, "b1c9");
        assert_eq!(reply.text, "switch to develop first");
        assert_eq!(reply.author_login, "katopz");
    }

    #[test]
    fn room_snapshot_with_replies_deserializes() {
        let json = r#"{
            "v": 1,
            "room": "test",
            "statuses": [],
            "messages": [],
            "states": [],
            "replies": [
                {
                    "v": 1,
                    "target_device": "m3",
                    "target_session_prefix": "f3a2",
                    "text": "commit now",
                    "author_login": "katopz",
                    "ts": 1700000000001
                }
            ]
        }"#;
        let snap: RoomSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snap.replies.len(), 1);
        assert_eq!(snap.replies[0].target_device, "m3");
        assert_eq!(snap.replies[0].target_session_prefix, "f3a2");
    }

    #[test]
    fn room_snapshot_without_replies_defaults_empty() {
        // Old worker that doesn't include replies field → empty vec, not error.
        let json = r#"{
            "v": 1,
            "room": "test",
            "statuses": [],
            "messages": [],
            "states": []
        }"#;
        let snap: RoomSnapshot = serde_json::from_str(json).unwrap();
        assert!(snap.replies.is_empty());
    }
}
