//! Fork-owned `AcpThread` API (plan 038): kept out of `acp_thread.rs` so
//! upstream syncs don't conflict on it. Child module of the crate root, so it
//! sees `AcpThread`'s private fields.

use crate::{
    AcpThread, AcpThreadEvent, AgentBoardNotification, AgentThreadEntry, AssistantMessageChunk,
    ToolCall, ToolCallStatus,
};
use agent_client_protocol::schema::v2 as acp_v2;
use gpui::{App, Context, SharedString};
use itertools::Itertools as _;

impl AcpThread {
    /// Returns the text content of the last assistant message, if any.
    /// Used to extract a subagent's final output after a retry.
    pub fn last_assistant_message_text(&self, cx: &App) -> Option<String> {
        for entry in self.entries.iter().rev() {
            if let AgentThreadEntry::AssistantMessage(message) = entry {
                let text: String = message
                    .chunks
                    .iter()
                    .filter_map(|chunk| match chunk {
                        AssistantMessageChunk::Message { block, .. } => Some(block.markdowns()),
                        AssistantMessageChunk::Thought { .. } => None,
                    })
                    .flatten()
                    .filter_map(|markdown| {
                        let source = markdown.read(cx).source().to_string();
                        (!source.is_empty()).then_some(source)
                    })
                    .join("\n\n");
                return (!text.is_empty()).then_some(text);
            }
        }
        None
    }

    /// Estimated character footprint of each sizable entry (tool calls and
    /// assistant messages), in thread order. Tool calls measure their
    /// raw_input/raw_output JSON; assistant messages measure their markdown
    /// sources. Used by auto_prompt's bloat detection to name the largest
    /// space consumers in a thread that died at the output token limit.
    pub fn bloat_candidates(&self, cx: &App) -> Vec<(String, usize)> {
        let mut candidates: Vec<(String, usize)> = Vec::new();
        let json_len = |value: Option<&serde_json::Value>| {
            value.map(|value| value.to_string().len()).unwrap_or(0)
        };
        for entry in &self.entries {
            match entry {
                AgentThreadEntry::ToolCall(tool_call) => {
                    let chars = json_len(tool_call.raw_input.as_ref())
                        + json_len(tool_call.raw_output.as_ref());
                    if chars > 0 {
                        candidates.push((tool_call_bloat_label(tool_call), chars));
                    }
                }
                AgentThreadEntry::AssistantMessage(message) => {
                    let chars: usize = message
                        .chunks
                        .iter()
                        .flat_map(|chunk| match chunk {
                            AssistantMessageChunk::Message { block, .. }
                            | AssistantMessageChunk::Thought { block, .. } => block.markdowns(),
                        })
                        .map(|markdown| markdown.read(cx).source().len())
                        .sum();
                    if chars > 0 {
                        candidates.push(("assistant message".to_string(), chars));
                    }
                }
                _ => {}
            }
        }
        candidates
    }

    /// Returns the current turn ID, which increments each time a new turn starts
    /// (via `send`, `retry`, or `resume`). Useful for detecting whether a turn
    /// was replaced by a newer one (e.g. a retry).
    pub fn turn_id(&self) -> u32 {
        self.turn_id
    }

    /// True only when the completion request itself failed (a genuine
    /// stream/network/API error), never for an individual failed tool call.
    /// See the field doc on `had_api_error` for why this distinction matters.
    pub fn had_api_error(&self) -> bool {
        self.had_api_error
    }

    /// Text of the last turn-level API error, if any. Only meaningful while
    /// `had_api_error()` is true (cleared at the start of every turn).
    pub fn last_api_error(&self) -> Option<&str> {
        self.last_api_error.as_deref()
    }

    /// Push an agent-board notification into the thread. Called by `agent_ui`
    /// when new peer-agent states arrive via the board poll loop. The
    /// notification is display-only — it never reaches the model backend.
    pub fn push_agent_board_notification(
        &mut self,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.push_entry(
            AgentThreadEntry::AgentBoardNotification(AgentBoardNotification { text: text.into() }),
            cx,
        );
    }

    fn subagent_tool_call_mut(
        &mut self,
        subagent_session_id: &acp_v2::SessionId,
    ) -> Option<(usize, &mut ToolCall)> {
        self.entries
            .iter_mut()
            .enumerate()
            .find_map(|(index, entry)| match entry {
                AgentThreadEntry::ToolCall(tool_call)
                    if tool_call
                        .subagent_session_info
                        .as_ref()
                        .is_some_and(|info| &info.session_id == subagent_session_id) =>
                {
                    Some((index, tool_call))
                }
                _ => None,
            })
    }

    pub fn restart_subagent_tool_call(
        &mut self,
        subagent_session_id: &acp_v2::SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some((ix, tool_call)) = self.subagent_tool_call_mut(subagent_session_id) else {
            return;
        };
        tool_call.set_local_status(ToolCallStatus::InProgress);
        cx.emit(AcpThreadEvent::EntryUpdated(ix));
    }

    /// Transitions a subagent's tool call to a terminal state (`Completed` or
    /// `Failed`) and replaces its rendered content with the given output.
    ///
    /// This is used when a subagent is retried *after* its initial turn has
    /// already completed. In that case, the original `send` future has already
    /// returned, so the normal `process_tool_result` path (which updates the
    /// parent's tool call via the event stream) does not fire for the retry.
    /// Without this call, the parent's tool call remains `InProgress` forever
    /// (loading indicator stuck) and the parent LLM never sees the new result.
    pub fn complete_subagent_tool_call(
        &mut self,
        subagent_session_id: &acp_v2::SessionId,
        output: Option<String>,
        is_error: bool,
        cx: &mut Context<Self>,
    ) {
        let language_registry = self.project.read(cx).languages().clone();
        let Some((ix, tool_call)) = self.subagent_tool_call_mut(subagent_session_id) else {
            return;
        };

        tool_call.set_local_status(if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        });

        if let Some(output) = output {
            tool_call.raw_output = Some(serde_json::Value::String(output));
            // Raw output is only rendered when there is no structured content.
            tool_call.structured_content.clear();
            tool_call.update_raw_output_content(&language_registry, cx);
        }

        cx.emit(AcpThreadEvent::EntryUpdated(ix));
    }
}

/// Short human-readable label naming a tool call in the bloat report: the
/// tool name plus, when present, the first command/path argument so the
/// report can say "avoid re-running terminal: cargo check …" instead of a
/// bare tool name.
fn tool_call_bloat_label(tool_call: &ToolCall) -> String {
    const MAX_HINT_CHARS: usize = 48;
    let name = tool_call.tool_name.as_deref().unwrap_or("tool call");
    let Some(raw_input) = tool_call.raw_input.as_ref() else {
        return name.to_string();
    };
    let hint = ["command", "file_path", "path"]
        .iter()
        .find_map(|key| raw_input.get(key).and_then(|value| value.as_str()));
    let Some(hint) = hint else {
        return name.to_string();
    };
    let hint = hint.trim();
    let first_line = hint.split_once('\n').map_or(hint, |(line, _)| line);
    if first_line.chars().count() <= MAX_HINT_CHARS {
        format!("{name}: {first_line}")
    } else {
        let truncated: String = first_line.chars().take(MAX_HINT_CHARS).collect();
        format!("{name}: {truncated}…")
    }
}
