use acp_thread::{
    SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo, verdict, verdict::VerdictReviewer,
};
use agent_client_protocol::schema::v2 as acp;
use agent_settings::AgentSettings;
use anyhow::Result;
use gpui::{App, AsyncApp, Entity, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{LanguageModelSelection, Settings as _, VerdictReviewerSetting};
use std::rc::Rc;
use std::sync::Arc;

use super::spawn_agent_tool::deserialize_session_id;
use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};

/// Run one round of a verdict ping-pong with a dedicated reviewer sub-agent
/// (proposal 001: claude-sub-agent-verdict).
///
/// The first call creates a new reviewer thread; every follow-up MUST reuse
/// the returned `session_id` so the negotiation continues in the SAME thread
/// instead of starting over.
///
/// ### Protocol
/// - Round 1 message: include your complete `## Summary` plus this
///   instruction: "Reply with a verdict that MUST start with `#Verdict: AGREE`
///   or `#Verdict: REVISE` followed by bullet-point reasons." Include any file
///   paths or plan state the reviewer needs — it cannot see your context.
/// - If the reply starts with `#Verdict: REVISE`: address every reason with
///   evidence, then call this tool again with the SAME `session_id`.
/// - If the reply starts with `#Verdict: AGREE` and you agree with it: state
///   your own agreement, restate the final agreed `## Summary`, and stop
///   calling this tool. Pass `final_round: true` on that closing call so the
///   reviewer session is freed.
/// - Rounds are capped (`agent.verdict_max_rounds`, default 3). When the cap
///   is hit the tool refuses further calls — present the remaining
///   disagreement to the user instead of looping.
/// - Owner-gated items: when your summary parks work as owner-gated, list
///   each item and ask the reviewer to add an `#OwnerGate` section with one
///   decision per item. Act on those decisions afterwards instead of stopping
///   on them, unless the action is destructive, irreversible, or outward-facing.
///
/// The default reviewer backend is `agent.verdict_reviewer`: `native` (a
/// subagent pinned to `agent.verdict_model`) or `claude_code` (an off-screen
/// Claude Code session using its own subscription auth). Pass
/// `reviewer: "secondary"` only when the user asked for the secondary
/// reviewer (`agent.verdict_secondary_model`, e.g. GLM); keep the same value
/// for every round of a negotiation.
///
/// If the backend is rate-limited mid-negotiation, the tool fails over to
/// `agent.verdict_secondary_model` (the round budget carries over) or, when
/// no secondary is configured, re-parks with the backend's reset hint — do
/// not retry before that window passes. Transient external-lane failures
/// (spawn blips, timeouts) get one bounded retry before the round reports
/// failed.
///
/// ### Output
/// - You receive only the reviewer's final message for this round, plus the
///   `session_id`, the `round` just consumed, and `max_rounds`.
/// - Error results may include a `session_id` if a session already exists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct RequestVerdictToolInput {
    /// Short label displayed in the UI while the reviewer runs (e.g., "Verdict round 2")
    pub label: String,
    /// The message for the reviewer. For round 1 include the full summary and
    /// the `#Verdict` reply instruction; for later rounds reply to the
    /// reviewer's previous verdict directly.
    pub message: String,
    /// Session ID of the reviewer session from the previous round. Omit only
    /// for round 1.
    #[serde(default, deserialize_with = "deserialize_session_id")]
    pub session_id: Option<acp::SessionId>,
    /// Set `true` on the closing round (reviewer replied AGREE and you agree)
    /// so the reviewer session is freed. Omit on every other round.
    #[serde(default)]
    pub final_round: bool,
    /// Which reviewer runs the negotiation. Omit for the default reviewer;
    /// `secondary` only when the user asked for it.
    #[serde(default)]
    pub reviewer: RequestedReviewer,
}

/// Reviewer requested by the worker for a `request_verdict` negotiation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestedReviewer {
    /// The backend selected by `agent.verdict_reviewer`.
    #[default]
    Default,
    /// A native subagent pinned to `agent.verdict_secondary_model`.
    Secondary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[serde(rename_all = "snake_case")]
pub enum RequestVerdictToolOutput {
    Success {
        session_id: acp::SessionId,
        output: String,
        round: usize,
        max_rounds: usize,
        /// Reviewer backend label ("native" | provider label). Persisted with
        /// the thread record so the GOAT scorer (`.issues/016`) can join
        /// negotiations to threads without relying on log-only telemetry.
        /// Defaulted so pre-field threads still deserialize on replay.
        #[serde(default)]
        reviewer: String,
        session_info: SubagentSessionInfo,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        session_id: Option<acp::SessionId>,
        error: String,
        /// Same as the success variant's `reviewer`; empty when the failure
        /// predates route resolution (tool-input transport errors).
        #[serde(default)]
        reviewer: String,
        session_info: Option<SubagentSessionInfo>,
    },
}

impl From<RequestVerdictToolOutput> for LanguageModelToolResultContent {
    fn from(output: RequestVerdictToolOutput) -> Self {
        match output {
            RequestVerdictToolOutput::Success {
                session_id,
                output,
                round,
                max_rounds,
                reviewer,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "output": output,
                "round": round,
                "max_rounds": max_rounds,
                "reviewer": reviewer,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize request_verdict output: {e}"))
            .into(),
            RequestVerdictToolOutput::Error {
                session_id,
                error,
                reviewer,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "error": error,
                "reviewer": reviewer,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize request_verdict output: {e}"))
            .into(),
        }
    }
}

/// Tool that runs one verdict ping-pong round against a reviewer sub-agent.
pub struct RequestVerdictTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl RequestVerdictTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

/// Which backend the tool drives for this negotiation.
#[derive(Debug, Clone, PartialEq)]
enum ReviewerRoute {
    /// Subagent spawned through the environment (phase 1-4 path).
    Native,
    /// Native subagent pinned to `agent.verdict_secondary_model`.
    Secondary(LanguageModelSelection),
    /// External reviewer session spawned via the registered provider (phase 6).
    External(ReviewerLabel),
}

/// Newtype so `ReviewerRoute` can stay `Debug`/`PartialEq` without requiring
/// `Debug` on the provider trait object.
#[derive(Debug, Clone, PartialEq)]
struct ReviewerLabel(&'static str);

impl ReviewerRoute {
    fn external(provider: Arc<dyn VerdictReviewer>) -> Self {
        ReviewerRoute::External(ReviewerLabel(provider.label()))
    }
}

/// Pure so the routing table stays unit-testable: `claude_code` without a
/// registered provider is an error, never a silent fallback — a silent
/// fallback would run the review on the worker's own model, defeating the
/// point of a second opinion.
fn resolve_route(
    setting: &VerdictReviewerSetting,
    provider: Option<Arc<dyn VerdictReviewer>>,
) -> Result<ReviewerRoute, String> {
    match setting {
        VerdictReviewerSetting::Native => Ok(ReviewerRoute::Native),
        VerdictReviewerSetting::ClaudeCode => match provider {
            Some(provider) => Ok(ReviewerRoute::external(provider)),
            None => Err(
                "verdict_reviewer is claude_code but Claude Code is not connected — \
                 connect it in the agent panel or set agent.verdict_reviewer = \"native\""
                    .to_string(),
            ),
        },
    }
}

/// Routes a negotiation by the worker's requested reviewer. A `secondary`
/// request without `verdict_secondary_model` is an error, never a fallback
/// to the default reviewer — the user explicitly asked for a different model.
fn resolve_requested_route(
    requested: RequestedReviewer,
    setting: &VerdictReviewerSetting,
    secondary_model: Option<LanguageModelSelection>,
    provider: Option<Arc<dyn VerdictReviewer>>,
) -> Result<ReviewerRoute, String> {
    match requested {
        RequestedReviewer::Default => resolve_route(setting, provider),
        RequestedReviewer::Secondary => match secondary_model {
            Some(model) => Ok(ReviewerRoute::Secondary(model)),
            None => Err(
                "reviewer is secondary but agent.verdict_secondary_model is not set \
                 — set it (e.g. a GLM model) or omit `reviewer`"
                    .to_string(),
            ),
        },
    }
}

fn error_output(
    session_id: Option<acp::SessionId>,
    error: String,
    reviewer: String,
    session_info: Option<SubagentSessionInfo>,
) -> RequestVerdictToolOutput {
    RequestVerdictToolOutput::Error {
        session_id,
        error,
        reviewer,
        session_info,
    }
}

/// Label for a resolved route — the same string the telemetry event and the
/// persisted tool output carry.
fn route_label(route: &ReviewerRoute) -> String {
    match route {
        ReviewerRoute::Native => "native".to_string(),
        ReviewerRoute::Secondary(model) => {
            format!("secondary:{}/{}", model.provider.0, model.model)
        }
        ReviewerRoute::External(label) => label.0.to_string(),
    }
}

/// Label for a not-yet-resolved (or unresolvable) setting — keeps error
/// outputs self-describing for the GOAT scorer.
fn setting_label(setting: &VerdictReviewerSetting) -> String {
    match setting {
        VerdictReviewerSetting::Native => "native".to_string(),
        VerdictReviewerSetting::ClaudeCode => "claude_code".to_string(),
    }
}

/// One reviewer attempt that ended in a failure the orchestrator may still
/// react to (fail over, re-park) — issue 037. Plain aborts from config
/// errors keep using [`error_output`] directly.
struct AttemptFailure {
    session_id: Option<acp::SessionId>,
    session_info: Option<SubagentSessionInfo>,
    error: String,
    /// The reviewer thread's own API error text (`AcpThread::last_api_error`),
    /// read where the lane holds the thread (external route). Carries the real
    /// backend message behind a wrapper error like "empty reply".
    api_error: Option<String>,
    route: ReviewerRoute,
}

impl AttemptFailure {
    fn classify(&self) -> verdict::ReviewerFailure {
        verdict::classify_reviewer_failure(&self.error, self.api_error.as_deref())
    }

    fn error_output(self) -> RequestVerdictToolOutput {
        error_output(
            self.session_id,
            self.error,
            route_label(&self.route),
            self.session_info,
        )
    }
}

/// The re-park error for a rate-limited reviewer lane (issue 037 task 1):
/// carries the backend's reset hint and the resume path, so the worker
/// schedules a retry after the window instead of hammering the same
/// rate-limited backend.
fn repark_error(underlying: &str, reset_hint: Option<&str>) -> String {
    let window = reset_hint.unwrap_or("reset time unknown");
    format!(
        "verdict reviewer lane is rate-limited — window {window}. \
         Re-park this verdict request: do not call request_verdict again until \
         after the reset; resume the SAME session_id then, or present the \
         remaining disagreement to the user. Underlying backend error: {underlying}"
    )
}

impl RequestVerdictTool {
    /// Runs one attempt on the given route. `resume_session` resumes an
    /// existing native reviewer session when given (external sessions resume
    /// via the registry); `round_seed` carries over the rounds of a replaced
    /// negotiation (route failover, issue 037).
    async fn run_attempt(
        environment: &Rc<dyn ThreadEnvironment>,
        route: &mut ReviewerRoute,
        input: &RequestVerdictToolInput,
        resume_session: Option<acp::SessionId>,
        round_seed: usize,
        cx: &mut AsyncApp,
    ) -> Result<(acp::SessionId, String, usize, usize, Option<usize>), Box<AttemptFailure>> {
        let external = matches!(&*route, ReviewerRoute::External(_));
        if external {
            Self::run_external_attempt(environment, route, input, cx).await
        } else {
            let model_override = match &*route {
                ReviewerRoute::Secondary(model) => Some(model.clone()),
                _ => None,
            };
            Self::run_native_attempt(
                environment,
                input,
                model_override,
                resume_session,
                route,
                round_seed,
                cx,
            )
            .await
        }
    }

    async fn run_native_attempt(
        environment: &Rc<dyn ThreadEnvironment>,
        input: &RequestVerdictToolInput,
        model_override: Option<LanguageModelSelection>,
        resume_session: Option<acp::SessionId>,
        route: &ReviewerRoute,
        round_seed: usize,
        cx: &mut AsyncApp,
    ) -> Result<(acp::SessionId, String, usize, usize, Option<usize>), Box<AttemptFailure>> {
        let subagent = cx.update(|cx| {
            if let Some(session_id) = resume_session {
                environment.resume_subagent(session_id, cx)
            } else {
                environment.create_verdict_subagent(input.label.clone(), model_override, cx)
            }
            .map_err(|err| Box::new(AttemptFailure {
                session_id: None,
                session_info: None,
                error: err.to_string(),
                api_error: None,
                route: route.clone(),
            }))
        });
        let subagent = match subagent {
            Ok(subagent) => subagent,
            Err(failure) => return Err(failure),
        };

        let session_id = subagent.id();
        let round = cx.update(|_cx| verdict::register_with_offset(&session_id, round_seed));
        let message_start_index = cx.update(|cx| subagent.num_entries(cx));

        // The thread retries provider errors internally (exponential backoff,
        // retry-after capped at 2 min); a send error is post-exhaustion, so no
        // tool-level retry here — the orchestrator's failover/re-park decides.
        let reply = match subagent.send(input.message.clone(), cx).await {
            Ok(reply) => reply,
            Err(err) => {
                return Err(Box::new(AttemptFailure {
                    session_id: Some(session_id.clone()),
                    session_info: Some(SubagentSessionInfo {
                        session_id,
                        message_start_index,
                        message_end_index: None,
                    }),
                    error: err.to_string(),
                    api_error: None,
                    route: route.clone(),
                }));
            }
        };
        let message_end_index = cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

        if input.final_round {
            cx.update(|_cx| verdict::complete(&session_id));
        }

        Ok((
            session_id,
            reply,
            round,
            message_start_index,
            message_end_index,
        ))
    }

    async fn run_external_attempt(
        environment: &Rc<dyn ThreadEnvironment>,
        route: &mut ReviewerRoute,
        input: &RequestVerdictToolInput,
        cx: &mut AsyncApp,
    ) -> Result<(acp::SessionId, String, usize, usize, Option<usize>), Box<AttemptFailure>> {
        let Some(provider) = verdict::reviewer() else {
            return Err(Box::new(AttemptFailure {
                session_id: None,
                session_info: None,
                error: "external verdict reviewer was unregistered mid-negotiation".to_string(),
                api_error: None,
                route: route.clone(),
            }));
        };
        *route = ReviewerRoute::external(provider.clone());

        let (thread, session_id, turn_timeout) = if let Some(session_id) = input.session_id.clone()
        {
            match verdict::reviewer_thread(&session_id) {
                Some(thread) => (thread, session_id, verdict::REVIEWER_TURN_TIMEOUT),
                None => {
                    return Err(Box::new(AttemptFailure {
                        session_id: Some(session_id),
                        session_info: None,
                        error: "verdict reviewer session expired — start a new negotiation \
                             (call request_verdict without session_id)"
                            .to_string(),
                        api_error: None,
                        route: route.clone(),
                    }));
                }
            }
        } else {
            // Issue 037 task 2: one bounded retry on transient spawn blips —
            // retrying a spawn is cheaper than burning the negotiation.
            let spawn_result = Self::spawn_external(environment, route, provider.as_ref(), cx).await;
            let thread = match spawn_result {
                Ok(thread) => thread,
                Err(failure) => {
                    if let Some(delay) = verdict::reviewer_retry_delay(&failure.classify()) {
                        log::warn!(
                            "verdict reviewer spawn failed transiently ({}); retrying once in {delay:?}",
                            failure.error
                        );
                        cx.background_executor().timer(delay).await;
                        match Self::spawn_external(environment, route, provider.as_ref(), cx).await {
                            Ok(thread) => thread,
                            Err(retry_failure) => return Err(retry_failure),
                        }
                    } else {
                        return Err(failure);
                    }
                }
            };
            let session_id = cx.update(|cx| thread.read(cx).session_id().clone());
            // Fresh session: the first turn also pays the CLI's one-time cold
            // start (see REVIEWER_COLD_START_TURN_TIMEOUT).
            (thread, session_id, verdict::REVIEWER_COLD_START_TURN_TIMEOUT)
        };

        let round = cx.update(|_cx| {
            verdict::register_reviewer_session(&session_id, thread.clone())
        });

        // Issue 037 task 2: one bounded retry on transient turn failures. The
        // thread's own API error (rate-limit JSON, overload notice) is read
        // off the AcpThread so a wrapper error ("empty reply") can't hide the
        // real class.
        let mut retried_turn = false;
        let reply = loop {
            match verdict::reviewer_turn(&thread, input.message.clone(), turn_timeout, cx).await {
                Ok(reply) => break Ok(reply),
                Err(err) => {
                    let api_error = cx.update(|cx| {
                        let reviewer = thread.read(cx);
                        reviewer
                            .had_api_error()
                            .then(|| reviewer.last_api_error().map(str::to_string))
                            .flatten()
                    });
                    let class = verdict::classify_reviewer_failure(
                        &err.to_string(),
                        api_error.as_deref(),
                    );
                    if !retried_turn && let Some(delay) = verdict::reviewer_retry_delay(&class) {
                        retried_turn = true;
                        log::warn!(
                            "verdict reviewer turn failed transiently ({err:#}); retrying once in {delay:?}"
                        );
                        cx.background_executor().timer(delay).await;
                        continue;
                    }
                    break Err(Box::new(AttemptFailure {
                        session_id: Some(session_id.clone()),
                        session_info: None,
                        error: err.to_string(),
                        api_error,
                        route: route.clone(),
                    }));
                }
            }
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(failure) => return Err(failure),
        };

        if input.final_round {
            cx.update(|cx| verdict::complete_reviewer(&session_id, cx));
        }

        Ok((session_id, reply, round, 0, None))
    }

    async fn spawn_external(
        environment: &Rc<dyn ThreadEnvironment>,
        route: &ReviewerRoute,
        provider: &dyn VerdictReviewer,
        cx: &mut AsyncApp,
    ) -> Result<Entity<acp_thread::AcpThread>, Box<AttemptFailure>> {
        let Some(project) = cx.update(|cx| environment.project(cx)) else {
            return Err(Box::new(AttemptFailure {
                session_id: None,
                session_info: None,
                error: "cannot spawn a claude_code reviewer without a project".to_string(),
                api_error: None,
                route: route.clone(),
            }));
        };
        let work_dirs = cx.update(|cx| environment.work_dirs(cx));
        let spawn = cx
            .update(|cx| provider.spawn_session(project, work_dirs.unwrap_or_default(), cx))
            .await;
        spawn.map_err(|err| Box::new(AttemptFailure {
            session_id: None,
            session_info: None,
            error: err.to_string(),
            api_error: None,
            route: route.clone(),
        }))
    }
}

impl AgentTool for RequestVerdictTool {
    type Input = RequestVerdictToolInput;
    type Output = RequestVerdictToolOutput;

    const NAME: &'static str = "request_verdict";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => i.label.into(),
            Err(value) => value
                .get("label")
                .and_then(|v| v.as_str())
                .map(|s| SharedString::from(s.to_owned()))
                .unwrap_or_else(|| "Requesting verdict".into()),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| RequestVerdictToolOutput::Error {
                    session_id: None,
                    error: e.to_string(),
                    reviewer: String::new(),
                    session_info: None,
                })?;

            let (verdict_enabled, max_rounds, reviewer_setting, secondary_model) =
                cx.update(|cx| {
                    let settings = AgentSettings::get_global(cx);
                    (
                        settings.fork.verdict_ping_pong,
                        settings.fork.verdict_max_rounds,
                        settings.fork.verdict_reviewer.clone(),
                        settings.fork.verdict_secondary_model.clone(),
                    )
                });

            // GOAT gate (proposal 001 phase 5): user-invoked only, so the
            // gate defaults on — it exists as the demote path while the
            // benchmark (issue 016) decides whether the feature stays.
            if !verdict_enabled || max_rounds == 0 {
                return Err(error_output(
                    None,
                    "verdict ping-pong is disabled (agent.verdict_ping_pong)".to_string(),
                    setting_label(&reviewer_setting),
                    None,
                ));
            }

            let route = match resolve_requested_route(
                input.reviewer,
                &reviewer_setting,
                secondary_model.clone(),
                verdict::reviewer(),
            ) {
                Ok(route) => route,
                Err(error) => {
                    return Err(error_output(
                        None,
                        error,
                        setting_label(&reviewer_setting),
                        None,
                    ));
                }
            };

            // Budget check for resumed negotiations, before touching any
            // session: an over-budget negotiation can no longer continue, so
            // it is torn down here (reviewer session closed) as well.
            if let Some(session_id) = input.session_id.clone()
                && let Some(rounds) = verdict::rounds(&session_id)
                && rounds >= max_rounds
            {
                let error = format!(
                    "verdict negotiation already used all {max_rounds} rounds; \
                     stop calling request_verdict and present the remaining \
                     disagreement to the user"
                );
                match &route {
                    ReviewerRoute::Native | ReviewerRoute::Secondary(_) => {
                        verdict::complete(&session_id)
                    }
                    ReviewerRoute::External(_) => {
                        cx.update(|cx| verdict::complete_reviewer(&session_id, cx));
                    }
                }
                return Err(error_output(
                    Some(session_id),
                    error,
                    route_label(&route),
                    None,
                ));
            }

            // Read before any attempt registers a session: the failover seeds
            // its replacement session with the negotiation's used rounds so
            // the budget carries over (issue 037).
            let prior_rounds = input
                .session_id
                .as_ref()
                .and_then(|session_id| cx.update(|_cx| verdict::rounds(session_id)))
                .unwrap_or(0);

            let mut route = route;
            let mut result = Self::run_attempt(
                &self.environment,
                &mut route,
                &input,
                input.session_id.clone(),
                0,
                cx,
            )
            .await;

            // Issue 037 task 1: a rate-limited reviewer used to burn an abort.
            // With a secondary reviewer configured, this round fails over to
            // it (the round budget carries over); without one, the error below
            // re-parks the request with the reset hint instead.
            let rate_limited = match &result {
                Err(failure) => matches!(
                    failure.classify(),
                    verdict::ReviewerFailure::RateLimit { .. }
                ),
                Ok(_) => false,
            };
            if rate_limited
                && let Some(model) = secondary_model.clone()
                && !matches!(&route, ReviewerRoute::Secondary(_))
            {
                let from_label = route_label(&route);
                let to_route = ReviewerRoute::Secondary(model.clone());
                let to_label = route_label(&to_route);
                // The negotiation moves off the failed session — release it
                // now instead of waiting out the registry TTL.
                let failed_session = match &result {
                    Err(failure) => failure.session_id.clone(),
                    Ok(_) => None,
                };
                if let Some(session_id) = failed_session {
                    match &route {
                        ReviewerRoute::External(_) => {
                            cx.update(|cx| verdict::complete_reviewer(&session_id, cx));
                        }
                        _ => cx.update(|_cx| verdict::complete(&session_id)),
                    }
                }
                telemetry::event!(
                    "Verdict Reviewer Failover",
                    from = from_label.as_str(),
                    to = to_label.as_str(),
                    reason = "rate_limit",
                );
                route = to_route;
                result = Self::run_attempt(&self.environment, &mut route, &input, None, prior_rounds, cx)
                    .await;
            }

            let (session_id, output, round, message_start_index, message_end_index) = match result
            {
                Ok(reply) => reply,
                Err(failure) => {
                    let class = failure.classify();
                    let reviewer = route_label(&failure.route);
                    match class {
                        verdict::ReviewerFailure::RateLimit { reset_hint } => {
                            telemetry::event!(
                                "Verdict Reviewer Failed",
                                reviewer = reviewer.as_str(),
                                class = "rate_limit",
                                status = "re-parked",
                            );
                            return Err(error_output(
                                failure.session_id,
                                repark_error(&failure.error, reset_hint.as_deref()),
                                reviewer,
                                failure.session_info,
                            ));
                        }
                        other_class => {
                            telemetry::event!(
                                "Verdict Reviewer Failed",
                                reviewer = reviewer.as_str(),
                                class = if matches!(
                                    other_class,
                                    verdict::ReviewerFailure::Transient { .. }
                                ) {
                                    "transient"
                                } else {
                                    "other"
                                },
                                status = "failed",
                            );
                            return Err(failure.error_output());
                        }
                    }
                }
            };

            let reviewer = route_label(&route);
            let status = "completed";
            telemetry::event!(
                "Verdict Subagent Completed",
                subagent_session = session_id.to_string(),
                round,
                max_rounds,
                reviewer = reviewer.as_str(),
                status,
            );

            // Only native reviewer sessions are registered with the panel and
            // can be rendered as expandable subagent tool calls. External
            // reviewer sessions are invisible (hidden-orchestrator precedent),
            // so their tool call carries the reply as plain content.
            let session_info = SubagentSessionInfo {
                session_id: session_id.clone(),
                message_start_index,
                message_end_index,
            };
            let meta = Some(acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )]));
            // The subagent session-info meta is what lets the panel render the
            // reviewer as an expandable subagent tool call.
            if matches!(route, ReviewerRoute::Native | ReviewerRoute::Secondary(_))
                && let Some(meta) = meta.clone()
            {
                event_stream.update_fields(|update| update.meta(meta));
            }

            event_stream.update_fields(|update| {
                let update = update.content(vec![output.clone().into()]);
                match meta {
                    Some(meta) => update.meta(meta),
                    None => update,
                }
            });

            Ok(RequestVerdictToolOutput::Success {
                session_id,
                round,
                max_rounds,
                output,
                reviewer,
                session_info,
            })
        })
    }

    fn replay(
        &self,
        _input: Self::Input,
        output: Self::Output,
        event_stream: ToolCallEventStream,
        _cx: &mut App,
    ) -> Result<()> {
        let (content, session_info) = match output {
            RequestVerdictToolOutput::Success {
                output,
                session_info,
                ..
            } => (output.into(), Some(session_info)),
            RequestVerdictToolOutput::Error {
                error,
                session_info,
                ..
            } => (error.into(), session_info),
        };

        let meta = session_info.map(|session_info| {
            acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )])
        });
        event_stream.update_fields(|update| {
            let update = update.content(vec![content]);
            match meta {
                Some(meta) => update.meta(meta),
                None => update,
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::verdict::VerdictReviewer;
    use serde_json::json;

    #[test]
    fn deserializes_blank_session_id_as_absent() {
        for session_id in [json!(null), json!(""), json!("   ")] {
            let input: RequestVerdictToolInput = serde_json::from_value(json!({
                "label": "label",
                "message": "message",
                "session_id": session_id,
            }))
            .unwrap();

            assert!(input.session_id.is_none());
        }

        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
        }))
        .unwrap();
        assert!(input.session_id.is_none());

        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
            "session_id": "existing-session",
        }))
        .unwrap();
        assert_eq!(input.session_id.unwrap().to_string(), "existing-session");
    }

    #[test]
    fn repark_error_names_the_window_and_resume_path() {
        let message = repark_error("boom: quota", Some("resets 1:20am (Asia/Bangkok)"));
        assert!(message.contains("resets 1:20am"), "hint carried: {message}");
        assert!(message.contains("Re-park"));
        assert!(message.contains("session_id"));
        assert!(message.contains("boom: quota"));

        let message = repark_error("boom", None);
        assert!(message.contains("reset time unknown"));
    }

    #[test]
    fn final_round_defaults_to_false_and_parses_true() {
        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
        }))
        .unwrap();
        assert!(!input.final_round);

        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
            "final_round": true,
        }))
        .unwrap();
        assert!(input.final_round);
    }

    struct UnavailableReviewer;

    impl VerdictReviewer for UnavailableReviewer {
        fn label(&self) -> &'static str {
            "unavailable"
        }

        fn spawn_session(
            &self,
            _project: gpui::Entity<project::Project>,
            _work_dirs: util::path_list::PathList,
            _cx: &mut App,
        ) -> Task<anyhow::Result<gpui::Entity<acp_thread::AcpThread>>> {
            Task::ready(Err(anyhow::anyhow!("not available")))
        }
    }

    #[test]
    fn resolve_route_never_silently_falls_back() {
        // claude_code is the shipped default (real second opinion); pin it so
        // a future refactor can't quietly flip back to self-review.
        assert_eq!(
            VerdictReviewerSetting::default(),
            VerdictReviewerSetting::ClaudeCode
        );

        assert_eq!(
            resolve_route(&VerdictReviewerSetting::Native, None),
            Ok(ReviewerRoute::Native)
        );

        // claude_code without a connected provider must error, not fall back:
        // a silent fallback would run the review on the worker's own model.
        let error = resolve_route(&VerdictReviewerSetting::ClaudeCode, None)
            .expect_err("should require a registered provider");
        assert!(error.contains("verdict_reviewer is claude_code"));

        let provider: Arc<dyn VerdictReviewer> = Arc::new(UnavailableReviewer);
        match resolve_route(&VerdictReviewerSetting::ClaudeCode, Some(provider)) {
            Ok(ReviewerRoute::External(label)) => assert_eq!(label.0, "unavailable"),
            other => panic!("expected external route, got {other:?}"),
        }
    }

    #[test]
    fn secondary_reviewer_routes_to_its_model_and_never_falls_back() {
        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
        }))
        .unwrap();
        assert_eq!(input.reviewer, RequestedReviewer::Default);

        let input: RequestVerdictToolInput = serde_json::from_value(json!({
            "label": "label",
            "message": "message",
            "reviewer": "secondary",
        }))
        .unwrap();
        assert_eq!(input.reviewer, RequestedReviewer::Secondary);

        let glm = LanguageModelSelection {
            provider: settings::LanguageModelProviderSetting("GLM".to_string()),
            model: "glm-5.3".to_string(),
            enable_thinking: false,
            effort: None,
            speed: None,
        };
        let route = resolve_requested_route(
            RequestedReviewer::Secondary,
            &VerdictReviewerSetting::ClaudeCode,
            Some(glm.clone()),
            None,
        )
        .expect("secondary model is configured");
        assert_eq!(route_label(&route), "secondary:GLM/glm-5.3");
        assert_eq!(route, ReviewerRoute::Secondary(glm));

        // Unset secondary model must error rather than silently using the
        // default reviewer the user did not pick.
        let error = resolve_requested_route(
            RequestedReviewer::Secondary,
            &VerdictReviewerSetting::Native,
            None,
            None,
        )
        .expect_err("secondary requires verdict_secondary_model");
        assert!(error.contains("verdict_secondary_model"));

        assert_eq!(
            resolve_requested_route(
                RequestedReviewer::Default,
                &VerdictReviewerSetting::Native,
                None,
                None,
            ),
            Ok(ReviewerRoute::Native)
        );
    }

    #[test]
    fn success_output_serializes_round_budget_for_the_model() {
        let output = RequestVerdictToolOutput::Success {
            session_id: acp::SessionId::new("reviewer-1"),
            output: "#Verdict: AGREE".to_string(),
            round: 2,
            max_rounds: 3,
            reviewer: "claude_code".to_string(),
            session_info: SubagentSessionInfo {
                session_id: acp::SessionId::new("reviewer-1"),
                message_start_index: 0,
                message_end_index: None,
            },
        };
        let content: LanguageModelToolResultContent = output.into();
        let text = match content {
            LanguageModelToolResultContent::Text(text) => text.to_string(),
            other => panic!("expected text content, got {other:?}"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["session_id"], "reviewer-1");
        assert_eq!(parsed["round"], 2);
        assert_eq!(parsed["max_rounds"], 3);
        assert_eq!(parsed["reviewer"], "claude_code");
        // session_info must not leak to the model
        assert!(parsed.get("session_info").is_none());
    }

    #[test]
    fn output_deserializes_pre_reviewer_field_records() {
        // Threads saved before the reviewer field existed replay unchanged.
        let success: RequestVerdictToolOutput = serde_json::from_value(json!({
            "session_id": "reviewer-1",
            "output": "#Verdict: AGREE",
            "round": 1,
            "max_rounds": 3,
            "session_info": {
                "session_id": "reviewer-1",
                "message_start_index": 0,
                "message_end_index": null,
            },
        }))
        .unwrap();
        match success {
            RequestVerdictToolOutput::Success { reviewer, .. } => {
                assert_eq!(reviewer, "");
            }
            other => panic!("expected success, got {other:?}"),
        }

        let error: RequestVerdictToolOutput = serde_json::from_value(json!({
            "session_id": "reviewer-1",
            "error": "timed out",
            "session_info": null,
        }))
        .unwrap();
        match error {
            RequestVerdictToolOutput::Error { reviewer, .. } => {
                assert_eq!(reviewer, "");
            }
            other => panic!("expected error, got {other:?}"),
        }
    }
}
