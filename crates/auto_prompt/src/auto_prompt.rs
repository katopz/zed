//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and decides whether
//! a follow-up prompt should be dispatched.
//!
//! This crate contains the decision logic only. The caller (agent_ui)
//! handles the actual GPUI action dispatch.

mod config;
mod context;

pub use config::AutoPromptConfig;
pub use context::{AutoPromptContext, AutoPromptResponse, PlanFileContent};

use agent_client_protocol as acp;
use anyhow::Context as _;
use futures::StreamExt;
use gpui::App;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Seconds of inactivity before an auto-prompt chain is considered stale.
const CHAIN_TIMEOUT_SECS: u64 = 300;

/// Iteration counter for the current auto-prompt chain.
static AUTO_PROMPT_ITERATION: AtomicU32 = AtomicU32::new(0);

/// UNIX timestamp of the last auto-prompt iteration.
static LAST_ITERATION_SECS: AtomicU64 = AtomicU64::new(0);

/// Data needed to dispatch a follow-up prompt via GPUI action.
///
/// The caller (agent_ui) wraps this in `AutoPromptNewThread` action.
#[derive(Clone, Debug)]
pub struct AutoPromptAction {
    pub from_session_id: acp::SessionId,
    pub from_title: Option<String>,
    pub next_prompt: String,
}

/// Result of the auto-prompt decision logic.
pub enum AutoPromptDecision {
    /// No action needed. Chain stops or is paused.
    NoAction,
    /// Dispatch this action immediately (e.g. token overflow forces "continue").
    DispatchNow(AutoPromptAction),
    /// Dispatch this action after a delay (e.g. error backoff).
    DispatchAfterDelay {
        action: AutoPromptAction,
        delay_ms: u64,
    },
    /// Need to call LLM asynchronously to determine the next step.
    NeedsLlmCall(LlmCallData),
}

/// Data needed for the async LLM call path.
pub struct LlmCallData {
    pub model: Arc<dyn LanguageModel>,
    pub system_prompt: String,
    pub context_json: String,
    pub forced_prompt: Option<String>,
    pub session_id: acp::SessionId,
    pub title: Option<String>,
    pub iteration_count: u32,
}

/// Synchronous pre-check and decision.
///
/// Returns `NoAction` if auto-prompt should not fire (disabled, no tools,
/// cancelled, max iterations, no model configured).
/// Returns `DispatchNow` or `DispatchAfterDelay` for cases that bypass the LLM
/// (token overflow, error backoff).
/// Returns `NeedsLlmCall` when the orchestration LLM must decide.
pub fn decide(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    cx: &App,
) -> AutoPromptDecision {
    let config = match AutoPromptConfig::load() {
        Ok(c) => c,
        Err(err) => {
            log::debug!("auto_prompt: config load failed: {err}");
            return AutoPromptDecision::NoAction;
        }
    };

    if !config.enabled {
        return AutoPromptDecision::NoAction;
    }

    if !used_tools {
        return AutoPromptDecision::NoAction;
    }

    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::debug!("auto_prompt: thread was cancelled, skipping");
        return AutoPromptDecision::NoAction;
    }

    let iteration_count = get_iteration();

    if iteration_count > config.max_iterations {
        log::info!(
            "auto_prompt: max iterations ({}) reached, stopping chain",
            config.max_iterations
        );
        reset_iteration();
        return AutoPromptDecision::NoAction;
    }

    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry
        .thread_summary_model()
        .or_else(|| registry.default_model())
    else {
        log::debug!("auto_prompt: no language model configured in Zed");
        return AutoPromptDecision::NoAction;
    };
    let model = configured_model.model;

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

    let make_action = |prompt: String| AutoPromptAction {
        from_session_id: session_id.clone(),
        from_title: thread_title.clone(),
        next_prompt: prompt,
    };

    if auto_prompt_ctx.exceeds_token_limit(config.max_context_tokens) {
        log::info!(
            "auto_prompt: token limit exceeded (~{} > {}), forcing continue",
            auto_prompt_ctx.approximate_token_count,
            config.max_context_tokens
        );
        return AutoPromptDecision::DispatchNow(make_action("continue".to_string()));
    }

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
        return AutoPromptDecision::DispatchAfterDelay {
            action: make_action("continue".to_string()),
            delay_ms: delay,
        };
    }

    let forced_prompt = if auto_prompt_ctx.last_message_indicates_completion() {
        let (pending, in_progress, completed) = auto_prompt_ctx.plan_stats();
        Some(format!(
            "The AI indicates the task is complete. Verify against the plan:\n\
             - Plan stats: {pending} pending, {in_progress} in progress, {completed} completed\n\
             - Check current_plan against plan_files from .plan/ folder\n\
             - If ALL items are done, respond with #ALL_PLAN_DONE\n\
             - If items remain, continue with the next pending item"
        ))
    } else if auto_prompt_ctx.last_message_is_question() {
        Some(
            "The AI asked a question. Re-examine plan_files from .plan/ folder and current_plan \
             to determine the best course of action. Choose the option that best aligns with the \
             original plan. If you are not confident (below 0.5), set should_continue to false \
             and explain why in reason."
                .to_string(),
        )
    } else {
        None
    };

    let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
    let context_json = match serde_json::to_string(&auto_prompt_ctx) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize context: {err}");
            return AutoPromptDecision::NoAction;
        }
    };

    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model,
        system_prompt,
        context_json,
        forced_prompt,
        session_id,
        title: thread_title,
        iteration_count,
    })
}

/// Async LLM call to determine the next prompt.
///
/// Returns `Some(action)` if the chain should continue, `None` to stop.
pub async fn decide_with_llm(data: LlmCallData, cx: &gpui::AsyncApp) -> Option<AutoPromptAction> {
    log::info!(
        "auto_prompt: iteration {}, calling language model for next-prompt decision",
        data.iteration_count
    );

    let result =
        call_language_model(&data.model, &data.system_prompt, &data.context_json, cx).await;

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

            let all_done = response.all_plan_done
                || response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

            if all_done {
                log::info!("auto_prompt: #ALL_PLAN_DONE detected, stopping chain");
                reset_iteration();
                return None;
            }

            if response.confidence.is_some_and(|c| c < 0.5) {
                log::info!(
                    "auto_prompt: low confidence ({:?}), stopping chain",
                    response.confidence
                );
                reset_iteration();
                return None;
            }

            if !response.should_continue && !has_prompt {
                log::info!("auto_prompt: LLM says stop, no next_prompt");
                reset_iteration();
                return None;
            }

            let next_prompt = if has_prompt {
                let prompt = response.next_prompt.unwrap();
                prompt.replace("#ALL_PLAN_DONE", "").trim().to_string()
            } else if let Some(forced) = data.forced_prompt {
                forced
            } else {
                log::info!("auto_prompt: no prompt determined, stopping");
                reset_iteration();
                return None;
            };

            if next_prompt.is_empty() {
                log::info!("auto_prompt: prompt was empty after cleanup, stopping");
                reset_iteration();
                return None;
            }

            log::info!(
                "auto_prompt: dispatching new thread with prompt: {}...",
                next_prompt.chars().take(80).collect::<String>()
            );

            Some(AutoPromptAction {
                from_session_id: data.session_id,
                from_title: data.title,
                next_prompt,
            })
        }
        Err(err) => {
            log::warn!("auto_prompt: language model call failed: {err}");
            None
        }
    }
}

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

        ## Git Flow (always apply):
        - main: production-ready code only
        - develop: integration branch, features merge here
        - feature/NN_description: new features from develop, merge back to develop
        - hotfix/NN_description: urgent fixes from main, merge to main AND develop
        - release/vX.Y.Z: from develop, merge to main and develop, tag on main
        - Use git rebase instead of merge when possible
        - Never push force to shared branches

        ## Conventional Commits (always apply):
        - feat: for new features
        - fix: for bug fixes
        - refactor: for code restructuring
        - test: for test additions/changes
        - chore: for maintenance tasks
        - docs: for documentation

        ## Plan Status Tracking (always apply):
        - Plan files live in .plan/ folder (accessible via plan_files in context)
        - Plans have a status index at the top using checkboxes: [ ] pending, [x] done
        - When a step completes, instruct the agent to mark it [x] in the plan file
        - When suggesting next_prompt, reference the next [ ] step by number
        - When all steps are [x], set all_plan_done to true
    "}
    .to_string()
}

fn reset_iteration() {
    AUTO_PROMPT_ITERATION.store(0, Ordering::Relaxed);
}

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

fn read_plan_files(thread: &acp_thread::AcpThread) -> Vec<PlanFileContent> {
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

async fn call_language_model(
    model: &Arc<dyn LanguageModel>,
    system_prompt: &str,
    context_json: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptResponse> {
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![system_prompt.to_owned().into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![context_json.to_owned().into()],
                cache: false,
                reasoning_details: None,
            },
        ],
        ..Default::default()
    };

    let mut stream = model
        .stream_completion(request, cx)
        .await
        .context("auto_prompt: failed to start completion stream")?;

    let mut response_text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(LanguageModelCompletionEvent::Text(text)) => response_text.push_str(&text),
            Ok(_) => {}
            Err(err) => {
                log::warn!("auto_prompt: stream error: {err}");
                break;
            }
        }
    }

    parse_response(&response_text)
}

fn parse_response(text: &str) -> anyhow::Result<AutoPromptResponse> {
    let json_str = extract_json(text);
    serde_json::from_str(json_str).with_context(|| {
        format!(
            "auto_prompt: failed to parse response as JSON: {}",
            text.chars().take(500).collect::<String>()
        )
    })
}

fn extract_json(text: &str) -> &str {
    if let Some(start) = text.find("```json") {
        let content_start = start + 7;
        if let Some(end) = text[content_start..].find("```") {
            return text[content_start..content_start + end].trim();
        }
    }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                return &text[start..=end];
            }
        }
    }
    text.trim()
}
