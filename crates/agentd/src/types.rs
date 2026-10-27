use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub type ClientId = u64;

/// The ACP agent process the daemon keeps alive on behalf of detachable clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl BackendSpec {
    /// Sessions are scoped to a backend by this key, so two different agents
    /// (e.g. claude and codex) never see each other's session lists.
    pub fn key(&self) -> String {
        let mut key = self.command.clone();
        for arg in &self.args {
            key.push('\u{1f}');
            key.push_str(arg);
        }
        key
    }
}

/// What to do with a backend → client request (tool permission prompt) when no
/// GUI is attached to the session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DetachedPolicy {
    /// Hold the request until a client reattaches.
    #[default]
    Wait,
    Allow,
    Deny,
}

/// First line a connection sends, before any ACP traffic.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Handshake {
    Attach {
        backend: BackendSpec,
        #[serde(default)]
        detached_permission: DetachedPolicy,
    },
    Status,
    Stop,
    Gc {
        #[serde(default)]
        policy: RetentionPolicy,
    },
}

/// Journal retention. Only idle sessions are ever evicted: not live in a
/// backend, no client attached, nothing queued or in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Evict idle sessions not updated for this many days.
    pub max_age_days: u64,
    /// Then evict the oldest idle sessions until the update logs fit.
    pub max_total_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub removed_sessions: Vec<String>,
    pub freed_bytes: u64,
    pub kept_sessions: usize,
    pub kept_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandshakeEnvelope {
    pub agentd: Handshake,
}

/// Persisted per-session metadata. `setup` is the backend's `session/new` result
/// (minus `sessionId`) so `session/load` can answer with the same modes/models.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub backend_key: String,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub setup: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: PathBuf,
    pub updated_at: String,
    pub live: bool,
    pub attached_clients: usize,
    pub queued_requests: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub clients: usize,
    pub backends: Vec<String>,
    pub sessions: Vec<SessionStatus>,
}

/// Inputs to the routing state machine.
#[derive(Debug)]
pub enum HubEvent {
    ClientConnected {
        client: ClientId,
        backend: BackendSpec,
        detached_policy: DetachedPolicy,
    },
    ClientMessage {
        client: ClientId,
        message: Value,
    },
    ClientDisconnected {
        client: ClientId,
    },
    BackendMessage {
        backend_key: String,
        generation: u64,
        message: Value,
    },
    BackendExited {
        backend_key: String,
        generation: u64,
    },
}

/// Side effects the state machine asks the IO layer to perform.
#[derive(Debug, PartialEq)]
pub enum HubOutput {
    ToClient { client: ClientId, message: Value },
    ToBackend { backend_key: String, message: Value },
    SpawnBackend { spec: BackendSpec, generation: u64 },
}
