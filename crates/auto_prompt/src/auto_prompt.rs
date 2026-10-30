//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and decides whether
//! a follow-up prompt should be dispatched.
//!
//! This crate contains the decision logic only. The caller (agent_ui)
//! handles the actual GPUI action dispatch.

pub mod api_unreachable;
pub mod auth_failure;
mod chain;
pub mod claude_agent;
mod config;
pub mod context;
mod context_files;
pub(crate) mod debug_log;
pub mod lightweight_context;
mod model_call;
mod overflow;
pub mod peer_states;
mod pending_question;
pub mod plan_registry;
mod plan_source;
mod remaining_work;
pub mod session_limit;
mod verdict_dispatch;
pub mod watchdog;

/// Test-harness hook: disables `plan_source`'s real git process spawns so
/// deterministic test executors are never woken by the foreign
/// `async-io`/`async-process` driver threads that spawning creates. Set by
/// agent_ui's test init; production never calls this.
pub fn set_plan_source_git_spawns_disabled(disabled: bool) {
    plan_source::set_git_spawns_disabled(disabled);
}

pub use config::{AutoPromptConfig, DEFAULT_MAX_CONCURRENT_STREAMS, default_max_context_tokens};
pub use context::{
    AutoPromptContext, AutoPromptContextSnapshot, AutoPromptResponse, PlanFileContent, StopPhase,
    truncate_to_paragraph_budget,
};
pub use plan_registry::ActivePlanClaim;

use agent_client_protocol::schema::v2 as acp;
pub use context_files::warm_plan_snapshots;
use context_files::{
    ContextFileInputs, context_file_inputs, plan_inputs_without_message, read_doc_files,
    read_plan_files, read_plan_files_cached,
};
use gpui::App;
use language_model::LanguageModel;
use lightweight_context::{count_actionable_tasks, has_unchecked_items};
use model_call::call_language_model;
pub use model_call::stream_model_completion;
pub use overflow::with_bloat_section;
use overflow::{
    MAX_TOKENS_RESUME_PROMPT, api_unreachable_decision, context_overflow_outcome,
    cutoff_context_full_outcome, max_tokens_context_window_full, output_bloat_directive,
};
pub(crate) use remaining_work::{STOP_KEYWORD, message_requests_stop};
use remaining_work::{
    SummaryContinuation, detect_remaining_work, extract_summary_next_steps,
    housekeeping_continuation, is_auto_prompt_summary_response, looks_like_voluntary_summary,
};
use std::path::PathBuf;

/// Per-session summary tracking for the context-overflow flow.
///
/// Replaces the former global `SUMMARY_REQUESTED` atomic to prevent race
/// conditions when multiple auto_prompt chains overlap.
///
/// State values:
///   0 = no summary requested yet.
///   1 = summary requested, waiting for AI response.
static SUMMARY_REGISTRY: std::sync::RwLock<Option<std::collections::HashMap<String, u32>>> =
    std::sync::RwLock::new(None);

fn with_summary_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut std::collections::HashMap<String, u32>) -> R,
{
    let mut guard = SUMMARY_REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    f(map)
}

fn read_summary_registry<F, R>(f: F) -> R
where
    F: FnOnce(&std::collections::HashMap<String, u32>) -> R,
{
    let guard = SUMMARY_REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let empty = std::collections::HashMap::new();
    let map = guard.as_ref().unwrap_or(&empty);
    f(map)
}

fn summary_state_for(session_id: &str) -> u32 {
    read_summary_registry(|map| map.get(session_id).copied().unwrap_or(0))
}

fn set_summary_state(session_id: &str, state: u32) {
    with_summary_registry(|map| {
        if state == 0 {
            map.remove(session_id);
        } else {
            map.insert(session_id.to_string(), state);
        }
    })
}

fn clear_summary_for_session(session_id: &str) {
    with_summary_registry(|map| {
        map.remove(session_id);
    })
}

// Plan 023 D: clarification loop guard — the pros/cons clarify prompt fires at
// most once per chain. Deliberately sticky (not cleared on stop/reset): a
// stop→clarify→stop cycle must not re-ask; the next chain hop gets a fresh
// session id and a fresh budget.
static CLARIFY_REGISTRY: std::sync::RwLock<Option<std::collections::HashMap<String, bool>>> =
    std::sync::RwLock::new(None);

fn clarify_fired_for(session_id: &str) -> bool {
    let guard = CLARIFY_REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .as_ref()
        .is_some_and(|map| map.get(session_id).copied().unwrap_or(false))
}

fn mark_clarify_fired(session_id: &str) {
    let mut guard = CLARIFY_REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(session_id.to_string(), true);
}

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

/// Kill-switch probe for the automatic chain (plan 031 silent stop): true
/// when `paused` is set in auto_prompt.json or ZED_AUTO_PROMPT_PAUSED. The
/// config cache re-validates on file mtime, so flipping the flag takes
/// effect on the next chain event without a restart. Manual dispatch does
/// not consult this — explicit human intent overrides the pause.
pub fn paused() -> bool {
    load_config_cached()
        .map(|config| config.paused)
        .unwrap_or(false)
}

/// Data needed to dispatch a follow-up prompt via GPUI action.
///
/// The caller (agent_ui) wraps this in `AutoPromptNewThread` action.
#[derive(Clone, Debug)]
pub struct AutoPromptAction {
    pub from_session_id: acp::SessionId,
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
    /// Approximate token count (chars/4) of the auto_prompt context JSON.
    /// Used as fallback when `actual_input_tokens` is `None` (e.g. model
    /// doesn't report usage) to avoid the same-thread infinite loop during
    /// ContextOverflow flow.
    pub approximate_token_count: usize,
    /// The last assistant message from the previous thread,
    /// passed to ThreadSummary for loading indicator + summary flow.
    pub last_assistant_message: Option<String>,
    /// When true, `dispatch_action` must create a new thread regardless
    /// of token counts. Set after ContextOverflow Phase 2 (AI produced
    /// a summary) so the continuation always lands in a fresh thread.
    pub force_new_thread: bool,
    /// When true, the new thread (if one is created) steals keyboard focus
    /// regardless of the `auto_focus_new_thread` setting. Set by
    /// `manual_auto_prompt` so a user-initiated continuation focuses the
    /// thread they just asked for; LLM-decided continuations leave this
    /// false and defer to the setting.
    pub focus_new_thread: bool,
}

/// Outcome of an auto-prompt LLM decision.
#[derive(Clone, Debug)]
pub enum AutoPromptOutcome {
    /// Chain should continue with this action.
    Continue(AutoPromptAction),
    /// Chain stopped with a reason (shown to user as info toast).
    Stopped { reason: String },
    /// Context exceeds token limit. The caller should send a summarization
    /// prompt to the current thread so the AI produces a summary as its last
    /// message, then on the next cycle a new thread is created with that summary.
    ContextOverflow(AutoPromptAction),
    /// Cannot safely make a forward decision right now — the source thread
    /// stopped with an error (typically all API keys rate-limited) AND its
    /// context has overflowed, so creating a new thread would propagate the
    /// failure. The caller should wait `delay_ms`, then re-run `decide_with_llm`
    /// (the rate limit may have cleared). See issue 007.
    RetryAfterBackoff { delay_ms: u64, reason: String },
    /// The orchestration LLM is not confident how to proceed, and the worker's
    /// last message presents options/decision points without a pros/cons
    /// layout. The caller should send the clarification prompt to the SAME
    /// thread so the worker lays out the trade-offs and recommends before any
    /// stop/continue decision is made (plan 023 D, req 5). Fires at most once
    /// per chain via `CLARIFY_REGISTRY`.
    ClarificationRequest(AutoPromptAction),
}

/// Marker introducing the addition-request section appended to new-thread
/// prompts (plan 023 B1).
const ADDITION_REQUEST_MARKER: &str = "## 4. Addition request";

/// Extract the decision text from a `with_first_prompt_context`-formatted string.
/// Returns the content after the last `## N. Decision` header, or None if not found.
/// The extraction is cut at the `## 4. Addition request` boundary so the
/// carried draft never pollutes the decision text (plan 023 B2).
pub fn extract_decision_prompt(prompt: &str) -> Option<String> {
    let marker = "## 3. Decision";
    let alt_marker = "## 2. Decision";
    let start = prompt
        .find(marker)
        .map(|i| i + marker.len())
        .or_else(|| prompt.find(alt_marker).map(|i| i + alt_marker.len()));
    start
        .map(|i| {
            let decision = &prompt[i..];
            match decision.find(ADDITION_REQUEST_MARKER) {
                Some(boundary) => decision[..boundary].trim().to_string(),
                None => decision.trim().to_string(),
            }
        })
        .filter(|s| !s.is_empty())
}

/// Append the stashed input-box draft as a `## 4. Addition request` section to
/// a `with_first_prompt_context` payload (plan 023 B1, req 2.1). No-op when the
/// draft is absent/empty or the section is already present.
pub fn append_addition_request(prompt: &str, addition_request: Option<&str>) -> String {
    let Some(draft) = addition_request.map(str::trim).filter(|d| !d.is_empty()) else {
        return prompt.to_string();
    };
    if prompt.contains(ADDITION_REQUEST_MARKER) {
        return prompt.to_string();
    }
    format!("{prompt}\n\n{ADDITION_REQUEST_MARKER}\n\n{draft}")
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

/// Why a [`AutoPromptDecision::DispatchAfterDelay`] is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPromptDelayReason {
    /// A usage-limit window (session/weekly/Opus) reset schedule parsed from
    /// the provider's error text or usage API.
    UsageLimitReset,
    /// Generic backoff before retrying (e.g. after a refusal).
    Backoff,
    /// Connectivity backoff: the failed turn is re-run in place (when the
    /// agent supports retry) instead of sending `next_prompt`, so an outage
    /// doesn't stack one continuation prompt + start-context block per retry.
    ApiUnreachable,
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
        /// Why the dispatch is delayed; drives the resume notification.
        reason: AutoPromptDelayReason,
    },
    /// Need to call LLM asynchronously to determine the next step.
    NeedsLlmCall(LlmCallData),
}

/// Data needed for the async LLM call path.
#[derive(Clone)]
pub struct LlmCallData {
    pub model: LanguageModel,
    pub system_prompt: String,
    pub context_json: String,
    pub project_root: Option<PathBuf>,
    pub session_id: acp::SessionId,
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
    /// Whether the source thread had errors (rate limit, refusal, max tokens,
    /// or merely a failed tool call). Used by the caller to decide whether to
    /// add a pre-call delay — a broad, low-stakes use where over-triggering
    /// just costs one extra wait, so the wider signal is fine here.
    pub had_error: bool,
    /// Narrower than `had_error`: true only when the completion request
    /// itself failed (network/stream error), never for a failed tool call.
    /// Use this (not `had_error`) for decisions with a high cost of a false
    /// positive, like the context-overflow backoff guard in `decide_with_llm`.
    pub had_api_error: bool,
    /// Current stop lifecycle phase (Working, PreStop, Verified).
    /// Controls confidence thresholds and scopes the handbrake to post-verification.
    pub stop_phase: context::StopPhase,
    /// Whether the context exceeds `max_context_tokens` — skip the expensive
    /// full-context LLM call and go directly to the lightweight retry path.
    pub context_exceeds_limit: bool,
    /// True when the turn ended with `StopReason::MaxTokens` AND the model's
    /// context window is full (input + output ≈ window). A same-thread request
    /// — including the Phase 1 "stop and summarize" — would be rejected by the
    /// API because the input alone already fills the window, so the handoff
    /// summary must be produced by the orchestrator call, then forked into a
    /// fresh thread (`cutoff_context_full_outcome`).
    pub cutoff_context_full: bool,
    /// Approximate token count (chars/4) of the auto_prompt context JSON.
    /// Passed through to AutoPromptAction as fallback when actual_input_tokens
    /// is None.
    pub approximate_token_count: usize,
    /// Claude hidden-thread orchestrator only: the worker thread's connection,
    /// cloned so a second invisible Claude Code session can be spawned to decide
    /// continue/stop. None on the native path and the LLM-call Claude path.
    pub connection: Option<std::rc::Rc<dyn acp_thread::AgentConnection>>,
    /// Claude hidden-thread orchestrator only: the project entity needed by
    /// `new_session`. None on the native path and the LLM-call Claude path.
    pub project: Option<gpui::Entity<project::Project>>,
    /// Phase 2 (agent board): formatted text describing what peer agents on
    /// other devices are doing right now, populated from the agent board's
    /// latest unmuted state snapshot. None when no board is configured or no
    /// peers are active. Injected into the LLM/hidden-thread context so the
    /// decider can reason about concurrent work.
    pub peer_agent_states: Option<String>,
    /// Computed when the thread's output-token budget ran (nearly) dry:
    /// names the largest space consumers in the source thread so the resume
    /// or forked thread avoids re-running them at full width. None when no
    /// bloat evidence — keeps continuation prompts unpolluted.
    pub bloat_directive: Option<String>,
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
            .field("had_api_error", &self.had_api_error)
            .field("stop_phase", &self.stop_phase)
            .field("context_exceeds_limit", &self.context_exceeds_limit)
            .field("cutoff_context_full", &self.cutoff_context_full)
            .field("approximate_token_count", &self.approximate_token_count)
            .field(
                "bloat_directive",
                &self
                    .bloat_directive
                    .as_ref()
                    .map(|s| format!("<{} chars>", s.len())),
            )
            .finish()
    }
}

impl LlmCallData {
    pub(crate) fn make_continue_action(&self, next_prompt: String) -> AutoPromptAction {
        let next_prompt = with_bloat_section(&next_prompt, self.bloat_directive.as_deref());
        // The stop that follows this same-thread continuation must count as
        // the same chain no matter how long the turn took — otherwise the
        // chain-timeout reset wipes the idle streak every slow hop and the
        // runaway cap never fires (`loop-bug`). New-thread forks consume this
        // flag benignly: the forked session starts a fresh chain anyway.
        chain::arm_continuation(&self.session_id.to_string());
        AutoPromptAction {
            from_session_id: self.session_id.clone(),
            from_title: self.title.clone(),
            next_prompt,
            work_dirs: self.work_dirs.clone(),
            original_user_message: self.original_user_message.clone(),
            profile_id: self.profile_id.clone(),
            actual_input_tokens: self.actual_input_tokens,
            approximate_token_count: self.approximate_token_count,
            last_assistant_message: self.last_assistant_message.clone(),
            force_new_thread: false,
            focus_new_thread: false,
        }
    }

    fn make_continue(&self, next_prompt: String) -> AutoPromptOutcome {
        AutoPromptOutcome::Continue(self.make_continue_action(next_prompt))
    }
}

/// Confidence threshold below which we stop during the Working phase.
/// Low threshold = biased toward continuing.
const WORKING_CONFIDENCE_THRESHOLD: f64 = 0.2;

/// Confidence threshold below which we stop during the PreStop phase.
/// High threshold = biased toward stopping (hard to restart).
const PRESTOP_CONFIDENCE_THRESHOLD: f64 = 0.8;

/// Input for the pure evaluation function.
pub struct EvaluationInput {
    pub confidence: Option<f64>,
    pub next_prompt: Option<String>,
    pub reason: Option<String>,
    pub last_assistant_message: Option<String>,
    /// True when the LLM failed to produce a usable response and a synthetic
    /// stop was generated (e.g. "model returned zero events"). Pre-stop
    /// verification is skipped in this case because there is no real decision
    /// to verify.
    pub is_synthetic_failure: bool,
    /// Current stop lifecycle phase (Working, PreStop, Verified).
    /// Determines which confidence threshold to apply.
    pub stop_phase: context::StopPhase,
}

/// Provenance of the final decision — answers "who decided this?"
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionSource {
    /// LLM produced a real response that crossed the confidence threshold.
    LlmResponse,
    /// Confidence below phase threshold (Working < 0.2, PreStop < 0.8).
    ConfidenceGate,
    /// Worker AI explicitly declared stopping after verification (handbrake).
    Handbrake,
    /// Code detected remaining work patterns via rules (`detect_remaining_work`).
    RuleRemainingWork,
    /// LLM crossed threshold but provided no usable prompt.
    LlmNoPrompt,
    /// Plan files have unchecked tasks despite LLM wanting to stop.
    PlanTaskFallback,
}

/// Result of evaluating an LLM response.
#[derive(Debug, PartialEq)]
pub enum EvaluationResult {
    /// Continue the chain with this prompt.
    Continue { prompt: String, reason: String },
    /// LLM wants to stop — must go through verification gate.
    WantsStop { reason: String },
    /// Rules-based detection found potential remaining work — needs LLM second opinion
    /// before deciding. Contains the extracted section and what the rule matched on.
    NeedsSecondOpinion {
        extracted_section: String,
        rule_reason: String,
    },
}

impl EvaluationResult {
    pub fn source(&self) -> DecisionSource {
        match self {
            EvaluationResult::Continue { .. } => DecisionSource::LlmResponse,
            EvaluationResult::WantsStop { reason } => {
                if reason.contains("handbrake") {
                    DecisionSource::Handbrake
                } else if reason.contains("< ") || reason.contains("confidence") {
                    DecisionSource::ConfidenceGate
                } else if reason.contains("no usable prompt") {
                    DecisionSource::LlmNoPrompt
                } else {
                    DecisionSource::LlmResponse
                }
            }
            EvaluationResult::NeedsSecondOpinion { .. } => DecisionSource::RuleRemainingWork,
        }
    }
}

pub fn evaluate_response(input: &EvaluationInput) -> EvaluationResult {
    let confidence = input.confidence.unwrap_or(0.0);
    let has_prompt = input
        .next_prompt
        .as_ref()
        .is_some_and(|prompt| !prompt.trim().is_empty());

    // Phase-dependent confidence thresholds.
    // Working: low threshold (0.2) — biased toward continuing.
    // PreStop: high threshold (0.8) — biased toward stopping.
    let threshold = match input.stop_phase {
        context::StopPhase::Working => WORKING_CONFIDENCE_THRESHOLD,
        context::StopPhase::PreStop | context::StopPhase::Verified => PRESTOP_CONFIDENCE_THRESHOLD,
    };

    if confidence >= threshold {
        // ── Confidence crosses threshold → continue ─────────────────
        if let Some(prompt) = input.next_prompt.as_deref() {
            let cleaned = prompt
                .replace("#ALL_PLAN_DONE", "")
                .replace("#SKIP", "")
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                return EvaluationResult::Continue {
                    prompt: cleaned,
                    reason: format!(
                        "confidence {confidence:.2} >= {threshold:.2} ({:?} phase), LLM provided prompt",
                        input.stop_phase
                    ),
                };
            }
        }

        // Confidence is high enough but no usable prompt.
        return EvaluationResult::WantsStop {
            reason: format!("confidence {confidence:.2} >= {threshold:.2} but no usable prompt"),
        };
    }

    // ── Confidence below threshold → check safety nets before stopping ──

    // Handbrake: worker AI explicitly declared stopping after verification.
    // This overrides everything — breaks loops where orchestration keeps seeing
    // unchecked plan items and continuing despite the worker's explicit stop.
    // Scoped to PreStop/Verified only to prevent false positives during normal work.
    if matches!(
        input.stop_phase,
        context::StopPhase::PreStop | context::StopPhase::Verified
    ) {
        if let Some(msg) = input.last_assistant_message.as_deref() {
            let lower = msg.to_lowercase();
            let has_stopping = lower.contains("stopping");
            let has_qualifier = lower.contains("nothing related")
                || lower.contains("no further action")
                || lower.contains("nothing left")
                || lower.contains("no further work");
            if has_stopping && has_qualifier {
                return EvaluationResult::WantsStop {
                    reason: format!(
                        "confidence {confidence:.2} < {threshold:.2}, handbrake: worker declared stop ({:?} phase)",
                        input.stop_phase
                    ),
                };
            }
        }
    }

    // Safety net: last assistant message contains remaining work patterns.
    if let Some(remaining_prompt) = detect_remaining_work(input.last_assistant_message.as_deref()) {
        return EvaluationResult::NeedsSecondOpinion {
            extracted_section: remaining_prompt,
            rule_reason: format!(
                "confidence {confidence:.2} < {threshold:.2} but last_assistant_message contains remaining work pattern"
            ),
        };
    }

    // Default: respect the low-confidence stop.
    let reason = match (&input.reason, has_prompt) {
        (Some(reason), _) => reason.clone(),
        (None, false) => format!(
            "confidence {confidence:.2} < {threshold:.2} ({:?} phase), no prompt",
            input.stop_phase
        ),
        (None, true) => format!(
            "confidence {confidence:.2} < {threshold:.2} ({:?} phase) despite having prompt",
            input.stop_phase
        ),
    };
    EvaluationResult::WantsStop { reason }
}

/// Confidence threshold below which the orchestration LLM is considered "decisive"
/// about stopping. When confidence is this low AND no prompt is provided,
/// the LLM is certain the chain should stop — verification is pointless.
const DECISIVE_STOP_THRESHOLD: f64 = 0.15;

/// Fixed decision directive used by the manual sparkle click and the
/// summary-first fast path (plan 027). When the worker's last message already
/// ends with a summary (the mandated `## Summary` handoff format), auto_prompt
/// does NOT re-process it with an orchestrator/reasoning LLM call — the
/// new/same thread is continued with this instruction and the summary itself
/// carries "what remains". The manual click dispatches this same directive
/// directly, with no orchestrator call at all.
pub const CONTINUE_REMAINS_DECISION: &str = "Continue remaining work with best perf/sec, prod-grade decisions (SOLID, DRY); file issues/plans as needed to ensure full coverage. If nothing remains, check other repos for unfinished tasks or do housekeeping (doc-sync, riir-clippy mining, code-smell hunt, online search to distill perf/sec or unblock deferred work). If everything that remains is blocked, owner-gated, or otherwise not executable now, do not loop on re-checking: end your final message with a single line containing only <stop> — auto-prompt halts the chain when it sees that keyword.";

/// Returns true when the orchestration LLM is decisively stopping.
/// A decisive stop has very low confidence and no prompt — the LLM is
/// certain, so verification would just create wasted work.
pub fn is_decisive_stop(input: &EvaluationInput) -> bool {
    let confidence = input.confidence.unwrap_or(0.0);
    let has_prompt = input
        .next_prompt
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty());
    confidence <= DECISIVE_STOP_THRESHOLD && !has_prompt
}

/// Returns true when the worker AI's last message explicitly defers a decision
/// to the user — i.e. it is asking the user to pick between options or to
/// provide input that the worker itself cannot supply.
///
/// This is distinct from permission-seeking questions ("Want me to implement X?")
/// which rule 3 of the orchestration prompt auto-answers. Here the worker is
/// asking for a *strategic* or *external* decision and explicitly declines to
/// make the choice itself (e.g. "I won't pick for you", "you decide",
/// "I need a decision from you"). Continuing the chain is pointless — another
/// nudge will produce the same question — so we stop cleanly and surface the
/// message to the user.
///
/// Must be scoped to the worker's own output: auto_prompt's own summary /
/// verification prompts are not the worker deferring to the user, so we exclude
/// those explicitly via `is_auto_prompt_summary_response`.
pub fn is_waiting_for_user_decision(last_assistant_message: Option<&str>) -> bool {
    let msg = match last_assistant_message {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return false,
    };

    // auto_prompt's own Phase 1 summary responses naturally contain phrases like
    // "what remains" and "recommended next steps" — those are not the worker
    // deferring a decision to the user.
    if is_auto_prompt_summary_response(msg) {
        return false;
    }

    let lower = msg.to_lowercase();

    // Worker explicitly defers the decision to the user. These phrases are
    // deliberately specific — they require the worker to *say* it is deferring,
    // not merely to mention the word "you" near options.
    let explicit_deferral = lower.contains("i won't pick for you")
        || lower.contains("i won't choose for you")
        || lower.contains("i can't pick for you")
        || lower.contains("i can't decide for you")
        || lower.contains("i won't decide for you")
        || lower.contains("you need to decide")
        || lower.contains("you should decide")
        || lower.contains("you decide")
        || lower.contains("i need a decision from you")
        || lower.contains("i need your decision")
        || lower.contains("i need your input")
        || lower.contains("need your decision")
        || lower.contains("need your input")
        || lower.contains("need your choice")
        || lower.contains("awaiting your decision")
        || lower.contains("waiting for your decision")
        || lower.contains("waiting on you")
        || lower.contains("waiting for you to")
        || lower.contains("let me know which")
        || lower.contains("let me know what you prefer")
        || lower.contains("let me know how you'd like");

    explicit_deferral
}

/// Summary-first fast path (plan 027, shared by native + Claude flows).
///
/// When the worker's last message is already a voluntary summary (the mandated
/// `## Summary` handoff), skip the orchestrator LLM call entirely:
///
/// - summary names its remaining work → continue in the SAME thread with
///   those steps AHEAD of the fixed [`CONTINUE_REMAINS_DECISION`] directive
///   (plan 031 enrichment: the named task wins over re-derivation);
/// - summary declares nothing left → continue same-thread with the
///   housekeeping directive (rules; the old deferral paid an orchestrator
///   call only to stop into the same hook);
/// - summary is TERMINAL (every remaining item armed/deferred/owner-gated) →
///   `Stopped` — gated work is not picked up autonomously (plan 031);
/// - no next-steps section → continue same-thread with the fixed directive.
///
/// Returns `None` also when there is no summary at the last paragraph (the
/// normal flow runs: overflow asks Phase 1 for a summary first; same-thread
/// consults the orchestrator) and when the thread overflowed (the caller
/// routes through the shared Phase 1/2 machine, which forks with the summary).
///
/// The returned action always has `force_new_thread = false`: below the
/// overflow gate the chain continues same-thread (plan 023 req 4).
pub fn summary_continuation_fast_path(data: &LlmCallData) -> Option<AutoPromptOutcome> {
    if data.context_exceeds_limit {
        return None;
    }
    // The source turn failed at the API level: the "voluntary summary" we're
    // about to act on is STALE (it predates the failed turn — no new assistant
    // message was produced). Continuing from it (including the housekeeping
    // directive) dispatches straight back into the same blocked API and loops
    // forever: send → 405 → error-path chaining → summary fast path → send.
    // Defer to the LLM path instead, whose retry-exhaustion guard backs off
    // and stops the chain when the API stays down.
    if data.had_api_error {
        log::warn!(
            "[auto_prompt] summary fast path skipped: source turn had an API error — not continuing from a stale summary"
        );
        return None;
    }
    let last = data.last_assistant_message.as_deref()?;
    if !looks_like_voluntary_summary(last) {
        return None;
    }
    match extract_summary_next_steps(last) {
        Some(SummaryContinuation::NothingLeft) => {
            // Rules-based (plan 031): nothing left → housekeeping directly.
            // The old deferral to the orchestrator paid an LLM call only for
            // it to stop into the same housekeeping hook.
            log::info!(
                "[auto_prompt] summary fast path: summary declares nothing left — switching to housekeeping (rules, no orchestrator call)"
            );
            let action = data.make_continue_action(housekeeping_continuation());
            return Some(AutoPromptOutcome::Continue(action));
        }
        Some(SummaryContinuation::Terminal) => {
            log::warn!(
                "[auto_prompt] summary fast path: summary is terminal (all remaining work armed/deferred/owner-gated) — stopping the chain (housekeeping skipped)"
            );
            return Some(AutoPromptOutcome::Stopped {
                reason: "summary is terminal — all remaining work is armed/deferred/owner-gated"
                    .to_string(),
            });
        }
        Some(SummaryContinuation::Steps(steps)) => {
            let repeats = chain::record_summary_steps(&data.session_id.to_string(), &steps);
            if repeats >= chain::MAX_UNCHANGED_REPEATS {
                log::warn!(
                    "[auto_prompt] summary fast path: next steps unchanged for {repeats} summaries in a row — stopping the chain (no progress possible without the user)"
                );
                return Some(AutoPromptOutcome::Stopped {
                    reason: format!(
                        "no progress — the summary's next steps were unchanged for {repeats} summaries in a row"
                    ),
                });
            }
            log::warn!(
                "[auto_prompt] summary fast path: voluntary summary — continuing same-thread with the summary's next steps + fixed decision fallback (no orchestrator call)"
            );
            let action =
                data.make_continue_action(format!("{steps}\n\n{CONTINUE_REMAINS_DECISION}"));
            return Some(AutoPromptOutcome::Continue(action));
        }
        None => {}
    }
    log::warn!(
        "[auto_prompt] summary fast path: last message is a voluntary summary — continuing same-thread with fixed decision (no orchestrator call)"
    );
    let action = data.make_continue_action(CONTINUE_REMAINS_DECISION.to_string());
    Some(AutoPromptOutcome::Continue(action))
}

/// True when the last assistant message is a voluntary summary whose
/// remaining-work section is entirely non-executable now — every bullet
/// armed / deferred / owner-gated / thermal-gated (plan 031 silent stop).
/// Such a summary ends the chain: gated work is not picked up autonomously,
/// and the stop-time housekeeping hook is skipped so nothing new spins up.
pub fn summary_declares_terminal(last_assistant_message: Option<&str>) -> bool {
    let Some(text) = last_assistant_message
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return false;
    };
    // A worker-requested stop (`<stop>` keyword, loop-bug) is terminal even
    // without a `## Summary` heading: the stop-time housekeeping hook checks
    // this function and must not spin up new work behind an explicit stop.
    if message_requests_stop(text) {
        return true;
    }
    if !looks_like_voluntary_summary(text) {
        return false;
    }
    matches!(
        extract_summary_next_steps(text),
        Some(SummaryContinuation::Terminal)
    )
}

/// Outcome of the cheap pre-decide checks: either an immediate decision
/// (auto-prompt declines before touching the filesystem) or the context
/// needed to finish the decision once plan/doc files have been read.
enum DecidePrecheck {
    Decision(AutoPromptDecision),
    Proceed(Box<DecidePrecheckContext>),
}

struct DecidePrecheckContext {
    project_root: Option<std::path::PathBuf>,
    iteration_count: u32,
    config: AutoPromptConfig,
    stop_phase: StopPhase,
    verification_count: u32,
    model: LanguageModel,
}

/// Cheap pre-checks that run on the main thread before any context file is
/// read. Mirrors the original `decide` prefix: config load, cancel/max-iteration
/// gates, model lookup. Everything here is in-memory (config is mtime-cached),
/// so it never blocks the UI thread meaningfully.
fn decide_precheck(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    cx: &gpui::App,
) -> DecidePrecheck {
    log::info!("[auto_prompt::decide] Starting decision process");

    let project_root = thread
        .read(cx)
        .work_dirs()
        .and_then(|pl| pl.paths().first().cloned());

    debug_log::write_log(
        "decide_entry",
        serde_json::json!({
            "stop_reason": format!("{stop_reason:?}"),
            "used_tools": used_tools,
            "iteration": chain::current_turn(&thread.read(cx).session_id().to_string()),
        }),
    );

    let config = match load_config_cached() {
        Ok(c) => {
            log::info!("[auto_prompt::decide] Config loaded");
            c
        }
        Err(err) => {
            log::warn!("[auto_prompt::decide] config load failed: {err}");
            debug_log::write_log(
                "no_action",
                serde_json::json!({"reason": "config_load_failed", "error": format!("{err}")}),
            );
            return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
        }
    };

    // Config kill switch (plan 031 silent stop): stop the chain with only a
    // log line — no orchestrator call, no fast path, no overflow Phase 1/2.
    if config.paused {
        log::warn!(
            "[auto_prompt::decide] paused=true — silently stopping chain (session={:?})",
            thread.read(cx).session_id()
        );
        debug_log::write_log("no_action", serde_json::json!({"reason": "paused"}));
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

    // Verdict ping-pong (proposal 001): a stopped verdict thread between
    // rounds is normal, not a thread that needs continuing. Manual dispatch
    // never reaches this function, so explicit human intent still wins.
    if acp_thread::verdict::is_active(thread.read(cx).session_id()) {
        log::info!(
            "[auto_prompt::decide] verdict negotiation active — skipping (session={:?})",
            thread.read(cx).session_id()
        );
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "verdict_negotiation_active"}),
        );
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

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

    log::info!(
        "[auto_prompt::decide] Tools were used in this session (last_turn={}), continuing evaluation",
        used_tools
    );

    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::info!("[auto_prompt::decide] Thread was cancelled, skipping auto-prompt");
        let session_id_str = thread.read(cx).session_id().to_string();
        reset_iteration_with_session(&session_id_str);
        debug_log::write_log("no_action", serde_json::json!({"reason": "cancelled"}));
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

    log::info!("[auto_prompt::decide] Stop reason: {:?}", stop_reason);

    // Worker-requested stop (`<stop>` keyword, loop-bug): an explicit stop
    // keyword on its own line in the last assistant message ends the chain
    // deterministically — ahead of connectivity retries, idle accounting and
    // every fast path. This is the worker's only channel to halt chaining
    // when everything left is blocked or owner-gated; prose deferrals get
    // reworded every round and defeat the repetition caps.
    let last_assistant_message = thread.read(cx).last_assistant_message_text(cx);
    if last_assistant_message
        .as_deref()
        .is_some_and(message_requests_stop)
    {
        log::warn!(
            "[auto_prompt::decide] worker emitted the `{STOP_KEYWORD}` stop keyword — stopping chain (session={:?})",
            thread.read(cx).session_id()
        );
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "worker_stop_keyword"}),
        );
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

    // Before `chain::record_turn`: connectivity retries have their own bounded
    // budget and must not burn the chain's max-iterations runaway budget —
    // otherwise a short outage ends the chain a few retries in. Also before
    // any path that would call the dead API.
    if let Some(decision) = api_unreachable_decision(thread, &config, cx) {
        return DecidePrecheck::Decision(decision);
    }
    let session_id_str = thread.read(cx).session_id().to_string();
    let turn = chain::record_turn(
        &session_id_str,
        chain::turn_made_progress(thread.read(cx).entries()),
    );
    let iteration_count = turn.turn;

    // Rule-based check: if the last tool call was an interactive auth command
    // (browser login, device auth, etc.), the user is mid-flow — don't chain.
    if is_interactive_tool_pending(thread, cx) {
        log::info!("[auto_prompt::decide] Interactive auth tool pending, stopping");
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "interactive_tool_pending"}),
        );
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

    log::info!(
        "[auto_prompt::decide] Current iteration: {iteration_count} ({} without progress)",
        turn.idle_turns
    );

    if turn.idle_turns > config.max_iterations {
        log::info!(
            "[auto_prompt::decide] {} turns in a row without progress (max {}), stopping chain",
            turn.idle_turns,
            config.max_iterations
        );
        reset_iteration_with_session(&session_id_str);
        debug_log::write_log(
            "no_action",
            serde_json::json!({
                "reason": "max_iterations",
                "iteration": iteration_count,
                "idle_turns": turn.idle_turns,
                "max_iterations": config.max_iterations,
            }),
        );
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    }

    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry.default_model() else {
        log::warn!("[auto_prompt::decide] No language model configured in Zed");
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "no_model_configured"}),
        );
        return DecidePrecheck::Decision(AutoPromptDecision::NoAction);
    };
    let model = configured_model;
    log::info!("[auto_prompt::decide] Using model: {:?}", model.id());

    let verification_count = chain::verification_attempts(&session_id_str);
    let stop_phase = if verification_count == 0 {
        StopPhase::Working
    } else if verification_count >= config.max_verification_attempts {
        StopPhase::Verified
    } else {
        StopPhase::PreStop
    };

    // Plan 031 rules-base short circuit: the last message is ALREADY a
    // voluntary summary (the mandated `## Summary` handoff). The summary is
    // the authority — no orchestrator reasoning and no plan-file scans are
    // needed. Fork (over limit) or continue same-thread (under limit) via the
    // deterministic summary machines in `decide_with_llm`, with a LIGHT
    // LlmCallData: the full pipeline (origin git fetches across every work
    // dir + whole-thread context serialization) took ~45s of silent latency
    // before the fork, which read as "auto prompt not triggered".
    // Requires reported token usage: without it we can't classify over/under
    // limit cheaply, so fall through to the full pipeline (its chars/4
    // estimate keeps no-usage providers forking).
    let usage = thread
        .read(cx)
        .token_usage()
        .map(|usage| usage.input_tokens);
    if let Some(last_assistant_message) = last_assistant_message {
        if usage.is_some() && looks_like_voluntary_summary(&last_assistant_message) {
            let actual_input_tokens = usage;
            let effective_token_count = actual_input_tokens.unwrap_or(0) as usize;
            // A MaxTokens stop with a full window must still reach the Phase 1/2
            // machine: `skip_phase_1` detects the voluntary summary and forks
            // straight to Phase 2 — no doomed same-thread summarize request.
            let usage = thread.read(cx).token_usage();
            let window_full = matches!(stop_reason, acp::StopReason::MaxTokens)
                && max_tokens_context_window_full(
                    usage.map(|u| u.input_tokens),
                    usage.map(|u| u.output_tokens).unwrap_or(0),
                    model.max_token_count(),
                );
            let context_exceeds_limit =
                effective_token_count > config.max_context_tokens || window_full;
            let session_id = thread.read(cx).session_id().clone();
            let light_data = LlmCallData {
                model,
                system_prompt: String::new(),
                // Minimal: the summary machines never read plan files from
                // context (a NothingLeft summary routes to housekeeping
                // directly — endless housekeeping is the intended base state).
                context_json: serde_json::json!({ "session_id": session_id }).to_string(),
                project_root: None,
                session_id,
                title: thread.read(cx).title().map(|title| title.to_string()),
                iteration_count,
                max_verification_attempts: config.max_verification_attempts,
                work_dirs: thread
                    .read(cx)
                    .work_dirs()
                    .map(|plan| plan.paths().to_vec()),
                first_user_message: thread
                    .read(cx)
                    .entries()
                    .iter()
                    .find_map(|entry| match entry {
                        acp_thread::AgentThreadEntry::UserMessage(message) => {
                            Some(message.content.to_markdown(cx))
                        }
                        _ => None,
                    })
                    .filter(|text| !text.trim().is_empty()),
                original_user_message: None,
                last_assistant_message: Some(last_assistant_message),
                profile_id: None,
                actual_input_tokens,
                had_error: thread.read(cx).had_error(),
                had_api_error: thread.read(cx).had_api_error(),
                stop_phase,
                context_exceeds_limit,
                cutoff_context_full: false,
                approximate_token_count: 0,
                connection: None,
                project: None,
                peer_agent_states: peer_states::unmuted_states_for_context(),
                bloat_directive: output_bloat_directive(thread, cx),
            };
            log::info!(
                "[auto_prompt::decide] voluntary summary — rules-based handoff, skipping plan scans + context serialization (over_limit={context_exceeds_limit}, session={:?})",
                light_data.session_id
            );
            debug_log::write_log(
                "summary_light_path",
                serde_json::json!({
                    "over_limit": context_exceeds_limit,
                    "actual_input_tokens": actual_input_tokens,
                }),
            );
            return DecidePrecheck::Decision(AutoPromptDecision::NeedsLlmCall(light_data));
        }
    }

    DecidePrecheck::Proceed(Box::new(DecidePrecheckContext {
        project_root,
        iteration_count,
        config,
        stop_phase,
        verification_count,
        model,
    }))
}

/// Synchronous pre-check and decision.
///
/// Returns `NoAction` if auto-prompt should not fire (disabled, no tools,
/// cancelled, max iterations, no model configured).
/// Returns `NeedsLlmCall` for all non-trivial cases (LLM decides).
///
/// Prefer [`decide_async`], which runs the plan/doc file reads on a background
/// thread; this sync form exists for callers that cannot await.
pub fn decide(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    cx: &gpui::App,
) -> AutoPromptDecision {
    let pre = match decide_precheck(thread, used_tools, stop_reason, cx) {
        DecidePrecheck::Decision(decision) => return decision,
        DecidePrecheck::Proceed(pre) => pre,
    };
    let inputs = context_file_inputs(&thread.read(cx), cx);
    // Main thread, no executor handle for an async read here: serve the
    // last origin snapshot when fresh, otherwise the worktree fallback.
    // The primary dispatch path (decide_async) keeps the cache warm.
    let plan_files = read_plan_files_cached(&inputs);
    let doc_files = read_doc_files(inputs.work_dirs.as_deref().unwrap_or_default());
    decide_finish(thread, stop_reason, *pre, inputs, plan_files, doc_files, cx)
}

/// Non-blocking variant of [`decide`]: the `.plans/`/`.docs/` reads (up to 10
/// × 100KB files plus directory scans) and the O(thread) context serialization
/// run on a background executor so the UI thread stays responsive; only cheap
/// entity-source snapshots and the decision logic run on the main thread.
/// Behavior is identical to the sync path, except plan files are sourced from
/// origin's remote-tracking refs (with a gated `git fetch origin` first — see
/// `plan_source`), never the possibly-dirty working tree.
pub async fn decide_async(
    thread: gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: acp::StopReason,
    cx: &gpui::AsyncApp,
) -> AutoPromptDecision {
    let pre = match cx.update(|cx| decide_precheck(&thread, used_tools, &stop_reason, cx)) {
        DecidePrecheck::Decision(decision) => return decision,
        DecidePrecheck::Proceed(pre) => pre,
    };
    let inputs = cx.update(|cx| context_file_inputs(&thread.read(cx), cx));
    let bg_inputs = inputs.clone();
    let executor = cx.background_executor().clone();
    let plan_executor = executor.clone();
    let (plan_files, doc_files) = cx
        .background_executor()
        .spawn(async move {
            let work_dirs = bg_inputs.work_dirs.as_deref().unwrap_or_default();
            let doc_files = read_doc_files(work_dirs);
            // Background thread: refresh remote-tracking refs via a gated
            // `git fetch origin`, then read plans from them — never from the
            // possibly-dirty working tree.
            let plan_files = read_plan_files(&bg_inputs, Some(&plan_executor)).await;
            (plan_files, doc_files)
        })
        .await;
    // Main thread: cheap snapshot of the thread (entity-source clones only).
    let (snapshot, thread_title) = cx.update(|cx| {
        snapshot_decision_context(
            &thread,
            &stop_reason,
            &pre,
            &inputs,
            plan_files,
            doc_files,
            cx,
        )
    });
    // O(thread) markdown/JSON serialization runs off the UI thread.
    let mut auto_prompt_ctx = executor.spawn(async move { snapshot.finish() }).await;
    auto_prompt_ctx.stop_phase = pre.stop_phase.clone();
    auto_prompt_ctx.verification_count = pre.verification_count;
    cx.update(|cx| {
        decide_with_context(
            &thread,
            &stop_reason,
            *pre,
            auto_prompt_ctx,
            inputs.session_id,
            thread_title,
            inputs.work_dirs,
            cx,
        )
    })
}

/// Shared main-thread preamble of both decide paths: heartbeats the claimed
/// plan files, snapshots every `cx`-dependent read of the thread (cheap
/// clones — the O(thread) processing is offloaded by [`decide_async`]), and
/// reads the thread title.
fn snapshot_decision_context(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    stop_reason: &acp::StopReason,
    pre: &DecidePrecheckContext,
    inputs: &ContextFileInputs,
    plan_files: Vec<PlanFileContent>,
    doc_files: Vec<String>,
    cx: &gpui::App,
) -> (AutoPromptContextSnapshot, Option<String>) {
    let thread_ref = thread.read(cx);
    let stop_reason_str = format!("{stop_reason:?}").to_lowercase();
    let sid_str = inputs.session_id.to_string();
    for plan in &plan_files {
        plan_registry::heartbeat(&plan.path, &sid_str);
    }
    let snapshot = AutoPromptContext::snapshot(
        thread_ref,
        cx,
        stop_reason_str,
        plan_files,
        doc_files,
        pre.iteration_count,
    );
    let thread_title = thread_ref.title().map(|t| t.to_string());
    (snapshot, thread_title)
}

/// Second half of the sync decide pipeline: collects the full
/// [`AutoPromptContext`] on the main thread and runs the decision logic.
/// `decide_async` shares [`decide_with_context`] but runs the serialization
/// on a background executor.
fn decide_finish(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    stop_reason: &acp::StopReason,
    pre: DecidePrecheckContext,
    inputs: ContextFileInputs,
    plan_files: Vec<PlanFileContent>,
    doc_files: Vec<String>,
    cx: &gpui::App,
) -> AutoPromptDecision {
    let (snapshot, thread_title) = snapshot_decision_context(
        thread,
        stop_reason,
        &pre,
        &inputs,
        plan_files,
        doc_files,
        cx,
    );
    let mut auto_prompt_ctx = snapshot.finish();
    auto_prompt_ctx.stop_phase = pre.stop_phase.clone();
    auto_prompt_ctx.verification_count = pre.verification_count;
    decide_with_context(
        thread,
        stop_reason,
        pre,
        auto_prompt_ctx,
        inputs.session_id,
        thread_title,
        inputs.work_dirs,
        cx,
    )
}

/// Decision logic over an already-collected [`AutoPromptContext`]. Runs on the
/// main thread (one `thread.read(cx)` for the session-limit check); shared by
/// the sync and async decide paths.
#[allow(clippy::too_many_arguments)]
fn decide_with_context(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    stop_reason: &acp::StopReason,
    pre: DecidePrecheckContext,
    auto_prompt_ctx: AutoPromptContext,
    session_id: acp::SessionId,
    thread_title: Option<String>,
    work_dirs: Option<Vec<std::path::PathBuf>>,
    cx: &gpui::App,
) -> AutoPromptDecision {
    let DecidePrecheckContext {
        project_root,
        iteration_count,
        config,
        stop_phase,
        model,
        ..
    } = pre;

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

    let mut cutoff_context_full = false;
    // MaxTokens = the response stream was cut off mid-generation. Two very
    // different sub-cases share this stop reason:
    //
    // - Output cap: the window has room — the context is intact and the right
    //   move is to resume the SAME thread. Forking here (the old behavior)
    //   threw the whole conversation away: `build_prompt_summary` ignores the
    //   last message, so the new thread got title + "context limit reached"
    //   and no summary (2026-09-28 stream-cutoff bug).
    // - Window full: input + output ≈ the model window. Every same-thread
    //   request is now rejected by the API (input alone fills the window), so
    //   neither a resume nor the Phase 1 summarize request can run. Route to
    //   `cutoff_context_full_outcome`, which produces the handoff summary with
    //   a bounded orchestrator call and then forks with a real summary.
    if matches!(stop_reason, acp::StopReason::MaxTokens) {
        let usage = thread.read(cx).token_usage();
        let usage_input = usage.map(|u| u.input_tokens);
        let usage_output = usage.map(|u| u.output_tokens).unwrap_or(0);
        if max_tokens_context_window_full(usage_input, usage_output, model.max_token_count()) {
            log::warn!(
                "auto_prompt: MaxTokens with context window full (input={:?}, output={usage_output}, window={}) — routing to orchestrator-summary fork",
                usage_input,
                model.max_token_count(),
            );
            cutoff_context_full = true;
            // Fall through to the shared NeedsLlmCall construction below.
        } else {
            let effective_token_count = auto_prompt_ctx
                .actual_input_tokens
                .map(|t| t as usize)
                .unwrap_or(auto_prompt_ctx.approximate_token_count);
            if effective_token_count > config.max_context_tokens {
                // Over the soft gate but with window room: the existing Phase
                // 1/2 machine summarizes same-thread (the request fits) and
                // forks with that summary.
                log::info!(
                    "auto_prompt: MaxTokens with window room but over the soft token gate — falling through to the Phase 1/2 machine"
                );
            } else {
                log::info!(
                    "auto_prompt: MaxTokens is an output-cap cutoff (input={:?}, output={usage_output}) with window room — resuming same thread",
                    usage_input
                );
                // The generation just proved the thread can blow the output
                // budget: name the hog on the resume prompt, or the resume
                // dies the same death (and the chain loops).
                let next_prompt = match output_bloat_directive(thread, cx) {
                    Some(directive) => format!("{MAX_TOKENS_RESUME_PROMPT}\n\n{directive}"),
                    None => MAX_TOKENS_RESUME_PROMPT.to_string(),
                };
                return AutoPromptDecision::DispatchNow(AutoPromptAction {
                    from_session_id: session_id,
                    from_title: thread_title,
                    next_prompt,
                    work_dirs,
                    original_user_message,
                    profile_id: None,
                    actual_input_tokens: auto_prompt_ctx.actual_input_tokens,
                    approximate_token_count: auto_prompt_ctx.approximate_token_count,
                    last_assistant_message: _last_assistant_msg,
                    force_new_thread: false,
                    focus_new_thread: false,
                });
            }
        }
    }

    if matches!(stop_reason, acp::StopReason::Refusal) {
        let delay = config.backoff_delay_ms(iteration_count);
        log::warn!(
            "[auto_prompt::decide] PATH=refusal_bypass: stop_reason={:?}, iteration={} → DispatchAfterDelay({}ms) (LLM bypassed)",
            stop_reason,
            iteration_count,
            delay
        );
        let next_prompt = with_first_prompt_context(
            "The model refused the request. Retry from where we left off.".to_string(),
            build_prompt_summary(
                None,
                thread_title.as_deref(),
                Some("refusal, retrying"),
                _last_assistant_msg.as_deref(),
                original_user_message.as_deref(),
                auto_prompt_ctx.first_user_message.as_deref(),
            )
            .as_deref(),
            thread_title.as_deref(),
            _last_assistant_msg.as_deref(),
        );
        return AutoPromptDecision::DispatchAfterDelay {
            action: AutoPromptAction {
                from_session_id: session_id,
                from_title: thread_title,
                next_prompt,
                work_dirs,
                original_user_message,
                profile_id: None,
                actual_input_tokens: auto_prompt_ctx.actual_input_tokens,
                approximate_token_count: auto_prompt_ctx.approximate_token_count,
                last_assistant_message: _last_assistant_msg,
                force_new_thread: false,
                focus_new_thread: false,
            },
            delay_ms: delay,
            reason: AutoPromptDelayReason::Backoff,
        };
    }

    // Usage limit: the provider embeds the reset time in the turn error or
    // a synthetic message (e.g. Claude Code's "You've hit your session limit
    // · resets 1:20am (Asia/Bangkok)"; weekly/Opus variants carry a weekday
    // prefix). Schedule the continuation at reset + margin instead of
    // burning the orchestration call against the same exhausted quota.
    // See .plans/018_session_limit_scheduled_retry.md.
    if let Some(limit) = crate::session_limit::session_limit_from_thread(
        &thread.read(cx),
        cx,
        config.session_limit_margin_secs,
    ) {
        log::warn!(
            "[auto_prompt::decide] PATH=session_limit: reset at {} — scheduling continuation in {}ms",
            limit.retry_display,
            limit.retry_delay_ms
        );
        debug_log::write_log(
            "session_limit_scheduled",
            serde_json::json!({
                "retry_at": limit.retry_at.to_rfc3339(),
                "delay_ms": limit.retry_delay_ms,
            }),
        );
        let last_assistant_message = _last_assistant_msg
            .filter(|message| !crate::session_limit::looks_like_usage_limit(message));
        let next_prompt = with_first_prompt_context(
            "The provider session limit window has reset. Continue from where we left off."
                .to_string(),
            build_prompt_summary(
                None,
                thread_title.as_deref(),
                Some("session limit reset, retrying"),
                last_assistant_message.as_deref(),
                original_user_message.as_deref(),
                auto_prompt_ctx.first_user_message.as_deref(),
            )
            .as_deref(),
            thread_title.as_deref(),
            last_assistant_message.as_deref(),
        );
        return AutoPromptDecision::DispatchAfterDelay {
            action: AutoPromptAction {
                from_session_id: session_id,
                from_title: thread_title,
                next_prompt,
                work_dirs,
                original_user_message,
                profile_id: None,
                actual_input_tokens: auto_prompt_ctx.actual_input_tokens,
                approximate_token_count: auto_prompt_ctx.approximate_token_count,
                last_assistant_message,
                force_new_thread: false,
                focus_new_thread: false,
            },
            delay_ms: limit.retry_delay_ms,
            reason: AutoPromptDelayReason::UsageLimitReset,
        };
    }

    if auto_prompt_ctx.had_error {
        log::info!(
            "[auto_prompt::decide] had_error=true but proceeding to LLM call (error is non-fatal, LLM decides)"
        );
    }

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
            debug_log::write_log(
                "no_action",
                serde_json::json!({"reason": "context_serialize_failed", "error": format!("{err}")}),
            );
            return AutoPromptDecision::NoAction;
        }
    };

    let last_assistant_message = auto_prompt_ctx
        .last_assistant_message()
        .map(|s| s.to_string());

    // Use actual_input_tokens from the API when available, falling back to
    // the approximate chars/4 estimate. The approximate count only reflects
    // the auto_prompt context JSON, not the full thread — so it underestimates.
    let effective_token_count = auto_prompt_ctx
        .actual_input_tokens
        .map(|t| t as usize)
        .unwrap_or(auto_prompt_ctx.approximate_token_count);
    let context_exceeds_limit =
        effective_token_count > config.max_context_tokens || cutoff_context_full;
    if context_exceeds_limit {
        log::info!(
            "[auto_prompt::decide] Context exceeds limit (effective={effective_token_count} > {} tokens, actual={:?}, approx={}) — will use lightweight path",
            config.max_context_tokens,
            auto_prompt_ctx.actual_input_tokens,
            auto_prompt_ctx.approximate_token_count
        );
    }

    log::info!("[auto_prompt::decide] Returning NeedsLlmCall decision");
    debug_log::write_log(
        "needs_llm_call",
        serde_json::json!({
            "iteration": iteration_count,
            "stop_phase": format!("{:?}", stop_phase),
            "context_exceeds_limit": context_exceeds_limit,
            "had_error": auto_prompt_ctx.had_error,
            "model": format!("{:?}", model.id()),
            "last_assistant_message": debug_log::truncate(
                last_assistant_message.as_deref().unwrap_or(""),
                2000,
            ),
        }),
    );
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
        had_api_error: auto_prompt_ctx.had_api_error,
        stop_phase,
        context_exceeds_limit,
        cutoff_context_full,
        approximate_token_count: auto_prompt_ctx.approximate_token_count,
        connection: None,
        project: None,
        peer_agent_states: peer_states::unmuted_states_for_context(),
        bloat_directive: output_bloat_directive(thread, cx),
    })
}

/// Char-boundary-safe truncation (project rule: never slice a `&str` by byte
/// index without a boundary check — summaries routinely contain em-dashes
/// and CJK). This is the canonical truncation helper referenced by the repo
/// `.rules` for log/preview truncation. The limit counts chars, not bytes; for
/// a byte budget slice with `&text[..text.floor_char_boundary(max_bytes)]`.
pub fn truncate_at_char_boundary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

/// Build a one-shot pros/cons clarification request (plan 023 D, req 5).
///
/// Fires only when ALL hold:
/// - the clarify prompt has not already fired for this chain (`CLARIFY_REGISTRY`),
/// - the worker's last message presents an options/decision point it could
///   resolve itself (`pending_question::mentions_decision_point`),
/// - the worker has not already laid out pros/cons (`has_pros_cons_layout`).
///
/// Genuine user-input requests (credentials, explicit deferral) are excluded
/// upstream by `is_waiting_for_user_decision`, which runs before this check.
fn maybe_clarification_request(data: &LlmCallData) -> Option<AutoPromptOutcome> {
    let session_id_str = data.session_id.to_string();
    if clarify_fired_for(&session_id_str) {
        return None;
    }
    let last = data
        .last_assistant_message
        .as_deref()
        .map(str::trim)
        .filter(|msg| !msg.is_empty())?;
    if pending_question::has_pros_cons_layout(Some(last)) {
        return None;
    }
    if !pending_question::mentions_decision_point(Some(last)) {
        return None;
    }
    mark_clarify_fired(&session_id_str);
    let prompt = "Before deciding, list the pros and cons of each option you mentioned, then recommend one and proceed with it.".to_string();
    let mut action = data.make_continue_action(prompt);
    action.force_new_thread = false;
    Some(AutoPromptOutcome::ClarificationRequest(action))
}

/// Whether the retry-exhaustion path in `decide_with_llm` (all lightweight
/// retries failed) must defer (`RetryAfterBackoff`) instead of running the
/// no-LLM safety nets (`detect_remaining_work` / `detect_remaining_plan_tasks`).
/// When the source thread's turn itself failed at the API level, a safety-net
/// "continue" dispatches back into the same blocked API forever (the
/// send → 405 → decide → send loop that froze Zed while the GLM endpoint was
/// WAF-blocked, 2026-09-11). Extracted so the rule is directly testable.
fn should_defer_after_retry_exhaustion(had_api_error: bool) -> bool {
    had_api_error
}

/// Async LLM call to determine the next prompt.
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

    let session_id_str = data.session_id.to_string();

    // ── MaxTokens + window full cutoff (plan 032) ──────────────────
    //
    // The stream was cut off because the conversation itself filled the
    // model's context window. Every same-thread request is now rejected by
    // the API, so neither the output-cap resume nor the Phase 1 summarize
    // request can run — produce the handoff summary with a bounded
    // orchestrator call and fork with it. Checked FIRST: the truncated
    // mid-reasoning last message is neither a summary nor a question, so the
    // fast paths below it have nothing to work with.
    if data.cutoff_context_full {
        log::warn!(
            "[auto_prompt::decide_with_llm] MaxTokens + window full — orchestrating handoff summary then forking (session={session_id_str})"
        );
        return Ok(cutoff_context_full_outcome(&data, &cx).await);
    }

    // Worker-requested stop (`<stop>` keyword, loop-bug): defense in depth —
    // decide_precheck / decide_claude already gate on this, but data can be
    // constructed directly (tests, manual dispatches), so re-check before any
    // fast path answers a stop request with a continuation.
    if data
        .last_assistant_message
        .as_deref()
        .is_some_and(message_requests_stop)
    {
        log::warn!(
            "[auto_prompt::decide_with_llm] worker emitted the `{STOP_KEYWORD}` stop keyword — stopping chain (session={session_id_str})"
        );
        return Ok(AutoPromptOutcome::Stopped {
            reason: format!("worker requested stop ({STOP_KEYWORD} keyword)"),
        });
    }

    // ── Summary-first fast path (plan 027, tightened plan 031) ──────────
    //
    // A voluntary summary at the last paragraph carries its own "what
    // remains" — RULES ONLY, no orchestrator call and no pending-question
    // LLM pass (a summary is not a question; the rules are authoritative):
    //
    // - over the context limit → None here; the Phase 1/2 machine below
    //   forks a new thread whose Decision slot carries the rules decision
    //   (summary steps + fixed directive / housekeeping / stop);
    // - under the limit → continue SAME-THREAD with the summary's next
    //   steps + fixed directive, or housekeeping when the summary declares
    //   nothing left (endless housekeeping is the intended base state);
    // - TERMINAL summary (all remaining work armed/deferred/owner-gated) →
    //   Some(Stopped), chain ends right here (plan 031).
    if let Some(outcome) = summary_continuation_fast_path(&data) {
        return Ok(outcome);
    }

    // ── Pending-question fast path ─────────────────────────────────────
    //
    // If the worker stopped to ASK THE USER a question ("Which do you want?
    // Option A or Option B?", "Want me to do that?"), answer it directly via
    // a targeted LLM call on the last 2-3 paragraphs and dispatch that answer.
    // Runs AFTER the summary fast path: a voluntary summary is never a
    // question, and this pass is an LLM call the rules-base must not pay.
    // On any failure (no question, LLM unreachable, low confidence) the helper
    // returns Ok(None) and we fall through to the normal decision flow — the
    // user explicitly required that uncertain cases still reach stop/summary.
    if let Some(outcome) = pending_question::try_answer_pending_question(&data, cx).await? {
        log::warn!(
            "[auto_prompt::decide_with_llm] Pending-question fast path fired — dispatching answer (session={session_id_str})"
        );
        return Ok(outcome);
    }

    if data.context_exceeds_limit {
        // Shared Phase 1/2 state machine (plan 023 A2) — also reused by the
        // Claude overflow parity path (plan 023 A3). No reasoning pass: when a
        // summary exists it is used verbatim with the fixed decision directive
        // (plan 027 "do not process"); when it doesn't, Phase 1 asks for one.
        let outcome = context_overflow_outcome(&data);
        return Ok(outcome);
    }

    // Use lightweight context: last assistant message + plan summaries only.
    // Reduces token usage from ~80K to ~500 tokens.
    let lightweight_context = lightweight_context::build_lightweight_orchestration_context(
        &data.context_json,
        &data.stop_phase,
        data.iteration_count,
        data.had_error,
    );
    log::info!(
        "[auto_prompt::decide_with_llm] Using lightweight context ({} chars) instead of full context ({} chars)",
        lightweight_context.len(),
        data.context_json.len(),
    );
    let result =
        call_language_model(&data.model, &data.system_prompt, &lightweight_context, cx).await;

    log::info!(
        "[auto_prompt::decide_with_llm] LLM call completed with result: {:?}",
        result.is_ok()
    );

    let mut response = match result {
        Ok((_raw_response, response)) => response,
        Err(err) => {
            log::warn!("auto_prompt: language model call failed: {err}");
            return Err(err);
        }
    };

    let has_prompt = response
        .next_prompt
        .as_ref()
        .is_some_and(|p| !p.trim().is_empty());

    let is_synthetic_failure = model_call::is_synthetic_failure(&response);

    log::info!(
        "[auto_prompt::decide_with_llm] Response received: confidence={:?}, has_next_prompt={:?}",
        response.confidence,
        has_prompt,
    );

    if let Some(reason) = &response.reason {
        log::info!("[auto_prompt::decide_with_llm] Reason: {reason}");
    }

    if let Some(prompt) = &response.next_prompt {
        log::info!("[auto_prompt::decide_with_llm] Next prompt: {prompt}");
    }

    let prompt_summary = build_prompt_summary(
        response.thread_summary.as_deref(),
        data.title.as_deref(),
        response.reason.as_deref(),
        data.last_assistant_message.as_deref(),
        data.original_user_message.as_deref(),
        data.first_user_message.as_deref(),
    );

    if is_synthetic_failure {
        log::warn!(
            "[auto_prompt::decide_with_llm] Synthetic failure detected: confidence={:?}, reason={:?} — entering lightweight retry path",
            response.confidence,
            response.reason
        );
    }

    let input = EvaluationInput {
        confidence: response.confidence,
        next_prompt: std::mem::take(&mut response.next_prompt),
        reason: std::mem::take(&mut response.reason),
        last_assistant_message: data.last_assistant_message.clone(),
        is_synthetic_failure,
        stop_phase: data.stop_phase.clone(),
    };

    log::info!(
        "[auto_prompt::decide_with_llm] evaluate_response input: confidence={:?}, stop_phase={:?}",
        input.confidence,
        input.stop_phase,
    );

    let evaluation = evaluate_response(&input);

    debug_log::write_log(
        "evaluate_response",
        serde_json::json!({
            "confidence": input.confidence,
            "has_prompt": input.next_prompt.as_ref().is_some_and(|p| !p.trim().is_empty()),
            "is_synthetic_failure": input.is_synthetic_failure,
            "stop_phase": format!("{:?}", input.stop_phase),
            "source": format!("{:?}", evaluation.source()),
            "result": format!("{:?}", evaluation),
            "last_assistant_message": debug_log::truncate(
                input.last_assistant_message.as_deref().unwrap_or(""),
                2000,
            ),
        }),
    );

    log::info!(
        "[auto_prompt::decide_with_llm] evaluate_response: source={:?}, result={:?}",
        evaluation.source(),
        evaluation
    );

    Ok(verdict_dispatch::dispatch_evaluation(
        evaluation,
        &input,
        &data,
        prompt_summary.as_deref(),
        cx,
    )
    .await)
}

/// Build the prompt summary for the next chained thread.
///
/// Priority:
/// 1. LLM-generated `thread_summary` (preferred — comprehensive, with active plan bolded)
/// 2. Synthesized from title + reason (when LLM returns null)
/// 3. Raw `original_user_message` carried from thread 0 (last resort before final fallback)
/// 4. Extracted from `first_user_message` (absolute fallback)
pub fn build_prompt_summary(
    thread_summary: Option<&str>,
    title: Option<&str>,
    reason: Option<&str>,
    _last_assistant_message: Option<&str>,
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

/// Checks if the LAST terminal tool call suggests the worker is blocked on an
/// interactive flow (browser-based auth, login) and auto_prompt should wait.
/// Only the most recent terminal tool call counts: if the worker ran further
/// commands after an auth-shaped one, it was not blocked on it. Scanning the
/// whole turn killed chains on stale matches (2026-09-01: a fleet repo-sync
/// `for r in …` loop tripped the substring matcher and silently stopped a
/// chain whose worker had long moved on and posted its summary).
fn is_interactive_tool_pending(thread: &gpui::Entity<acp_thread::AcpThread>, cx: &App) -> bool {
    let thread_ref = thread.read(cx);
    let Some(command) = latest_terminal_command(thread_ref.entries()) else {
        return false;
    };
    if is_interactive_auth_command(&command) {
        log::warn!(
            "[auto_prompt::decide] Latest terminal call is an interactive auth command: '{}' — pausing (chain resumes on next stop)",
            command.chars().take(100).collect::<String>()
        );
        true
    } else {
        false
    }
}

/// The command of the MOST RECENT terminal tool call before the last user
/// message. `None` when there is none (or it carries no command) — the
/// caller treats that as "not pending" rather than scanning deeper into the
/// turn for stale auth-shaped calls.
fn latest_terminal_command(entries: &[acp_thread::AgentThreadEntry]) -> Option<String> {
    for entry in entries.iter().rev() {
        match entry {
            acp_thread::AgentThreadEntry::UserMessage(_) => return None,
            acp_thread::AgentThreadEntry::ToolCall(tool) => {
                if tool.tool_name.as_deref() != Some("terminal") {
                    continue;
                }
                return tool
                    .raw_input
                    .as_ref()
                    .and_then(|input| input.get("command"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            _ => continue,
        }
    }
    None
}

/// Concrete invocations that open a browser or wait for external user action
/// (auth flows, device login). Matched against the START of a shell segment
/// (`&&` / `;` / `|` / `&` / newline / backtick separated) — never as a
/// substring of the whole command, so compound scripts that merely MENTION
/// auth words (repo-sync for-loops, `gh auth status` health checks, greps)
/// do not trip the guard. Missing an exotic flow is safe: the chain just
/// continues and the worker's next turn fails visibly; a false positive
/// silently killed chains.
const INTERACTIVE_AUTH_INVOCATIONS: &[&str] = &[
    "login",
    "gh auth login",
    "gh auth refresh",
    "gh auth switch",
    "gcloud auth login",
    "gcloud auth application-default login",
    "aws sso login",
    "az login",
    "az devops login",
    "vercel login",
    "netlify login",
    "wrangler login",
    "supabase login",
    "firebase login",
    "docker login",
    "oras login",
    "crane auth login",
    "npm login",
    "npm adduser",
    "pnpm login",
    "yarn login",
    "cargo login",
    "huggingface-cli login",
    "hf auth login",
    "wandb login",
    "claude login",
    "codex login",
    "gemini login",
    "op login",
];

fn is_interactive_auth_command(command: &str) -> bool {
    let lower = command.to_lowercase();
    lower
        .split(|c: char| matches!(c, '&' | ';' | '|' | '\n' | '(' | '`'))
        .map(str::trim_start)
        .any(|segment| {
            INTERACTIVE_AUTH_INVOCATIONS
                .iter()
                .any(|invocation| segment.starts_with(invocation))
        })
}

/// Reset the auto-prompt chain counters **and** release any plan claims held by
/// the given session. Call this at every chain stop point so competing agents
/// can pick up the released plans.
pub fn reset_iteration_with_session(session_id: &str) {
    clear_summary_for_session(session_id);
    chain::clear_session(session_id);
    plan_registry::release_all_for_session(session_id);
}

/// Withdraw a Phase 1 summarize request that was decided but never sent.
/// Leaving `summary_state == 1` behind would make the next stop treat the
/// agent's ordinary reply as the requested summary and fork on it.
pub fn abandon_summary_request(session_id: &str) {
    clear_summary_for_session(session_id);
}

pub fn increment_llm_failure_count(session_id: &str) -> u32 {
    chain::record_llm_failure(session_id)
}

pub fn reset_llm_failure_count(session_id: &str) {
    chain::reset_llm_failures(session_id);
}

/// Build a concise summary of remaining plans grouped by project folder.
/// Lists actionable unchecked tasks (skipping "Out of Scope", "Deferred" sections).
/// Returns None if no plans have actionable unchecked items.
fn build_plan_landscape(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Context {
        #[serde(default)]
        session_id: Option<String>,
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: Context = serde_json::from_str(context_json).ok()?;
    let session_id = ctx.session_id.as_deref().unwrap_or("");

    type Project = String;
    type PlanLine = String;
    let mut groups: Vec<(Project, Vec<PlanLine>)> = Vec::new();

    // Extract the active project from the first plan file's path prefix,
    // then use it to sort same-repo plans first.
    let active_project = ctx.plan_files.first().and_then(|f| {
        let path = std::path::Path::new(&f.path);
        path.parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
    });

    for file in &ctx.plan_files {
        let task_count = count_actionable_tasks(&file.content);
        if task_count == 0 {
            continue;
        }

        if plan_registry::is_claimed_by_other(&file.path, session_id) {
            log::info!(
                "[auto_prompt::build_plan_landscape] Skipping plan claimed by another agent: {}",
                file.path
            );
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

    // Sort groups: active project's repo first, then alphabetical.
    // Within each group, plans are already in the order from read_plan_files
    // (active project first, then newest plan index first).
    if let Some(ref active) = active_project {
        groups.sort_by(|a, b| {
            let a_active = a.0 == *active;
            let b_active = b.0 == *active;
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.0.cmp(&b.0),
            }
        });
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

/// Extract the active project path from a user message.
///
/// Recognizes three patterns:
/// 1. `file:///abs/path/.plans/xxx.md` — file URL
/// 2. `zed:///agent/thread/...` followed elsewhere by a plain path with `/.plans/` or `/.plan/`
/// 3. Bare absolute paths like `/Users/foo/proj/.plans/239_bar.md`
///
/// Returns the project root (e.g. `/Users/foo/proj`) on success.
fn extract_active_project(msg: &str) -> Option<String> {
    // Strategy 1: find `file:///` URL
    if let Some(idx) = msg.find("file:///") {
        let path = &msg[idx + 7..];
        let end = path
            .find(|c: char| c == ')' || c == ' ' || c == '\n' || c == '"' || c == '`')
            .unwrap_or(path.len());
        let full_path = &path[..end];
        if let Some(pos) = full_path.rfind("/.plans/") {
            return Some(full_path[..pos].to_string());
        }
        if let Some(pos) = full_path.rfind("/.plan/") {
            return Some(full_path[..pos].to_string());
        }
    }

    // Strategy 2: scan for any absolute path containing `/.plans/` or `/.plan/`.
    // This catches bare paths like `/Users/katopz/git/riir-ai/.plans/239_fol_game_rule_extraction.md`
    // and markdown links like `riir-ai/.plans/239_fol_game_rule_extraction.md`.
    let plan_dir_patterns = ["/.plans/", "/.plan/"];
    let mut best_match: Option<&str> = None;
    let mut best_len = 0;

    for pattern in &plan_dir_patterns {
        let mut search_from = 0;
        while let Some(idx) = msg[search_from..].find(pattern) {
            let abs_start = search_from + idx;
            // Walk backwards to find the start of the path (either start of string, whitespace, or common delimiters)
            let path_start = msg[..abs_start]
                .rfind(|c: char| matches!(c, ' ' | '\n' | '(' | '`' | '"' | '>' | '['))
                .map(|i| i + 1)
                .unwrap_or(0);
            let candidate = &msg[path_start..abs_start];
            // Prefer longer prefixes — more specific project paths win
            if candidate.len() > best_len {
                best_len = candidate.len();
                best_match = Some(candidate);
            }
            search_from = abs_start + pattern.len();
        }
    }

    best_match.map(|s| s.to_string())
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

/// Detect if the LLM's next_prompt references a plan number that is HIGHER than
/// an existing plan with unchecked tasks. This catches the pattern where the
/// orchestration LLM skips blocked/easy plans to work on a shiny new plan.
///
/// Returns the lowest-numbered plan file with unchecked tasks if the LLM is
/// trying to skip it, or None if the prompt is fine.
fn detect_plan_skip(prompt: &str, context_json: &str) -> Option<String> {
    let prompt_lower = prompt.to_lowercase();

    // Extract all plan numbers mentioned in the prompt
    let prompt_plan_numbers: Vec<u32> = extract_plan_numbers(&prompt_lower);
    if prompt_plan_numbers.is_empty() {
        return None;
    }
    let highest_prompt_plan = *prompt_plan_numbers.iter().max()?;

    // Parse plan files from context
    #[derive(serde::Deserialize)]
    struct PlanFile {
        path: String,
        content: String,
    }
    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        plan_files: Vec<PlanFile>,
    }
    let ctx: Ctx = serde_json::from_str(context_json).ok()?;

    let mut lowest_unchecked: Option<(u32, String)> = None;

    for plan in &ctx.plan_files {
        let numbers = extract_plan_numbers(&plan.path.to_lowercase());
        let Some(plan_num) = numbers.first() else {
            continue;
        };

        if has_unchecked_items(&plan.content) {
            match &lowest_unchecked {
                None => lowest_unchecked = Some((*plan_num, plan.path.clone())),
                Some((current_low, _)) if plan_num < current_low => {
                    lowest_unchecked = Some((*plan_num, plan.path.clone()));
                }
                _ => {}
            }
        }
    }

    let Some((lowest_num, lowest_path)) = &lowest_unchecked else {
        return None;
    };

    if *lowest_num < highest_prompt_plan {
        log::info!(
            "auto_prompt: PLAN SKIP DETECTED — prompt references plan {highest_prompt_plan} but plan {lowest_num} ({lowest_path}) has unchecked tasks"
        );
        Some(lowest_path.clone())
    } else {
        None
    }
}

/// Extract plan numbers from a string (e.g. "plan 292" → 292, "292_worms" → 292).
fn extract_plan_numbers(text: &str) -> Vec<u32> {
    let mut numbers = Vec::new();
    // Match patterns like "plan 292", "plan_292", "292_foo", ".plans/292_"
    for part in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if let Ok(num) = part.parse::<u32>() {
            if num >= 100 {
                // Only consider plan numbers >= 100 to avoid false positives
                numbers.push(num);
            }
        }
    }
    numbers
}

/// Build a correction prompt that redirects to the lowest-numbered plan with
/// unchecked tasks, explicitly overriding the LLM's plan-skip attempt.
fn build_plan_correction_prompt(plan_path: &str) -> String {
    format!(
        "Do NOT start a new plan. Continue with the lowest-numbered remaining plan: {}. \
         Read the plan file, identify the next unchecked task, and implement it. \
         GPU training, benchmarks, WASM, and external dependencies are NOT valid reasons to skip — implement them. \
         Mark completed steps as [x].",
        plan_path
    )
}

#[cfg(test)]
mod auto_prompt_tests;
