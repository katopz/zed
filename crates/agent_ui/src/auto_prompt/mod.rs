use acp_thread::{AgentThreadEntry, MentionUri, ThreadStatus, ToolCallStatus};
use agent::ZED_AGENT_ID;
use agent_client_protocol::schema::v1 as acp;
use agent_servers::CLAUDE_AGENT_ID;
use gpui::Window;
use language_model::LanguageModelRegistry;
use notifications::status_toast::StatusToast;
use prompt_store::{BuiltInPrompt, PromptId, PromptStore};
use settings::Settings;
use std::path::PathBuf;
use ui::prelude::*;
use workspace::PathList;

use crate::thread_metadata_store::ThreadMetadataStore;

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
                        persist_upgraded_prompt(&global_auto_prompt_path, &default_content);
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
        persist_upgraded_prompt(&global_auto_prompt_path, &default_content);
        Some((default_content, true))
    } else {
        Some((stored_prompt, false))
    }
}

/// Write the upgraded default prompt back to the global AUTO_PROMPT.md so the
/// "outdated" detection does not recur on every thread stop. The toast still
/// fires once (signaling the upgrade to the user), but subsequent runs see the
/// up-to-date version and skip both the write and the toast.
fn persist_upgraded_prompt(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            log::warn!(
                "[auto_prompt] Failed to create parent dir for {path:?}: {err}"
            );
            return;
        }
    }
    if let Err(err) = std::fs::write(path, content) {
        log::warn!(
            "[auto_prompt] Failed to persist upgraded AUTO_PROMPT.md to {path:?}: {err}"
        );
    } else {
        log::info!(
            "[auto_prompt] Persisted upgraded AUTO_PROMPT.md to {path:?}"
        );
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

/// Build a same-thread continuation prompt. Used for all agents — the LLM
/// orchestration call already reasoned about the last assistant message and
/// plan state to decide whether to continue, pre-stop, or stop. This function
/// only formats the instruction; it does not second-guess that decision.
///
/// For same-thread continuations the last assistant message is already visible
/// in the thread history, so the decision is emitted as-is. A static preamble
/// is only used as a fallback when the decision is empty or a bare generic
/// "Continue from where we left off." (e.g. from manual_auto_prompt). Bolting a
/// generic preamble onto a substantive decision produced absurd two-paragraph
/// messages (e.g. preamble + "Yes, I love you" in reply to "Do you love me?").
fn build_continuation_prompt(
    _last_assistant_message: Option<&str>,
    decision: &str,
) -> String {
    let trimmed = decision.trim();

    // Bare generic continuation (manual_auto_prompt, overflow fallbacks) — no
    // substantive task to emit, so use a minimal continuation instruction.
    let is_generic_continuation = trimmed
        .strip_prefix("Continue from where we left off")
        .map_or(false, |rest| rest.trim().trim_end_matches('.').is_empty());

    if !trimmed.is_empty() && !is_generic_continuation {
        return trimmed.to_string();
    }

    "Continue from where we left off.".to_string()
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

    // Same-thread continuation for all agents. The LLM orchestration already
    // decided to continue based on the last assistant message + plan state —
    // we just format the instruction. No /compact: it caused infinite loops
    // (succeed once, then fail "not enough messages" forever) and the
    // orchestration call already handles context overflow via ContextOverflow
    // → summarize → new thread when tokens exceed the limit.
    if !use_new_thread {
        if let Some(active_tv) = conversation_view.active_thread() {
            // Check if the user is mid-composition in the active thread's editor.
            // The same-thread path overwrites the editor via set_message, which
            // would destroy the user's draft. We log the state for diagnosis.
            let editor_has_text = active_tv.read(cx)
                .message_editor
                .read(cx)
                .text(cx)
                .trim()
                .is_empty();
            log::info!(
                "[auto_prompt] SAME-THREAD continuation: editor_was_empty={}, sending to same thread (tokens={:?})",
                editor_has_text,
                action.actual_input_tokens
            );
            let decision = strip_first_prompt_wrapper(&action.next_prompt);
            let prompt = build_continuation_prompt(
                action.last_assistant_message.as_deref(),
                &decision,
            );
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
                "[auto_prompt] dispatch_action: sent continuation to same thread (tokens={:?})",
                action.actual_input_tokens,
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

    // Continuation threads must inherit the agent of the thread they're
    // continuing, not whatever agent happens to be the panel's stale
    // `selected_agent` (which tracks the last-focused thread across the
    // whole workspace, or a persisted cross-workspace value). Otherwise a
    // GLM/native-agent conversation can silently continue as a Claude Code
    // (or any other ACP) agent thread if the user last looked at one of
    // those elsewhere in the panel.
    let source_agent = conversation_view.agent_key().clone();

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
            let Some(panel) = workspace.panel::<crate::AgentPanel>(cx) else {
                log::warn!("[auto_prompt] dispatch_action: AgentPanel not found in workspace");
                return;
            };

            // Focus the new thread when either:
            //   (a) the user explicitly asked for it (manual_auto_prompt sets
            //       `focus_new_thread = true`), or
            //   (b) the global `auto_focus_new_thread` setting is on.
            // LLM-decided continuations leave `focus_new_thread = false` and
            // defer entirely to the setting, so background chains never steal
            // focus unless the user has opted in.
            let focus = action.focus_new_thread
                || agent_settings::AgentSettings::get_global(cx).auto_focus_new_thread;
            if focus {
                workspace.focus_panel::<crate::AgentPanel>(window, cx);
            }

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
                let continued_from = match &initial_content {
                    crate::AgentInitialContent::ThreadSummary { session_id, .. } => {
                        Some(session_id.clone())
                    }
                    _ => None,
                };
                // When the user has not opted into auto-focus, create the
                // continuation thread in the background (retained_threads)
                // instead of replacing the active base_view. This avoids
                // visual focus stealing: the new thread generates silently
                // and shows up in the sidebar without disturbing whatever
                // the user is currently looking at.
                let new_thread_id = if focus {
                    panel.external_thread(
                        Some(source_agent.clone()),
                        None,
                        work_dirs,
                        action.from_title.clone().map(Into::into),
                        Some(initial_content),
                        focus,
                        crate::AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                } else {
                    panel.external_thread_background(
                        Some(source_agent.clone()),
                        None,
                        work_dirs,
                        action.from_title.clone().map(Into::into),
                        Some(initial_content),
                        crate::AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                };
                if let (Some(thread_id), Some(from_session_id)) = (new_thread_id, continued_from) {
                    if let Some(store) = ThreadMetadataStore::try_global(cx) {
                        store.update(cx, |store, cx| {
                            store.set_continued_from(thread_id, from_session_id, cx);
                        });
                    }
                }
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

    // Route to the isolated Claude path for ACP Claude agents. Claude Code
    // manages its own context/compaction — it must never hit the native
    // ContextOverflow / summarize / new-thread flow (see claude_agent.rs).
    let is_claude_agent = thread
        .read(cx)
        .connection()
        .agent_id()
        .as_ref()
        == CLAUDE_AGENT_ID;

    let decision = if is_claude_agent {
        log::info!("[auto_prompt] Claude agent detected — using claude_agent::decide_claude");
        auto_prompt::claude_agent::decide_claude(thread, used_tools, stop_reason, cx)
    } else {
        auto_prompt::decide(thread, used_tools, stop_reason, cx)
    };
    log::info!("[auto_prompt] decision result: {:?}", decision);
    let is_claude_agent_for_task = is_claude_agent;

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

                let mut data = data;

                // Claude path keeps its own minimal system prompt and skips the
                // native path's plan/summary/verification prompt overrides.
                // The native path may still apply AUTO_PROMPT.md / store prompt.
                if !is_claude_agent_for_task {
                    let store_prompt_result = load_auto_prompt_system_prompt(cx).await;
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

                let mut result = if is_claude_agent_for_task {
                    auto_prompt::claude_agent::decide_claude_async(data.clone(), cx).await
                } else {
                    auto_prompt::decide_with_llm(data.clone(), cx).await
                };

                // Unified retry loop with exponential backoff. Handles two retry triggers:
                //   1. `Err(err)` — orchestration LLM call itself failed (network/parse/timeout).
                //   2. `Ok(RetryAfterBackoff { delay_ms, reason })` — call succeeded but the
                //      decision was "defer" (issue 007: context overflow + had_error means
                //      Phase 2 would create a doomed thread; wait for the rate limit to clear
                //      and re-run the decision).
                // Both paths share the same `llm_failure_count` budget and `max_llm_retries`
                // cap; when exhausted, the loop exits and the terminal outcome (Stopped for
                // RetryAfterBackoff, or the original Err) falls through to the match below.
                loop {
                    let (delay_ms, retry_label) = match &result {
                        Err(err) => {
                            let failure_count = auto_prompt::increment_llm_failure_count();
                            if failure_count > config.max_llm_retries {
                                break; // Max retries exhausted — fall through to error handler.
                            }
                            let delay = config.backoff_delay_ms(failure_count);
                            log::warn!(
                                "[auto_prompt] LLM call failed (attempt {}/{}): {err}, retrying in {}ms",
                                failure_count,
                                config.max_llm_retries,
                                delay
                            );
                            (delay, format!("LLM call failure #{failure_count}"))
                        }
                        Ok(auto_prompt::AutoPromptOutcome::RetryAfterBackoff { delay_ms, reason }) => {
                            // Issue 007 deferred decision. Counted against the same retry budget
                            // as orchestration-call failures so a permanently-exhausted API
                            // eventually surfaces a Stopped to the user instead of looping.
                            let failure_count = auto_prompt::increment_llm_failure_count();
                            if failure_count > config.max_llm_retries {
                                log::warn!(
                                    "[auto_prompt] RetryAfterBackoff exhausted {} retries — converting to Stopped ({reason})",
                                    config.max_llm_retries
                                );
                                // Replace result with a terminal Stopped carrying the reason.
                                result = Ok(auto_prompt::AutoPromptOutcome::Stopped {
                                    reason: reason.clone(),
                                });
                                break;
                            }
                            log::warn!(
                                "[auto_prompt] RetryAfterBackoff (attempt {}/{}) — deferring decision: {reason}, waiting {}ms",
                                failure_count,
                                config.max_llm_retries,
                                delay_ms
                            );
                            (*delay_ms, format!("RetryAfterBackoff #{failure_count} ({reason})"))
                        }
                        Ok(_) => break, // Terminal success outcome — exit loop.
                    };

                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(delay_ms))
                        .await;

                    if let Some(ref tv) = thread_weak {
                        if is_cancelled(tv, cx) {
                            log::info!(
                                "[auto_prompt] Cancelled during retry delay ({retry_label})"
                            );
                            return;
                        }
                    }

                    log::info!("[auto_prompt] Retrying LLM call ({retry_label})");
                    result = if is_claude_agent_for_task {
                        auto_prompt::claude_agent::decide_claude_async(data.clone(), cx).await
                    } else {
                        auto_prompt::decide_with_llm(data.clone(), cx).await
                    };
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
                    Ok(auto_prompt::AutoPromptOutcome::RetryAfterBackoff { delay_ms, reason }) => {
                        // Defensive: the unified retry loop above converts every
                        // `RetryAfterBackoff` to `Stopped` once `max_llm_retries` is
                        // exhausted, so this arm should be unreachable. If a future
                        // refactor changes that invariant, fall back to a stop with
                        // the same reason rather than silently dropping the outcome.
                        log::warn!(
                            "[auto_prompt] RetryAfterBackoff reached post-loop match unexpectedly — treating as Stopped ({delay_ms}ms, {reason})"
                        );
                        auto_prompt::reset_llm_failure_count();
                        if let Some(ref tv) = thread_weak {
                            if let Err(err) = tv.update(cx, |tv, cx| {
                                tv.auto_prompt_state = AutoPromptState::Idle;
                                cx.notify();
                            }) {
                                log::warn!("[auto_prompt] failed to reset state on unexpected RetryAfterBackoff: {err}");
                            }
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

// ──────────────────────────────────────────────────────────────────────
// Stuck-thread watchdog
// ──────────────────────────────────────────────────────────────────────
//
// See `auto_prompt::watchdog` and `.issues/002_auto_prompt_stuck_after_tool_call_watchdog.md`
// for the full design rationale. Short version: if the worker LLM stream
// hangs after a tool call, `on_thread_stopped` never fires and every other
// auto_prompt timeout is unreachable. The watchdog is the only recovery path.
//
// The watchdog task is stored in `ThreadView._watchdog_task` and dropped
// (cancelled) when the thread stops normally. If it fires, it calls a
// headless reasoning LLM that decides `continue` (reschedule) or `halt`
// (cancel worker + inject timeout notice).

/// Build the `WatchdogContext` by scanning the thread's entries in reverse
/// for the last tool call (input + output) and the last assistant message.
///
/// Returns `None` when the thread is no longer `Generating` — the caller
/// should treat that as "nothing to do, the thread recovered on its own."
fn gather_watchdog_context(
    thread: &acp_thread::AcpThread,
    cx: &gpui::App,
    cumulative_elapsed_secs: u64,
    timeout_number: u32,
) -> Option<auto_prompt::watchdog::WatchdogContext> {
    if thread.status() != ThreadStatus::Generating {
        return None;
    }

    let entries = thread.entries();

    // Walk backwards to find the most-recent tool call. We stop at the first
    // user message (anything before that is a previous turn).
    let mut last_tool_input: Option<String> = None;
    let mut last_tool_output: Option<String> = None;

    for entry in entries.iter().rev() {
        match entry {
            AgentThreadEntry::UserMessage(_) => break,
            AgentThreadEntry::ToolCall(tool_call)
                if last_tool_input.is_none()
                    && matches!(
                        tool_call.status,
                        ToolCallStatus::Completed
                            | ToolCallStatus::Failed
                            | ToolCallStatus::InProgress
                            | ToolCallStatus::Pending
                    ) =>
            {
                // raw_input / raw_output are serde_json::Value — serialise to a
                // compact string for the reasoning prompt.
                last_tool_input = tool_call.raw_input.as_ref().map(|v| v.to_string());
                last_tool_output = tool_call.raw_output.as_ref().map(|v| v.to_string());
            }
            AgentThreadEntry::ToolCall(_)
            | AgentThreadEntry::AssistantMessage(_)
            | AgentThreadEntry::CompletedPlan(_)
            | AgentThreadEntry::ContextCompaction(_)
            | AgentThreadEntry::Elicitation(_)
            | AgentThreadEntry::AgentBoardNotification(_) => {}
        }
    }

    // Reuse the thread's own helper — it handles Markdown entity reads correctly.
    let last_assistant = thread.last_assistant_message_text(cx);

    Some(auto_prompt::watchdog::WatchdogContext {
        last_tool_call_input: last_tool_input,
        last_tool_call_output: last_tool_output,
        last_assistant_message: last_assistant,
        cumulative_elapsed_secs,
        timeout_number,
    })
}

/// Start the stuck-thread watchdog for a thread.
///
/// Call this when the thread enters `Generating` and auto-prompt is enabled.
/// The returned `Task` should be stored in `ThreadView._watchdog_task` so it
/// is cancelled when the thread stops normally (just drop the task).
///
/// Returns `None` when the watchdog is disabled in config or no model is
/// configured.
///
/// `thread` is the worker thread to watch. `conversation_view` is used only
/// for the timeout-notice dispatch (clearing the watchdog task and sending
/// the recovery prompt). Both are passed as weak handles to avoid requiring
/// an active entity lease at the call site — this function only reads `cx`.
pub fn start_watchdog(
    thread: gpui::WeakEntity<acp_thread::AcpThread>,
    conversation_view: gpui::WeakEntity<crate::ConversationView>,
    activity_gen: std::rc::Rc<std::cell::Cell<u64>>,
    window: &Window,
    cx: &gpui::App,
) -> Option<gpui::Task<()>> {
    let config = auto_prompt::load_config_cached().ok()?;
    if !config.watchdog_enabled || config.watchdog_timeout_secs == 0 {
        return None;
    }

    let registry = LanguageModelRegistry::read_global(cx);
    let configured_model = registry.default_model()?;
    let model = configured_model.model.clone();

    // Claude Code authenticates itself outside Zed's LanguageModelRegistry, so
    // the watchdog's reasoning call only makes sense when Zed's default model
    // is a real Anthropic model. Otherwise it would silently burn calls
    // against whatever other provider is configured (possibly the same one
    // that's rate-limited, if Claude is being used as a fallback for it).
    let is_claude_agent = thread
        .upgrade()
        .map(|t| t.read(cx).connection().agent_id().as_ref() == CLAUDE_AGENT_ID)
        .unwrap_or(false);
    if is_claude_agent && model.provider_id() != language_model::ANTHROPIC_PROVIDER_ID {
        log::info!(
            "[auto_prompt::watchdog] Claude agent thread but default model provider is {:?}, \
             not Anthropic — skipping watchdog",
            model.provider_id()
        );
        return None;
    }

    let timeout_secs = config.watchdog_timeout_secs;
    let thread_weak = thread;

    log::info!(
        "[auto_prompt::watchdog] Starting watchdog: timeout={}s, model={:?}",
        timeout_secs,
        model.id()
    );

    let task = window.spawn(cx, async move |cx| {
        let mut timeout_number: u32 = 0;

        loop {
            // Capture the activity generation before sleeping. If it changes
            // during the sleep (send, NewEntry, EntryUpdated), the thread is
            // actively working — re-sleep instead of firing.
            let gen_at_sleep = activity_gen.get();

            cx.background_executor()
                .timer(std::time::Duration::from_secs(timeout_secs))
                .await;

            if activity_gen.get() != gen_at_sleep {
                // Activity happened during the sleep window. The thread is not
                // stuck — reset and sleep again.
                continue;
            }

            timeout_number += 1;

            log::warn!(
                "[auto_prompt::watchdog] Timeout #{} fired — no activity for {}s. \
                 Reasoning about whether to halt.",
                timeout_number,
                timeout_secs
            );

            // Gather context from the thread. If the thread is no longer
            // generating, it recovered on its own — exit quietly.
            let context = match thread_weak.read_with(cx, |thread, cx| {
                gather_watchdog_context(thread, cx, timeout_secs, timeout_number)
            }) {
                Ok(Some(ctx)) => ctx,
                Ok(None) => {
                    log::info!(
                        "[auto_prompt::watchdog] Thread is no longer Generating — exiting watchdog"
                    );
                    return;
                }
                Err(err) => {
                    log::warn!(
                        "[auto_prompt::watchdog] Thread entity dropped, exiting: {err}"
                    );
                    return;
                }
            };

            // Ask the reasoning LLM whether to continue or halt.
            let decision =
                auto_prompt::watchdog::reason_about_stuck_thread(&model, &context, cx)
                    .await
                    .unwrap_or(auto_prompt::watchdog::WatchdogDecision::Continue {
                        reason: "reasoning call errored".to_string(),
                    });

            match decision {
                auto_prompt::watchdog::WatchdogDecision::Continue { reason } => {
                    log::info!(
                        "[auto_prompt::watchdog] Decision: CONTINUE (timeout #{}, reason: {})",
                        timeout_number,
                        reason
                    );
                    // Loop again — sleep another window.
                    continue;
                }
                auto_prompt::watchdog::WatchdogDecision::Halt { reason } => {
                    log::warn!(
                        "[auto_prompt::watchdog] Decision: HALT (timeout #{}, reason: {}) \
                         — cancelling worker and injecting timeout notice",
                        timeout_number,
                        reason
                    );

                    // Cancel the worker thread. This triggers Stopped(Cancelled),
                    // which on_thread_stopped treats as NoAction (resets iteration).
                    // We then dispatch a timeout prompt to the SAME thread so the
                    // worker can recover with full context.
                    let cancel_task = match thread_weak.update(cx, |thread, cx| {
                        thread.cancel(cx)
                    }) {
                        Ok(task) => task,
                        Err(err) => {
                            log::warn!(
                                "[auto_prompt::watchdog] Cannot cancel thread (dropped?): {err}"
                            );
                            return;
                        }
                    };

                    // Wait for the cancel to fully complete before injecting the
                    // timeout message — otherwise the new prompt may race with
                    // the in-flight cancellation.
                    cancel_task.await;

                    // Build and dispatch the timeout-recovery prompt.
                    let timeout_prompt = format!(
                        "⚠️ Watchdog timeout: your last tool call completed \
                         approximately {} minutes ago but you produced no follow-up \
                         response (the LLM stream appeared to hang). You have been \
                         automatically cancelled. Please decide how to proceed: \
                         retry the task from where you left off, try a different \
                         approach, or explicitly state that you are done. \
                         Reason for halt: {}",
                        timeout_secs / 60,
                        reason
                    );

                    let action = match thread_weak.read_with(cx, |thread, _cx| {
                        auto_prompt::AutoPromptAction {
                            from_session_id: thread.session_id().clone(),
                            from_title: thread.title().map(|t| t.to_string()),
                            next_prompt: timeout_prompt,
                            work_dirs: thread
                                .work_dirs()
                                .map(|pl| pl.paths().to_vec()),
                            original_user_message: None,
                            profile_id: None,
                            actual_input_tokens: None,
                            approximate_token_count: 0,
                            last_assistant_message: None,
                            force_new_thread: false,
                            focus_new_thread: false,
                        }
                    }) {
                        Ok(action) => action,
                        Err(err) => {
                            log::warn!(
                                "[auto_prompt::watchdog] Cannot build timeout action \
                                 (thread dropped?): {err}"
                            );
                            return;
                        }
                    };

                    match conversation_view.update_in(cx, |view, window, cx| {
                        // Clear the watchdog task before dispatching so it doesn't
                        // interfere with the new generation's lifecycle.
                        if let Some(active) = view.active_thread() {
                            active.update(cx, |active, cx| {
                                active._watchdog_task = None;
                                cx.notify();
                            });
                        }
                        dispatch_action(action, view, window, cx);
                    }) {
                        Ok(()) => {
                            log::info!(
                                "[auto_prompt::watchdog] Timeout notice dispatched to thread"
                            );
                        }
                        Err(err) => {
                            log::warn!(
                                "[auto_prompt::watchdog] Failed to dispatch timeout notice \
                                 (view dropped?): {err}"
                            );
                        }
                    }

                    // The dispatch above starts a new generation (via
                    // dispatch_action -> tv.send -> send_content). A NEW watchdog
                    // is armed inside `send_content` when that generation begins
                    // — so we exit this loop here.
                    return;
                }
            }
        }
    });

    Some(task)
}

/// Cancel any running watchdog task for a specific thread.
///
/// Call this when the thread stops normally (Stopped / Error / Refusal) — the
/// watchdog is no longer needed. Simply dropping the `Task` cancels it.
///
/// Targets the thread identified by `session_id`, NOT the currently-active
/// view. The previous implementation used `active_thread()`, which cancels
/// the wrong thread when the user has switched away from the thread that
/// emitted the stop event — leaving a stale watchdog behind that accumulates
/// elapsed time across generations (see issue 004).
pub fn cancel_watchdog_for_thread(
    conversation_view: &crate::ConversationView,
    session_id: &acp::SessionId,
    cx: &mut gpui::App,
) {
    if let Some(view) = conversation_view.thread_view(session_id) {
        view.update(cx, |view, _cx| {
            view.cancel_watchdog();
        });
    }
}
