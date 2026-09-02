//! Verdict ping-pong session registry (`.proposals/001_claude_sub_agent_verdict.md`).
//!
//! A `request_verdict` negotiation runs across multiple stopped turns of a
//! dedicated reviewer subagent thread. auto_prompt must treat those inter-round
//! stops as normal — never auto-continue the verdict thread mid-negotiation —
//! so the tool registers the session here and every auto_prompt decision path
//! checks [`is_active`] before acting. Entries expire after [`SESSION_TTL`] so
//! a crashed or forgotten negotiation can never suppress auto_prompt forever.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1 as acp;

/// How long a verdict session stays active after its last tool call. Every
/// tool call refreshes this, so it only bounds the "negotiation abandoned
/// mid-flight" case.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);

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
}

fn registry() -> &'static Mutex<Option<HashMap<String, VerdictSession>>> {
    static REGISTRY: Mutex<Option<HashMap<String, VerdictSession>>> = Mutex::new(None);
    &REGISTRY
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
        });
    entry.rounds += 1;
    entry.last_activity = now;
    entry.rounds
}

/// Marks the negotiation finished — auto_prompt resumes normal behavior for
/// the session immediately.
pub fn complete(session_id: &acp::SessionId) {
    let mut guard = lock_registry();
    if let Some(registry) = guard.as_mut() {
        registry.remove(&session_id.to_string());
    }
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
}
