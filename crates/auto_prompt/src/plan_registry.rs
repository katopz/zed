use std::collections::HashMap;
use std::sync::RwLock;

/// Default timeout after which a plan claim is considered stale and can be
/// reaped by another agent. Matches the auto_prompt chain timeout.
const DEFAULT_STALE_TIMEOUT_SECS: u64 = 300;

/// Ownership record for a claimed plan. Internal representation — not serialized.
#[derive(Debug, Clone)]
pub struct PlanOwnership {
    /// Session ID of the thread that claimed this plan.
    pub session_id: String,
    /// When the plan was first claimed (seconds since a common epoch, via
    /// `time_monotonic_secs` helper).
    pub claimed_at_secs: f64,
    /// Last heartbeat (seconds since same epoch).
    pub last_heartbeat_secs: f64,
    /// Brief description of what the agent is working on.
    pub task_summary: String,
}

/// A claim entry returned for context / JSON serialization.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivePlanClaim {
    /// The plan file path (e.g., "/path/to/.plans/05_auth.md").
    pub plan_file: String,
    /// Session ID of the owning thread.
    pub session_id: String,
    /// Brief description of the work being done.
    pub task_summary: String,
    /// How many seconds ago the plan was claimed.
    pub claimed_ago_secs: u64,
}

// ---------------------------------------------------------------------------
// Global registry — `RwLock<Option<..>>` pattern matches CACHED_CONFIG above.
// ---------------------------------------------------------------------------

static REGISTRY: RwLock<Option<HashMap<String, PlanOwnership>>> = RwLock::new(None);

/// Helper to access (or lazily create) the inner HashMap.
fn with_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut HashMap<String, PlanOwnership>) -> R,
{
    let mut guard = REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    f(map)
}

/// Monotonic seconds used for cheap timestamp comparison without `Instant`
/// (which is not `Send` on some platforms and cannot be stored in a global
/// `RwLock` without extra gymnastics).
fn time_monotonic_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Try to claim a plan file for a session.
///
/// Returns `Ok(())` if the claim was successful (no existing claim, same
/// session re-claiming, or the existing claim is stale).
/// Returns `Err(reason)` if another **active** session already owns this plan.
pub fn try_claim(plan_file: &str, session_id: &str, task_summary: &str) -> Result<(), String> {
    let now = time_monotonic_secs();

    with_registry(|registry| {
        // Garbage-collect stale claims first.
        registry.retain(|_, ownership| {
            (now - ownership.last_heartbeat_secs) < DEFAULT_STALE_TIMEOUT_SECS as f64
        });

        match registry.get(plan_file) {
            Some(existing) if existing.session_id != session_id => {
                let elapsed_secs = (now - existing.claimed_at_secs) as u64;
                Err(format!(
                    "Plan already claimed by session {} ({}s ago): {}",
                    existing.session_id, elapsed_secs, existing.task_summary,
                ))
            }
            _ => {
                registry.insert(
                    plan_file.to_string(),
                    PlanOwnership {
                        session_id: session_id.to_string(),
                        claimed_at_secs: now,
                        last_heartbeat_secs: now,
                        task_summary: task_summary.to_string(),
                    },
                );
                Ok(())
            }
        }
    })
}

/// Release a plan claim for a specific session.
///
/// Only releases if the claiming session matches — prevents accidental release
/// by a different session.
pub fn release(plan_file: &str, session_id: &str) {
    with_registry(|registry| {
        if let Some(ownership) = registry.get(plan_file) {
            if ownership.session_id == session_id {
                registry.remove(plan_file);
                log::info!(
                    "[auto_prompt::plan_registry] Released plan {plan_file} for session {session_id}"
                );
            }
        }
    });
}

/// Release **all** claims held by a session.
///
/// Called when a session/thread is closed or the auto_prompt chain stops.
pub fn release_all_for_session(session_id: &str) {
    let released_files = with_registry(|registry| {
        let before = registry.len();
        let released_files: Vec<String> = registry
            .iter()
            .filter(|(_, ownership)| ownership.session_id == session_id)
            .map(|(plan_file, _)| plan_file.clone())
            .collect();
        registry.retain(|_, ownership| ownership.session_id != session_id);
        let released = before - registry.len();
        if released > 0 {
            log::info!(
                "[auto_prompt::plan_registry] Released {released} plan claim(s) for session {session_id}"
            );
        }
        released_files
    });
    // Broadcast after the registry lock is released; no-op without a broadcaster.
    for plan_file in released_files {
        let plan_name = plan_file.rsplit('/').next().unwrap_or(plan_file.as_str());
        crate::peer_states::broadcast_state(
            session_id,
            None,
            &format!("released: {plan_name}"),
            &plan_file,
        );
    }
}

/// Update the heartbeat for a plan claim.
///
/// Should be called on each auto_prompt iteration to keep the claim alive.
pub fn heartbeat(plan_file: &str, session_id: &str) {
    let now = time_monotonic_secs();
    with_registry(|registry| {
        if let Some(ownership) = registry.get_mut(plan_file) {
            if ownership.session_id == session_id {
                ownership.last_heartbeat_secs = now;
            }
        }
    });
}

/// Get all active (non-stale) claims.
pub fn active_claims() -> Vec<ActivePlanClaim> {
    let now = time_monotonic_secs();
    with_registry(|registry| {
        registry
            .iter()
            .filter(|(_, ownership)| {
                (now - ownership.last_heartbeat_secs) < DEFAULT_STALE_TIMEOUT_SECS as f64
            })
            .map(|(plan_file, ownership)| ActivePlanClaim {
                plan_file: plan_file.clone(),
                session_id: ownership.session_id.clone(),
                task_summary: ownership.task_summary.clone(),
                claimed_ago_secs: (now - ownership.claimed_at_secs) as u64,
            })
            .collect()
    })
}

/// Get active claims **excluding** those from the given session.
///
/// Used to show "what other agents are working on" to a specific session's
/// orchestration LLM.
pub fn active_claims_for_others(session_id: &str) -> Vec<ActivePlanClaim> {
    active_claims()
        .into_iter()
        .filter(|claim| claim.session_id != session_id)
        .collect()
}

/// Check if a plan file is currently claimed by another (non-stale) session.
pub fn is_claimed_by_other(plan_file: &str, session_id: &str) -> bool {
    let now = time_monotonic_secs();
    with_registry(|registry| {
        registry.get(plan_file).is_some_and(|ownership| {
            ownership.session_id != session_id
                && (now - ownership.last_heartbeat_secs) < DEFAULT_STALE_TIMEOUT_SECS as f64
        })
    })
}

/// Filter a list of plan file paths, removing those claimed by other sessions.
///
/// Plans claimed by the given session itself are **kept** (the session may
/// continue working on its own plans).
pub fn filter_unclaimed<'a>(plan_files: &'a [String], session_id: &str) -> Vec<&'a String> {
    let now = time_monotonic_secs();
    with_registry(|registry| {
        plan_files
            .iter()
            .filter(|path| match registry.get(*path) {
                None => true,
                Some(ownership) => {
                    if ownership.session_id == session_id {
                        true // own claim — keep
                    } else {
                        // other session's claim — only keep if stale
                        (now - ownership.last_heartbeat_secs) >= DEFAULT_STALE_TIMEOUT_SECS as f64
                    }
                }
            })
            .collect()
    })
}

/// Auto-detect and claim the first plan that matches a prompt's content.
///
/// Scans the prompt text for references to plan filenames and attempts to
/// claim the first unclaimed one for the given session. Returns the claimed
/// plan path, or `None` if nothing matched or everything was already claimed.
pub fn auto_claim_from_prompt(
    prompt: &str,
    plan_files: &[impl AsRef<str>],
    session_id: &str,
    task_summary: &str,
) -> Option<String> {
    let prompt_lower = prompt.to_lowercase();
    for plan_file in plan_files {
        let plan_file_ref = plan_file.as_ref();
        let filename = plan_file_ref.rsplit('/').next().unwrap_or(plan_file_ref);
        let filename_lower = filename.to_lowercase();
        let stem = filename_lower
            .strip_suffix(".md")
            .unwrap_or(&filename_lower);

        if prompt_lower.contains(&filename_lower) || prompt_lower.contains(stem) {
            if try_claim(plan_file_ref, session_id, task_summary).is_ok() {
                log::info!(
                    "[auto_prompt::plan_registry] Auto-claimed plan {plan_file_ref} for session {session_id}"
                );
                return Some(plan_file_ref.to_string());
            }
        }
    }
    None
}

/// Return a human-readable summary of active claims (for logging / debugging).
pub fn format_active_claims() -> String {
    let claims = active_claims();
    if claims.is_empty() {
        return "No active plan claims".to_string();
    }
    let mut lines = Vec::with_capacity(claims.len() + 1);
    lines.push(format!("{} active claim(s):", claims.len()));
    for claim in &claims {
        lines.push(format!(
            "  - {} → session {} ({}s ago): {}",
            claim.plan_file, claim.session_id, claim.claimed_ago_secs, claim.task_summary,
        ));
    }
    lines.join("\n")
}

/// Build a concise context string describing claims by **other** sessions.
///
/// Returns `None` when there are no competing claims. The output is suitable
/// for inclusion in an orchestration LLM prompt.
pub fn format_claims_for_context(session_id: &str) -> Option<String> {
    let claims = active_claims_for_others(session_id);
    if claims.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(claims.len());
    for claim in &claims {
        let filename = claim
            .plan_file
            .rsplit('/')
            .next()
            .unwrap_or(&claim.plan_file);
        lines.push(format!(
            "- `{filename}` — claimed by session {} ({}s ago): {}",
            &claim.session_id[..claim.session_id.len().min(8)],
            claim.claimed_ago_secs,
            claim.task_summary,
        ));
    }
    Some(format!(
        "The following plans are currently being worked on by other agents and should NOT be picked:\n{}\n\
         Only pick from plans NOT listed above.",
        lines.join("\n"),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire a global test lock and clear the registry.
    /// The returned guard must be bound (`let _lock = setup();`) so it is
    /// held for the duration of the test, serialising all plan_registry tests.
    /// Recovers from poisoned locks so a panic in one test doesn't cascade.
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        REGISTRY
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        guard
    }

    #[test]
    fn test_claim_and_release() {
        let _lock = setup();
        assert!(try_claim(".plans/01_test.md", "sess_a", "working on test").is_ok());
        assert!(
            try_claim(".plans/01_test.md", "sess_b", "also working").is_err(),
            "second session should be blocked"
        );
        release(".plans/01_test.md", "sess_a");
        assert!(
            try_claim(".plans/01_test.md", "sess_b", "now mine").is_ok(),
            "should succeed after release"
        );
    }

    #[test]
    fn test_double_claim_same_session_is_ok() {
        let _lock = setup();
        assert!(try_claim(".plans/01_test.md", "sess_a", "first").is_ok());
        assert!(
            try_claim(".plans/01_test.md", "sess_a", "re-claim").is_ok(),
            "same session re-claiming should succeed"
        );
    }

    #[test]
    fn test_release_all_for_session() {
        let _lock = setup();
        try_claim(".plans/01_a.md", "sess_a", "a").ok();
        try_claim(".plans/02_b.md", "sess_a", "b").ok();
        try_claim(".plans/03_c.md", "sess_b", "c").ok();

        release_all_for_session("sess_a");

        let claims = active_claims();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].session_id, "sess_b");
    }

    #[test]
    fn test_release_all_for_session_idempotent_and_others_kept() {
        let _lock = setup();
        try_claim(".plans/01_a.md", "sess_a", "a").ok();
        try_claim(".plans/02_b.md", "sess_a", "b").ok();
        try_claim(".plans/03_c.md", "sess_b", "c").ok();

        release_all_for_session("sess_a");
        // Second call with no remaining claims must not panic and must not
        // touch other sessions' claims.
        release_all_for_session("sess_a");

        let claims = active_claims();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].session_id, "sess_b");
        assert_eq!(claims[0].plan_file, ".plans/03_c.md");
    }

    #[test]
    fn test_is_claimed_by_other() {
        let _lock = setup();
        try_claim(".plans/01_test.md", "sess_a", "working").ok();

        assert!(
            is_claimed_by_other(".plans/01_test.md", "sess_b"),
            "should be claimed by other"
        );
        assert!(
            !is_claimed_by_other(".plans/01_test.md", "sess_a"),
            "should not be claimed by other for same session"
        );
        assert!(
            !is_claimed_by_other(".plans/02_other.md", "sess_b"),
            "unclaimed plan should return false"
        );
    }

    #[test]
    fn test_filter_unclaimed() {
        let _lock = setup();
        try_claim(".plans/01_a.md", "sess_a", "a").ok();
        try_claim(".plans/02_b.md", "sess_b", "b").ok();

        let files = vec![
            ".plans/01_a.md".to_string(),
            ".plans/02_b.md".to_string(),
            ".plans/03_c.md".to_string(),
        ];

        // sess_a should see its own claim (01_a) + unclaimed (03_c), but NOT 02_b
        let filtered = filter_unclaimed(&files, "sess_a");
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|f| *f == ".plans/01_a.md"));
        assert!(filtered.iter().any(|f| *f == ".plans/03_c.md"));
    }

    #[test]
    fn test_auto_claim_from_prompt() {
        let _lock = setup();
        let plans = [".plans/05_auth_flow.md", ".plans/06_cache.md"];

        let claimed = auto_claim_from_prompt(
            "Continue with plan 05_auth_flow",
            &plans,
            "sess_a",
            "auth work",
        );

        assert_eq!(
            claimed,
            Some(".plans/05_auth_flow.md".to_string()),
            "should auto-claim the referenced plan"
        );
        assert!(
            is_claimed_by_other(".plans/05_auth_flow.md", "sess_b"),
            "claimed plan should be visible to other sessions"
        );
    }

    #[test]
    fn test_heartbeat_keeps_claim_alive() {
        let _lock = setup();
        try_claim(".plans/01_test.md", "sess_a", "working").ok();

        heartbeat(".plans/01_test.md", "sess_a");

        let claims = active_claims();
        assert_eq!(claims.len(), 1, "heartbeat should keep claim alive");
    }

    #[test]
    fn test_active_claims_excludes_session() {
        let _lock = setup();
        try_claim(".plans/01_a.md", "sess_a", "a").ok();
        try_claim(".plans/02_b.md", "sess_b", "b").ok();

        let claims = active_claims_for_others("sess_a");
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].session_id, "sess_b");
    }

    #[test]
    fn test_format_claims_for_context_none() {
        let _lock = setup();
        assert!(
            format_claims_for_context("sess_a").is_none(),
            "no claims → None"
        );
    }

    #[test]
    fn test_format_claims_for_context_some() {
        let _lock = setup();
        try_claim(".plans/01_a.md", "sess_b", "working on auth").ok();
        let ctx = format_claims_for_context("sess_a").unwrap();
        assert!(
            ctx.contains("01_a.md"),
            "context should mention the claimed plan file"
        );
        assert!(
            ctx.contains("should NOT be picked"),
            "context should include the instruction"
        );
    }
}
