use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{
    AgentTool, ThreadEnvironment, ThreadMessageOutcome, ThreadMessageRequest, ToolCallEventStream,
    ToolInput,
};

/// Send a message to another live agent thread (a subagent, or a top-level thread
/// such as one started with `create_thread`), or to an agent on another machine.
///
/// - `to` may be `name@rig` (e.g. `zed-bb@m3`, `*@amo`) to reach an agent on another
///   machine, or a Claude Code session, through agent-mail. It returns
///   `delivery: "mailed"` and any reply arrives later as a new message.
/// - `deliver_at` schedules agent-mail for later. To wake yourself at a time (check a
///   long job, read results), send to your own `mail_address` (see `list_threads`) with
///   `deliver_at` and end your turn, instead of sleeping or polling in a loop.
/// - Call `list_threads` first if you don't know the target's session_id.
/// - The recipient sees the message wrapped with your thread's title and session_id,
///   so it can answer by calling `send_message` back to you.
/// - If the target is idle, the message starts a turn right away. If it is mid-turn,
///   the message is queued and delivered when that turn ends (`delivery: "queued"`).
/// - Set `wait_for_reply` to block until an idle target finishes the turn your message
///   started and get its final message back. It has no effect when the message is
///   queued, so you never deadlock waiting on a busy thread.
/// - The recipient does not see your conversation; include the context it needs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct SendMessageToolInput {
    /// Session ID of the target thread, its exact title (case-insensitive), or an
    /// agent-mail address `name@rig` for an agent on another machine.
    pub to: String,
    /// The message to deliver.
    pub message: String,
    /// Wait for the target's reply when it is idle. Defaults to false.
    #[serde(default)]
    pub wait_for_reply: bool,
    /// Deliver later instead of now: a delay (`30m`, `8h`, `2d`), a local clock time
    /// (`23:45`), or an RFC 3339 timestamp. Only for `name@rig` targets.
    #[serde(default)]
    pub deliver_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SendMessageToolOutput {
    Success(ThreadMessageOutcome),
    Error { error: String },
}

impl From<SendMessageToolOutput> for LanguageModelToolResultContent {
    fn from(output: SendMessageToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize send_message output: {e}"))
            .into()
    }
}

pub struct SendMessageTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl SendMessageTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for SendMessageTool {
    type Input = SendMessageToolInput;
    type Output = SendMessageToolOutput;

    const NAME: &'static str = "send_message";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        let target = match &input {
            Ok(input) => Some(input.to.as_str()),
            Err(value) => value.get("to").and_then(|to| to.as_str()),
        };
        match target {
            Some(target) => format!("Message {target}").into(),
            None => "Send message".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| SendMessageToolOutput::Error {
                    error: format!("Failed to receive tool input: {e}"),
                })?;
            let request = ThreadMessageRequest {
                to: input.to,
                message: input.message,
                wait_for_reply: input.wait_for_reply,
                deliver_at: input.deliver_at,
            };
            match self.environment.send_message_to_thread(request, cx).await {
                Ok(outcome) => Ok(SendMessageToolOutput::Success(outcome)),
                Err(error) => Err(SendMessageToolOutput::Error {
                    error: error.to_string(),
                }),
            }
        })
    }
}
