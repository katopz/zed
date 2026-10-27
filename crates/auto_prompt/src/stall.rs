//! Per-session no-progress detection for same-thread summary continuations.
//!
//! A thread whose remaining work waits on something only the user can change
//! (access, a rebuild, data that accrues from real use) produces the same
//! next-steps section summary after summary. Re-dispatching it spends a turn
//! to learn nothing new. The terminal-summary check catches this only when the
//! wording carries a deferral marker; this catches it from the repetition
//! itself, whatever the wording.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::Mutex;

/// Consecutive repeats of the same next steps before the chain stops. One
/// repeat is a normal re-check round; a second means nothing is moving.
pub const MAX_UNCHANGED_REPEATS: u32 = 2;

struct StallState {
    steps_fingerprint: u64,
    repeats: u32,
}

/// Per session, so one thread's progress never resets another's streak.
static STALLS: Mutex<Option<HashMap<String, StallState>>> = Mutex::new(None);

fn with_stalls<R>(f: impl FnOnce(&mut HashMap<String, StallState>) -> R) -> R {
    let mut guard = STALLS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Case, punctuation and whitespace are ignored so cosmetic rewording of the
/// same items still counts as unchanged.
fn fingerprint(steps: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    for word in steps
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        word.to_lowercase().hash(&mut hasher);
    }
    hasher.finish()
}

/// Records a summary's next steps for the session and returns how many
/// summaries in a row before this one carried the same steps.
pub fn record_summary_steps(session_id: &str, steps: &str) -> u32 {
    let steps_fingerprint = fingerprint(steps);
    with_stalls(|stalls| {
        let state = stalls.entry(session_id.to_string()).or_insert(StallState {
            steps_fingerprint,
            repeats: 0,
        });
        if state.steps_fingerprint == steps_fingerprint {
            state.repeats += 1;
        } else {
            *state = StallState {
                steps_fingerprint,
                repeats: 1,
            };
        }
        state.repeats - 1
    })
}

pub fn clear_session(session_id: &str) {
    with_stalls(|stalls| {
        stalls.remove(session_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_consecutive_unchanged_steps_per_session() {
        let session = "stall-test-consecutive";
        let other = "stall-test-other";
        clear_session(session);
        clear_session(other);
        let steps = "- Plan 037 phase 3: needs SSH.\n- Issue 016: 9 more chains.";
        assert_eq!(record_summary_steps(session, steps), 0);
        assert_eq!(
            record_summary_steps(
                session,
                "- plan 037 phase 3 needs ssh\n- issue 016, 9 more chains"
            ),
            1
        );
        // Another session's summaries never touch this streak.
        assert_eq!(record_summary_steps(other, "- Something else entirely."), 0);
        assert_eq!(record_summary_steps(session, steps), 2);
        // Progress (changed steps) restarts the count.
        assert_eq!(
            record_summary_steps(session, "- Issue 016: 8 more chains."),
            0
        );
        assert_eq!(
            record_summary_steps(session, "- Issue 016: 8 more chains."),
            1
        );
        clear_session(session);
        assert_eq!(record_summary_steps(session, steps), 0);
        clear_session(session);
        clear_session(other);
    }
}
