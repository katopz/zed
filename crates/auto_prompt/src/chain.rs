//! Per-session auto-prompt chain state: turn counts, the no-progress streak
//! the runaway cap reads, pre-stop verification attempts, orchestration-call
//! failures, and the repetition of a summary's next steps.
//!
//! Everything is keyed by session. These used to be process-wide atomics that
//! every thread incremented and any thread's stop reset, so with several agents
//! in one window the cap never fired and one thread's stop wiped another's
//! verification progress (`.issues/037`; `api_unreachable` fixed the same
//! defect for its retry streaks).

use std::collections::{HashMap, HashSet};
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

/// A chained hop that finished this fast is a systematic failure, not
/// progress: a real agentic turn takes minutes, so a near-instant exit
/// between auto-prompts is the worker crashing on its first turn, again
/// (issue 058, the `MIN_RUNTIME_FOR_RESUME` analog; complements the
/// unchanged-steps repeat cap, which it can outrun by re-wording).
pub const MIN_TURN_RUNTIME_SECS: u64 = 10;
/// Near-instant chained hops in a row before the chain halts.
pub const NEAR_INSTANT_LIMIT: u32 = 3;

/// How many distinct next-steps proposals the tried-steps ledger keeps.
/// Oldest entries drop off the front; the cap bounds the prompt block that
/// carries the ledger into every continuation.
pub const STEPS_LEDGER_CAP: usize = 8;

/// Excerpt length per ledger entry, in characters. Steps text routinely
/// carries CJK and em-dashes, so excerpts are never cut by byte index.
const LEDGER_EXCERPT_CHARS: usize = 120;

/// One distinct next-steps proposal this chain has already seen.
#[derive(Clone)]
struct LedgerEntry {
    fingerprint: u64,
    excerpt: String,
}

#[derive(Default)]
struct ChainState {
    turns: u32,
    idle_turns: u32,
    verification_attempts: u32,
    llm_failures: u32,
    last_turn_secs: u64,
    steps_fingerprint: Option<u64>,
    steps_repeats: u32,
    /// Rolling ledger of every distinct next-steps proposal this chain has
    /// seen, oldest first. `steps_fingerprint` is only the last slot: a
    /// third, differently-worded summary resets the repeat count and the
    /// chain re-runs earlier steps. The ledger rides every continuation
    /// prompt so the worker sees what was already proposed (openrsi
    /// harnessed-RSI lesson: the outer loop carries every lesson into the
    /// next round).
    steps_ledger: Vec<LedgerEntry>,
    /// Last (checked, actionable-unchecked) task counts across this
    /// session's claimed plan files. The round-over-round delta is the
    /// plan-level scoreboard: flipped checkboxes count as progress even
    /// when the summary's wording repeats.
    last_plan_progress: Option<(u32, u32)>,
    /// A valid `<stop>` already got its one housekeeping pass; the next
    /// `<stop>` in this session halts the chain outright.
    stop_housekeeping_sent: bool,
    /// Fingerprint of the last rejected keyword-stop section. The same
    /// section re-emitted after its corrective round honors the stop —
    /// one correction round is enough, and the runaway caps stay a
    /// backstop instead of the primary loop breaker.
    stop_violation_fingerprint: Option<u64>,
    /// Consecutive chained hops that finished under [`MIN_TURN_RUNTIME_SECS`]
    /// (issue 058's minimum-runtime guard). Any turn that takes real time
    /// resets the streak.
    /// Chain KPIs (issue 057): when this chain started, when its claimed
    /// plans first showed progress, and how many orchestration rounds it
    /// took — the "time-to-first-verified-gain" and review-median analogs.
    started_at_secs: u64,
    first_gain_secs: Option<u64>,
    /// Verdict-tool sightings: first and last turn that upserted a
    /// `request_verdict` tool call, and how many turns did so. First→last
    /// is the observed verdict turnaround for this chain.
    verdict_first_seen_secs: Option<u64>,
    verdict_last_seen_secs: Option<u64>,
    verdict_rounds: u32,
    /// Tool-call ids already counted, so retry upserts do not double-count.
    verdict_tool_ids: Vec<String>,
    /// Consecutive chained hops that finished under [`MIN_TURN_RUNTIME_SECS`]
    /// (issue 058's minimum-runtime guard). Any turn that takes real time
    /// resets the streak.
    near_instant_turns: u32,
}

/// One chain's KPI snapshot (issue 057), logged at every chain stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainKpis {
    /// Chain duration so far, in seconds.
    pub duration_secs: u64,
    /// Seconds from chain start to the first plan-scoreboard gain.
    pub first_gain_secs: Option<u64>,
    /// Orchestration rounds in this chain.
    pub turns: u32,
    /// Turns that upserted a `request_verdict` tool call.
    pub verdict_rounds: u32,
    /// First→last verdict sighting, the observed turnaround.
    pub verdict_turnaround_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnCount {
    /// Turns in this chain, for logs and the orchestrator's context.
    pub turn: u32,
    /// Consecutive turns without progress; what the runaway cap compares.
    pub idle_turns: u32,
    /// Consecutive chained hops that finished near-instantly (issue 058);
    /// what the crash-loop guard compares.
    pub near_instant_turns: u32,
}

static CHAINS: Mutex<Option<HashMap<String, ChainState>>> = Mutex::new(None);

/// Sessions a same-thread auto-prompt continuation was just dispatched to.
/// The next finished turn on such a session counts as the same chain no
/// matter how long the turn took: agentic turns routinely outlive
/// `CHAIN_TIMEOUT_SECS`, and applying the timeout to chained hops resets
/// the idle streak every round, so the runaway cap could never fire on a
/// slow idle loop (`loop-bug`). One-shot: consumed by the next
/// [`record_turn`].
static CONTINUATION_ARMED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn with_continuation_armed<R>(f: impl FnOnce(&mut HashSet<String>) -> R) -> R {
    let mut guard = CONTINUATION_ARMED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(HashSet::new))
}

/// Marks the session's next finished turn as auto-prompt-chained. Called
/// when a continuation outcome targeting the same thread is built.
pub fn arm_continuation(session_id: &str) {
    with_continuation_armed(|armed| {
        armed.insert(session_id.to_string());
    });
}

fn take_armed_continuation(session_id: &str) -> bool {
    with_continuation_armed(|armed| armed.remove(session_id))
}

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
/// A turn following an auto-prompt continuation ([`arm_continuation`]) never
/// counts as a chain timeout, however long the worker turn took.
pub fn record_turn(session_id: &str, made_progress: bool) -> TurnCount {
    let chained = take_armed_continuation(session_id);
    record_turn_at_impl(session_id, made_progress, chained, now_secs())
}

/// Test seam for [`record_turn`]'s timeout behavior with a fixed clock.
#[cfg(test)]
fn record_turn_at(session_id: &str, made_progress: bool, now: u64) -> TurnCount {
    record_turn_at_impl(session_id, made_progress, false, now)
}

/// Test seam for [`record_turn`]'s auto-chained hop behavior with a fixed
/// clock.
#[cfg(test)]
fn record_turn_at_chained(session_id: &str, made_progress: bool, now: u64) -> TurnCount {
    record_turn_at_impl(session_id, made_progress, true, now)
}

fn record_turn_at_impl(
    session_id: &str,
    made_progress: bool,
    chained: bool,
    now: u64,
) -> TurnCount {
    with_chain(session_id, |chain| {
        if !chained
            && chain.last_turn_secs > 0
            && now.saturating_sub(chain.last_turn_secs) > CHAIN_TIMEOUT_SECS
        {
            log::info!(
                "auto_prompt: chain timeout ({}s since the last turn), starting a new chain (session={session_id})",
                now.saturating_sub(chain.last_turn_secs)
            );
            *chain = ChainState::default();
        }
        chain.turns += 1;
        if chain.started_at_secs == 0 {
            chain.started_at_secs = now;
        }
        chain.idle_turns = if made_progress {
            0
        } else {
            chain.idle_turns + 1
        };
        // Minimum-runtime guard (issue 058): only chained hops have a
        // meaningful runtime — the gap since the previous turn's end is the
        // worker turn itself. A near-instant hop is the worker dying on its
        // first turn; real work resets the streak.
        if chained {
            let runtime = now.saturating_sub(chain.last_turn_secs);
            chain.near_instant_turns = if made_progress || runtime >= MIN_TURN_RUNTIME_SECS {
                0
            } else {
                chain.near_instant_turns + 1
            };
        } else {
            chain.near_instant_turns = 0;
        }
        chain.last_turn_secs = now;
        TurnCount {
            turn: chain.turns,
            idle_turns: chain.idle_turns,
            near_instant_turns: chain.near_instant_turns,
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
        if !chain
            .steps_ledger
            .iter()
            .any(|entry| entry.fingerprint == steps_fingerprint)
        {
            chain.steps_ledger.push(LedgerEntry {
                fingerprint: steps_fingerprint,
                excerpt: ledger_excerpt(steps),
            });
            if chain.steps_ledger.len() > STEPS_LEDGER_CAP {
                chain.steps_ledger.remove(0);
            }
        }
        chain.steps_repeats
    })
}

/// Collapses a steps block to a single-line excerpt of at most
/// [`LEDGER_EXCERPT_CHARS`] characters, taken via `chars().take()` so the
/// cut always lands on a char boundary.
fn ledger_excerpt(steps: &str) -> String {
    let collapsed = steps.split_whitespace().collect::<Vec<&str>>().join(" ");
    collapsed.chars().take(LEDGER_EXCERPT_CHARS).collect()
}

/// The tried-steps ledger for this chain, oldest first. Continuation
/// prompts embed it so the worker does not re-propose steps the chain has
/// already worked through.
pub fn tried_steps_excerpts(session_id: &str) -> Vec<String> {
    with_chain(session_id, |chain| {
        chain
            .steps_ledger
            .iter()
            .map(|entry| entry.excerpt.clone())
            .collect()
    })
}

/// The plan-scoreboard delta between this round and the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanProgress {
    /// First observation this chain — no opinion yet.
    First,
    /// At least one plan checkbox flipped to checked since the last round.
    Advanced {
        /// How many more tasks are checked than last round.
        checked_delta: u32,
    },
    /// Identical counts — no plan-file movement this round.
    Flat,
    /// Fewer checked boxes than last round (a box was unchecked or the plan
    /// shrank). Never resets the idle streak; the caller logs it.
    Regressed,
}

/// Records this round's (checked, actionable-unchecked) task counts across
/// the session's claimed plan files and classifies the delta. Flipped
/// checkboxes are the plan-level scoreboard: a round that advances the plan
/// counts as progress even when the summary's wording repeats.
pub fn record_plan_progress(session_id: &str, checked: u32, actionable: u32) -> PlanProgress {
    with_chain(session_id, |chain| {
        let progress = match chain.last_plan_progress {
            None => PlanProgress::First,
            Some((last_checked, _)) if checked > last_checked => PlanProgress::Advanced {
                checked_delta: checked - last_checked,
            },
            Some((last_checked, _)) if checked < last_checked => PlanProgress::Regressed,
            Some(_) => PlanProgress::Flat,
        };
        if matches!(progress, PlanProgress::Advanced { .. }) && chain.first_gain_secs.is_none() {
            chain.first_gain_secs = Some(now_secs());
        }
        chain.last_plan_progress = Some((checked, actionable));
        progress
    })
}

/// Counts the round's `request_verdict` tool upserts for the KPI record
/// (issue 057). Tool ids already seen this chain are skipped — retries
/// upsert the same id, and the turnaround measures negotiations, not polls.
pub fn record_verdict_sightings(session_id: &str, tool_ids: &[String]) {
    if tool_ids.is_empty() {
        return;
    }
    let now = now_secs();
    with_chain(session_id, |chain| {
        for id in tool_ids {
            if chain.verdict_tool_ids.iter().any(|seen| seen == id) {
                continue;
            }
            chain.verdict_tool_ids.push(id.clone());
            chain.verdict_rounds += 1;
            chain.verdict_first_seen_secs.get_or_insert(now);
            chain.verdict_last_seen_secs = Some(now);
        }
    })
}

/// The chain's KPI snapshot, for the stop log (issue 057). `None` when the
/// session holds no chain (nothing ran).
pub fn chain_kpis(session_id: &str) -> Option<ChainKpis> {
    with_chain(session_id, |chain| {
        if chain.turns == 0 {
            return None;
        }
        Some(ChainKpis {
            duration_secs: chain.last_turn_secs.saturating_sub(chain.started_at_secs),
            first_gain_secs: chain
                .first_gain_secs
                .map(|at| at.saturating_sub(chain.started_at_secs)),
            turns: chain.turns,
            verdict_rounds: chain.verdict_rounds,
            verdict_turnaround_secs: match (
                chain.verdict_first_seen_secs,
                chain.verdict_last_seen_secs,
            ) {
                (Some(first), Some(last)) => Some(last.saturating_sub(first)),
                _ => None,
            },
        })
    })
}

/// Whether this session's first valid keyword stop already got its
/// housekeeping pass.
pub fn stop_housekeeping_sent(session_id: &str) -> bool {
    with_chain(session_id, |chain| chain.stop_housekeeping_sent)
}

pub fn record_stop_housekeeping_sent(session_id: &str) {
    with_chain(session_id, |chain| chain.stop_housekeeping_sent = true);
}

/// Pure read: has this exact violating section already been rejected once in
/// this session?
pub fn stop_violation_seen(session_id: &str, section: &str) -> bool {
    let print = fingerprint(section);
    with_chain(session_id, |chain| {
        chain.stop_violation_fingerprint == Some(print)
    })
}

/// Commit a rejected violation section so a re-emitted identical stop is
/// honored instead of corrected again.
pub fn record_stop_violation(session_id: &str, section: &str) {
    let print = fingerprint(section);
    with_chain(session_id, |chain| {
        chain.stop_violation_fingerprint = Some(print)
    });
}

pub fn clear_session(session_id: &str) {
    let mut guard = CHAINS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(chains) = guard.as_mut() {
        chains.remove(session_id);
    }
    with_continuation_armed(|armed| {
        armed.remove(session_id);
    });
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

/// Builds a stub-connection thread whose entries follow `steps`: `None` is a
/// user message, `Some(tool_call)` an upserted tool call.
#[cfg(test)]
pub(crate) async fn stub_thread_from_steps(
    steps: Vec<Option<agent_client_protocol::schema::v1::ToolCall>>,
    project: &gpui::Entity<project::Project>,
    cx: &mut gpui::TestAppContext,
) -> gpui::Entity<acp_thread::AcpThread> {
    use acp_thread::AgentConnection as _;

    let connection = std::rc::Rc::new(acp_thread::StubAgentConnection::new());
    let thread = cx
        .update(|cx| {
            connection.new_session(
                project.clone(),
                util::path_list::PathList::new(&[std::path::Path::new("/test")]),
                cx,
            )
        })
        .await
        .expect("stub session");
    thread.update(cx, |thread, cx| {
        for step in steps {
            match step {
                None => thread.push_user_content_block(None, "user".into(), cx),
                Some(tool_call) => thread
                    .upsert_tool_call(tool_call, cx)
                    .expect("tool call upsert"),
            }
        }
    });
    thread
}

#[cfg(test)]
pub(crate) async fn test_project(cx: &mut gpui::TestAppContext) -> gpui::Entity<project::Project> {
    cx.update(|cx| {
        let mut settings_store = settings::SettingsStore::test(cx);
        settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
        cx.set_global(settings_store);
    });
    let fs = fs::FakeFs::new(cx.executor());
    project::Project::test(fs, [], cx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1 as acp_v1;

    fn tool(
        id: &str,
        kind: acp_v1::ToolKind,
        input: serde_json::Value,
    ) -> Option<acp_v1::ToolCall> {
        Some(
            acp_v1::ToolCall::new(id.to_string(), "label")
                .kind(kind)
                .status(acp_v1::ToolCallStatus::Completed)
                .raw_input(input),
        )
    }

    fn terminal(id: &str, command: &str) -> Option<acp_v1::ToolCall> {
        tool(
            id,
            acp_v1::ToolKind::Execute,
            serde_json::json!({ "command": command }),
        )
    }

    async fn made_progress(
        steps: Vec<Option<acp_v1::ToolCall>>,
        project: &gpui::Entity<project::Project>,
        cx: &mut gpui::TestAppContext,
    ) -> bool {
        stub_thread_from_steps(steps, project, cx)
            .await
            .read_with(cx, |thread, _| turn_made_progress(thread.entries()))
    }

    #[gpui::test]
    async fn progress_reads_only_the_latest_turns_tool_calls(cx: &mut gpui::TestAppContext) {
        let project = test_project(cx).await;
        let path = serde_json::json!({ "path": "/test/a.rs" });

        for kind in [
            acp_v1::ToolKind::Edit,
            acp_v1::ToolKind::Delete,
            acp_v1::ToolKind::Move,
        ] {
            assert!(
                made_progress(vec![None, tool("t", kind, path.clone())], &project, cx).await,
                "{kind:?} must count as progress"
            );
        }
        assert!(
            made_progress(
                vec![None, terminal("t", "cargo fmt && git commit -qm 'fix: x'")],
                &project,
                cx
            )
            .await
        );
        assert!(made_progress(vec![None, terminal("t", "git push -q")], &project, cx).await);

        // Reading, searching and re-checking change nothing.
        assert!(
            !made_progress(
                vec![
                    None,
                    tool("read", acp_v1::ToolKind::Read, path.clone()),
                    tool(
                        "search",
                        acp_v1::ToolKind::Search,
                        serde_json::json!({ "pattern": "git commit" })
                    ),
                    terminal("status", "git status --short && ./script/clippy"),
                ],
                &project,
                cx
            )
            .await
        );
        // An edit from an earlier turn does not count for this one.
        assert!(
            !made_progress(
                vec![
                    None,
                    tool("edit", acp_v1::ToolKind::Edit, path.clone()),
                    None,
                    terminal("log", "git log --oneline -3"),
                ],
                &project,
                cx
            )
            .await
        );
        // A turn with no tool calls at all.
        assert!(!made_progress(vec![None], &project, cx).await);
    }

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
                idle_turns: 1,
                near_instant_turns: 0
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
                idle_turns: 0,
                near_instant_turns: 0
            }
        );
        assert_eq!(record_turn_at(session, false, now + 3).idle_turns, 1);
        assert_eq!(current_turn(session), 4);
        // A long gap starts a new chain.
        assert_eq!(
            record_turn_at(session, false, now + 3 + CHAIN_TIMEOUT_SECS + 1),
            TurnCount {
                turn: 1,
                idle_turns: 1,
                near_instant_turns: 0
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
    fn ledger_keeps_distinct_proposals_and_caps_size() {
        let session = "chain-test-ledger";
        clear_session(session);
        record_summary_steps(session, "- Plan 055: mine the schedulers");
        // The same proposal again must not add a second ledger entry.
        record_summary_steps(session, "- plan 055 mine the schedulers");
        record_summary_steps(session, "- Issue 016: 9 more chains");
        assert_eq!(tried_steps_excerpts(session).len(), 2);
        for index in 0..(STEPS_LEDGER_CAP + 2) {
            record_summary_steps(session, &format!("- fresh proposal {index} with new words"));
        }
        assert_eq!(tried_steps_excerpts(session).len(), STEPS_LEDGER_CAP);
        // The oldest entries dropped off the front.
        assert!(
            !tried_steps_excerpts(session)
                .iter()
                .any(|excerpt| excerpt.contains("Plan 055"))
        );
        clear_session(session);
    }

    #[test]
    fn ledger_excerpt_is_single_line_and_char_bounded() {
        let session = "chain-test-ledger-excerpt";
        clear_session(session);
        let long = format!("- {} {}", "汉字".repeat(200), "tail marker");
        record_summary_steps(session, &long);
        let excerpts = tried_steps_excerpts(session);
        assert_eq!(excerpts.len(), 1);
        let excerpt = &excerpts[0];
        assert!(!excerpt.contains('\n'));
        assert!(excerpt.chars().count() <= LEDGER_EXCERPT_CHARS);
        clear_session(session);
    }

    #[test]
    fn plan_progress_classifies_deltas_per_session() {
        let session = "chain-test-plan-progress";
        clear_session(session);
        assert_eq!(record_plan_progress(session, 1, 4), PlanProgress::First);
        assert_eq!(
            record_plan_progress(session, 3, 2),
            PlanProgress::Advanced { checked_delta: 2 }
        );
        assert_eq!(record_plan_progress(session, 3, 2), PlanProgress::Flat);
        assert_eq!(record_plan_progress(session, 2, 3), PlanProgress::Regressed);
        // A fresh chain starts over at First.
        clear_session(session);
        assert_eq!(record_plan_progress(session, 0, 0), PlanProgress::First);
        clear_session(session);
    }

    #[test]
    fn landing_commands_count_as_progress() {
        assert!(command_lands_work("cargo fmt && git commit -qm 'fix: x'"));
        assert!(command_lands_work("git pull --rebase -q && git push -q"));
        assert!(!command_lands_work("git status --short && git log -1"));
        assert!(!command_lands_work("ssh host true"));
    }

    #[test]
    fn chained_hops_never_time_out_the_chain() {
        let session = "chain-test-chained";
        clear_session(session);
        arm_continuation(session);
        // First chained hop lands long after the timeout window: the idle
        // streak must still start at 1, not restart a fresh chain.
        let late = 1_000 + CHAIN_TIMEOUT_SECS + 1;
        assert_eq!(
            record_turn_at_chained(session, false, late),
            TurnCount {
                turn: 1,
                idle_turns: 1,
                near_instant_turns: 0
            }
        );
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, false, late + CHAIN_TIMEOUT_SECS + 1).idle_turns,
            2,
            "a second chained hop must keep accumulating the idle streak"
        );
        // An unchained turn after the timeout still resets — a manual session
        // that went quiet is a new chain.
        assert_eq!(
            record_turn_at(session, false, late + 2 * CHAIN_TIMEOUT_SECS + 2),
            TurnCount {
                turn: 1,
                idle_turns: 1,
                near_instant_turns: 0
            }
        );
        clear_session(session);
    }

    #[test]
    fn armed_continuation_is_consumed_by_one_turn() {
        let session = "chain-test-armed-consume";
        clear_session(session);
        assert!(!take_armed_continuation(session));
        arm_continuation(session);
        assert!(take_armed_continuation(session));
        assert!(!take_armed_continuation(session));
        clear_session(session);
    }

    #[test]
    fn near_instant_chained_hops_accumulate_until_real_work_resets() {
        let session = "chain-test-near-instant";
        clear_session(session);
        let start = 2_000;
        // Seed the chain with a real first turn.
        record_turn_at(session, false, start);
        // Chained hops finishing under MIN_TURN_RUNTIME_SECS stack the
        // crash-loop streak.
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, false, start + 1).near_instant_turns,
            1
        );
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, false, start + 2).near_instant_turns,
            2
        );
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, false, start + 3).near_instant_turns,
            3,
            "at NEAR_INSTANT_LIMIT the decider halts the chain"
        );
        // A hop that takes real time (or lands work) resets the streak.
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, false, start + 3 + MIN_TURN_RUNTIME_SECS)
                .near_instant_turns,
            0
        );
        // Even a near-instant hop that made progress resets — the worker
        // may legitimately finish fast when there was nothing to do.
        arm_continuation(session);
        assert_eq!(
            record_turn_at_chained(session, true, start + 3 + MIN_TURN_RUNTIME_SECS + 1)
                .near_instant_turns,
            0
        );
        // An unchained turn never counts: a fresh stop after long quiet is
        // not a crash loop.
        assert_eq!(
            record_turn_at(session, false, start + 3 + MIN_TURN_RUNTIME_SECS + 2)
                .near_instant_turns,
            0
        );
        clear_session(session);
    }
}
