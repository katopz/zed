//! Process-global store for peer agent states, populated by `agent_board`'s
//! feeder and read by `decide_with_llm` / `decide_claude_with_hidden_thread`
//! to inject what other agents are doing into the decider's context.
//!
//! This module lives in `auto_prompt` (not `agent_board`) to avoid a circular
//! dependency: `agent_board` already depends on `auto_prompt::plan_registry`,
//! so the feeder can write here, while `auto_prompt` reads here without
//! depending on `agent_board`. The dependency direction is one-way:
//! `agent_board → auto_prompt::peer_states` (write), `auto_prompt` reads its
//! own module.
//!
//! The muted-set filter is a simple `Vec<PeerStateMute>` (defined here, not in
//! agent_board, to keep the filter logic local to the reader). `agent_board`
//! translates its config `MuteKey`s into these plain-struct filters when it
//! calls [`set_muted`].

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};

/// A trait object that broadcasts agent states to the board. Implemented by
/// `agent_board` (which holds the HTTP client + room), registered globally by
/// the panel during init. When None (board not configured), calls are no-ops.
/// This breaks the circular dependency: auto_prompt defines the trait, agent_board
/// implements it.
pub trait AgentStateBroadcaster: Send + Sync {
    /// Fire-and-forget: spawn a background post of the agent state. The
    /// implementation handles truncation + the actual HTTP call.
    fn broadcast(&self, session_id: &str, sub_agent_id: Option<&str>, state_text: &str, meta: &str);
}

/// A mute filter: any `None` field is a wildcard. Mirrors
/// `agent_board::types::MuteKey` but defined here to keep this module
/// dependency-free (agent_board copies into this shape at the boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStateMute {
    pub device_id: Option<String>,
    pub session_id: Option<String>,
    pub sub_agent_id: Option<String>,
}

/// A peer agent's state, as seen by the local device. Populated by
/// `agent_board`'s feeder from the room snapshot; read by the auto_prompt
/// deciders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAgentState {
    pub device_id: String,
    pub device_name: String,
    pub session_id: String,
    pub sub_agent_id: Option<String>,
    pub state_text: String,
}

static PEER_STATES: RwLock<Option<Vec<PeerAgentState>>> = RwLock::new(None);
static MUTED: RwLock<Vec<PeerStateMute>> = RwLock::new(Vec::new());
static BROADCASTER: RwLock<Option<Arc<dyn AgentStateBroadcaster>>> = RwLock::new(None);

/// Signatures of peer states that have already been injected as chat
/// notifications. A signature is `{device_id}\x1f{session_id}\x1f{sub_agent_id}\x1f{state_text}`
/// — when the same agent posts the same state_text again (heartbeat), we skip
/// re-notifying. When the state_text changes, the signature changes and a new
/// notification fires.
static NOTIFIED_SIGNATURES: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Register the agent-state broadcaster (called by `agent_board`'s panel
/// during init). Pass `None` to unregister (board disabled). When no
/// broadcaster is registered, [`broadcast_state`] is a silent no-op.
pub fn register_broadcaster(broadcaster: Option<Arc<dyn AgentStateBroadcaster>>) {
    if let Ok(mut guard) = BROADCASTER.write() {
        *guard = broadcaster;
    }
}

/// Broadcast an agent state to the board (fire-and-forget). No-op when no
/// broadcaster is registered. Callers (auto_prompt) treat the no-op as a
/// silent skip — the board is strictly additive.
pub fn broadcast_state(
    session_id: &str,
    sub_agent_id: Option<&str>,
    state_text: &str,
    meta: &str,
) {
    let broadcaster = match BROADCASTER.read() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    if let Some(broadcaster) = broadcaster {
        broadcaster.broadcast(session_id, sub_agent_id, state_text, meta);
    }
}

/// Replace the muted filter set. Called by `agent_board` when config loads.
pub fn set_muted(muted: Vec<PeerStateMute>) {
    if let Ok(mut guard) = MUTED.write() {
        *guard = muted;
    }
}

/// Replace the peer-state snapshot. Called by `agent_board`'s feeder on each
/// poll round. States from `own_device_id` are excluded (auto_prompt only
/// reasons about PEERS, not itself).
pub fn set_peer_states(states: Vec<PeerAgentState>, own_device_id: &str) {
    let peers: Vec<PeerAgentState> = states
        .into_iter()
        .filter(|s| s.device_id != own_device_id)
        .collect();
    if let Ok(mut guard) = PEER_STATES.write() {
        *guard = Some(peers);
    }
}

/// Read the latest unmuted peer states as a formatted context string.
/// Returns `None` when there are no visible peers (so the caller can omit the
/// field entirely and save tokens).
pub fn unmuted_states_for_context() -> Option<String> {
    let states = PEER_STATES.read().ok()?.clone()?;
    if states.is_empty() {
        return None;
    }
    let muted = MUTED.read().ok().map(|g| g.clone()).unwrap_or_default();
    let visible: Vec<&PeerAgentState> = states
        .iter()
        .filter(|s| !muted.iter().any(|m| matches_mute(m, s)))
        .collect();
    if visible.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(256 * visible.len().min(10));
    out.push_str("Peer agent states (what other agents are doing right now):\n");
    for state in visible.iter().take(10) {
        let label = state.sub_agent_id.as_deref().unwrap_or("(main)");
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            state.device_name, label, state.state_text
        ));
    }
    Some(out)
}

/// Drain peer states that haven't been notified to the UI yet. Returns
/// formatted notification strings and marks them as seen so heartbeats
/// (same state_text re-broadcast) don't re-trigger notifications.
/// Called by `agent_ui` on a foreground timer to inject into the active
/// agent thread. Muted states are excluded.
pub fn drain_unseen_notifications() -> Vec<String> {
    let states = match PEER_STATES.read() {
        Ok(guard) => guard.clone(),
        Err(_) => return Vec::new(),
    };
    let Some(states) = states else {
        return Vec::new();
    };
    let muted = MUTED
        .read()
        .map(|g| g.clone())
        .unwrap_or_default();

    let (mut seen, mut notifications) = match NOTIFIED_SIGNATURES.write() {
        Ok(mut guard) => (std::mem::take(&mut *guard), Vec::new()),
        Err(_) => (HashSet::new(), Vec::new()),
    };

    for state in &states {
        if muted.iter().any(|m| matches_mute(m, state)) {
            continue;
        }
        let sig = format_signature(state);
        if !seen.contains(&sig) {
            seen.insert(sig.clone());
            let label = state.sub_agent_id.as_deref().unwrap_or("(main)");
            let text = truncate_to_char_boundary(&state.state_text, 256);
            notifications.push(format!(
                "[peer] {} / {}: {}",
                state.device_name, label, text
            ));
        }
    }

    if let Ok(mut guard) = NOTIFIED_SIGNATURES.write() {
        *guard = seen;
    }

    notifications
}

fn format_signature(state: &PeerAgentState) -> String {
    format!(
        "{}\x1f{}\x1f{}\x1f{}",
        state.device_id,
        state.session_id,
        state.sub_agent_id.as_deref().unwrap_or(""),
        state.state_text,
    )
}

/// Truncate a string to at most `max_bytes` bytes, rolling back to the
/// previous UTF-8 char boundary if the cut point falls mid-character.
/// Process-killing to slice without this check.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn matches_mute(mute: &PeerStateMute, state: &PeerAgentState) -> bool {
    mute.device_id
        .as_deref()
        .is_none_or(|d| d == state.device_id)
        && mute
            .session_id
            .as_deref()
            .is_none_or(|s| s == state.session_id)
        && mute
            .sub_agent_id
            .as_deref()
            .is_none_or(|s| state.sub_agent_id.as_deref() == Some(s))
}

/// Clear all global state. Test-only.
#[cfg(test)]
pub fn clear_for_test() {
    if let Ok(mut guard) = PEER_STATES.write() {
        *guard = None;
    }
    if let Ok(mut guard) = MUTED.write() {
        guard.clear();
    }
    if let Ok(mut guard) = BROADCASTER.write() {
        *guard = None;
    }
    if let Ok(mut guard) = NOTIFIED_SIGNATURES.write() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the global test lock and clear all peer-states globals. The
    /// returned guard must be bound (`let _lock = setup();`) so it is held for
    /// the duration of the test, serialising all peer_states tests (they share
    /// process-global state). Mirrors the plan_registry test pattern.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_for_test();
        guard
    }

    fn state(device_id: &str, name: &str, session_id: &str, text: &str) -> PeerAgentState {
        PeerAgentState {
            device_id: device_id.to_string(),
            device_name: name.to_string(),
            session_id: session_id.to_string(),
            sub_agent_id: None,
            state_text: text.to_string(),
        }
    }

    #[test]
    fn excludes_own_device() {
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-a", "laptop", "s1", "debugging"),
                state("dev-b", "desktop", "s2", "building"),
            ],
            "dev-a",
        );
        let ctx = unmuted_states_for_context().unwrap();
        assert!(ctx.contains("desktop"));
        assert!(!ctx.contains("laptop"));
    }

    #[test]
    fn returns_none_when_empty() {
        let _lock = setup();
        set_peer_states(vec![], "dev-a");
        assert!(unmuted_states_for_context().is_none());
    }

    #[test]
    fn filters_muted_device() {
        let _lock = setup();
        set_peer_states(
            vec![state("dev-b", "desktop", "s2", "building")],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: Some("dev-b".to_string()),
            session_id: None,
            sub_agent_id: None,
        }]);
        assert!(unmuted_states_for_context().is_none());
    }

    #[test]
    fn caps_at_ten() {
        let _lock = setup();
        let states: Vec<_> = (0..15)
            .map(|i| state("dev-b", "desktop", &format!("s{i}"), &format!("task-{i}")))
            .collect();
        set_peer_states(states, "dev-a");
        let ctx = unmuted_states_for_context().unwrap();
        assert!(ctx.contains("task-9"));
        assert!(!ctx.contains("task-10"));
    }

    #[test]
    fn shows_sub_agent_label() {
        let _lock = setup();
        let mut s = state("dev-b", "desktop", "s2", "researching");
        s.sub_agent_id = Some("investigator".to_string());
        set_peer_states(vec![s], "dev-a");
        let ctx = unmuted_states_for_context().unwrap();
        assert!(ctx.contains("investigator"));
    }

    #[test]
    fn drain_returns_new_states_only_once() {
        let _lock = setup();
        set_peer_states(
            vec![state("dev-b", "desktop", "s2", "debugging auth")],
            "dev-a",
        );
        let first = drain_unseen_notifications();
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("desktop"));
        assert!(first[0].contains("debugging auth"));

        // Same state re-broadcast (heartbeat) → no new notification.
        let second = drain_unseen_notifications();
        assert!(second.is_empty(), "heartbeat should not re-notify");
    }

    #[test]
    fn drain_detects_state_text_change() {
        let _lock = setup();
        set_peer_states(
            vec![state("dev-b", "desktop", "s2", "debugging")],
            "dev-a",
        );
        let _ = drain_unseen_notifications();

        // Agent updates its state text → new notification fires.
        set_peer_states(
            vec![state("dev-b", "desktop", "s2", "fixed the bug")],
            "dev-a",
        );
        let second = drain_unseen_notifications();
        assert_eq!(second.len(), 1);
        assert!(second[0].contains("fixed the bug"));
    }

    #[test]
    fn drain_excludes_muted() {
        let _lock = setup();
        set_muted(vec![PeerStateMute {
            device_id: Some("dev-b".to_string()),
            session_id: None,
            sub_agent_id: None,
        }]);
        set_peer_states(
            vec![state("dev-b", "desktop", "s2", "working")],
            "dev-a",
        );
        let notifications = drain_unseen_notifications();
        assert!(notifications.is_empty(), "muted states should not notify");
    }
}
