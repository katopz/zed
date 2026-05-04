use acp::schema::{ContentBlock, SessionId, StopReason, TextContent};
use agent::ZED_AGENT_ID;
use agent_client_protocol as acp;
use gpui::Window;
use notifications::status_toast::StatusToast;
use prompt_store::{BuiltInPrompt, PromptId, PromptStore};
use std::path::PathBuf;
use ui::prelude::*;

/// Strip the context wrapper produced by `with_first_prompt_context`.
/// For same-thread continuation (ACP agents) the AI already has full
/// context — the wrapper wastes tokens, so we extract just the instruction.
///
/// Handles two formats:
/// 1. New structured: `## User (checkpoint)\n...\n---\nrefer to first thread\n---\n[metadata]\n{instruction}`
/// 2. Legacy block: `refer to first prompt:\n===---===\n...\n===---===\n{instruction}`
fn strip_first_prompt_wrapper(prompt: &str) -> String {
    // New 4-part structured format: find "## 4. Decision" and extract the instruction
    if prompt.starts_with("## 1. First Prompt (original request)") {
        const DECISION_HEADER: &str = "## 4. Decision\n\n";
        if let Some(pos) = prompt.find(DECISION_HEADER) {
            let instruction = &prompt[pos + DECISION_HEADER.len()..];
            let trimmed = instruction.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // Old structured format: "## User (checkpoint)" with "---\nrefer to first thread\n---"
    if prompt.starts_with("## User (checkpoint)") {
        const SEPARATOR: &str = "---\nrefer to first thread\n---\n";
        if let Some(pos) = prompt.find(SEPARATOR) {
            let after = &prompt[pos + SEPARATOR.len()..];
            let result = skip_context_metadata(after);
            if !result.is_empty() {
                return result;
            }
        }
    }

    // Legacy block-delimited format
    const DELIM: &str = "===---===";
    if let Some(rest) = prompt.strip_prefix("refer to first prompt:") {
        let rest = rest.trim_start_matches('\n');
        if let Some(after_open) = rest.strip_prefix(DELIM) {
            let after_open = after_open.trim_start_matches('\n');
            if let Some(end_pos) = after_open.find(DELIM) {
                let tail = after_open[end_pos + DELIM.len()..].trim_start_matches('\n');
                if !tail.is_empty() {
                    return tail.to_string();
                }
            }
        }
    }

    prompt.to_string()
}

/// Skip known metadata sections (Thread summary, Last assistant message)
/// and return the actual instruction prompt.
///
/// The metadata and instruction are separated by a `---` line.
/// We find the last `---` that sits on its own line and return what follows.
fn skip_context_metadata(text: &str) -> String {
    // Find the last `---` separator line — everything after it is the instruction.
    // Scan backwards for a line that is exactly "---".
    let lines: Vec<&str> = text.lines().collect();
    for i in (0..lines.len()).rev() {
        if lines[i].trim() == "---" {
            let instruction = lines[i + 1..].join("\n").trim().to_string();
            if !instruction.is_empty() {
                return instruction;
            }
        }
    }

    // Fallback: no separator found, return trimmed text as-is
    text.trim().to_string()
}

async fn load_auto_prompt_system_prompt(cx: &mut gpui::AsyncWindowContext) -> Option<String> {
    let store_future = cx.update(|_window, cx| PromptStore::global(cx)).ok()?;
    let store = store_future.await.ok()?;
    let task = store.update(cx, |s, cx| {
        s.load(PromptId::BuiltIn(BuiltInPrompt::AutoPromptSystemPrompt), cx)
    });
    task.await.ok()
}

/// Toggle auto-prompt on/off from the agent panel toolbar.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize, gpui::Action)]
#[action(namespace = agent)]
pub struct ToggleAutoPrompt;

/// State of the auto-prompt system.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum AutoPromptState {
    /// Auto-prompt is idle (not processing).
    #[default]
    Idle,
    /// Auto-prompt is waiting for LLM decision or dispatching.
    Processing,
    /// Auto-prompt failed with an error.
    Failed,
}

/// Action dispatched when the external LLM returns a next_prompt.
///
/// Registered in `agent_panel.rs` — creates a new thread with summary link + prompt, auto-submits.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
    gpui::Action,
)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct AutoPromptNewThread {
    /// Session ID of the previous thread (for summary link).
    pub from_session_id: SessionId,
    /// Title of the previous thread.
    pub from_title: Option<String>,
    /// The follow-up prompt text from the external LLM.
    pub next_prompt: String,
    /// Work directories to propagate to the new thread.
    pub work_dirs: Option<Vec<PathBuf>>,
    /// The raw original user message from the very first thread,
    /// carried across chain hops to prevent summary drift.
    #[serde(default)]
    pub original_user_message: Option<String>,
    /// The profile/mode from the previous thread (e.g. "Auto", "Sonnet", "High"),
    /// carried across chain hops to preserve the user's selection.
    #[serde(default)]
    pub profile_id: Option<String>,
}

fn dispatch_action(
    action: auto_prompt::AutoPromptAction,
    conversation_view: &crate::ConversationView,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    let is_native_agent = conversation_view
        .active_thread()
        .is_some_and(|tv| tv.read(cx).thread.read(cx).connection().agent_id() == *ZED_AGENT_ID);

    if !is_native_agent {
        if let Some(active_tv) = conversation_view.active_thread() {
            // Strip the "refer to first prompt" wrapper — same-thread AI
            // already has full context, the preamble just wastes tokens.
            let prompt = strip_first_prompt_wrapper(&action.next_prompt);
            active_tv.update(cx, |tv, cx| {
                tv.message_editor.update(cx, |editor, cx| {
                    editor.set_message(
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                        window,
                        cx,
                    );
                });
                tv.send(window, cx);
            });
            log::info!(
                "[auto_prompt] dispatch_action: sent continuation to same thread (ACP agent)"
            );
            return;
        }
        log::warn!(
            "[auto_prompt] dispatch_action: no active thread for ACP agent, falling back to new thread"
        );
    }

    log::info!(
        "[auto_prompt] dispatch_action: dispatching AutoPromptNewThread (prompt {} chars)",
        action.next_prompt.len()
    );

    let action = Box::new(AutoPromptNewThread {
        from_session_id: action.from_session_id,
        from_title: action.from_title,
        next_prompt: action.next_prompt,
        work_dirs: action.work_dirs,
        original_user_message: action.original_user_message,
        profile_id: action.profile_id,
    });

    window.dispatch_action(action, cx);
    log::info!("[auto_prompt] dispatch_action: action dispatched");
}

fn is_cancelled(
    thread_view: &gpui::WeakEntity<crate::conversation_view::ThreadView>,
    cx: &gpui::AsyncWindowContext,
) -> bool {
    thread_view
        .read_with(cx, |tv, _| {
            !matches!(tv.auto_prompt_state, AutoPromptState::Processing)
        })
        .unwrap_or(true)
}

/// Entry point — called from `ConversationView::handle_thread_event`
/// when `AcpThreadEvent::Stopped` fires.
///
/// Delegates decision logic to the `auto_prompt` crate and handles
/// GPUI action dispatch for the results.
///
/// Returns the spawned `Task` for `DispatchAfterDelay` and `NeedsLlmCall`
/// variants so the caller can store it in `ThreadView._auto_prompt_task`
/// for cancellation support.
pub fn on_thread_stopped(
    conversation_view: &crate::ConversationView,
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &StopReason,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) -> Option<gpui::Task<()>> {
    log::warn!(
        "[auto_prompt] *** ENTRY POINT *** on_thread_stopped called: used_tools={}, stop_reason={:?}",
        used_tools,
        stop_reason
    );

    if matches!(stop_reason, StopReason::MaxTokens) {
        log::warn!(
            "[auto_prompt] Error/Rate Limit detected - stop_reason={:?}, will apply backoff retry",
            stop_reason
        );
    }
    let decision = auto_prompt::decide(thread, used_tools, stop_reason, cx);
    log::info!("[auto_prompt] decision result: {:?}", decision);

    let mut profile_id = conversation_view
        .active_thread()
        .and_then(|tv| tv.read(cx).current_mode_id(cx))
        .map(|id| id.to_string());
    log::info!("[auto_prompt] captured profile_id: {:?}", profile_id);

    match decision {
        auto_prompt::AutoPromptDecision::NoAction => {
            log::info!("[auto_prompt] NoAction - taking no action");
            None
        }

        auto_prompt::AutoPromptDecision::DispatchNow(mut action) => {
            action.profile_id = profile_id.take();
            log::info!(
                "[auto_prompt] DispatchNow - dispatching action with prompt: {}",
                action.next_prompt
            );
            dispatch_action(action, conversation_view, window, cx);
            None
        }

        auto_prompt::AutoPromptDecision::DispatchAfterDelay {
            mut action,
            delay_ms,
        } => {
            action.profile_id = profile_id.take();
            log::info!(
                "[auto_prompt] DispatchAfterDelay - scheduling action in {}ms with prompt: {}",
                delay_ms,
                action.next_prompt
            );

            let task = cx.spawn_in(window, async move |_view, cx| {
                let thread_weak = _view
                    .update_in(cx, |cv, _window, cx| {
                        cv.active_thread().map(|tv| {
                            tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Processing;
                                cx.notify();
                            });
                            tv.downgrade()
                        })
                    })
                    .unwrap_or_else(|err| {
                        log::warn!("[auto_prompt] failed to get active thread (view may have been dropped): {err}");
                        None
                    });

                cx.background_executor()
                    .timer(std::time::Duration::from_millis(delay_ms))
                    .await;

                if let Some(ref tv) = thread_weak {
                    if is_cancelled(tv, cx) {
                        log::info!("[auto_prompt] Cancelled during delay, aborting dispatch");
                        return;
                    }
                }

                if let Some(ref tv) = thread_weak {
                    if let Err(err) = tv.update(cx, |tv, cx| {
                        tv.auto_prompt_state = AutoPromptState::Idle;
                        cx.notify();
                    }) {
                        log::warn!("[auto_prompt] failed to reset state after delay: {err}");
                    }
                }

                match _view.update_in(cx, |_view, window, cx| {
                    dispatch_action(action, _view, window, cx);
                }) {
                    Ok(()) => {
                        log::info!("[auto_prompt] DispatchAfterDelay dispatch submitted");
                    }
                    Err(err) => {
                        log::warn!(
                            "[auto_prompt] FAILED to dispatch after delay (view may have been dropped): {err}"
                        );
                    }
                }
            });

            Some(task)
        }

        auto_prompt::AutoPromptDecision::NeedsLlmCall(mut data) => {
            data.profile_id = profile_id.take();
            log::info!(
                "[auto_prompt] NeedsLlmCall - spawning task to call LLM with model: {:?}",
                data.model.id()
            );

            let task = cx.spawn_in(window, async move |_view, cx| {
                log::info!("[auto_prompt] ASYNC TASK: starting LLM call");

                let thread_weak = _view
                    .update_in(cx, |cv, _window, cx| {
                        cv.active_thread().map(|tv| {
                            tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Processing;
                                cx.notify();
                            });
                            tv.downgrade()
                        })
                    })
                    .unwrap_or_else(|err| {
                        log::warn!("[auto_prompt] failed to get active thread (view may have been dropped): {err}");
                        None
                    });

                let workspace_weak = _view
                    .update_in(cx, |cv, _window, cx| {
                        cv.active_thread().map(|tv| tv.read(cx).workspace.clone())
                    })
                    .unwrap_or_else(|err| {
                        log::warn!("[auto_prompt] failed to get workspace: {err}");
                        None
                    });

                let config = auto_prompt::load_config_cached().unwrap_or_default();

                let store_prompt = load_auto_prompt_system_prompt(cx).await;

                let mut data = data;
                match config.system_prompt.as_ref() {
                    Some(prompt) => data.system_prompt = prompt.clone(),
                    None => {
                        if let Some(store_prompt) = store_prompt {
                            data.system_prompt = store_prompt;
                        }
                    }
                }

                let mut result = auto_prompt::decide_with_llm(data.clone(), cx).await;

                // Retry loop with exponential backoff
                while let Err(ref err) = result {
                    let failure_count = auto_prompt::increment_llm_failure_count();

                    if failure_count > config.max_llm_retries {
                        break; // Max retries exhausted
                    }

                    let delay = config.backoff_delay_ms(failure_count);
                    log::warn!(
                        "[auto_prompt] LLM call failed (attempt {}/{}): {err}, retrying in {}ms",
                        failure_count,
                        config.max_llm_retries,
                        delay
                    );

                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(delay))
                        .await;

                    if let Some(ref tv) = thread_weak {
                        if is_cancelled(tv, cx) {
                            log::info!("[auto_prompt] Cancelled during retry delay");
                            return;
                        }
                    }

                    log::info!("[auto_prompt] Retrying LLM call (attempt {})", failure_count);
                    result = auto_prompt::decide_with_llm(data.clone(), cx).await;
                }

                if let Some(ref tv) = thread_weak {
                    if is_cancelled(tv, cx) {
                        log::info!("[auto_prompt] Cancelled during LLM call, discarding result");
                        return;
                    }
                }

                log::info!("[auto_prompt] ASYNC TASK: LLM call completed");

                match result {
                    Ok(auto_prompt::AutoPromptOutcome::Continue(action)) => {
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Idle;
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to reset state before dispatch: {err}");
                            }
                        }

                        log::info!(
                            "[auto_prompt] LLM returned action - dispatching with prompt: {}",
                            action.next_prompt
                        );
                        match _view.update_in(cx, |_view, window, cx| {
                            dispatch_action(action, _view, window, cx);
                        }) {
                            Ok(()) => {
                                log::info!("[auto_prompt] NeedsLlmCall dispatch submitted");
                            }
                            Err(err) => {
                                log::warn!(
                                    "[auto_prompt] FAILED to dispatch new thread (view may have been dropped): {err}"
                                );
                            }
                        }
                    }
                    Ok(auto_prompt::AutoPromptOutcome::Stopped { reason }) => {
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Idle;
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to reset state on stop: {err}");
                            }
                        }
                        log::info!("[auto_prompt] Chain stopped: {reason}");

                        if let Some(ref workspace) = workspace_weak {
                            let reason_for_toast = reason.clone();
                            let _ = workspace.update(cx, |workspace, cx| {
                                let status_toast = StatusToast::new(
                                    format!("Auto-prompt stopped: {reason_for_toast}"),
                                    cx,
                                    |this, _| {
                                        this.icon(
                                            Icon::new(IconName::Check)
                                                .size(IconSize::Small)
                                                .color(Color::Muted),
                                        )
                                        .auto_dismiss(true)
                                        .dismiss_button(true)
                                    },
                                );
                                workspace.toggle_status_toast(status_toast, cx);
                            });
                        }
                    }

                    Err(err) => {
                        // Max retries exhausted (already tried in the loop above)
                        if let Some(ref tv) = thread_weak {
                            if let Err(update_err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Failed;
                                tv._auto_prompt_retry_data = Some(data.clone());
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to set Failed state: {update_err}");
                            }
                        }
                        log::warn!(
                            "[auto_prompt] LLM call failed after {} attempts: {err}",
                            config.max_llm_retries
                        );
                    }
                }
            });

            Some(task)
        }
    }
}
