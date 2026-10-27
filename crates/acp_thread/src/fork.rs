//! Fork-owned `AcpThread` API (plan 038): kept out of `acp_thread.rs` so
//! upstream syncs don't conflict on it. Child module of the crate root, so it
//! sees `AcpThread`'s private fields.

use crate::{
    AcpThread, AcpThreadEvent, AgentBoardNotification, AgentThreadEntry, AssistantMessageChunk,
    RunningTurn, ToolCall, ToolCallStatus,
};
use agent_client_protocol::schema::v2 as acp_v2;
use gpui::{App, Context, SharedString, Task};
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
    /// See [`TurnApiError`] for why this distinction matters.
    pub fn had_api_error(&self) -> bool {
        self.api_error.message.is_some()
    }

    /// Text of the last turn-level API error, if any. Only meaningful while
    /// `had_api_error()` is true (cleared at the start of every turn).
    pub fn last_api_error(&self) -> Option<&str> {
        self.api_error.message.as_deref()
    }

    /// Called from `run_turn`'s `Err` branch: the completion request itself
    /// failed (network, timeout, 5xx, rate limit, etc.) — unlike a failed tool
    /// call, this is a real signal about API availability.
    pub(crate) fn record_turn_api_error(&mut self, error: &anyhow::Error) {
        self.api_error.message = Some(format!("{error:#}"));
    }

    /// Toasts and the error banner are transient — without a transcript entry
    /// a turn truncated at the output token limit leaves no trace once they
    /// expire and the session reads as a silent stop.
    pub(crate) fn push_turn_truncated_note(&mut self, cx: &mut Context<Self>) {
        let usage_note = self
            .token_usage
            .as_ref()
            .map(|usage| {
                format!(
                    " — {} output tokens used{}",
                    usage.output_tokens,
                    usage
                        .max_output_tokens
                        .map(|max| format!(" of {max}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_default();
        self.push_agent_board_notification(
            format!(
                "Turn truncated: the model hit its output token limit{}. \
                 Continue the thread to resume where it stopped.",
                usage_note
            ),
            cx,
        );
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

/// Turn-level API error state of an [`AcpThread`], cleared at the start of
/// every turn.
///
/// Narrower than `had_error`: set only when the completion request itself
/// failed (network/stream error from `run_turn`'s `Err` branch), never for an
/// individual failed tool call. `had_error` is set by any tool call failing
/// mid-turn — extremely common in normal agentic work and unrelated to API
/// availability — so code that reasons about actual API exhaustion (e.g.
/// auto_prompt's context-overflow backoff guard) must check this, not
/// `had_error`.
#[derive(Debug, Default)]
pub(crate) struct TurnApiError {
    /// Text of the last turn-level API error, e.g. Claude Code's `Internal
    /// error: You've hit your session limit · resets 1:20am (Asia/Bangkok):
    /// {"errorKind": "rate_limit"}`.
    message: Option<String>,
}

impl AcpThread {
    /// Mirrors a turn that zed-agentd reports still running on the host after
    /// this client reattached mid-turn (plan 037 P4). ACP v1 has no turn-state
    /// update and this client never sent that turn's prompt, so without this
    /// the thread looks idle while it streams and offers no Stop. Cancelling
    /// the adopted turn goes through the normal `session/cancel` path.
    pub fn set_detached_turn_running(&mut self, running: bool, cx: &mut Context<Self>) {
        match running {
            true => {
                if self.running_turn.is_some() {
                    return;
                }
                self.turn_id += 1;
                self.detached_turn = Some(self.turn_id);
                self.running_turn = Some(RunningTurn {
                    id: self.turn_id,
                    first_entry_index: self.entries.len().saturating_sub(1),
                    // Nothing to await: the host owns the turn, and the
                    // cancel path waits on this task.
                    send_task: Task::ready(()),
                });
                self.set_foreground_state(
                    acp_v2::StateUpdate::Running(acp_v2::RunningStateUpdate::new()),
                    cx,
                );
                cx.emit(AcpThreadEvent::StatusChanged);
            }
            false => {
                let Some(turn_id) = self.detached_turn.take() else {
                    return;
                };
                // A cancel or a follow-up prompt has already replaced it.
                if self
                    .running_turn
                    .as_ref()
                    .is_none_or(|turn| turn.id != turn_id)
                {
                    return;
                }
                self.running_turn = None;
                self.flush_streaming_text(cx);
                self.set_foreground_state(
                    acp_v2::StateUpdate::Idle(acp_v2::IdleStateUpdate::new()),
                    cx,
                );
                cx.emit(AcpThreadEvent::StatusChanged);
                cx.emit(AcpThreadEvent::Stopped {
                    activity_generation: self.activity.generation(),
                    activity_duration: self.activity.duration(),
                    stop_reason: None,
                });
            }
        }
    }
}

/// Streaming reveal tick. Each tick drains pending text into the Markdown
/// source, which triggers a background parse and `cx.refresh_windows()` —
/// forcing every window to re-render. At upstream's 16ms (60fps) the repaint
/// churn from a single streaming response can freeze the UI on long outputs.
/// 300ms (~3fps) keeps the foreground thread responsive so it can still pump
/// the incoming HTTP stream.
///
/// `push_assistant_content_block_with_message_id` also skips upstream's per-chunk `EntryUpdated`
/// emit: the text is only buffered, and the reveal task's `markdown.append()`
/// notifies and refreshes windows when it lands. Emitting per chunk would
/// remeasure the list 100+ times/sec during fast streaming.
pub(crate) const STREAMING_TASK_UPDATE_MS: u64 = 300;

/// Equal to [`STREAMING_TASK_UPDATE_MS`] so buffered text drains in a single
/// tick — text arrives from the model in bursts and is flushed every tick.
pub(crate) const STREAMING_REVEAL_TARGET_MS: f32 = 300.0;

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

impl AcpThread {
    pub fn has_tool_calls(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, AgentThreadEntry::ToolCall(..)))
    }

    /// Upstream's `ToolCallUpdate::V2` arm plus failure marking, so
    /// auto_prompt sees failed local tool calls.
    pub(crate) fn upsert_local_tool_call_marking_failure(
        &mut self,
        update: acp_v2::ToolCallUpdate,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let id = update.tool_call_id.clone();
        self.upsert_local_tool_call(update, cx)?;
        if let Some(index) = self.index_for_tool_call(&id) {
            self.mark_error_if_tool_call_failed(index);
        }
        Ok(())
    }

    /// Lets auto_prompt back off instead of cycling when tool calls fail mid-turn.
    pub(super) fn mark_error_if_tool_call_failed(&mut self, index: usize) {
        if let Some(AgentThreadEntry::ToolCall(call)) = self.entries.get(index)
            && matches!(call.status(), ToolCallStatus::Failed)
        {
            self.had_error = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{AcpThreadEvent, AgentConnection as _, StubAgentConnection, ThreadStatus};
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use std::{cell::RefCell, path::Path, rc::Rc};
    use util::{path, path_list::PathList};

    #[gpui::test]
    async fn detached_turn_runs_until_ended_cancelled_or_replaced(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings_store = SettingsStore::test(cx);
            settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
            cx.set_global(settings_store);
        });
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let connection = Rc::new(StubAgentConnection::new());
        let thread = cx
            .update(|cx| {
                connection.clone().new_session(
                    project,
                    PathList::new(&[Path::new(path!("/test"))]),
                    cx,
                )
            })
            .await
            .expect("stub session");
        let stops = Rc::new(RefCell::new(0));
        let _subscription = cx.update(|cx| {
            let stops = stops.clone();
            cx.subscribe(&thread, move |_, event, _| {
                if matches!(event, AcpThreadEvent::Stopped { .. }) {
                    *stops.borrow_mut() += 1;
                }
            })
        });
        let status = |cx: &mut TestAppContext| thread.read_with(cx, |thread, _| thread.status());
        let set = |running: bool, cx: &mut TestAppContext| {
            thread.update(cx, |thread, cx| {
                thread.set_detached_turn_running(running, cx)
            });
        };

        set(true, cx);
        assert_eq!(status(cx), ThreadStatus::Generating);
        set(false, cx);
        assert_eq!(status(cx), ThreadStatus::Idle);
        assert_eq!(*stops.borrow(), 1);
        set(false, cx);
        assert_eq!(*stops.borrow(), 1, "a second end is a no-op");

        set(true, cx);
        thread
            .update(cx, |thread, cx| thread.cancel(cx))
            .await
            .expect("cancel resolves without the host's reply");
        assert_eq!(status(cx), ThreadStatus::Idle);
        assert_eq!(
            connection.take_cancel_calls().len(),
            1,
            "Stop reaches the agent"
        );
        set(false, cx);
        assert_eq!(
            *stops.borrow(),
            1,
            "the late end of a cancelled turn is ignored"
        );

        set(true, cx);
        let reply = connection.defer_next_prompt_response();
        let _follow_up = thread.update(cx, |thread, cx| thread.send(vec!["next".into()], cx));
        cx.run_until_parked();
        set(false, cx);
        assert_eq!(
            status(cx),
            ThreadStatus::Generating,
            "the follow-up turn keeps running after the replaced one ends"
        );
        drop(reply);
    }
}
