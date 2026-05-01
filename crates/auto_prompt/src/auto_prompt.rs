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

fn with_first_prompt_context(next_prompt: String, summary: Option<&str>) -> String {
    match summary {
        Some(summary) if !summary.trim().is_empty() => {
            format!("refer to first prompt \"{summary}\"\n---\n{next_prompt}")
        }
        _ => next_prompt,
    }
}

/// Extract the raw original user intent from a thread's `first_user_message`.
///
/// When auto_prompt chains threads, the new thread's first user message looks like:
///   `[@Thread Name](link)\n\nrefer to first prompt "..."\n---\nactual work prompt`
///
/// This function strips the auto-generated wrapper to recover the original
/// user intent, which may be embedded in a `refer to first prompt "..."` clause.
fn extract_original_user_message(first_user_message: &str) -> Option<String> {
    let stripped = first_user_message.trim();

    // Strip leading markdown link line(s) like "[@Thread Name](zed:///agent/thread/...)"
    let without_link = stripped
        .lines()
        .skip_while(|line| line.trim_start().starts_with('['))
        .collect::<Vec<_>>()
        .join("\n");

    let without_link = without_link.trim();

    // Try to extract from "refer to first prompt "..."" pattern
    if let Some(rest) = without_link.strip_prefix("refer to first prompt") {
        let rest = rest.trim();
        // Extract the quoted string
        if let Some(after_quote) = rest.strip_prefix('"') {
            if let Some(end) = after_quote.find('"') {
                let original = after_quote[..end].to_string();
                if !original.trim().is_empty() {
                    return Some(original);
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
            .field("profile_id", &self.profile_id)
            .finish()
    }
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
    stop_reason: &StopReason,
    cx: &App,
) -> AutoPromptDecision {
    log::info!("[auto_prompt::decide] Starting decision process");

    let config = match load_config_cached() {
        Ok(c) => {
            log::info!("[auto_prompt::decide] Config loaded");
            c
        }
        Err(err) => {
            log::warn!("[auto_prompt::decide] config load failed: {err}");
            return AutoPromptDecision::NoAction;
        }
    };

    log::info!("[auto_prompt::decide] Auto-prompt evaluating");

    if !used_tools {
        log::info!("[auto_prompt::decide] No tools were used, skipping auto-prompt");
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Tools were used, continuing evaluation");

    if matches!(stop_reason, StopReason::Cancelled) {
        log::info!("[auto_prompt::decide] Thread was cancelled, skipping auto-prompt");
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Stop reason: {:?}", stop_reason);

    let iteration_count = get_iteration();
    log::info!(
        "[auto_prompt::decide] Current iteration: {}",
        iteration_count
    );

    if iteration_count > config.max_iterations {
        log::info!(
            "[auto_prompt::decide] Max iterations ({}) reached, stopping chain",
            config.max_iterations
        );
        reset_iteration();
        return AutoPromptDecision::NoAction;
    }

    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry.default_model() else {
        log::warn!("[auto_prompt::decide] No language model configured in Zed");
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

    let make_action = |prompt: String| {
        let fallback_summary = original_user_message.as_deref().map(|s| {
            let line = s.lines().next().unwrap_or(s);
            line.trim().to_string()
        });
        let next_prompt = with_first_prompt_context(prompt, fallback_summary.as_deref());
        AutoPromptAction {
            from_session_id: session_id.clone(),
            from_title: thread_title.clone(),
            next_prompt,
            work_dirs: work_dirs.clone(),
            original_user_message: original_user_message.clone(),
            profile_id: None,
        }
    };

    let make_continue_prompt = || {
        // Do not blindly pick plan files here — the user's first message
        // (prepended via with_first_prompt_context) carries their intent.
        // Plan transitions are handled by the LLM decision path.
        "continue".to_string()
    };

    log::info!(
        "[auto_prompt::decide] Approximate token count: {}",
        auto_prompt_ctx.approximate_token_count
    );

    if auto_prompt_ctx.exceeds_token_limit(config.max_context_tokens) {
        log::warn!(
            "[auto_prompt::decide] Token limit exceeded ({} > {}), forcing continue",
            auto_prompt_ctx.approximate_token_count,
            config.max_context_tokens
        );
        log::info!(
            "auto_prompt: token limit exceeded (~{} > {}), forcing continue",
            auto_prompt_ctx.approximate_token_count,
            config.max_context_tokens
        );
        return AutoPromptDecision::DispatchNow(make_action(make_continue_prompt()));
    }

    log::info!(
        "[auto_prompt::decide] Had error: {}",
        auto_prompt_ctx.had_error
    );

    // MaxTokens is a hard context limit, not a transient error.
    // No amount of waiting will help — dispatch new thread immediately.
    if matches!(stop_reason, StopReason::MaxTokens) {
        log::info!(
            "auto_prompt: MaxTokens reached (context limit), dispatching new thread immediately"
        );
        return AutoPromptDecision::DispatchNow(make_action(make_continue_prompt()));
    }

    if auto_prompt_ctx.had_error || matches!(stop_reason, StopReason::Refusal) {
        let delay = config.backoff_delay_ms(iteration_count);
        log::info!(
            "[auto_prompt::decide] Error state detected, backing off {}ms",
            delay
        );
        log::info!(
            "auto_prompt: error state (had_error={}, stop_reason={:?}), backing off {}ms then continuing",
            auto_prompt_ctx.had_error,
            stop_reason,
            delay
        );
        return AutoPromptDecision::DispatchAfterDelay {
            action: make_action(make_continue_prompt()),
            delay_ms: delay,
        };
    }

    log::info!("[auto_prompt::decide] Will call LLM for decision");

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
            return AutoPromptDecision::NoAction;
        }
    };

    let project_root = auto_prompt_ctx.current_paths.first().map(PathBuf::from);

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
        profile_id: None,
    })
}

/// Async LLM call to determine the next prompt.
///
/// Returns `Some(action)` if the chain should continue, `None` to stop.
pub async fn decide_with_llm(
    data: LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<Option<AutoPromptAction>> {
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
        Ok((raw_response, response)) => {
            if let Some(ref root) = data.project_root {
                write_decision_log(
                    root,
                    data.iteration_count,
                    &format!("{:?}", data.model.id()),
                    &data.system_prompt,
                    &data.context_json,
                    &raw_response,
                    &response,
                );
            }

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
                .as_deref()
                .map(|s| {
                    let line = s.lines().next().unwrap_or(s);
                    line.trim().chars().take(120).collect::<String>()
                })
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
                        .and_then(|raw| raw.lines().next())
                        .map(|line| line.trim().chars().take(120).collect::<String>())
                });

            let all_done = response.all_plan_done
                || response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

            if all_done {
                match find_next_plan_prompt(&data.context_json, data.work_dirs.as_deref()) {
                    Some(next_plan_prompt) => {
                        log::info!(
                            "[auto_prompt::decide_with_llm] Current plan done, transitioning to next plan"
                        );
                        let next_prompt = format!(
                            "Create a git feature branch for the completed plan from develop and commit all changes with conventional commit messages. Then {next_plan_prompt}"
                        );
                        let next_prompt =
                            with_first_prompt_context(next_prompt, prompt_summary.as_deref());
                        return Ok(Some(AutoPromptAction {
                            from_session_id: data.session_id,
                            from_title: data.title,
                            next_prompt,
                            work_dirs: data.work_dirs,
                            original_user_message: data.original_user_message,
                            profile_id: data.profile_id.clone(),
                        }));
                    }
                    None => {
                        if response.should_continue {
                            log::info!(
                                "[auto_prompt::decide_with_llm] #ALL_PLAN_DONE but LLM says continue, dispatching final gitflow commit"
                            );
                            let gitflow_prompt = "All plans are complete. Create or reuse a git feature branch from develop and commit all changes with conventional commit messages (feat/fix/refactor) if not committed yet. Do not merge — leave the branch for review.".to_string();
                            let next_prompt = with_first_prompt_context(
                                gitflow_prompt,
                                prompt_summary.as_deref(),
                            );
                            return Ok(Some(AutoPromptAction {
                                from_session_id: data.session_id,
                                from_title: data.title,
                                next_prompt,
                                work_dirs: data.work_dirs,
                                original_user_message: data.original_user_message,
                                profile_id: data.profile_id.clone(),
                            }));
                        } else {
                            log::info!(
                                "[auto_prompt::decide_with_llm] #ALL_PLAN_DONE, no remaining plans, LLM says stop — chain complete"
                            );
                            reset_iteration();
                            return Ok(None);
                        }
                    }
                }
            }

            if response.confidence.is_some_and(|c| c < 0.5) {
                log::info!(
                    "[auto_prompt::decide_with_llm] Confidence too low ({} < 0.5), stopping chain",
                    response.confidence.unwrap()
                );
                reset_iteration();
                return Ok(None);
            }

            if !response.should_continue && !has_prompt {
                let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
                let max_verifications = data.max_verification_attempts;

                if verification_count == 0 {
                    log::info!(
                        "auto_prompt: LLM says stop, initiating pre-stop verification (attempt 1/{max_verifications})"
                    );
                    VERIFICATION_COUNT.fetch_add(1, Ordering::Relaxed);

                    match build_pre_stop_verification_prompt(&data.context_json, &data.work_dirs) {
                        Some(verification_prompt) => {
                            log::info!(
                                "auto_prompt: dispatching pre-stop verification prompt: {}...",
                                verification_prompt.chars().take(80).collect::<String>()
                            );
                            let next_prompt = with_first_prompt_context(
                                verification_prompt,
                                prompt_summary.as_deref(),
                            );
                            return Ok(Some(AutoPromptAction {
                                from_session_id: data.session_id,
                                from_title: data.title,
                                next_prompt,
                                work_dirs: data.work_dirs,
                                original_user_message: data.original_user_message,
                                profile_id: data.profile_id.clone(),
                            }));
                        }
                        None => {
                            log::info!(
                                "auto_prompt: no verification needed (no plan files found), stopping"
                            );
                            reset_iteration();
                            return Ok(None);
                        }
                    }
                } else if verification_count < max_verifications {
                    log::info!(
                        "auto_prompt: LLM says stop after verification attempt {verification_count}/{max_verifications}, accepting stop"
                    );
                    reset_iteration();
                    return Ok(None);
                } else {
                    log::warn!(
                        "auto_prompt: max verification attempts ({max_verifications}) exceeded, forcing stop"
                    );
                    reset_iteration();
                    return Ok(None);
                }
            }

            let next_prompt = if has_prompt {
                let prompt = response.next_prompt.unwrap();
                prompt.replace("#ALL_PLAN_DONE", "").trim().to_string()
            } else {
                log::info!("auto_prompt: no prompt determined, stopping");
                reset_iteration();
                return Ok(None);
            };

            if next_prompt.is_empty() {
                log::info!("auto_prompt: prompt was empty after cleanup, stopping");
                reset_iteration();
                return Ok(None);
            }

            // Safety check: if heading to doc creation but plan has unchecked items,
            // override to checkbox verification first.
            let next_prompt = if is_doc_creation_prompt(&next_prompt) {
                match build_checkbox_verification_prompt(&data.context_json) {
                    Some(verification_prompt) => {
                        log::info!(
                            "auto_prompt: plan has unchecked items, overriding doc creation with checkbox verification"
                        );
                        verification_prompt
                    }
                    None => next_prompt,
                }
            } else {
                next_prompt
            };

            // If LLM self-corrects during PreStop, reset verification for fresh cycle on next stop
            let current_verification = VERIFICATION_COUNT.load(Ordering::Relaxed);
            if current_verification > 0 {
                log::info!(
                    "auto_prompt: LLM continuing during PreStop (verification_count={}), resetting for fresh verification on next stop",
                    current_verification
                );
                VERIFICATION_COUNT.store(0, Ordering::Relaxed);
            }

            log::info!(
                "auto_prompt: dispatching new thread with prompt: {}...",
                next_prompt.chars().take(80).collect::<String>()
            );

            let next_prompt = with_first_prompt_context(next_prompt, prompt_summary.as_deref());

            Ok(Some(AutoPromptAction {
                from_session_id: data.session_id,
                from_title: data.title,
                next_prompt,
                work_dirs: data.work_dirs,
                original_user_message: data.original_user_message,
                profile_id: data.profile_id.clone(),
            }))
        }
        Err(err) => {
            if let Some(ref root) = data.project_root {
                write_error_log(
                    root,
                    data.iteration_count,
                    &format!("{:?}", data.model.id()),
                    &err,
                );
            }
            log::warn!("auto_prompt: language model call failed: {err}");
            Err(err)
        }
    }
}

fn write_decision_log(
    project_root: &PathBuf,
    iteration: u32,
    model: &str,
    system_prompt: &str,
    context_json: &str,
    raw_response: &str,
    parsed: &AutoPromptResponse,
) {
    let logs_dir = project_root.join(".logs");
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

fn write_error_log(project_root: &PathBuf, iteration: u32, model: &str, error: &anyhow::Error) {
    let logs_dir = project_root.join(".logs");
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

fn default_system_prompt() -> String {
    indoc::indoc! {r#"
        You decide whether to continue working. You receive conversation context and plan files.

        Respond ONLY with valid JSON:
        {"should_continue": bool, "next_prompt": string | null, "reason": string | null, "all_plan_done": bool, "confidence": float, "first_prompt_summary": string | null}

        ## first_prompt_summary:
        - Read the first user message in messages[] and distill it into a concise one-liner
        - Capture the user's actual intent (e.g. "finish plan 083 085", "implement auth flow")
        - Omit file references, code blocks, and verbose details
        - This gets prepended to every subsequent auto-prompt to keep the chain grounded
        - Only set to null if there is no first user message

        ## Rules (in order):

        1. If stop_phase is "pre_stop":
           - The LLM worker wants to stop but verification is required
           - If any plan file has unchecked `- [ ]` items → should_continue=true, next_prompt = continue that work
           - If diagnostics are likely dirty → should_continue=true, next_prompt = "Run cargo check and cargo clippy. Fix ALL errors and warnings."
           - If git status is likely dirty → should_continue=true, next_prompt = "Commit all changes with conventional commit messages to a feature branch from develop."
           - Only if ALL checks pass → should_continue=false

        2. If plan_files has unchecked `- [ ]` steps:
           - should_continue=true
           - next_prompt = continue the next unchecked step
           - Use the actual file paths from plan_files (e.g. .plans/ or .plan/)
           - Include "Mark completed steps as [x]" referencing actual plan file paths
           - Process plan files in filename order (01 before 02)
           - When one plan completes, transition to the next

        3. If plan_has_checkboxes is false but plan_files exist:
           - Use the actual path from plan_files[0].path, not a hardcoded directory
           - next_prompt = "Read {actual_path} and add checkboxes (- [ ]) for all tasks at the top. Keep existing content below."
           - should_continue=true

        4. If the last message asks a question or lists options:
           - should_continue=true
           - Pick option 1 or what the AI recommends
           - If unsure, search the codebase and pick the best default
           - confidence >= 0.6

        5. If all plan steps are [x]:
           - If diagnostics or test failures likely exist → next_prompt = "Fix all diagnostics and ensure test coverage. Production grade only — no mock, no TODO, no placeholder."
           - Else if doc_files is empty → next_prompt = "Create .docs/{NN}_summary.md documenting what was implemented, key decisions, file changes, and how to test."
           - Else if no git feature branch was created for this plan in the conversation → next_prompt = "Create a git feature branch feature/{plan_number}_{description} from develop. Commit all changes with conventional commit messages."
           - Else → all_plan_done=true, should_continue=false

        6. If no plan exists but work seems incomplete → should_continue=true, next_prompt="continue"

        7. confidence < 0.5 → should_continue=false
        8. iteration_count > 15 → consider stopping

        ## Pre-stop verification (when stop_phase is "pre_stop"):
        Before confirming stop, verify ALL of these:
        - All plan checkboxes are [x] (no [ ] remaining)
        - Code diagnostics are clean (no errors, no warnings)
        - Git is committed with conventional commit messages
        If ANY check fails → should_continue=true with a fix prompt

        ## Quality (always enforce):
        - Production grade: no mock, no TODO, no placeholder, no unwrap()
        - Fix all compiler diagnostics and warnings before marking done
        - Ensure test coverage for new code

        ## Git flow (when applicable):
        - Feature: feature/{plan_number}_description from develop
        - Hotfix: hotfix/{plan_number}_description from main
        - Complete: rebase onto develop, merge to develop
        - Conventional commits: feat:, fix:, refactor:, test:, chore:, docs:
    "#}
    .to_string()
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
    checks.push("3. **Git status**: Verify all changes are committed with conventional commit messages (feat/fix/refactor/test/chore/docs). Create or reuse a feature branch from develop if not done.".to_string());

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
