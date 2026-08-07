//! Claude (ACP) agent auto-prompt decision logic.
//!
//! Intentionally simple and isolated from the native-agent path. Claude Code
//! manages its own context window, compaction, and tool loop internally —
//! Zed's auto_prompt layer must never stop, compact, or fork a new thread
//! for it. The only job here is to look at the agent's last 2-3 paragraphs
//! of output, ask the orchestration LLM whether the task is done, and if not,
//! produce the next prompt to nudge the same thread forward.
//!
//! Contract (do not change without explicit owner sign-off):
//!   1. Never return `ContextOverflow` — no token-limit triggers.
//!   2. Never set `force_new_thread = true` — always continue in the same thread.
//!   3. No pre-stop verification, no max-iterations gate, no rules-based stop.
//!      The orchestration LLM is the sole decider.
//!   4. The only hard stops are: user cancel, no model configured, or the
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

use crate::{AutoPromptAction, AutoPromptDecision, AutoPromptOutcome, LlmCallData, get_iteration};

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
#[cfg(feature = "claude-hidden-orchestrator")]
const HIDDEN_ORCHESTRATOR_PROMPT: &str = "\
You are an auto-prompt orchestrator. Your ONLY job is to read another Claude
Code agent's last output and decide whether IT should continue or stop.

HARD CONSTRAINTS (do not violate under any circumstance):
- Do NOT run any tools. Do NOT read or write files. Do NOT use any tool.
- Do NOT do the task yourself. You are a judge, not a worker.
- Respond with ONLY a single JSON object and nothing else — no prose,
  no markdown fences, no explanation outside the JSON.

JSON schema (each key once, never duplicate):
  {\"continue\": bool, \"confidence\": float, \"next_prompt\": string|null, \"reason\": string}

Rules:
- continue=true iff the worker clearly has more work to do (unfinished steps,
  remaining tasks, partial implementation, an error to fix, a question it can
  answer itself by continuing).
- continue=false iff the task is done, the worker is waiting for genuine user
  input (credentials, explicit choice), or it stopped with a clear completion
  summary.
- confidence is 0.0..1.0 — how sure you are of the continue/stop verdict.
- next_prompt: when continue=true, a direct imperative instruction for the
  worker's next step (standalone, not a conversational reply). null when stop.
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
///   `.issues/014_claude_offscreen_orchestrator.md`.
///
/// Never returns `DispatchNow` / `DispatchAfterDelay` / `ContextOverflow`.
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

    // User/system cancel — never auto-continue.
    if matches!(stop_reason, acp::StopReason::Cancelled) {
        log::info!("[auto_prompt::claude] Cancelled — stopping chain");
        let session_id_str = thread.read(cx).session_id().to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return AutoPromptDecision::NoAction;
    }

    // Need a configured model to reason about the next step.
    let registry = language_model::LanguageModelRegistry::read_global(cx);
    let configured_model = registry.default_model();

    // Two orchestrator backends, selected at compile time:
    //
    // 1. claude-hidden-orchestrator (default for operators without an Anthropic
    //    API key): spawn an off-screen hidden Claude Code session on the same
    //    connection and ask IT to decide continue/stop. Reuses Claude Code's
    //    own auth — no LanguageModelRegistry model required. See
    //    .issues/014_claude_offscreen_orchestrator.md.
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

    Some((session_id, title, work_dirs, last_assistant_message))
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
        log::info!(
            "[auto_prompt::claude] No assistant message to reason about — stopping chain"
        );
        crate::reset_iteration_with_session(&thread.read(cx).session_id().to_string());
        return AutoPromptDecision::NoAction;
    };

    let iteration_count = get_iteration();
    log::info!(
        "[auto_prompt::claude] iteration {iteration_count}, handing to orchestration LLM"
    );

    let context_json = serde_json::json!({
        "session_id": session_id.to_string(),
        "iteration_count": iteration_count,
        "last_assistant_message": last_assistant_message,
    })
    .to_string();

    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model,
        system_prompt: CLAUDE_SYSTEM_PROMPT.to_string(),
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
    })
}

/// Async LLM call for the Claude path.
/// Hidden-thread backend (`claude-hidden-orchestrator` feature): package a
/// `NeedsLlmCall` decision that carries the worker's connection + project so
/// the async phase can spawn an off-screen Claude Code session to decide
/// continue/stop. The hidden session uses Claude Code's own auth — no
/// Anthropic API key required. The configured LanguageModelRegistry model (if
/// any) is carried only to satisfy the shared `LlmCallData` shape and is
/// ignored by `decide_claude_with_hidden_thread`.
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
        log::info!(
            "[auto_prompt::claude] No assistant message to reason about — stopping chain"
        );
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
        connection: Some(connection),
        project: Some(project),
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
            reason: format!(
                "orchestrator: low confidence ({confidence:.2}) continue verdict"
            ),
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

    // Send the reasoning prompt. The system instruction lives in
    // data.system_prompt (HIDDEN_ORCHESTRATOR_PROMPT); the user turn carries
    // the worker's last paragraphs as JSON so the orchestrator has the signal.
    let message = vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
        "{}\n\n--- WORKER OUTPUT BELOW ---\n{}",
        data.context_json,
        data.last_assistant_message
            .as_deref()
            .unwrap_or("(no worker output)")
    )))];

    // send() runs a full turn and resolves on stop. Bound it with a timeout so
    // a runaway hidden session can't wedge the chain. 180s is generous for a
    // single judgment turn including Claude Code session startup.
    let send_future = cx.update(|cx| hidden_thread.update(cx, |thread, cx| thread.send(message, cx)));
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
            let _ = cx.update(|cx| hidden_thread.update(cx, |t, cx| {
                t.cancel(cx).detach();
            }));
            let session_id_str = data.session_id.to_string();
            crate::reset_iteration_with_session(&session_id_str);
            return Ok(AutoPromptOutcome::Stopped {
                reason: "hidden orchestrator timed out after 180s".to_string(),
            });
        }
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
    let value: serde_json::Value = serde_json::from_str(json_str)
        .with_context(|| format!("invalid JSON: {json_str}"))?;

    let continue_work = value
        .get("continue")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow::anyhow!("missing or non-boolean 'continue' field"))?;

    let next_prompt = value
        .get("next_prompt")
        .and_then(|v| {
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
    if trimmed.starts_with('{') {
        return trimmed;
    }
    if let Some(start) = trimmed.find('{') {
        let rest = &trimmed[start..];
        // Walk to the matching closing brace (depth-aware).
        let mut depth = 0i32;
        for (i, ch) in rest.char_indices() {
            match ch {
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
        return rest;
    }
    trimmed
}

/// Headings/labels that mark a paragraph as the start of a self-contained
/// summary. Matched case-insensitively against the start of a paragraph.
const SUMMARY_MARKERS: &[&str] = &["## summary", "# summary", "summary:", "tl;dr"];

/// How many trailing paragraphs to scan for a summary marker. Bounded so a
/// stray "summary:" mention deep in the transcript doesn't misfire.
const SUMMARY_SEARCH_WINDOW: usize = 6;

fn is_summary_heading(paragraph: &str) -> bool {
    let lower = paragraph.trim_start().to_ascii_lowercase();
    SUMMARY_MARKERS.iter().any(|marker| lower.starts_with(marker))
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
        assert_eq!(verdict.next_prompt.as_deref(), Some("Run the failing test."));
    }

    #[cfg(feature = "claude-hidden-orchestrator")]
    #[test]
    fn test_hidden_orchestrator_parse_tool_leak_reply_stops() {
        // If the hidden session tool-leaks (runs a tool instead of replying
        // JSON), the reply won't parse → the async path must map that to Stopped,
        // never loop. Pin that parse_claude_response errors on non-JSON.
        let tool_leak_reply = "I read the file and found the bug is on line 42. You should fix it there.";
        assert!(parse_claude_response(tool_leak_reply).is_err());
    }

    // --- Async hidden-thread orchestrator (needs TestAppContext + Project +
    //     StubAgentConnection harness). Covers the GOAT-gate items that can be
    //     verified without a live Claude Code run: the hidden session spawns,
    //     the verdict round-trips, and the outcome maps correctly. The
    //     remaining GOAT items (no-tool-leak in production, sidebar invisibility,
    //     no Anthropic key) need a live run. ---

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
            }
        }

        fn init_test(cx: &mut gpui::TestAppContext) {
            cx.update(|cx| {
                let mut settings_store = settings::SettingsStore::test(cx);
                settings_store
                    .register_setting::<feature_flags::FeatureFlagsSettings>();
                cx.set_global(settings_store);
            });
        }

        #[gpui::test]
        async fn test_hidden_thread_continue_verdict_roundtrips(
            cx: &mut gpui::TestAppContext,
        ) {
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
                    cx.spawn(async move |cx| {
                        decide_claude_with_hidden_thread(data, cx).await
                    })
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
                    cx.spawn(async move |cx| {
                        decide_claude_with_hidden_thread(data, cx).await
                    })
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
                acp::ContentChunk::new(
                    "I read the file and the bug is on line 42.".into(),
                ),
            )]);

            let data =
                build_test_data(connection, project, "Investigating the bug in file.rs.");

            let outcome = cx
                .update(|cx| {
                    cx.spawn(async move |cx| {
                        decide_claude_with_hidden_thread(data, cx).await
                    })
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
                    cx.spawn(async move |cx| {
                        decide_claude_with_hidden_thread(data, cx).await
                    })
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
    }
}
