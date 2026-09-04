use acp_thread::{
    SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo, verdict, verdict::VerdictReviewer,
};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings as _, VerdictReviewerSetting};
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
///
/// The reviewer backend is `agent.verdict_reviewer`: `native` (a subagent
/// pinned to `agent.verdict_model`) or `claude_code` (an off-screen Claude
/// Code session using its own subscription auth).
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
#[derive(Debug, PartialEq)]
enum ReviewerRoute {
    /// Subagent spawned through the environment (phase 1-4 path).
    Native,
    /// External reviewer session spawned via the registered provider (phase 6).
    External(ReviewerLabel),
}

/// Newtype so `ReviewerRoute` can stay `Debug`/`PartialEq` without requiring
/// `Debug` on the provider trait object.
#[derive(Debug, PartialEq)]
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

            let (verdict_enabled, max_rounds, reviewer_setting) = cx.update(|cx| {
                let settings = AgentSettings::get_global(cx);
                (
                    settings.verdict_ping_pong,
                    settings.verdict_max_rounds,
                    settings.verdict_reviewer.clone(),
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

            let route = match resolve_route(&reviewer_setting, verdict::reviewer()) {
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
                    ReviewerRoute::Native => verdict::complete(&session_id),
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

            let mut route = route;
            let reply = match &route {
                ReviewerRoute::Native => {
                    let subagent = cx.update(|cx| {
                        if let Some(session_id) = input.session_id.clone() {
                            self.environment.resume_subagent(session_id, cx)
                        } else {
                            self.environment
                                .create_verdict_subagent(input.label.clone(), cx)
                        }
                        .map_err(|err| error_output(None, err.to_string(), "native".into(), None))
                    });
                    let subagent = match subagent {
                        Ok(subagent) => subagent,
                        Err(error) => return Err(error),
                    };

                    let session_id = subagent.id();
                    let round = cx.update(|_cx| verdict::register(&session_id));
                    let message_start_index = cx.update(|cx| subagent.num_entries(cx));

                    let send_result = subagent.send(input.message, cx).await;
                    let reply = match send_result {
                        Ok(reply) => reply,
                        Err(err) => {
                            let error = err.to_string();
                            return Err(error_output(
                                Some(session_id.clone()),
                                error,
                                "native".into(),
                                Some(SubagentSessionInfo {
                                    session_id,
                                    message_start_index,
                                    message_end_index: None,
                                }),
                            ));
                        }
                    };
                    let message_end_index =
                        cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

                    if input.final_round {
                        cx.update(|_cx| verdict::complete(&session_id));
                    }

                    (
                        session_id,
                        reply,
                        round,
                        message_start_index,
                        message_end_index,
                    )
                }
                ReviewerRoute::External(_) => {
                    let Some(provider) = verdict::reviewer() else {
                        return Err(error_output(
                            None,
                            "external verdict reviewer was unregistered mid-negotiation"
                                .to_string(),
                            route_label(&route),
                            None,
                        ));
                    };
                    route = ReviewerRoute::external(provider.clone());

                    let (thread, session_id) = if let Some(session_id) = input.session_id.clone() {
                        match verdict::reviewer_thread(&session_id) {
                            Some(thread) => (thread, session_id),
                            None => {
                                return Err(error_output(
                                    Some(session_id),
                                    "verdict reviewer session expired — start a new negotiation \
                                     (call request_verdict without session_id)"
                                        .to_string(),
                                    route_label(&route),
                                    None,
                                ));
                            }
                        }
                    } else {
                        let Some(project) = cx.update(|cx| self.environment.project(cx)) else {
                            return Err(error_output(
                                None,
                                "cannot spawn a claude_code reviewer without a project".to_string(),
                                route_label(&route),
                                None,
                            ));
                        };
                        let work_dirs = cx.update(|cx| self.environment.work_dirs(cx));
                        let spawn = cx
                            .update(|cx| {
                                provider.spawn_session(project, work_dirs.unwrap_or_default(), cx)
                            })
                            .await;
                        let thread = match spawn {
                            Ok(thread) => thread,
                            Err(err) => {
                                return Err(error_output(
                                    None,
                                    err.to_string(),
                                    route_label(&route),
                                    None,
                                ));
                            }
                        };
                        let session_id = cx.update(|cx| thread.read(cx).session_id().clone());
                        (thread, session_id)
                    };

                    let round = cx.update(|_cx| {
                        verdict::register_reviewer_session(&session_id, thread.clone())
                    });

                    match verdict::reviewer_turn(
                        &thread,
                        input.message.clone(),
                        verdict::REVIEWER_TURN_TIMEOUT,
                        cx,
                    )
                    .await
                    {
                        Ok(reply) => {
                            if input.final_round {
                                cx.update(|cx| verdict::complete_reviewer(&session_id, cx));
                            }
                            (session_id, reply, round, 0, None)
                        }
                        Err(err) => {
                            let error = err.to_string();
                            return Err(error_output(
                                Some(session_id),
                                error,
                                route_label(&route),
                                None,
                            ));
                        }
                    }
                }
            };

            let (session_id, output, round, message_start_index, message_end_index) = reply;

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
            if matches!(route, ReviewerRoute::Native) {
                event_stream.subagent_spawned(session_id.clone());
                event_stream
                    .update_fields_with_meta(acp::ToolCallUpdateFields::new(), meta.clone());
            }

            event_stream.update_fields_with_meta(
                acp::ToolCallUpdateFields::new().content(vec![output.clone().into()]),
                meta,
            );

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
        event_stream.update_fields_with_meta(
            acp::ToolCallUpdateFields::new().content(vec![content]),
            meta,
        );

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
    fn resolve_route_defaults_to_native_and_never_silently_falls_back() {
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
