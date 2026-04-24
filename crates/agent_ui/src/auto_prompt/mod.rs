use acp::schema::{SessionId, StopReason};
use agent_client_protocol as acp;
use gpui::Window;
use std::path::PathBuf;

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
}

fn dispatch_action(
    action: auto_prompt::AutoPromptAction,
    conversation_view: &crate::ConversationView,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    log::info!(
        "[auto_prompt] dispatch_action: dispatching AutoPromptNewThread (prompt {} chars)",
        action.next_prompt.len()
    );

    let workspace = conversation_view.workspace().clone();
    let action = Box::new(AutoPromptNewThread {
        from_session_id: action.from_session_id,
        from_title: action.from_title,
        next_prompt: action.next_prompt,
        work_dirs: action.work_dirs,
    });

    if let Some(workspace) = workspace.upgrade() {
        workspace.update(cx, |_workspace, cx| {
            window.dispatch_action(action, cx);
        });
        log::info!("[auto_prompt] dispatch_action: action dispatched via workspace");
    } else {
        log::error!("[auto_prompt] dispatch_action: workspace is gone, cannot dispatch");
    }
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

    match decision {
        auto_prompt::AutoPromptDecision::NoAction => {
            log::info!("[auto_prompt] NoAction - taking no action");
            None
        }

        auto_prompt::AutoPromptDecision::DispatchNow(action) => {
            log::info!(
                "[auto_prompt] DispatchNow - dispatching action with prompt: {}",
                action.next_prompt
            );
            dispatch_action(action, conversation_view, window, cx);
            None
        }

        auto_prompt::AutoPromptDecision::DispatchAfterDelay { action, delay_ms } => {
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

        auto_prompt::AutoPromptDecision::NeedsLlmCall(data) => {
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

                let config = auto_prompt::load_config_cached().unwrap_or_default();
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
                    Ok(Some(action)) => {
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
                    Ok(None) => {
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Idle;
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to reset state on no-action: {err}");
                            }
                        }
                        log::info!("[auto_prompt] LLM returned no action (normal stop)");
                    }
                    Err(err) => {
                        // Max retries exhausted (already tried in the loop above)
                        if let Some(ref tv) = thread_weak {
                            if let Err(update_err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Failed;
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
