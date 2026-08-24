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

    /// Fire-and-forget batch of thread-timeline entries (Plan 026 web Threads
    /// tab). Default no-op so existing implementations (tests, boards without
    /// the endpoint) are unaffected.
    fn broadcast_thread_update(
        &self,
        _session_id: &str,
        _title: Option<&str>,
        _entries: &[ThreadEntry],
    ) {
    }
}

/// One thread-timeline entry mirrored to the web Threads tab: a user /
/// assistant / tool turn rendered to markdown by the producer (agent_ui),
/// capped before it reaches the broadcaster. `seq` is the entry's index in
/// the local thread — consumers upsert by it so re-sends (streaming growth)
/// replace instead of duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadEntry {
    pub seq: u64,
    pub role: String,
    pub text: String,
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

/// Pending web replies (Plan 015): steering replies posted from the browser
/// that are waiting to be injected into agent threads. Keyed by the 4-char
/// session_id prefix so the agent_ui can resolve them by prefix-matching
/// active session_ids. Drained by the notification timer in agent_panel.
static WEB_REPLIES: LazyLock<RwLock<Vec<(String, String)>>> =
    LazyLock::new(|| RwLock::new(Vec::new()));

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

/// Broadcast new thread-timeline entries for a session (fire-and-forget,
/// Plan 026). Same no-op semantics as [`broadcast_state`].
pub fn broadcast_thread_update(
    session_id: &str,
    title: Option<&str>,
    entries: &[ThreadEntry],
) {
    let broadcaster = match BROADCASTER.read() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    if let Some(broadcaster) = broadcaster {
        broadcaster.broadcast_thread_update(session_id, title, entries);
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

/// Inject a pending web reply (Plan 015). Called by the agent_board feeder
/// when it finds a reply targeting this device in the room snapshot, or by
/// the real-time SSE/WebSocket client when a reply is pushed. The reply is
/// queued by its 4-char session-id prefix and drained by the agent_panel
/// notification timer, which resolves the prefix to an active session and
/// injects the text into the agent thread.
pub fn inject_web_reply(session_prefix: String, text: String) {
    if let Ok(mut guard) = WEB_REPLIES.write() {
        guard.push((session_prefix, text));
    }
}

/// Drain all pending web replies. Called by the agent_panel notification
/// timer alongside `drain_unseen_notifications`. Returns `(session_prefix, text)`
/// pairs. The caller resolves the prefix to an active AcpThread and injects
/// the text via `send` (native: + steering; Claude: regular send).
pub fn drain_web_replies() -> Vec<(String, String)> {
    if let Ok(mut guard) = WEB_REPLIES.write() {
        std::mem::take(&mut *guard)
    } else {
        Vec::new()
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
    if let Ok(mut guard) = WEB_REPLIES.write() {
        guard.clear();
    }
}

/// Acquire the process-wide test lock so callers in other modules' test
/// suites (e.g. `claude_agent::tests`) can serialize their access to the
/// peer_states globals. The returned guard must be held for the duration of
/// the test. Also clears all globals so the test starts from a known state.
#[cfg(test)]
pub fn lock_for_test() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Acquire the global test lock and clear all peer-states globals. The
    /// returned guard must be bound (`let _lock = setup();`) so it is held for
    /// the duration of the test, serialising all peer_states tests (they share
    /// process-global state). Mirrors the plan_registry test pattern.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = lock_for_test();
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

    // ── Mute filtering gap tests (GOAT gate: "Muting works") ──
    // These exercise mute dimensions not covered by the basic device-id mute.

    #[test]
    fn mute_by_session_id_only() {
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-b", "desktop", "sess-muted", "building"),
                state("dev-b", "desktop", "sess-live", "testing"),
            ],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: None,
            session_id: Some("sess-muted".to_string()),
            sub_agent_id: None,
        }]);
        let ctx = unmuted_states_for_context().unwrap();
        assert!(!ctx.contains("building"), "muted session excluded from context");
        assert!(ctx.contains("testing"), "unmuted session still in context");
    }

    #[test]
    fn mute_by_sub_agent_id_only() {
        let _lock = setup();
        let mut muted_state = state("dev-b", "desktop", "s2", "investigating");
        muted_state.sub_agent_id = Some("sub-investigator".to_string());
        let mut live_state = state("dev-b", "desktop", "s3", "coding");
        live_state.sub_agent_id = Some("sub-coder".to_string());
        set_peer_states(vec![muted_state, live_state], "dev-a");
        set_muted(vec![PeerStateMute {
            device_id: None,
            session_id: None,
            sub_agent_id: Some("sub-investigator".to_string()),
        }]);
        let ctx = unmuted_states_for_context().unwrap();
        assert!(!ctx.contains("investigating"));
        assert!(ctx.contains("coding"));
    }

    #[test]
    fn mute_sub_agent_with_none_state_sub_agent_does_not_match() {
        // A muted sub_agent_id of Some("x") should NOT match a state whose
        // sub_agent_id is None. This is the asymmetric matching documented in
        // matches_mute.
        let _lock = setup();
        let state_none_sub = state("dev-b", "desktop", "s2", "working");
        set_peer_states(vec![state_none_sub], "dev-a");
        set_muted(vec![PeerStateMute {
            device_id: None,
            session_id: None,
            sub_agent_id: Some("some-sub".to_string()),
        }]);
        let ctx = unmuted_states_for_context();
        assert!(ctx.is_some(), "state with None sub_agent_id not muted by Some sub");
    }

    #[test]
    fn mute_compound_device_and_session() {
        // Muting by {device_id + session_id} should only match that exact combo.
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-b", "desktop", "target-session", "should-be-muted"),
                state("dev-b", "desktop", "other-session", "should-be-visible"),
            ],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: Some("dev-b".to_string()),
            session_id: Some("target-session".to_string()),
            sub_agent_id: None,
        }]);
        let ctx = unmuted_states_for_context().unwrap();
        assert!(!ctx.contains("should-be-muted"));
        assert!(ctx.contains("should-be-visible"));
    }

    #[test]
    fn mute_all_none_wildcard_mutes_everything() {
        // All-None mute key = wildcard, mutes everything.
        let _lock = setup();
        set_peer_states(
            vec![state("dev-b", "desktop", "s1", "task-a")],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: None,
            session_id: None,
            sub_agent_id: None,
        }]);
        assert!(unmuted_states_for_context().is_none());
    }

    #[test]
    fn mute_partial_only_some_peers_muted() {
        // Multiple peers: only one is muted, the rest remain visible.
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-b", "desktop", "s1", "muted-task"),
                state("dev-c", "server", "s2", "visible-task-1"),
                state("dev-d", "laptop", "s3", "visible-task-2"),
            ],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: Some("dev-b".to_string()),
            session_id: None,
            sub_agent_id: None,
        }]);
        let ctx = unmuted_states_for_context().unwrap();
        assert!(!ctx.contains("muted-task"));
        assert!(ctx.contains("visible-task-1"));
        assert!(ctx.contains("visible-task-2"));
    }

    #[test]
    fn mute_multiple_entries_or_semantics() {
        // Multiple PeerStateMute entries in the vec act as OR: if any matches,
        // the state is muted.
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-b", "desktop", "s1", "muted-by-device"),
                state("dev-c", "server", "s2", "muted-by-session"),
                state("dev-d", "laptop", "s3", "visible"),
            ],
            "dev-a",
        );
        set_muted(vec![
            PeerStateMute {
                device_id: Some("dev-b".to_string()),
                session_id: None,
                sub_agent_id: None,
            },
            PeerStateMute {
                device_id: None,
                session_id: Some("s2".to_string()),
                sub_agent_id: None,
            },
        ]);
        let ctx = unmuted_states_for_context().unwrap();
        assert!(!ctx.contains("muted-by-device"));
        assert!(!ctx.contains("muted-by-session"));
        assert!(ctx.contains("visible"));
    }

    #[test]
    fn mute_filters_drain_notifications_too() {
        // Muting should suppress drain notifications for the muted peer while
        // allowing unmuted peers through.
        let _lock = setup();
        set_peer_states(
            vec![
                state("dev-b", "desktop", "s1", "muted-notification"),
                state("dev-c", "server", "s2", "live-notification"),
            ],
            "dev-a",
        );
        set_muted(vec![PeerStateMute {
            device_id: Some("dev-b".to_string()),
            session_id: None,
            sub_agent_id: None,
        }]);
        let notifications = drain_unseen_notifications();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0].contains("live-notification"));
        assert!(!notifications.iter().any(|n| n.contains("muted-notification")));
    }

    // ── Broadcaster forwarding tests (GOAT gate: "Both agents post + read") ──

    /// A mock broadcaster that records all calls for assertion.
    struct MockBroadcaster {
        calls: std::sync::Mutex<Vec<(String, Option<String>, String, String)>>,
    }

    impl AgentStateBroadcaster for MockBroadcaster {
        fn broadcast(
            &self,
            session_id: &str,
            sub_agent_id: Option<&str>,
            state_text: &str,
            meta: &str,
        ) {
            self.calls
                .lock()
                .unwrap()
                .push((
                    session_id.to_string(),
                    sub_agent_id.map(|s| s.to_string()),
                    state_text.to_string(),
                    meta.to_string(),
                ));
        }
    }

    #[test]
    fn broadcast_state_forwards_to_registered_broadcaster() {
        let _lock = setup();
        let mock = Arc::new(MockBroadcaster {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        register_broadcaster(Some(mock.clone()));

        broadcast_state("sess-1", Some("sub-a"), "debugging", "summary");
        broadcast_state("sess-2", None, "building", "plan: 013");

        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "both broadcasts forwarded");
        assert_eq!(calls[0].0, "sess-1");
        assert_eq!(calls[0].1.as_deref(), Some("sub-a"));
        assert_eq!(calls[0].2, "debugging");
        assert_eq!(calls[0].3, "summary");
        assert_eq!(calls[1].1, None, "None sub_agent_id passed through");
    }

    #[test]
    fn broadcast_state_noop_without_broadcaster() {
        let _lock = setup();
        // No broadcaster registered — should be a silent no-op, not a panic.
        broadcast_state("sess-1", None, "working", "test");
        // If we get here without panicking, the test passes.
    }

    #[test]
    fn broadcast_thread_update_forwards_and_defaults_noop() {
        let _lock = setup();
        // Default (no override) implementations must tolerate the call.
        struct Bare;
        impl AgentStateBroadcaster for Bare {
            fn broadcast(&self, _: &str, _: Option<&str>, _: &str, _: &str) {}
        }
        register_broadcaster(Some(Arc::new(Bare {})));
        broadcast_thread_update("sess-1", None, &[]);
        broadcast_thread_update(
            "sess-1",
            Some("t"),
            &[ThreadEntry { seq: 0, role: "user".into(), text: "hi".into() }],
        );

        // And a recording override receives the batch verbatim.
        #[derive(Default)]
        struct Rec(std::sync::Mutex<Vec<(String, Option<String>, Vec<ThreadEntry>)>>);
        impl AgentStateBroadcaster for Rec {
            fn broadcast(&self, _: &str, _: Option<&str>, _: &str, _: &str) {}
            fn broadcast_thread_update(
                &self,
                session_id: &str,
                title: Option<&str>,
                entries: &[ThreadEntry],
            ) {
                self.0
                    .lock()
                    .unwrap()
                    .push((session_id.to_string(), title.map(str::to_string), entries.to_vec()));
            }
        }
        let rec = Arc::new(Rec::default());
        register_broadcaster(Some(rec.clone()));
        broadcast_thread_update(
            "sess-2",
            Some("title"),
            &[ThreadEntry { seq: 7, role: "assistant".into(), text: "done".into() }],
        );
        let calls = rec.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sess-2");
        assert_eq!(calls[0].1.as_deref(), Some("title"));
        assert_eq!(calls[0].2.len(), 1);
        assert_eq!(calls[0].2[0].seq, 7);
    }

    // ── Web reply tests (Plan 015) ──

    #[test]
    fn inject_and_drain_web_reply() {
        let _lock = setup();
        inject_web_reply("f3a2".to_string(), "commit now".to_string());
        let replies = drain_web_replies();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, "f3a2");
        assert_eq!(replies[0].1, "commit now");
    }

    #[test]
    fn drain_clears_pending_replies() {
        let _lock = setup();
        inject_web_reply("a1b2".to_string(), "reply 1".to_string());
        let first = drain_web_replies();
        assert_eq!(first.len(), 1);
        // Second drain should be empty — the first drain cleared the queue.
        let second = drain_web_replies();
        assert!(second.is_empty(), "drain should clear pending replies");
    }

    #[test]
    fn multiple_replies_different_sessions() {
        let _lock = setup();
        inject_web_reply("f3a2".to_string(), "reply for f3a2".to_string());
        inject_web_reply("b1c9".to_string(), "reply for b1c9".to_string());
        inject_web_reply("f3a2".to_string(), "second reply for f3a2".to_string());
        let replies = drain_web_replies();
        assert_eq!(replies.len(), 3);
        assert_eq!(replies[0].0, "f3a2");
        assert_eq!(replies[1].0, "b1c9");
        assert_eq!(replies[2].0, "f3a2");
    }
}
