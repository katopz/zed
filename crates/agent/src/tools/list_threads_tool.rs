use agent_client_protocol::schema::v2 as acp;
use anyhow::Result;
use gpui::{App, SharedString, Task};
use language_model::LanguageModelToolResultContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use std::sync::Arc;

use crate::{AgentTool, ThreadEnvironment, ThreadListing, ToolCallEventStream, ToolInput};

/// List the live agent threads in this project that you can message with `send_message`.
///
/// Includes your own thread (`kind: "self"`), subagents spawned with `spawn_agent`, and
/// top-level threads (created by the user or via `create_thread`). `status` tells you
/// whether a message would start a turn now (`idle`) or wait for the current turn to end
/// (`generating`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct ListThreadsToolInput {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ListThreadsToolOutput {
    Success { threads: Vec<ThreadListing> },
    Error { error: String },
}

impl From<ListThreadsToolOutput> for LanguageModelToolResultContent {
    fn from(output: ListThreadsToolOutput) -> Self {
        serde_json::to_string(&output)
            .unwrap_or_else(|e| format!("Failed to serialize list_threads output: {e}"))
            .into()
    }
}

pub struct ListThreadsTool {
    environment: Rc<dyn ThreadEnvironment>,
}

impl ListThreadsTool {
    pub fn new(environment: Rc<dyn ThreadEnvironment>) -> Self {
        Self { environment }
    }
}

impl AgentTool for ListThreadsTool {
    type Input = ListThreadsToolInput;
    type Output = ListThreadsToolOutput;

    const NAME: &'static str = "list_threads";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        _input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        "List threads".into()
    }

    fn run(
        self: Arc<Self>,
        _input: ToolInput<Self::Input>,
        _event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        // The tool runs while the calling `Thread` is still leased by its own
        // `update`, so `list_threads` would re-read the leased entity and
        // panic. Defer to a later executor tick, after the lease is released.
        let environment = self.environment.clone();
        cx.spawn(
            async move |cx| match cx.update(|cx| environment.list_threads(cx)) {
                Ok(threads) => Ok(ListThreadsToolOutput::Success { threads }),
                Err(error) => Err(ListThreadsToolOutput::Error {
                    error: error.to_string(),
                }),
            },
        )
    }
}
