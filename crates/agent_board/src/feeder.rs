//! Bidirectional mirror between the local in-process `auto_prompt::plan_registry`
//! and the remote agent-board worker.
//!
//! - `post_local_claims`: reads local (non-remote) active claims from the
//!   registry and POSTs them as this device's status, so other devices see them.
//! - `inject_remote_claims`: reads the remote room snapshot, then re-claims each
//!   remote device's scopes into the local registry under a composite
//!   `remote:{device_id}:{session_id}` session id. The existing `auto_prompt`
//!   orchestration loop already calls `format_claims_for_context(session)`,
//!   which filters out the caller's own session and renders everyone else's
//!   claims — so remote claims flow into the LLM context with **zero** changes
//!   to `auto_prompt`'s core. Stale remote claims are reaped by plan_registry's
//!   own 300s heartbeat GC once we stop refreshing them.

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::client::BoardClient;
use crate::identity::DeviceIdentity;
use crate::mentions::{MentionRoute, ScanOutcome, scan_message_for_device};
use crate::types::{ActiveScope, PostStatusBody, RoomSnapshot, ScopeKind};

/// Prefix used for session ids that originate from a remote device, so we can
/// (a) avoid echoing them back to the worker and (b) distinguish them from
/// genuine local thread sessions in logs.
pub const REMOTE_SESSION_PREFIX: &str = "remote:";

/// Refresh round: pull the room, inject remote claims, then push our own status.
///
/// Returns the snapshot that was read (useful for the panel to render).
pub async fn sync_round(
    client: &Arc<BoardClient>,
    identity: &Arc<DeviceIdentity>,
    room: &str,
    project_path: &str,
    local_session_id: &str,
) -> Result<RoomSnapshot> {
    let snapshot = client
        .get_room(room)
        .await
        .context("fetching room snapshot")?;

    // 1. Inject remote claims into the local registry so auto_prompt's existing
    //    context builder picks them up. Each remote device's scopes are
    //    re-claimed under a composite session id.
    inject_remote_claims(&snapshot, local_session_id);

    // 2. Phase 2: store the latest agent states in auto_prompt's process-global
    //    peer-states store so the deciders can inject what peer agents are
    //    doing into their context. Translates the board wire type into
    //    auto_prompt's dependency-free PeerAgentState.
    let peer_states: Vec<auto_prompt::peer_states::PeerAgentState> = snapshot
        .states
        .iter()
        .map(|s| auto_prompt::peer_states::PeerAgentState {
            device_id: s.device_id.clone(),
            device_name: s.device_name.clone(),
            session_id: s.session_id.clone(),
            sub_agent_id: s.sub_agent_id.clone(),
            state_text: s.state_text.clone(),
        })
        .collect();
    auto_prompt::peer_states::set_peer_states(peer_states, identity.device_id());

    // 3. Phase 2 (Plan 015): drain web replies targeting this device. Each
    //    reply is queued in auto_prompt's peer-states store, keyed by the
    //    4-char session-id prefix. The agent_panel notification timer resolves
    //    the prefix to an active session and injects the text.
    let device_name = identity.device_name();
    for (prefix, text) in extract_replies_for_device(&snapshot, device_name) {
        auto_prompt::peer_states::inject_web_reply(prefix, text);
    }

    // 4. Push our own status (local claims only) so other devices see us.
    let body = build_local_status(identity, project_path);
    if let Err(error) = client.post_status(room, body).await {
        log::warn!("[agent_board] failed to post local status: {error:#}");
    }

    // 5. Cache the snapshot globally for the MCP tool (`GetAgentRoom`) and any
    //    other consumer that needs the full room state without a GPUI handle.
    crate::board_state::set_room_snapshot(snapshot.clone());

    Ok(snapshot)
}

/// Extract the `(target_session_prefix, text)` pairs from a room snapshot
/// that target the given device. Pure filter — no side effects — so it can be
/// unit-tested without touching the global `plan_registry` or `peer_states`
/// stores. The caller (`sync_round`) injects each pair into the peer-states
/// reply queue.
pub(crate) fn extract_replies_for_device(
    snapshot: &RoomSnapshot,
    device_name: &str,
) -> Vec<(String, String)> {
    snapshot
        .replies
        .iter()
        .filter(|reply| reply.target_device == device_name)
        .map(|reply| (reply.target_session_prefix.clone(), reply.text.clone()))
        .collect()
}

/// Result of a mention scan over a snapshot (Plan 024 P1): the routes that
/// target this device, plus the watermark the caller should persist so these
/// messages are never re-scanned.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MentionScan {
    pub routes: Vec<MentionRoute>,
    pub new_watermark_ts: i64,
}

/// Pure mention extraction (Plan 024 P1): every message newer than
/// `watermark_ts` whose first token is a routing mention `@device:prefix4`
/// targeting `device_name`. Self-mentions (`sender == device:prefix`) are
/// skipped. The caller applies the loop guard + injection.
pub(crate) fn extract_mentions_for_device(
    snapshot: &RoomSnapshot,
    device_name: &str,
    watermark_ts: i64,
) -> MentionScan {
    let mut scan = MentionScan {
        routes: Vec::new(),
        new_watermark_ts: watermark_ts,
    };
    for message in &snapshot.messages {
        scan.new_watermark_ts = scan.new_watermark_ts.max(message.ts);
        if message.ts <= watermark_ts {
            continue;
        }
        let (outcome, route) = scan_message_for_device(message, device_name);
        if let ScanOutcome::SelfMention = outcome {
            log::debug!(
                "[agent_board] dropping self-mention from {}: {}",
                message.sender,
                message.text
            );
        }
        if let Some(route) = route {
            scan.routes.push(route);
        }
    }
    scan
}

/// Re-claim every remote scope into the local `plan_registry`. Local (non-remote)
/// claims are left untouched. Stale remote claims are dropped by plan_registry's
/// heartbeat GC after `DEFAULT_STALE_TIMEOUT_SECS` (300s) once a device stops
/// posting — because we only re-heartbeat claims we saw in the latest snapshot.
fn inject_remote_claims(snapshot: &RoomSnapshot, _local_session_id: &str) {
    for status in &snapshot.statuses {
        if status.stale {
            continue;
        }
        for scope in &status.scopes {
            // Compose a stable, namespaced session id. plan_registry keys by
            // (plan_file, session_id); re-claiming the same key on each poll
            // acts as a heartbeat (it updates last_heartbeat_secs via try_claim's
            // overwrite path). See plan_registry::try_claim.
            let remote_session = format!(
                "{REMOTE_SESSION_PREFIX}{}:{}",
                status.device_id, scope.session_id
            );
            if let Some(plan_file) = &scope.plan_file {
                if let Err(reason) = auto_prompt::plan_registry::try_claim(
                    plan_file,
                    &remote_session,
                    &scope.task_summary,
                ) {
                    // A *local* session genuinely owns this plan — that's fine,
                    // we just skip injecting the remote claim. Only log at debug
                    // to avoid noise during normal multi-agent overlap.
                    log::debug!(
                        "[agent_board] did not mirror remote claim on {plan_file}: {reason}"
                    );
                }
            }
        }
    }
}

/// Build the status body from the local registry: all active claims that are
/// NOT remote-mirrored (i.e. genuine local thread sessions) become our scopes.
fn build_local_status(identity: &Arc<DeviceIdentity>, project_path: &str) -> PostStatusBody {
    let claims = auto_prompt::plan_registry::active_claims();
    let scopes: Vec<ActiveScope> = claims
        .into_iter()
        .filter(|claim| !claim.session_id.starts_with(REMOTE_SESSION_PREFIX))
        .map(|claim| {
            // We can't know the scope kind from the registry alone; default to
            // Plan. The plan file path itself disambiguates for display.
            let scope_kind = classify_scope(&claim.plan_file);
            ActiveScope {
                session_id: claim.session_id,
                plan_file: Some(claim.plan_file),
                task_summary: claim.task_summary,
                scope_kind,
            }
        })
        .collect();

    PostStatusBody {
        device_name: identity.device_name().to_string(),
        location_hash: identity.location_hash().to_string(),
        project_path: project_path.to_string(),
        scopes,
    }
}

/// Infer the scope kind from the file path's directory (`.plans` / `.issues` /
/// `.proposals`). Falls back to Plan.
fn classify_scope(plan_file: &str) -> ScopeKind {
    let normalized = plan_file.replace('\\', "/");
    if normalized.contains("/.issues/") || normalized.ends_with("/.issues") {
        ScopeKind::Issue
    } else if normalized.contains("/.proposals/") || normalized.ends_with("/.proposals") {
        ScopeKind::Proposal
    } else {
        ScopeKind::Plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_directory() {
        assert_eq!(
            classify_scope("/Volumes/SDXC1TB/git/zed/.plans/001_agent_board.md"),
            ScopeKind::Plan
        );
        assert_eq!(
            classify_scope("/Volumes/SDXC1TB/git/zed/.issues/002_thing.md"),
            ScopeKind::Issue
        );
        assert_eq!(
            classify_scope("/home/katopz/.proposals/003_x.md"),
            ScopeKind::Proposal
        );
        assert_eq!(classify_scope("random/path.md"), ScopeKind::Plan);
    }

    #[test]
    fn remote_prefix_is_namespaced() {
        assert!(REMOTE_SESSION_PREFIX.starts_with("remote:"));
    }

    fn make_reply(target_device: &str, prefix: &str, text: &str) -> crate::types::WebReply {
        use crate::types::WebReply;
        WebReply {
            v: 1,
            target_device: target_device.to_string(),
            target_session_prefix: prefix.to_string(),
            text: text.to_string(),
            author_login: "katopz".to_string(),
            ts: 1700000000000,
        }
    }

    fn snapshot_with_replies(replies: Vec<crate::types::WebReply>) -> RoomSnapshot {
        RoomSnapshot {
            v: 1,
            room: "test".to_string(),
            statuses: Vec::new(),
            messages: Vec::new(),
            states: Vec::new(),
            replies,
        }
    }

    #[test]
    fn extract_replies_for_matching_device() {
        let snapshot = snapshot_with_replies(vec![
            make_reply("m3", "f3a2", "stop and commit"),
            make_reply("SHIKUWA", "b1c9", "switch to develop"),
            make_reply("m3", "9d0e", "rebase first"),
        ]);
        let result = extract_replies_for_device(&snapshot, "m3");
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0],
            ("f3a2".to_string(), "stop and commit".to_string())
        );
        assert_eq!(result[1], ("9d0e".to_string(), "rebase first".to_string()));
    }

    #[test]
    fn extract_replies_skips_other_devices() {
        let snapshot = snapshot_with_replies(vec![
            make_reply("SHIKUWA", "b1c9", "switch to develop"),
            make_reply("SHIKUWA", "a2f1", "another"),
        ]);
        let result = extract_replies_for_device(&snapshot, "m3");
        assert!(
            result.is_empty(),
            "no replies should match a different device"
        );
    }

    #[test]
    fn extract_replies_empty_snapshot() {
        let snapshot = snapshot_with_replies(Vec::new());
        let result = extract_replies_for_device(&snapshot, "m3");
        assert!(result.is_empty());
    }

    // ── extract_mentions_for_device (Plan 024 P1) ──

    fn board_message(
        device_name: &str,
        sender: &str,
        text: &str,
        ts: i64,
    ) -> crate::types::BoardMessage {
        crate::types::BoardMessage {
            v: 1,
            device_id: String::new(),
            device_name: device_name.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            ts,
        }
    }

    fn snapshot_with_messages(messages: Vec<crate::types::BoardMessage>) -> RoomSnapshot {
        RoomSnapshot {
            v: 1,
            room: "test".to_string(),
            statuses: Vec::new(),
            messages,
            states: Vec::new(),
            replies: Vec::new(),
        }
    }

    #[test]
    fn mentions_route_own_device() {
        let snapshot = snapshot_with_messages(vec![
            board_message("phone", "web", "@m3:f3a2 stop and commit", 1000),
            board_message("SHIKUWA", "operator", "@SHIKUWA:b1c9 rebase", 2000),
            board_message("m3", "m3:aaaa", "@m3:9d0e mind rebasing?", 3000),
        ]);
        let scan = extract_mentions_for_device(&snapshot, "m3", 0);
        assert_eq!(scan.routes.len(), 2);
        assert_eq!(scan.routes[0].prefix, "f3a2");
        assert_eq!(scan.routes[0].text, "stop and commit");
        assert_eq!(scan.routes[0].sender_label, "web");
        assert_eq!(scan.routes[1].prefix, "9d0e");
        assert_eq!(scan.new_watermark_ts, 3000);
    }

    #[test]
    fn mentions_skip_other_devices_and_plain_text() {
        let snapshot = snapshot_with_messages(vec![
            board_message("phone", "web", "@SHIKUWA:b1c9 rebase", 1000),
            board_message("phone", "web", "just chatting, no mention", 2000),
        ]);
        let scan = extract_mentions_for_device(&snapshot, "m3", 0);
        assert!(scan.routes.is_empty());
        // Watermark still advances over non-mention messages.
        assert_eq!(scan.new_watermark_ts, 2000);
    }

    #[test]
    fn mentions_honor_watermark() {
        let snapshot = snapshot_with_messages(vec![
            board_message("phone", "web", "@m3:f3a2 old command", 1000),
            board_message("phone", "web", "@m3:f3a2 new command", 2000),
        ]);
        let scan = extract_mentions_for_device(&snapshot, "m3", 1000);
        assert_eq!(scan.routes.len(), 1);
        assert_eq!(scan.routes[0].text, "new command");
        assert_eq!(scan.new_watermark_ts, 2000);
    }

    #[test]
    fn mentions_drop_self_mention() {
        let snapshot = snapshot_with_messages(vec![board_message(
            "m3",
            "m3:f3a2",
            "@m3:f3a2 keep going",
            1000,
        )]);
        let scan = extract_mentions_for_device(&snapshot, "m3", 0);
        assert!(scan.routes.is_empty());
        assert_eq!(scan.new_watermark_ts, 1000);
    }

    #[test]
    fn mentions_at_all_is_deferred() {
        let snapshot = snapshot_with_messages(vec![board_message(
            "phone",
            "web",
            "@all stop everything",
            1000,
        )]);
        let scan = extract_mentions_for_device(&snapshot, "m3", 0);
        assert!(scan.routes.is_empty());
    }
}
