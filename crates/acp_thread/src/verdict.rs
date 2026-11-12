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

use crate::{AcpThread, AcpThreadEvent, SubmissionResponse};
use agent_client_protocol::schema::v2 as acp;
use futures::StreamExt as _;
use gpui::{App, AsyncApp, Entity, Task, TaskExt as _};
use project::Project;
use util::path_list::PathList;

/// How long a verdict session stays active after its last tool call. Every
/// tool call refreshes this, so it only bounds the "negotiation abandoned
/// mid-flight" case.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

/// Upper bound for one external reviewer turn on a warm session (proposal 001
/// phase 6). Mirrors the hidden orchestrator's budget: generous for a single
/// judgment turn, tight enough that a runaway reviewer can't wedge the
/// worker's tool call.
pub const REVIEWER_TURN_TIMEOUT: Duration = Duration::from_secs(180);

/// Upper bound for the first turn of a freshly spawned external reviewer
/// session. That turn additionally pays the underlying CLI's one-time cold
/// start (process boot, MCP server init, auth) before the judgment turn even
/// begins, which observably blows past [`REVIEWER_TURN_TIMEOUT`] on Claude
/// Code: the first verdict timed out and only the manual retry against the
/// now-warm session succeeded. Follow-up turns reuse the warm budget.
pub const REVIEWER_COLD_START_TURN_TIMEOUT: Duration = Duration::from_secs(600);

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

/// Reviewer threads orphaned by TTL expiry, awaiting an `App` handle to be
/// closed (`.issues/016` part 2). `prune_expired` has no `cx`, so it defers
/// the close here; the panel's 10s drain loop pumps the list via
/// [`drain_pending_closes`]. Holding the `Entity` keeps the `AcpThread` alive
/// until its session can be closed.
fn pending_closes() -> &'static Mutex<Vec<Entity<AcpThread>>> {
    static PENDING: Mutex<Vec<Entity<AcpThread>>> = Mutex::new(Vec::new());
    &PENDING
}

fn lock_registry() -> std::sync::MutexGuard<'static, Option<HashMap<String, VerdictSession>>> {
    registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_pending_closes() -> std::sync::MutexGuard<'static, Vec<Entity<AcpThread>>> {
    pending_closes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn prune_expired(registry: &mut HashMap<String, VerdictSession>, now: Instant) {
    // Lock order: registry → pending. The only other pending-lock site
    // (`drain_pending_closes`) never takes the registry lock, so no cycle.
    let mut pending = lock_pending_closes();
    registry.retain(|_, entry| {
        if now.duration_since(entry.last_activity) < SESSION_TTL {
            true
        } else {
            if let Some(thread) = entry.reviewer_thread.take() {
                pending.push(thread);
            }
            false
        }
    });
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

/// Registers (or refreshes) a verdict negotiation session and returns the
/// 1-based round number for this tool call. Idempotent per call site: the
/// tool calls this once per `request_verdict` invocation, so the return value
/// is the round the negotiation has reached.
pub fn register(session_id: &acp::SessionId) -> usize {
    register_with_offset(session_id, 0)
}

/// Like [`register`], but seeds a brand-new session with rounds already used
/// by the negotiation it replaces (route failover, issue 037) so the round
/// budget carries over. An already-registered session ignores the seed.
pub fn register_with_offset(session_id: &acp::SessionId, used_rounds: usize) -> usize {
    let now = Instant::now();
    let mut guard = lock_registry();
    let registry = guard.get_or_insert_with(HashMap::new);
    prune_expired(registry, now);
    let entry = registry
        .entry(session_id.to_string())
        .and_modify(|entry| entry.last_activity = now)
        .or_insert_with(|| VerdictSession {
            last_activity: now,
            rounds: used_rounds,
            reviewer_thread: None,
        });
    entry.rounds += 1;
    entry.last_activity = now;
    entry.rounds
}

/// Classification of a reviewer-lane failure (issue 037): the reliability
/// gate counts rate-limits separately from honest aborts, and the lane
/// reacts differently — fail over or re-park on rate limits, one bounded
/// retry on transient blips, nothing on the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewerFailure {
    /// The backend is rate-limited. `reset_hint` carries the backend's
    /// verbatim reset clause (e.g. "resets 1:20am (Asia/Bangkok)") for the
    /// re-park message; wall-clock hints are deliberately not parsed into
    /// durations — a multi-hour window is a failover/re-park decision, never
    /// a sleep inside a tool call.
    RateLimit { reset_hint: Option<String> },
    /// Timeout / connection-lost / overloaded class: worth one bounded
    /// retry. `retry_after` carries a parseable "try again in Ns/Nm" hint.
    Transient { retry_after: Option<Duration> },
    /// Empty replies, protocol errors — neither retryable nor
    /// failover-worthy.
    Other,
}

/// Upper bound for a retry delay derived from a backend hint. Matches the
/// fork's `MAX_RETRY_AFTER` posture: long provider windows are a failover or
/// re-park decision, not a silent sleep.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(2 * 60);

/// Backoff for transient reviewer failures with no parseable hint.
const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Longest reset clause copied into a re-park message, in chars.
const MAX_RESET_HINT_CHARS: usize = 80;

/// ASCII-case-insensitive substring search. Byte-index based, but a match on
/// ASCII bytes can only start at a char boundary (continuation bytes are
/// ≥ 0x80), so the returned index is always a safe slice point.
fn find_ascii_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Classifies a reviewer failure from the turn's error text plus the
/// reviewer thread's recorded API error (`AcpThread::last_api_error`), which
/// carries the real backend message when a turn dies on one (e.g. Claude
/// Code's `… {"errorKind": "rate_limit"}`). Rate-limit detection wins over
/// transient detection: a message mentioning both is a rate limit.
pub fn classify_reviewer_failure(turn_error: &str, api_error: Option<&str>) -> ReviewerFailure {
    let combined = match api_error {
        Some(api_error) => format!("{api_error}\n{turn_error}"),
        None => turn_error.to_string(),
    };
    let lower = combined.to_lowercase();
    if lower.contains("rate_limit") || lower.contains("rate limit") {
        return ReviewerFailure::RateLimit {
            reset_hint: reset_hint(&combined),
        };
    }
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("connection lost")
        || lower.contains("released mid-turn")
        || lower.contains("overloaded")
    {
        return ReviewerFailure::Transient {
            retry_after: parse_try_after(&combined),
        };
    }
    ReviewerFailure::Other
}

/// The bounded retry delay for a transient failure: the backend's parsed
/// hint (already clamped to [`MAX_RETRY_DELAY`]), else a short default
/// backoff. Rate limits return `None` — their windows are hours long; the
/// lane must fail over or re-park instead of sleeping.
pub fn reviewer_retry_delay(failure: &ReviewerFailure) -> Option<Duration> {
    match failure {
        ReviewerFailure::Transient { retry_after } => {
            Some(retry_after.unwrap_or(DEFAULT_RETRY_DELAY))
        }
        _ => None,
    }
}

/// Extracts a bounded "resets …" clause for re-park messages, trimming the
/// raw JSON error object backends sometimes append. Char-boundary safe:
/// capped by chars, and the JSON cut lands on an ASCII match.
fn reset_hint(text: &str) -> Option<String> {
    let start = find_ascii_ignore_case(text, "resets")?;
    let line: String = text[start..]
        .chars()
        .take_while(|ch| *ch != '\n')
        .collect();
    let capped: String = line.chars().take(MAX_RESET_HINT_CHARS).collect();
    let end = find_ascii_ignore_case(&capped, "{").unwrap_or(capped.len());
    let hint = capped[..end].trim();
    let hint = hint.strip_suffix(':').unwrap_or(hint);
    if hint.is_empty() {
        None
    } else {
        Some(hint.to_string())
    }
}

/// Parses a "try again in Ns / retry after: Nm" style hint into a bounded
/// duration.
fn parse_try_after(text: &str) -> Option<Duration> {
    for needle in ["try again in", "retry after"] {
        let Some(start) = find_ascii_ignore_case(text, needle) else {
            continue;
        };
        let mut chars = text[start + needle.len()..]
            .chars()
            .skip_while(|ch| ch.is_whitespace() || *ch == ':')
            .peekable();
        let seconds: String = chars
            .by_ref()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        let Ok(seconds) = seconds.parse::<u64>() else { continue };
        let Some(unit) = chars.find(|ch| !ch.is_whitespace()) else {
            continue;
        };
        let delay = match unit.to_ascii_lowercase() {
            's' => Duration::from_secs(seconds),
            'm' => Duration::from_secs(seconds * 60),
            'h' => Duration::from_secs(seconds * 3600),
            _ => continue,
        };
        return Some(delay.min(MAX_RETRY_DELAY));
    }
    None
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
    // until app exit, but must not fail the round.
    connection
        .clone()
        .close_session(&session_id, cx)
        .detach_and_log_err(cx);
}

/// Closes reviewer sessions orphaned by TTL expiry (`.issues/016` part 2).
/// Takes every deferred thread off the pending-close list and closes its
/// session. Intended to be pumped from the agent panel's existing 10s
/// notification drain loop, so no new timer is needed.
pub fn drain_pending_closes(cx: &mut App) {
    let pending = {
        let mut guard = lock_pending_closes();
        std::mem::take(&mut *guard)
    };
    if pending.is_empty() {
        return;
    }
    log::debug!(
        "[verdict] closing {} expired reviewer session(s)",
        pending.len()
    );
    for thread in pending {
        close_thread_session(&thread, cx);
    }
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
    // A receipt-driven submission resolves once the agent *accepts* the
    // prompt, not when the turn ends, so the end of the turn is observed via
    // the thread's events. Subscribe before sending so a fast turn can't stop
    // before we start listening.
    let (turn_end_sender, mut turn_end_receiver) = futures::channel::mpsc::unbounded::<()>();
    let _subscription = cx.update(|cx| {
        cx.subscribe(thread, move |_, event: &AcpThreadEvent, _| match event {
            AcpThreadEvent::Stopped { .. }
            | AcpThreadEvent::Error
            | AcpThreadEvent::ConnectionLost => {
                if turn_end_sender.unbounded_send(()).is_err() {
                    log::debug!("verdict reviewer turn already resolved");
                }
            }
            _ => {}
        })
    });
    let submission = cx.update(|cx| thread.update(cx, |thread, cx| thread.send(blocks, cx)));
    let turn_future = async move {
        match submission.await? {
            Some(SubmissionResponse::Accepted(_)) => {
                if turn_end_receiver.next().await.is_none() {
                    anyhow::bail!("verdict reviewer thread was released mid-turn");
                }
            }
            Some(SubmissionResponse::LegacyCompleted(_)) | None => {}
        }
        anyhow::Ok(())
    };
    let timeout_future = cx.background_executor().timer(timeout);
    let turn_future = std::pin::pin!(turn_future);
    let timeout_future = std::pin::pin!(timeout_future);

    match futures::future::select(turn_future, timeout_future).await {
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
    use crate::AgentConnection;
    use action_log::ActionLog;
    use gpui::AppContext as _;
    use project::{AgentId, FakeFs};
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use util::path;

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
    fn classify_flags_rate_limit_and_keeps_the_reset_hint() {
        let claude_code = "Internal error: You've hit your session limit · resets 1:20am (Asia/Bangkok): {\"errorKind\": \"rate_limit\"}";
        match classify_reviewer_failure("verdict reviewer returned an empty reply", Some(claude_code)) {
            ReviewerFailure::RateLimit { reset_hint } => {
                let hint = reset_hint.expect("reset hint extracted");
                assert!(hint.starts_with("resets 1:20am"), "hint: {hint}");
                assert!(!hint.contains("errorKind"), "JSON tail trimmed: {hint}");
                assert!(!hint.ends_with(':'), "trailing colon trimmed: {hint}");
            }
            other => panic!("expected rate limit, got {other:?}"),
        }

        // Also detected from the turn error alone (native lane's stringified 429s).
        assert!(matches!(
            classify_reviewer_failure("HTTP 429: rate limit exceeded", None),
            ReviewerFailure::RateLimit { reset_hint: None }
        ));

        // Unknown failures stay Other — no retry, no failover.
        assert_eq!(
            classify_reviewer_failure("connection refused", None),
            ReviewerFailure::Other
        );
    }

    #[test]
    fn classify_prefers_rate_limit_over_transient_keywords() {
        assert!(matches!(
            classify_reviewer_failure(
                "rate limit exceeded. please try again in 3 seconds",
                None
            ),
            ReviewerFailure::RateLimit { .. }
        ));
    }

    #[test]
    fn classify_flags_transient_and_parses_retry_hints() {
        assert_eq!(
            classify_reviewer_failure("verdict reviewer timed out after 180s", None),
            ReviewerFailure::Transient { retry_after: None }
        );
        assert_eq!(
            classify_reviewer_failure(
                "The server is overloaded. Please try again in 21 seconds",
                None
            ),
            ReviewerFailure::Transient {
                retry_after: Some(Duration::from_secs(21))
            }
        );
        // Minute hints are honored but clamped to MAX_RETRY_DELAY.
        assert_eq!(
            classify_reviewer_failure("overloaded — retry after: 90 minutes", None),
            ReviewerFailure::Transient {
                retry_after: Some(MAX_RETRY_DELAY)
            }
        );
        // A matched needle with no parseable hint falls through cleanly.
        assert_eq!(
            classify_reviewer_failure("timed out, retry after the window resets", None),
            ReviewerFailure::Transient { retry_after: None }
        );
    }

    #[test]
    fn retry_delay_borrows_the_hint_but_never_sleeps_on_rate_limits() {
        assert_eq!(
            reviewer_retry_delay(&ReviewerFailure::Transient { retry_after: None }),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            reviewer_retry_delay(&ReviewerFailure::RateLimit { reset_hint: None }),
            None
        );
        assert_eq!(reviewer_retry_delay(&ReviewerFailure::Other), None);
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
    fn register_with_offset_carries_the_budget_over_on_failover() {
        // The replaced negotiation had used 2 rounds (issue 037 failover).
        let session = session_id("verdict-test-failover");
        assert_eq!(register_with_offset(&session, 2), 3);
        assert_eq!(rounds(&session), Some(3));
        // An already-registered session ignores the seed.
        assert_eq!(register_with_offset(&session, 0), 4);
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

    /// Minimal connection that counts `close_session` calls, so the
    /// pending-close drain can be asserted end-to-end.
    #[derive(Default)]
    struct CloseCountingConnection {
        close_count: Arc<AtomicUsize>,
    }

    impl CloseCountingConnection {
        fn close_count(&self) -> usize {
            self.close_count.load(Ordering::SeqCst)
        }
    }

    impl crate::AgentConnection for CloseCountingConnection {
        fn agent_id(&self) -> AgentId {
            AgentId::new("verdict-close-test")
        }

        fn telemetry_id(&self) -> gpui::SharedString {
            "verdict-close-test".into()
        }

        fn auth_methods(&self) -> &[acp::AuthMethod] {
            &[]
        }

        fn new_session(
            self: Rc<Self>,
            project: Entity<Project>,
            work_dirs: PathList,
            cx: &mut App,
        ) -> Task<anyhow::Result<Entity<AcpThread>>> {
            let session_id = acp::SessionId::new(uuid::Uuid::now_v7().to_string());
            let action_log = cx.new(|_| ActionLog::new(project.clone()));
            let thread = cx.new(|cx| {
                AcpThread::new(
                    None,
                    None,
                    Some(work_dirs),
                    self.clone(),
                    project,
                    action_log,
                    session_id,
                    watch::Receiver::constant(acp::PromptCapabilities::new()),
                    cx,
                )
            });
            Task::ready(Ok(thread))
        }

        fn supports_close_session(&self) -> bool {
            true
        }

        fn close_session(
            self: Rc<Self>,
            _session_id: &acp::SessionId,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            self.close_count.fetch_add(1, Ordering::SeqCst);
            Task::ready(Ok(()))
        }

        fn authenticate(
            &self,
            _method: acp::AuthMethodId,
            _cx: &mut App,
        ) -> Task<anyhow::Result<()>> {
            Task::ready(Err(anyhow::anyhow!("not implemented in tests")))
        }

        fn prompt(
            &self,
            _params: acp::PromptRequest,
            _cx: &mut App,
        ) -> Task<anyhow::Result<agent_client_protocol::schema::v1::PromptResponse>> {
            Task::ready(Err(anyhow::anyhow!("not implemented in tests")))
        }

        fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) -> anyhow::Result<()> {
            Ok(())
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
            self
        }
    }

    fn init_verdict_test(cx: &mut gpui::TestAppContext) {
        env_logger::try_init().ok();
        cx.update(|cx| {
            let mut settings_store = settings::SettingsStore::test(cx);
            settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(settings_store);
        });
    }

    #[gpui::test]
    async fn expired_reviewer_sessions_defer_close_and_drain_closes_them(
        cx: &mut gpui::TestAppContext,
    ) {
        init_verdict_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let connection = Rc::new(CloseCountingConnection::default());
        let thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project,
                    PathList::new(&[Path::new(path!("/verdict-test"))]),
                    cx,
                )
            })
            .await
            .unwrap();

        // A reviewer session whose parent abandoned mid-negotiation: the entry
        // ages past the TTL with the reviewer thread still attached.
        let session = session_id("verdict-test-drain");
        {
            let mut guard = lock_registry();
            let registry = guard.get_or_insert_with(HashMap::new);
            registry.insert(
                session.to_string(),
                VerdictSession {
                    last_activity: Instant::now() - SESSION_TTL - Duration::from_secs(1),
                    rounds: 1,
                    reviewer_thread: Some(thread),
                },
            );
        }

        // Pruning drops the expired entry and defers its reviewer thread for a
        // later close instead of dropping the handle silently.
        assert!(!is_active(&session));
        assert_eq!(lock_pending_closes().len(), 1);

        // Draining closes the session exactly once and empties the list.
        cx.update(drain_pending_closes);
        cx.run_until_parked();
        assert_eq!(connection.close_count(), 1);
        assert!(lock_pending_closes().is_empty());
    }
}
