//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and decides whether
//! a follow-up prompt should be dispatched.
//!
//! This crate contains the decision logic only. The caller (agent_ui)
//! handles the actual GPUI action dispatch.

mod config;
pub mod claude_agent;
pub mod context;
pub(crate) mod debug_log;
pub mod lightweight_context;
mod pending_question;
pub mod peer_states;
pub mod plan_registry;
pub mod session_limit;
pub mod watchdog;

pub use config::AutoPromptConfig;
pub use context::{
    AutoPromptContext, AutoPromptResponse, PlanFileContent, StopPhase, truncate_to_paragraph_budget,
};
pub use plan_registry::ActivePlanClaim;

use agent_client_protocol::schema::v1 as acp;
use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use gpui::App;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use lightweight_context::{count_actionable_tasks, has_unchecked_items, is_actionable_checkbox};
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
}

/// Extract the decision text from a `with_first_prompt_context`-formatted string.
/// Returns the content after the last `## N. Decision` header, or None if not found.
pub fn extract_decision_prompt(prompt: &str) -> Option<String> {
    let marker = "## 3. Decision";
    let alt_marker = "## 2. Decision";
    let start = prompt
        .find(marker)
        .map(|i| i + marker.len())
        .or_else(|| prompt.find(alt_marker).map(|i| i + alt_marker.len()));
    start
        .map(|i| prompt[i..].trim().to_string())
        .filter(|s| !s.is_empty())
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
            .field("approximate_token_count", &self.approximate_token_count)
            .finish()
    }
}

impl LlmCallData {
    pub(crate) fn make_continue_action(&self, next_prompt: String) -> AutoPromptAction {
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

/// Synchronous pre-check and decision.
///
/// Returns `NoAction` if auto-prompt should not fire (disabled, no tools,
/// cancelled, max iterations, no model configured).
/// Returns `NeedsLlmCall` for all non-trivial cases (LLM decides).
pub fn decide(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    cx: &App,
) -> AutoPromptDecision {
    log::info!("[auto_prompt::decide] Starting decision process");

    let project_root = thread
        .read(cx)
        .work_dirs()
        .and_then(|pl| pl.paths().first().cloned());
    let iteration_count = get_iteration();

    debug_log::write_log(
        "decide_entry",
        serde_json::json!({
            "stop_reason": format!("{stop_reason:?}"),
            "used_tools": used_tools,
            "iteration": iteration_count,
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

    log::info!(
        "[auto_prompt::decide] Tools were used in this session (last_turn={}), continuing evaluation",
        used_tools
    );

    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::info!("[auto_prompt::decide] Thread was cancelled, skipping auto-prompt");
        let session_id_str = thread.read(cx).session_id().to_string();
        reset_iteration_with_session(&session_id_str);
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "cancelled"}),
        );
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Stop reason: {:?}", stop_reason);

    // Rule-based check: if the last tool call was an interactive auth command
    // (browser login, device auth, etc.), the user is mid-flow — don't chain.
    if is_interactive_tool_pending(thread, cx) {
        log::info!("[auto_prompt::decide] Interactive auth tool pending, stopping");
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "interactive_tool_pending"}),
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
        let session_id_str = thread.read(cx).session_id().to_string();
        clear_summary_for_session(&session_id_str);
        reset_iteration_with_session(&session_id_str);
        debug_log::write_log(
            "no_action",
            serde_json::json!({
                "reason": "max_iterations",
                "iteration": iteration_count,
                "max_iterations": config.max_iterations,
            }),
        );
        return AutoPromptDecision::NoAction;
    }

    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let Some(configured_model) = registry.default_model() else {
        log::warn!("[auto_prompt::decide] No language model configured in Zed");
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "no_model_configured"}),
        );
        return AutoPromptDecision::NoAction;
    };
    let model = configured_model.model;
    log::info!("[auto_prompt::decide] Using model: {:?}", model.id());

    let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
    let stop_phase = if verification_count == 0 {
        StopPhase::Working
    } else if verification_count >= config.max_verification_attempts {
        StopPhase::Verified
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
        let sid = thread_ref.session_id().clone();
        let sid_str = sid.to_string();
        for plan in &plan_files {
            plan_registry::heartbeat(&plan.path, &sid_str);
        }
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

    // MaxTokens is a hard context limit, not a transient error.
    // No amount of waiting will help — dispatch new thread immediately.
    if matches!(stop_reason, acp::StopReason::MaxTokens) {
        log::info!(
            "auto_prompt: MaxTokens reached (context limit), dispatching new thread immediately"
        );

        // Preserve slash commands through context overflow so the new thread
        // re-activates the skill and continues the loop.
        let max_tokens_continuation = match original_user_message.as_deref() {
            Some(msg) if msg.trim().starts_with('/') => {
                let cmd = msg.trim();
                log::info!(
                    "auto_prompt: MaxTokens — original message is slash command '{cmd}', preserving it"
                );
                format!(
                    "{cmd}\n\nContext limit reached. Pick up where we left off. \
                     Do NOT summarize — continue working immediately."
                )
            }
            _ => "Context limit reached. Continue from where we left off.".to_string(),
        };
        let next_prompt = with_first_prompt_context(
            max_tokens_continuation,
            build_prompt_summary(
                None,
                thread_title.as_deref(),
                Some("context limit reached (MaxTokens)"),
                _last_assistant_msg.as_deref(),
                original_user_message.as_deref(),
                auto_prompt_ctx.first_user_message.as_deref(),
            )
            .as_deref(),
            thread_title.as_deref(),
            _last_assistant_msg.as_deref(),
        );
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
        };
    }

    // Session limit: the provider embeds the reset time in the turn error or
    // a synthetic message (e.g. Claude Code's "You've hit your session limit
    // · resets 1:20am (Asia/Bangkok)"). Schedule the continuation at
    // reset + margin instead of burning the orchestration call against the
    // same exhausted quota. See .plans/018_session_limit_scheduled_retry.md.
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
            .filter(|message| !crate::session_limit::looks_like_session_limit(message));
        let next_prompt = with_first_prompt_context(
            "The provider session limit window has reset. Continue from where we left off.".to_string(),
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
    let context_exceeds_limit = effective_token_count > config.max_context_tokens;
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
        approximate_token_count: auto_prompt_ctx.approximate_token_count,
        connection: None,
        project: None,
        peer_agent_states: peer_states::unmuted_states_for_context(),
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

    let session_id_str = data.session_id.to_string();

    // ── Pending-question fast path ───────────────────────────────────────
    //
    // If the worker stopped to ASK THE USER a question ("Which do you want?
    // Option A or Option B?", "Want me to do that?"), answer it directly via
    // a targeted LLM call on the last 2-3 paragraphs and dispatch that answer.
    // This runs before the overflow / lightweight / verification paths because
    // answering a short question is cheap and avoids the expensive summary
    // dance that would otherwise drain tokens and throw away the question.
    //
    // On any failure (no question, LLM unreachable, low confidence) the helper
    // returns Ok(None) and we fall through to the normal decision flow — the
    // user explicitly required that uncertain cases still reach stop/summary.
    if let Some(outcome) = pending_question::try_answer_pending_question(&data, cx).await? {
        log::warn!(
            "[auto_prompt::decide_with_llm] Pending-question fast path fired — dispatching answer (session={session_id_str})"
        );
        return Ok(outcome);
    }

    let result = if data.context_exceeds_limit {
        // ── Issue 007 guard: API exhaustion + context overflow ────────────
        //
        // If the source thread's completion request itself failed
        // (`had_api_error=true`: rate limit, network, 5xx, etc.) AND its
        // context has overflowed, Phase 2 would create a new continuation
        // thread whose first turn immediately hits the same exhausted API
        // and fails — a death spiral of doomed threads that burns quota
        // without making progress.
        //
        // Instead, defer the Phase 1/2 decision: return `RetryAfterBackoff`
        // so the caller sleeps and re-runs `decide_with_llm`. If retries are
        // exhausted, the caller surfaces a `Stopped` to the user.
        //
        // Deliberately checks `had_api_error`, NOT the broader `had_error`.
        // `had_error` is also set by a single failed tool call anywhere in
        // the turn (see `AcpThread::had_error` doc) — extremely common in
        // normal agentic work and unrelated to API availability. Gating this
        // guard on the broad signal previously misfired constantly: a long
        // session with context overflow plus any one incidental tool-call
        // failure (a grep with no matches, a bad `old_string` in an edit,
        // etc.) would defer, exhaust `max_llm_retries`, and permanently stop
        // with a misleading "likely rate limit" reason — even though the API
        // was healthy and Phase 2 would have worked fine immediately.
        if data.had_api_error {
            let config = load_config_cached().unwrap_or_default();
            // failure count is incremented by the caller's retry loop on
            // receipt of this outcome; use the current value to size the
            // initial delay so we don't pile up retries faster than the
            // upstream API can recover.
            let current_failures = AUTO_PROMPT_LLM_FAILURE_COUNT.load(Ordering::Relaxed);
            let delay_ms = config.backoff_delay_ms(current_failures.saturating_add(1));
            log::warn!(
                "[auto_prompt::decide_with_llm] Context overflow + had_error — deferring Phase 1/2 by {delay_ms}ms (current_failures={current_failures}, session={session_id_str})"
            );
            return Ok(AutoPromptOutcome::RetryAfterBackoff {
                delay_ms,
                reason: "context overflow with source thread error (likely rate limit)".to_string(),
            });
        }

        let summary_state = summary_state_for(&session_id_str);

        // If the last assistant message is already a voluntary summary (e.g. the
        // agent followed an "Always end with TL;DR" instruction and self-summarized
        // before context overflowed), skip Phase 1's redundant "Stop and summarize"
        // request and go straight to Phase 2 — reuse the existing summary as the
        // thread handoff. Saves a full assistant response of tokens.
        //
        // Only applies at summary_state==0 (Phase 1 has not fired yet). Once
        // Phase 1 has fired (state==1) the response is already a Phase 1 summary
        // and the normal Phase 2 path handles it.
        let skip_phase_1 = summary_state == 0
            && data
                .last_assistant_message
                .as_deref()
                .map_or(false, looks_like_voluntary_summary);
        if skip_phase_1 {
            log::warn!(
                "[auto_prompt::decide_with_llm] Last message is already a voluntary summary — skipping Phase 1, going straight to Phase 2 (session={session_id_str})"
            );
            // Keep summary_state==0 in the registry: we never asked for a summary,
            // so there's nothing to clear. The Phase 2 branch below handles the
            // handoff directly when `skip_phase_1` is set.
        }

        log::info!(
            "[auto_prompt::decide_with_llm] Context exceeds token limit — session={session_id_str} summary_state={summary_state} skip_phase_1={skip_phase_1}"
        );

        if summary_state == 0 && !skip_phase_1 {
            // Phase 1: Request summarization. Return ContextOverflow so the
            // UI sends a "summarize" message to the current thread.
            //
            // Always goes to the SAME thread — no need for ## 1/## 2/## 3 headers
            // since the AI already has full conversation context. Raw instruction only.
            let next_prompt = "Stop what you are doing and provide a concise summary of your progress. Include: (1) what was the original task, (2) what was accomplished, (3) what remains to be done, (4) the current state of any active plans (reference by filename). Be thorough — this summary will be used to continue in a fresh context.".to_string();
            set_summary_state(&session_id_str, 1);
            log::info!(
                "[auto_prompt::decide_with_llm] Returning ContextOverflow — requesting summary from AI (session={session_id_str})"
            );
            return Ok(AutoPromptOutcome::ContextOverflow(AutoPromptAction {
                from_session_id: data.session_id,
                from_title: data.title,
                next_prompt,
                work_dirs: data.work_dirs,
                original_user_message: data.original_user_message,
                profile_id: data.profile_id.clone(),
                actual_input_tokens: data.actual_input_tokens,
                approximate_token_count: data.approximate_token_count,
                last_assistant_message: data.last_assistant_message.clone(),
                force_new_thread: false,
                focus_new_thread: false,
            }));
        } else if summary_state == 1 || skip_phase_1 {
            // Phase 2: AI has responded with summary (or already had a voluntary
            // summary, via `skip_phase_1`). The last_assistant_message IS the
            // summary. Create a new thread with ThreadSummary flow.
            clear_summary_for_session(&session_id_str);

            // Phase 2 (P2.2 native summary hook): broadcast the summary to the
            // agent board so peer agents can see what this agent just concluded.
            // Mirrors the claude_agent path's `maybe_broadcast_summary_to_board`.
            // We already know this is a summary (summary_state==1 or voluntary),
            // so no contains_summary check needed.
            if let Some(summary) = data.last_assistant_message.as_deref() {
                peer_states::broadcast_state(&session_id_str, None, summary, "summary");
            }
            log::info!(
                "[auto_prompt::decide_with_llm] Summary received — creating new thread with ThreadSummary flow (session={session_id_str})"
            );

            let prompt_summary = build_prompt_summary(
                None,
                data.title.as_deref(),
                Some("context overflow: continuing in new thread with summary"),
                data.last_assistant_message.as_deref(),
                data.original_user_message.as_deref(),
                data.first_user_message.as_deref(),
            );

            // Build the continuation prompt in priority order:
            //   1. Summary's own Recommended Next Steps (the AI just wrote them —
            //      they're the most authoritative source of what to do next).
            //   2. Unchecked tasks in current-repo plan files (the session's
            //      actual target project, not a noisy neighbour repo).
            //   3. Unchecked tasks in other-repo plan files (last-resort).
            //   4. Generic "continue" fallback.
            //
            // `detect_remaining_work` is intentionally NOT consulted here: it
            // skips auto_prompt summary responses (see its guard) to avoid
            // re-summarization loops in the safety-net path. Phase 2 wants the
            // summary's guidance, so we use `extract_summary_next_steps` instead.
            let continuation = if llm_acknowledged_all_tasks_blocked(
                data.last_assistant_message.as_deref(),
            ) {
                log::info!(
                    "auto_prompt: ContextOverflow — LLM acknowledged blocked tasks, using generic continuation"
                );
                "Continue from where we left off.".to_string()
            } else if let Some(steps) = data
                .last_assistant_message
                .as_deref()
                .and_then(extract_summary_next_steps)
            {
                log::warn!(
                    "auto_prompt: ContextOverflow Phase 2 — using summary's Recommended Next Steps as continuation"
                );
                steps
            } else if let Some(plan_prompt) = detect_remaining_plan_tasks(
                &data.context_json,
                PlanRepoFilter::CurrentRepo,
                data.work_dirs.as_deref(),
            ) {
                log::warn!(
                    "auto_prompt: ContextOverflow Phase 2 — no summary next steps, falling back to current-repo plan tasks"
                );
                plan_prompt
            } else if let Some(plan_prompt) = detect_remaining_plan_tasks(
                &data.context_json,
                PlanRepoFilter::OtherRepos,
                data.work_dirs.as_deref(),
            ) {
                log::warn!(
                    "auto_prompt: ContextOverflow Phase 2 — no current-repo tasks, falling back to other-repo plan tasks"
                );
                plan_prompt
            } else {
                log::info!(
                    "auto_prompt: ContextOverflow Phase 2 — no detectors matched, generic continuation"
                );
                "Continue from where we left off.".to_string()
            };

            // Preserve slash commands (e.g. /optimize) through context overflow
            // so the new thread re-activates the skill and continues the loop.
            let continuation = match data.original_user_message.as_deref() {
                Some(msg) if msg.trim().starts_with('/') => {
                    let cmd = msg.trim();
                    log::info!(
                        "auto_prompt: ContextOverflow — original message is slash command '{cmd}', preserving it"
                    );
                    format!(
                        "{cmd}\n\nContext overflowed mid-task. Pick up where the summary left off. \
                         Do NOT summarize again — continue working immediately."
                    )
                }
                _ => continuation,
            };

            let next_prompt = with_first_prompt_context(
                continuation,
                prompt_summary.as_deref(),
                data.title.as_deref(),
                data.last_assistant_message.as_deref(),
            );

            auto_claim_plan(
                &next_prompt,
                &data.context_json,
                &data.session_id,
                data.title.as_deref(),
            );
            // Reset token counts — the new thread starts from a summary, not the
            // bloated old context. Carrying the old token counts forward causes
            // dispatch_action to always choose new-thread AND can re-trigger
            // ContextOverflow on the fresh thread.
            // Force new-thread creation regardless of token counts — after Phase 2
            // the old thread's context is full and the summary must go to a fresh thread.
            let mut action = data.make_continue_action(next_prompt);
            action.actual_input_tokens = None;
            action.approximate_token_count = 0;
            action.force_new_thread = true;
            return Ok(AutoPromptOutcome::Continue(action));
        } else {
            // Unexpected state — reset and stop
            clear_summary_for_session(&session_id_str);
            let stop_reason = "context overflow: unexpected summary state".to_string();
            reset_iteration_with_session(&data.session_id.to_string());
            return Ok(AutoPromptOutcome::Stopped {
                reason: stop_reason,
            });
        }
    } else {
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
        call_language_model(&data.model, &data.system_prompt, &lightweight_context, cx).await
    };

    log::info!(
        "[auto_prompt::decide_with_llm] LLM call completed with result: {:?}",
        result.is_ok()
    );

    match result {
        Ok((_raw_response, mut response)) => {
            let has_prompt = response
                .next_prompt
                .as_ref()
                .is_some_and(|p| !p.trim().is_empty());

            let is_synthetic_failure = response.confidence.unwrap_or(1.0) <= 0.3
                && response.reason.as_ref().is_some_and(|r| {
                    let lower = r.to_ascii_lowercase();
                    lower.starts_with("model returned") || lower.starts_with("model stream")
                });

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
                    } else if let Some(lowest_plan) = detect_plan_skip(&prompt, &data.context_json)
                    {
                        log::info!(
                            "auto_prompt: overriding plan-skip with correction prompt for {lowest_plan}"
                        );
                        build_plan_correction_prompt(&lowest_plan)
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

                    auto_claim_plan(
                        &next_prompt,
                        &data.context_json,
                        &data.session_id,
                        data.title.as_deref(),
                    );

                    Ok(data.make_continue(next_prompt))
                }
                EvaluationResult::NeedsSecondOpinion {
                    extracted_section,
                    rule_reason,
                } => {
                    log::info!(
                        "[auto_prompt::decide_with_llm] Evaluation: NeedsSecondOpinion — {rule_reason}"
                    );

                    let second_opinion_system = "# version: second_opinion\n\
                        You are a second-opinion judge. The main orchestration LLM returned low confidence \n\
                        but pattern detection found potential remaining work in the worker AI's last message.\n\n\
                        Respond ONLY with valid JSON:\n\
                        {\"confidence\": float, \"next_prompt\": string | null, \"reason\": string, \"thread_summary\": null}\n\n\
                        ## Rules:\n\
                        1. Read the extracted section carefully — is there SPECIFIC, ACTIONABLE remaining work?\n\
                        2. Generic statements like 'remaining work:' followed by nothing actionable → confidence <= 0.2\n\
                        3. Summary/completion messages that happen to contain trigger words → confidence <= 0.2\n\
                        4. Actual unchecked tasks, bugs to fix, features to implement → confidence >= 0.8\n\
                        5. When in doubt, favor stopping (low confidence) — the main LLM already said stop";

                    let second_opinion_context = format!(
                        "## Main LLM decision\n- confidence: low (below threshold)\n- reason: {}\n\n\
                         ## Pattern detection\n- rule: {}\n\n\
                         ## Extracted section\n{}\n\n\
                         ## Last assistant message (for context)\n{}",
                        input.reason.as_deref().unwrap_or("(none)"),
                        rule_reason,
                        extracted_section,
                        data.last_assistant_message.as_deref().unwrap_or("(none)"),
                    );

                    match call_language_model(
                        &data.model,
                        second_opinion_system,
                        &second_opinion_context,
                        cx,
                    )
                    .await
                    {
                        Ok((_raw, response)) => {
                            let second_opinion_confidence = response.confidence.unwrap_or(0.5);
                            if second_opinion_confidence >= WORKING_CONFIDENCE_THRESHOLD {
                                log::info!(
                                    "[auto_prompt::decide_with_llm] Second opinion: Continue (confidence={second_opinion_confidence:.2}) — {:?}",
                                    response.reason
                                );
                                let next_prompt = with_first_prompt_context(
                                    extracted_section,
                                    prompt_summary.as_deref(),
                                    data.title.as_deref(),
                                    data.last_assistant_message.as_deref(),
                                );
                                auto_claim_plan(
                                    &next_prompt,
                                    &data.context_json,
                                    &data.session_id,
                                    data.title.as_deref(),
                                );
                                Ok(data.make_continue(next_prompt))
                            } else {
                                let stop_reason = format!(
                                    "second opinion confirmed stop (confidence={second_opinion_confidence:.2}): {}",
                                    response.reason.as_deref().unwrap_or("no reason given")
                                );
                                log::info!("[auto_prompt::decide_with_llm] {stop_reason}");
                                reset_iteration_with_session(&data.session_id.to_string());
                                Ok(AutoPromptOutcome::Stopped {
                                    reason: stop_reason,
                                })
                            }
                        }
                        Err(err) => {
                            let stop_reason = format!(
                                "second opinion LLM call failed: {err:#} — defaulting to stop"
                            );
                            log::warn!("[auto_prompt::decide_with_llm] {stop_reason}");
                            reset_iteration_with_session(&data.session_id.to_string());
                            Ok(AutoPromptOutcome::Stopped {
                                reason: stop_reason,
                            })
                        }
                    }
                }
                EvaluationResult::WantsStop { reason } => {
                    if input.is_synthetic_failure {
                        // Full-context LLM call failed (context too large or model error).
                        // Retry with lightweight context: last message + incomplete plan names only.
                        log::info!(
                            "[auto_prompt::decide_with_llm] Building lightweight retry context — last_assistant_message={} chars, has_title={}, reason for WantsStop: {}",
                            data.last_assistant_message
                                .as_ref()
                                .map(|m| m.len())
                                .unwrap_or(0),
                            data.title.is_some(),
                            reason
                        );
                        let lightweight_ctx = build_lightweight_retry_context(
                            &data.context_json,
                            data.last_assistant_message.as_deref(),
                            data.title.as_deref(),
                        );
                        log::info!(
                            "[auto_prompt::decide_with_llm] Lightweight retry context built ({} chars):\n---\n{}\n---",
                            lightweight_ctx.len(),
                            lightweight_ctx.chars().take(800).collect::<String>()
                        );

                        let retry_system = "# version: retry\n\
                            You decide what to do next based on the AI's last message.\n\
                            Priority: the LAST ASSISTANT MESSAGE is the most important signal.\n\n\
                            Respond ONLY with valid JSON:\n\
                            {\"confidence\": float, \"next_prompt\": string | null, \"reason\": string | null, \"thread_summary\": null}\n\n\
                            ## Rules (in order):\n\
                            1. LAST MESSAGE IS KING — reason about it first, before looking at plans\n\
                            2. If it asks \"would you like to continue?\" or \"want me to ...?\" → confidence >= 0.8, \n\
                               next_prompt=\"continue as you prefer\"\n\
                            3. If it presents options to pick from AND is willing to pick → confidence >= 0.8, \n\
                               next_prompt=\"select best for performance, security, SOLID, DRY principles\"\n\
                            3a. If it presents options BUT explicitly defers to the user (e.g. \"I won't pick\", \n\
                               \"you decide\", \"need your input\", \"let me know which\") → confidence <= 0.2, \n\
                               next_prompt = null — another nudge produces the same question\n\
                            4. If it reports plan done but mentions remaining phases/next steps → confidence >= 0.7, \n\
                               next_prompt=\"continue with the next phase/step\"\n\
                            5. If it describes specific remaining work → confidence >= 0.7, \n\
                               next_prompt=continue that specific work\n\
                            6. If genuinely complete with nothing left → confidence <= 0.2\n\
                            7. Struck-through / skipped tasks (~~text~~, \"Skipped\", \"Cancelled\") count as DONE — \n\
                               do NOT continue them. If only skipped tasks remain → confidence <= 0.2\n\
                            8. If remaining tasks seem unjustified or low-value, include #SKIP in next_prompt to signal skip\n\
                            9. confidence must be >= 0.7 to continue\n";

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
                                        "auto_prompt: lightweight retry attempt {attempt} ok: confidence={:?}, prompt={:?}",
                                        parsed.confidence,
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
                            Some(parsed)
                                if parsed.confidence.unwrap_or(0.0)
                                    >= WORKING_CONFIDENCE_THRESHOLD =>
                            {
                                let prompt = parsed
                                    .next_prompt
                                    .unwrap_or_else(|| "Continue with remaining work.".to_string());
                                let next_prompt = with_first_prompt_context(
                                    prompt,
                                    prompt_summary.as_deref(),
                                    data.title.as_deref(),
                                    data.last_assistant_message.as_deref(),
                                );
                                auto_claim_plan(
                                    &next_prompt,
                                    &data.context_json,
                                    &data.session_id,
                                    data.title.as_deref(),
                                );
                                Ok(data.make_continue(next_prompt))
                            }
                            Some(parsed) => {
                                let stop_reason = parsed
                                    .reason
                                    .unwrap_or_else(|| "lightweight retry says stop".to_string());
                                log::info!(
                                    "auto_prompt: lightweight retry says stop: {stop_reason}"
                                );
                                log::info!(
                                    "auto_prompt: checking detect_remaining_work safety net before accepting retry stop"
                                );
                                if let Some(remaining_prompt) =
                                    detect_remaining_work(data.last_assistant_message.as_deref())
                                {
                                    log::warn!(
                                        "auto_prompt: SAFETY NET OVERRIDE — detect_remaining_work found actionable work despite retry saying stop. Extracted prompt:\n---\n{}\n---",
                                        remaining_prompt.chars().take(500).collect::<String>()
                                    );
                                    let next_prompt = with_first_prompt_context(
                                        remaining_prompt,
                                        prompt_summary.as_deref(),
                                        data.title.as_deref(),
                                        data.last_assistant_message.as_deref(),
                                    );
                                    auto_claim_plan(
                                        &next_prompt,
                                        &data.context_json,
                                        &data.session_id,
                                        data.title.as_deref(),
                                    );
                                    Ok(data.make_continue(next_prompt))
                                } else if let Some(plan_prompt) =
                                    detect_remaining_plan_tasks(
                                        &data.context_json,
                                        PlanRepoFilter::All,
                                        data.work_dirs.as_deref(),
                                    )
                                {
                                    log::warn!(
                                        "auto_prompt: PLAN TASK FALLBACK — detect_remaining_work found nothing but plan files have unchecked tasks"
                                    );
                                    let next_prompt = with_first_prompt_context(
                                        plan_prompt,
                                        prompt_summary.as_deref(),
                                        data.title.as_deref(),
                                        data.last_assistant_message.as_deref(),
                                    );
                                    auto_claim_plan(
                                        &next_prompt,
                                        &data.context_json,
                                        &data.session_id,
                                        data.title.as_deref(),
                                    );
                                    Ok(data.make_continue(next_prompt))
                                } else {
                                    log::info!(
                                        "auto_prompt: all safety nets exhausted — no remaining work patterns and no unchecked plan tasks, accepting retry stop"
                                    );
                                    reset_iteration_with_session(&data.session_id.to_string());
                                    Ok(AutoPromptOutcome::Stopped {
                                        reason: stop_reason,
                                    })
                                }
                            }
                            None => {
                                log::warn!(
                                    "auto_prompt: all 3 lightweight retries failed, checking detect_remaining_work safety net"
                                );
                                if let Some(remaining_prompt) =
                                    detect_remaining_work(data.last_assistant_message.as_deref())
                                {
                                    log::warn!(
                                        "auto_prompt: SAFETY NET OVERRIDE — detect_remaining_work found actionable work after all retries failed. Extracted prompt:\n---\n{}\n---",
                                        remaining_prompt.chars().take(500).collect::<String>()
                                    );
                                    let next_prompt = with_first_prompt_context(
                                        remaining_prompt,
                                        prompt_summary.as_deref(),
                                        data.title.as_deref(),
                                        data.last_assistant_message.as_deref(),
                                    );
                                    auto_claim_plan(
                                        &next_prompt,
                                        &data.context_json,
                                        &data.session_id,
                                        data.title.as_deref(),
                                    );
                                    Ok(data.make_continue(next_prompt))
                                } else if let Some(plan_prompt) =
                                    detect_remaining_plan_tasks(
                                        &data.context_json,
                                        PlanRepoFilter::All,
                                        data.work_dirs.as_deref(),
                                    )
                                {
                                    log::warn!(
                                        "auto_prompt: PLAN TASK FALLBACK — all retries failed but plan files have unchecked tasks"
                                    );
                                    let next_prompt = with_first_prompt_context(
                                        plan_prompt,
                                        prompt_summary.as_deref(),
                                        data.title.as_deref(),
                                        data.last_assistant_message.as_deref(),
                                    );
                                    auto_claim_plan(
                                        &next_prompt,
                                        &data.context_json,
                                        &data.session_id,
                                        data.title.as_deref(),
                                    );
                                    Ok(data.make_continue(next_prompt))
                                } else {
                                    log::warn!(
                                        "auto_prompt: all safety nets exhausted — no remaining work patterns and no unchecked plan tasks, giving up"
                                    );
                                    reset_iteration_with_session(&data.session_id.to_string());
                                    Ok(AutoPromptOutcome::Stopped {
                                        reason: format!("lightweight retry failed: {reason}"),
                                    })
                                }
                            }
                        }
                    } else if is_decisive_stop(&input) {
                        log::info!(
                            "auto_prompt: decisive stop (confidence={:?}), skipping verification",
                            input.confidence
                        );
                        reset_iteration_with_session(&data.session_id.to_string());
                        return Ok(AutoPromptOutcome::Stopped { reason });
                    } else if is_waiting_for_user_decision(data.last_assistant_message.as_deref()) {
                        // Worker explicitly deferred a decision to the user (e.g.
                        // "I won't pick for you", "you decide", "need your input").
                        // Any further nudge will produce the same question, so stop
                        // cleanly and surface the worker's message to the user.
                        // Skipping pre-stop verification here is the whole point —
                        // otherwise we inject PRE-STOP VERIFICATION noise that
                        // ends in a `stopping` declaration anyway (see plan/bug
                        // on Plan 456 close-out where this fired needlessly).
                        log::info!(
                            "auto_prompt: worker is waiting for user decision — stopping without verification"
                        );
                        reset_iteration_with_session(&data.session_id.to_string());
                        return Ok(AutoPromptOutcome::Stopped { reason });
                    } else {
                        // Before accepting stop, check plan files for unchecked tasks.
                        // If the LLM explicitly declared ALL tasks blocked (not just some),
                        // respect that assessment.
                        if let Some(plan_prompt) = detect_remaining_plan_tasks(
                            &data.context_json,
                            PlanRepoFilter::All,
                            data.work_dirs.as_deref(),
                        ) {
                            if llm_acknowledged_all_tasks_blocked(
                                data.last_assistant_message.as_deref(),
                            ) {
                                log::info!(
                                    "auto_prompt: LLM declared all remaining tasks blocked, respecting stop decision"
                                );
                            } else {
                                log::warn!(
                                    "auto_prompt: PLAN TASK FALLBACK — confidence below threshold but plan files have unchecked tasks, continuing"
                                );
                                let next_prompt = with_first_prompt_context(
                                    plan_prompt,
                                    prompt_summary.as_deref(),
                                    data.title.as_deref(),
                                    data.last_assistant_message.as_deref(),
                                );
                                auto_claim_plan(
                                    &next_prompt,
                                    &data.context_json,
                                    &data.session_id,
                                    data.title.as_deref(),
                                );
                                return Ok(data.make_continue(next_prompt));
                            }
                        }

                        let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
                        let max_verifications = data.max_verification_attempts;

                        if verification_count >= max_verifications {
                            let stop_reason =
                                format!("max verification attempts ({max_verifications}) exceeded");
                            log::warn!("auto_prompt: {stop_reason}");
                            reset_iteration_with_session(&data.session_id.to_string());
                            Ok(AutoPromptOutcome::Stopped {
                                reason: stop_reason,
                            })
                        } else {
                            let attempt = verification_count + 1;
                            log::info!(
                                "auto_prompt: WantsStop ('{reason}') — initiating pre-stop verification (attempt {attempt}/{max_verifications})"
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
                                    Ok(data.make_continue(next_prompt))
                                }
                                None => {
                                    let stop_reason =
                                        "confidence below threshold, no plan files found for verification"
                                            .to_string();
                                    log::info!(
                                        "auto_prompt: no verification needed (no plan files found), stopping"
                                    );
                                    reset_iteration_with_session(&data.session_id.to_string());
                                    Ok(AutoPromptOutcome::Stopped {
                                        reason: stop_reason,
                                    })
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(err) => {
            log::warn!("auto_prompt: language model call failed: {err}");
            Err(err)
        }
    }
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
    // Summary state is now per-session in SUMMARY_REGISTRY and cleared
    // via clear_summary_for_session / reset_iteration_with_session.
}

/// Reset the auto-prompt chain counters **and** release any plan claims held by
/// the given session. Call this at every chain stop point so competing agents
/// can pick up the released plans.
pub fn reset_iteration_with_session(session_id: &str) {
    reset_iteration();
    clear_summary_for_session(session_id);
    plan_registry::release_all_for_session(session_id);
}

pub fn increment_llm_failure_count() -> u32 {
    AUTO_PROMPT_LLM_FAILURE_COUNT.fetch_add(1, Ordering::Relaxed) + 1
}

pub fn reset_llm_failure_count() {
    AUTO_PROMPT_LLM_FAILURE_COUNT.store(0, Ordering::Relaxed);
}

pub(crate) fn get_iteration() -> u32 {
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
        // Clear all stale summary states on chain timeout
        with_summary_registry(|map| map.clear());
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

    let session_id = thread.session_id().to_string();

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

            let path_str = path.to_string_lossy().to_string();
            if plan_registry::is_claimed_by_other(&path_str, &session_id) {
                log::info!(
                    "[auto_prompt::read_plan_files] Skipping plan claimed by another agent: {}",
                    path_str
                );
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            plan_files.push(PlanFileContent {
                path: path_str,
                content,
            });
        }
    }

    // Identify active project from first user message.
    // This prioritizes the session's target project over other workspace projects.
    // Supports: file:///... URLs, zed:///... URLs, and plain absolute paths containing /.plans/ or /.plan/
    let active_project = first_user_message.and_then(|msg| extract_active_project(msg));

    if let Some(ref active) = active_project {
        log::info!("[auto_prompt::read_plan_files] Active project: {active}");

        // Sort: active project's plans first, then newest (highest index) first within each group
        plan_files.sort_by(|a, b| {
            let a_active = a.path.starts_with(active.as_str());
            let b_active = b.path.starts_with(active.as_str());
            match (a_active, b_active) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => b.path.cmp(&a.path),
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
    } else {
        plan_files.sort_by(|a, b| b.path.cmp(&a.path));
    }

    let max_plan_files = 10;
    if plan_files.len() > max_plan_files {
        let before = plan_files.len();
        plan_files.truncate(max_plan_files);
        log::info!(
            "[auto_prompt::read_plan_files] Truncated to {max_plan_files} most recent plan files: {before} → {}",
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

fn read_doc_files(thread: &acp_thread::AcpThread) -> Vec<String> {
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

            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if filename.is_empty() {
                continue;
            }

            doc_files.push(filename);
        }
    }

    if !doc_files.is_empty() {
        log::info!(
            "[auto_prompt::read_doc_files] Found {} doc file(s): {:?}",
            doc_files.len(),
            doc_files
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
                    "next_prompt": null,
                    "reason": format!("Model returned {} Thinking events but no Text output", thinking_parts.len()),
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
                    "next_prompt": null,
                    "reason": format!("model returned no usable content ({} empty Text, {} empty Thinking, {} stream errors)", text_parts.len(), thinking_parts.len(), stream_errors.len()),
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
                "next_prompt": null,
                "reason": format!("model stream produced only errors ({})", stream_errors.len()),
                "confidence": 0.0,
                "thread_summary": null
            });
            anyhow::Ok(synthetic.to_string())
        } else {
            log::warn!(
                "auto_prompt: model returned zero events (0 Text, 0 Thinking) out of {total_events} total events. Other types seen: {other_event_types:?}"
            );
            let synthetic = serde_json::json!({
                "next_prompt": null,
                "reason": format!("model returned zero events ({} total stream events)", total_events),
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
            let err_string = format!("{parse_err}");
            if err_string.contains("duplicate field") {
                if let Ok(response) = deduplicate_and_parse(json_str) {
                    log::info!("auto_prompt: recovered from duplicate key error: {parse_err}");
                    return Ok(response);
                }
            }
            let preview = text.chars().take(200).collect::<String>();
            log::warn!("auto_prompt: failed to parse response as JSON ({parse_err}): {preview:?}");
            log::warn!("auto_prompt: synthesizing stop response to avoid retry loop");
            Ok(AutoPromptResponse {
                next_prompt: None,
                reason: Some(format!(
                    "unparseable response ({} bytes, {} extracted): {parse_err}",
                    text.len(),
                    json_str.len()
                )),
                confidence: Some(0.0),
                thread_summary: None,
            })
        }
    }
}

/// Attempt to parse JSON with duplicate object keys by keeping the first occurrence
/// of each key. This handles LLMs that emit e.g.
/// `"thread_summary": "...", "thread_summary": null`.
fn deduplicate_and_parse(json_str: &str) -> anyhow::Result<AutoPromptResponse> {
    let deduped = rebuild_deduplicated_json(json_str)?;
    log::debug!("auto_prompt: deduplicated JSON: {deduped}");
    serde_json::from_str(&deduped).context("re-parsed deduplicated JSON")
}

/// Rebuild a JSON object string, keeping only the first occurrence of each key.
fn rebuild_deduplicated_json(json_str: &str) -> anyhow::Result<String> {
    let trimmed = json_str.trim();
    anyhow::ensure!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "expected JSON object"
    );

    // Walk the string character by character to split top-level key-value pairs,
    // tracking brace/bracket depth to avoid splitting inside nested structures.
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result_entries: Vec<String> = Vec::new();

    let bytes = inner.as_bytes();
    let mut pos = 0;

    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len() && bytes[pos] == b' '
            || bytes[pos] == b'\n'
            || bytes[pos] == b'\r'
            || bytes[pos] == b'\t'
        {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }

        // Expect a key (quoted string)
        if bytes[pos] != b'"' {
            break;
        }
        let key_start = pos;
        pos += 1;
        let mut escaped = false;
        while pos < bytes.len() {
            if escaped {
                escaped = false;
            } else if bytes[pos] == b'\\' {
                escaped = true;
            } else if bytes[pos] == b'"' {
                break;
            }
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        pos += 1; // skip closing quote
        let key_end = pos;

        // Parse the actual key value
        let key_str = &trimmed[1 + key_start..1 + key_end]; // +1 for the opening brace
        let parsed_key: String = serde_json::from_str(key_str).unwrap_or_default();

        // Skip whitespace and colon
        while pos < bytes.len()
            && (bytes[pos] == b' '
                || bytes[pos] == b'\n'
                || bytes[pos] == b'\r'
                || bytes[pos] == b'\t'
                || bytes[pos] == b':')
        {
            pos += 1;
        }

        // Now parse the value — could be string, number, bool, null, object, or array
        let value_start = pos;
        // We need to find where the value ends (at top level, before a comma)
        let mut depth = 0i32;
        let mut in_string = false;
        let mut was_escaped = false;
        while pos < bytes.len() {
            let ch = bytes[pos];
            if was_escaped {
                was_escaped = false;
            } else if in_string {
                if ch == b'\\' {
                    was_escaped = true;
                } else if ch == b'"' {
                    in_string = false;
                }
            } else {
                match ch {
                    b'"' => in_string = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => depth -= 1,
                    b',' if depth == 0 => break,
                    _ => {}
                }
            }
            pos += 1;
        }
        let value_end = pos;

        // Skip optional comma
        if pos < bytes.len() && bytes[pos] == b',' {
            pos += 1;
        }

        // The raw entry from key_start to value_end (relative to `inner`)
        let full_entry = format!("{key_str}: {}", &inner[value_start..value_end]);

        if seen_keys.insert(parsed_key.clone()) {
            result_entries.push(full_entry);
        } else {
            log::debug!("auto_prompt: deduplicating key '{parsed_key}' in LLM response");
        }
    }

    Ok(format!("{{{}}}", result_entries.join(",")))
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

/// Extract the "what to do next" section from a ContextOverflow Phase 1 summary.
///
/// Unlike `detect_remaining_work`, which deliberately skips auto_prompt
/// summary responses to avoid re-summarization loops in the safety-net path,
/// this function is called from the Phase 2 continuation builder where the
/// summary IS the authoritative source of what should happen next.
///
/// Looks for the first section whose heading contains a next-steps indicator:
///   "recommended next steps", "next steps", "what remains", "remaining",
///   "what's left", "todo", "action items", "follow-up", "pending".
/// Returns the heading + body (everything until the next same-or-higher level
/// heading or end of message), so the continuation prompt keeps full context.
/// Falls back to the last 3 paragraphs (matching `extract_remaining_section` semantics)
/// when no explicit section is found, then to None when nothing actionable remains.
fn extract_summary_next_steps(summary: &str) -> Option<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let trigger_phrases = [
        "recommended next steps",
        "next steps",
        "what remains",
        "what is left",
        "what's left",
        "remaining",
        "to do",
        "todo",
        "action items",
        "follow-up",
        "follow up",
        "pending",
        "when resuming",
        "need to",
    ];

    // Collect every heading that mentions a trigger phrase, paired with its
    // section body. We scan ALL of them (not just the first) because a summary
    // may have a "## What Remains" section that just says "some tasks left"
    // (no actionable markers) followed by a "## Recommended Next Steps" section
    // that actually lists what to do.
    let all_lines: Vec<&str> = trimmed.lines().collect();
    let mut candidate_sections: Vec<String> = Vec::new();
    for (idx, line) in all_lines.iter().enumerate() {
        let lower = line.to_lowercase();
        if !lower.starts_with('#') {
            continue;
        }
        if !trigger_phrases.iter().any(|t| lower.contains(t)) {
            continue;
        }
        let heading_level = line.chars().take_while(|c| *c == '#').count();
        let mut end_idx = all_lines.len();
        for (j, l) in all_lines.iter().enumerate().skip(idx + 1) {
            let lvl = l.chars().take_while(|c| *c == '#').count();
            if lvl > 0 && lvl <= heading_level {
                end_idx = j;
                break;
            }
        }
        candidate_sections.push(all_lines[idx..end_idx].join("\n").trim().to_string());
    }

    // Prefer the first trigger heading whose body has actionable markers.
    // If none qualify, fall back to prose scan of the last 3 paragraphs.
    let section: Option<String> = candidate_sections.into_iter().find(|sec| {
        let lower = sec.to_lowercase();
        sec.contains("- ")
            || sec.contains("* ")
            || sec.contains("1.")
            || sec.contains("2.")
            || lower.contains("must")
            || lower.contains("need to")
            || lower.contains("should")
            || lower.contains("want me to")
            || lower.contains("decide")
    });

    let section = match section {
        Some(s) => s,
        None => {
            // No actionable trigger heading. Fall back to scanning the last
            // 3 paragraphs for trigger words (same heuristic as extract_remaining_section)
            // so a summary that embeds next steps in prose is still picked up.
            let paragraphs: Vec<&str> = trimmed
                .split("\n\n")
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .collect();
            let scan = 3.min(paragraphs.len());
            let scan_start = paragraphs.len() - scan;
            let mut hit: Option<usize> = None;
            for i in (scan_start..paragraphs.len()).rev() {
                let lower = paragraphs[i].to_lowercase();
                if trigger_phrases.iter().any(|t| lower.contains(t)) {
                    hit = Some(i);
                    break;
                }
            }
            match hit {
                Some(i) => paragraphs[i..].join("\n\n").trim().to_string(),
                None => return None,
            }
        }
    };

    if section.trim().is_empty() {
        return None;
    }

    Some(format!(
        "Continuing from the previous session's summary. Recommended next steps:\n\n\
         {section}\n\n\
         Pick up from here. Do NOT summarize again — start working immediately."
    ))
}

fn detect_remaining_work(last_assistant_message: Option<&str>) -> Option<String> {
    let msg = last_assistant_message?.trim();
    if msg.is_empty() {
        return None;
    }

    // Skip auto_prompt's own ContextOverflow Phase 1 summary responses.
    // The Phase 1 prompt asks the worker to summarize with sections:
    //   ### 1. Original Task / ### 2. What Was Accomplished / ### 3. What Remains / ### 4. Active Plan State
    // The summary naturally contains "What Remains" with unchecked items — those
    // were already evaluated by the worker as deferred/blocked, not actionable.
    // Firing the safety net on these creates a re-summarization loop.
    if is_auto_prompt_summary_response(msg) {
        log::info!(
            "[auto_prompt::detect_remaining_work] Skipping — message is an auto_prompt summary response"
        );
        return None;
    }

    let section = extract_remaining_section(msg);

    // Tightened pattern set (issue 006): only phrases that are authoritative
    // task-list markers. Generic phrases like "remaining work", "remaining:",
    // "still need", "next step(s)" were dropped because they matched inside
    // ordinary prose — e.g. "No remaining work" — and forced an extra LLM
    // second-opinion call every time a worker summarized a clean stop.
    //
    // We also require the phrase to appear in a *list/heading context*: the
    // line must start with a markdown list/heading marker. This prevents
    // free-form mentions like "consider the action items above" from firing.
    const PATTERNS: &[&str] = &[
        "todo:",
        "action items",
        "left to do",
    ];

    // Negation cues: if any appear within ~40 chars before the matched phrase
    // on the same line, treat the mention as referring to work that does NOT
    // exist and skip the override. Covers "no remaining work", "nothing left
    // to do", "no action items", etc.
    const NEGATIONS: &[&str] = &[
        "no ",
        "none ",
        "nothing ",
        "no further ",
        "nothing left ",
        "already done",
        "all done",
        "complete",
        "finished",
        "shipped",
        "landed",
        "resolved",
    ];

    for (line_idx, line) in msg.lines().enumerate() {
        let trimmed = line.trim_start();
        if !is_list_or_heading_line(trimmed) {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let mut matched_pattern: Option<&str> = None;
        for pattern in PATTERNS {
            if let Some(pos) = lower.find(pattern) {
                // Negation guard: scan up to 40 chars before the match for a
                // negation cue on the same line.
                let window_start = pos.saturating_sub(40);
                let prefix = &lower[window_start..pos];
                if NEGATIONS.iter().any(|neg| prefix.contains(neg)) {
                    log::info!(
                        "[auto_prompt::detect_remaining_work] Pattern '{pattern}' found on line {line_idx} but negated by preceding text — skipping override"
                    );
                    continue;
                }
                matched_pattern = Some(pattern);
                break;
            }
        }
        let Some(pattern) = matched_pattern else { continue };
        log::warn!(
            "[auto_prompt::detect_remaining_work] Pattern found: {pattern} in a list/heading line of last_assistant_message — overriding stop"
        );
        let section_text = section.as_deref().unwrap_or(msg);
        // We already know the matched line is a list/heading line, so the
        // section is actionable by construction; surface it to the second
        // opinion and let the LLM decide whether to continue.
        return Some(format!(
            "Previous assistant mentioned remaining work. Extracted section:\n\n\
             {section_text}\n\n\
             If this describes specific actionable remaining work, continue with it. \
             If the work is already done or this is a false positive, stop."
        ));
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

/// Whether a line starts with a markdown list or heading marker. Used by
/// [`detect_remaining_work`] to require task-list context for the substring
/// patterns (issue 006): a phrase like "action items" buried mid-paragraph in
/// prose does not count, but `## Action items` or `- Action items:` does.
fn is_list_or_heading_line(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('-')
        || trimmed.starts_with('*')
        || trimmed.starts_with('+')
        || trimmed.starts_with('#')
    {
        return true;
    }
    // Ordered list: digit(s) followed by `.` or `)`.
    let bytes = trimmed.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0 && idx + 1 < bytes.len() {
        let sep = bytes[idx];
        let next = bytes[idx + 1];
        return (sep == b'.' || sep == b')') && (next == b' ' || next == b'\t');
    }
    false
}

/// Detect whether an assistant message is ALREADY a summary suitable for a
/// context-overflow thread handoff, so Phase 1's "Stop and summarize" request
/// can be skipped.
///
/// Broader than `is_auto_prompt_summary_response` (which only matches responses
/// to Phase 1's own prompt wording). This also catches VOLUNTARY summaries that
/// agents produce following instructions like "Always end with TL;DR" — the
/// common case when context overflows on an agent that already self-summarized.
///
/// A markdown heading (levels 1-3) containing "summary" or "tl;dr" is treated
/// as a deliberate handoff signal. The Phase 1 response pattern (≥3 of 4
/// markers: original task / accomplished / what remains / active plan) is also
/// accepted via `is_auto_prompt_summary_response`.
fn looks_like_voluntary_summary(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Phase 1 response pattern — catch auto_prompt's own echo.
    if is_auto_prompt_summary_response(trimmed) {
        return true;
    }

    // A markdown heading at level 1-3 containing "summary" / "tl;dr" / "tldr"
    // is a deliberate handoff marker from the agent. Line-anchored and requires
    // a space after the `#` run (strict ATX heading) so prose mentions like
    // "see the Summary section" don't match.
    for raw_line in trimmed.lines() {
        let line = raw_line.trim_start();
        let bytes = line.as_bytes();
        let hash_count = line.chars().take_while(|c| *c == '#').count();
        if !(1..=3).contains(&hash_count) {
            continue;
        }
        // Require a space after the '#' run (e.g. "## Summary").
        if bytes.len() <= hash_count || bytes[hash_count] != b' ' {
            continue;
        }
        let lower = line.to_lowercase();
        if lower.contains("summary") || lower.contains("tl;dr") || lower.contains("tldr") {
            return true;
        }
    }

    false
}

/// Detect auto_prompt's own ContextOverflow Phase 1 summary responses.
///
/// When context overflows, auto_prompt sends:
///   "Stop what you are doing and provide a concise summary of your progress.
///    Include: (1) what was the original task, (2) what was accomplished,
///    (3) what remains to be done, (4) the current state of any active plans."
///
/// The worker responds with a structured summary that naturally has "What Remains"
/// sections with unchecked items. Both `detect_remaining_work` and
/// `llm_acknowledged_all_tasks_blocked` must skip these to avoid false positives.
pub(crate) fn is_auto_prompt_summary_response(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Phase 1 prompt asks for exactly these 4 sections
    let has_original_task = lower.contains("original task");
    let has_accomplished =
        lower.contains("what was accomplished") || lower.contains("accomplished");
    let has_remains = lower.contains("what remains") || lower.contains("what remain");
    let has_active_plan = lower.contains("active plan") || lower.contains("plan state");
    let section_matches = [
        has_original_task,
        has_accomplished,
        has_remains,
        has_active_plan,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    // Need at least 3 of 4 to avoid false positives on random text
    section_matches >= 3
}

/// Check if the LLM explicitly declares that ALL remaining tasks are blocked,
/// not just that some tasks happen to mention blocking keywords.
///
/// This is a stricter version of the old `llm_acknowledged_blocked_tasks` — it requires
/// the LLM to state that the work as a whole cannot proceed (e.g. "nothing actionable",
/// "no further action"). Summary responses from auto_prompt's own Phase 1 are excluded
/// because they naturally mention "blocked" items as part of their structured output.
fn llm_acknowledged_all_tasks_blocked(last_assistant_message: Option<&str>) -> bool {
    let msg = match last_assistant_message {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return false,
    };

    // Never match on auto_prompt's own summary responses — they always mention
    // "blocked" items as part of the Phase 1 summary structure.
    if is_auto_prompt_summary_response(msg) {
        log::info!(
            "[auto_prompt::llm_acknowledged_all_tasks_blocked] Skipping — message is an auto_prompt summary response"
        );
        return false;
    }

    let lower = msg.to_lowercase();

    // The LLM must explicitly say everything is blocked / nothing can proceed.
    // Requires BOTH a qualifier (nothing/no further/can't) AND "blocked"/"deferred" together.
    // Bare "all remaining" + "blocked" is excluded because summary sections naturally
    // contain both words without meaning the worker is declaring a hard stop.
    let all_blocked = lower.contains("nothing actionable")
        || lower.contains("nothing left to do")
        || lower.contains("nothing left to implement")
        || lower.contains("no further action")
        || lower.contains("no further work")
        || lower.contains("cannot proceed further")
        || lower.contains("can't proceed further")
        || lower.contains("all remaining tasks are blocked")
        || lower.contains("all remaining work is blocked");

    all_blocked
}

fn extract_plan_paths_from_context(context_json: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    serde_json::from_str::<Ctx>(context_json)
        .ok()
        .map(|ctx| ctx.plan_files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default()
}

fn auto_claim_plan(
    next_prompt: &str,
    context_json: &str,
    session_id: &acp::SessionId,
    title: Option<&str>,
) {
    let plan_paths = extract_plan_paths_from_context(context_json);
    let session_id_str = session_id.to_string();
    if let Some(claimed) = plan_registry::auto_claim_from_prompt(
        next_prompt,
        &plan_paths,
        &session_id_str,
        title.unwrap_or("auto_prompt continuation"),
    ) {
        log::info!("[auto_prompt] Auto-claimed plan {claimed} for session {session_id_str}");
        // Phase 2 (P2.2 plan-start hook): announce to the agent board that this
        // agent is starting work on a plan, so peer agents know what we're about
        // to do. The meta field carries the plan filename for display.
        let plan_name = claimed.rsplit('/').next().unwrap_or(&claimed);
        peer_states::broadcast_state(
            &session_id_str,
            None,
            &format!("starting: {plan_name}"),
            &claimed,
        );
    }
}

/// Scope filter for `detect_remaining_plan_tasks`.
///
/// When context overflows and auto_prompt must build a continuation prompt
/// without an LLM round-trip, plan files are consulted in priority order:
/// current-repo unclaimed plans first, then other-repo unclaimed plans.
/// This prevents a noisy workspace from hijacking the continuation away from
/// the session's actual target project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanRepoFilter {
    /// All unclaimed plans regardless of which repo they live in.
    All,
    /// Only plans whose path is under one of `work_dirs` (the session's own repos).
    CurrentRepo,
    /// Only plans whose path is NOT under any of `work_dirs`.
    OtherRepos,
}

fn plan_belongs_to_current_repo(plan_path: &str, work_dirs: &[PathBuf]) -> bool {
    work_dirs
        .iter()
        .any(|dir| plan_path.starts_with(dir.to_string_lossy().as_ref()))
}

fn detect_remaining_plan_tasks(
    context_json: &str,
    filter: PlanRepoFilter,
    work_dirs: Option<&[PathBuf]>,
) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    let ctx = serde_json::from_str::<Ctx>(context_json).ok()?;
    let session_id = ctx.session_id.as_deref().unwrap_or("");

    // For CurrentRepo / OtherRepos filters, use the session's actual work_dirs
    // (passed in from the caller) to classify each plan. When work_dirs is None
    // or empty, fall back to All-filter behaviour — we can't classify without
    // knowing which repos the session is targeting.
    let dirs: Vec<PathBuf> = match (filter, work_dirs) {
        (PlanRepoFilter::All, _) | (_, None | Some(&[])) => Vec::new(),
        (PlanRepoFilter::CurrentRepo | PlanRepoFilter::OtherRepos, Some(dirs)) => {
            dirs.to_vec()
        }
    };
    let can_classify = !dirs.is_empty();

    let mut remaining = Vec::new();
    for plan in &ctx.plan_files {
        let count = count_actionable_tasks(&plan.content);
        if count == 0 {
            continue;
        }
        if plan_registry::is_claimed_by_other(&plan.path, session_id) {
            log::info!(
                "[auto_prompt::detect_remaining_plan_tasks] Skipping plan claimed by another agent: {}",
                plan.path
            );
            continue;
        }
        match filter {
            PlanRepoFilter::All => {}
            PlanRepoFilter::CurrentRepo => {
                if can_classify && !plan_belongs_to_current_repo(&plan.path, &dirs) {
                    continue;
                }
            }
            PlanRepoFilter::OtherRepos => {
                if can_classify && plan_belongs_to_current_repo(&plan.path, &dirs) {
                    continue;
                }
            }
        }
        let filename = plan.path.rsplit('/').next().unwrap_or("?");
        remaining.push(format!("- {filename}: {count} unchecked task(s)"));
    }
    if remaining.is_empty() {
        return None;
    }
    let scope_label = match filter {
        PlanRepoFilter::All => "",
        PlanRepoFilter::CurrentRepo => " (current repo)",
        PlanRepoFilter::OtherRepos => " (other repos)",
    };
    log::warn!(
        "[auto_prompt::detect_remaining_plan_tasks{scope_label}] Found {} unclaimed plan file(s) with unchecked tasks:\n{}",
        remaining.len(),
        remaining.join("\n")
    );
    let claims_note = plan_registry::format_claims_for_context(session_id)
        .map(|claims| format!("\n\n{claims}"))
        .unwrap_or_default();
    Some(format!(
        "Plan files have remaining unchecked tasks{scope_label}:\n\n{}\n\n\
         Continue with the next unchecked task. Mark completed steps as [x].{claims_note}",
        remaining.join("\n")
    ))
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
        "1. **Last message**: Re-read your last message. Any remaining work or unchecked items?"
            .to_string(),
        "2. **Diagnostics**: Are there any errors or warnings from `cargo check` / `cargo clippy`?"
            .to_string(),
        "3. **Git**: Any uncommitted changes?".to_string(),
    ];

    if is_perf {
        checks.push("4. **Benchmarks**: Did you run relevant benchmarks?".to_string());
    }

    let mut sections = vec![checks.join("\n")];

    if let Some(landscape) = landscape {
        sections.push(format!(
            "## Remaining Plans\n\n\
             {landscape}\n\n\
             If any plan is in the SAME repo as the current work and has unchecked tasks, \
             declare `continuing`. Otherwise declare `stopping`."
        ));
    }

    sections.push(
        "## Declare\n\n\
         State one of:\n\
         - `continuing: <what remains>`\n\
         - `stopping: <reason>` — when nothing remains in this repo"
            .to_string(),
    );

    Some(format!(
        "PRE-STOP VERIFICATION — check state, then decide.\n\n{}\n\n\
         IMPORTANT: This is a read-only check. Do NOT run commands, do NOT fix anything, do NOT commit. \
         Just review your state and declare continuing or stopping.",
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
        // last_assistant_message is excluded from summary (added separately by with_first_prompt_context)
        assert!(
            !summary.contains("Fixed the scale clamp in affine quantization"),
            "should NOT contain last assistant message"
        );
        assert!(
            !summary.contains("so no way rust can beat python"),
            "should NOT contain raw first prompt"
        );
    }

    #[test]
    fn test_build_prompt_summary_synthesizes_title_only_when_last_message_present() {
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
        // last_assistant_message is excluded from summary; only title is used
        assert_eq!(summary, "Title");
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
            confidence: Some(0.8),
            next_prompt: None,
            reason: None,
            last_assistant_message: None,
            is_synthetic_failure: false,
            stop_phase: context::StopPhase::Working,
        }
    }

    // --- Task 4: evaluate_response() state machine tests ---

    #[test]
    fn test_eval_high_confidence_with_prompt_continues() {
        let input = EvaluationInput {
            confidence: Some(0.9),
            next_prompt: Some("commit changes".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { prompt, reason } => {
                assert_eq!(prompt, "commit changes");
                assert!(reason.contains("confidence 0.90 >= 0.20"));
            }
            _ => panic!("expected Continue, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_no_prompt_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.9),
            next_prompt: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.90 >= 0.20"));
                assert!(reason.contains("no usable prompt"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_low_confidence_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            next_prompt: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.10 < 0.20"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_remaining_work_remaining_work_pattern() {
        // Issue 006 tightened the patterns: "remaining work" alone no longer
        // fires. Use an authoritative task-list marker in a heading line.
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("## Action items\n- fix tests".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::NeedsSecondOpinion { .. }),
            "expected NeedsSecondOpinion for action-items heading with list, got {result:?}"
        );
    }

    #[test]
    fn test_eval_remaining_work_unchecked_checkbox() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("- [ ] do thing".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::NeedsSecondOpinion { .. }),
            "expected NeedsSecondOpinion for unchecked checkbox pattern, got {result:?}"
        );
    }

    #[test]
    fn test_eval_remaining_work_todo_pattern() {
        // Issue 006: the phrase must appear in a list/heading line, not
        // free-form prose.
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("- TODO: fix this".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::NeedsSecondOpinion { .. }),
            "expected NeedsSecondOpinion for TODO list item, got {result:?}"
        );
    }

    #[test]
    fn test_eval_remaining_work_no_match() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("all done, nothing left".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_remaining_work_false_positive_no_actionable_items() {
        let input = EvaluationInput {
            confidence: Some(0.1),
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
        // Issue 006: "remaining work" prose removed; require an authoritative
        // task-list heading.
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some(
                "Done with part 1.\n\n### Action items\n\n- Fix the bug\n- Add tests"
                    .to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::NeedsSecondOpinion {
                extracted_section, ..
            } => {
                assert!(extracted_section.contains("Fix the bug"));
                assert!(extracted_section.contains("false positive"));
            }
            _ => panic!("expected NeedsSecondOpinion, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_with_valid_prompt_continues() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            next_prompt: Some("commit changes".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { prompt, reason } => {
                assert_eq!(prompt, "commit changes");
                assert!(reason.contains("confidence 0.80 >= 0.20"));
            }
            _ => panic!("expected Continue, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_empty_prompt_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            next_prompt: Some("".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.80 >= 0.20"));
                assert!(reason.contains("no usable prompt"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_whitespace_prompt_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            next_prompt: Some("   ".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.80 >= 0.20"));
                assert!(reason.contains("no usable prompt"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_no_prompt_wants_stop_working() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            next_prompt: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.80 >= 0.20"));
                assert!(reason.contains("no usable prompt"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_low_confidence_with_prompt_still_stops() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            next_prompt: Some("review code".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.10 < 0.20"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_confidence_0_3_working_phase_continues_with_prompt() {
        let input = EvaluationInput {
            confidence: Some(0.3),
            next_prompt: Some("go".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { prompt, reason } => {
                assert_eq!(prompt, "go");
                assert!(reason.contains("confidence 0.30 >= 0.20"));
            }
            _ => panic!("expected Continue for confidence 0.3 in Working phase, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_confidence_0_1_working_phase_stops() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.10 < 0.20"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_high_confidence_default_no_prompt_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.80 >= 0.20"));
                assert!(reason.contains("no usable prompt"));
            }
            _ => panic!("expected WantsStop, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_prestop_confidence_below_threshold_stops() {
        let input = EvaluationInput {
            confidence: Some(0.7),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.70 < 0.80"));
            }
            _ => panic!("expected WantsStop for confidence 0.7 in PreStop phase, got {result:?}"),
        }
    }

    #[test]
    fn test_eval_low_confidence_remaining_work_needs_second_opinion() {
        // Issue 006: "Remaining work:" prose removed; require an authoritative
        // task-list marker.
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("## Action items\n- fix test".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::NeedsSecondOpinion { .. }),
            "expected NeedsSecondOpinion for low confidence with action-items heading, got {result:?}"
        );
    }

    #[test]
    fn test_eval_high_confidence_all_plan_done_tag_stripped_from_prompt() {
        let input = EvaluationInput {
            confidence: Some(0.8),
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
    fn test_eval_last_assistant_message_none_low_confidence() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: None,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_eval_last_assistant_message_empty_low_confidence() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(matches!(result, EvaluationResult::WantsStop { .. }));
    }

    #[test]
    fn test_phase_threshold_working_uses_0_2() {
        let input = EvaluationInput {
            confidence: Some(0.2),
            next_prompt: Some("keep going".to_string()),
            stop_phase: context::StopPhase::Working,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("confidence 0.20 >= 0.20"));
            }
            _ => panic!("expected Continue for confidence 0.2 in Working phase, got {result:?}"),
        }
    }

    #[test]
    fn test_phase_threshold_prestop_uses_0_8() {
        let input = EvaluationInput {
            confidence: Some(0.8),
            next_prompt: Some("keep going".to_string()),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("confidence 0.80 >= 0.80"));
            }
            _ => panic!("expected Continue for confidence 0.8 in PreStop phase, got {result:?}"),
        }
    }

    #[test]
    fn test_phase_threshold_prestop_below_threshold_stops() {
        let input = EvaluationInput {
            confidence: Some(0.79),
            next_prompt: Some("please continue".to_string()),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.79 < 0.80"));
            }
            _ => panic!("expected WantsStop for confidence 0.79 in PreStop phase, got {result:?}"),
        }
    }

    #[test]
    fn test_phase_threshold_verified_uses_0_8() {
        let input = EvaluationInput {
            confidence: Some(0.9),
            next_prompt: Some("one more thing".to_string()),
            stop_phase: context::StopPhase::Verified,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::Continue { reason, .. } => {
                assert!(reason.contains("confidence 0.90 >= 0.80"));
            }
            _ => panic!("expected Continue for confidence 0.9 in Verified phase, got {result:?}"),
        }
    }

    #[test]
    fn test_phase_threshold_working_0_19_stops() {
        let input = EvaluationInput {
            confidence: Some(0.19),
            next_prompt: Some("please continue".to_string()),
            stop_phase: context::StopPhase::Working,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(reason.contains("confidence 0.19 < 0.20"));
            }
            _ => panic!("expected WantsStop for confidence 0.19 in Working phase, got {result:?}"),
        }
    }

    #[test]
    fn test_handbrake_prestop_worker_declared_stop() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some(
                "All tasks complete. stopping, nothing related to current work.".to_string(),
            ),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should force stop when worker declares stop in PreStop, got {result:?}"
        );
        assert_eq!(result.source(), DecisionSource::Handbrake);
    }

    #[test]
    fn test_handbrake_verified_worker_declared_stop() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some(
                "Reviewed all plans. stopping, no further action needed.".to_string(),
            ),
            stop_phase: context::StopPhase::Verified,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should force stop in Verified phase, got {result:?}"
        );
        assert_eq!(result.source(), DecisionSource::Handbrake);
    }

    #[test]
    fn test_handbrake_not_triggered_in_working_phase() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("stopping, nothing related to current work.".to_string()),
            stop_phase: context::StopPhase::Working,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "handbrake should NOT trigger in Working phase, got {result:?}"
        );
        assert_ne!(result.source(), DecisionSource::Handbrake);
    }

    #[test]
    fn test_handbrake_not_triggered_without_qualifier() {
        let input = EvaluationInput {
            confidence: Some(0.1),
            last_assistant_message: Some("I am stopping now.".to_string()),
            stop_phase: context::StopPhase::PreStop,
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "stopping alone should not trigger handbrake, got {result:?}"
        );
        assert_ne!(result.source(), DecisionSource::Handbrake);
    }

    #[derive(Debug, PartialEq)]
    enum VerificationGateResult {
        DispatchVerification,
        StopNoPlanFiles,
        #[expect(dead_code)]
        StopAfterVerification,
        StopMaxExceeded,
    }

    fn handle_wants_stop(
        verification_count: u32,
        max_verifications: u32,
        has_verification_prompt: bool,
    ) -> VerificationGateResult {
        if verification_count >= max_verifications {
            VerificationGateResult::StopMaxExceeded
        } else if has_verification_prompt {
            VerificationGateResult::DispatchVerification
        } else {
            VerificationGateResult::StopNoPlanFiles
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
        assert_eq!(result, VerificationGateResult::DispatchVerification);
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
            confidence: Some(0.0),
            reason: Some("model returned zero events (1 total stream events)".to_string()),
            is_synthetic_failure: true,
            ..make_input()
        };
        let result = evaluate_response(&input);
        match result {
            EvaluationResult::WantsStop { reason } => {
                assert!(
                    reason.contains("model returned zero events"),
                    "expected synthetic failure reason, got: {reason}"
                );
            }
            other => panic!("expected WantsStop for synthetic failure, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_synthetic_failure_no_thinking_returns_wants_stop() {
        let input = EvaluationInput {
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
        // "Model returned X Thinking events but no Text output" IS a synthetic failure
        // (starts with "model returned"), but the test name reflects the original intent.
        // evaluate_response just reads is_synthetic_failure from the input — it returns WantsStop
        // for confidence < 0.5 regardless.
        let input = EvaluationInput {
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
        let active = extract_active_project(msg);
        assert_eq!(
            active,
            Some("/Users/katopz/git/temp2/riir-burner".to_string())
        );
    }

    #[test]
    fn test_active_project_extraction_no_file_url() {
        let msg = "just a regular message with no file references";
        let active = extract_active_project(msg);
        assert_eq!(active, None);
    }

    #[test]
    fn test_active_project_extraction_from_plain_path() {
        // Simulates the chain prompt format which includes bare paths (no file:/// prefix)
        let msg = "Continue with plan 239. Check the plan file at /Users/katopz/git/riir-ai/.plans/239_fol_game_rule_extraction.md and proceed.";
        let active = extract_active_project(msg);
        assert_eq!(active, Some("/Users/katopz/git/riir-ai".to_string()));
    }

    #[test]
    fn test_active_project_extraction_prefers_file_url_over_plain() {
        let msg = "file:///Users/katopz/git/project-a/.plans/001.md and also /Users/katopz/git/project-b/.plans/002.md";
        let active = extract_active_project(msg);
        // file:/// should take priority
        assert_eq!(active, Some("/Users/katopz/git/project-a".to_string()));
    }

    #[test]
    fn test_active_project_extraction_longest_path_wins() {
        // When multiple plain paths exist, longest (most specific) wins
        let msg = "Check /Users/katopz/git/riir-ai/.plans/239_fol.md in /Users/katopz/git/riir-ai/.plans/240_eql.md";
        let active = extract_active_project(msg);
        assert_eq!(active, Some("/Users/katopz/git/riir-ai".to_string()));
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
    fn test_detect_remaining_work_ignores_no_remaining_work_prose() {
        // Regression for issue 006: worker says "No remaining work" and the
        // old naive substring match on "remaining work" forced a second-opinion
        // LLM call. Tightened patterns + list-context requirement must skip it.
        let msg = "All 7 commits landed and pushed successfully.\n\nNo remaining work.";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "'No remaining work' in prose must NOT trigger override, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_ignores_pattern_in_prose_without_list_context() {
        // "action items" buried mid-paragraph must not fire.
        let msg = "I reviewed the action items above and they are all complete. Stopping now.";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "pattern in prose without list/heading context must NOT trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_fires_on_action_items_heading_with_list() {
        let msg = "Done with phase 1.\n\n## Action items\n\n- Fix bug A\n- Add tests for B";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "'## Action items' heading followed by list items SHOULD trigger"
        );
    }

    #[test]
    fn test_detect_remaining_work_fires_on_todo_list_item() {
        let msg = "Phase 1 complete.\n\n- TODO: wire up the new config flag";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "'- TODO:' list item SHOULD trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_negation_guard_skips_override() {
        // Even in a list/heading line, a negation cue before the pattern must
        // suppress the override.
        let msg = "Summary.\n\n- No action items left to do — all complete";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "negated 'No action items left to do' must NOT trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_is_list_or_heading_line_markers() {
        assert!(is_list_or_heading_line("- item"));
        assert!(is_list_or_heading_line("* item"));
        assert!(is_list_or_heading_line("+ item"));
        assert!(is_list_or_heading_line("# Heading"));
        assert!(is_list_or_heading_line("## Subheading"));
        assert!(is_list_or_heading_line("1. first"));
        assert!(is_list_or_heading_line("12) twelfth"));
        assert!(!is_list_or_heading_line("Plain prose."));
        assert!(!is_list_or_heading_line("1.NoSpace"));
        assert!(!is_list_or_heading_line(""));
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
            confidence: Some(0.1),
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
            confidence: Some(0.1),
            last_assistant_message: Some(
                "Phase 1 done.\n\n- [ ] Run benchmarks\n- [ ] ~~T5: SIMD~~ Skipped".to_string(),
            ),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::NeedsSecondOpinion { .. }),
            "expected NeedsSecondOpinion for real checkbox with skipped, got {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_no_plan_files() {
        let context_json = r#"{"messages":[]}"#;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_none(), "no plan_files field => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_all_checked() {
        let context_json = r##"{"plan_files":[{"path":".plans/001_test.md","content":"# Plan\n\n- [x] Done task\n- [x] Another done task"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_none(), "all tasks checked => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_has_unchecked() {
        let context_json = r##"{"plan_files":[{"path":".plans/001_test.md","content":"# Plan\n\n- [x] Done\n- [ ] Remaining task\n"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_some(), "unchecked task should return Some");
        let prompt = result.unwrap();
        assert!(
            prompt.contains("001_test.md"),
            "should mention plan filename"
        );
        assert!(
            prompt.contains("1 unchecked"),
            "should report 1 unchecked task"
        );
        assert!(
            prompt.contains("Continue with the next unchecked task"),
            "should instruct to continue"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_multiple_plans() {
        let context_json = r##"{"plan_files":[
            {"path":".plans/001_alpha.md","content":"- [x] Done\n- [ ] T2: Fix bug"},
            {"path":".plans/002_beta.md","content":"- [ ] T1: New feature"},
            {"path":".plans/003_done.md","content":"- [x] All done"}
        ]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_some(), "plans with unchecked tasks => Some");
        let prompt = result.unwrap();
        assert!(prompt.contains("001_alpha.md"), "should list alpha");
        assert!(prompt.contains("002_beta.md"), "should list beta");
        assert!(
            !prompt.contains("003_done.md"),
            "should NOT list fully-done plan"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_invalid_json() {
        let result = detect_remaining_plan_tasks("not json at all", PlanRepoFilter::All, None);
        assert!(result.is_none(), "invalid json => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_skipped_only_is_not_actionable() {
        let context_json = r##"{"plan_files":[{"path":".plans/004_skip.md","content":"- [ ] ~~Task A~~ Skipped\n- [ ] ~~Task B~~ — deferred"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(
            result.is_none(),
            "only skipped/strikethrough checkboxes should not be actionable"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_current_repo_filters_other_repos() {
        // Two repos, each with an unclaimed plan. CurrentRepo filter should
        // pick up only the plan whose path is under the session's work_dirs.
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-a/.plans/001_current.md","content":"- [ ] T1"},
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        let current =
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::CurrentRepo, Some(&work_dirs))
                .expect("current-repo plan should be found");
        assert!(
            current.contains("001_current.md"),
            "current-repo plan should be listed"
        );
        assert!(
            !current.contains("002_other.md"),
            "other-repo plan should be filtered out"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_other_repos_excludes_current() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-a/.plans/001_current.md","content":"- [ ] T1"},
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        let other =
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::OtherRepos, Some(&work_dirs))
                .expect("other-repo plan should be found");
        assert!(
            other.contains("002_other.md"),
            "other-repo plan should be listed"
        );
        assert!(
            !other.contains("001_current.md"),
            "current-repo plan should be excluded"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_current_repo_none_falls_through() {
        // Only other-repo plans exist — CurrentRepo returns None.
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        assert!(
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::CurrentRepo, Some(&work_dirs))
                .is_none(),
            "no current-repo plans => None"
        );
        assert!(
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::OtherRepos, Some(&work_dirs))
                .is_some(),
            "other-repo plan should be found"
        );
    }

    #[test]
    fn test_plan_belongs_to_current_repo_uses_work_dirs() {
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        assert!(
            plan_belongs_to_current_repo("/Users/me/proj-a/.plans/001.md", &work_dirs),
            "plan under a work_dir is current-repo"
        );
        assert!(
            !plan_belongs_to_current_repo("/Users/me/proj-b/.plans/002.md", &work_dirs),
            "plan outside all work_dirs is NOT current-repo"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_recommended_heading() {
        let summary = "# Session Summary\n\n## What Was Accomplished\n\nDid X.\n\n\
                       ## Recommended Next Steps (When Resuming)\n\n\
                       1. Fix the riir-gpu build break\n\
                       2. Finish the Cargo.toml cleanup\n\
                       3. Commit on develop\n\n\
                       ## Active Plan Files\n\n\n- .plans/302_*";
        let result =
            extract_summary_next_steps(summary).expect("recommended next steps should be found");
        assert!(
            result.contains("Fix the riir-gpu build break"),
            "should contain the first recommended step"
        );
        assert!(
            result.contains("Commit on develop"),
            "should contain the last recommended step"
        );
        assert!(
            !result.contains("Active Plan Files"),
            "should NOT bleed into the next section"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_what_remains_heading() {
        let summary = "# Summary\n\n## Original Task\n\nRefactor X.\n\n## What Remains\n\n\
                       - [ ] T2: verify build\n\
                       - [ ] T3: commit\n";
        let result =
            extract_summary_next_steps(summary).expect("What Remains section should be found");
        assert!(
            result.contains("T2: verify build"),
            "should contain the unchecked task"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_ignores_heading_without_actionable_body() {
        let summary = "# Summary\n\n## Next Steps\n\nSee the plan file for details.\n\n## Done\n\nAll good.";
        let result = extract_summary_next_steps(summary);
        assert!(
            result.is_none(),
            "heading with no actionable markers should be ignored"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_no_heading_falls_back_to_prose() {
        // Summary embeds next steps in the last paragraph without a dedicated heading.
        let summary = "# Summary\n\nDid A.\n\nDid B.\n\nStill need to run the benchmark and commit the results.";
        let result =
            extract_summary_next_steps(summary).expect("prose trigger 'need to' should match");
        assert!(
            result.contains("run the benchmark"),
            "fallback should pick up the prose next-step"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_empty_returns_none() {
        assert!(extract_summary_next_steps("").is_none());
        assert!(extract_summary_next_steps("   ").is_none());
    }

    #[test]
    fn test_extract_summary_next_steps_user_real_world_summary() {
        // Reproduces the exact structure from the user's reported bug:
        // the summary had "Recommended Next Steps (When Resuming)" with 3 numbered
        // items. Before the fix, auto_prompt's Phase 2 ignored this entirely and
        // continued with "plan files have remaining unchecked tasks" instead.
        let summary = "# Session Summary\n\n## Original Task\n\nRefactor.\n\n\
                       ## What Was Accomplished\n\nSplit done.\n\n\
                       ## What Remains\n\nSome tasks left.\n\n\
                       ## Recommended Next Steps (When Resuming)\n\n\
                       1. Decide on the riir-gpu build break first\n\
                       2. Skip verification and finish cleanup\n\
                       3. Revert the in-flight work\n\n\
                       ## Active Plan / Issue Files\n\n- .plans/302_* complete";
        let result = extract_summary_next_steps(summary)
            .expect("recommended next steps must be extracted for Phase 2");
        assert!(
            result.contains("Decide on the riir-gpu build break"),
            "must contain step 1"
        );
        assert!(
            result.contains("Revert the in-flight work"),
            "must contain step 3"
        );
        assert!(
            result.contains("Pick up from here"),
            "continuation framing should be present"
        );
    }

    #[test]
    fn test_extract_decision_prompt_3_part_format() {
        let prompt = "## 1. Thread Summary\n\nSummary here\n\n---\n\n## 2. Last Assistant Message\n\nLast msg\n\n---\n\n## 3. Decision\n\nContinue with step 4";
        let result = extract_decision_prompt(prompt);
        assert_eq!(result, Some("Continue with step 4".to_string()));
    }

    #[test]
    fn test_extract_decision_prompt_2_part_format() {
        let prompt =
            "## 1. Last Assistant Message\n\nLast msg\n\n---\n\n## 2. Decision\n\nDo the thing";
        let result = extract_decision_prompt(prompt);
        assert_eq!(result, Some("Do the thing".to_string()));
    }

    #[test]
    fn test_extract_decision_prompt_no_marker() {
        let prompt = "## 1. Thread Summary\n\nNo decision here";
        let result = extract_decision_prompt(prompt);
        assert!(result.is_none(), "no Decision header => None");
    }

    #[test]
    fn test_extract_decision_prompt_empty_after_marker() {
        let prompt = "## 3. Decision\n\n";
        let result = extract_decision_prompt(prompt);
        assert!(result.is_none(), "empty decision text => None");
    }

    #[test]
    fn test_extract_decision_prompt_roundtrip_with_first_prompt_context() {
        let summary = "Implementing feature X in **plan 085**";
        let last_msg = "Completed steps 1-3, need to do step 4";
        let decision = "do step 4 now and commit";
        let full =
            with_first_prompt_context(decision.to_string(), Some(summary), None, Some(last_msg));
        let extracted = extract_decision_prompt(&full);
        assert_eq!(extracted, Some(decision.to_string()));
    }

    #[test]
    fn test_context_overflow_synthetic_failure_routes_to_wants_stop() {
        let input = EvaluationInput {
            confidence: Some(0.0),
            reason: Some("model skipped: context exceeds token limit".to_string()),
            ..make_input()
        };
        let result = evaluate_response(&input);
        assert!(
            matches!(result, EvaluationResult::WantsStop { .. }),
            "synthetic failure from context overflow should route to WantsStop"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_gpu() {
        // Old pattern: "remaining + gpu" was a false positive — the worker might have
        // 5 remaining tasks where only 1 needs GPU and 4 are actionable.
        // New function requires explicit "all remaining ... blocked" language.
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("5 remaining tasks require GPU hardware")),
            "remaining + gpu should NOT suppress fallback — some tasks may still be actionable"
        );
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "All remaining tasks are blocked by GPU hardware requirement"
            )),
            "all remaining + blocked should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_nothing_actionable() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "Nothing actionable to implement without hardware access"
            )),
            "nothing actionable should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_nothing_left() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "Nothing left to do, all tasks complete or blocked"
            )),
            "nothing left to do should suppress fallback"
        );
    }

    // ── looks_like_voluntary_summary ─────────────────────────────────────

    #[test]
    fn looks_like_voluntary_summary_user_real_world_transcript() {
        // The exact message from the user's reported bug: an agent following
        // an "Always end with TL;DR" instruction self-summarized with a
        // `## Summary` heading. auto_prompt's Phase 1 then asked for ANOTHER
        // summary, wasting a full response. This message MUST match so Phase 1
        // is skipped and Phase 2 reuses it directly.
        let msg = "Both commits are clean. The remaining uncommitted files \
                   in riir-game-sdk (.docs/, Cargo.lock, README.md) and \
                   poc-maxman (.docs/, Cargo.toml, Cargo.lock, README.md) are \
                   from another agent's WIP — I left them untouched per the rule.

## Summary

Three commits landed across two repos:

| Repo | Commit | What |
|---|---|---|
| `riir-game-sdk` | `8bad2ea` | `riir-games-cluster` crate skeleton |
| `poc-maxman` | `31adb56` | AGENTS.md references to Issue 025 |

### What was created

**`riir-game-sdk/crates/riir-games-cluster/`** — skeleton crate, compiles clean.

### What's NOT done (intentionally)

- T2–T5 — actual code extraction, needs a plan.

### Next step

Ready to execute Issue 024 (Hero → Avatar rename)?";
        assert!(
            looks_like_voluntary_summary(msg),
            "voluntary `## Summary` heading must trigger skip-Phase-1"
        );
    }

    #[test]
    fn looks_like_voluntary_summary_tldr_heading() {
        assert!(looks_like_voluntary_summary("## TL;DR\n\nDid X, Y, Z. Next: do W."));
        assert!(looks_like_voluntary_summary("### TL;DR\n\nDid the thing."));
    }

    #[test]
    fn looks_like_voluntary_summary_summary_at_h1_h2_h3() {
        assert!(looks_like_voluntary_summary("# Summary\n\nDone."));
        assert!(looks_like_voluntary_summary("## Summary\n\nDone."));
        assert!(looks_like_voluntary_summary("### Summary\n\nDone."));
    }

    #[test]
    fn looks_like_voluntary_summary_phase1_response_still_matches() {
        // The existing is_auto_prompt_summary_response pattern must still be
        // accepted (≥3 of 4 markers) so Phase 2 handles it after Phase 1 fires.
        let phase1_response = "## Original Task\n\nRefactor.\n\n## What Was \
            Accomplished\n\nSplit done.\n\n## What Remains\n\nSome left.\n\n
            ## Active Plan State\n\n- .plans/302 complete";
        assert!(looks_like_voluntary_summary(phase1_response));
    }

    #[test]
    fn looks_like_voluntary_summary_no_heading_no_match() {
        // A normal working message with no summary heading must NOT match —
        // otherwise Phase 1 would never fire and context would overflow with
        // no handoff summary.
        assert!(!looks_like_voluntary_summary(
            "I committed the fix on develop. All tests pass."
        ));
        assert!(!looks_like_voluntary_summary(
            "Working on the refactor now, 3 files done, 2 to go."
        ));
    }

    #[test]
    fn looks_like_voluntary_summary_prose_mention_no_match() {
        // Prose that mentions the word "summary" but isn't a heading must NOT
        // match — avoids false positives when the agent references a summary
        // elsewhere (e.g. the agent ui's own summary, or a prior thread).
        assert!(!looks_like_voluntary_summary(
            "I added a summary field to the struct. Next I'll wire up the UI."
        ));
        assert!(!looks_like_voluntary_summary(
            "See the summary section of the PR description for details."
        ));
    }

    #[test]
    fn looks_like_voluntary_summary_heading_no_space_no_match() {
        // `#Summary` (no space) is not a valid ATX heading — reject it to keep
        // the detector strict and avoid matching version strings like `#Summary`.
        assert!(!looks_like_voluntary_summary("#Summary of changes"));
    }

    #[test]
    fn looks_like_voluntary_summary_heading_too_deep_no_match() {
        // `#### Summary` (level 4+) is too deep to be a deliberate handoff
        // marker — reject to reduce false positives.
        assert!(!looks_like_voluntary_summary("#### Summary\n\nnested note"));
    }

    #[test]
    fn looks_like_voluntary_summary_empty_no_match() {
        assert!(!looks_like_voluntary_summary(""));
        assert!(!looks_like_voluntary_summary("   \n\n  "));
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_no_further() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "No further action possible without external API"
            )),
            "no further action should suppress fallback"
        );
        assert!(
            llm_acknowledged_all_tasks_blocked(Some("No further work can be done at this time")),
            "no further work should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_remaining_work_heading() {
        // This is the key test case from the user's bug report:
        // The summary contains "Remaining Work" as a heading AND mentions "blocked",
        // but NOT all tasks are blocked — some are just not started yet.
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(
                "## Remaining Work (blocked or needs real .mlmodelc)\n\
                 - Refactor main.rs: Not started\n\
                 - Stateful KV cache: Blocked on macOS 15+\n\
                 - FP16 tensor: Blocked on conversion pipeline"
            )),
            "summary with Remaining Work heading + some blocked tasks should NOT suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_just_remaining() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("5 remaining tasks to implement")),
            "remaining without all-blocked language should NOT be detected as blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_just_blocked() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(
                "The build is blocked by a missing dependency"
            )),
            "blocked without all-tasks-are-blocked language should NOT be detected as blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_none() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(None),
            "None should not be blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_empty() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("")),
            "empty string should not be blocked"
        );
    }

    // --- Summary response detection tests ---

    #[test]
    fn test_is_auto_prompt_summary_response_real_summary() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             User asked to implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 complete with 38 tests.\n\
             \n### 3. What Remains\n\
             Phase 5-7 deferred.\n\
             \n### 4. Active Plan State\n\
             Plan 264 is in progress.";
        assert!(
            is_auto_prompt_summary_response(summary),
            "real Phase 1 summary should be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_real_summary_from_bug() {
        // Actual text from bug.md L7968-8059
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             \nUser asked to continue where previous session left off.\n\
             \n### 2. What Was Accomplished\n\
             \n6 commits on develop.\n\
             \n### 3. What Remains\n\
             \nPhase 5-7 need GPU.\n\
             \n### 4. Active Plan State\n\
             \nPlan 264 complete through Phase 4.";
        assert!(
            is_auto_prompt_summary_response(summary),
            "bug.md summary should be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_not_summary() {
        assert!(
            !is_auto_prompt_summary_response("I implemented the feature and committed."),
            "normal assistant response should NOT be detected as summary"
        );
        assert!(
            !is_auto_prompt_summary_response(""),
            "empty string should NOT be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_partial_not_enough() {
        // Only 2 of 4 markers
        let text = "### Original Task\nSome task\n\n### What Remains\nTodo items";
        assert!(
            !is_auto_prompt_summary_response(text),
            "only 2 of 4 markers should NOT be detected (need >= 3)"
        );
    }

    #[test]
    fn test_detect_remaining_work_skips_summary_response() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             Implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 done.\n\
             \n### 3. What Remains\n\
             - [ ] Phase 5: GPU training\n\
             - [ ] Phase 6: Benchmarks\n\
             \n### 4. Active Plan State\n\
             Plan 264 in progress.";
        assert_eq!(
            detect_remaining_work(Some(summary)),
            None,
            "should NOT detect remaining work in auto_prompt Phase 1 summary"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_skips_summary_response() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             Implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 done.\n\
             \n### 3. What Remains\n\
             All remaining tasks are blocked by GPU.\n\
             \n### 4. Active Plan State\n\
             Plan 264 in progress.";
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(summary)),
            "should NOT match 'all tasks blocked' inside auto_prompt Phase 1 summary"
        );
    }

    // --- Decisive stop tests ---

    #[test]
    fn test_decisive_stop_low_confidence_no_prompt() {
        let input = EvaluationInput {
            confidence: Some(0.10),
            next_prompt: None,
            ..make_input()
        };
        assert!(
            is_decisive_stop(&input),
            "confidence 0.10 with no prompt should be decisive"
        );
    }

    #[test]
    fn test_decisive_stop_at_threshold() {
        let input = EvaluationInput {
            confidence: Some(0.15),
            next_prompt: None,
            ..make_input()
        };
        assert!(
            is_decisive_stop(&input),
            "confidence 0.15 with no prompt should be decisive"
        );
    }

    #[test]
    fn test_decisive_stop_above_threshold() {
        let input = EvaluationInput {
            confidence: Some(0.16),
            next_prompt: None,
            ..make_input()
        };
        assert!(
            !is_decisive_stop(&input),
            "confidence 0.16 should NOT be decisive"
        );
    }

    #[test]
    fn test_decisive_stop_with_prompt() {
        let input = EvaluationInput {
            confidence: Some(0.05),
            next_prompt: Some("continue with next task".to_string()),
            ..make_input()
        };
        assert!(
            !is_decisive_stop(&input),
            "having a prompt should prevent decisive stop even at 0.05"
        );
    }

    #[test]
    fn test_decisive_stop_with_empty_prompt() {
        let input = EvaluationInput {
            confidence: Some(0.10),
            next_prompt: Some("   ".to_string()),
            ..make_input()
        };
        assert!(
            is_decisive_stop(&input),
            "whitespace-only prompt should count as no prompt"
        );
    }

    #[test]
    fn test_decisive_stop_none_confidence() {
        let input = EvaluationInput {
            confidence: None,
            next_prompt: None,
            ..make_input()
        };
        assert!(
            is_decisive_stop(&input),
            "None confidence defaults to 0.0, should be decisive"
        );
    }

    // --- Waiting-for-user-decision tests ---
    // Reproduces the Plan 456 close-out bug: worker presented A/B/C/D options
    // and explicitly said "I won't pick for you". The chain fired pre-stop
    // verification anyway, ending in a needless `stopping` declaration.
    // These tests guard `is_waiting_for_user_decision` against regressions.

    #[test]
    fn test_waiting_for_user_decision_explicit_wont_pick() {
        let msg = "Before I do anything else, I need a decision from you. \
                   | Option A | Option B | Option C |\n\
                   Which one? I won't pick for you — A and B both commit to a new plan.";
        assert!(
            is_waiting_for_user_decision(Some(msg)),
            "explicit 'I won't pick for you' with options table should trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_you_decide() {
        let msg = "Both approaches are valid. You decide which one fits your priorities.";
        assert!(
            is_waiting_for_user_decision(Some(msg)),
            "'you decide' phrase should trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_need_your_input() {
        let msg = "I've laid out the tradeoffs. Need your input before proceeding.";
        assert!(
            is_waiting_for_user_decision(Some(msg)),
            "'need your input' phrase should trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_let_me_know_which() {
        let msg = "Pick one of the three. Let me know which you prefer.";
        assert!(
            is_waiting_for_user_decision(Some(msg)),
            "'let me know which' phrase should trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_awaiting() {
        let msg = "Drafted the proposal. Awaiting your decision on the deployment target.";
        assert!(
            is_waiting_for_user_decision(Some(msg)),
            "'awaiting your decision' phrase should trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_permission_seeking_does_not_trigger() {
        // Permission-seeking questions are auto-answered by rule 3 — they must
        // NOT trigger the waiting-for-user path (or the chain would stop
        // unnecessarily on every "want me to proceed?").
        let msg = "Want me to implement this? Should I proceed with the refactor?";
        assert!(
            !is_waiting_for_user_decision(Some(msg)),
            "permission-seeking questions must not be treated as user-decision-required"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_which_approach_alone_does_not_trigger() {
        // "Which approach?" without an explicit deferral is rule 3 territory —
        // orchestration LLM should auto-pick. Only an explicit deferral
        // ("I won't pick", "you decide", etc.) bypasses that.
        let msg = "I see two approaches. Which approach do you recommend?";
        assert!(
            !is_waiting_for_user_decision(Some(msg)),
            "bare 'which approach' without an explicit deferral should not trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_summary_response_skipped() {
        // auto_prompt's own Phase 1 summary responses mention 'what remains' and
        // 'recommended next steps' — they are NOT the worker deferring to user.
        let summary = "### 1. Original Task\nImplement Plan 264.\n\n\
                       ### 2. What Was Accomplished\nPhases 1-4 done.\n\n\
                       ### 3. What Remains\nPhase 5 blocked.\n\n\
                       ### 4. Active Plan State\nPlan 264 closed.\n\
                       You decide on next steps.";
        assert!(
            !is_waiting_for_user_decision(Some(summary)),
            "Phase 1 summary responses must not be mistaken for worker deferral"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_none_message() {
        assert!(
            !is_waiting_for_user_decision(None),
            "None message should not trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_empty_message() {
        assert!(
            !is_waiting_for_user_decision(Some("   \n  \t")),
            "whitespace-only message should not trigger"
        );
    }

    #[test]
    fn test_waiting_for_user_decision_normal_completion_does_not_trigger() {
        // A normal completion message without an explicit deferral should NOT
        // trigger — that path is handled by the LLM confidence / decisive stop.
        let msg = "All tasks complete. Tests pass. Commits landed on develop.";
        assert!(
            !is_waiting_for_user_decision(Some(msg)),
            "normal completion message without explicit deferral should not trigger"
        );
    }

    // --- Verification prompt content tests ---
    // These catch the bug where the prompt told the AI to DO work
    // (cargo check, fix errors, commit) instead of just asking about state.

    #[test]
    fn test_verification_prompt_is_read_only() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        // Must contain the read-only disclaimer
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("do not run commands"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
        assert!(
            lower.contains("do not fix anything"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
        assert!(
            lower.contains("do not commit"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
    }

    #[test]
    fn test_verification_prompt_no_action_imperatives() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        let lower = prompt.to_lowercase();

        // Check items should be questions, not commands
        assert!(
            !lower.contains("fix errors"),
            "prompt should not command 'fix errors', got:\n{prompt}"
        );
        assert!(
            !lower.contains("fix warnings"),
            "prompt should not command 'fix warnings', got:\n{prompt}"
        );
        assert!(
            !lower.contains("commit with"),
            "prompt should not command 'commit with', got:\n{prompt}"
        );
        assert!(
            !lower.contains("run relevant benchmarks"),
            "prompt should not command 'run relevant benchmarks', got:\n{prompt}"
        );
    }

    #[test]
    fn test_verification_prompt_uses_questions_not_commands() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        // Check items should be questions
        assert!(
            prompt.contains("Are there any errors"),
            "diagnostics check should be a question, got:\n{prompt}"
        );
        assert!(
            prompt.contains("Any uncommitted changes"),
            "git check should be a question, got:\n{prompt}"
        );
    }

    // --- Regression tests for force_new_thread (ContextOverflow Phase 2) ---

    #[test]
    fn test_auto_prompt_action_force_new_thread_field() {
        // Verify the force_new_thread field exists and defaults correctly.
        // Phase 1 (ContextOverflow) sets false; Phase 2 (after summary) sets true.
        let action_false = AutoPromptAction {
            from_session_id: acp::SessionId::new("test-session"),
            from_title: None,
            next_prompt: "continue".to_string(),
            work_dirs: None,
            original_user_message: None,
            profile_id: None,
            actual_input_tokens: Some(5000),
            approximate_token_count: 5000,
            last_assistant_message: None,
            force_new_thread: false,
            focus_new_thread: false,
        };
        assert!(!action_false.force_new_thread);

        let mut action_true = AutoPromptAction {
            from_session_id: acp::SessionId::new("test-session"),
            from_title: None,
            next_prompt: "continue".to_string(),
            work_dirs: None,
            original_user_message: None,
            profile_id: None,
            actual_input_tokens: Some(200000),
            approximate_token_count: 200000,
            last_assistant_message: None,
            force_new_thread: false,
            focus_new_thread: false,
        };
        // Simulate Phase 2: reset tokens, set force_new_thread = true
        action_true.actual_input_tokens = None;
        action_true.approximate_token_count = 0;
        action_true.force_new_thread = true;

        assert!(action_true.force_new_thread);
        assert_eq!(action_true.actual_input_tokens, None);
        assert_eq!(action_true.approximate_token_count, 0);
    }

    #[test]
    fn test_context_overflow_summary_state_lifecycle() {
        // Verify the summary state machine transitions correctly:
        //   0 (no state) → Phase 1 sets 1 → Phase 2 clears back to 0.
        // This is the lifecycle that drives force_new_thread behavior.
        let session_id = "test-summary-lifecycle";
        clear_summary_for_session(session_id);

        // Initial state: no summary pending
        assert_eq!(summary_state_for(session_id), 0);

        // Phase 1: AI asked to summarize → state set to 1
        set_summary_state(session_id, 1);
        assert_eq!(summary_state_for(session_id), 1);

        // Phase 2: AI produced summary → state cleared
        clear_summary_for_session(session_id);
        assert_eq!(summary_state_for(session_id), 0);

        // Unexpected state: state 2+ should be clearable
        set_summary_state(session_id, 2);
        assert_eq!(summary_state_for(session_id), 2);
        clear_summary_for_session(session_id);
        assert_eq!(summary_state_for(session_id), 0);
    }

    #[test]
    fn test_phase2_token_reset_prevents_same_thread_dispatch() {
        // Regression: Without force_new_thread, zeroed tokens cause
        // dispatch_action to route Phase 2 continuation to the same thread.
        // The fix ensures force_new_thread overrides the token heuristic.
        //
        // Simulate what dispatch_action does:
        //   use_new_thread = force_new_thread || (is_native_agent && exceeds_threshold)
        let is_native_agent = true;
        let same_thread_threshold = 80000;

        // Before fix: tokens reset to 0, no force_new_thread
        let effective_tokens_before = 0; // Reset in Phase 2
        let force_new_thread_before = false; // Didn't exist
        let use_new_thread_before = force_new_thread_before
            || (is_native_agent && effective_tokens_before >= same_thread_threshold);
        assert!(
            !use_new_thread_before,
            "BUG: zeroed tokens without force_new_thread routes to same thread"
        );

        // After fix: tokens reset to 0 BUT force_new_thread = true
        let effective_tokens_after = 0;
        let force_new_thread_after = true;
        let use_new_thread_after = force_new_thread_after
            || (is_native_agent && effective_tokens_after >= same_thread_threshold);
        assert!(
            use_new_thread_after,
            "FIX: force_new_thread overrides token heuristic"
        );
    }

    // --- Issue 007: API exhaustion + context overflow death spiral ------------

    /// Document the issue 007 guard contract: when `context_exceeds_limit` is
    /// true AND `had_api_error` is true, `decide_with_llm` must return
    /// `RetryAfterBackoff` rather than proceeding to Phase 1/2. We can't call
    /// `decide_with_llm` directly in a unit test (it needs a real LanguageModel
    /// + AsyncApp), so this test documents the decision rule as code and guards
    /// against regressions in the guard condition.
    ///
    /// Deliberately keyed on `had_api_error` (completion request failed), not
    /// the broader `had_error` (also set by any failed tool call). An earlier
    /// version of this guard used `had_error` and misfired on ordinary tool
    /// failures unrelated to API health, permanently stopping healthy threads
    /// with a misleading "likely rate limit" reason.
    #[test]
    fn test_issue_007_guard_condition_contract() {
        // The guard fires iff both conditions hold. Each row is
        // (context_exceeds_limit, had_api_error) -> should_defer.
        let cases: &[(bool, bool, bool)] = &[
            // Normal context-overflow Phase 1/2 path: no API error, proceed.
            (true, false, false),
            // Issue 007 scenario: API error + overflow → defer with backoff.
            (true, true, true),
            // API error but context fits → normal lightweight path, no defer.
            (false, true, false),
            // Clean stop with room to spare → normal path, no defer.
            (false, false, false),
        ];
        for &(ctx_exceeds, had_api_err, expect_defer) in cases {
            let should_defer = ctx_exceeds && had_api_err;
            assert_eq!(
                should_defer, expect_defer,
                "guard mismatch for (context_exceeds_limit={ctx_exceeds}, had_api_error={had_api_err})"
            );
        }
    }

    /// Issue 007 backoff delay must be monotonically non-decreasing as the
    /// failure count climbs (so sustained rate-limit retries don't pile up
    /// faster than the upstream API can recover). Mirrors the contract used
    /// inside `decide_with_llm` and the unified retry loop.
    #[test]
    fn test_issue_007_backoff_delay_is_monotonic() {
        let config = load_config_cached().unwrap_or_default();
        let mut prev: u64 = 0;
        for failure_count in 1..=config.max_llm_retries {
            let delay = config.backoff_delay_ms(failure_count);
            assert!(
                delay >= prev,
                "backoff_delay_ms({failure_count})={delay} < previous {prev} — retries would pile up"
            );
            prev = delay;
        }
    }

    /// The RetryAfterBackoff variant must be constructible and Debug-printable
    /// (the unified retry loop in `on_thread_stopped` formats it via the
    /// `Debug` impl on `AutoPromptOutcome` indirectly through `{reason}`).
    #[test]
    fn test_issue_007_outcome_variant_constructible() {
        let outcome = AutoPromptOutcome::RetryAfterBackoff {
            delay_ms: 1_000,
            reason: "test".to_string(),
        };
        let debug = format!("{outcome:?}");
        assert!(debug.contains("RetryAfterBackoff"), "debug repr missing variant name: {debug}");
        assert!(debug.contains("1000"), "debug repr missing delay_ms: {debug}");
    }

    #[test]
    fn test_verification_prompt_no_plan_files_still_returns_prompt() {
        // Even without plan files, the function returns Some with just
        // the checklist (no Remaining Plans section).
        let context_json = r#"{"plan_files":[]}"#;
        let result = build_pre_stop_verification_prompt(context_json, &None);
        assert!(
            result.is_some(),
            "should return Some even with empty plan files"
        );
        let prompt = result.unwrap();
        assert!(
            !prompt.contains("## Remaining Plans"),
            "should not have Remaining Plans section when no plan files"
        );
    }

    // --- Regression tests for retry dispatch path (## 3. Decision roundtrip) ---

    /// Simulates what `dispatch_action` does when it receives a `Continue(action)`:
    /// 1. `with_first_prompt_context` wraps the decision in structured format
    /// 2. `extract_decision_prompt` extracts the decision back out
    /// 3. The result must be non-None (used by `build_auto_prompt_follow_up` to
    ///    produce `## 3. Decision` in the new thread's follow-up)
    ///
    /// If this test fails, either the prompt format changed or extraction broke.
    #[test]
    fn test_dispatch_action_decision_roundtrip_3_part() {
        let summary = "Implementing feature X in plan 085";
        let last_msg = "Completed steps 1-3, need to do step 4";
        let decision = "do step 4 now and commit";

        // This is what decide_with_llm produces via make_continue
        let next_prompt =
            with_first_prompt_context(decision.to_string(), Some(summary), None, Some(last_msg));

        // Verify the structured format is correct
        assert!(
            next_prompt.contains("## 1. Thread Summary"),
            "3-part format should start with Thread Summary"
        );
        assert!(
            next_prompt.contains("## 2. Last Assistant Message"),
            "3-part format should have Last Assistant Message"
        );
        assert!(
            next_prompt.contains("## 3. Decision"),
            "3-part format MUST have Decision section"
        );

        // This is what dispatch_action does
        let extracted = extract_decision_prompt(&next_prompt);
        assert_eq!(
            extracted,
            Some(decision.to_string()),
            "dispatch_action must be able to extract the decision for build_auto_prompt_follow_up"
        );
    }

    /// Same roundtrip but for the 2-part fallback (no summary, only last assistant message).
    #[test]
    fn test_dispatch_action_decision_roundtrip_2_part() {
        let last_msg = "Completed steps 1-3";
        let decision = "continue with step 4";

        let next_prompt = with_first_prompt_context(
            decision.to_string(),
            None, // no summary
            None,
            Some(last_msg),
        );

        assert!(
            next_prompt.contains("## 1. Last Assistant Message"),
            "2-part format should start with Last Assistant Message"
        );
        assert!(
            next_prompt.contains("## 2. Decision"),
            "2-part format MUST have Decision section"
        );
        assert!(
            !next_prompt.contains("## 3. Decision"),
            "2-part format should NOT have ## 3. Decision"
        );

        let extracted = extract_decision_prompt(&next_prompt);
        assert_eq!(
            extracted,
            Some(decision.to_string()),
            "extract_decision_prompt must find ## 2. Decision via alt_marker fallback"
        );
    }

    /// Regression: when both summary and last_assistant_message are None,
    /// with_first_prompt_context returns raw text (no Decision header).
    /// extract_decision_prompt returns None, so dispatch_action falls through
    /// to ContentBlock path instead of ThreadSummary. This is correct behavior
    /// but should be documented.
    #[test]
    fn test_dispatch_action_decision_none_when_no_context() {
        let decision = "just do the thing";

        let next_prompt = with_first_prompt_context(
            decision.to_string(),
            None, // no summary
            None,
            None, // no last assistant message
        );

        assert_eq!(
            next_prompt, decision,
            "no context => raw prompt, no structured headers"
        );

        let extracted = extract_decision_prompt(&next_prompt);
        assert_eq!(
            extracted, None,
            "raw prompt has no Decision header => extract returns None"
        );
    }

    #[test]
    fn test_parse_response_recovers_from_duplicate_key() {
        let raw = r#"```json
{"confidence": 0.75, "next_prompt": "Continue with plan 260", "reason": "Plan 257 done", "thread_summary": "Plan 257 fully implemented.", "thread_summary": null}
```"#;
        let result = parse_response(raw).expect("should recover from duplicate key");
        assert_eq!(
            result.next_prompt.as_deref(),
            Some("Continue with plan 260")
        );
        assert_eq!(result.confidence, Some(0.75));
        assert_eq!(
            result.thread_summary.as_deref(),
            Some("Plan 257 fully implemented.")
        );
    }

    #[test]
    fn test_parse_response_no_duplicate_still_works() {
        let raw = r#"```json
{"confidence": 0.9, "next_prompt": "Do the thing", "reason": "work remains", "thread_summary": "Summary here."}
```"#;
        let result = parse_response(raw).expect("normal JSON should parse fine");
        assert_eq!(result.next_prompt.as_deref(), Some("Do the thing"));
        assert_eq!(result.confidence, Some(0.9));
    }

    #[test]
    fn test_rebuild_deduplicated_json_keeps_first_occurrence() {
        let json = r#"{"a": 1, "b": "two", "a": 3}"#;
        let result = rebuild_deduplicated_json(json).expect("should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should be valid JSON");
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], "two");
    }

    #[test]
    fn test_rebuild_deduplicated_json_duplicate_null_after_value() {
        let json = r#"{"thread_summary": "long summary here", "confidence": 0.75, "thread_summary": null}"#;
        let result = rebuild_deduplicated_json(json).expect("should succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should be valid JSON");
        assert_eq!(parsed["thread_summary"], "long summary here");
        assert_eq!(parsed["confidence"], 0.75);
    }

    #[test]
    fn test_detect_plan_skip_catches_higher_plan_jump() {
        let context = serde_json::json!({
            "plan_files": [
                {
                    "path": "/repo/.plans/284_plan.md",
                    "content": "- [ ] Task 1\n- [x] Task 2\n- [ ] Task 3"
                },
                {
                    "path": "/repo/.plans/292_worms.md",
                    "content": "- [ ] Task A\n- [ ] Task B"
                }
            ]
        })
        .to_string();

        let result = detect_plan_skip("Start with plan 292 — it has pure code tasks", &context);
        assert_eq!(
            result,
            Some("/repo/.plans/284_plan.md".to_string()),
            "should detect skip from 292 to lower plan 284"
        );
    }

    #[test]
    fn test_detect_plan_skip_allows_same_plan() {
        let context = serde_json::json!({
            "plan_files": [
                {
                    "path": "/repo/.plans/284_plan.md",
                    "content": "- [ ] Task 1\n- [x] Task 2"
                }
            ]
        })
        .to_string();

        let result = detect_plan_skip("Continue with plan 284 — implement Task 1", &context);
        assert_eq!(result, None, "should allow continuing the same plan");
    }

    #[test]
    fn test_detect_plan_skip_allows_when_no_lower_plan_has_unchecked() {
        let context = serde_json::json!({
            "plan_files": [
                {
                    "path": "/repo/.plans/284_plan.md",
                    "content": "- [x] Task 1\n- [x] Task 2"
                },
                {
                    "path": "/repo/.plans/292_worms.md",
                    "content": "- [ ] Task A\n- [ ] Task B"
                }
            ]
        })
        .to_string();

        let result = detect_plan_skip("Start with plan 292", &context);
        assert_eq!(result, None, "lower plan is all done, skip is fine");
    }

    #[test]
    fn test_detect_plan_skip_returns_none_for_no_plan_numbers() {
        let context = serde_json::json!({
            "plan_files": [
                {
                    "path": "/repo/.plans/284_plan.md",
                    "content": "- [ ] Task 1"
                }
            ]
        })
        .to_string();

        let result = detect_plan_skip("Continue working on the current task", &context);
        assert_eq!(result, None, "no plan number in prompt => no skip detected");
    }

    #[test]
    fn test_extract_plan_numbers() {
        assert_eq!(
            extract_plan_numbers("start with plan 292 worms fft"),
            vec![292]
        );
        assert_eq!(
            extract_plan_numbers(".plans/284_plan.md and plan 290"),
            vec![284, 290]
        );
        assert_eq!(
            extract_plan_numbers("no plan numbers here"),
            Vec::<u32>::new()
        );
        assert_eq!(
            extract_plan_numbers("plan 42 is too small"),
            Vec::<u32>::new(),
            "numbers < 100 should be ignored"
        );
    }

    #[test]
    fn test_build_plan_correction_prompt() {
        let result = build_plan_correction_prompt("/repo/.plans/284_plan.md");
        assert!(result.contains("284_plan.md"));
        assert!(result.contains("NOT valid reasons to skip"));
        assert!(result.contains("GPU training"));
        assert!(result.contains("benchmarks"));
    }
}
