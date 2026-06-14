use acp_thread::MentionUri;
use agent::ZED_AGENT_ID;
use agent_client_protocol::schema as acp;
use gpui::Window;
use notifications::status_toast::StatusToast;
use prompt_store::{BuiltInPrompt, PromptId, PromptStore};
use std::path::PathBuf;
use ui::prelude::*;
use workspace::PathList;

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

    // Current 3-part format: "## 1. Thread Summary" → "## 3. Decision"
    if prompt.starts_with("## 1. Thread Summary") {
        const DECISION_HEADER: &str = "## 3. Decision\n\n";
        if let Some(pos) = prompt.find(DECISION_HEADER) {
            let instruction = &prompt[pos + DECISION_HEADER.len()..];
            let trimmed = instruction.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    // Fallback 2-part format (summary failed): "## 1. Last Assistant Message" → "## 2. Decision"
    if prompt.starts_with("## 1. Last Assistant Message") {
        const DECISION_HEADER: &str = "## 2. Decision\n\n";
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

async fn load_auto_prompt_system_prompt(
    cx: &mut gpui::AsyncWindowContext,
) -> Option<(String, bool)> {
    let builtin = BuiltInPrompt::AutoPromptSystemPrompt;
    let default_version = builtin.default_version();
    let default_content = builtin.default_content().to_string();

    // Check global AUTO_PROMPT.md file first
    let global_auto_prompt_path = paths::auto_prompt_file();
    if global_auto_prompt_path.exists() {
        match std::fs::read_to_string(global_auto_prompt_path) {
            Ok(content) => {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    let file_version = prompt_store::parse_prompt_version(trimmed);
                    if file_version < default_version {
                        log::warn!(
                            "[auto_prompt] Global AUTO_PROMPT.md is outdated (file=v{file_version}, default=v{default_version}), using default"
                        );
                        return Some((default_content, true));
                    }
                    log::info!("[auto_prompt] Using global AUTO_PROMPT.md (v{file_version})");
                    return Some((trimmed.to_string(), false));
                }
            }
            Err(err) => {
                log::warn!(
                    "[auto_prompt] Failed to read global AUTO_PROMPT.md: {err}, falling back to PromptStore"
                );
            }
        }
    }

    // Fall back to PromptStore
    let store_future = cx.update(|_window, cx| PromptStore::global(cx)).ok()?;
    let store = store_future.await.ok()?;

    let stored_prompt = store
        .update(cx, |s, cx| s.load(PromptId::BuiltIn(builtin), cx))
        .await
        .ok()?;

    let stored_version = prompt_store::parse_prompt_version(&stored_prompt);

    if stored_version < default_version {
        log::warn!(
            "[auto_prompt] Stored system prompt is outdated (stored=v{stored_version}, default=v{default_version}), using default and resetting stored prompt"
        );
        Some((default_content, true))
    } else {
        Some((stored_prompt, false))
    }
}

/// Toggle auto-prompt on/off from the agent panel toolbar.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize, gpui::Action)]
#[action(namespace = agent)]
pub struct ToggleAutoPrompt;

/// State of the auto-prompt system.
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum AutoPromptState {
    /// Auto-prompt is idle (not processing).
    #[default]
    Idle,
    /// Auto-prompt is waiting for LLM decision or dispatching.
    Processing,
    /// Auto-prompt failed with an error. Contains the error message for display.
    Failed(String),
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
    pub from_session_id: acp::SessionId,
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
    /// The last assistant message from the previous thread, used to build
    /// the follow-up section after the LLM-generated summary.
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    /// The decision/continue prompt for the new thread, used to build
    /// the follow-up section after the LLM-generated summary.
    #[serde(default)]
    pub decision_prompt: Option<String>,
}

/// Build a same-thread continuation prompt for ACP agents (e.g. Claude) that
/// support the `/compact` command to summarize conversation history.
///
/// Format: `/compact` + newline + decision.
///
/// For same-thread continuations the last assistant message is already visible in the
/// thread history, so we only include the decision prompt — not a verbatim repeat.
fn build_compact_prompt(_last_assistant_message: Option<&str>, decision: &str) -> String {
    let mut parts = vec!["/compact".to_string()];

    if !decision.trim().is_empty() {
        parts.push(String::new());
        parts.push(decision.trim().to_string());
    }

    parts.join("\n")
}

/// Build a same-thread continuation prompt for the native Zed agent which does not
/// support `/compact` as a slash command. Uses plain-text instructions instead.
///
/// For same-thread continuations the last assistant message is already visible in the
/// thread history, so we only include the decision prompt — not a verbatim repeat.
fn build_native_continuation_prompt(
    _last_assistant_message: Option<&str>,
    decision: &str,
) -> String {
    let preamble = "Continue from where we left off. Summarize prior context internally and proceed.\n\
         Review your progress and continue any remaining work in the current repo or context first.\n\
         If everything is complete, commit all changes with conventional commit messages.";
    let mut parts = vec![preamble.to_string()];

    let trimmed = decision.trim();
    // Skip appending the decision when it is just a generic restatement of the preamble
    // (e.g. "Continue from where we left off." from manual_auto_prompt).
    let is_generic_continuation = trimmed
        .strip_prefix("Continue from where we left off")
        .map_or(false, |rest| rest.trim().trim_end_matches('.').is_empty());

    if !trimmed.is_empty() && !is_generic_continuation {
        parts.push(String::new());
        parts.push(trimmed.to_string());
    }

    parts.join("\n")
}

pub(crate) fn dispatch_action(
    action: auto_prompt::AutoPromptAction,
    conversation_view: &crate::ConversationView,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    let is_native_agent = conversation_view
        .active_thread()
        .is_some_and(|tv| tv.read(cx).thread.read(cx).connection().agent_id() == *ZED_AGENT_ID);

    let same_thread_threshold = match auto_prompt::load_config_cached()
        .map(|config| config.same_thread_token_threshold)
        .unwrap_or(0)
    {
        // Explicit positive override from config/env.
        threshold if threshold > 0 => threshold,
        // Auto: 50% of the active model's max input tokens, capped at 100k.
        // 60k fallback applies only before the first usage report populates token_usage.
        _ => {
            let max_input: usize = conversation_view
                .active_thread()
                .and_then(|tv| {
                    tv.read(cx)
                        .thread
                        .read(cx)
                        .token_usage()
                        .and_then(|usage| {
                            let max_input = usage
                                .max_tokens
                                .saturating_sub(usage.max_output_tokens.unwrap_or_default());
                            (max_input > 0).then(|| max_input as usize)
                        })
                })
                .unwrap_or(60_000);
            (max_input / 2).clamp(1, 100_000)
        }
    };

    // Use actual API-reported tokens when available; fall back to the
    // chars/4 estimate. Without the fallback, models that don't report
    // usage (actual_input_tokens=None) would always stay in the same
    // thread, creating an infinite ContextOverflow loop.
    let effective_tokens = action
        .actual_input_tokens
        .map(|t| t as usize)
        .unwrap_or(action.approximate_token_count);
    let exceeds_same_thread = effective_tokens >= same_thread_threshold;

    let use_new_thread = action.force_new_thread || (is_native_agent && exceeds_same_thread);

    log::info!(
        "[auto_prompt] dispatch_action: is_native_agent={}, actual={:?}, approx={}, effective={effective_tokens}, threshold={same_thread_threshold}, use_new_thread={use_new_thread}, force_new_thread={}",
        is_native_agent,
        action.actual_input_tokens,
        action.approximate_token_count,
        action.force_new_thread
    );

    // Native agent with high token count must use new thread (no /compact support).
    // ACP agents (e.g. Claude) always use same-thread /compact.
    if !use_new_thread {
        if let Some(active_tv) = conversation_view.active_thread() {
            let decision = strip_first_prompt_wrapper(&action.next_prompt);
            let prompt = if is_native_agent {
                build_native_continuation_prompt(
                    action.last_assistant_message.as_deref(),
                    &decision,
                )
            } else {
                build_compact_prompt(action.last_assistant_message.as_deref(), &decision)
            };
            active_tv.update(cx, |tv, cx| {
                tv.message_editor.update(cx, |editor, cx| {
                    editor.set_message(
                        vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                        window,
                        cx,
                    );
                });
                tv.send(window, cx);
            });
            log::info!(
                "[auto_prompt] dispatch_action: sent {} continuation to same thread (tokens={:?})",
                if is_native_agent {
                    "native"
                } else {
                    "/compact"
                },
                action.actual_input_tokens
            );
            return;
        }
        // ACP agents (Claude, etc.) must NEVER create new threads — they rely on
        // conversation history in the same thread. If the active thread is gone,
        // stop instead of falling through to the new-thread path.
        if !is_native_agent {
            log::warn!(
                "[auto_prompt] dispatch_action: no active thread for ACP agent continuation, stopping (ACP agents cannot use new threads)"
            );
            return;
        }
        log::warn!(
            "[auto_prompt] dispatch_action: no active thread for native agent continuation, falling back to new thread"
        );
    }

    log::info!(
        "[auto_prompt] dispatch_action: creating new thread directly (prompt {} chars, tokens={:?})",
        action.next_prompt.len(),
        action.actual_input_tokens
    );

    let decision_prompt = auto_prompt::extract_decision_prompt(&action.next_prompt);

    // Create the new thread directly via AgentPanel instead of dispatching
    // a GPUI action. window.dispatch_action is unreliable when the user is
    // idle (no focused element in the Workspace focus chain) — the action
    // reaches the App-level listener but never the Workspace handler.
    //
    // We must defer the workspace update to avoid a recursive entity
    // update panic. dispatch_action is called inside
    // conversation_view.update(cx, ...), and external_thread creates a new
    // ConversationView whose observers may re-enter the one still on the
    // update stack.
    let workspace_handle = conversation_view.workspace();
    let Some(workspace) = workspace_handle.upgrade() else {
        log::warn!("[auto_prompt] dispatch_action: workspace dropped, cannot create new thread");
        return;
    };

    window.defer(cx, move |window, cx| {
        let _ = workspace.update(cx, |workspace, cx| {
            workspace.focus_panel::<crate::AgentPanel>(window, cx);

            let Some(panel) = workspace.panel::<crate::AgentPanel>(cx) else {
                log::warn!("[auto_prompt] dispatch_action: AgentPanel not found in workspace");
                return;
            };

            let work_dirs = action.work_dirs.clone().map(|dirs| PathList::new(&dirs));

            let from_session_id = action.from_session_id.clone();
            let from_title = action.from_title.clone();

            let initial_content = if action.last_assistant_message.is_some()
                || decision_prompt.is_some()
            {
                let follow_up = crate::AgentPanel::build_auto_prompt_follow_up(
                    action.last_assistant_message.as_deref(),
                    decision_prompt.as_deref(),
                );

                log::info!(
                    "[auto_prompt] dispatch_action: using ThreadSummary with follow_up ({} chars)",
                    follow_up.as_ref().map_or(0, |s| s.len())
                );

                crate::AgentInitialContent::ThreadSummary {
                    session_id: from_session_id,
                    title: from_title.map(gpui::SharedString::from),
                    follow_up,
                    auto_submit: true,
                }
            } else {
                let next_prompt = action.next_prompt.clone();

                let raw_title = from_title.as_deref().unwrap_or("Thread");
                let mut clean_title = raw_title.to_string();
                while let Some(rest) = clean_title.strip_prefix("[@") {
                    if let Some(end) = rest.find("](zed:///agent/thread/") {
                        clean_title = rest[..end].to_string();
                    } else {
                        break;
                    }
                }

                let mention_uri = MentionUri::Thread {
                    id: from_session_id,
                    name: clean_title,
                };
                let summary_link = format!("{}\n\n", mention_uri.as_link());
                let full_prompt = format!("{summary_link}{next_prompt}");

                let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(full_prompt))];

                crate::AgentInitialContent::ContentBlock {
                    blocks,
                    auto_submit: true,
                    auto_prompt_enabled: true,
                    profile_id: action.profile_id.clone(),
                }
            };

            panel.update(cx, |panel, cx| {
                panel.external_thread(
                    None,
                    None,
                    work_dirs,
                    action.from_title.clone().map(Into::into),
                    Some(initial_content),
                    true,
                    crate::AgentThreadSource::AgentPanel,
                    window,
                    cx,
                );
            });
        });
    });

    log::info!("[auto_prompt] dispatch_action: new thread creation deferred");
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
    stop_reason: &acp::StopReason,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) -> Option<gpui::Task<()>> {
    log::warn!(
        "[auto_prompt] *** ENTRY POINT *** on_thread_stopped called: used_tools={}, stop_reason={:?}",
        used_tools,
        stop_reason
    );

    if matches!(stop_reason, acp::StopReason::MaxTokens) {
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
            data.supports_compact = thread.read(cx).connection().agent_id() != *ZED_AGENT_ID;
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

                let store_prompt_result = load_auto_prompt_system_prompt(cx).await;

                let mut data = data;
                match config.system_prompt.as_ref() {
                    Some(prompt) => data.system_prompt = prompt.clone(),
                    None => {
                        // Check project AUTO_PROMPT.md first (project overrides global)
                        let project_auto_prompt = data.work_dirs.as_ref().and_then(|dirs| {
                            for dir in dirs {
                                let path = dir.join("AUTO_PROMPT.md");
                                if path.exists() {
                                    if let Ok(content) = std::fs::read_to_string(&path) {
                                        let trimmed = content.trim();
                                        if !trimmed.is_empty() {
                                            return Some(trimmed.to_string());
                                        }
                                    }
                                }
                            }
                            None
                        });

                        if let Some(project_prompt) = project_auto_prompt {
                            data.system_prompt = project_prompt;
                        } else if let Some((store_prompt, is_outdated)) = store_prompt_result {
                            data.system_prompt = store_prompt;
                            if is_outdated {
                                if let Some(ref workspace) = workspace_weak {
                                    let _ = workspace.update(cx, |workspace, cx| {
                                        let toast = notifications::status_toast::StatusToast::new(
                                            gpui::SharedString::from("Auto-prompt system prompt updated to a newer version. You can customize it via AUTO_PROMPT.md."),
                                            cx,
                                            |this, _| {
                                                this.icon(ui::Icon::new(ui::IconName::Info)
                                                    .color(ui::Color::Muted))
                                                    .auto_dismiss(true)
                                                    .dismiss_button(true)
                                            },
                                        );
                                        workspace.toggle_status_toast(toast, cx);
                                    });
                                }
                            }
                        }
                    }
                }

                // When the source thread had an error (rate limit, refusal, max tokens, etc.),
                // add a pre-call delay to avoid immediately hitting the same rate-limited API.
                if data.had_error {
                    let pre_call_delay = config.backoff_delay_ms(1);
                    log::info!(
                        "[auto_prompt] Source thread had error, waiting {pre_call_delay}ms before orchestration LLM call"
                    );
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(pre_call_delay))
                        .await;

                    if let Some(ref tv) = thread_weak {
                        if is_cancelled(tv, cx) {
                            log::info!("[auto_prompt] Cancelled during pre-call delay");
                            return;
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
                        auto_prompt::reset_llm_failure_count();
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                // Clear the task BEFORE dispatch so send_content() doesn't
                                // see a stale task and reset iteration/verification counters.
                                tv._auto_prompt_task = None;
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
                    Ok(auto_prompt::AutoPromptOutcome::ContextOverflow(action)) => {
                        auto_prompt::reset_llm_failure_count();
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                tv._auto_prompt_task = None;
                                tv.auto_prompt_state = AutoPromptState::Idle;
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to reset state before context overflow dispatch: {err}");
                            }
                        }

                        // ContextOverflow Phase 1 must always send to the SAME thread
                        // so the AI can produce a summary. We bypass dispatch_action
                        // because it would redirect to a new thread when tokens >= threshold.
                        log::info!(
                            "[auto_prompt] ContextOverflow — sending summarization prompt to same thread (tokens={:?})",
                            action.actual_input_tokens
                        );
                        match _view.update_in(cx, |_view, window, cx| {
                            if let Some(active_tv) = _view.active_thread() {
                                let prompt = action.next_prompt.clone();
                                active_tv.update(cx, |tv, cx| {
                                    tv.message_editor.update(cx, |editor, cx| {
                                        editor.set_message(
                                            vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))],
                                            window,
                                            cx,
                                        );
                                    });
                                    tv.send(window, cx);
                                });
                                log::info!(
                                    "[auto_prompt] ContextOverflow summarization sent to same thread"
                                );
                            } else {
                                log::warn!(
                                    "[auto_prompt] ContextOverflow: no active thread for summarization"
                                );
                            }
                        }) {
                            Ok(()) => {}
                            Err(err) => {
                                log::warn!(
                                    "[auto_prompt] FAILED to dispatch context overflow (view may have been dropped): {err}"
                                );
                            }
                        }
                    }
                    Ok(auto_prompt::AutoPromptOutcome::Stopped { reason }) => {
                        auto_prompt::reset_llm_failure_count();
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
                            let _ = workspace.update(cx, |workspace, cx| {
                                let status_toast = StatusToast::new(
                                    format!("Auto-prompt stopped: {reason}"),
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
                        let error_message = format!("{err:#}");
                        if let Some(ref tv) = thread_weak {
                            if let Err(update_err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state =
                                    AutoPromptState::Failed(error_message.clone());
                                tv._auto_prompt_retry_data = Some(data.clone());
                                cx.notify();
                            }) {
                                log::warn!(
                                    "[auto_prompt] failed to set Failed state: {update_err}"
                                );
                            }
                        }
                        log::warn!(
                            "[auto_prompt] LLM call failed after {} attempts: {err}",
                            config.max_llm_retries
                        );
                        if let Some(ref workspace) = workspace_weak {
                            let short_message = error_message
                                .lines()
                                .next()
                                .unwrap_or(&error_message);
                            let _ = workspace.update(cx, |workspace, cx| {
                                let toast = StatusToast::new(
                                    format!("Auto-prompt failed: {short_message}"),
                                    cx,
                                    |this, _| {
                                        this.icon(
                                            Icon::new(IconName::XCircle)
                                                .size(IconSize::Small)
                                                .color(Color::Error),
                                        )
                                        .auto_dismiss(false)
                                        .dismiss_button(true)
                                    },
                                );
                                workspace.toggle_status_toast(toast, cx);
                            });
                        }
                    }
                }
            });

            Some(task)
        }
    }
}
