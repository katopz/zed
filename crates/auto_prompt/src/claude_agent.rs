//! Claude (ACP) agent auto-prompt decision logic.
//!
//! Intentionally simple and isolated from the native-agent path. Below the
//! context-overflow threshold Claude Code manages its own context window,
//! compaction, and tool loop internally — Zed's auto_prompt layer must not
//! stop, compact, or fork a new thread for it. The only job here is to look
//! at the agent's last 2-3 paragraphs of output, ask the orchestration LLM
//! whether the task is done, and if not, produce the next prompt to nudge
//! the same thread forward.
//!
//! Contract (do not change without explicit owner sign-off):
//!   1. Never return `ContextOverflow` and never set `force_new_thread = true`
//!      — EXCEPT above the Claude overflow gate (`claude_context_overflow_tokens`,
//!      which defaults to following the native `max_context_tokens` gate;
//!      plan 023 A3): the thread is near the model ceiling, so `decide_claude`
//!      routes it through the shared native Phase 1/2 summarize→fork flow
//!      (`context_overflow_outcome`) via a `NeedsLlmCall` carrying
//!      `context_exceeds_limit = true`. Below the threshold: always
//!      same-thread.
//!   2. No pre-stop verification, no max-iterations gate, no rules-based stop
//!      — except the session-limit rule (`decide_claude`), which schedules a
//!      same-thread continuation at the provider's embedded reset time
//!      instead of consulting the (equally rate-limited) orchestrator.
//!   3. The only hard stops are: user cancel, no model configured, or the
//!      configured default model is not Anthropic (see `decide_claude`).

use agent_client_protocol::schema::v1 as acp;
use anyhow::Context as _;
use futures::{StreamExt, pin_mut};
use gpui::App;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::sync::Arc;
use std::time::Duration;

use crate::{
    AutoPromptAction, AutoPromptDecision, AutoPromptDelayReason, AutoPromptOutcome, LlmCallData,
    get_iteration,
};

/// Maximum chars of the last assistant message to send to the orchestration LLM.
/// Targets the last 2-3 paragraphs — enough signal to decide done-vs-continue
/// without paying for the full thread context.
const LAST_MESSAGE_BUDGET_CHARS: usize = 4_000;

/// Minimum confidence for the orchestration LLM's "continue" decision to be
/// honored. Below this we stop — uncertainty means the task is likely done or
/// the LLM cannot tell, and another nudge would just loop.
const CONTINUE_CONFIDENCE_THRESHOLD: f64 = 0.5;

/// System prompt for the Claude-path orchestration LLM.
///
/// Deliberately tiny compared to the native-agent system prompt: no plan
/// files, no stop-phase gating, no permission-seeking heuristics. Claude
/// already has the full conversation — we just need a continue-or-stop
/// verdict and, if continue, the next instruction.
const CLAUDE_SYSTEM_PROMPT: &str = "\
You are an auto-prompt orchestrator for a Claude Code agent thread.

You receive the agent's last 2-3 paragraphs of output. Decide whether the task
is complete or the agent should keep working.

Respond ONLY with valid JSON — no prose, no markdown fences:
{\"continue\": bool, \"confidence\": float, \"next_prompt\": string | null, \"reason\": string}

Rules:
- continue=true iff the agent clearly has more work to do (unfinished steps,
  remaining tasks, partial implementation, error to fix, question it should
  answer itself by continuing).
- continue=false iff the task is done, the agent is waiting for genuine user
  input (API key, credentials, explicit choice the user must make), or the
  agent has stopped with a clear completion summary.
- confidence is 0.0..1.0 — how sure you are about the continue/stop verdict.
- next_prompt: when continue=true, a direct instruction for the next step
  (imperative, standalone — NOT a conversational reply). null when stop.
- reason: one short sentence explaining the verdict.
- Only include each key ONCE. Do not duplicate keys.";

/// System prompt for the hidden-thread orchestrator (`claude-hidden-orchestrator`).
///
/// Unlike `CLAUDE_SYSTEM_PROMPT`, this is sent to a full Claude Code session
/// that has tool access. The prompt MUST forbid tool use and demand JSON-only
/// output, otherwise the hidden session could start doing real work instead of
/// just judging the worker's output (tool-leak). This is the primary safety
/// control for the hidden-thread path.
///
/// Decision rules are ported from `default_auto_prompt_system_prompt.txt` so the
/// hidden path has the same task-awareness as the native-agent GLM path:
/// plan_summary awareness (unchecked `[ ]` tasks → continue), stop-phase
/// thresholds, permission-seeking auto-answer, and the "never declare done/blocked
/// when unchecked tasks remain" rule. See .plans/014 (context-parity fix).
#[cfg(feature = "claude-hidden-orchestrator")]
const HIDDEN_ORCHESTRATOR_PROMPT: &str = "\
You are an auto-prompt orchestrator. Your ONLY job is to read another Claude
Code agent's last output and the plan context, then decide whether IT should
continue or stop.

HARD CONSTRAINTS (do not violate under any circumstance):
- Do NOT run any tools. Do NOT read or write files. Do NOT use any tool.
- Do NOT do the task yourself. You are a judge, not a worker.
- Respond with ONLY a single JSON object and nothing else — no prose,
  no markdown fences, no explanation outside the JSON.

JSON schema (each key once, never duplicate):
  {\"continue\": bool, \"confidence\": float, \"next_prompt\": string|null, \"reason\": string}

You receive:
- Context JSON with: stop_phase (Working/PreStop/Verified), iteration_count,
  had_error, last_assistant_message, plan_summary (list of plans with unchecked
  task counts). READ plan_summary — it is the most important continue signal.
- Worker's last output (2-3 paragraphs).

## Decision rules (in priority order):

1. CHECK plan_summary FIRST. This is the strongest signal:
   - If ANY plan has unchecked tasks (unchecked > 0) → continue=true,
     confidence >= 0.8. The worker is NOT done.
   - NEVER declare a plan \"done\" or \"blocked\" when it has unchecked tasks.
     \"Blocked on GPU/training/benchmark\" is NOT a valid stop reason — those
     are tasks to implement, not skip.
   - If a task requires GPU training → next_prompt = \"Implement the GPU training
     task. Set up the pipeline and benchmarks.\"
   - If a task requires benchmarks → next_prompt = \"Implement the benchmark.
     Write the benchmark code and run it.\"
   - Prefer the LOWEST-NUMBERED plan with unchecked tasks. Do NOT skip to a
     newer plan just because it looks easier.
   - next_prompt should reference the plan file: \"Continue with [task] from
     .plans/NNN. Mark completed steps as [x].\"

2. CHECK last_assistant_message for completion signals:
   - Mentions \"remaining\", \"next step\", \"still need\", \"todo\", or lists unchecked
     work → continue=true, confidence >= 0.7, next_prompt = that work as instruction.
   - Summarizes completion with NOTHING left (AND plan_summary is empty) →
     continue=false, confidence >= 0.8.
   - Says \"done\" BUT plan_summary shows unchecked tasks → plan_summary wins,
     continue=true.

3. PERMISSION-SEEKING questions → answer automatically by restating the task:
   - \"Want me to implement X?\" → continue=true, confidence >= 0.8,
     next_prompt = \"Implement X as described. Production grade only.\"
   - \"Should I proceed?\" → continue=true, confidence >= 0.8,
     next_prompt = \"Proceed with the work described. Production grade only.\"
   - Do NOT prefix next_prompt with \"Yes\", \"Sure\", \"OK\" — next_prompt is a
     standalone instruction, not a conversational reply.

4. USER-DECISION questions → stop (genuinely needs the human):
   - Triggers: \"I won't pick for you\", \"you decide\", \"need your input\",
     \"awaiting your decision\".
   - → continue=false, confidence >= 0.7. Another nudge produces the same
     question; stop so the user sees it.

5. STOP-PHASE thresholds (stop_phase is in the Context JSON):
   - Working phase: lenient — when in doubt, lean toward continuing.
   - PreStop phase: strict — the worker already tried to stop once. Only
     continue if unchecked tasks clearly remain (confidence >= 0.8).
   - Verified phase: very strict — only continue on hard evidence of remaining work.

6. iteration_count > 15 → continue=false (runaway loop guard).

7. No active task / greeting / small talk → continue=false, confidence >= 0.8.
   Do NOT fabricate work.

- confidence is 0.0..1.0 — how sure you are of the continue/stop verdict.
- reason: one short sentence explaining the verdict.";

/// Decide the next auto-prompt action for a Claude (ACP) agent thread.
///
/// Two orchestrator backends, selected at compile time by the
/// `claude-hidden-orchestrator` feature:
///
/// - **default build** (requires an Anthropic API key configured in Zed):
///   returns `NeedsLlmCall` carrying the configured default model. Only
///   Anthropic is honored — Claude Code's own auth (browser/subscription) is
///   invisible to `LanguageModelRegistry`, so orchestrating its continuation
///   needs a real Anthropic key. Returns `NoAction` if no model is configured
///   or the default isn't Anthropic (falling back to another provider, e.g. an
///   already-rate-limited one you switched to Claude to get away from, defeats
///   the point).
/// - **`claude-hidden-orchestrator` feature** (no API key required): returns
///   `NeedsLlmCall` carrying the worker's connection + project, so the async
///   phase can spawn an off-screen hidden Claude Code session on the same
///   connection and ask IT to decide. Reuses Claude Code's own auth. The
///   configured LanguageModelRegistry model (if any) is carried only to
///   satisfy `LlmCallData`'s shape and is ignored. Returns `NoAction` if no
///   model is configured (the struct still needs *some* model slot) or there's
///   no assistant message to reason about. See
///   `.plans/014_claude_offscreen_orchestrator.md`.
///
/// Returns `DispatchAfterDelay` only when a session-limit reset time was
/// parsed from the worker's turn error or synthetic message (scheduled at
/// reset + margin; see `session_limit`). Otherwise never returns
/// `DispatchNow` / `DispatchAfterDelay` / `ContextOverflow` — except above
/// `claude_context_overflow_tokens` (plan 023 A3), where it returns a
/// `NeedsLlmCall` with `context_exceeds_limit = true` that the caller routes
/// through the native shared overflow flow.
pub fn decide_claude(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    _used_tools: bool,
    stop_reason: &acp::StopReason,
    cx: &App,
) -> AutoPromptDecision {
    log::info!(
        "[auto_prompt::claude] decide_claude called: stop_reason={:?}",
        stop_reason
    );

    // Config kill switch (plan 031 silent stop): drop the automatic Claude
    // chain with only a log line. Manual clicks bypass this function.
    if crate::paused() {
        log::warn!(
            "[auto_prompt::claude] paused — silently stopping chain (stop_reason={stop_reason:?})"
        );
        return AutoPromptDecision::NoAction;
    }

    // Verdict ping-pong (proposal 001): never auto-continue a verdict thread
    // mid-negotiation, whichever agent backs it.
    if acp_thread::verdict::is_active(thread.read(cx).session_id()) {
        log::info!("[auto_prompt::claude] verdict negotiation active — skipping");
        return AutoPromptDecision::NoAction;
    }

    // User/system cancel — never auto-continue.
    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::info!("[auto_prompt::claude] Cancelled — stopping chain");
        let session_id_str = thread.read(cx).session_id().to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return AutoPromptDecision::NoAction;
    }

    // Session limit: the reset time is embedded in the turn error or the
    // synthetic message ("You've hit your session limit · resets 1:20am
    // (Asia/Bangkok)"). Schedule the continuation at reset + margin instead
    // of asking the hidden orchestrator — it shares the subscription and
    // would hit the same limit. Runs before the model lookup: no model needs
    // to be configured, because no orchestrator is consulted.
    // See .plans/018_session_limit_scheduled_retry.md.
    {
        let margin_secs = crate::load_config_cached()
            .map(|config| config.session_limit_margin_secs)
            .unwrap_or(crate::session_limit::DEFAULT_SESSION_LIMIT_MARGIN_SECS);
        if let Some(limit) =
            crate::session_limit::session_limit_from_thread(&thread.read(cx), cx, margin_secs)
        {
            log::warn!(
                "[auto_prompt::claude] PATH=session_limit: reset at {} — scheduling continuation in {}ms",
                limit.retry_display,
                limit.retry_delay_ms
            );
            let thread_ref = thread.read(cx);
            let last_assistant_message = thread_ref
                .last_assistant_message_text(cx)
                .filter(|message| !crate::session_limit::looks_like_usage_limit(message));
            let action = AutoPromptAction {
                from_session_id: thread_ref.session_id().clone(),
                from_title: thread_ref.title().map(|title| title.to_string()),
                next_prompt:
                    "The Claude session limit window has reset. Continue from where you left off."
                        .to_string(),
                work_dirs: thread_ref.work_dirs().map(|pl| pl.paths().to_vec()),
                original_user_message: None,
                profile_id: None,
                actual_input_tokens: thread_ref.token_usage().map(|usage| usage.input_tokens),
                approximate_token_count: 0,
                last_assistant_message,
                force_new_thread: false,
                focus_new_thread: false,
            };
            return AutoPromptDecision::DispatchAfterDelay {
                action,
                delay_ms: limit.retry_delay_ms,
                reason: AutoPromptDelayReason::UsageLimitReset,
            };
        }
    }

    // Need a configured model to reason about the next step.
    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let configured_model = registry.default_model();

    // Plan 023 A3 (req 1): context-overflow parity. Above the Claude
    // overflow gate (defaults to the native `max_context_tokens` gate) the
    // thread is close to the model ceiling — route it through the shared
    // native Phase 1/2 summarize→fork flow instead of same-thread
    // continuation. Below the threshold: unchanged same-thread behavior.
    if let Some(decision) = claude_context_overflow_decision(thread, configured_model.as_ref(), cx)
    {
        return decision;
    }

    // Two orchestrator backends, selected at compile time:
    //
    // 1. claude-hidden-orchestrator (default for operators without an Anthropic
    //    API key): spawn an off-screen hidden Claude Code session on the same
    //    connection and ask IT to decide continue/stop. Reuses Claude Code's
    //    own auth — no LanguageModelRegistry model required. See
    //    .plans/014_claude_offscreen_orchestrator.md.
    // 2. default (requires an Anthropic key in Zed): a streaming LLM call via
    //    the configured default model. Only Anthropic is honored — see the
    //    rationale below.
    #[cfg(not(feature = "claude-hidden-orchestrator"))]
    {
        let Some(configured_model) = configured_model else {
            log::warn!("[auto_prompt::claude] No language model configured in Zed — stopping");
            return AutoPromptDecision::NoAction;
        };
        let model = configured_model.model;

        // Claude Code authenticates itself (browser/subscription) outside Zed's
        // LanguageModelRegistry, so the only way to orchestrate its continuation
        // is with a real Anthropic-backed model configured as Zed's default. If
        // the default model is some other provider, skip rather than silently
        // burn calls against it — that provider may well be the one you're
        // running Claude as a fallback for in the first place.
        if model.provider_id() != language_model::ANTHROPIC_PROVIDER_ID {
            log::info!(
                "[auto_prompt::claude] Default model provider is {:?}, not Anthropic — \
                 skipping auto-continue for this Claude Code thread",
                model.provider_id()
            );
            return AutoPromptDecision::NoAction;
        }
        return claude_decision_needs_llm(thread, stop_reason, model, cx);
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    {
        return claude_decision_hidden(thread, stop_reason, configured_model, cx);
    }
}

/// Shared extraction of the worker thread's last message + metadata, used by
/// both orchestrator backends. Returns `(session_id, title, work_dirs, last_message)`
/// or `None` if there's no assistant message to reason about (caller stops).
fn extract_worker_signal(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    cx: &App,
) -> Option<(
    acp::SessionId,
    Option<String>,
    Option<Vec<std::path::PathBuf>>,
    String,
)> {
    let thread_ref = thread.read(cx);
    let session_id = thread_ref.session_id().clone();
    let title = thread_ref.title().map(|t| t.to_string());
    let work_dirs = thread_ref.work_dirs().map(|pl| pl.paths().to_vec());

    let full_last_message = thread_ref.last_assistant_message_text(cx);
    let last_assistant_message = full_last_message
        .as_deref()
        .map(truncate_last_paragraphs)
        .filter(|s| !s.trim().is_empty())?;

    // Phase 2: broadcast the summary to the agent board if the worker self-summarized.
    maybe_broadcast_summary_to_board(&session_id, full_last_message.as_deref());

    Some((session_id, title, work_dirs, last_assistant_message))
}

/// Pure threshold check for the Claude overflow gate, isolated so the
/// "no usage data → stay same-thread" policy is unit-testable without a
/// live thread (plan 023 A3).
fn claude_tokens_exceed_overflow(effective_tokens: Option<u64>, threshold: usize) -> bool {
    // Without API-reported usage we cannot distinguish 10k from 400k tokens;
    // stay on the same-thread path rather than fork on a guess.
    effective_tokens.is_some_and(|tokens| tokens as usize > threshold)
}

/// Effective context tokens for the Claude overflow gate.
///
/// ACP `UsageUpdate` populates `used_tokens` ("tokens currently in context"
/// — what Claude Code reports), while `input_tokens` is only set from
/// stop-response usage behind the `acp-beta` feature flag. Take the max of
/// the two so the gate fires no matter which field the agent populates.
fn claude_effective_context_tokens(usage: &acp_thread::TokenUsage) -> u64 {
    usage.input_tokens.max(usage.used_tokens)
}

/// Claude context-overflow parity gate (plan 023 A3, req 1).
///
/// Returns `Some(NeedsLlmCall { context_exceeds_limit: true })` when the
/// thread's effective context tokens exceed the Claude overflow gate — the
/// caller (agent_ui) then routes the decision through the native
/// `decide_with_llm`, whose shared `context_overflow_outcome` runs Phase 1
/// (same-thread summarize) → Phase 2 (new thread with inlined summary).
/// Returns `None` below the threshold, without usage data, or with no
/// configured model (the normal backends then handle the NoAction).
fn claude_context_overflow_decision(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    configured_model: Option<&language_model::ConfiguredModel>,
    cx: &App,
) -> Option<AutoPromptDecision> {
    let threshold = crate::load_config_cached()
        .map(|config| config.effective_claude_context_overflow_tokens())
        .unwrap_or_else(|_| crate::default_max_context_tokens());

    let thread_ref = thread.read(cx);
    let effective_tokens = thread_ref
        .token_usage()
        .map(claude_effective_context_tokens);
    if !claude_tokens_exceed_overflow(effective_tokens, threshold) {
        return None;
    }

    // The overflow state machine never calls the LLM with `data.model`, but
    // `LlmCallData` requires the slot — bail like the normal paths when
    // nothing is configured.
    let configured = configured_model?;

    let session_id = thread_ref.session_id().clone();
    log::warn!(
        "[auto_prompt::claude] effective tokens {:?} > {threshold} — routing to shared context-overflow flow (session={session_id})",
        effective_tokens
    );

    let title = thread_ref.title().map(|t| t.to_string());
    let work_dirs = thread_ref.work_dirs().map(|pl| pl.paths().to_vec());
    let last_assistant_message = thread_ref.last_assistant_message_text(cx);
    let plan_files = crate::read_plan_files_cached(&crate::plan_inputs_without_message(thread_ref));

    // Shape matches what `detect_remaining_plan_tasks` parses, so Phase 2's
    // plan-task fallback works on this path too.
    let context_json = serde_json::json!({
        "session_id": session_id.to_string(),
        "last_assistant_message": last_assistant_message,
        "plan_files": plan_files,
    })
    .to_string();

    Some(AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model: configured.model.clone(),
        // Unused by the overflow path (Phase 1/2 are deterministic and the
        // pending-question fast path has its own prompt).
        system_prompt: String::new(),
        context_json,
        project_root: work_dirs.as_ref().and_then(|d| d.first().cloned()),
        session_id,
        title,
        iteration_count: get_iteration(),
        max_verification_attempts: 0,
        work_dirs,
        first_user_message: None,
        original_user_message: None,
        last_assistant_message,
        profile_id: None,
        actual_input_tokens: effective_tokens,
        had_error: false,
        had_api_error: false,
        stop_phase: crate::context::StopPhase::Working,
        context_exceeds_limit: true,
        approximate_token_count: 0,
        connection: None,
        project: None,
        peer_agent_states: crate::peer_states::unmuted_states_for_context(),
    }))
}

/// LLM-call backend (default build): package a `NeedsLlmCall` decision carrying
/// the configured Anthropic model + the worker's last paragraphs as JSON context.
/// The caller then spawns `decide_claude_with_llm`.
#[cfg(not(feature = "claude-hidden-orchestrator"))]
fn claude_decision_needs_llm(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    stop_reason: &acp::StopReason,
    model: Arc<dyn LanguageModel>,
    cx: &App,
) -> AutoPromptDecision {
    let Some((session_id, title, work_dirs, last_assistant_message)) =
        extract_worker_signal(thread, cx)
    else {
        log::info!("[auto_prompt::claude] No assistant message to reason about — stopping chain");
        crate::reset_iteration_with_session(&thread.read(cx).session_id().to_string());
        return AutoPromptDecision::NoAction;
    };

    let iteration_count = get_iteration();
    log::info!("[auto_prompt::claude] iteration {iteration_count}, handing to orchestration LLM");

    let context_json = serde_json::json!({
        "session_id": session_id.to_string(),
        "iteration_count": iteration_count,
        "last_assistant_message": last_assistant_message,
    })
    .to_string();

    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model: configured.model,
        system_prompt: HIDDEN_ORCHESTRATOR_PROMPT.to_string(),
        context_json,
        project_root: work_dirs.as_ref().and_then(|d| d.first().cloned()),
        session_id,
        title,
        iteration_count,
        max_verification_attempts: 0,
        work_dirs,
        first_user_message: None,
        original_user_message: None,
        last_assistant_message: Some(last_assistant_message),
        profile_id: None,
        actual_input_tokens: None,
        had_error: matches!(stop_reason, acp::StopReason::Refusal),
        had_api_error: false,
        stop_phase: crate::context::StopPhase::Working,
        context_exceeds_limit: false,
        approximate_token_count: 0,
        connection: None,
        project: None,
        peer_agent_states: crate::peer_states::unmuted_states_for_context(),
    })
}

/// Async LLM call for the Claude path.
/// Hidden-thread backend (`claude-hidden-orchestrator` feature): package a
/// `NeedsLlmCall` decision that carries the worker's connection + project so
/// Compute the stop lifecycle phase from the process-global verification
/// counter, mirroring the native-agent path's logic. The hidden orchestrator
/// needs this to apply the correct confidence threshold (Working: lenient,
/// PreStop/Verified: strict) and to include it in the context JSON so the
/// hidden session can see it.
#[cfg(feature = "claude-hidden-orchestrator")]
fn compute_claude_stop_phase() -> crate::context::StopPhase {
    use std::sync::atomic::Ordering;
    let verification_count = crate::VERIFICATION_COUNT.load(Ordering::Relaxed);
    let max = crate::AutoPromptConfig::load()
        .map(|c| c.max_verification_attempts)
        .unwrap_or(2);
    if verification_count == 0 {
        crate::context::StopPhase::Working
    } else if verification_count >= max {
        crate::context::StopPhase::Verified
    } else {
        crate::context::StopPhase::PreStop
    }
}

/// Hidden-thread orchestrator decision builder (feature `claude-hidden-orchestrator`).
///
/// Captures the worker thread's connection + project so the async phase can
/// spawn an off-screen hidden Claude Code session to decide continue/stop.
/// The hidden session uses Claude Code's own auth — no Anthropic API key
/// required. The configured LanguageModelRegistry model (if any) is carried
/// only to satisfy the shared `LlmCallData` shape and is ignored by
/// `decide_claude_with_hidden_thread`.
#[cfg(feature = "claude-hidden-orchestrator")]
fn claude_decision_hidden(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    stop_reason: &acp::StopReason,
    configured_model: Option<language_model::ConfiguredModel>,
    cx: &App,
) -> AutoPromptDecision {
    let connection = thread.read(cx).connection().clone();

    let Some((session_id, title, work_dirs, last_assistant_message)) =
        extract_worker_signal(thread, cx)
    else {
        log::info!("[auto_prompt::claude] No assistant message to reason about — stopping chain");
        crate::reset_iteration_with_session(&thread.read(cx).session_id().to_string());
        return AutoPromptDecision::NoAction;
    };

    // `LlmCallData.model` is required by the shared struct shape even though
    // the hidden-thread path never reads it. We need *some* configured model to
    // fill the slot; if none is configured, bail gracefully (the operator
    // normally has GLM configured, so this branch is rare). We do NOT fabricate
    // a fake model — that would risk a downstream panic.
    let Some(configured) = configured_model else {
        log::warn!(
            "[auto_prompt::claude] No language model configured in Zed — \
             cannot build hidden-orchestrator decision, stopping"
        );
        crate::reset_iteration_with_session(&session_id.to_string());
        return AutoPromptDecision::NoAction;
    };

    let iteration_count = get_iteration();
    log::info!(
        "[auto_prompt::claude] iteration {iteration_count}, handing to HIDDEN Claude orchestrator"
    );

    let project = thread.read(cx).project().clone();

    // Read plan files (same as the native-agent path) so the hidden
    // orchestrator can see unchecked tasks. Without this, a worker
    // that emits a completion summary with `[ ]` items still in the plan
    // looks "done" — the orchestrator has no signal that work remains.
    // See .plans/014_claude_offscreen_orchestrator.md (context-parity fix).
    // Synchronous main-thread read: serves the prewarmed origin snapshot.
    let plan_files =
        crate::read_plan_files_cached(&crate::plan_inputs_without_message(&thread.read(cx)));
    let stop_phase = compute_claude_stop_phase();

    let context_json = serde_json::json!({
        "session_id": session_id.to_string(),
        "iteration_count": iteration_count,
        "last_assistant_message": last_assistant_message,
        "stop_phase": format!("{:?}", stop_phase),
        "had_error": matches!(stop_reason, acp::StopReason::Refusal),
        "plan_files": plan_files,
    })
    .to_string();

    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model: configured.model,
        system_prompt: HIDDEN_ORCHESTRATOR_PROMPT.to_string(),
        context_json,
        project_root: work_dirs.as_ref().and_then(|d| d.first().cloned()),
        session_id,
        title,
        iteration_count,
        max_verification_attempts: 0,
        work_dirs,
        first_user_message: None,
        original_user_message: None,
        last_assistant_message: Some(last_assistant_message),
        profile_id: None,
        actual_input_tokens: None,
        had_error: matches!(stop_reason, acp::StopReason::Refusal),
        had_api_error: false,
        stop_phase: stop_phase,
        context_exceeds_limit: false,
        approximate_token_count: 0,
        connection: Some(connection),
        project: Some(project),
        peer_agent_states: crate::peer_states::unmuted_states_for_context(),
    })
}

/// Async LLM call for the Claude path.
///
/// Calls the orchestration model with the Claude-specific system prompt and
/// the agent's last paragraphs, then maps the verdict to:
/// - `Continue(action)` — `force_new_thread = false`, dispatched to same thread.
/// - `Stopped { reason }` — task done or confidence below threshold.
///
/// Never returns `ContextOverflow`.
pub async fn decide_claude_with_llm(
    data: LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptOutcome> {
    log::info!(
        "[auto_prompt::claude] decide_claude_with_llm: session={:?}, iteration={}",
        data.session_id,
        data.iteration_count
    );

    let response_text = call_claude_orchestrator(&data.model, &data.context_json, cx).await?;

    let parsed = parse_claude_response(&response_text).with_context(|| {
        format!(
            "auto_prompt::claude: failed to parse orchestrator response: {}",
            crate::context::truncate_to_paragraph_budget(&response_text, 500)
        )
    })?;

    log::info!(
        "[auto_prompt::claude] orchestrator verdict: continue={}, confidence={:?}, reason={:?}",
        parsed.continue_work,
        parsed.confidence,
        parsed.reason
    );

    if !parsed.continue_work {
        let session_id_str = data.session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return Ok(AutoPromptOutcome::Stopped {
            reason: parsed
                .reason
                .unwrap_or_else(|| "orchestrator: task complete".to_string()),
        });
    }

    // Continue verdict, but confidence too low — treat as stop to avoid loops.
    let confidence = parsed.confidence.unwrap_or(0.0);
    if confidence < CONTINUE_CONFIDENCE_THRESHOLD {
        log::info!(
            "[auto_prompt::claude] continue verdict but confidence {confidence} < {CONTINUE_CONFIDENCE_THRESHOLD} — stopping"
        );
        let session_id_str = data.session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return Ok(AutoPromptOutcome::Stopped {
            reason: format!("orchestrator: low confidence ({confidence:.2}) continue verdict"),
        });
    }

    let next_prompt = parsed.next_prompt.unwrap_or_else(|| {
        log::warn!(
            "[auto_prompt::claude] continue verdict missing next_prompt — using minimal nudge"
        );
        // Last-resort nudge. Not a static rule — only reached when the
        // orchestrator returned continue=true without a prompt body.
        "Continue the task from where you left off.".to_string()
    });

    let action = AutoPromptAction {
        from_session_id: data.session_id.clone(),
        from_title: data.title.clone(),
        next_prompt,
        work_dirs: data.work_dirs.clone(),
        original_user_message: None,
        profile_id: data.profile_id.clone(),
        actual_input_tokens: None,
        approximate_token_count: 0,
        last_assistant_message: data.last_assistant_message.clone(),
        force_new_thread: false,
        focus_new_thread: false,
    };

    Ok(AutoPromptOutcome::Continue(action))
}

/// Dispatcher used by `agent_ui::auto_prompt::on_thread_stopped` for the Claude
/// path. Selects the LLM-call backend or the hidden-thread backend at compile
/// time via the `claude-hidden-orchestrator` feature. Both return the same
/// `Result<AutoPromptOutcome>` so the shared retry/cancel scaffolding in the
/// caller works unchanged.
pub async fn decide_claude_async(
    data: LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptOutcome> {
    // Summary-first fast path (plan 027, parity with the native flow): a
    // voluntary summary at the last paragraph carries its own "what
    // remains" — skip the orchestrator and continue same-thread with the
    // summary's steps + fixed decision directive (or stop on a terminal
    // summary, plan 031). Overflow/nothing-left cases fall through to
    // the flows that own them (overflow routes through the shared native
    // Phase 1/2 machine before reaching here).
    if let Some(outcome) = crate::summary_continuation_fast_path(&data) {
        log::warn!(
            "[auto_prompt::claude] summary fast path — dispatching fast-path outcome (no orchestrator call)"
        );
        return Ok(outcome);
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    {
        decide_claude_with_hidden_thread(data, cx).await
    }
    #[cfg(not(feature = "claude-hidden-orchestrator"))]
    {
        decide_claude_with_llm(data, cx).await
    }
}

/// Hidden-thread orchestrator (`claude-hidden-orchestrator` feature).
///
/// Spawns an off-screen Claude Code session on the worker's own connection,
/// sends it the worker's last 2-3 paragraphs + the continue/stop question, and
/// reads the verdict back. The hidden session is never registered in any panel
/// list, so it's invisible — it lives only as the `Entity<AcpThread>` held here
/// and is dropped (cleaned up) when this function returns.
///
/// Uses Claude Code's own auth (browser/subscription) — no Anthropic API key in
/// Zed required. This is the whole point: available exactly when GLM (the usual
/// default) is rate-limited and the operator has fallen back to Claude Code.
///
/// Maps the verdict exactly as the LLM-call path does:
/// - `Continue(action)` — same thread, `force_new_thread = false`.
/// - `Stopped { reason }` — task done, confidence too low, or parse failure.
#[cfg(feature = "claude-hidden-orchestrator")]
pub async fn decide_claude_with_hidden_thread(
    data: LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptOutcome> {
    use std::rc::Rc;

    log::info!(
        "[auto_prompt::claude] decide_claude_with_hidden_thread: session={:?}, iteration={}",
        data.session_id,
        data.iteration_count
    );

    // The connection + project were captured in `claude_decision_hidden`. They
    // are always Some on this path; bail clearly if not (defensive — would
    // indicate a routing bug, not a runtime condition).
    let connection: Rc<dyn acp_thread::AgentConnection> = data
        .connection
        .clone()
        .ok_or_else(|| anyhow::anyhow!("hidden-thread path missing connection"))?;
    let project = data
        .project
        .clone()
        .ok_or_else(|| anyhow::anyhow!("hidden-thread path missing project"))?;

    // Build the work_dirs PathList for new_session. Falls back to empty (the
    // orchestrator doesn't read files; it only judges text).
    let work_dirs = data
        .work_dirs
        .as_deref()
        .map(util::path_list::PathList::new)
        .unwrap_or_default();

    // Spawn the hidden session. new_session is sync-Rpc -> Task; run it on the
    // foreground executor via cx.update. The returned Entity is held locally
    // and never inserted into AgentPanel.retained_threads, so it's invisible.
    let hidden_thread = cx
        .update(|cx| connection.clone().new_session(project, work_dirs, cx))
        .await?;
    let hidden_session_id = cx.update(|cx| hidden_thread.read(cx).session_id().clone());
    log::info!(
        "[auto_prompt::claude] spawned hidden orchestrator session {:?}",
        hidden_session_id
    );

    // Run the judgment turn in a helper so we can guarantee session cleanup
    // (close_session) on every exit path — success, error, timeout, parse
    // failure. Without close_session, the session leaks: the connection's
    // sessions map ref_count is never decremented and the underlying ACP
    // process is never killed (CloseSessionRequest never sent).
    let outcome = judge_with_hidden_session(&hidden_thread, &data, cx).await;

    // Close the hidden session to free the underlying ACP process and remove
    // it from the connection's sessions map. This decrements ref_count (set to
    // 1 by new_session) and sends CloseSessionRequest when it hits 0.
    if connection.supports_close_session() {
        let close_result = cx
            .update(|cx| connection.clone().close_session(&hidden_session_id, cx))
            .await;
        if let Err(err) = close_result {
            log::warn!("[auto_prompt::claude] hidden session close failed: {err:#}");
        }
    }
    drop(hidden_thread);

    outcome
}

/// Send the orchestrator prompt to the hidden session, await its verdict, and
/// map it to `AutoPromptOutcome`. Does NOT close the session — the caller is
/// responsible for cleanup regardless of outcome.
#[cfg(feature = "claude-hidden-orchestrator")]
async fn judge_with_hidden_session(
    hidden_thread: &gpui::Entity<acp_thread::AcpThread>,
    data: &LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<AutoPromptOutcome> {
    // The system instruction (HIDDEN_ORCHESTRATOR_PROMPT, stored in
    // data.system_prompt) defines the judge role, the JSON schema, and the
    // tool-use prohibition. The user turn carries the worker's last paragraphs
    // as JSON so the orchestrator has the signal to judge. Both must be sent
    // — without the system instruction the hidden session has no idea it's
    // supposed to be a judge and will act as a normal Claude Code session
    // (running tools, doing the task itself).
    let worker_output = data
        .last_assistant_message
        .as_deref()
        .unwrap_or("(no worker output)");

    // Build the same lightweight context the native-agent GLM path uses: parses
    // context_json for plan_files, computes plan_summary (unchecked task counts
    // per plan file), and includes stop_phase + iteration_count + had_error.
    // Without this, the hidden session only sees the raw worker output and has
    // no signal that plan tasks remain — causing false stops on completion
    // summaries that still have `[ ]` items. See .plans/014 (context-parity).
    let lightweight_context = crate::lightweight_context::build_lightweight_orchestration_context(
        &data.context_json,
        &data.stop_phase,
        data.iteration_count,
        data.had_error,
    );

    let message = vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
        "{}\n\n--- CONTEXT + WORKER OUTPUT BELOW ---\n\nContext JSON:\n{}\n\nWorker's last output:\n{}",
        data.system_prompt, lightweight_context, worker_output,
    )))];

    // send() runs a full turn and resolves on stop. Bound it with a timeout so
    // a runaway hidden session can't wedge the chain. 180s is generous for a
    // single judgment turn including Claude Code session startup.
    let send_future =
        cx.update(|cx| hidden_thread.update(cx, |thread, cx| thread.send(message, cx)));
    let timeout_future = cx.background_executor().timer(Duration::from_secs(180));
    pin_mut!(send_future, timeout_future);

    let send_outcome = futures::future::select(send_future, timeout_future).await;
    match send_outcome {
        futures::future::Either::Left((result, _)) => {
            if let Err(err) = result {
                log::warn!("[auto_prompt::claude] hidden session send failed: {err:#}");
                let session_id_str = data.session_id.to_string();
                crate::reset_iteration_with_session(&session_id_str);
                return Ok(AutoPromptOutcome::Stopped {
                    reason: format!("hidden orchestrator send failed: {err}"),
                });
            }
        }
        futures::future::Either::Right(_) => {
            log::warn!("[auto_prompt::claude] hidden session timed out after 180s");
            // Cancel the hidden turn so we don't leak a running session.
            let _ = cx.update(|cx| {
                hidden_thread.update(cx, |t, cx| {
                    t.cancel(cx).detach();
                })
            });
            let session_id_str = data.session_id.to_string();
            crate::reset_iteration_with_session(&session_id_str);
            return Ok(AutoPromptOutcome::Stopped {
                reason: "hidden orchestrator timed out after 180s".to_string(),
            });
        }
    }

    // Tool-leak guard (layer 2 of 3): the HIDDEN_ORCHESTRATOR_PROMPT forbids
    // tool use (layer 1), and parse_claude_response rejects non-JSON replies
    // (layer 3). But a real Claude Code session COULD ignore the prompt, run a
    // tool, AND return valid JSON — which would be a false Continue. This guard
    // catches that by inspecting the hidden session's entry history directly:
    // any ToolCall since the last user message means the orchestrator did work
    // instead of judging, so we stop regardless of what the JSON says.
    let used_tools = cx.update(|cx| hidden_thread.read(cx).used_tools_since_last_user_message());
    if used_tools {
        log::warn!(
            "[auto_prompt::claude] hidden orchestrator used tools despite the no-tools constraint — stopping"
        );
        let session_id_str = data.session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return Ok(AutoPromptOutcome::Stopped {
            reason:
                "hidden orchestrator: tool-leak detected (used tools despite no-tools constraint)"
                    .to_string(),
        });
    }

    // Read the verdict from the hidden session's last assistant message.
    let response_text = cx
        .update(|cx| hidden_thread.read(cx).last_assistant_message_text(cx))
        .unwrap_or_default();

    let parsed = parse_claude_response(&response_text).with_context(|| {
        format!(
            "hidden orchestrator: failed to parse response: {}",
            crate::context::truncate_to_paragraph_budget(&response_text, 500)
        )
    });

    let parsed = match parsed {
        Ok(v) => v,
        Err(err) => {
            // Non-JSON reply (tool-leak, prose, or empty) → stop. Never loop on
            // an unparseable hidden-session reply.
            log::warn!("[auto_prompt::claude] hidden parse failed, stopping: {err:#}");
            let session_id_str = data.session_id.to_string();
            crate::reset_iteration_with_session(&session_id_str);
            return Ok(AutoPromptOutcome::Stopped {
                reason: format!("hidden orchestrator: unparseable reply ({err})"),
            });
        }
    };

    log::info!(
        "[auto_prompt::claude] hidden verdict: continue={}, confidence={:?}, reason={:?}",
        parsed.continue_work,
        parsed.confidence,
        parsed.reason
    );

    if !parsed.continue_work {
        let session_id_str = data.session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return Ok(AutoPromptOutcome::Stopped {
            reason: parsed
                .reason
                .unwrap_or_else(|| "hidden orchestrator: task complete".to_string()),
        });
    }

    let confidence = parsed.confidence.unwrap_or(0.0);
    if confidence < CONTINUE_CONFIDENCE_THRESHOLD {
        log::info!(
            "[auto_prompt::claude] hidden continue verdict but confidence {confidence} < {CONTINUE_CONFIDENCE_THRESHOLD} — stopping"
        );
        let session_id_str = data.session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return Ok(AutoPromptOutcome::Stopped {
            reason: format!(
                "hidden orchestrator: low confidence ({confidence:.2}) continue verdict"
            ),
        });
    }

    let next_prompt = parsed.next_prompt.unwrap_or_else(|| {
        log::warn!(
            "[auto_prompt::claude] hidden continue verdict missing next_prompt — using minimal nudge"
        );
        "Continue the task from where you left off.".to_string()
    });

    let action = AutoPromptAction {
        from_session_id: data.session_id.clone(),
        from_title: data.title.clone(),
        next_prompt,
        work_dirs: data.work_dirs.clone(),
        original_user_message: None,
        profile_id: data.profile_id.clone(),
        actual_input_tokens: None,
        approximate_token_count: 0,
        last_assistant_message: data.last_assistant_message.clone(),
        force_new_thread: false,
        focus_new_thread: false,
    };

    Ok(AutoPromptOutcome::Continue(action))
}
///
/// Reuses the same streaming + timeout machinery as the native path but with
/// the minimal Claude context (just the last paragraphs JSON). Falls back to
/// a stop verdict on empty/error responses so we never silently loop.
async fn call_claude_orchestrator(
    model: &Arc<dyn LanguageModel>,
    context_json: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<String> {
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![CLAUDE_SYSTEM_PROMPT.to_owned().into()],
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
            .context("auto_prompt::claude: failed to start completion stream")?;

        let mut text_parts: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => text_parts.push(text),
                Ok(_) => {}
                Err(err) => {
                    log::warn!("[auto_prompt::claude] stream error: {err:#}");
                    return Err(anyhow::anyhow!(err));
                }
            }
        }
        Ok(text_parts.concat())
    };

    let timeout_future = cx.background_executor().timer(Duration::from_secs(60));
    pin_mut!(completion_future, timeout_future);

    match futures::future::select(completion_future, timeout_future).await {
        futures::future::Either::Left((Ok(text), _)) => Ok(text),
        futures::future::Either::Left((Err(err), _)) => Err(err),
        futures::future::Either::Right(_) => {
            anyhow::bail!("auto_prompt::claude: LLM call timed out after 60 seconds")
        }
    }
}

/// Parse the orchestrator's JSON response into a verdict.
fn parse_claude_response(raw: &str) -> anyhow::Result<ClaudeVerdict> {
    let json_str = extract_json_object(raw);
    let value: serde_json::Value =
        serde_json::from_str(json_str).with_context(|| format!("invalid JSON: {json_str}"))?;

    let continue_work = value
        .get("continue")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow::anyhow!("missing or non-boolean 'continue' field"))?;

    let next_prompt = value.get("next_prompt").and_then(|v| {
        if v.is_null() {
            None
        } else {
            v.as_str().map(|s| s.to_string())
        }
    });
    let reason = value
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let confidence = value.get("confidence").and_then(|v| v.as_f64());

    Ok(ClaudeVerdict {
        continue_work,
        next_prompt,
        reason,
        confidence,
    })
}

#[derive(Debug)]
struct ClaudeVerdict {
    continue_work: bool,
    next_prompt: Option<String>,
    reason: Option<String>,
    confidence: Option<f64>,
}

/// Extract the first `{...}` JSON object from raw text, tolerating surrounding
/// prose or markdown fences. Returns the input unchanged if no braces found.
fn extract_json_object(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(start) = trimmed.find('{') else {
        return trimmed;
    };
    let rest = &trimmed[start..];
    // Walk to the matching closing brace, tracking string literals so braces
    // inside JSON string values don't prematurely close the object. We compare
    // against byte values to avoid char-literal escaping ambiguity.
    const BACKSLASH: char = '\u{005C}';
    const QUOTE: char = '\u{0022}';
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            match ch {
                BACKSLASH => escaped = true,
                QUOTE => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            QUOTE => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..=i];
                }
            }
            _ => {}
        }
    }
    rest
}

/// Headings/labels that mark a paragraph as the start of a self-contained
/// summary. Matched case-insensitively against the start of a paragraph.
const SUMMARY_MARKERS: &[&str] = &["## summary", "# summary", "summary:", "tl;dr"];

/// How many trailing paragraphs to scan for a summary marker. Bounded so a
/// stray "summary:" mention deep in the transcript doesn't misfire.
const SUMMARY_SEARCH_WINDOW: usize = 6;

fn is_summary_heading(paragraph: &str) -> bool {
    let lower = paragraph.trim_start().to_ascii_lowercase();
    SUMMARY_MARKERS
        .iter()
        .any(|marker| lower.starts_with(marker))
}

/// Check if any paragraph in the text starts with a summary marker. Used to
/// trigger the board broadcast (Phase 2: post summary to board so peer agents
/// know what this agent concluded).
fn contains_summary(text: &str) -> bool {
    text.split("\n\n").any(is_summary_heading)
}

/// Phase 2 hook: when the worker's last message contains a self-summary
/// (`SUMMARY_MARKERS`), broadcast it to the agent board so peer agents on other
/// devices can see what this agent concluded. Fire-and-forget — no-op when no
/// board is configured (the broadcaster is a silent skip).
fn maybe_broadcast_summary_to_board(session_id: &acp::SessionId, full_last_message: Option<&str>) {
    let Some(message) = full_last_message else {
        return;
    };
    if !contains_summary(message) {
        return;
    }
    // Broadcast the summary text. The board truncates to 256 chars. The meta
    // field carries the session id for display.
    crate::peer_states::broadcast_state(&session_id.to_string(), None, message, "summary");
}

/// Truncate to the last N paragraphs within a char budget.
///
/// Takes whole paragraphs from the end until the budget is exceeded, always
/// including at least one paragraph so we never send an empty context.
///
/// Exception: if a summary heading (`SUMMARY_MARKERS`) appears among the
/// last `SUMMARY_SEARCH_WINDOW` paragraphs, the agent has already
/// self-summarized — everything from that heading to the end is a complete,
/// self-contained signal on its own. Return just that instead of the usual
/// last-3-paragraphs window, since the preceding blow-by-blow narration is
/// redundant with what the summary already restates (cheaper and just as
/// useful for the continue/stop decision).
fn truncate_last_paragraphs(text: &str) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();

    let search_start = paragraphs.len().saturating_sub(SUMMARY_SEARCH_WINDOW);
    if let Some(offset) = paragraphs[search_start..]
        .iter()
        .position(|p| is_summary_heading(p))
    {
        let summary = paragraphs[search_start + offset..].join("\n\n");
        // Still bounded by the budget in case the "summary" itself is huge.
        return crate::context::truncate_to_paragraph_budget(&summary, LAST_MESSAGE_BUDGET_CHARS);
    }

    let mut taken: Vec<&str> = Vec::new();
    let mut total = 0usize;
    for paragraph in paragraphs.iter().rev() {
        total += paragraph.len();
        taken.push(paragraph);
        if taken.len() >= 3 || total >= LAST_MESSAGE_BUDGET_CHARS {
            break;
        }
    }
    if taken.is_empty() {
        return text.to_string();
    }
    taken.reverse();
    taken.join("\n\n")
}

#[cfg(test)]
mod gate_tests {
    use super::{claude_effective_context_tokens, claude_tokens_exceed_overflow};

    // Plan 023 A3 (req 1): the gate fires only above the threshold, and never
    // without API-reported usage.
    #[test]
    fn claude_overflow_gate_fires_above_threshold() {
        assert!(claude_tokens_exceed_overflow(Some(320_001), 320_000));
        assert!(claude_tokens_exceed_overflow(Some(1_000_000), 320_000));
    }

    #[test]
    fn claude_overflow_gate_silent_at_or_below_threshold() {
        assert!(!claude_tokens_exceed_overflow(Some(320_000), 320_000));
        assert!(!claude_tokens_exceed_overflow(Some(80_000), 320_000));
        assert!(!claude_tokens_exceed_overflow(None, 320_000));
    }

    fn usage(input_tokens: u64, used_tokens: u64) -> acp_thread::TokenUsage {
        acp_thread::TokenUsage {
            input_tokens,
            used_tokens,
            ..Default::default()
        }
    }

    // .docs/010: Claude Code populates `used_tokens` (ACP UsageUpdate)
    // while `input_tokens` stays 0 without the acp-beta flag — the gate
    // must read the populated field.
    #[test]
    fn claude_effective_tokens_reads_used_tokens_when_input_is_zero() {
        assert_eq!(claude_effective_context_tokens(&usage(0, 210_000)), 210_000);
    }

    #[test]
    fn claude_effective_tokens_reads_input_tokens_when_larger() {
        assert_eq!(
            claude_effective_context_tokens(&usage(230_000, 50_000)),
            230_000
        );
    }

    #[test]
    fn claude_effective_tokens_gate_fires_from_used_tokens() {
        // The exact reported bug: 200k+ context, input_tokens never set.
        assert!(claude_tokens_exceed_overflow(
            Some(claude_effective_context_tokens(&usage(0, 200_001))),
            200_000
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_claude_response_continue_with_prompt() {
        let raw = r#"{"continue": true, "confidence": 0.9, "next_prompt": "Implement the remaining tests.", "reason": "Agent listed unchecked tasks."}"#;
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(verdict.continue_work);
        assert_eq!(verdict.confidence, Some(0.9));
        assert_eq!(
            verdict.next_prompt.as_deref(),
            Some("Implement the remaining tests.")
        );
        assert_eq!(
            verdict.reason.as_deref(),
            Some("Agent listed unchecked tasks.")
        );
    }

    #[test]
    fn test_parse_claude_response_stop_with_null_prompt() {
        let raw = r#"{"continue": false, "confidence": 0.2, "next_prompt": null, "reason": "Task complete."}"#;
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(!verdict.continue_work);
        assert_eq!(verdict.confidence, Some(0.2));
        assert_eq!(verdict.next_prompt, None);
    }

    #[test]
    fn test_parse_claude_response_tolerates_prose_wrapper() {
        let raw = "Here is my decision:\n```json\n{\"continue\": true, \"confidence\": 0.7, \"next_prompt\": \"Fix the bug.\", \"reason\": null}\n```\nDone.";
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(verdict.continue_work);
        assert_eq!(verdict.next_prompt.as_deref(), Some("Fix the bug."));
    }

    #[test]
    fn test_parse_claude_response_missing_continue_errors() {
        let raw = r#"{"confidence": 0.5, "next_prompt": null}"#;
        let err = parse_claude_response(raw).expect_err("should error");
        assert!(format!("{err}").contains("missing or non-boolean 'continue'"));
    }

    #[test]
    fn test_parse_claude_response_confidence_optional() {
        let raw = r#"{"continue": true, "next_prompt": "Go.", "reason": "more work"}"#;
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(verdict.continue_work);
        assert_eq!(verdict.confidence, None);
    }

    #[test]
    fn test_parse_claude_response_trailing_prose() {
        // The hidden orchestrator may add commentary after the JSON verdict.
        // Regression for a bug where extract_json_object's starts_with('{')
        // fast-path returned the trailing prose too, causing serde_json to
        // reject valid JSON as trailing data.
        let raw = "{\"continue\": true, \"confidence\": 0.8, \"next_prompt\": \"Go.\", \"reason\": \"more\"}\nI've analyzed the output.";
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(verdict.continue_work);
        assert_eq!(verdict.confidence, Some(0.8));
        assert_eq!(verdict.next_prompt.as_deref(), Some("Go."));
    }

    #[test]
    fn test_extract_json_object_plain() {
        assert_eq!(extract_json_object(r#"{"a":1}"#), r#"{"a":1}"#);
    }

    #[test]
    fn test_extract_json_object_with_prefix() {
        let raw = "prose {\"a\":1} trailing";
        assert_eq!(extract_json_object(raw), r#"{"a":1}"#);
    }

    #[test]
    fn test_extract_json_object_nested() {
        let raw = "noise {\"a\":{\"b\":2},\"c\":3} tail";
        assert_eq!(extract_json_object(raw), r#"{"a":{"b":2},"c":3}"#);
    }

    #[test]
    fn test_extract_json_object_no_braces() {
        assert_eq!(extract_json_object("no json here"), "no json here");
    }

    #[test]
    fn test_extract_json_object_trailing_prose() {
        // A real Claude session may add commentary after the JSON. The
        // starts_with('{') fast-path used to return the whole string,
        // including the trailing prose, which serde_json rejects as trailing
        // data — producing a false "Stopped" on a valid continue verdict.
        let raw = "{\"continue\": true} Hope this helps!";
        assert_eq!(extract_json_object(raw), r#"{"continue": true}"#);
    }

    #[test]
    fn test_extract_json_object_close_brace_in_string() {
        // A `}` inside a JSON string value must not close the object.
        let raw = r#"{"reason": "has } char"}"#;
        assert_eq!(extract_json_object(raw), r#"{"reason": "has } char"}"#);
    }

    #[test]
    fn test_extract_json_object_open_brace_in_string() {
        // An unbalanced `{` inside a string value must not inflate depth.
        let raw = r#"{"reason": "has { char"}"#;
        assert_eq!(extract_json_object(raw), r#"{"reason": "has { char"}"#);
    }

    #[test]
    fn test_extract_json_object_escaped_quote_in_string() {
        // An escaped `\"` inside a string must not terminate the string.
        let raw = r#"{"reason": "has \" quote"}"#;
        assert_eq!(extract_json_object(raw), r#"{"reason": "has \" quote"}"#);
    }

    #[test]
    fn test_truncate_last_paragraphs_keeps_last_three() {
        let text = "para one.\n\npara two.\n\npara three.\n\npara four.\n\npara five.";
        let out = truncate_last_paragraphs(text);
        assert_eq!(out, "para three.\n\npara four.\n\npara five.");
    }

    #[test]
    fn test_truncate_last_paragraphs_under_budget_keeps_all_up_to_three() {
        let text = "one.\n\ntwo.";
        let out = truncate_last_paragraphs(text);
        assert_eq!(out, "one.\n\ntwo.");
    }

    #[test]
    fn test_truncate_last_paragraphs_single_paragraph() {
        let text = "only one paragraph here.";
        let out = truncate_last_paragraphs(text);
        assert_eq!(out, "only one paragraph here.");
    }

    #[test]
    fn test_truncate_last_paragraphs_stops_at_char_budget() {
        // Many small paragraphs whose cumulative size exceeds the budget.
        // The function must stop once the running total crosses the budget,
        // keeping only the last few paragraphs (not the whole history).
        let chunk = "x".repeat(LAST_MESSAGE_BUDGET_CHARS / 2 + 10);
        // 6 chunks → walking from the end, the 2nd paragraph crosses the
        // budget (each ~budget/2), so we keep the last 2.
        let paras: Vec<String> = (0..6).map(|i| format!("{chunk}-{i}")).collect();
        let text = paras.join("\n\n");
        let out = truncate_last_paragraphs(&text);
        assert_eq!(
            out,
            paras[4..6].join("\n\n"),
            "should keep only the last paragraphs within the budget"
        );
    }

    #[test]
    fn test_truncate_last_paragraphs_uses_only_summary_when_present() {
        let text = "para one.\n\npara two.\n\npara three.\n\n\
                     ## Summary\n\nEverything is done, nothing left to do.";
        let out = truncate_last_paragraphs(text);
        assert_eq!(
            out, "## Summary\n\nEverything is done, nothing left to do.",
            "should drop the preceding narration once a summary heading is found"
        );
    }

    #[test]
    fn test_truncate_last_paragraphs_ignores_summary_marker_outside_search_window() {
        // "summary:" appears, but it's buried far earlier than the trailing
        // SUMMARY_SEARCH_WINDOW paragraphs — must not misfire on it.
        let mut paras = vec!["summary: this is just prose mentioning the word".to_string()];
        paras.extend((0..8).map(|i| format!("para {i}.")));
        let text = paras.join("\n\n");
        let out = truncate_last_paragraphs(&text);
        assert_eq!(
            out,
            paras[paras.len() - 3..].join("\n\n"),
            "falls back to the normal last-3 window when no summary marker is trailing"
        );
    }

    #[test]
    fn test_truncate_last_paragraphs_caps_oversized_summary_to_budget() {
        let filler = "x".repeat(LAST_MESSAGE_BUDGET_CHARS + 500);
        let text = format!("para one.\n\n## Summary\n\n{filler}");
        let out = truncate_last_paragraphs(&text);
        assert!(
            out.len() < text.len(),
            "an oversized summary must still be capped by the char budget"
        );
        assert!(out.starts_with("## Summary"));
    }

    #[test]
    fn test_is_summary_heading_matches_common_markers() {
        assert!(is_summary_heading("## Summary"));
        assert!(is_summary_heading("# summary of changes"));
        assert!(is_summary_heading("Summary: all done"));
        assert!(is_summary_heading("TL;DR: shipped it"));
        assert!(!is_summary_heading("This paragraph mentions summary later"));
    }

    // ── contains_summary + maybe_broadcast_summary_to_board (GOAT gate: posting hooks) ──

    #[test]
    fn test_contains_summary_detects_marker_in_any_paragraph() {
        // Summary marker in the first paragraph.
        assert!(contains_summary("## summary\n\ndetails"));
        // Summary marker in a later paragraph.
        assert!(contains_summary("para one.\n\nsummary: concluded work"));
        // No summary marker.
        assert!(!contains_summary("just regular text\n\nno summary here"));
        // Empty string.
        assert!(!contains_summary(""));
    }

    /// Mock broadcaster for verifying the posting pipeline. Records all calls.
    struct RecordingBroadcaster {
        calls: std::sync::Mutex<Vec<(String, Option<String>, String, String)>>,
    }

    impl crate::peer_states::AgentStateBroadcaster for RecordingBroadcaster {
        fn broadcast(
            &self,
            session_id: &str,
            sub_agent_id: Option<&str>,
            state_text: &str,
            meta: &str,
        ) {
            self.calls.lock().unwrap().push((
                session_id.to_string(),
                sub_agent_id.map(|s| s.to_string()),
                state_text.to_string(),
                meta.to_string(),
            ));
        }
    }

    #[test]
    fn maybe_broadcast_summary_fires_when_summary_detected() {
        let _lock = crate::peer_states::lock_for_test();
        crate::peer_states::clear_for_test();
        let mock = std::sync::Arc::new(RecordingBroadcaster {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        crate::peer_states::register_broadcaster(Some(mock.clone()));

        let session_id = acp::SessionId::new("test-session");
        let message = "Working on stuff.\n\n## summary\n\nFixed the bug and wrote tests.";
        maybe_broadcast_summary_to_board(&session_id, Some(message));

        let calls = mock.calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "summary should trigger exactly one broadcast"
        );
        assert_eq!(calls[0].0, "test-session");
        assert_eq!(calls[0].3, "summary", "meta should be 'summary'");
        assert!(calls[0].2.contains("Fixed the bug"));
    }

    #[test]
    fn maybe_broadcast_summary_skips_when_no_summary() {
        let _lock = crate::peer_states::lock_for_test();
        crate::peer_states::clear_for_test();
        let mock = std::sync::Arc::new(RecordingBroadcaster {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        crate::peer_states::register_broadcaster(Some(mock.clone()));

        let session_id = acp::SessionId::new("test-session");
        // Regular text without any summary marker — must NOT broadcast.
        maybe_broadcast_summary_to_board(&session_id, Some("just working on stuff"));

        let calls = mock.calls.lock().unwrap();
        assert!(calls.is_empty(), "no broadcast when summary marker absent");
    }

    #[test]
    fn maybe_broadcast_summary_skips_when_no_message() {
        let _lock = crate::peer_states::lock_for_test();
        crate::peer_states::clear_for_test();
        let mock = std::sync::Arc::new(RecordingBroadcaster {
            calls: std::sync::Mutex::new(Vec::new()),
        });
        crate::peer_states::register_broadcaster(Some(mock.clone()));

        let session_id = acp::SessionId::new("test-session");
        // No last message at all (None) — must NOT broadcast or panic.
        maybe_broadcast_summary_to_board(&session_id, None);

        let calls = mock.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn maybe_broadcast_summary_skips_when_no_broadcaster() {
        let _lock = crate::peer_states::lock_for_test();
        crate::peer_states::clear_for_test();
        // No broadcaster registered — must be a silent no-op, not a panic.
        let session_id = acp::SessionId::new("test-session");
        maybe_broadcast_summary_to_board(&session_id, Some("## summary\n\ndone"));
        // If we reach here without panicking, the test passes.
    }

    #[test]
    fn test_confidence_threshold_is_reasonable() {
        // Guard against accidental drift — this threshold is the only thing
        // standing between a confused orchestrator and an infinite loop.
        assert!(CONTINUE_CONFIDENCE_THRESHOLD >= 0.4);
        assert!(CONTINUE_CONFIDENCE_THRESHOLD <= 0.7);
    }

    #[test]
    fn test_system_prompt_has_continue_field_contract() {
        // The orchestrator contract depends on these tokens appearing in the
        // system prompt. If they drift, parsing breaks silently.
        assert!(CLAUDE_SYSTEM_PROMPT.contains("\"continue\""));
        assert!(CLAUDE_SYSTEM_PROMPT.contains("\"confidence\""));
        assert!(CLAUDE_SYSTEM_PROMPT.contains("\"next_prompt\""));
        assert!(CLAUDE_SYSTEM_PROMPT.contains("\"reason\""));
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_prompt_has_plan_summary_awareness() {
        // Context-parity fix: the hidden orchestrator must be aware of plan_summary
        // so it doesn't stop on a completion summary when plan tasks remain.
        // Pin the key decision rules so a future edit can't silently regress.
        let prompt = HIDDEN_ORCHESTRATOR_PROMPT;
        assert!(
            prompt.contains("plan_summary"),
            "hidden prompt must reference plan_summary (the primary continue signal)"
        );
        assert!(
            prompt.contains("unchecked"),
            "hidden prompt must reference unchecked tasks"
        );
        assert!(
            prompt.contains("NEVER"),
            "hidden prompt must have a NEVER-declare-done rule for unchecked tasks"
        );
        assert!(
            prompt.to_ascii_lowercase().contains("gpu training")
                || prompt.to_ascii_lowercase().contains("benchmark"),
            "hidden prompt must treat GPU/benchmark tasks as continue, not stop"
        );
        assert!(
            prompt.contains("stop_phase"),
            "hidden prompt must reference stop_phase for phase-aware thresholds"
        );
    }

    // --- Hidden-thread orchestrator (claude-hidden-orchestrator feature) ---

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_prompt_forbids_tools() {
        // The whole safety case for the hidden-thread path rests on the
        // orchestrator NOT running tools — it's a full Claude Code session and
        // could otherwise start doing real work (tool-leak). Pin the forbidding
        // language so a future edit can't soften it without breaking this test.
        let lower = HIDDEN_ORCHESTRATOR_PROMPT.to_ascii_lowercase();
        assert!(
            lower.contains("do not"),
            "hidden prompt must forbid tool use explicitly"
        );
        assert!(
            lower.contains("tool"),
            "hidden prompt must mention tools to forbid them"
        );
        assert!(
            lower.contains("json"),
            "hidden prompt must demand JSON-only output"
        );
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_prompt_has_continue_field_contract() {
        // Same JSON schema contract as CLAUDE_SYSTEM_PROMPT — if it drifts,
        // parse_claude_response breaks.
        assert!(HIDDEN_ORCHESTRATOR_PROMPT.contains("\"continue\""));
        assert!(HIDDEN_ORCHESTRATOR_PROMPT.contains("\"confidence\""));
        assert!(HIDDEN_ORCHESTRATOR_PROMPT.contains("\"next_prompt\""));
        assert!(HIDDEN_ORCHESTRATOR_PROMPT.contains("\"reason\""));
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_parse_roundtrip_continue() {
        // The hidden path reuses parse_claude_response — verify a well-formed
        // hidden reply maps to a continue verdict.
        let raw = r#"{"continue": true, "confidence": 0.8, "next_prompt": "Run the failing test.", "reason": "Test still failing."}"#;
        let verdict = parse_claude_response(raw).expect("parse ok");
        assert!(verdict.continue_work);
        assert!(verdict.confidence.unwrap() >= CONTINUE_CONFIDENCE_THRESHOLD);
        assert_eq!(
            verdict.next_prompt.as_deref(),
            Some("Run the failing test.")
        );
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_parse_tool_leak_reply_stops() {
        // If the hidden session tool-leaks (runs a tool instead of replying
        // JSON), the reply won't parse → the async path must map that to Stopped,
        // never loop. Pin that parse_claude_response errors on non-JSON.
        let tool_leak_reply =
            "I read the file and found the bug is on line 42. You should fix it there.";
        assert!(parse_claude_response(tool_leak_reply).is_err());
    }

    // --- Async hidden-thread orchestrator (needs TestAppContext + Project +
    //     StubAgentConnection harness). Covers the GOAT-gate items that can be
    //     verified without a live Claude Code run: the hidden session spawns,
    //     the verdict round-trips, and the outcome maps correctly. Tool-leak is
    //     defended at three layers (prompt + programmatic entry-history guard +
    //     parse-side). Sidebar invisibility + no-API-key are structural (the
    //     hidden session is a local Entity, never inserted into AgentPanel;
    //     the path never calls stream_completion). What remains for live run:
    //     concurrency under real ACP multiplexing, and prompt compliance of a
    //     real Claude Code session. ---

    #[cfg(feature = "claude-hidden-orchestrator")]
    mod hidden_thread_async {
        use super::*;
        use acp_thread::StubAgentConnection;
        use std::rc::Rc;

        /// Minimal LlmCallData for the hidden-thread path. Only `connection`,
        /// `project`, `system_prompt`, `context_json`, `session_id`,
        /// `last_assistant_message`, and `work_dirs` are read by
        /// `decide_claude_with_hidden_thread`; the rest are zeroed/filled with
        /// a FakeLanguageModel placeholder.
        fn build_test_data(
            connection: Rc<dyn acp_thread::AgentConnection>,
            project: gpui::Entity<project::Project>,
            worker_output: &str,
        ) -> LlmCallData {
            LlmCallData {
                model: Arc::new(language_model::fake_provider::FakeLanguageModel::default()),
                system_prompt: HIDDEN_ORCHESTRATOR_PROMPT.to_string(),
                context_json: serde_json::json!({
                    "session_id": "test-worker",
                    "iteration_count": 1,
                    "last_assistant_message": worker_output,
                })
                .to_string(),
                project_root: None,
                session_id: acp::SessionId::new("test-worker"),
                title: Some("worker".to_string()),
                iteration_count: 1,
                max_verification_attempts: 0,
                work_dirs: Some(vec![]),
                first_user_message: None,
                original_user_message: None,
                last_assistant_message: Some(worker_output.to_string()),
                profile_id: None,
                actual_input_tokens: None,
                had_error: false,
                had_api_error: false,
                stop_phase: crate::context::StopPhase::Working,
                context_exceeds_limit: false,
                approximate_token_count: 0,
                connection: Some(connection),
                project: Some(project),
                peer_agent_states: None,
            }
        }

        fn init_test(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let mut settings_store = settings::SettingsStore::test(cx);
                settings_store.register_setting::<feature_flags::FeatureFlagsSettings>();
                cx.set_global(settings_store);
            });
        }

        #[gpui::test]
        async fn test_hidden_thread_continue_verdict_roundtrips(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // Pre-set the hidden session's response: a well-formed continue
            // verdict. The stub's `prompt` drains these updates and auto-ends
            // the turn, so `thread.send` resolves immediately.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.8, "next_prompt": "Run the failing test.", "reason": "Test still failing."}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection, project, "Tests are still failing.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Continue(action) => {
                    assert_eq!(action.next_prompt, "Run the failing test.");
                    assert!(!action.force_new_thread);
                    assert_eq!(action.from_session_id.to_string(), "test-worker");
                }
                other => panic!("expected Continue, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_stop_verdict_stops(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "Task complete."}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection, project, "Done. All tests pass.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(reason.contains("Task complete"));
                }
                other => panic!("expected Stopped, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_tool_leak_reply_stops(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // Simulate a tool-leak: the hidden session returned prose instead
            // of JSON. The async path must map this to Stopped, never loop.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new("I read the file and the bug is on line 42.".into()),
            )]);

            let data = build_test_data(connection, project, "Investigating the bug in file.rs.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(
                        reason.contains("unparseable"),
                        "tool-leak reply should stop with unparseable reason, got: {reason}"
                    );
                }
                other => panic!("expected Stopped for tool-leak reply, got {other:?}"),
            }
        }

        /// Tool-leak guard (layer 2): even if the hidden session returns valid
        /// JSON (which parse_claude_response would accept → false Continue), the
        /// programmatic entry-history guard catches that it used a ToolCall and
        /// stops. This is the defense against a real Claude Code session that
        /// ignores HIDDEN_ORCHESTRATOR_PROMPT's no-tools constraint, does the
        /// work itself, AND returns well-formed JSON.
        #[gpui::test]
        async fn test_hidden_thread_tool_use_stops_even_with_valid_json(
            cx: &mut gpui::TestAppContext,
        ) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // The hidden session used a tool (ToolCall entry) AND returned valid
            // JSON that would normally produce a Continue verdict. The guard
            // must stop BEFORE parsing, because the tool use is the violation.
            let valid_continue_json = r#"{"continue": true, "confidence": 0.9, "next_prompt": "Keep going.", "reason": "More work."}"#;
            connection.set_next_prompt_updates(vec![
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new("leaked-read", "Read file.rs")
                        .kind(acp::ToolKind::Fetch)
                        .status(acp::ToolCallStatus::Completed),
                ),
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    valid_continue_json.into(),
                )),
            ]);

            let data = build_test_data(connection, project, "Investigating the bug in file.rs.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(
                        reason.contains("tool-leak"),
                        "tool-using hidden session should stop with tool-leak reason, got: {reason}"
                    );
                }
                other => panic!("expected Stopped for tool-leak with valid JSON, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_low_confidence_stops(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // continue=true but confidence below the 0.5 threshold → must stop.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.2, "next_prompt": "maybe continue?", "reason": "unsure"}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection, project, "Not sure if done.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(
                        reason.contains("low confidence"),
                        "low-confidence continue should stop, got: {reason}"
                    );
                }
                other => panic!("expected Stopped for low-confidence, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_missing_next_prompt_uses_fallback_nudge(
            cx: &mut gpui::TestAppContext,
        ) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // continue=true with high confidence but next_prompt is null.
            // The code must fall back to the default nudge.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.9, "next_prompt": null, "reason": "keep going"}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection, project, "Still working on it.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Continue(action) => {
                    assert_eq!(
                        action.next_prompt,
                        "Continue the task from where you left off."
                    );
                }
                other => panic!("expected Continue with fallback nudge, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_missing_confidence_stops(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            // continue=true but confidence field is entirely absent.
            // parse_claude_response returns confidence=None, which
            // unwrap_or(0.0) makes 0.0 < threshold → must stop.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "next_prompt": "do something", "reason": "unsure"}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection, project, "Not sure if done.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(
                        reason.contains("low confidence"),
                        "missing confidence (0.0) should stop, got: {reason}"
                    );
                }
                other => panic!("expected Stopped for missing confidence, got {other:?}"),
            }
        }

        #[gpui::test]
        async fn test_hidden_thread_closes_session_when_supported(cx: &mut gpui::TestAppContext) {
            // Regression: the hidden session must be closed via
            // connection.close_session() after the judgment turn, so the
            // underlying ACP process is killed and the session is removed
            // from the connection's sessions map. Without this, every
            // auto-prompt decision leaks a session (ref_count never
            // decremented, process never killed).
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new().with_supports_close_session(true));
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "done"}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection.clone(), project, "All done.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            assert!(
                matches!(outcome, AutoPromptOutcome::Stopped { .. }),
                "expected Stopped, got {outcome:?}"
            );
            assert_eq!(
                connection.close_count(),
                1,
                "close_session must be called exactly once after the judgment turn"
            );
        }

        #[gpui::test]
        async fn test_hidden_thread_skips_close_when_not_supported(cx: &mut gpui::TestAppContext) {
            // When the connection doesn't support close_session (the default
            // for StubAgentConnection), the orchestrator must not try to call
            // it — otherwise it would get an error every time.
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "done"}"#
                        .into(),
                ),
            )]);

            let data = build_test_data(connection.clone(), project, "All done.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            assert!(
                matches!(outcome, AutoPromptOutcome::Stopped { .. }),
                "expected Stopped, got {outcome:?}"
            );
            assert_eq!(
                connection.close_count(),
                0,
                "close_session must NOT be called when supports_close_session is false"
            );
        }

        /// No-API-key guarantee (GOAT item): the hidden-thread path must NEVER
        /// call the LanguageModel's stream_completion. It uses Claude Code's
        /// own auth via the ACP connection, not Zed's LanguageModelRegistry.
        /// This test captures the FakeLanguageModel and asserts completion_count
        /// stays 0 after a full Continue verdict roundtrip — if the hidden path
        /// ever regressed to calling the model (e.g. by accidentally routing to
        /// decide_claude_with_llm), this count would be 1.
        #[gpui::test]
        async fn test_hidden_thread_never_calls_language_model(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new());
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.9, "next_prompt": "Keep going.", "reason": "More work."}"#
                        .into(),
                ),
            )]);

            let model = Arc::new(language_model::fake_provider::FakeLanguageModel::default());
            let mut data =
                build_test_data(connection, project, "Investigating the bug in file.rs.");
            data.model = model.clone();

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            assert!(
                matches!(outcome, AutoPromptOutcome::Continue { .. }),
                "expected Continue, got {outcome:?}"
            );
            assert_eq!(
                model.completion_count(),
                0,
                "hidden-thread path must never call stream_completion on the LanguageModel"
            );
        }

        /// Concurrency isolation: two concurrent hidden-thread decisions
        /// (simulating two workers stopping simultaneously) must complete
        /// independently with correct verdicts and no cross-contamination.
        /// Each decision gets its own connection, its own hidden session, and
        /// its own verdict — proving the orchestration logic has no shared
        /// mutable state between concurrent invocations.
        ///
        /// This is the static half of the 'Concurrency under real ACP
        /// multiplexing' GOAT item. The live run confirms no deadlock under
        /// real ACP protocol multiplexing (two sessions on one OS process);
        /// this test confirms no deadlock or state leakage in the orchestration
        /// layer itself.
        #[gpui::test]
        async fn test_hidden_thread_concurrent_decisions_isolate(cx: &mut gpui::TestAppContext) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            // Two separate connections (simulating two worker threads, each
            // with its own AcpConnection). The StubAgentConnection's
            // next_prompt_updates is shared per-connection, so two concurrent
            // decisions on the same stub connection would deadlock (first
            // caller drains the updates, second caller waits forever). Separate
            // connections avoid that stub limitation while still testing the
            // orchestration layer's concurrency safety.
            let connection_a =
                Rc::new(StubAgentConnection::new().with_supports_close_session(true));
            let connection_b =
                Rc::new(StubAgentConnection::new().with_supports_close_session(true));

            // Decision A: continue, run the failing test.
            connection_a.set_next_prompt_updates(vec![
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.9, "next_prompt": "Run test A.", "reason": "A still failing."}"#.into(),
                )),
            ]);
            // Decision B: stop, task complete.
            connection_b.set_next_prompt_updates(vec![
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "B done."}"#.into(),
                )),
            ]);

            let data_a = build_test_data(
                connection_a.clone(),
                project.clone(),
                "Worker A: tests still failing.",
            );
            let data_b = build_test_data(
                connection_b.clone(),
                project.clone(),
                "Worker B: all tests pass.",
            );

            // Spawn both decisions BEFORE awaiting either, so they run
            // concurrently on the GPUI executor. If there were shared mutable
            // state or a lock ordering issue, one or both would deadlock or
            // produce the wrong verdict.
            let task_a = cx.update(|cx| {
                cx.spawn(async move |cx| decide_claude_with_hidden_thread(data_a, cx).await)
            });
            let task_b = cx.update(|cx| {
                cx.spawn(async move |cx| decide_claude_with_hidden_thread(data_b, cx).await)
            });

            let outcome_a = task_a.await.expect("concurrent decision A succeeded");
            let outcome_b = task_b.await.expect("concurrent decision B succeeded");

            // Verify no cross-contamination: A continues, B stops.
            match outcome_a {
                AutoPromptOutcome::Continue(action) => {
                    assert_eq!(
                        action.next_prompt, "Run test A.",
                        "decision A should get its own verdict, not B's"
                    );
                }
                other => panic!("expected Continue for A, got {other:?}"),
            }
            match outcome_b {
                AutoPromptOutcome::Stopped { reason } => {
                    assert!(
                        reason.contains("B done"),
                        "decision B should get its own verdict, not A's: {reason}"
                    );
                }
                other => panic!("expected Stopped for B, got {other:?}"),
            }

            // Both hidden sessions must be closed (no leak under concurrency).
            assert_eq!(
                connection_a.close_count(),
                1,
                "decision A's hidden session must be closed exactly once"
            );
            assert_eq!(
                connection_b.close_count(),
                1,
                "decision B's hidden session must be closed exactly once"
            );
        }

        /// Plan-summary awareness (context-parity fix): when the worker outputs a
        /// completion summary BUT the plan_summary shows unchecked tasks, the
        /// orchestrator must CONTINUE, not stop. This is the exact scenario the
        /// fix addresses: a worker says "done" with `[ ]` items still in the
        /// plan — the GLM path already continues via plan_summary, and now the
        /// hidden Claude path does too.
        #[gpui::test]
        async fn test_hidden_thread_continues_when_plan_has_unchecked_tasks(
            cx: &mut gpui::TestAppContext,
        ) {
            init_test(cx);

            let fs = fs::FakeFs::new(cx.executor());
            let project = project::Project::test(fs, [], cx).await;

            let connection = Rc::new(StubAgentConnection::new().with_supports_close_session(true));
            // The hidden session sees plan_summary with unchecked tasks and
            // returns continue (the new prompt rule fires). The worker's own
            // output says "all done" — plan_summary must win.
            connection.set_next_prompt_updates(vec![acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(
                    r#"{"continue": true, "confidence": 0.9, "next_prompt": "Continue with T4.4 from .plans/330. Mark completed steps as [x].", "reason": "Plan 330 has unchecked tasks."}"#.into(),
                ),
            )]);

            let mut data = build_test_data(
                connection.clone(),
                project,
                "## Summary\n\nAll done. T4.4 blocked on M3.",
            );
            // Inject plan_files into context_json so
            // build_lightweight_orchestration_context sees unchecked tasks.
            data.context_json = serde_json::json!({
                "session_id": "test-worker",
                "iteration_count": 1,
                "last_assistant_message": "## Summary\n\nAll done. T4.4 blocked on M3.",
                "plan_files": [{
                    "path": ".plans/330_test.md",
                    "content": "- [x] T4.1 done\n- [x] T4.2 done\n- [ ] T4.4 sweep\n- [ ] T5.3 NF-CoT\n"
                }],
            }).to_string();

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await)
                })
                .await
                .expect("hidden-thread decision succeeded");

            match outcome {
                AutoPromptOutcome::Continue(action) => {
                    assert!(
                        action.next_prompt.contains("330"),
                        "next_prompt should reference the plan with unchecked tasks, got: {}",
                        action.next_prompt
                    );
                }
                other => panic!("expected Continue when plan has unchecked tasks, got {other:?}"),
            }
        }
    }
}
