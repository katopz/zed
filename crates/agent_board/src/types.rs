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

/// Full snapshot returned by `GET /v1/rooms/{room}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    pub v: u32,
    pub room: String,
    pub statuses: Vec<DeviceStatus>,
    pub messages: Vec<BoardMessage>,
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
