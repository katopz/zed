//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and optionally creates
//! a new thread with a follow-up prompt.
//!
//! # Configuration
//!
//! Create `~/.config/zed/auto_prompt.json`:
//! ```json
//! {
//!   "enabled": true,
//!   "max_iterations": 20,
//!   "max_context_tokens": 80000,
//!   "backoff_base_ms": 2000
//! }
//! ```
//!
//! # LLM Response Format
//!
//! The model should return JSON:
//! ```json
//! {
//!   "should_continue": true,
//!   "next_prompt": "Now run the tests and fix any failures",
//!   "reason": "optional explanation",
//!   "all_plan_done": false,
//!   "confidence": 0.9
//! }
//! ```
//!
//! # Stop Conditions
//!
//! The auto-prompt loop stops when:
//! - The LLM responds with `all_plan_done: true` or includes `#ALL_PLAN_DONE` in the prompt
//! - The LLM's confidence is below 0.5 (too uncertain to proceed)
//! - Max iterations is reached (default: 20)
//! - The LLM sets `should_continue: false` with no `next_prompt`
//! - The thread was cancelled by the user
//!
//! # Default Prompt Logic
//!
//! Based on the conversation state, different strategies are applied:
//! 1. **Completion detected**: Ask LLM to verify plan items against `.plan/` folder
//! 2. **Question detected**: Let LLM recommend based on plan context, stop if low confidence
//! 3. **Error state**: Force "continue" with exponential backoff delay
//! 4. **Token overflow**: Force "continue" to keep momentum and avoid hallucination
//! 5. **Max iterations**: Hard stop regardless of other conditions
//!
//! # Files touched outside this folder (minimal)
//!
//! - `conversation_view.rs`: 1 line in `AcpThreadEvent::Stopped` handler
//! - `agent_panel.rs`: ~20 lines to register `AutoPromptNewThread` action handler
//! - `agent_ui.rs`: 1 line `mod auto_prompt;`

mod client;
mod config;
mod context;

pub use config::AutoPromptConfig;
pub use context::AutoPromptContext;

use agent_client_protocol as acp;
use context::PlanFileContent;
use gpui::Window;

use acp_thread::AcpThread;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Seconds of inactivity before an auto-prompt chain is considered stale.
const CHAIN_TIMEOUT_SECS: u64 = 300;

/// Iteration counter for the current auto-prompt chain.
static AUTO_PROMPT_ITERATION: AtomicU32 = AtomicU32::new(0);

/// UNIX timestamp of the last auto-prompt iteration.
static LAST_ITERATION_SECS: AtomicU64 = AtomicU64::new(0);

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
}

/// Default system prompt instructing the model how to respond.
fn default_system_prompt() -> String {
    indoc::indoc! {"
        You are an orchestration assistant embedded in the Zed editor.
        You receive the full context of a conversation that just finished \
        and decide whether a follow-up action is needed.

        Respond ONLY with valid JSON in this exact format:
        {\"should_continue\": bool, \"next_prompt\": string | null, \"reason\": string | null, \"all_plan_done\": bool, \"confidence\": float}

        ## Cases to handle:

        ### Case 1: Task completion detected
        If the last assistant message indicates task completion (e.g. 'all done', 'task complete'):
        - Compare current_plan entries against plan_files (the original plan from .plan/ folder)
        - Check the code changes against each plan item to verify completion
        - If ALL plan items are completed AND verified, set all_plan_done to true
        - If some items remain, set next_prompt to continue with the next pending item
        - You may include #ALL_PLAN_DONE in next_prompt to signal the loop should stop

        ### Case 2: Question detected
        If the last assistant message asks a question:
        - Re-examine plan_files for context on what the user intended
        - Choose the most reasonable option based on the plan
        - If confidence is low (< 0.5), set should_continue to false and explain why in reason
        - If confidence is high enough, provide a clear next_prompt answering the question

        ### Case 3: Normal continuation
        If the conversation ended normally without completion or questions:
        - Check if there are pending plan items in current_plan
        - If yes, set next_prompt to continue with the next pending item
        - If no plan items remain, check if the overall goal seems achieved
        - If achieved, set should_continue to false

        ## Rules:
        - Keep next_prompt concise and actionable
        - confidence ranges from 0.0 (not sure at all) to 1.0 (very confident)
        - If confidence < 0.5, always set should_continue to false
        - Never repeat the same prompt that was just executed
        - iteration_count tells you how many auto-prompt cycles have run; consider stopping if > 10
    "}
    .to_string()
}

/// Reset the auto-prompt iteration counter.
fn reset_iteration() {
    AUTO_PROMPT_ITERATION.store(0, Ordering::Relaxed);
}

/// Get the current iteration count, with timeout-based chain reset.
///
/// If more than `CHAIN_TIMEOUT_SECS` have passed since the last iteration,
/// the counter is reset (treats this as a new user-initiated chain).
fn get_iteration() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let last = LAST_ITERATION_SECS.load(Ordering::Relaxed);

    if last > 0 && now.saturating_sub(last) > CHAIN_TIMEOUT_SECS {
        log::info!(
            "auto_prompt: chain timeout ({}s since last iteration), resetting",
            now.saturating_sub(last)
        );
        AUTO_PROMPT_ITERATION.store(0, Ordering::Relaxed);
    }

    let iteration = AUTO_PROMPT_ITERATION.fetch_add(1, Ordering::Relaxed) + 1;
    LAST_ITERATION_SECS.store(now, Ordering::Relaxed);

    log::debug!("auto_prompt: iteration {iteration}");
    iteration
}

/// Read `.plan/` folder files from the thread's work directories.
///
/// Looks for a `.plan/` directory in each work dir and reads all files
/// (non-recursive, skipping files > 100KB).
fn read_plan_files(thread: &AcpThread) -> Vec<PlanFileContent> {
    let work_dirs = match thread.work_dirs() {
        Some(dirs) => dirs.paths().to_vec(),
        None => return Vec::new(),
    };

    let mut plan_files = Vec::new();

    for work_dir in &work_dirs {
        let plan_dir = work_dir.join(".plan");
        if !plan_dir.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&plan_dir) {
            Ok(entries) => entries,
            Err(err) => {
                log::debug!("auto_prompt: cannot read {}: {err}", plan_dir.display());
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.len() > 100_000 {
                log::debug!(
                    "auto_prompt: skipping large plan file ({} bytes): {}",
                    metadata.len(),
                    path.display()
                );
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            plan_files.push(PlanFileContent {
                path: path.to_string_lossy().to_string(),
                content,
            });
        }
    }

    if !plan_files.is_empty() {
        log::debug!("auto_prompt: loaded {} plan file(s)", plan_files.len());
    }

    plan_files
}

/// Main entry point — called from `ConversationView::handle_thread_event`
/// when `AcpThreadEvent::Stopped` fires.
///
/// Add to the `Stopped` handler in `conversation_view.rs`:
/// ```rust
/// crate::auto_prompt::on_thread_stopped(&thread, used_tools, stop_reason, window, cx);
/// ```
pub fn on_thread_stopped(
    thread: &gpui::Entity<AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    let config = match AutoPromptConfig::load() {
        Ok(c) => c,
        Err(err) => {
            log::debug!("auto_prompt: config load failed: {err}");
            return;
        }
    };

    if !config.enabled {
        return;
    }

    if !used_tools {
        return;
    }

    // User cancelled — don't auto-prompt, but don't reset the chain either
    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::debug!("auto_prompt: thread was cancelled, skipping");
        return;
    }

    let iteration_count = get_iteration();

    // CASE 5: Max iterations guard — hard stop
    if iteration_count > config.max_iterations {
        log::info!(
            "auto_prompt: max iterations ({}) reached, stopping chain",
            config.max_iterations
        );
        reset_iteration();
        return;
    }

    // Use Zed's configured model (thread_summary_model → default_model)
    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry
        .thread_summary_model()
        .or_else(|| registry.default_model())
    else {
        log::debug!("auto_prompt: no language model configured in Zed");
        return;
    };
    let model = configured_model.model;

    // Collect context with all new fields
    let (auto_prompt_ctx, session_id, thread_title) = {
        let thread_ref = thread.read(cx);
        let stop_reason_str = format!("{stop_reason:?}").to_lowercase();
        let plan_files = read_plan_files(thread_ref);
        let ctx = AutoPromptContext::collect(
            thread_ref,
            cx,
            stop_reason_str,
            plan_files,
            iteration_count,
        );
        let sid = thread_ref.session_id().clone();
        let title = thread_ref.title().map(|t| t.to_string());
        (ctx, sid, title)
    };

    // CASE 4: Token overflow — force "continue" without calling the LLM
    if auto_prompt_ctx.exceeds_token_limit(config.max_context_tokens) {
        log::info!(
            "auto_prompt: token limit exceeded (~{} > {}), forcing continue",
            auto_prompt_ctx.approximate_token_count,
            config.max_context_tokens
        );
        let next_prompt = "continue".to_string();
        let _ = cx.spawn_in(window, async move |_view, cx| {
            _view
                .update_in(cx, |_view, window, cx| {
                    window.dispatch_action(
                        Box::new(AutoPromptNewThread {
                            from_session_id: session_id,
                            from_title: thread_title,
                            next_prompt,
                        }),
                        cx,
                    );
                })
                .ok();
        });
        return;
    }

    // CASE 3: Error state — backoff delay + "continue" without calling the LLM
    if auto_prompt_ctx.had_error
        || matches!(
            stop_reason,
            acp::StopReason::Refusal | acp::StopReason::MaxTokens
        )
    {
        let delay = config.backoff_delay_ms(iteration_count);
        log::info!(
            "auto_prompt: error state (had_error={}, stop_reason={:?}), backing off {}ms then continuing",
            auto_prompt_ctx.had_error,
            stop_reason,
            delay
        );
        let next_prompt = "continue".to_string();
        let _ = cx.spawn_in(window, async move |_view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;

            _view
                .update_in(cx, |_view, window, cx| {
                    window.dispatch_action(
                        Box::new(AutoPromptNewThread {
                            from_session_id: session_id,
                            from_title: thread_title,
                            next_prompt,
                        }),
                        cx,
                    );
                })
                .ok();
        });
        return;
    }

    // Determine forced prompt based on conversation state
    let forced_prompt = if auto_prompt_ctx.last_message_indicates_completion() {
        // CASE 1: Completion detected — ask LLM to verify plan against code
        let (pending, in_progress, completed) = auto_prompt_ctx.plan_stats();
        Some(format!(
            "The AI indicates the task is complete. Verify against the plan:\n\
             - Plan stats: {pending} pending, {in_progress} in progress, {completed} completed\n\
             - Check current_plan against plan_files from .plan/ folder\n\
             - If ALL items are done, respond with #ALL_PLAN_DONE\n\
             - If items remain, continue with the next pending item"
        ))
    } else if auto_prompt_ctx.last_message_is_question() {
        // CASE 2: Question detected — ask LLM to recheck plan and recommend or stop
        Some(
            "The AI asked a question. Re-examine plan_files from .plan/ folder and current_plan \
             to determine the best course of action. Choose the option that best aligns with the \
             original plan. If you are not confident (below 0.5), set should_continue to false \
             and explain why in reason."
                .to_string(),
        )
    } else {
        // Normal: let LLM decide naturally based on system prompt rules
        None
    };

    let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
    let context_json = match serde_json::to_string(&auto_prompt_ctx) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize context: {err}");
            return;
        }
    };

    let _ = cx.spawn_in(window, async move |_view, cx| {
        log::info!(
            "auto_prompt: iteration {iteration_count}, calling language model for next-prompt decision"
        );

        let result = client::call_language_model(&model, &system_prompt, &context_json, cx).await;

        match result {
            Ok(response) => {
                let has_prompt = response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| !p.trim().is_empty());

                log::info!(
                    "auto_prompt: response received, should_continue={}, has_next_prompt={}, all_plan_done={}",
                    response.should_continue,
                    has_prompt,
                    response.all_plan_done
                );

                if let Some(reason) = &response.reason {
                    log::info!("auto_prompt: reason: {reason}");
                }

                // Check #ALL_PLAN_DONE stop signal
                let all_done = response.all_plan_done
                    || response
                        .next_prompt
                        .as_ref()
                        .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

                if all_done {
                    log::info!("auto_prompt: #ALL_PLAN_DONE detected, stopping chain");
                    reset_iteration();
                    return;
                }

                // Check low confidence stop
                if response.confidence.is_some_and(|c| c < 0.5) {
                    log::info!(
                        "auto_prompt: low confidence ({:?}), stopping chain",
                        response.confidence
                    );
                    reset_iteration();
                    return;
                }

                // Check explicit stop
                if !response.should_continue && !has_prompt {
                    log::info!("auto_prompt: LLM says stop, no next_prompt");
                    reset_iteration();
                    return;
                }

                // Determine the actual prompt to use
                let next_prompt = if has_prompt {
                    let prompt = response.next_prompt.unwrap();
                    // Strip any #ALL_PLAN_DONE that might have snuck in
                    prompt.replace("#ALL_PLAN_DONE", "").trim().to_string()
                } else if let Some(forced) = forced_prompt {
                    forced
                } else {
                    log::info!("auto_prompt: no prompt determined, stopping");
                    reset_iteration();
                    return;
                };

                if next_prompt.is_empty() {
                    log::info!("auto_prompt: prompt was empty after cleanup, stopping");
                    reset_iteration();
                    return;
                }

                log::info!(
                    "auto_prompt: dispatching new thread with prompt: {}...",
                    next_prompt.chars().take(80).collect::<String>()
                );

                _view.update_in(cx, |_view, window, cx| {
                    window.dispatch_action(
                        Box::new(AutoPromptNewThread {
                            from_session_id: session_id,
                            from_title: thread_title,
                            next_prompt,
                        }),
                        cx,
                    );
                })
                .ok();
            }
            Err(err) => {
                log::warn!("auto_prompt: language model call failed: {err}");
            }
        }
    });
}
