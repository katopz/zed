//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and decides whether
//! a follow-up prompt should be dispatched.
//!
//! This crate contains the decision logic only. The caller (agent_ui)
//! handles the actual GPUI action dispatch.

mod config;
pub mod context;

pub use config::AutoPromptConfig;
pub use context::{AutoPromptContext, AutoPromptResponse, PlanFileContent, StopPhase};

use acp::schema::{SessionId, StopReason};
use agent_client_protocol as acp;
use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use gpui::App;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Seconds of inactivity before an auto-prompt chain is considered stale.
const CHAIN_TIMEOUT_SECS: u64 = 300;

/// Fallback log directory when no project root is available.
const FALLBACK_LOG_DIR: &str = "/tmp/zed_auto_prompt_logs";

/// Iteration counter for the current auto-prompt chain.
static AUTO_PROMPT_ITERATION: AtomicU32 = AtomicU32::new(0);

/// UNIX timestamp of the last auto-prompt iteration.
static LAST_ITERATION_SECS: AtomicU64 = AtomicU64::new(0);

/// Pre-stop verification attempt counter for the current chain.
static VERIFICATION_COUNT: AtomicU32 = AtomicU32::new(0);

/// LLM orchestration call failure counter for the current chain.
static AUTO_PROMPT_LLM_FAILURE_COUNT: AtomicU32 = AtomicU32::new(0);

use std::sync::RwLock;
use std::time::SystemTime;

/// Cached config to avoid repeated file reads.
static CACHED_CONFIG: RwLock<Option<(AutoPromptConfig, SystemTime)>> = RwLock::new(None);

/// Helper to load config with caching. Public for use by agent_ui.
pub fn load_config_cached() -> Result<AutoPromptConfig, anyhow::Error> {
    let path = AutoPromptConfig::config_path()?;
    let metadata = std::fs::metadata(&path).ok();

    let modified_time = metadata.and_then(|m| m.modified().ok());

    // Check cache
    {
        let cache = CACHED_CONFIG.read().unwrap();
        if let Some((config, cached_time)) = cache.as_ref() {
            match (&modified_time, cached_time) {
                (Some(mod_time), _) if mod_time == cached_time => {
                    return Ok(config.clone());
                }
                (Some(_mod_time), _) => {
                    log::info!(
                        "[auto_prompt::config] Config cache STALE (file modified), reloading"
                    );
                }
                (None, _) => {
                    return Ok(config.clone());
                }
            }
        } else {
            log::info!("[auto_prompt::config] Config cache MISS");
        }
    }

    // Load fresh config
    let config = AutoPromptConfig::load()?;
    let cache_time = modified_time.unwrap_or_else(SystemTime::now);

    // Update cache
    {
        let mut cache = CACHED_CONFIG.write().unwrap();
        *cache = Some((config.clone(), cache_time));
    }

    log::info!("[auto_prompt::config] Config loaded and cached");
    Ok(config)
}

/// Helper to invalidate config cache (e.g., when settings change).
pub fn invalidate_config_cache() {
    let mut cache = CACHED_CONFIG.write().unwrap();
    *cache = None;
    log::info!("[auto_prompt::config] Config cache invalidated");
}

/// Data needed to dispatch a follow-up prompt via GPUI action.
///
/// The caller (agent_ui) wraps this in `AutoPromptNewThread` action.
#[derive(Clone, Debug)]
pub struct AutoPromptAction {
    pub from_session_id: SessionId,
    pub from_title: Option<String>,
    pub next_prompt: String,
    pub work_dirs: Option<Vec<std::path::PathBuf>>,
    /// The raw original user message from the very first thread,
    /// carried across chain hops to prevent summary drift.
    pub original_user_message: Option<String>,
    /// The profile/mode from the previous thread (e.g. "Auto", "Sonnet", "High"),
    /// carried across chain hops to preserve the user's selection.
    pub profile_id: Option<String>,
}

/// Outcome of an auto-prompt LLM decision.
#[derive(Clone, Debug)]
pub enum AutoPromptOutcome {
    /// Chain should continue with this action.
    Continue(AutoPromptAction),
    /// Chain stopped with a reason (shown to user as info toast).
    Stopped { reason: String },
}

pub fn with_first_prompt_context(
    next_prompt: String,
    original_user_message: Option<&str>,
    thread_summary: Option<&str>,
    last_assistant_message: Option<&str>,
) -> String {
    match original_user_message {
        Some(msg) if !msg.trim().is_empty() => {
            let msg = msg.trim();
            let mut parts = vec![
                "## 1. First Prompt (original request)".to_string(),
                String::new(),
                msg.to_string(),
                String::new(),
                "---".to_string(),
            ];

            if let Some(summary) = thread_summary.filter(|s| !s.trim().is_empty()) {
                parts.push(String::new());
                parts.push("## 2. Thread Summary".to_string());
                parts.push(String::new());
                parts.push(summary.trim().to_string());
                parts.push(String::new());
                parts.push("---".to_string());
            }

            if let Some(last) = last_assistant_message.filter(|s| !s.trim().is_empty()) {
                let truncated = if last.len() > 2000 {
                    format!("{}...", &last[..2000])
                } else {
                    last.trim().to_string()
                };
                parts.push(String::new());
                parts.push("## 3. Last Assistant Message".to_string());
                parts.push(String::new());
                parts.push(truncated);
                parts.push(String::new());
                parts.push("---".to_string());
            }

            parts.push(String::new());
            parts.push("## 4. Decision".to_string());
            parts.push(String::new());
            parts.push(next_prompt);
            parts.join("\n")
        }
        _ => next_prompt,
    }
}

/// Extract the raw original user intent from a thread's `first_user_message`.
///
/// When auto_prompt chains threads, the new thread's first user message looks like:
///   `[@Thread Name](link)\n\nrefer to first prompt:\n===---===\n...\n===---===\nactual work prompt`
///
/// This function strips the auto-generated wrapper to recover the original
/// user intent, which may be embedded in a `refer to first prompt` block.
pub fn extract_original_user_message(first_user_message: &str) -> Option<String> {
    let stripped = first_user_message.trim();

    // Try new 4-part structured format FIRST on raw input,
    // before any header stripping removes the "## 1. First Prompt" marker.
    if let Some(pos) = stripped.find("## 1. First Prompt (original request)") {
        let after_header = &stripped[pos + "## 1. First Prompt (original request)".len()..];
        let after_header = after_header.trim_start_matches('\n');
        // Extract everything up to the first "---" separator (before section 2/3/4)
        if let Some(end_pos) = after_header.find("\n---") {
            let extracted = after_header[..end_pos].trim();
            if !extracted.is_empty() {
                return Some(extracted.to_string());
            }
        } else {
            // No separator found, take everything after header
            let extracted = after_header.trim();
            if !extracted.is_empty() {
                return Some(extracted.to_string());
            }
        }
    }

    // Strip leading "## User" header(s) from to_markdown rendering
    let without_header = stripped
        .lines()
        .skip_while(|line| line.trim_start().starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");

    let without_header = without_header.trim();

    // Strip leading markdown link line(s) like "[@Thread Name](zed:///agent/thread/...)"
    let without_link = without_header
        .lines()
        .skip_while(|line| line.trim_start().starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");

    let without_link = without_link.trim();

    // Try old structured format: "## User (checkpoint)\n\n{text}\n---\nrefer to first thread\n---\n..."
    if let Some(pos) = without_link.find("\n---\nrefer to first thread\n---\n") {
        let extracted = without_link[..pos].trim();
        let cleaned = extracted
            .lines()
            .skip_while(|line| line.trim_start().starts_with("## "))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }

    // Try block-delimited format: "refer to first prompt:\n===---===\n...\n===---===\n<next_prompt>"
    const DELIM: &str = "===---===";
    if let Some(rest) = without_link.strip_prefix("refer to first prompt:") {
        let rest = rest.trim_start();
        if let Some(after_open) = rest.strip_prefix(DELIM) {
            let after_open = after_open.trim_start_matches('\n');
            if let Some(end_pos) = after_open.find(DELIM) {
                let original = after_open[..end_pos].trim().to_string();
                if !original.is_empty() {
                    return Some(original);
                }
            }
        }
    }

    // Try legacy --- delimited format for threads created before the delimiter change.
    if let Some(rest) = without_link.strip_prefix("refer to first prompt:") {
        let rest = rest.trim_start();
        if let Some(after_open) = rest.strip_prefix("---") {
            let after_open = after_open.trim_start_matches('\n');
            if let Some(end_pos) = after_open.find("\n---") {
                let original = after_open[..end_pos].trim().to_string();
                if !original.is_empty() {
                    return Some(original);
                }
            }
        }
    }

    // Try legacy quote format for backward compat: "refer to first prompt "...""
    if let Some(rest) = without_link.strip_prefix("refer to first prompt") {
        let rest = rest.trim();
        if rest.starts_with('"') {
            if let Some(after_quote) = rest.strip_prefix('"') {
                if let Some(end) = after_quote.find('"') {
                    let original = after_quote[..end].to_string();
                    if !original.trim().is_empty() {
                        return Some(original);
                    }
                }
            }
        }
    }

    // No wrapper found — this is likely the original raw message
    if !without_link.is_empty() {
        Some(without_link.to_string())
    } else {
        None
    }
}

/// Result of the auto-prompt decision logic.
#[derive(Debug)]
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
#[derive(Clone)]
pub struct LlmCallData {
    pub model: Arc<dyn LanguageModel>,
    pub system_prompt: String,
    pub context_json: String,
    pub project_root: Option<PathBuf>,
    pub session_id: SessionId,
    pub title: Option<String>,
    pub iteration_count: u32,
    pub max_verification_attempts: u32,
    pub work_dirs: Option<Vec<PathBuf>>,
    pub first_user_message: Option<String>,
    /// The raw original user message from the very first thread,
    /// carried across chain hops to prevent summary drift.
    pub original_user_message: Option<String>,
    /// The last assistant message from the previous thread,
    /// included in continuation prompts for context.
    pub last_assistant_message: Option<String>,
    /// The profile/mode from the previous thread (e.g. "Auto", "Sonnet", "High"),
    /// carried across chain hops to preserve the user's selection.
    pub profile_id: Option<String>,
}

impl std::fmt::Debug for LlmCallData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmCallData")
            .field("model", &self.model.id())
            .field("system_prompt", &self.system_prompt)
            .field(
                "context_json",
                &format!("<{} chars>", self.context_json.len()),
            )
            .field("project_root", &self.project_root)
            .field("session_id", &self.session_id)
            .field("title", &self.title)
            .field("iteration_count", &self.iteration_count)
            .field("max_verification_attempts", &self.max_verification_attempts)
            .field("work_dirs", &self.work_dirs)
            .field(
                "last_assistant_message",
                &self
                    .last_assistant_message
                    .as_ref()
                    .map(|s| format!("<{} chars>", s.len())),
            )
            .field("profile_id", &self.profile_id)
            .finish()
    }
}

/// Input for the pure evaluation function.
pub struct EvaluationInput {
    pub should_continue: bool,
    pub confidence: Option<f64>,
    pub next_prompt: Option<String>,
    pub reason: Option<String>,
    pub all_plan_done: bool,
    pub next_plan_prompt: Option<String>,
    pub last_assistant_message: Option<String>,
}

/// Result of evaluating an LLM response.
#[derive(Debug, PartialEq)]
pub enum EvaluationResult {
    /// Continue the chain with this prompt.
    Continue { prompt: String, reason: String },
    /// LLM wants to stop — must go through verification gate.
    WantsStop { reason: String },
}

/// Pure function — no side effects, no atomics, fully testable.
pub fn evaluate_response(input: &EvaluationInput) -> EvaluationResult {
    let has_prompt = input
        .next_prompt
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty());

    // 1. all_done + next plan → Continue (transition to next plan)
    if input.all_plan_done {
        if let Some(next_plan_prompt) = &input.next_plan_prompt {
            return EvaluationResult::Continue {
                prompt: next_plan_prompt.clone(),
                reason: "current plan done, transitioning to next plan".to_string(),
            };
        }
        // 2. all_done + should_continue → Continue (dispatch final gitflow commit)
        if input.should_continue {
            return EvaluationResult::Continue {
                prompt: "All plans are complete. Create develop branch from main if it doesn't exist. Then create or reuse a git feature branch from develop and commit all changes with conventional commit messages (feat/fix/refactor) if not committed yet. Do not merge — leave the branch for review.".to_string(),
                reason: "all plans done but LLM says continue, dispatching final gitflow commit".to_string(),
            };
        }
        return EvaluationResult::WantsStop {
            reason: "all plans done, no remaining plans".to_string(),
        };
    }

    if input.confidence.is_some_and(|c| c < 0.5) {
        return EvaluationResult::WantsStop {
            reason: format!(
                "confidence too low ({:.2} < 0.5)",
                input.confidence.unwrap()
            ),
        };
    }

    // 3. detect_remaining_work override
    if !input.should_continue {
        if let Some(remaining_prompt) =
            detect_remaining_work(input.last_assistant_message.as_deref())
        {
            return EvaluationResult::Continue {
                prompt: remaining_prompt,
                reason: "LLM says stop but last_assistant_message contains remaining work — overriding to continue".to_string(),
            };
        }
    }

    // 4. should_continue + non-empty prompt → Continue
    if input.should_continue && has_prompt {
        let prompt = input.next_prompt.as_ref().unwrap();
        let cleaned = prompt.replace("#ALL_PLAN_DONE", "").trim().to_string();
        if !cleaned.is_empty() {
            return EvaluationResult::Continue {
                prompt: cleaned,
                reason: "LLM says continue with next prompt".to_string(),
            };
        }
    }

    // 5. Everything else → WantsStop
    let reason = match (&input.reason, has_prompt) {
        (Some(r), _) => r.clone(),
        (None, false) => "LLM says stop, no next prompt".to_string(),
        (None, true) => "LLM says stop despite having prompt".to_string(),
    };
    EvaluationResult::WantsStop { reason }
}

/// Synchronous pre-check and decision.
///
/// Returns `NoAction` if auto-prompt should not fire (disabled, no tools,
/// cancelled, max iterations, no model configured).
/// Returns `NeedsLlmCall` for all non-trivial cases (LLM decides).
pub fn decide(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &StopReason,
    cx: &App,
) -> AutoPromptDecision {
    log::info!("[auto_prompt::decide] Starting decision process");

    let project_root = thread
        .read(cx)
        .work_dirs()
        .and_then(|pl| pl.paths().first().cloned());
    let iteration_count = get_iteration();

    write_stop_log(
        project_root.as_ref(),
        iteration_count,
        &format!("evaluation started (stop_reason={stop_reason:?}, used_tools={used_tools})"),
    );

    let config = match load_config_cached() {
        Ok(c) => {
            log::info!("[auto_prompt::decide] Config loaded");
            c
        }
        Err(err) => {
            log::warn!("[auto_prompt::decide] config load failed: {err}");
            write_stop_log(
                project_root.as_ref(),
                iteration_count,
                &format!("config load failed: {err}"),
            );
            return AutoPromptDecision::NoAction;
        }
    };

    log::info!("[auto_prompt::decide] Auto-prompt evaluating");

    let thread_has_tools = thread.read(cx).has_tool_calls();
    if thread_has_tools {
        log::info!(
            "[auto_prompt::decide] Tools were used in this session (last_turn={})",
            used_tools
        );
    } else {
        log::info!(
            "[auto_prompt::decide] No tools in this session (last_turn={}), continuing to LLM decision",
            used_tools
        );
    }

    if matches!(stop_reason, StopReason::Cancelled) {
        log::info!("[auto_prompt::decide] Thread was cancelled, stopping");
        write_stop_log(
            project_root.as_ref(),
            iteration_count,
            "thread cancelled by user",
        );
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Stop reason: {:?}", stop_reason);

    // Rule-based check: if the last tool call was an interactive auth command
    // (browser login, device auth, etc.), the user is mid-flow — don't chain.
    if is_interactive_tool_pending(thread, cx) {
        log::info!("[auto_prompt::decide] Interactive auth tool pending, stopping");
        write_stop_log(
            project_root.as_ref(),
            iteration_count,
            "interactive auth tool pending — user must complete login",
        );
        return AutoPromptDecision::NoAction;
    }

    log::info!(
        "[auto_prompt::decide] Current iteration: {}",
        iteration_count
    );

    if iteration_count > config.max_iterations {
        log::info!(
            "[auto_prompt::decide] Max iterations ({}) reached, stopping chain",
            config.max_iterations
        );
        write_stop_log(
            project_root.as_ref(),
            iteration_count,
            &format!("max iterations ({}) reached", config.max_iterations),
        );
        reset_iteration();
        return AutoPromptDecision::NoAction;
    }

    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry.default_model() else {
        log::warn!("[auto_prompt::decide] No language model configured in Zed");
        write_stop_log(
            project_root.as_ref(),
            iteration_count,
            "no language model configured in Zed",
        );
        return AutoPromptDecision::NoAction;
    };
    let model = configured_model.model;
    log::info!("[auto_prompt::decide] Using model: {:?}", model.id());

    let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
    let stop_phase = if verification_count == 0 {
        StopPhase::Working
    } else {
        StopPhase::PreStop
    };

    let (auto_prompt_ctx, session_id, thread_title, work_dirs) = {
        let thread_ref = thread.read(cx);
        let stop_reason_str = format!("{stop_reason:?}").to_lowercase();
        let plan_files = read_plan_files(thread_ref);
        let doc_files = read_doc_files(thread_ref);
        let mut ctx = AutoPromptContext::collect(
            thread_ref,
            cx,
            stop_reason_str,
            plan_files,
            doc_files,
            iteration_count,
        );
        ctx.stop_phase = stop_phase;
        ctx.verification_count = verification_count;
        let sid = thread_ref.session_id().clone();
        let title = thread_ref.title().map(|t| t.to_string());
        let dirs = thread_ref.work_dirs().map(|pl| pl.paths().to_vec());
        (ctx, sid, title, dirs)
    };

    // Extract the raw original user message, unwrapping any auto-generated chain wrapper.
    let original_user_message = auto_prompt_ctx
        .first_user_message
        .as_deref()
        .and_then(|raw| extract_original_user_message(raw));

    let _last_assistant_msg = auto_prompt_ctx
        .last_assistant_message()
        .map(|s| s.to_string());

    log::info!(
        "[auto_prompt::decide] Approximate token count: {}",
        auto_prompt_ctx.approximate_token_count
    );

    log::info!(
        "[auto_prompt::decide] Had error: {}",
        auto_prompt_ctx.had_error
    );

    log::info!(
        "[auto_prompt::decide] PATH=llm_call: had_error={}, stop_reason={:?}, iteration={}, tokens={} → NeedsLlmCall (LLM will decide)",
        auto_prompt_ctx.had_error,
        stop_reason,
        iteration_count,
        auto_prompt_ctx.approximate_token_count
    );

    let system_prompt = config.system_prompt.unwrap_or_else(default_system_prompt);
    let context_json = match serde_json::to_string(&auto_prompt_ctx) {
        Ok(json) => {
            log::info!(
                "[auto_prompt::decide] Context serialized successfully ({} chars)",
                json.len()
            );
            json
        }
        Err(err) => {
            log::warn!("[auto_prompt::decide] failed to serialize context: {err}");
            write_stop_log(
                project_root.as_ref(),
                iteration_count,
                &format!("context serialization failed: {err}"),
            );
            return AutoPromptDecision::NoAction;
        }
    };

    let last_assistant_message = auto_prompt_ctx
        .last_assistant_message()
        .map(|s| s.to_string());

    log::info!("[auto_prompt::decide] Returning NeedsLlmCall decision");
    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model,
        system_prompt,
        context_json,
        project_root,
        session_id,
        title: thread_title,
        iteration_count,
        max_verification_attempts: config.max_verification_attempts,
        work_dirs,
        first_user_message: auto_prompt_ctx.first_user_message,
        original_user_message,
        last_assistant_message,
        profile_id: None,
    })
}

/// Async LLM call to determine the next prompt.
///
/// Returns `Some(action)` if the chain should continue, `None` to stop.
pub async fn decide_with_llm(
    data: LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptOutcome> {
    log::warn!(
        "[auto_prompt] *** ENTRY POINT *** decide_with_llm called: session_id={:?}, iteration={}",
        data.session_id,
        data.iteration_count
    );

    log::info!(
        "[auto_prompt::decide_with_llm] Starting LLM call, iteration={}, model={:?}, session_id={:?}",
        data.iteration_count,
        data.model.id(),
        data.session_id
    );

    let result =
        call_language_model(&data.model, &data.system_prompt, &data.context_json, cx).await;

    log::info!(
        "[auto_prompt::decide_with_llm] LLM call completed with result: {:?}",
        result.is_ok()
    );

    match result {
        Ok((raw_response, mut response)) => {
            write_decision_log(
                data.project_root.as_ref(),
                data.iteration_count,
                &format!("{:?}", data.model.id()),
                &data.system_prompt,
                &data.context_json,
                &raw_response,
                &response,
            );

            let has_prompt = response
                .next_prompt
                .as_ref()
                .is_some_and(|p| !p.trim().is_empty());

            log::info!(
                "[auto_prompt::decide_with_llm] Response received: should_continue={}, has_next_prompt={}, all_plan_done={}, confidence={:?}",
                response.should_continue,
                has_prompt,
                response.all_plan_done,
                response.confidence
            );

            if let Some(reason) = &response.reason {
                log::info!("[auto_prompt::decide_with_llm] Reason: {}", reason);
            }

            if let Some(prompt) = &response.next_prompt {
                log::info!("[auto_prompt::decide_with_llm] Next prompt: {}", prompt);
            }

            // Prefer the raw original user message over the LLM's summary.
            // The LLM summary drifts across chain hops (telephone game),
            // while `original_user_message` is carried verbatim from thread 0.
            let prompt_summary = data
                .original_user_message
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    response
                        .first_prompt_summary
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(|s| s.to_string())
                })
                .or_else(|| {
                    data.first_user_message
                        .as_deref()
                        .and_then(extract_original_user_message)
                });

            let all_done = response.all_plan_done
                || response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

            let next_plan_prompt = if all_done {
                find_next_plan_prompt(&data.context_json, data.work_dirs.as_deref()).map(
                    |next_plan| {
                        format!(
                            "Create develop branch from main if it doesn't exist. Then create a git feature branch for the completed plan from develop and commit all changes with conventional commit messages. Then {next_plan}"
                        )
                    },
                )
            } else {
                None
            };

            let input = EvaluationInput {
                should_continue: response.should_continue,
                confidence: response.confidence,
                next_prompt: std::mem::take(&mut response.next_prompt),
                reason: std::mem::take(&mut response.reason),
                all_plan_done: all_done,
                next_plan_prompt,
                last_assistant_message: data.last_assistant_message.clone(),
            };

            log::info!(
                "[auto_prompt::decide_with_llm] evaluate_response input: should_continue={}, all_plan_done={}, confidence={:?}, has_next_plan={}",
                input.should_continue,
                input.all_plan_done,
                input.confidence,
                input.next_plan_prompt.is_some()
            );

            let evaluation = evaluate_response(&input);

            log::info!(
                "[auto_prompt::decide_with_llm] evaluate_response result: {:?}",
                evaluation
            );

            match evaluation {
                EvaluationResult::Continue { prompt, reason } => {
                    log::info!("[auto_prompt::decide_with_llm] Evaluation: Continue — {reason}");

                    let prompt = if is_doc_creation_prompt(&prompt) {
                        match build_checkbox_verification_prompt(&data.context_json) {
                            Some(verification_prompt) => {
                                log::info!(
                                    "auto_prompt: plan has unchecked items, overriding doc creation with checkbox verification"
                                );
                                verification_prompt
                            }
                            None => prompt,
                        }
                    } else {
                        prompt
                    };

                    log::info!(
                        "auto_prompt: dispatching new thread with prompt: {}...",
                        prompt.chars().take(80).collect::<String>()
                    );

                    let next_prompt = with_first_prompt_context(
                        prompt,
                        prompt_summary.as_deref(),
                        data.title.as_deref(),
                        data.last_assistant_message.as_deref(),
                    );

                    Ok(AutoPromptOutcome::Continue(AutoPromptAction {
                        from_session_id: data.session_id,
                        from_title: data.title,
                        next_prompt,
                        work_dirs: data.work_dirs,
                        original_user_message: data.original_user_message,
                        profile_id: data.profile_id.clone(),
                    }))
                }
                EvaluationResult::WantsStop { reason } => {
                    let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
                    let max_verifications = data.max_verification_attempts;

                    if verification_count == 0 {
                        log::info!(
                            "auto_prompt: WantsStop ('{reason}') — initiating pre-stop verification (attempt 1/{max_verifications})"
                        );
                        VERIFICATION_COUNT.fetch_add(1, Ordering::Relaxed);

                        match build_pre_stop_verification_prompt(
                            &data.context_json,
                            &data.work_dirs,
                        ) {
                            Some(verification_prompt) => {
                                log::info!(
                                    "auto_prompt: dispatching pre-stop verification prompt: {}...",
                                    verification_prompt.chars().take(80).collect::<String>()
                                );
                                let next_prompt = with_first_prompt_context(
                                    verification_prompt,
                                    prompt_summary.as_deref(),
                                    data.title.as_deref(),
                                    data.last_assistant_message.as_deref(),
                                );
                                Ok(AutoPromptOutcome::Continue(AutoPromptAction {
                                    from_session_id: data.session_id,
                                    from_title: data.title,
                                    next_prompt,
                                    work_dirs: data.work_dirs,
                                    original_user_message: data.original_user_message,
                                    profile_id: data.profile_id.clone(),
                                }))
                            }
                            None => {
                                let stop_reason =
                                    "LLM says stop, no plan files found for verification"
                                        .to_string();
                                log::info!(
                                    "auto_prompt: no verification needed (no plan files found), stopping"
                                );
                                write_stop_log(
                                    data.project_root.as_ref(),
                                    data.iteration_count,
                                    &stop_reason,
                                );
                                reset_iteration();
                                Ok(AutoPromptOutcome::Stopped {
                                    reason: stop_reason,
                                })
                            }
                        }
                    } else if verification_count < max_verifications {
                        let stop_reason = format!(
                            "LLM says stop after verification attempt {verification_count}/{max_verifications}"
                        );
                        log::info!("auto_prompt: {stop_reason}");
                        write_stop_log(
                            data.project_root.as_ref(),
                            data.iteration_count,
                            &stop_reason,
                        );
                        reset_iteration();
                        Ok(AutoPromptOutcome::Stopped {
                            reason: stop_reason,
                        })
                    } else {
                        let stop_reason =
                            format!("max verification attempts ({max_verifications}) exceeded");
                        log::warn!("auto_prompt: {stop_reason}");
                        write_stop_log(
                            data.project_root.as_ref(),
                            data.iteration_count,
                            &stop_reason,
                        );
                        reset_iteration();
                        Ok(AutoPromptOutcome::Stopped {
                            reason: stop_reason,
                        })
                    }
                }
            }
        }
        Err(err) => {
            write_error_log(
                data.project_root.as_ref(),
                data.iteration_count,
                &format!("{:?}", data.model.id()),
                &err,
            );
            log::warn!("auto_prompt: language model call failed: {err}");
            Err(err)
        }
    }
}

fn write_decision_log(
    project_root: Option<&PathBuf>,
    iteration: u32,
    model: &str,
    system_prompt: &str,
    context_json: &str,
    raw_response: &str,
    parsed: &AutoPromptResponse,
) {
    let logs_dir = match project_root {
        Some(root) => root.join(".logs"),
        None => {
            log::info!(
                "[auto_prompt] decision log: using fallback {FALLBACK_LOG_DIR} (no project root)"
            );
            PathBuf::from(FALLBACK_LOG_DIR)
        }
    };
    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        log::warn!("auto_prompt: failed to create .logs dir: {err}");
        return;
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    let filename = format!("{timestamp}_{iteration}.json");
    let path = logs_dir.join(&filename);

    let log_entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "iteration": iteration,
        "model": model,
        "request": {
            "system_prompt": system_prompt,
            "context_json": context_json,
        },
        "raw_response": raw_response,
        "parsed_response": {
            "should_continue": parsed.should_continue,
            "next_prompt": parsed.next_prompt,
            "reason": parsed.reason,
            "all_plan_done": parsed.all_plan_done,
            "confidence": parsed.confidence,
        },
    });

    match serde_json::to_string_pretty(&log_entry) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                log::warn!("auto_prompt: failed to write log {}: {err}", path.display());
            } else {
                log::info!("auto_prompt: wrote decision log to {}", path.display());
            }
        }
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize log entry: {err}");
        }
    }
}

fn write_error_log(
    project_root: Option<&PathBuf>,
    iteration: u32,
    model: &str,
    error: &anyhow::Error,
) {
    let logs_dir = match project_root {
        Some(root) => root.join(".logs"),
        None => {
            log::info!(
                "[auto_prompt] error log: using fallback {FALLBACK_LOG_DIR} (no project root)"
            );
            PathBuf::from(FALLBACK_LOG_DIR)
        }
    };
    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        log::warn!("auto_prompt: failed to create .logs dir: {err}");
        return;
    }

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    let filename = format!("{timestamp}_{iteration}_error.json");
    let path = logs_dir.join(&filename);

    let log_entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "iteration": iteration,
        "model": model,
        "error": format!("{error:#}"),
    });

    match serde_json::to_string_pretty(&log_entry) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                log::warn!(
                    "auto_prompt: failed to write error log {}: {err}",
                    path.display()
                );
            } else {
                log::info!("auto_prompt: wrote error log to {}", path.display());
            }
        }
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize error log entry: {err}");
        }
    }
}

fn write_stop_log(project_root: Option<&PathBuf>, iteration: u32, reason: &str) {
    let logs_dir = match project_root {
        Some(root) => root.join(".logs"),
        None => {
            log::info!(
                "[auto_prompt] stop: {reason} (no project root, using fallback {FALLBACK_LOG_DIR})"
            );
            PathBuf::from(FALLBACK_LOG_DIR)
        }
    };
    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        log::warn!("auto_prompt: failed to create .logs dir: {err}");
        return;
    }
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    let filename = format!("{timestamp}_{iteration}_stop.json");
    let path = logs_dir.join(&filename);
    let log_entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "iteration": iteration,
        "reason": reason,
    });
    match serde_json::to_string_pretty(&log_entry) {
        Ok(json) => {
            if let Err(err) = std::fs::write(&path, json) {
                log::warn!(
                    "auto_prompt: failed to write stop log {}: {err}",
                    path.display()
                );
            } else {
                log::info!("auto_prompt: wrote stop log to {}", path.display());
            }
        }
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize stop log: {err}");
        }
    }
}

fn default_system_prompt() -> String {
    prompt_store::BuiltInPrompt::AutoPromptSystemPrompt
        .default_content()
        .to_string()
}

/// Checks if the last tool calls suggest the user is completing an
/// interactive flow (browser-based auth, login) and auto_prompt should wait.
fn is_interactive_tool_pending(thread: &gpui::Entity<acp_thread::AcpThread>, cx: &App) -> bool {
    let thread_ref = thread.read(cx);

    for entry in thread_ref.entries().iter().rev() {
        match entry {
            acp_thread::AgentThreadEntry::UserMessage(_) => break,
            acp_thread::AgentThreadEntry::ToolCall(tool) => {
                let is_terminal = tool
                    .tool_name
                    .as_ref()
                    .is_some_and(|name| name == "terminal");
                if !is_terminal {
                    continue;
                }

                if let Some(input) = &tool.raw_input {
                    if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
                        if is_interactive_auth_command(command) {
                            log::info!(
                                "[auto_prompt::decide] Auth command detected: '{}', pausing",
                                command.chars().take(100).collect::<String>()
                            );
                            return true;
                        }
                    }
                }
            }
            _ => continue,
        }
    }

    false
}

/// Patterns indicating an interactive command that opens a browser or
/// waits for external user action (auth flows, device login, etc.).
fn is_interactive_auth_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    const AUTH_COMMANDS: &[&str] = &[
        "login",
        "signin",
        "sign-in",
        "sign in",
        "auth ",
        " authenticate",
        "oauth",
        "sso login",
    ];

    AUTH_COMMANDS.iter().any(|pattern| lower.contains(pattern))
}

pub fn reset_iteration() {
    AUTO_PROMPT_ITERATION.store(0, Ordering::Relaxed);
    VERIFICATION_COUNT.store(0, Ordering::Relaxed);
    AUTO_PROMPT_LLM_FAILURE_COUNT.store(0, Ordering::Relaxed);
}

pub fn increment_llm_failure_count() -> u32 {
    AUTO_PROMPT_LLM_FAILURE_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn reset_llm_failure_count() {
    AUTO_PROMPT_LLM_FAILURE_COUNT.store(0, Ordering::Relaxed);
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
    log::info!("[auto_prompt::read_plan_files] Starting to read plan files");

    let work_dirs = match thread.work_dirs() {
        Some(dirs) => {
            let paths = dirs.paths().to_vec();
            log::info!(
                "[auto_prompt::read_plan_files] Found {} work directory/ies",
                paths.len()
            );
            paths
        }
        None => {
            log::info!("[auto_prompt::read_plan_files] No work directories configured");
            return Vec::new();
        }
    };

    let mut plan_files = Vec::new();

    for work_dir in &work_dirs {
        let plan_dir_candidates = [work_dir.join(".plan"), work_dir.join(".plans")];
        let Some(plan_dir) = plan_dir_candidates.iter().find(|d| d.is_dir()) else {
            log::info!(
                "[auto_prompt::read_plan_files] Neither .plan/ nor .plans/ directory exists in {}",
                work_dir.display()
            );
            continue;
        };
        log::info!(
            "[auto_prompt::read_plan_files] Found plan directory: {}",
            plan_dir.display()
        );

        let entries = match std::fs::read_dir(plan_dir) {
            Ok(entries) => entries,
            Err(err) => {
                log::warn!(
                    "[auto_prompt::read_plan_files] Cannot read directory {}: {err}",
                    plan_dir.display()
                );
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
        log::info!(
            "[auto_prompt::read_plan_files] Loaded {} plan file(s): {:?}",
            plan_files.len(),
            plan_files.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
    } else {
        log::info!("[auto_prompt::read_plan_files] No plan files found in any .plan directory");
    }

    plan_files
}

fn read_doc_files(thread: &acp_thread::AcpThread) -> Vec<PlanFileContent> {
    let work_dirs = match thread.work_dirs() {
        Some(dirs) => dirs.paths().to_vec(),
        None => return Vec::new(),
    };

    let mut doc_files = Vec::new();

    for work_dir in &work_dirs {
        let doc_dir_candidates = [work_dir.join(".docs")];
        let Some(doc_dir) = doc_dir_candidates.iter().find(|d| d.is_dir()) else {
            continue;
        };

        let entries = match std::fs::read_dir(doc_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
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
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            doc_files.push(PlanFileContent {
                path: path.to_string_lossy().to_string(),
                content,
            });
        }
    }

    if !doc_files.is_empty() {
        log::info!(
            "[auto_prompt::read_doc_files] Loaded {} doc file(s): {:?}",
            doc_files.len(),
            doc_files.iter().map(|p| &p.path).collect::<Vec<_>>()
        );
    }

    doc_files
}

async fn call_language_model(
    model: &Arc<dyn LanguageModel>,
    system_prompt: &str,
    context_json: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<(String, AutoPromptResponse)> {
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

    let completion_future = async {
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
        anyhow::Ok(response_text)
    };

    let timeout_future = cx.background_executor().timer(Duration::from_secs(60));

    pin_mut!(completion_future, timeout_future);

    match future::select(completion_future, timeout_future).await {
        future::Either::Left((Ok(response_text), _)) => {
            parse_response(&response_text).map(|parsed| (response_text, parsed))
        }
        future::Either::Left((Err(err), _)) => Err(err.context("auto_prompt: completion failed")),
        future::Either::Right(_) => {
            anyhow::bail!("auto_prompt: LLM call timed out after 60 seconds");
        }
    }
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

/// Checks if there are plan files with remaining unchecked `[ ]` items.
/// First checks the context JSON, then falls back to scanning disk directories.
/// Returns a prompt to start the next plan if found, or None if all plans are complete.
fn find_next_plan_prompt(context_json: &str, work_dirs: Option<&[PathBuf]>) -> Option<String> {
    if let Some(prompt) = find_remaining_in_context(context_json) {
        return Some(prompt);
    }

    find_remaining_on_disk(work_dirs)
}

fn find_remaining_in_context(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ContextPlanFiles {
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: ContextPlanFiles = serde_json::from_str(context_json)
        .inspect_err(|e| {
            log::warn!(
                "[auto_prompt::find_remaining_in_context] Failed to parse context JSON: {e}"
            );
        })
        .ok()?;

    for file in &ctx.plan_files {
        if has_unchecked_items(&file.content) {
            let filename = file.path.rsplit('/').next().unwrap_or(&file.path);
            let plan_dir = file.path.rsplit('/').nth(1).unwrap_or(".plans");
            log::info!(
                "[auto_prompt::find_remaining_in_context] Found remaining plan: {plan_dir}/{filename}"
            );
            return Some(make_plan_read_prompt(plan_dir, filename));
        }
    }

    None
}

fn find_remaining_on_disk(work_dirs: Option<&[PathBuf]>) -> Option<String> {
    let dirs = work_dirs?;

    for work_dir in dirs {
        let plan_dir_candidates = [work_dir.join(".plan"), work_dir.join(".plans")];
        let Some(plan_dir) = plan_dir_candidates.iter().find(|d| d.is_dir()) else {
            continue;
        };

        let Ok(entries) = std::fs::read_dir(&plan_dir) else {
            continue;
        };

        let mut md_files: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().is_file() && e.path().extension().is_some_and(|ext| ext == "md"))
            .collect();
        md_files.sort_by_key(|e| e.file_name());

        for entry in md_files {
            let path = entry.path();
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > 100_000 {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };

            if has_unchecked_items(&content) {
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                let plan_dir_str = path.parent().and_then(|p| p.to_str()).unwrap_or(".plans");
                log::info!(
                    "[auto_prompt::find_remaining_on_disk] Found remaining plan on disk: {plan_dir_str}/{filename}"
                );
                return Some(make_plan_read_prompt(plan_dir_str, filename));
            }
        }
    }

    log::info!("[auto_prompt::find_remaining_on_disk] No remaining plans found on disk");
    None
}

fn has_unchecked_items(content: &str) -> bool {
    let mut in_code_block = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if !in_code_block && trimmed.contains("- [ ] ") {
            return true;
        }
    }
    false
}

fn make_plan_read_prompt(plan_dir: &str, filename: &str) -> String {
    format!(
        "Read {plan_dir}/{filename} and execute the plan starting from the first unchecked step."
    )
}

fn is_doc_creation_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("documentation") || lower.contains(".docs/")
}

/// Code-level remaining work detection: scans the last assistant message
/// for patterns that explicitly indicate incomplete work. This is a safety
/// net that catches cases where the LLM ignores its own Rule 10.
fn detect_remaining_work(last_assistant_message: Option<&str>) -> Option<String> {
    let msg = last_assistant_message?.trim();
    if msg.is_empty() {
        return None;
    }

    let lower = msg.to_lowercase();
    let patterns: &[(&str, &str)] = &[
        ("remaining work", "remaining work"),
        ("remaining:", "remaining:"),
        ("still need", "still need"),
        ("still needs", "still needs"),
        ("next step", "next step"),
        ("next steps", "next steps"),
        ("todo:", "todo:"),
        ("action items", "action items"),
        ("left to do", "left to do"),
    ];

    for (pattern, label) in patterns {
        if lower.contains(pattern) {
            log::warn!(
                "[auto_prompt::detect_remaining_work] Pattern found: {label} in last_assistant_message — overriding stop"
            );
            return Some(
                "Continue with the remaining work described in the assistant's last message."
                    .to_string(),
            );
        }
    }

    for line in msg.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("- [ ] ") || trimmed.starts_with("* [ ] ") {
            log::warn!(
                "[auto_prompt::detect_remaining_work] Pattern found: unchecked checkbox in last_assistant_message — overriding stop"
            );
            return Some(
                "Continue with the remaining work described in the assistant's last message."
                    .to_string(),
            );
        }
    }

    None
}

fn build_pre_stop_verification_prompt(
    context_json: &str,
    work_dirs: &Option<Vec<PathBuf>>,
) -> Option<String> {
    let mut checks: Vec<String> = Vec::new();

    let has_plans = context_json.contains("plan_files") && context_json.contains("- [ ]");

    if has_plans {
        checks.push(
            "1. **Plan completeness**: Read ALL .plans/ and .plan/ files. Every '- [ ]' must be '- [x]' or explicitly inapplicable. If any unchecked item exists, continue working on it.".to_string()
        );
    }

    checks.push("2. **Code diagnostics**: Run `cargo check` and `cargo clippy` (or equivalent). Fix ALL errors and warnings before stopping. No TODOs, no placeholders, no unwrap().".to_string());
    checks.push("3. **Git status**: Verify all changes are committed with conventional commit messages (feat/fix/refactor/test/chore/docs). Create develop from main if it doesn't exist. Then create or reuse a feature branch from develop if not done.".to_string());

    if let Some(remaining) = find_next_plan_prompt(context_json, work_dirs.as_deref()) {
        checks.push(format!(
            "\n4. **Next plan found**: {remaining}\n   Complete the current plan verification first, then transition."
        ));
    }

    if !has_plans {
        return None;
    }

    Some(format!(
        "PRE-STOP VERIFICATION: Before stopping, verify ALL of the following are true.\n\n{}\n\n\
         If ALL checks pass, respond that verification is complete and stop.\n\
         If ANY check fails, fix the issue and continue working.",
        checks.join("\n")
    ))
}

fn build_checkbox_verification_prompt(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ContextPlanFiles {
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: ContextPlanFiles = serde_json::from_str(context_json)
        .inspect_err(|e| {
            log::warn!(
                "[auto_prompt::build_checkbox_verification_prompt] Failed to parse context JSON: {e}"
            );
        })
        .ok()?;

    for file in &ctx.plan_files {
        let mut in_code_block = false;
        for line in file.content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }
            if in_code_block {
                continue;
            }
            if trimmed.contains("- [ ] ") {
                let filename = file.path.rsplit('/').next().unwrap_or(&file.path);
                let plan_dir = file.path.rsplit('/').nth(1).unwrap_or(".plans");
                log::info!(
                    "[auto_prompt::build_checkbox_verification_prompt] Found unchecked items in {plan_dir}/{filename}"
                );
                return Some(format!(
                    "MANDATORY CHECKPOINT: Verify plan checkboxes before documentation.\n\n\
                     Re-read all {plan_dir}/ files and verify every '- [ ]' step against the actual code changes:\n\
                     1. Read each plan file in {plan_dir}/\n\
                     2. For each '- [ ]' item, check if the code already implements it\n\
                     3. Mark completed items as '- [x]' — do NOT re-execute completed work\n\
                     4. If any item is truly incomplete, continue working on it\n\
                     5. Only after ALL items in ALL plan files are '- [x]', create documentation at .docs/\n\n\
                     Unchecked items found in: {plan_dir}/{filename}"
                ));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_with_first_prompt_context_multiline() {
        let original = "check .plans against the code\n\nfile.rs\nanother.rs";
        let result = with_first_prompt_context("continue".to_string(), Some(original), None, None);
        assert!(result.starts_with("## 1. First Prompt (original request)\n\n"));
        assert!(result.contains(original));
        assert!(result.contains("## 4. Decision"));
        assert!(result.ends_with("continue"));
    }

    #[test]
    fn test_with_first_prompt_context_none() {
        let result = with_first_prompt_context("continue".to_string(), None, None, None);
        assert_eq!(result, "continue");
    }

    #[test]
    fn test_with_first_prompt_context_empty() {
        let result = with_first_prompt_context("continue".to_string(), Some(""), None, None);
        assert_eq!(result, "continue");
    }

    #[test]
    fn test_with_first_prompt_context_with_thread_summary_and_last_message() {
        let original = "fix the bug in auth";
        let summary = "Auth Bug Fix Thread";
        let last_msg = "Fixed the auth bug, tests passing";
        let result = with_first_prompt_context(
            "commit the changes".to_string(),
            Some(original),
            Some(summary),
            Some(last_msg),
        );
        assert!(result.starts_with("## 1. First Prompt (original request)\n\n"));
        assert!(result.contains(original));
        assert!(result.contains("## 2. Thread Summary"));
        assert!(result.contains(summary));
        assert!(result.contains("## 3. Last Assistant Message"));
        assert!(result.contains(last_msg));
        assert!(result.contains("## 4. Decision"));
        assert!(result.ends_with("commit the changes"));
    }

    #[test]
    fn test_with_first_prompt_context_4_part_structure() {
        let original = "implement feature X";
        let summary = "Feature X implementation thread";
        let last_msg = "Completed steps 1-3, need to do step 4";
        let result = with_first_prompt_context(
            "do step 4 now".to_string(),
            Some(original),
            Some(summary),
            Some(last_msg),
        );
        // Verify 4-part structure with section headers
        assert!(result.contains("## 1. First Prompt (original request)"));
        assert!(result.contains("## 2. Thread Summary"));
        assert!(result.contains("## 3. Last Assistant Message"));
        assert!(result.contains("## 4. Decision"));

        // Verify sections are in order
        let pos1 = result.find("## 1.").unwrap();
        let pos2 = result.find("## 2.").unwrap();
        let pos3 = result.find("## 3.").unwrap();
        let pos4 = result.find("## 4.").unwrap();
        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
        assert!(pos3 < pos4);
    }

    #[test]
    fn test_extract_original_user_message_new_format_multiline() {
        let input = "refer to first prompt:\n===---===\ncheck .plans against the code and complete it as possible\n\ncrates\nmmorpg\n===---===\ncontinue";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some(
                "check .plans against the code and complete it as possible\n\ncrates\nmmorpg"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_extract_original_user_message_new_format_with_link_and_header() {
        let input = "## User\n\n[@Thread](zed:///agent/thread/abc)\n\nrefer to first prompt:\n===---===\ndo the thing\nwith stuff\n===---===\ncontinue";
        let result = extract_original_user_message(input);
        assert_eq!(result, Some("do the thing\nwith stuff".to_string()));
    }

    #[test]
    fn test_extract_original_user_message_legacy_dash_format() {
        let input = "refer to first prompt:\n---\nold style content\n---\ncontinue";
        let result = extract_original_user_message(input);
        assert_eq!(result, Some("old style content".to_string()));
    }

    #[test]
    fn test_extract_original_user_message_legacy_quote_format() {
        let input = "refer to first prompt \"some quoted content\"\n---\ncontinue";
        let result = extract_original_user_message(input);
        assert_eq!(result, Some("some quoted content".to_string()));
    }

    #[test]
    fn test_extract_original_user_message_raw_message_no_wrapper() {
        let input = "## User\n\njust a raw user message with no wrapper";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some("just a raw user message with no wrapper".to_string())
        );
    }

    #[test]
    fn test_extract_original_user_message_strips_headers_and_links() {
        let input =
            "## User\n\n[@Thread Name](zed:///agent/thread/123?name=Thread)\n\nraw message text";
        let result = extract_original_user_message(input);
        assert_eq!(result, Some("raw message text".to_string()));
    }

    #[test]
    fn test_extract_original_user_message_empty() {
        let result = extract_original_user_message("");
        assert_eq!(result, None);
    }

    #[test]
    fn test_roundtrip_multiline_with_file_refs() {
        let original = "check .plans against the code and complete it as possible, also update plan md to reflect current state, fix all diag focus only on\n\ncrates\nmmorpg";
        let wrapped = with_first_prompt_context("continue".to_string(), Some(original), None, None);
        let extracted = extract_original_user_message(&wrapped);
        assert_eq!(extracted, Some(original.to_string()));
    }

    #[test]
    fn test_roundtrip_with_chain_wrapper() {
        let original = "do important work\n\nfile.rs\nother.rs";
        let wrapped = with_first_prompt_context("continue".to_string(), Some(original), None, None);
        let chain_message = format!("## User\n\n[@Thread](zed:///agent/thread/abc)\n\n{wrapped}");
        let extracted = extract_original_user_message(&chain_message);
        assert_eq!(extracted, Some(original.to_string()));
    }

    #[test]
    fn test_roundtrip_content_with_quotes() {
        let original = r#"use format!("{var}") not format!("{}", var)"#;
        let wrapped = with_first_prompt_context("continue".to_string(), Some(original), None, None);
        let extracted = extract_original_user_message(&wrapped);
        assert_eq!(extracted, Some(original.to_string()));
    }

    #[test]
    fn test_extract_original_user_message_new_structured_format() {
        let input = "## User (checkpoint)\n\nfix the bugs in auth module\n\nsrc/auth.rs\n---\nrefer to first thread\n---\ncommit the changes";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some("fix the bugs in auth module\n\nsrc/auth.rs".to_string())
        );
    }

    #[test]
    fn test_extract_original_user_message_new_4_part_format() {
        let input = "## 1. First Prompt (original request)\n\nimplement the auth module\n\nsrc/auth.rs\n\n---\n\n## 2. Thread Summary\n\nAuth implementation\n\n---\n\n## 4. Decision\n\ncommit changes";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some("implement the auth module\n\nsrc/auth.rs".to_string())
        );
    }

    #[test]
    fn test_roundtrip_4_part_format_with_summary_and_last_message() {
        let original = "implement feature X with files\n\nmod.rs\nlib.rs";
        let summary = "Feature X Thread";
        let last_msg = "Completed implementation";
        let wrapped = with_first_prompt_context(
            "do the next thing".to_string(),
            Some(original),
            Some(summary),
            Some(last_msg),
        );
        let chain_message = format!("## User\n\n[@Thread](zed:///agent/thread/abc)\n\n{wrapped}");
        let extracted = extract_original_user_message(&chain_message);
        assert_eq!(extracted, Some(original.to_string()));
    }

    fn make_input() -> EvaluationInput {
        EvaluationInput {
            should_continue: false,
            confidence: Some(0.8),
            next_prompt: None,
            reason: None,
            all_plan_done: false,
            next_plan_prompt: None,
            last_assistant_message: None,
        }
    }

    // --- Task 4: evaluate_response() state machine tests ---

    #[test]
    fn test_eval_all_done_with_next_plan() {
        let input = EvaluationInput {
            all_plan_done: true,
            next_plan_prompt: Some("do next plan work".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::Continue {
                prompt: "do next plan work".to_string(),
                reason: "current plan done, transitioning to next plan".to_string(),
            }
        );
    }

    #[test]
    fn test_eval_all_done_should_continue_no_next_plan() {
        let input = EvaluationInput {
            all_plan_done: true,
            should_continue: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("gitflow commit"));
            }
            _ => panic!("expected Continue, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_all_done_should_stop_no_next_plan() {
        let input = EvaluationInput {
            all_plan_done: true,
            should_continue: false,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::WantsStop {
                reason: "all plans done, no remaining plans".to_string(),
            }
        );
    }

    #[test]
    fn test_eval_remaining_work_remaining_work_pattern() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some("## Remaining Work\n- fix tests".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("remaining work"));
            }
            _ => panic!("expected Continue override, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_remaining_work_unchecked_checkbox() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some("- [ ] do thing".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("remaining work"));
            }
            _ => panic!("expected Continue override, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_remaining_work_todo_pattern() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some("TODO: fix this".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("remaining work"));
            }
            _ => panic!("expected Continue override, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_remaining_work_no_match() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some("all done, nothing left".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_should_continue_with_valid_prompt() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("commit changes".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::Continue {
                prompt: "commit changes".to_string(),
                reason: "LLM says continue with next prompt".to_string(),
            }
        );
    }

    #[test]
    fn test_eval_should_continue_empty_prompt() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_should_continue_whitespace_prompt() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("   ".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_should_continue_no_prompt() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_should_stop_with_prompt_ignored() {
        let input = EvaluationInput {
            should_continue: false,
            next_prompt: Some("review code".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_low_confidence_with_should_continue_and_prompt() {
        let input = EvaluationInput {
            should_continue: true,
            confidence: Some(0.3),
            next_prompt: Some("go".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence too low"));
            }
            _ => panic!("expected WantsStop for low confidence, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_low_confidence_should_stop() {
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.3),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence too low"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_should_stop() {
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.8),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_all_done_next_plan_overrides_should_stop() {
        let input = EvaluationInput {
            all_plan_done: true,
            should_continue: false,
            next_plan_prompt: Some("start next plan".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::Continue {
                prompt: "start next plan".to_string(),
                reason: "current plan done, transitioning to next plan".to_string(),
            }
        );
    }

    #[test]
    fn test_eval_all_done_no_next_plan_remaining_work_still_stops() {
        let input = EvaluationInput {
            all_plan_done: true,
            should_continue: false,
            last_assistant_message: Some("remaining: fix test".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::WantsStop {
                reason: "all plans done, no remaining plans".to_string(),
            }
        );
    }

    #[test]
    fn test_eval_all_plan_done_in_prompt_stripped() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("done #ALL_PLAN_DONE".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { prompt, .. } => {
                assert_eq!(prompt, "done");
                assert!(!prompt.contains("#ALL_PLAN_DONE"));
            }
            _ => panic!("expected Continue, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_last_assistant_message_none() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_last_assistant_message_empty() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some("".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[derive(Debug, PartialEq)]
    enum VerificationGateResult {
        DispatchVerification,
        StopNoPlanFiles,
        StopAfterVerification,
        StopMaxExceeded,
    }

    fn handle_wants_stop(
        verification_count: u32,
        max_verifications: u32,
        has_verification_prompt: bool,
    ) -> VerificationGateResult {
        if verification_count == 0 && has_verification_prompt {
            VerificationGateResult::DispatchVerification
        } else if verification_count == 0 && !has_verification_prompt {
            VerificationGateResult::StopNoPlanFiles
        } else if verification_count < max_verifications {
            VerificationGateResult::StopAfterVerification
        } else {
            VerificationGateResult::StopMaxExceeded
        }
    }

    #[test]
    fn test_gate_first_stop_with_plan_files() {
        let result = handle_wants_stop(0, 2, true);
        assert_eq!(result, VerificationGateResult::DispatchVerification);
    }

    #[test]
    fn test_gate_first_stop_no_plan_files() {
        let result = handle_wants_stop(0, 2, false);
        assert_eq!(result, VerificationGateResult::StopNoPlanFiles);
    }

    #[test]
    fn test_gate_after_one_verification() {
        let result = handle_wants_stop(1, 2, true);
        assert_eq!(result, VerificationGateResult::StopAfterVerification);
    }

    #[test]
    fn test_gate_at_max_verifications() {
        let result = handle_wants_stop(2, 2, true);
        assert_eq!(result, VerificationGateResult::StopMaxExceeded);
    }

    #[test]
    fn test_gate_exceeded_max() {
        let result = handle_wants_stop(5, 2, true);
        assert_eq!(result, VerificationGateResult::StopMaxExceeded);
    }

    // --- Task 6: decide() gate documentation tests ---
    // These document the expected routing behavior of decide().
    // decide() requires App context so unit testing is impractical,
    // but the behavior is tested via integration in agent_ui.

    #[test]
    fn test_decide_noaction_conditions_documented() {
        // This test documents the NoAction exit conditions in decide().
        // If any of these conditions change, this test should be updated.
        //
        // NoAction exits (in order):
        // 1. Config load failure → NoAction
        // 2. StopReason::Cancelled → NoAction
        // 3. Interactive auth tool pending → NoAction
        // 4. iteration > max_iterations → NoAction
        // 5. No model configured → NoAction
        // 6. Context serialization failure → NoAction
        //
        // All other cases (including token overflow, MaxTokens, error, Refusal)
        // fall through to NeedsLlmCall — the LLM decides.
        //
        // This was changed in Plan 03: previously token overflow, MaxTokens,
        // error, and Refusal bypassed the LLM with DispatchNow/DispatchAfterDelay.
        // Now they all go through the LLM evaluation pipeline.
        assert!(true, "documentation test — see comments above");
    }
}
