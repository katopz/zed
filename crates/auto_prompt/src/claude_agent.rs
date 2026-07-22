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
//!   4. The only hard stops are: user cancel, or no model configured.

use agent_client_protocol::schema as acp;
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

/// Decide the next auto-prompt action for a Claude (ACP) agent thread.
///
/// Returns:
/// - `NoAction` — cancelled, or no model configured.
/// - `NeedsLlmCall(data)` — the caller spawns `decide_claude_with_llm(data)`.
///
/// Never returns `DispatchNow` / `DispatchAfterDelay` / `ContextOverflow` —
/// every non-cancel decision goes through the LLM.
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
    let Some(configured_model) = registry.default_model() else {
        log::warn!("[auto_prompt::claude] No language model configured in Zed — stopping");
        return AutoPromptDecision::NoAction;
    };
    let model = configured_model.model;

    let thread_ref = thread.read(cx);
    let session_id = thread_ref.session_id().clone();
    let title = thread_ref.title().map(|t| t.to_string());
    let work_dirs = thread_ref.work_dirs().map(|pl| pl.paths().to_vec());

    // The only signal we feed the orchestrator: the agent's last paragraphs.
    let full_last_message = thread_ref.last_assistant_message_text(cx);
    let last_assistant_message = full_last_message
        .as_deref()
        .map(truncate_last_paragraphs)
        .filter(|s| !s.trim().is_empty());

    if last_assistant_message.is_none() {
        log::info!(
            "[auto_prompt::claude] No assistant message to reason about — stopping chain"
        );
        let session_id_str = session_id.to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return AutoPromptDecision::NoAction;
    }

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
        // Claude path never does pre-stop verification.
        max_verification_attempts: 0,
        work_dirs,
        first_user_message: None,
        original_user_message: None,
        last_assistant_message,
        profile_id: None,
        actual_input_tokens: None,
        had_error: matches!(stop_reason, acp::StopReason::Refusal),
        stop_phase: crate::context::StopPhase::Working,
        context_exceeds_limit: false,
        approximate_token_count: 0,
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
    };

    Ok(AutoPromptOutcome::Continue(action))
}

/// Call the orchestration LLM with the Claude-path system prompt + context.
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

/// Truncate to the last N paragraphs within a char budget.
///
/// Takes whole paragraphs from the end until the budget is exceeded, always
/// including at least one paragraph so we never send an empty context.
fn truncate_last_paragraphs(text: &str) -> String {
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
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
}
