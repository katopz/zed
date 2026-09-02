//! Verdict ping-pong session registry (`.proposals/001_claude_sub_agent_verdict.md`).
//!
//! A `request_verdict` negotiation runs across multiple stopped turns of a
//! dedicated reviewer subagent thread. auto_prompt must treat those inter-round
//! stops as normal — never auto-continue the verdict thread mid-negotiation —
//! so the tool registers the session here and every auto_prompt decision path
//! checks [`is_active`] before acting. Entries expire after [`SESSION_TTL`] so
//! a crashed or forgotten negotiation can never suppress auto_prompt forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::AcpThread;
use agent_client_protocol::schema::v1 as acp;
use gpui::{App, AsyncApp, Entity, Task, TaskExt as _};
use project::Project;
use util::path_list::PathList;

/// How long a verdict session stays active after its last tool call. Every
/// tool call refreshes this, so it only bounds the "negotiation abandoned
/// mid-flight" case.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Upper bound for one external reviewer turn (proposal 001 phase 6). Mirrors
/// the hidden orchestrator's budget: generous for Claude Code session startup
/// plus a single judgment turn, tight enough that a runaway reviewer can't
/// wedge the worker's tool call.
pub const REVIEWER_TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// A pluggable backend that can spawn external reviewer sessions for verdict
/// ping-pong negotiations (proposal 001 phase 6). Implemented by `agent_ui`,
/// which owns the live external-agent connections, and registered via
/// [`set_reviewer`] — the same break-the-circle pattern as
/// `auto_prompt::peer_states::register_broadcaster`.
///
/// The trait object must be `Send + Sync` for the global slot; implementations
/// hold `WeakEntity` handles (which are `Send + Sync`) rather than the
/// `Rc<dyn AgentConnection>` they resolve internally per call.
pub trait VerdictReviewer: Send + Sync {
    /// Backend label for logs and tool output (e.g. `"claude_code"`).
    fn label(&self) -> &'static str;

    /// Spawns a NEW reviewer session on the external agent connection.
    /// Visibility matches the hidden orchestrator: the session is never
    /// registered in any panel list, so it's invisible to the user.
    fn spawn_session(
        &self,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<AcpThread>>>;
}

/// The two outcomes a reviewer verdict can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    Agree,
    Revise,
}

impl VerdictKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictKind::Agree => "AGREE",
            VerdictKind::Revise => "REVISE",
        }
    }
}

const VERDICT_PREFIX: &str = "#verdict:";

/// Parses a reviewer reply that must start with `#Verdict: AGREE` or
/// `#Verdict: REVISE` (case-insensitive, leading whitespace tolerated).
/// Returns `None` when the message is not a verdict at all — callers should
/// treat that as `Revise` with a malformed-reviewer reason.
pub fn parse_verdict(message: &str) -> Option<VerdictKind> {
    let rest = message.trim_start().to_lowercase();
    let rest = rest.strip_prefix(VERDICT_PREFIX)?.trim_start();
    if rest.starts_with(VerdictKind::Agree.as_str().to_lowercase().as_str()) {
        Some(VerdictKind::Agree)
    } else if rest.starts_with(VerdictKind::Revise.as_str().to_lowercase().as_str()) {
        Some(VerdictKind::Revise)
    } else {
        None
    }
}

struct VerdictSession {
    last_activity: Instant,
    rounds: usize,
    /// Live reviewer thread for external (ACP) reviewers. `None` for native
    /// subagent reviewers, whose handles live in the agent crate.
    reviewer_thread: Option<Entity<AcpThread>>,
}

fn registry() -> &'static Mutex<Option<HashMap<String, VerdictSession>>> {
    static REGISTRY: Mutex<Option<HashMap<String, VerdictSession>>> = Mutex::new(None);
    &REGISTRY
}

fn reviewer_slot() -> &'static Mutex<Option<Arc<dyn VerdictReviewer>>> {
    static REVIEWER: Mutex<Option<Arc<dyn VerdictReviewer>>> = Mutex::new(None);
    &REVIEWER
}

/// Registers the external reviewer backend. Pass `None` to clear (e.g. when
/// the owning panel is dropped).
pub fn set_reviewer(reviewer: Option<Arc<dyn VerdictReviewer>>) {
    let mut slot = reviewer_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = reviewer;
}

/// The registered external reviewer backend, if any.
pub fn reviewer() -> Option<Arc<dyn VerdictReviewer>> {
    reviewer_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn lock_registry() -> std::sync::MutexGuard<'static, Option<HashMap<String, VerdictSession>>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn prune_expired(registry: &mut HashMap<String, VerdictSession>, now: Instant) {
    registry.retain(|_, entry| now.duration_since(entry.last_activity) < SESSION_TTL);
}

/// Registers (or refreshes) a verdict negotiation session and returns the
/// 1-based round number for this tool call. Idempotent per call site: the
/// tool calls this once per `request_verdict` invocation, so the return value
/// is the round the negotiation has reached.
pub fn register(session_id: &acp::SessionId) -> usize {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    let entry = registry
        .entry(session_id.to_string())
        .and_modify(|entry| entry.last_activity = now)
        .or_insert_with(|| VerdictSession {
            last_activity: now,
            rounds: 0,
            reviewer_thread: None,
        });
    entry.rounds += 1;
    entry.last_activity = now;
    entry.rounds
}

/// Like [`register`], but also stores the external reviewer thread so
/// follow-up rounds resume the SAME session and [`close_reviewer_session`]
/// can free the underlying ACP process.
pub fn register_reviewer_session(session_id: &acp::SessionId, thread: Entity<AcpThread>) -> usize {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    let entry = registry
        .entry(session_id.to_string())
        .and_modify(|entry| {
            entry.last_activity = now;
            entry.reviewer_thread = Some(thread.clone());
        })
        .or_insert_with(|| VerdictSession {
            last_activity: now,
            rounds: 0,
            reviewer_thread: Some(thread.clone()),
        });
    entry.rounds += 1;
    entry.last_activity = now;
    entry.rounds
}

/// Marks the negotiation finished — auto_prompt resumes normal behavior for
/// the session immediately. Registry-only: native reviewer handles need no
/// explicit teardown.
pub fn complete(session_id: &acp::SessionId) {
    remove_entry(session_id);
}

/// Ends the negotiation AND closes the external reviewer session (best-effort)
/// to free the underlying ACP process. Use on the `final_round` path and when
/// the negotiation hits its round budget.
pub fn complete_reviewer(session_id: &acp::SessionId, cx: &mut App) {
    let entry = remove_entry(session_id);
    if let Some(thread) = entry.and_then(|entry| entry.reviewer_thread) {
        close_thread_session(&thread, cx);
    }
}

fn remove_entry(session_id: &acp::SessionId) -> Option<VerdictSession> {
    let mut guard = lock_registry();
    guard
        .as_mut()
        .and_then(|registry| registry.remove(&session_id.to_string()))
}

fn close_thread_session(thread: &Entity<AcpThread>, cx: &mut App) {
    let connection = thread.read(cx).connection().clone();
    let session_id = thread.read(cx).session_id().clone();
    if !connection.supports_close_session() {
        return;
    }
    // Best-effort: a failed close leaves the session idling in the connection
    // until app exit (tracked in .issues/016), but must not fail the round.
    connection
        .clone()
        .close_session(&session_id, cx)
        .detach_and_log_err(cx);
}

/// The live external reviewer thread for a session, if it is still tracked.
pub fn reviewer_thread(session_id: &acp::SessionId) -> Option<Entity<AcpThread>> {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    registry
        .get(&session_id.to_string())
        .and_then(|entry| entry.reviewer_thread.clone())
}

/// Runs one reviewer turn: sends `message` to the session and resolves with
/// the reviewer's final assistant message, bounded by `timeout` so a runaway
/// reviewer can't wedge the worker's tool call. Mirrors the hidden
/// orchestrator's bounded-send pattern (`.plans/014`).
pub async fn reviewer_turn(
    thread: &Entity<AcpThread>,
    message: String,
    timeout: Duration,
    cx: &AsyncApp,
) -> anyhow::Result<String> {
    let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(message))];
    let send_future = cx.update(|cx| thread.update(cx, |thread, cx| thread.send(blocks, cx)));
    let timeout_future = cx.background_executor().timer(timeout);
    let send_future = std::pin::pin!(send_future);
    let timeout_future = std::pin::pin!(timeout_future);

    match futures::future::select(send_future, timeout_future).await {
        futures::future::Either::Left((result, _)) => {
            result?;
        }
        futures::future::Either::Right(_) => {
            // Cancel the runaway turn so we don't leak a running session.
            cx.update(|cx| thread.update(cx, |thread, cx| thread.cancel(cx).detach()));
            anyhow::bail!("verdict reviewer timed out after {}s", timeout.as_secs());
        }
    }

    let reply = cx
        .update(|cx| thread.read(cx).last_assistant_message_text(cx))
        .unwrap_or_default();
    if reply.trim().is_empty() {
        anyhow::bail!("verdict reviewer returned an empty reply");
    }
    Ok(reply)
}

/// Whether a verdict negotiation is active (registered and not TTL-expired)
/// for this session. Expired entries are pruned as a side effect.
pub fn is_active(session_id: &acp::SessionId) -> bool {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    registry.contains_key(&session_id.to_string())
}

/// The number of `request_verdict` rounds already used by this session, if it
/// is still tracked.
pub fn rounds(session_id: &acp::SessionId) -> Option<usize> {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    registry
        .get(&session_id.to_string())
        .map(|entry| entry.rounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id(id: &str) -> acp::SessionId {
        acp::SessionId::new(id)
    }

    #[test]
    fn parse_verdict_accepts_agree_and_revise() {
        assert_eq!(
            parse_verdict("#Verdict: AGREE — the summary is accurate"),
            Some(VerdictKind::Agree)
        );
        assert_eq!(
            parse_verdict("#Verdict: REVISE\n- claim 2 is unverified"),
            Some(VerdictKind::Revise)
        );
    }

    #[test]
    fn parse_verdict_is_case_and_whitespace_tolerant() {
        assert_eq!(parse_verdict("  #verdict:agree"), Some(VerdictKind::Agree));
        assert_eq!(
            parse_verdict("#VERDICT:    revise"),
            Some(VerdictKind::Revise)
        );
    }

    #[test]
    fn parse_verdict_rejects_non_verdict_and_unknown_kind() {
        assert_eq!(parse_verdict("## Summary\nAll done."), None);
        assert_eq!(parse_verdict("#Verdict: maybe"), None);
        assert_eq!(parse_verdict(""), None);
        assert_eq!(parse_verdict("the #verdict: appears late"), None);
    }

    #[test]
    fn register_counts_rounds_and_complete_resets() {
        let session = session_id("verdict-test-counting");
        assert_eq!(register(&session), 1);
        assert_eq!(register(&session), 2);
        assert_eq!(rounds(&session), Some(2));
        assert!(is_active(&session));
        complete(&session);
        assert!(!is_active(&session));
        assert_eq!(rounds(&session), None);
        // Re-registering after completion starts a fresh negotiation.
        assert_eq!(register(&session), 1);
        complete(&session);
    }

    #[test]
    fn expired_entries_are_pruned_and_stop_suppressing() {
        let session = session_id("verdict-test-ttl");
        {
            let mut guard = lock_registry();
            let registry = guard.get_or_insert_with(HashMap::new);
            registry.insert(
                session.to_string(),
                VerdictSession {
                    last_activity: Instant::now() - SESSION_TTL - Duration::from_secs(1),
                    rounds: 1,
                    reviewer_thread: None,
                },
            );
        }
        assert!(!is_active(&session));
        assert_eq!(rounds(&session), None);
        // The stale entry was pruned, not just reported inactive.
        let guard = lock_registry();
        assert!(
            guard
                .as_ref()
                .is_none_or(|registry| !registry.contains_key(&session.to_string()))
        );
    }

    #[test]
    fn verdict_kind_strings_round_trip_through_the_parser() {
        for kind in [VerdictKind::Agree, VerdictKind::Revise] {
            let message = format!("#Verdict: {} because reasons", kind.as_str());
            assert_eq!(parse_verdict(&message), Some(kind));
        }
    }

    struct TestReviewer;

    impl VerdictReviewer for TestReviewer {
        fn label(&self) -> &'static str {
            "test"
        }

        fn spawn_session(
            &self,
            _project: Entity<Project>,
            _work_dirs: PathList,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Entity<AcpThread>>> {
            Task::ready(Err(anyhow::anyhow!("not implemented in tests")))
        }
    }

    #[test]
    fn reviewer_provider_round_trips_and_clears() {
        assert!(reviewer().is_none());
        set_reviewer(Some(Arc::new(TestReviewer)));
        assert_eq!(
            reviewer().map(|r| r.label().to_string()),
            Some("test".into())
        );
        set_reviewer(None);
        assert!(reviewer().is_none());
    }

    #[test]
    fn register_reviewer_session_stores_thread_and_counts_rounds() {
        // App-less thread handle cannot be constructed here; the thread slot
        // stays None and registry bookkeeping still applies.
        let session = session_id("verdict-test-reviewer");
        assert_eq!(register(&session), 1);
        assert!(reviewer_thread(&session).is_none());
        assert_eq!(rounds(&session), Some(1));
        complete(&session);
        assert!(reviewer_thread(&session).is_none());
    }
}
