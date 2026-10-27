//! Per-session auto-prompt chain state: turn counts, the no-progress streak
//! the runaway cap reads, pre-stop verification attempts, orchestration-call
//! failures, and the repetition of a summary's next steps.
//!
//! Everything is keyed by session. These used to be process-wide atomics that
//! every thread incremented and any thread's stop reset, so with several agents
//! in one window the cap never fired and one thread's stop wiped another's
//! verification progress (`.issues/037`; `api_unreachable` fixed the same
//! defect for its retry streaks).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use acp_thread::AgentThreadEntry;
use agent_client_protocol::schema::v2 as acp;

/// Seconds without a turn after which a session's chain counts as a new one.
const CHAIN_TIMEOUT_SECS: u64 = 300;

/// Consecutive repeats of the same next steps before the chain stops. One
/// repeat is a normal re-check round; a second means nothing is moving.
pub const MAX_UNCHANGED_REPEATS: u32 = 2;

#[derive(Default)]
struct ChainState {
    turns: u32,
    idle_turns: u32,
    verification_attempts: u32,
    llm_failures: u32,
    last_turn_secs: u64,
    steps_fingerprint: Option<u64>,
    steps_repeats: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnCount {
    /// Turns in this chain, for logs and the orchestrator's context.
    pub turn: u32,
    /// Consecutive turns without progress; what the runaway cap compares.
    pub idle_turns: u32,
}

static CHAINS: Mutex<Option<HashMap<String, ChainState>>> = Mutex::new(None);

fn with_chain<R>(session_id: &str, f: impl FnOnce(&mut ChainState) -> R) -> R {
    let mut guard = CHAINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let chains = guard.get_or_insert_with(HashMap::new);
    f(chains.entry(session_id.to_string()).or_default())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Counts a finished turn for the session. A turn that made progress resets
/// the idle streak, so the cap never cuts off a chain that keeps landing work.
pub fn record_turn(session_id: &str, made_progress: bool) -> TurnCount {
    record_turn_at(session_id, made_progress, now_secs())
}

fn record_turn_at(session_id: &str, made_progress: bool, now: u64) -> TurnCount {
    with_chain(session_id, |chain| {
        if chain.last_turn_secs > 0 && now.saturating_sub(chain.last_turn_secs) > CHAIN_TIMEOUT_SECS
        {
            log::info!(
                "auto_prompt: chain timeout ({}s since the last turn), starting a new chain (session={session_id})",
                now.saturating_sub(chain.last_turn_secs)
            );
            *chain = ChainState::default();
        }
        chain.turns += 1;
        chain.idle_turns = if made_progress {
            0
        } else {
            chain.idle_turns + 1
        };
        chain.last_turn_secs = now;
        TurnCount {
            turn: chain.turns,
            idle_turns: chain.idle_turns,
        }
    })
}

/// The session's turn count without recording a new turn.
pub fn current_turn(session_id: &str) -> u32 {
    with_chain(session_id, |chain| chain.turns)
}

pub fn verification_attempts(session_id: &str) -> u32 {
    with_chain(session_id, |chain| chain.verification_attempts)
}

pub fn record_verification_attempt(session_id: &str) {
    with_chain(session_id, |chain| chain.verification_attempts += 1);
}

/// Counts a failed (or deferred) orchestration call and returns the session's
/// consecutive failures, which the caller compares against `max_llm_retries`.
pub fn record_llm_failure(session_id: &str) -> u32 {
    with_chain(session_id, |chain| {
        chain.llm_failures += 1;
        chain.llm_failures
    })
}

pub fn llm_failures(session_id: &str) -> u32 {
    with_chain(session_id, |chain| chain.llm_failures)
}

pub fn reset_llm_failures(session_id: &str) {
    with_chain(session_id, |chain| chain.llm_failures = 0);
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

/// Records a summary's next steps and returns how many summaries in a row
/// before this one carried the same steps. A thread waiting on something only
/// the user can change writes the same steps every round; the terminal check
/// catches that only when the wording carries a deferral marker, this catches
/// it from the repetition itself.
pub fn record_summary_steps(session_id: &str, steps: &str) -> u32 {
    let steps_fingerprint = fingerprint(steps);
    with_chain(session_id, |chain| {
        if chain.steps_fingerprint == Some(steps_fingerprint) {
            chain.steps_repeats += 1;
        } else {
            chain.steps_fingerprint = Some(steps_fingerprint);
            chain.steps_repeats = 0;
        }
        chain.steps_repeats
    })
}

pub fn clear_session(session_id: &str) {
    let mut guard = CHAINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(chains) = guard.as_mut() {
        chains.remove(session_id);
    }
}

/// Whether the latest turn (entries after the last user message) changed
/// anything: an edit, delete or move tool call, or a shell command that
/// commits or pushes. Reading, searching and re-checking are not progress.
pub fn turn_made_progress(entries: &[AgentThreadEntry]) -> bool {
    entries
        .iter()
        .rev()
        .take_while(|entry| !matches!(entry, AgentThreadEntry::UserMessage(_)))
        .any(|entry| match entry {
            AgentThreadEntry::ToolCall(tool) => {
                matches!(
                    tool.reported_kind,
                    Some(acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move)
                ) || tool
                    .raw_input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(|command| command.as_str())
                    .is_some_and(command_lands_work)
            }
            _ => false,
        })
}

fn command_lands_work(command: &str) -> bool {
    command.contains("git commit") || command.contains("git push")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_streak_counts_only_turns_without_progress_per_session() {
        let session = "chain-test-idle";
        let other = "chain-test-idle-other";
        clear_session(session);
        clear_session(other);
        let now = 1_000;
        assert_eq!(
            record_turn_at(session, false, now),
            TurnCount {
                turn: 1,
                idle_turns: 1
            }
        );
        assert_eq!(record_turn_at(session, false, now + 1).idle_turns, 2);
        // Another session's turns and stop never touch this one.
        record_turn_at(other, false, now + 1);
        clear_session(other);
        assert_eq!(
            record_turn_at(session, true, now + 2),
            TurnCount {
                turn: 3,
                idle_turns: 0
            }
        );
        assert_eq!(record_turn_at(session, false, now + 3).idle_turns, 1);
        assert_eq!(current_turn(session), 4);
        // A long gap starts a new chain.
        assert_eq!(
            record_turn_at(session, false, now + 3 + CHAIN_TIMEOUT_SECS + 1),
            TurnCount {
                turn: 1,
                idle_turns: 1
            }
        );
        clear_session(session);
    }

    #[test]
    fn verification_attempts_are_per_session() {
        let session = "chain-test-verification";
        let other = "chain-test-verification-other";
        clear_session(session);
        clear_session(other);
        record_verification_attempt(session);
        record_verification_attempt(session);
        record_verification_attempt(other);
        clear_session(other);
        assert_eq!(verification_attempts(session), 2);
        clear_session(session);
        assert_eq!(verification_attempts(session), 0);
        clear_session(session);
    }

    #[test]
    fn llm_failures_are_per_session() {
        let session = "chain-test-llm-failures";
        let other = "chain-test-llm-failures-other";
        clear_session(session);
        clear_session(other);
        assert_eq!(record_llm_failure(session), 1);
        // Another session succeeding never refills this session's budget.
        record_llm_failure(other);
        reset_llm_failures(other);
        assert_eq!(record_llm_failure(session), 2);
        reset_llm_failures(session);
        assert_eq!(record_llm_failure(session), 1);
        clear_session(session);
        clear_session(other);
    }

    #[test]
    fn counts_only_consecutive_unchanged_steps_per_session() {
        let session = "chain-test-steps";
        let other = "chain-test-steps-other";
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
        assert_eq!(record_summary_steps(other, "- Something else entirely."), 0);
        assert_eq!(record_summary_steps(session, steps), 2);
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

    #[test]
    fn landing_commands_count_as_progress() {
        assert!(command_lands_work("cargo fmt && git commit -qm 'fix: x'"));
        assert!(command_lands_work("git pull --rebase -q && git push -q"));
        assert!(!command_lands_work("git status --short && git log -1"));
        assert!(!command_lands_work("ssh host true"));
    }
}
