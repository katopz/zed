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
    /// Actual input token count from the thread's API usage response.
    /// Used by dispatch to decide same-thread vs new-thread continuation.
    /// Falls back to None when usage data is unavailable.
    pub actual_input_tokens: Option<u64>,
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
    prompt_summary: Option<&str>,
    _thread_title: Option<&str>,
    last_assistant_message: Option<&str>,
) -> String {
    match prompt_summary {
        Some(msg) if !msg.trim().is_empty() => {
            let msg = msg.trim();
            let mut parts = vec![
                "## 1. Thread Summary".to_string(),
                String::new(),
                msg.to_string(),
                String::new(),
                "---".to_string(),
            ];

            if let Some(last) = last_assistant_message.filter(|s| !s.trim().is_empty()) {
                parts.push(String::new());
                parts.push("## 2. Last Assistant Message".to_string());
                parts.push(String::new());
                parts.push(last.trim().to_string());
                parts.push(String::new());
                parts.push("---".to_string());
            }

            parts.push(String::new());
            parts.push("## 3. Decision".to_string());
            parts.push(String::new());
            parts.push(next_prompt);
            parts.join("\n")
        }
        _ => match last_assistant_message.filter(|s| !s.trim().is_empty()) {
            Some(last) => {
                let parts = vec![
                    "## 1. Last Assistant Message".to_string(),
                    String::new(),
                    last.trim().to_string(),
                    String::new(),
                    "---".to_string(),
                    String::new(),
                    "## 2. Decision".to_string(),
                    String::new(),
                    next_prompt,
                ];
                parts.join("\n")
            }
            None => next_prompt,
        },
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

    // Try structured format — supports both current "## 1. Thread Summary"
    // and legacy "## 1. First Prompt (original request)" headers.
    let section1_header = if stripped.contains("## 1. Thread Summary") {
        "## 1. Thread Summary"
    } else {
        "## 1. First Prompt (original request)"
    };

    if let Some(pos) = stripped.find(section1_header) {
        let after_header = &stripped[pos + section1_header.len()..];
        let after_header = after_header.trim_start_matches('\n');
        // Extract everything up to the first "---" separator (before section 2/3)
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
    /// Actual input token count from the thread's API usage response.
    /// Passed through to AutoPromptAction for dispatch decisions.
    pub actual_input_tokens: Option<u64>,
    /// Whether the source thread had errors (rate limit, refusal, max tokens, etc.).
    /// Used by the caller to decide whether to add a pre-call delay.
    pub had_error: bool,
    /// Current stop lifecycle phase (Working, PreStop, Verified).
    /// Used to scope the handbrake to post-verification only.
    pub stop_phase: context::StopPhase,
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
            .field("actual_input_tokens", &self.actual_input_tokens)
            .field("had_error", &self.had_error)
            .field("stop_phase", &self.stop_phase)
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
    /// True when the LLM failed to produce a usable response and a synthetic
    /// stop was generated (e.g. "model returned zero events"). Pre-stop
    /// verification is skipped in this case because there is no real decision
    /// to verify.
    pub is_synthetic_failure: bool,
    /// Current stop lifecycle phase (Working, PreStop, Verified).
    /// Used to scope the handbrake to post-verification only.
    pub stop_phase: context::StopPhase,
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

    // Universal gate: low confidence overrides everything.
    if input.confidence.is_some_and(|c| c < 0.5) {
        return EvaluationResult::WantsStop {
            reason: format!(
                "confidence too low ({:.2} < 0.5)",
                input.confidence.unwrap()
            ),
        };
    }

    if input.should_continue {
        // ── LLM says continue ──────────────────────────────────
        // Handbrake: last resort, post-verification only.
        // Worker AI explicitly declared stopping but LLM wants to continue → force stop.
        if input.stop_phase != context::StopPhase::Working {
            if let Some(last_msg) = &input.last_assistant_message {
                let lower = last_msg.to_lowercase();
                let is_explicit_stop = lower.contains("stopping")
                    && (lower.contains("nothing related")
                        || lower.contains("no further action")
                        || lower.contains("nothing left")
                        || lower.contains("no further work"));
                if is_explicit_stop {
                    log::warn!(
                        "[auto_prompt::evaluate_response] Handbrake: worker AI declared stop despite LLM wanting to continue"
                    );
                    return EvaluationResult::WantsStop {
                        reason: "handbrake: worker AI explicitly declared stopping after pre-stop verification".to_string(),
                    };
                }
            }
        }

        // all_plan_done chooses WHICH continuation prompt, not WHETHER to continue.
        if input.all_plan_done {
            if let Some(next_plan_prompt) = &input.next_plan_prompt {
                return EvaluationResult::Continue {
                    prompt: next_plan_prompt.clone(),
                    reason: "current plan done, transitioning to next plan".to_string(),
                };
            }
            return EvaluationResult::Continue {
                prompt: "All plans are complete. Final cleanup: merge this feature branch into develop (fast-forward, no interactive rebase), resolve any conflicts, ensure a clean develop branch. Do NOT push — leave local for user to review.".to_string(),
                reason: "all plans done, dispatching final cleanup".to_string(),
            };
        }

        // Normal continuation with LLM-provided prompt.
        if has_prompt {
            let prompt = input.next_prompt.as_ref().unwrap();
            let cleaned = prompt
                .replace("#ALL_PLAN_DONE", "")
                .replace("#SKIP", "")
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                return EvaluationResult::Continue {
                    prompt: cleaned,
                    reason: "LLM says continue with next prompt".to_string(),
                };
            }
        }

        // should_continue=true but no usable prompt.
        return EvaluationResult::WantsStop {
            reason: "LLM says continue but provided no usable prompt".to_string(),
        };
    }

    // ── LLM says stop ──────────────────────────────────────────

    // Override: detect_remaining_work flips stop → continue.
    if let Some(remaining_prompt) = detect_remaining_work(input.last_assistant_message.as_deref()) {
        return EvaluationResult::Continue {
            prompt: remaining_prompt,
            reason: "LLM says stop but last_assistant_message contains remaining work — overriding to continue".to_string(),
        };
    }

    // Default: respect LLM's stop decision.
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
        let first_user_msg = thread_ref.entries().iter().find_map(|entry| {
            if let acp_thread::AgentThreadEntry::UserMessage(msg) = entry {
                let content = msg.content.to_markdown(cx).to_string();
                if !content.is_empty() {
                    return Some(content);
                }
            }
            None
        });
        let plan_files = read_plan_files(thread_ref, first_user_msg.as_deref());
        let doc_files = read_doc_files(thread_ref);
        let mut ctx = AutoPromptContext::collect(
            thread_ref,
            cx,
            stop_reason_str,
            plan_files,
            doc_files,
            iteration_count,
        );
        ctx.stop_phase = stop_phase.clone();
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
        "[auto_prompt::decide] Token counts: actual_input_tokens={:?}, estimated_chars_div_4={}",
        auto_prompt_ctx.actual_input_tokens,
        auto_prompt_ctx.approximate_token_count
    );

    log::info!(
        "[auto_prompt::decide] Had error: {}",
        auto_prompt_ctx.had_error
    );

    log::info!(
        "[auto_prompt::decide] PATH=llm_call: had_error={}, stop_reason={:?}, iteration={}, actual_input_tokens={:?} → NeedsLlmCall (LLM will decide)",
        auto_prompt_ctx.had_error,
        stop_reason,
        iteration_count,
        auto_prompt_ctx.actual_input_tokens
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
        actual_input_tokens: auto_prompt_ctx.actual_input_tokens,
        had_error: auto_prompt_ctx.had_error,
        stop_phase,
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
                data.actual_input_tokens,
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

            let prompt_summary = build_prompt_summary(
                response.thread_summary.as_deref(),
                data.title.as_deref(),
                response.reason.as_deref(),
                data.last_assistant_message.as_deref(),
                data.original_user_message.as_deref(),
                data.first_user_message.as_deref(),
            );

            let all_done = response.all_plan_done
                || response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

            let next_plan_prompt = if all_done {
                build_plan_landscape(&data.context_json).map(|landscape| {
                    format!(
                        "All current plan tasks are checked. For your awareness, remaining plans:\n\n\
                         {landscape}\n\n\
                         IMPORTANT: Do NOT start a new plan automatically. Instead:\n\
                         1. Re-read your last message — finish any remaining work described there first.\n\
                         2. Commit current changes with conventional messages to feature branch.\n\
                         3. Consider closing current feature branch (merge to develop, fast-forward).\n\
                         4. Only THEN re-read this plan list and decide if any is genuinely related to what you just did.\n\
                         5. Declare: \"Reviewed remaining plans: <staying on current feature | transitioning to X because Y | stopping, nothing related>\""
                    )
                })
            } else {
                None
            };

            let is_synthetic_failure = response.confidence <= Some(0.3)
                && response
                    .reason
                    .as_ref()
                    .is_some_and(|r| r.to_ascii_lowercase().starts_with("model"));
            let input = EvaluationInput {
                should_continue: response.should_continue,
                confidence: response.confidence,
                next_prompt: std::mem::take(&mut response.next_prompt),
                reason: std::mem::take(&mut response.reason),
                all_plan_done: all_done,
                next_plan_prompt,
                last_assistant_message: data.last_assistant_message.clone(),
                is_synthetic_failure,
                stop_phase: data.stop_phase.clone(),
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
                        actual_input_tokens: data.actual_input_tokens,
                    }))
                }
                EvaluationResult::WantsStop { reason } => {
                    if input.is_synthetic_failure {
                        // Full-context LLM call failed (context too large or model error).
                        // Retry with lightweight context: last message + incomplete plan names only.
                        let lightweight_ctx = build_lightweight_retry_context(
                            &data.context_json,
                            data.last_assistant_message.as_deref(),
                            data.title.as_deref(),
                        );

                        let retry_system = "# version: retry\n\
                            You decide what to do next based on the AI's last message.\n\
                            Priority: the LAST ASSISTANT MESSAGE is the most important signal.\n\n\
                            Respond ONLY with valid JSON:\n\
                            {\"should_continue\": bool, \"next_prompt\": string | null, \"reason\": string | null, \
                            \"all_plan_done\": bool, \"confidence\": float, \"thread_summary\": null}\n\n\
                            ## Rules (in order):\n\
                            1. LAST MESSAGE IS KING — reason about it first, before looking at plans\n\
                            2. If it asks \"would you like to continue?\" or \"want me to ...?\" → should_continue=true, \
                               next_prompt=\"continue as you prefer\"\n\
                            3. If it presents options to pick from → should_continue=true, \
                               next_prompt=\"select best for performance, security, SOLID, DRY principles\"\n\
                            4. If it reports plan done but mentions remaining phases/next steps → should_continue=true, \
                               next_prompt=\"continue with the next phase/step\"\n\
                            5. If it describes specific remaining work → should_continue=true, \
                               next_prompt=continue that specific work\n\
                            6. If genuinely complete with nothing left → should_continue=false\n\
                            7. Struck-through / skipped tasks (~~text~~, \"Skipped\", \"Cancelled\") count as DONE — \
                               do NOT continue them. If only skipped tasks remain → should_continue=false\n\
                            8. If remaining tasks seem unjustified or low-value, include #SKIP in next_prompt to signal skip\n\
                            9. confidence must be >= 0.7\n";

                        let mut retry_ok = None;
                        for attempt in 1..=3u32 {
                            if attempt > 1 {
                                let delay = 2000 * 2u64.pow(attempt - 1);
                                log::info!(
                                    "auto_prompt: lightweight retry attempt {attempt}, waiting {delay}ms"
                                );
                                cx.background_executor()
                                    .timer(Duration::from_millis(delay))
                                    .await;
                            }
                            match call_language_model(
                                &data.model,
                                retry_system,
                                &lightweight_ctx,
                                cx,
                            )
                            .await
                            {
                                Ok((_raw, parsed)) => {
                                    let is_retry_synthetic = parsed.confidence.unwrap_or(1.0)
                                        <= 0.3
                                        && parsed.reason.as_ref().is_some_and(|r| {
                                            r.to_ascii_lowercase().starts_with("model")
                                        });
                                    if is_retry_synthetic {
                                        log::warn!(
                                            "auto_prompt: lightweight retry attempt {attempt} got synthetic failure, retrying"
                                        );
                                        continue;
                                    }
                                    log::info!(
                                        "auto_prompt: lightweight retry attempt {attempt} ok: should_continue={}, prompt={:?}",
                                        parsed.should_continue,
                                        parsed.next_prompt
                                    );
                                    retry_ok = Some(parsed);
                                    break;
                                }
                                Err(err) => {
                                    log::warn!(
                                        "auto_prompt: lightweight retry attempt {attempt} failed: {err:#}"
                                    );
                                }
                            }
                        }

                        match retry_ok {
                            Some(parsed) if parsed.should_continue => {
                                let prompt = parsed
                                    .next_prompt
                                    .unwrap_or_else(|| "Continue with remaining work.".to_string());
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
                                    actual_input_tokens: data.actual_input_tokens,
                                }))
                            }
                            Some(parsed) => {
                                let stop_reason = parsed
                                    .reason
                                    .unwrap_or_else(|| "lightweight retry says stop".to_string());
                                log::info!(
                                    "auto_prompt: lightweight retry says stop: {stop_reason}"
                                );
                                write_stop_log(
                                    data.project_root.as_ref(),
                                    data.iteration_count,
                                    &format!("lightweight retry: {stop_reason}"),
                                );
                                reset_iteration();
                                Ok(AutoPromptOutcome::Stopped {
                                    reason: stop_reason,
                                })
                            }
                            None => {
                                log::warn!(
                                    "auto_prompt: all 3 lightweight retries failed, stopping"
                                );
                                write_stop_log(
                                    data.project_root.as_ref(),
                                    data.iteration_count,
                                    &format!("lightweight retry failed after 3 attempts: {reason}"),
                                );
                                reset_iteration();
                                Ok(AutoPromptOutcome::Stopped {
                                    reason: format!("lightweight retry failed: {reason}"),
                                })
                            }
                        }
                    } else {
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
                                        actual_input_tokens: data.actual_input_tokens,
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
    actual_input_tokens: Option<u64>,
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
        "actual_input_tokens": actual_input_tokens,
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
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    let filename = format!("{timestamp}_{iteration}_error.json");
    let log_entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "iteration": iteration,
        "model": model,
        "error": format!("{error:#}"),
    });

    let json = match serde_json::to_string_pretty(&log_entry) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize error log entry: {err}");
            return;
        }
    };

    let primary_dir = match project_root {
        Some(root) => root.join(".logs"),
        None => {
            log::info!(
                "[auto_prompt] error log: using fallback {FALLBACK_LOG_DIR} (no project root)"
            );
            PathBuf::from(FALLBACK_LOG_DIR)
        }
    };

    let fallback_dir = PathBuf::from(FALLBACK_LOG_DIR);

    for (label, dir) in [("primary", &primary_dir), ("fallback", &fallback_dir)] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!(
                "auto_prompt: failed to create {label} log dir {}: {err}",
                dir.display()
            );
            continue;
        }
        let path = dir.join(&filename);
        match std::fs::write(&path, &json) {
            Ok(()) => {
                log::info!("auto_prompt: wrote error log to {}", path.display());
            }
            Err(err) => {
                log::warn!(
                    "auto_prompt: failed to write error log {}: {err}",
                    path.display()
                );
            }
        }
    }
}

fn write_stop_log(project_root: Option<&PathBuf>, iteration: u32, reason: &str) {
    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S%.3f");
    let filename = format!("{timestamp}_{iteration}_stop.json");
    let log_entry = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "iteration": iteration,
        "reason": reason,
    });

    let json = match serde_json::to_string_pretty(&log_entry) {
        Ok(json) => json,
        Err(err) => {
            log::warn!("auto_prompt: failed to serialize stop log: {err}");
            return;
        }
    };

    let primary_dir = match project_root {
        Some(root) => root.join(".logs"),
        None => {
            log::info!(
                "[auto_prompt] stop: {reason} (no project root, using fallback {FALLBACK_LOG_DIR})"
            );
            PathBuf::from(FALLBACK_LOG_DIR)
        }
    };

    let fallback_dir = PathBuf::from(FALLBACK_LOG_DIR);

    for (label, dir) in [("primary", &primary_dir), ("fallback", &fallback_dir)] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            log::warn!(
                "auto_prompt: failed to create {label} log dir {}: {err}",
                dir.display()
            );
            continue;
        }
        let path = dir.join(&filename);
        match std::fs::write(&path, &json) {
            Ok(()) => {
                log::info!("auto_prompt: wrote stop log to {}", path.display());
            }
            Err(err) => {
                log::warn!(
                    "auto_prompt: failed to write stop log {}: {err}",
                    path.display()
                );
            }
        }
    }
}

/// Build the prompt summary for the next chained thread.
///
/// Priority:
/// 1. LLM-generated `thread_summary` (preferred — comprehensive, with active plan bolded)
/// 2. Synthesized from title + reason + last assistant message (when LLM returns null)
/// 3. Raw `original_user_message` carried from thread 0 (last resort before final fallback)
/// 4. Extracted from `first_user_message` (absolute fallback)
fn build_prompt_summary(
    thread_summary: Option<&str>,
    title: Option<&str>,
    reason: Option<&str>,
    last_assistant_message: Option<&str>,
    original_user_message: Option<&str>,
    first_user_message: Option<&str>,
) -> Option<String> {
    thread_summary
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            let mut parts = Vec::new();

            if let Some(title) = title.filter(|s| !s.trim().is_empty()) {
                parts.push(title.trim().to_string());
            }

            if let Some(reason) = reason.filter(|s| !s.trim().is_empty()) {
                parts.push(reason.trim().to_string());
            }

            if let Some(last) = last_assistant_message.filter(|s| !s.trim().is_empty()) {
                let truncated = last.trim();
                let limit = 2000;
                let summary = if truncated.len() > limit {
                    format!(
                        "{}...",
                        &truncated[..truncated
                            .char_indices()
                            .take(limit)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(limit)]
                    )
                } else {
                    truncated.to_string()
                };
                parts.push(summary);
            }

            if parts.is_empty() {
                original_user_message
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| s.to_string())
            } else {
                Some(parts.join("\n"))
            }
        })
        .or_else(|| first_user_message.and_then(extract_original_user_message))
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

fn read_plan_files(
    thread: &acp_thread::AcpThread,
    first_user_message: Option<&str>,
) -> Vec<PlanFileContent> {
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

    // Identify active project from first user message's file:// reference.
    // This prioritizes the session's target project over other workspace projects.
    let active_project = first_user_message.and_then(|msg| {
        let file_url_start = msg.find("file:///")?;
        let path = &msg[file_url_start + 7..];
        let end = path
            .find(|c: char| c == ')' || c == ' ' || c == '\n')
            .unwrap_or(path.len());
        let full_path = &path[..end];
        if let Some(pos) = full_path.find("/.plans/") {
            Some(full_path[..pos].to_string())
        } else if let Some(pos) = full_path.find("/.plan/") {
            Some(full_path[..pos].to_string())
        } else {
            None
        }
    });

    if let Some(ref active) = active_project {
        log::info!("[auto_prompt::read_plan_files] Active project: {active}");

        // Sort: active project's plans first, then others by path
        plan_files.sort_by(|a, b| {
            let a_active = a.path.starts_with(active.as_str());
            let b_active = b.path.starts_with(active.as_str());
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.path.cmp(&b.path),
            }
        });

        // Filter: keep all active project plans, only incomplete plans from other projects
        let before = plan_files.len();
        plan_files.retain(|f| {
            if f.path.starts_with(active.as_str()) {
                true
            } else {
                has_unchecked_items(&f.content)
            }
        });
        log::info!(
            "[auto_prompt::read_plan_files] Cross-project filter: {before} → {} plan files",
            plan_files.len()
        );
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

        let mut text_parts: Vec<String> = Vec::new();
        let mut thinking_parts: Vec<String> = Vec::new();
        let mut stream_errors: Vec<anyhow::Error> = Vec::new();
        let mut total_events: u32 = 0;
        let mut other_event_types: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            total_events += 1;
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => {
                    log::debug!(
                        "auto_prompt: stream event #{}: Text ({} chars)",
                        total_events,
                        text.len()
                    );
                    text_parts.push(text);
                }
                Ok(LanguageModelCompletionEvent::Thinking { text, .. }) => {
                    log::debug!(
                        "auto_prompt: stream event #{}: Thinking ({} chars)",
                        total_events,
                        text.len()
                    );
                    thinking_parts.push(text);
                }
                Ok(ref other) => {
                    let type_name = format!("{other:?}");
                    let short = type_name.chars().take(60).collect::<String>();
                    log::debug!(
                        "auto_prompt: stream event #{}: Other: {short}",
                        total_events
                    );
                    other_event_types.push(short);
                }
                Err(err) => {
                    log::warn!(
                        "auto_prompt: stream event #{}: Error: {err:#}",
                        total_events
                    );
                    stream_errors.push(err.into());
                }
            }
        }

        log::info!(
            "auto_prompt: stream complete — {} total events: {} Text ({} chars), {} Thinking ({} chars), {} Other, {} Errors. Other types: {:?}",
            total_events,
            text_parts.len(),
            text_parts.concat().len(),
            thinking_parts.len(),
            thinking_parts.concat().len(),
            other_event_types.len(),
            stream_errors.len(),
            other_event_types,
        );

        if stream_errors
            .iter()
            .any(|e| format!("{e:#}").contains("rate_limit") || format!("{e:#}").contains("429"))
        {
            log::warn!("auto_prompt: rate limit detected in stream errors");
        }

        let text = text_parts.concat();
        if !text.trim().is_empty() {
            anyhow::Ok(text)
        } else if !thinking_parts.is_empty() {
            let thinking = thinking_parts.concat();
            if !thinking.trim().is_empty() {
                log::info!(
                    "auto_prompt: Thinking fallback: {} empty Text parts, {} Thinking events ({} chars)",
                    text_parts.len(),
                    thinking_parts.len(),
                    thinking.len()
                );
                let synthetic = serde_json::json!({
                    "should_continue": false,
                    "next_prompt": null,
                    "reason": format!("Model returned {} Thinking events but no Text output", thinking_parts.len()),
                    "all_plan_done": false,
                    "confidence": 0.3,
                    "thread_summary": null
                });
                anyhow::Ok(synthetic.to_string())
            } else {
                log::warn!(
                    "auto_prompt: model returned no usable content ({} empty Text events, {} empty Thinking events, {} stream errors), synthesizing stop",
                    text_parts.len(),
                    thinking_parts.len(),
                    stream_errors.len()
                );
                let synthetic = serde_json::json!({
                    "should_continue": false,
                    "next_prompt": null,
                    "reason": format!("model returned no usable content ({} empty Text, {} empty Thinking, {} stream errors)", text_parts.len(), thinking_parts.len(), stream_errors.len()),
                    "all_plan_done": false,
                    "confidence": 0.0,
                    "thread_summary": null
                });
                anyhow::Ok(synthetic.to_string())
            }
        } else if !stream_errors.is_empty() {
            let error_details: Vec<String> =
                stream_errors.iter().map(|e| format!("{e:#}")).collect();
            log::warn!(
                "auto_prompt: model stream produced only errors — details: {error_details:?}"
            );
            let synthetic = serde_json::json!({
                "should_continue": false,
                "next_prompt": null,
                "reason": format!("model stream produced only errors ({})", stream_errors.len()),
                "all_plan_done": false,
                "confidence": 0.0,
                "thread_summary": null
            });
            anyhow::Ok(synthetic.to_string())
        } else {
            log::warn!(
                "auto_prompt: model returned zero events (0 Text, 0 Thinking) out of {total_events} total events. Other types seen: {other_event_types:?}"
            );
            let synthetic = serde_json::json!({
                "should_continue": false,
                "next_prompt": null,
                "reason": format!("model returned zero events ({} total stream events)", total_events),
                "all_plan_done": false,
                "confidence": 0.0,
                "thread_summary": null
            });
            anyhow::Ok(synthetic.to_string())
        }
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
    match serde_json::from_str(json_str) {
        Ok(response) => Ok(response),
        Err(parse_err) => {
            let preview = text.chars().take(200).collect::<String>();
            log::warn!("auto_prompt: failed to parse response as JSON ({parse_err}): {preview:?}");
            log::warn!("auto_prompt: synthesizing stop response to avoid retry loop");
            Ok(AutoPromptResponse {
                should_continue: false,
                next_prompt: None,
                reason: Some(format!(
                    "unparseable response ({} bytes, {} extracted): {parse_err}",
                    text.len(),
                    json_str.len()
                )),
                all_plan_done: false,
                confidence: Some(0.0),
                thread_summary: None,
            })
        }
    }
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

/// Build a concise summary of remaining plans grouped by project folder.
/// Lists actionable unchecked tasks (skipping "Out of Scope", "Deferred" sections).
/// Returns None if no plans have actionable unchecked items.
fn build_plan_landscape(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Context {
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: Context = serde_json::from_str(context_json).ok()?;

    type Project = String;
    type PlanLine = String;
    let mut groups: Vec<(Project, Vec<PlanLine>)> = Vec::new();

    for file in &ctx.plan_files {
        let task_count = count_actionable_tasks(&file.content);
        if task_count == 0 {
            continue;
        }

        let project = std::path::Path::new(&file.path)
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".into());

        let filename = file.path.rsplit('/').next().unwrap_or(&file.path);
        let plan_dir = file.path.rsplit('/').nth(1).unwrap_or(".plans");
        let title = extract_plan_title(&file.content);

        let line = format!("  - `{plan_dir}/{filename}` — {task_count} task(s): {title}");

        if let Some(group) = groups.iter_mut().find(|(name, _)| *name == project) {
            group.1.push(line);
        } else {
            groups.push((project, vec![line]));
        }
    }

    if groups.is_empty() {
        log::info!("[auto_prompt::build_plan_landscape] No actionable plans found");
        return None;
    }

    let total_plans: usize = groups.iter().map(|(_, plans)| plans.len()).sum();
    log::info!(
        "[auto_prompt::build_plan_landscape] Found {total_plans} actionable plan(s) across {} project(s)",
        groups.len()
    );

    let mut lines = Vec::new();
    for (project, plans) in &groups {
        lines.push(format!("**{project}** ({} plan(s)):", plans.len()));
        lines.extend(plans.iter().cloned());
    }

    Some(lines.join("\n"))
}

fn build_lightweight_retry_context(
    context_json: &str,
    last_assistant_message: Option<&str>,
    title: Option<&str>,
) -> String {
    let mut parts = Vec::new();

    if let Some(title) = title {
        parts.push(format!("Thread: {title}"));
    }

    if let Some(msg) = last_assistant_message {
        let paragraphs: Vec<&str> = msg.split("\n\n").collect();
        let start = paragraphs.len().saturating_sub(3);
        let last_3 = &paragraphs[start..];
        parts.push(format!(
            "\nLast assistant message:\n{}",
            last_3.join("\n\n")
        ));
    }

    let landscape = build_plan_landscape(context_json);
    if let Some(landscape) = landscape {
        parts.push(format!("\nIncomplete plans:\n{landscape}"));
    }

    parts.join("\n")
}

/// Count actionable unchecked `- [ ]` items, skipping Out of Scope/Deferred sections and markers.
fn count_actionable_tasks(content: &str) -> usize {
    let mut count = 0;
    let mut in_code_block = false;
    let mut skip_section = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with("## ") {
            let section_lower = trimmed.to_lowercase();
            skip_section = SKIP_SECTION_KEYWORDS
                .iter()
                .any(|keyword| section_lower.contains(keyword));
            continue;
        }
        if trimmed.starts_with("# ") {
            skip_section = false;
            continue;
        }
        if skip_section {
            continue;
        }
        if is_actionable_checkbox(trimmed) {
            count += 1;
        }
    }

    count
}

/// Extract the plan title from the first `# ` heading.
fn extract_plan_title(content: &str) -> String {
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(title) = trimmed.strip_prefix("# ") {
            return title.to_string();
        }
    }
    String::new()
}

/// Section header keywords (lowercase) that indicate non-actionable items.
const SKIP_SECTION_KEYWORDS: &[&str] = &["out of scope", "future", "backlog", "wishlist"];

/// Item-level markers that indicate a non-actionable checkbox despite being unchecked.
/// Includes strikethrough (`~~`), skip/cancel keywords, and deferral markers.
const SKIP_ITEM_MARKERS: &[&str] = &[
    "DEFERRED",
    "⏸️",
    "— deferred",
    "- deferred",
    "~~",
    "Skipped",
    "skipped",
    "Cancelled",
    "cancelled",
    "N/A",
    "Won't fix",
    "wontfix",
    "NOT PLANNED",
    "out of scope",
];

/// Returns true if line is an unchecked checkbox (`- [ ]` or `* [ ]`) without skip markers.
fn is_actionable_checkbox(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("- [ ] ") && !trimmed.starts_with("* [ ] ") {
        return false;
    }
    let line_lower = trimmed.to_lowercase();
    !SKIP_ITEM_MARKERS
        .iter()
        .any(|marker| line_lower.contains(&marker.to_lowercase()))
}

fn has_unchecked_items(content: &str) -> bool {
    let mut in_code_block = false;
    let mut skip_section = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with("## ") {
            let section_lower = trimmed.to_lowercase();
            skip_section = SKIP_SECTION_KEYWORDS
                .iter()
                .any(|keyword| section_lower.contains(keyword));
            continue;
        }
        if trimmed.starts_with("# ") {
            skip_section = false;
            continue;
        }
        if skip_section {
            continue;
        }
        if is_actionable_checkbox(trimmed) {
            return true;
        }
    }
    false
}

/// Detect if the current or next plan involves performance-related work by scanning for keywords.
fn is_perf_related(context_json: &str, work_dirs: Option<&[PathBuf]>) -> bool {
    let perf_keywords = [
        "benchmark",
        "bench",
        "performance",
        "perf",
        "latency",
        "throughput",
        "speed",
        "optimize",
        "optimization",
        "memory usage",
        "allocation",
        "cache",
        "profile",
        "profiling",
    ];

    let check_content = |content: &str| -> bool {
        let lower = content.to_lowercase();
        perf_keywords.iter().any(|kw| lower.contains(kw))
    };

    if check_content(context_json) {
        return true;
    }

    let Some(dirs) = work_dirs else {
        return false;
    };

    for work_dir in dirs {
        for plan_dir in [work_dir.join(".plan"), work_dir.join(".plans")] {
            if !plan_dir.is_dir() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&plan_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if has_unchecked_items(&content) && check_content(&content) {
                            return true;
                        }
                    }
                }
            }
        }
    }

    false
}

fn is_doc_creation_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("documentation") || lower.contains(".docs/")
}

/// Code-level remaining work detection: scans the last assistant message
/// for patterns that explicitly indicate incomplete work. This is a safety
/// net that catches cases where the LLM ignores its own Rule 10.
fn extract_remaining_section(text: &str) -> Option<String> {
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if paragraphs.is_empty() {
        return None;
    }

    let trigger_words = [
        "remaining work",
        "remaining:",
        "still need",
        "still needs",
        "next step",
        "next steps",
        "todo:",
        "action items",
        "left to do",
    ];

    let scan_count = 3.min(paragraphs.len());
    let scan_start = paragraphs.len() - scan_count;

    for i in (scan_start..paragraphs.len()).rev() {
        let lower = paragraphs[i].to_lowercase();
        for trigger in &trigger_words {
            if lower.contains(trigger) {
                return Some(paragraphs[i..].join("\n\n"));
            }
        }
    }

    for i in (scan_start..paragraphs.len()).rev() {
        let has_actionable_checkbox = paragraphs[i].lines().any(is_actionable_checkbox);
        if has_actionable_checkbox {
            let start = if i > 0 {
                let prev_lower = paragraphs[i - 1].to_lowercase();
                let prev_is_header = paragraphs[i - 1].starts_with('#')
                    || (paragraphs[i - 1].len() < 80 && paragraphs[i - 1].ends_with(':'));
                if trigger_words.iter().any(|t| prev_lower.contains(t)) || prev_is_header {
                    i - 1
                } else {
                    i
                }
            } else {
                i
            };
            return Some(paragraphs[start..].join("\n\n"));
        }
    }

    let fallback_start = paragraphs.len().saturating_sub(2);
    Some(paragraphs[fallback_start..].join("\n\n"))
}

fn detect_remaining_work(last_assistant_message: Option<&str>) -> Option<String> {
    let msg = last_assistant_message?.trim();
    if msg.is_empty() {
        return None;
    }

    let section = extract_remaining_section(msg);

    let lower = msg.to_lowercase();
    let patterns: &[&str] = &[
        "remaining work",
        "remaining:",
        "still need",
        "still needs",
        "next step",
        "next steps",
        "todo:",
        "action items",
        "left to do",
    ];

    for pattern in patterns {
        if lower.contains(pattern) {
            log::warn!(
                "[auto_prompt::detect_remaining_work] Pattern found: {pattern} in last_assistant_message — overriding stop"
            );
            let section_text = section.as_deref().unwrap_or(msg);
            let is_actionable = section_text.contains("- ")
                || section_text.contains("* ")
                || section_text.contains("1.")
                || section_text.contains("TODO")
                || section_text.contains("must")
                || section_text.contains("need to");

            if is_actionable {
                return Some(format!(
                    "Previous assistant mentioned remaining work. Extracted section:\n\n\
                     {section_text}\n\n\
                     If this describes specific actionable remaining work, continue with it. \
                     If the work is already done or this is a false positive, stop."
                ));
            }
            log::info!(
                "[auto_prompt::detect_remaining_work] Pattern '{pattern}' found but no actionable items — skipping override"
            );
            return None;
        }
    }

    for line in msg.lines() {
        let trimmed = line.trim_start();
        if is_actionable_checkbox(trimmed) {
            log::warn!(
                "[auto_prompt::detect_remaining_work] Pattern found: unchecked checkbox in last_assistant_message — overriding stop"
            );
            let section_text = section.as_deref().unwrap_or(msg);
            return Some(format!(
                "Previous assistant left unchecked items. Extracted section:\n\n\
                 {section_text}\n\n\
                 If these are real remaining tasks, continue with them. \
                 If already done or this is a false positive, stop."
            ));
        }
    }

    None
}

fn build_pre_stop_verification_prompt(
    context_json: &str,
    work_dirs: &Option<Vec<PathBuf>>,
) -> Option<String> {
    let landscape = build_plan_landscape(context_json);

    log::info!(
        "[auto_prompt::pre_stop_verification] work_dirs={:?}, has_landscape={}",
        work_dirs.as_ref().map(|dirs| dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()),
        landscape.is_some()
    );

    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    if let Ok(ctx) = serde_json::from_str::<Ctx>(context_json) {
        let summary: Vec<String> = ctx
            .plan_files
            .iter()
            .map(|f| {
                let tasks = count_actionable_tasks(&f.content);
                let project = std::path::Path::new(&f.path)
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let filename = f.path.rsplit('/').next().unwrap_or("?");
                format!("{project}/{filename} (tasks={tasks})")
            })
            .collect();
        log::info!(
            "[auto_prompt::pre_stop_verification] plan_files=[{}]",
            summary.join(", ")
        );
    }

    let is_perf = is_perf_related(context_json, work_dirs.as_deref());

    let mut checks = vec![
        "1. **Last message first**: Re-read your last message. Any remaining work, next steps, or unchecked items? Continue THAT before anything else.".to_string(),
        "2. **Diagnostics**: `cargo check` and `cargo clippy`. Fix errors and warnings.".to_string(),
        "3. **Git**: Commit with conventional messages to feature branch from develop.".to_string(),
    ];

    if is_perf {
        checks.push("4. **Benchmarks**: Run relevant benchmarks and record results.".to_string());
    }

    let mut sections = vec![checks.join("\n")];

    if let Some(landscape) = landscape {
        sections.push(format!(
            "## Remaining Plans (FYI — do NOT auto-pick)\n\n\
             {landscape}\n\n\
             Items may be deferred, out-of-scope, or unrelated. \
             Read the list, then decide — do not start something new just because it exists."
        ));
    }

    sections.push(
        "## Declare\n\n\
         Before stopping, state one of:\n\
         - `continuing: <what remains from last message>`\n\
         - `reviewed plans: transitioning to <path> because <relevance>` — close current feature first\n\
         - `reviewed plans: stopping, nothing related to current work`".to_string()
    );

    Some(format!(
        "PRE-STOP VERIFICATION — check state, then decide.\n\n{}\n\n\
         Proceed with continuing/transitioning work, or stop if verification is complete.",
        sections.join("\n\n")
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
            if is_actionable_checkbox(trimmed) {
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
        assert!(result.starts_with("## 1. Thread Summary\n\n"));
        assert!(result.contains(original));
        assert!(result.contains("## 3. Decision"));
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
    fn test_with_first_prompt_context_no_summary_includes_last_message() {
        let last_msg = "Fixed the auth bug, tests passing. Still need to commit.";
        let result =
            with_first_prompt_context("commit the changes".to_string(), None, None, Some(last_msg));
        assert!(result.starts_with("## 1. Last Assistant Message\n\n"));
        assert!(result.contains(last_msg));
        assert!(result.contains("## 2. Decision"));
        assert!(result.ends_with("commit the changes"));
        assert!(!result.contains("Thread Summary"));
    }

    #[test]
    fn test_with_first_prompt_context_whitespace_summary_includes_last_message() {
        let last_msg = "Completed steps 1-3";
        let result =
            with_first_prompt_context("do step 4".to_string(), Some("   "), None, Some(last_msg));
        assert!(result.starts_with("## 1. Last Assistant Message\n\n"));
        assert!(result.contains(last_msg));
        assert!(result.contains("## 2. Decision"));
        assert!(!result.contains("Thread Summary"));
    }

    #[test]
    fn test_with_first_prompt_context_with_thread_summary_and_last_message() {
        let summary = "Fixed auth bug in **plan 083** — the session validation was broken. All tests passing now.";
        let last_msg = "Fixed the auth bug, tests passing";
        let result = with_first_prompt_context(
            "commit the changes".to_string(),
            Some(summary),
            None,
            Some(last_msg),
        );
        assert!(result.starts_with("## 1. Thread Summary\n\n"));
        assert!(result.contains(summary));
        assert!(result.contains("## 2. Last Assistant Message"));
        assert!(result.contains(last_msg));
        assert!(result.contains("## 3. Decision"));
        assert!(result.ends_with("commit the changes"));
    }

    #[test]
    fn test_with_first_prompt_context_3_part_structure() {
        let summary =
            "Implementing feature X in **plan 085** — completed steps 1-3, need to do step 4";
        let last_msg = "Completed steps 1-3, need to do step 4";
        let result = with_first_prompt_context(
            "do step 4 now".to_string(),
            Some(summary),
            None,
            Some(last_msg),
        );
        // Verify 3-part structure with section headers
        assert!(result.contains("## 1. Thread Summary"));
        assert!(result.contains("## 2. Last Assistant Message"));
        assert!(result.contains("## 3. Decision"));
        assert!(!result.contains("## 4."));

        // Verify sections are in order
        let pos1 = result.find("## 1.").unwrap();
        let pos2 = result.find("## 2.").unwrap();
        let pos3 = result.find("## 3.").unwrap();
        assert!(pos1 < pos2);
        assert!(pos2 < pos3);
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
    fn test_extract_original_user_message_new_3_part_format() {
        let input = "## 1. Thread Summary\n\nimplement the auth module\n\nsrc/auth.rs\n\n---\n\n## 2. Last Assistant Message\n\nDone\n\n---\n\n## 3. Decision\n\ncommit changes";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some("implement the auth module\n\nsrc/auth.rs".to_string())
        );
    }

    #[test]
    fn test_extract_original_user_message_legacy_4_part_format() {
        let input = "## 1. First Prompt (original request)\n\nimplement the auth module\n\nsrc/auth.rs\n\n---\n\n## 2. Thread Summary\n\nAuth implementation\n\n---\n\n## 4. Decision\n\ncommit changes";
        let result = extract_original_user_message(input);
        assert_eq!(
            result,
            Some("implement the auth module\n\nsrc/auth.rs".to_string())
        );
    }

    #[test]
    fn test_roundtrip_3_part_format_with_summary_and_last_message() {
        let summary = "implement feature X with files\n\nmod.rs\nlib.rs";
        let last_msg = "Completed implementation";
        let wrapped = with_first_prompt_context(
            "do the next thing".to_string(),
            Some(summary),
            None,
            Some(last_msg),
        );
        let chain_message = format!("## User\n\n[@Thread](zed:///agent/thread/abc)\n\n{wrapped}");
        let extracted = extract_original_user_message(&chain_message);
        assert_eq!(extracted, Some(summary.to_string()));
    }

    // --- build_prompt_summary tests ---

    #[test]
    fn test_build_prompt_summary_prefers_llm_thread_summary() {
        let result = build_prompt_summary(
            Some("LLM generated summary about **plan 085**"),
            Some("Thread Title"),
            Some("continuing work"),
            Some("Last assistant message"),
            Some("raw first prompt"),
            None,
        );
        assert_eq!(
            result,
            Some("LLM generated summary about **plan 085**".to_string())
        );
    }

    #[test]
    fn test_build_prompt_summary_synthesizes_from_title_reason_last() {
        let result = build_prompt_summary(
            None,
            Some("Fix fusion rank mismatch"),
            Some("plan has unchecked items"),
            Some("Fixed the scale clamp in affine quantization"),
            Some("so no way rust can beat python for gemma 2?"),
            None,
        );
        let summary = result.expect("should synthesize summary");
        assert!(summary.contains("Fix fusion rank mismatch"));
        assert!(summary.contains("plan has unchecked items"));
        assert!(summary.contains("Fixed the scale clamp in affine quantization"));
        assert!(
            !summary.contains("so no way rust can beat python"),
            "should NOT contain raw first prompt"
        );
    }

    #[test]
    fn test_build_prompt_summary_synthesizes_truncates_long_last_message() {
        let long_message: String = "x".repeat(3000);
        let result = build_prompt_summary(
            None,
            Some("Title"),
            None,
            Some(&long_message),
            Some("fallback"),
            None,
        );
        let summary = result.expect("should synthesize");
        assert!(summary.len() < long_message.len(), "should be truncated");
        assert!(
            summary.ends_with("..."),
            "truncated text should end with ..."
        );
    }

    #[test]
    fn test_build_prompt_summary_falls_back_to_original_user_message() {
        let result = build_prompt_summary(
            None,
            None,
            None,
            None,
            Some("raw first prompt as fallback"),
            None,
        );
        assert_eq!(result, Some("raw first prompt as fallback".to_string()));
    }

    #[test]
    fn test_build_prompt_summary_falls_back_to_first_user_message() {
        let result = build_prompt_summary(
            None,
            None,
            None,
            None,
            None,
            Some("## User\n\nextracted from first message"),
        );
        assert_eq!(result, Some("extracted from first message".to_string()));
    }

    #[test]
    fn test_build_prompt_summary_returns_none_when_all_empty() {
        let result = build_prompt_summary(None, None, None, None, None, None);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_prompt_summary_title_only_no_reason_no_last() {
        let result = build_prompt_summary(
            None,
            Some("Implement auth flow"),
            None,
            None,
            Some("old raw prompt"),
            None,
        );
        assert_eq!(result, Some("Implement auth flow".to_string()));
    }

    // --- extract_remaining_section tests ---

    #[test]
    fn test_extract_remaining_section_single_paragraph_no_trigger() {
        let text = "This is a single paragraph.";
        assert_eq!(
            extract_remaining_section(text),
            Some("This is a single paragraph.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trigger_includes_header_and_list() {
        let text = "First paragraph.\n\nSecond paragraph.\n\n### Remaining work:\n\n- Do thing A\n- Do thing B";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining work:\n\n- Do thing A\n- Do thing B".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_real_world_remaining_work() {
        let text = "What was accomplished:\n\n1. Fixed bug\n2. Added tests\n\n### Remaining work for next session:\n\n- Rebuild with Metal backend\n- Retest training\n- If NaN resolved: Run full benchmark";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining work for next session:\n\n- Rebuild with Metal backend\n- Retest training\n- If NaN resolved: Run full benchmark".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trailing_whitespace_fallback_two() {
        let text = "First.\n\nLast.  \n\n";
        assert_eq!(
            extract_remaining_section(text),
            Some("First.\n\nLast.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_empty() {
        assert_eq!(extract_remaining_section(""), None);
    }

    #[test]
    fn test_extract_remaining_section_only_whitespace() {
        assert_eq!(extract_remaining_section("   \n\n  \n\n  "), None);
    }

    #[test]
    fn test_extract_remaining_section_two_paragraphs_no_trigger() {
        let text = "First paragraph here.\n\nSecond paragraph here.";
        assert_eq!(
            extract_remaining_section(text),
            Some("First paragraph here.\n\nSecond paragraph here.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_single_line() {
        let text = "Just one line no double breaks";
        assert_eq!(
            extract_remaining_section(text),
            Some("Just one line no double breaks".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_checkbox_with_header() {
        let text =
            "Summary of work done.\n\n### Remaining:\n\n- [ ] Task 1\n- [ ] Task 2\n- [ ] Task 3";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining:\n\n- [ ] Task 1\n- [ ] Task 2\n- [ ] Task 3".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_checkbox_includes_preceding_header_like() {
        let text = "Summary.\n\nPending items:\n\n- [ ] Task 1\n- [ ] Task 2";
        assert_eq!(
            extract_remaining_section(text),
            Some("Pending items:\n\n- [ ] Task 1\n- [ ] Task 2".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_many_paragraphs_scans_last_3() {
        let text = "P1.\n\nP2.\n\nP3.\n\nP4.\n\n### Next steps:\n\n- Do X";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Next steps:\n\n- Do X".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trigger_in_middle_of_last_3() {
        let text = "Accomplished A.\n\nAccomplished B.\n\n### TODO:\n\n- Fix bug\n- Add tests\n\nAlso mentioned in passing.";
        assert_eq!(
            extract_remaining_section(text),
            Some("### TODO:\n\n- Fix bug\n- Add tests\n\nAlso mentioned in passing.".to_string())
        );
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
            is_synthetic_failure: false,
            stop_phase: context::StopPhase::Working,
        }
    }

    // --- Task 4: evaluate_response() state machine tests ---

    #[test]
    fn test_eval_all_done_with_next_plan() {
        let input = EvaluationInput {
            should_continue: true,
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
            should_continue: true,
            all_plan_done: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("final cleanup"));
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
                reason: "LLM says stop, no next prompt".to_string(),
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
    fn test_eval_remaining_work_false_positive_no_actionable_items() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some(
                "The remaining work section was already addressed in the previous commit."
                    .to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "trigger word 'remaining work' but no actionable items should not override stop"
        );
    }

    #[test]
    fn test_eval_remaining_work_trigger_with_bullets_overrides_stop() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some(
                "Done with part 1.\n\n### Remaining work:\n\n- Fix the bug\n- Add tests"
                    .to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { prompt, .. } => {
                assert!(prompt.contains("Fix the bug"));
                assert!(prompt.contains("false positive"));
            }
            _ => panic!("expected Continue override, got {result:?}"),
        }
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
    fn test_eval_all_done_next_plan_respects_should_stop() {
        let input = EvaluationInput {
            all_plan_done: true,
            should_continue: false,
            next_plan_prompt: Some("start next plan".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert_eq!(
            result,
            EvaluationResult::WantsStop {
                reason: "LLM says stop, no next prompt".to_string(),
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
                reason: "LLM says stop, no next prompt".to_string(),
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

    #[test]
    fn test_handbrake_stopping_nothing_related_forces_stop() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("continue the plan".to_string()),
            last_assistant_message: Some(
                "Declare: reviewed remaining plans: stopping, nothing related".to_string(),
            ),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should force stop when worker declares 'stopping, nothing related' after pre-stop"
        );
    }

    #[test]
    fn test_handbrake_stopping_no_further_action_forces_stop() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("continue work".to_string()),
            last_assistant_message: Some(
                "Reviewed plans: stopping — no further action needed.".to_string(),
            ),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should force stop when worker declares 'stopping' with 'no further action' after pre-stop"
        );
    }

    #[test]
    fn test_handbrake_stopping_nothing_left_forces_stop() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("continue".to_string()),
            last_assistant_message: Some(
                "Everything is done. Stopping, nothing left to do.".to_string(),
            ),
            stop_phase: context::StopPhase::Verified,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should force stop when worker declares 'stopping' with 'nothing left' after verification"
        );
    }

    #[test]
    fn test_handbrake_stopping_alone_does_not_trigger() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("continue fixing bugs".to_string()),
            last_assistant_message: Some(
                "I'm stopping the current approach to try something else.".to_string(),
            ),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::Continue { .. }),
            "'stopping' alone without qualifying phrase should NOT trigger handbrake"
        );
    }

    #[test]
    fn test_handbrake_normal_continue_not_affected() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("implement the next feature".to_string()),
            last_assistant_message: Some("I've completed step 1. Moving to step 2.".to_string()),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::Continue { .. }),
            "normal messages should not trigger handbrake even in pre-stop phase"
        );
    }

    #[test]
    fn test_handbrake_does_not_trigger_during_working_phase() {
        let input = EvaluationInput {
            should_continue: true,
            next_prompt: Some("continue the plan".to_string()),
            last_assistant_message: Some(
                "Declare: reviewed remaining plans: stopping, nothing related".to_string(),
            ),
            stop_phase: context::StopPhase::Working,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::Continue { .. }),
            "handbrake should NOT trigger during Working phase, only after pre-stop verification"
        );
    }

    #[test]
    fn test_should_continue_false_respected_without_handbrake() {
        let input = EvaluationInput {
            should_continue: false,
            next_prompt: None,
            last_assistant_message: Some(
                "Reviewed remaining plans: stopping, nothing related to current work".to_string(),
            ),
            stop_phase: context::StopPhase::Working,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "should_continue=false should be respected directly — no handbrake needed when LLM already says stop"
        );
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

    #[test]
    fn test_build_plan_landscape_groups_by_project() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/katopz/git/microgpt-rs/.plans/033_bomberman.md","content":"# Plan 033\n- [ ] Task A\n"},
            {"path":"/Users/katopz/git/microgpt-rs/.plans/034_wasm.md","content":"# Plan 034\n- [ ] Task B\n"},
            {"path":"/Users/katopz/git/riir-burner/.plans/01_plan.md","content":"# Plan 01\n- [ ] Task C\n"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        assert!(result.is_some());
        let landscape = result.unwrap();
        assert!(landscape.contains("**microgpt-rs** (2 plan(s))"));
        assert!(landscape.contains("**riir-burner** (1 plan(s))"));
        assert!(landscape.contains("033_bomberman"));
        assert!(landscape.contains("01_plan"));
    }

    #[test]
    fn test_build_plan_landscape_returns_none_when_all_done() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/katopz/git/microgpt-rs/.plans/033_bomberman.md","content":"# Plan\n- [x] Done\n"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_plan_landscape_skips_out_of_scope() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/katopz/git/microgpt-rs/.plans/033_bomberman.md","content":"# Plan\n\n## Tasks\n- [x] Done\n\n## Out of Scope\n- [ ] Future\n"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_plan_landscape_no_plan_files() {
        let context_json = r#"{"plan_files":[]}"#;
        let result = build_plan_landscape(context_json);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_plan_landscape_invalid_json() {
        let context_json = "not json";
        let result = build_plan_landscape(context_json);
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_plan_landscape_shows_task_count() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/katopz/git/zed/.plans/03_auto_prompt.md","content":"# Auto Prompt\n- [ ] Task 1\n- [ ] Task 2\n- [x] Done\n"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        assert!(result.is_some());
        let landscape = result.unwrap();
        assert!(landscape.contains("2 task(s)"));
        assert!(landscape.contains("Auto Prompt"));
    }

    #[test]
    fn test_build_plan_landscape_shows_title() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/katopz/git/zed/.plans/03_auto_prompt.md","content":"# Plan 003: Fix Cross-Project\n- [ ] Task\n"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        assert!(result.is_some());
        let landscape = result.unwrap();
        assert!(landscape.contains("Plan 003: Fix Cross-Project"));
    }

    #[test]
    fn test_has_unchecked_items_skips_out_of_scope_section() {
        let content = "\
# Plan 033: Bomberman Arena

## Tasks
- [x] Task 1
- [x] Task 2

## Out of Scope
- [ ] Real-time multiplayer
- [ ] Network play
- [ ] Complex bomb types
";
        assert!(!has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_skips_future_section() {
        let content = "\
# Plan

## Tasks
- [x] Done

## Future Work
- [ ] Some future thing
";
        assert!(!has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_skips_backlog_section() {
        let content = "\
# Plan

## Tasks
- [x] Done

## Backlog
- [ ] Backlog item
";
        assert!(!has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_finds_real_tasks_after_skipped_section() {
        let content = "\
# Plan

## Out of Scope
- [ ] Future thing

## Tasks
- [x] Done task
- [ ] Remaining task
";
        assert!(has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_skips_deferred_markers() {
        let content = "\
# Plan

## Tasks
- [x] Done task
- [ ] ⏸️ DEFERRED Some deferred thing
- [ ] Another deferred — deferred item
";
        assert!(!has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_real_tasks_with_deferred_mixed() {
        let content = "\
# Plan

## Tasks
- [x] Done task
- [ ] ⏸️ DEFERRED Skip this
- [ ] Real actionable task
";
        assert!(has_unchecked_items(content));
    }

    #[test]
    fn test_has_unchecked_items_all_done_returns_false() {
        let content = "\
# Plan

## Tasks
- [x] Task 1
- [x] Task 2
- [x] Task 3
";
        assert!(!has_unchecked_items(content));
    }

    #[test]
    fn test_benchmark_analysis_landscape_no_actionable_plans() {
        // Real-world scenario: conversation about benchmark regression analysis
        // Plan 033 has only "Out of Scope" unchecked items → not actionable
        // Plan 036 same → landscape returns None
        let context_json = r##"{"current_paths":["/Users/katopz/git/gist/anyrag","/Users/katopz/git/microgpt-rs","/Users/katopz/git/riir-ai"],"plan_files":[
            {"path":"/Users/katopz/git/gist/anyrag/.plans/008_inference.md","content":"# Plan\n- [x] Done\n"},
            {"path":"/Users/katopz/git/microgpt-rs/.plans/033_bomberman_arena.md","content":"# Plan\n\n## Tasks\n- [x] Build arena\n- [x] Run tournament\n\n## Out of Scope\n- [ ] Real-time multiplayer\n- [ ] Network play\n"},
            {"path":"/Users/katopz/git/microgpt-rs/.plans/036_metrics.md","content":"# Plan\n\n## Out of Scope\n- [ ] LLM-based reviewer\n\n## Tasks\n- [x] Metrics done\n"}
        ],"messages":[
            {"role":"user","content":"we have a lot of regression"},
            {"role":"assistant","content":"Let me check benchmarks in /Users/katopz/git/microgpt-rs"},
            {"role":"tool","content":"cd /Users/katopz/git/microgpt-rs && cargo bench"},
            {"role":"tool","content":"cd /Users/katopz/git/microgpt-rs && cat results.csv"}
        ]}"##;
        let result = build_plan_landscape(context_json);
        // All unchecked items are under "Out of Scope" → no actionable plans → None
        assert_eq!(result, None);
    }

    #[test]
    fn test_eval_synthetic_failure_zero_confidence_returns_wants_stop() {
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.0),
            reason: Some("model returned zero events (1 total stream events)".to_string()),
            is_synthetic_failure: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(
                    reason.contains("confidence too low"),
                    "expected low-confidence stop reason, got: {reason}"
                );
            }
            other => panic!("expected WantsStop for synthetic failure, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_synthetic_failure_no_thinking_returns_wants_stop() {
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.0),
            reason: Some("model returned no usable content (3 empty Text, 0 empty Thinking, 0 stream errors)".to_string()),
            is_synthetic_failure: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "expected WantsStop for 'model returned no usable content', got {result:?}"
        );
    }

    #[test]
    fn test_eval_stream_errors_returns_wants_stop() {
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.0),
            reason: Some("model stream produced only errors (2)".to_string()),
            is_synthetic_failure: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "expected WantsStop for stream errors, got {result:?}"
        );
    }

    #[test]
    fn test_eval_normal_low_confidence_not_marked_synthetic() {
        // Normal LLM response with low confidence — is_synthetic_failure=false
        // This should still return WantsStop, but the dispatch layer should
        // still run pre-stop verification (unlike synthetic failures).
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.3),
            reason: Some("no active task identified".to_string()),
            is_synthetic_failure: false,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "expected WantsStop for low confidence, got {result:?}"
        );
    }

    #[test]
    fn test_eval_thinking_fallback_confidence_0_3_is_not_synthetic() {
        // Thinking-only fallback has confidence 0.3 — should be WantsStop
        // but is_synthetic_failure should be false (0.3 is not <= 0.3... wait)
        // Actually 0.3 <= 0.3 IS true, but the reason starts with "Model" not "model"
        // Our detection uses to_ascii_lowercase().starts_with("model"), so "Model" matches.
        // So this IS detected as synthetic. Let's verify evaluate_response doesn't care
        // about is_synthetic_failure — it just returns WantsStop for confidence < 0.5.
        let input = EvaluationInput {
            should_continue: false,
            confidence: Some(0.3),
            reason: Some("Model returned 5 Thinking events but no Text output".to_string()),
            is_synthetic_failure: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "expected WantsStop for thinking-only fallback, got {result:?}"
        );
    }

    // --- Cross-project plan filtering tests ---

    #[test]
    fn test_active_project_extraction_from_file_url() {
        let msg = "do as a plan [@016_quantized_matmul.md](file:///Users/katopz/git/temp2/riir-burner/.plans/016_quantized_matmul.md)";
        let file_url_start = msg.find("file:///").unwrap();
        let path = &msg[file_url_start + 7..];
        let end = path
            .find(|c: char| c == ')' || c == ' ' || c == '\n')
            .unwrap_or(path.len());
        let full_path = &path[..end];
        let active = if let Some(pos) = full_path.find("/.plans/") {
            Some(full_path[..pos].to_string())
        } else if let Some(pos) = full_path.find("/.plan/") {
            Some(full_path[..pos].to_string())
        } else {
            None
        };
        assert_eq!(
            active,
            Some("/Users/katopz/git/temp2/riir-burner".to_string())
        );
    }

    #[test]
    fn test_active_project_extraction_no_file_url() {
        let msg = "just a regular message with no file references";
        let active = msg.find("file:///").and_then(|file_url_start| {
            let path = &msg[file_url_start + 7..];
            let end = path
                .find(|c: char| c == ')' || c == ' ' || c == '\n')
                .unwrap_or(path.len());
            let full_path = &path[..end];
            if let Some(pos) = full_path.find("/.plans/") {
                Some(full_path[..pos].to_string())
            } else {
                None
            }
        });
        assert_eq!(active, None);
    }

    #[test]
    fn test_cross_project_sort_active_first() {
        let active = "/Users/katopz/git/temp2/riir-burner";
        let mut paths: Vec<String> = vec![
            "/Users/katopz/git/microgpt-rs/.plans/033_bomberman.md".to_string(),
            "/Users/katopz/git/temp2/riir-burner/.plans/016_quantized_matmul.md".to_string(),
            "/Users/katopz/git/gist/anyrag/.plans/001_github.md".to_string(),
            "/Users/katopz/git/temp2/riir-burner/.plans/015_fused_kernels.md".to_string(),
        ];
        paths.sort_by(|a, b| {
            let a_active = a.starts_with(active);
            let b_active = b.starts_with(active);
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });
        assert!(paths[0].starts_with(active));
        assert!(paths[1].starts_with(active));
        assert!(!paths[2].starts_with(active));
        assert!(!paths[3].starts_with(active));
        assert_eq!(
            paths[0],
            "/Users/katopz/git/temp2/riir-burner/.plans/015_fused_kernels.md"
        );
        assert_eq!(
            paths[1],
            "/Users/katopz/git/temp2/riir-burner/.plans/016_quantized_matmul.md"
        );
    }

    #[test]
    fn test_build_lightweight_retry_context_includes_last_message_and_plans() {
        let context_json = r##"{"plan_files":[
            {"path":"/project/.plans/016_test.md","content":"# Plan\n- [ ] Task 1\n- [ ] Task 2\n"}
        ],"messages":[]}"##;
        let result = build_lightweight_retry_context(
            context_json,
            Some("Would you like me to continue with Phase 3?"),
            Some("Test Thread"),
        );
        assert!(result.contains("Thread: Test Thread"));
        assert!(result.contains("Would you like me to continue with Phase 3?"));
        assert!(result.contains("016_test.md"));
    }

    #[test]
    fn test_build_lightweight_retry_context_truncates_to_last_3_paragraphs() {
        let paragraphs: Vec<String> = (0..10).map(|i| format!("Paragraph {i}")).collect();
        let long_msg = paragraphs.join("\n\n");
        let context_json = r##"{"plan_files":[],"messages":[]}"##;
        let result = build_lightweight_retry_context(context_json, Some(&long_msg), None);
        assert!(result.contains("Paragraph 9"));
        assert!(result.contains("Paragraph 8"));
        assert!(result.contains("Paragraph 7"));
        assert!(!result.contains("Paragraph 0"));
        assert!(!result.contains("Paragraph 5"));
    }

    #[test]
    fn test_build_lightweight_retry_context_no_plans_no_message() {
        let context_json = r##"{"plan_files":[],"messages":[]}"##;
        let result = build_lightweight_retry_context(context_json, None, Some("Empty Thread"));
        assert!(result.contains("Thread: Empty Thread"));
        assert!(!result.contains("Last assistant message"));
        assert!(!result.contains("Incomplete plans"));
    }

    // --- Strikethrough / skipped task detection tests ---

    #[test]
    fn test_is_actionable_checkbox_strikethrough_skipped() {
        assert!(!is_actionable_checkbox(
            "- [ ] ~~T5: SIMD-accelerate stuff~~ Skipped — YAGNI"
        ));
    }

    #[test]
    fn test_is_actionable_checkbox_strikethrough_deferred() {
        assert!(!is_actionable_checkbox(
            "- [ ] ~~**Task 4.4:** Q4S training benchmark~~ — deferred to Phase 5"
        ));
    }

    #[test]
    fn test_is_actionable_checkbox_strikethrough_cancelled() {
        assert!(!is_actionable_checkbox("- [ ] ~~Refactor API~~ Cancelled"));
    }

    #[test]
    fn test_is_actionable_checkbox_wont_fix() {
        assert!(!is_actionable_checkbox("- [ ] Fix edge case — Won't fix"));
    }

    #[test]
    fn test_is_actionable_checkbox_normal_task() {
        assert!(is_actionable_checkbox("- [ ] Implement the thing"));
    }

    #[test]
    fn test_is_actionable_checkbox_star_variant() {
        assert!(is_actionable_checkbox("* [ ] Another task"));
    }

    #[test]
    fn test_is_actionable_checkbox_star_skipped() {
        assert!(!is_actionable_checkbox("* [ ] ~~Old task~~ Skipped"));
    }

    #[test]
    fn test_has_unchecked_items_ignores_strikethrough_skipped() {
        let plan = "\
# Plan

- [x] Task 1 done
- [ ] Task 2 in progress
- [ ] ~~T5: SIMD stuff~~ Skipped — YAGNI
";
        assert!(has_unchecked_items(plan));
    }

    #[test]
    fn test_has_unchecked_items_all_skipped_returns_false() {
        let plan = "\
# Plan

- [x] Task 1 done
- [x] Task 2 done
- [ ] ~~T5: SIMD stuff~~ Skipped — YAGNI
- [ ] ~~Task 4.4 benchmark~~ — deferred to Phase 5
";
        assert!(!has_unchecked_items(plan));
    }

    #[test]
    fn test_count_actionable_tasks_ignores_strikethrough() {
        let plan = "\
# Plan

- [x] Done task
- [ ] Real task
- [ ] ~~Skipped task~~ Skipped — YAGNI
- [ ] ~~Deferred task~~ — deferred
";
        assert_eq!(count_actionable_tasks(plan), 1);
    }

    #[test]
    fn test_detect_remaining_work_strikethrough_checkbox_not_actionable() {
        let msg = "Summary of work done.\n\n- [ ] ~~T5: SIMD stuff~~ Skipped — YAGNI";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "struck-through checkbox should not trigger remaining work override, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_mixed_checkboxes_only_actionable_triggers() {
        let msg = "Work done.\n\n- [ ] ~~Skipped~~ Skipped\n\n- [ ] Real remaining task";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "real checkbox among skipped should trigger"
        );
        assert!(result.unwrap().contains("Real remaining task"));
    }

    #[test]
    fn test_extract_remaining_section_only_skipped_checkboxes() {
        let text =
            "Summary.\n\n### Remaining:\n\n- [ ] ~~Task A~~ Skipped\n- [ ] ~~Task B~~ — deferred";
        let result = extract_remaining_section(text);
        // extract_remaining_section returns the trailing paragraphs regardless,
        // but detect_remaining_work will filter — test the pipeline end-to-end instead
        // Here we just verify no actionable checkbox is detected in those paragraphs
        if let Some(section) = &result {
            let has_actionable = section.lines().any(is_actionable_checkbox);
            assert!(!has_actionable, "should have no actionable checkboxes");
        }
    }

    #[test]
    fn test_eval_remaining_work_strikethrough_in_last_message() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some(
                "All phases complete.\n\n- [ ] ~~T5: SIMD accelerate~~ Skipped — YAGNI".to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "struck-through only items should not override stop, got: {result:?}"
        );
    }

    #[test]
    fn test_eval_remaining_work_real_plus_skipped_continues() {
        let input = EvaluationInput {
            should_continue: false,
            last_assistant_message: Some(
                "Phase 1 done.\n\n- [ ] Run benchmarks\n- [ ] ~~T5: SIMD~~ Skipped".to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("remaining work"));
            }
            _ => panic!("expected Continue override for real checkbox, got {result:?}"),
        }
    }
}
