use acp_thread::{SUBAGENT_SESSION_INFO_META_KEY, SubagentSessionInfo, verdict};
use agent_client_protocol::schema::v1 as acp;
use agent_settings::AgentSettings;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use super::spawn_agent_tool::deserialize_session_id;
use crate::{AgentTool, ThreadEnvironment, ToolCallEventStream, ToolInput};
use settings::Settings as _;

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
///   calling this tool.
/// - Rounds are capped (`agent.verdict_max_rounds`, default 3). When the cap
///   is hit the tool refuses further calls — present the remaining
///   disagreement to the user instead of looping.
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
        session_info: SubagentSessionInfo,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(default)]
        session_id: Option<acp::SessionId>,
        error: String,
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
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "output": output,
                "round": round,
                "max_rounds": max_rounds,
            }))
            .unwrap_or_else(|e| format!("Failed to serialize request_verdict output: {e}"))
            .into(),
            RequestVerdictToolOutput::Error {
                session_id,
                error,
                session_info: _, // Don't show this to the model
            } => serde_json::to_string(&serde_json::json!({
                "session_id": session_id,
                "error": error,
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
                    session_info: None,
                })?;

            let (verdict_enabled, max_rounds) = cx.update(|cx| {
                let settings = AgentSettings::get_global(cx);
                (settings.verdict_ping_pong, settings.verdict_max_rounds)
            });

            // GOAT gate (proposal 001 phase 5): the tool stays silent unless
            // the flag opts in, so default builds never pay for the loop.
            if !verdict_enabled || max_rounds == 0 {
                return Err(RequestVerdictToolOutput::Error {
                    session_id: None,
                    error: "verdict ping-pong is disabled (set agent.verdict_ping_pong = true)"
                        .to_string(),
                    session_info: None,
                });
            }

            let (subagent, mut session_info, round) = cx.update(|cx| {
                let subagent = if let Some(session_id) = input.session_id.clone() {
                    if let Some(rounds) = verdict::rounds(&session_id)
                        && rounds >= max_rounds
                    {
                        return Err(RequestVerdictToolOutput::Error {
                            session_id: Some(session_id),
                            error: format!(
                                "verdict negotiation already used all {max_rounds} rounds; \
                                 stop calling request_verdict and present the remaining \
                                 disagreement to the user"
                            ),
                            session_info: None,
                        });
                    }
                    self.environment.resume_subagent(session_id, cx)
                } else {
                    self.environment
                        .create_verdict_subagent(input.label.clone(), cx)
                }
                .map_err(|err| RequestVerdictToolOutput::Error {
                    session_id: None,
                    error: err.to_string(),
                    session_info: None,
                })?;

                let session_info = SubagentSessionInfo {
                    session_id: subagent.id(),
                    message_start_index: subagent.num_entries(cx),
                    message_end_index: None,
                };

                // Registers the session for auto_prompt suppression and
                // returns this round's 1-based number.
                let round = verdict::register(&session_info.session_id);

                event_stream.subagent_spawned(session_info.session_id.clone());
                event_stream.update_fields_with_meta(
                    acp::ToolCallUpdateFields::new(),
                    Some(acp::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        serde_json::json!(&session_info),
                    )])),
                );

                Ok((subagent, session_info, round))
            })?;

            let send_result = subagent.send(input.message, cx).await;

            let status = if send_result.is_ok() {
                "completed"
            } else {
                "error"
            };
            telemetry::event!(
                "Verdict Subagent Completed",
                subagent_session = session_info.session_id.to_string(),
                round,
                max_rounds,
                status,
            );

            session_info.message_end_index =
                cx.update(|cx| Some(subagent.num_entries(cx).saturating_sub(1)));

            let meta = Some(acp::Meta::from_iter([(
                SUBAGENT_SESSION_INFO_META_KEY.into(),
                serde_json::json!(&session_info),
            )]));

            let (output, result) = match send_result {
                Ok(output) => (
                    output.clone(),
                    Ok(RequestVerdictToolOutput::Success {
                        session_id: session_info.session_id.clone(),
                        session_info,
                        round,
                        max_rounds,
                        output,
                    }),
                ),
                Err(e) => {
                    let error = e.to_string();
                    (
                        error.clone(),
                        Err(RequestVerdictToolOutput::Error {
                            session_id: Some(session_info.session_id.clone()),
                            error,
                            session_info: Some(session_info),
                        }),
                    )
                }
            };
            event_stream.update_fields_with_meta(
                acp::ToolCallUpdateFields::new().content(vec![output.into()]),
                meta,
            );
            result
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
    fn success_output_serializes_round_budget_for_the_model() {
        let output = RequestVerdictToolOutput::Success {
            session_id: acp::SessionId::new("reviewer-1"),
            output: "#Verdict: AGREE".to_string(),
            round: 2,
            max_rounds: 3,
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
        // session_info must not leak to the model
        assert!(parsed.get("session_info").is_none());
    }
}
