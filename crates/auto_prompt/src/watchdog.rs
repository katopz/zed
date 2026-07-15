//! Stuck-thread watchdog reasoning.
//!
//! When the worker agent (Claude via ACP, or the native Zed agent) stays in
//! `Generating` for longer than `watchdog_timeout_secs` without emitting a
//! `Stopped` event, the worker LLM stream has almost certainly hung — either a
//! provider stall, a rate-limit that returned no body, or an ACP protocol
//! dead-end. None of auto_prompt's existing timeouts can fire in this state
//! because they all run *after* `on_thread_stopped`, which never arrives.
//!
//! This module provides the headless LLM call that decides, given the last
//! tool call (input + output), the last assistant message, and the cumulative
//! elapsed time, whether to:
//!
//!   - **continue** waiting (the command may legitimately be slow, e.g. a long
//!     build, and the watchdog was too eager), or
//!   - **halt** (cancel the worker and inject a timeout notice so the worker
//!     can recover — retry, try another approach, or stop).
//!
//! The decision is intentionally LLM-based rather than rule-based: a fixed
//! rule ("any spinner for >10 min = halt") would fire on every legitimately
//! long `cargo build` or test run. The LLM can tell the difference between
//! "a `git log` returned 3 lines 10 minutes ago and nothing happened since"
//! (halt) and "a `cargo test` is still streaming output" (continue).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use serde::Deserialize;

use crate::debug_log;

/// Maximum characters of tool-call input/output and assistant text to embed in
/// the reasoning prompt. Keeps the headless call cheap and bounded regardless
/// of how large the raw tool output was.
const CONTEXT_BUDGET_CHARS: usize = 4000;

/// Hard cap on the reasoning LLM call itself. If it can't decide in 60 seconds
/// we treat it as "continue" (safer than halting on a flaky reasoning call).
const REASONING_TIMEOUT_SECS: u64 = 60;

/// What the watchdog knows about the stuck thread when it asks the LLM.
#[derive(Debug, Clone)]
pub struct WatchdogContext {
    /// The raw input (command, arguments) of the last tool call, if any.
    /// Truncated to `CONTEXT_BUDGET_CHARS`.
    pub last_tool_call_input: Option<String>,
    /// The raw output of the last tool call, if any. Truncated to
    /// `CONTEXT_BUDGET_CHARS`.
    pub last_tool_call_output: Option<String>,
    /// The last assistant message text before the hang, if any. Truncated to
    /// `CONTEXT_BUDGET_CHARS`.
    pub last_assistant_message: Option<String>,
    /// Cumulative seconds the thread has been stuck (across all watchdog
    /// windows, not just the current one).
    pub cumulative_elapsed_secs: u64,
    /// Which timeout this is: 1 for the first expiry, 2 after one "continue",
    /// etc. Gives the LLM a sense of how patient we've already been.
    pub timeout_number: u32,
}

/// The LLM's verdict on a stuck thread.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchdogDecision {
    /// Keep waiting — reschedule the watchdog for another window.
    Continue { reason: String },
    /// Cancel the worker and inject a timeout notice so it can recover.
    Halt { reason: String },
}

/// System prompt for the reasoning LLM.
///
/// Instructs the model to classify the stuck state and respond as strict JSON.
const REASONING_SYSTEM_PROMPT: &str = "\
You are a watchdog for an AI coding agent that appears to be stuck.

The agent was running a task and made a tool call (terminal command, file \
read, etc.), but has not produced any new output or stopped for an unusually \
long time. This usually means the underlying LLM stream has hung — a provider \
rate-limit, a network stall, or a protocol dead-end.

You are given the last tool call (input and output), the agent's last \
message, how long it has been stuck, and which timeout number this is \
(1 = first check, 2 = second check after a 'continue', etc.).

Decide ONE action:

- \"continue\": The agent is likely doing legitimate slow work (a long build, \
a test suite, a large file read) and the timeout was premature. We will wait \
another full window and ask you again with a higher timeout number.

- \"halt\": The agent is truly stuck (e.g. a quick command like `git log` \
returned 3 lines 10 minutes ago and nothing happened since). We will cancel \
the worker and tell it about the timeout so it can retry, change approach, \
or explicitly stop.

Guidelines:
- A short command (grep, git log, ls, cat) with small output that has been \
stuck for minutes is almost certainly a hang → \"halt\".
- A long-running command (cargo build, cargo test, npm install) that may \
still be running → \"continue\" on the first timeout, \"halt\" if it is \
still stuck on the 3rd+ timeout.
- If the last tool call output contains an error and nothing followed, lean \
toward \"halt\" — the agent should have reacted by now.
- When in doubt on the first timeout, prefer \"continue\". On timeout \
number 3 or higher with no progress, prefer \"halt\".

Respond as JSON ONLY (no markdown fences, no prose outside the object):
{\"action\": \"continue\" | \"halt\", \"reason\": \"<one sentence>\"}";

/// Ask the configured language model whether to keep waiting or halt.
///
/// On any failure (LLM unreachable, unparseable response, timeout) this
/// returns `Continue` — a flaky reasoning call should never cause us to kill
/// a possibly-fine worker. The next watchdog window will try again.
pub async fn reason_about_stuck_thread(
    model: &Arc<dyn LanguageModel>,
    context: &WatchdogContext,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<WatchdogDecision> {
    let user_prompt = build_user_prompt(context);

    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![REASONING_SYSTEM_PROMPT.into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![user_prompt.into()],
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
            .context("watchdog: failed to start completion stream")?;

        let mut text_parts: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => text_parts.push(text),
                Ok(_) => {}
                Err(err) => {
                    log::warn!("[auto_prompt::watchdog] stream error: {err:#}");
                }
            }
        }
        let text = text_parts.concat();
        if text.trim().is_empty() {
            anyhow::bail!("watchdog: model returned no text events");
        }
        anyhow::Ok(text)
    };

    let timeout_future = cx
        .background_executor()
        .timer(Duration::from_secs(REASONING_TIMEOUT_SECS));
    pin_mut!(completion_future, timeout_future);

    let raw_text = match future::select(completion_future, timeout_future).await {
        future::Either::Left((Ok(text), _)) => text,
        future::Either::Left((Err(err), _)) => {
            log::warn!("[auto_prompt::watchdog] reasoning LLM failed: {err:#}");
            return Ok(default_continue("reasoning LLM call failed"));
        }
        future::Either::Right(_) => {
            log::warn!(
                "[auto_prompt::watchdog] reasoning LLM timed out after {REASONING_TIMEOUT_SECS}s"
            );
            return Ok(default_continue("reasoning LLM call timed out"));
        }
    };

    let decision = parse_decision(&raw_text);
    log::info!(
        "[auto_prompt::watchdog] decision: {:?} (timeout #{}, elapsed {}s)",
        decision,
        context.timeout_number,
        context.cumulative_elapsed_secs
    );

    debug_log::write_log(
        "watchdog_decision",
        serde_json::json!({
            "timeout_number": context.timeout_number,
            "cumulative_elapsed_secs": context.cumulative_elapsed_secs,
            "last_tool_call_input": context.last_tool_call_input.as_deref(),
            "last_tool_call_output_truncated":
                context.last_tool_call_output.as_deref().map(truncate_for_log),
            "decision": match &decision {
                WatchdogDecision::Continue { .. } => "continue",
                WatchdogDecision::Halt { .. } => "halt",
            },
            "reason": match &decision {
                WatchdogDecision::Continue { reason } |
                WatchdogDecision::Halt { reason } => reason.as_str(),
            },
            "raw_response_truncated": truncate_for_log(&raw_text),
        }),
    );

    Ok(decision)
}

/// Build the user-message portion of the reasoning prompt from the context.
fn build_user_prompt(context: &WatchdogContext) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(6);

    parts.push(format!(
        "Timeout number: {} (this is the {} time the watchdog has fired for this stuck period)",
        context.timeout_number,
        ordinal(context.timeout_number)
    ));
    parts.push(format!(
        "Cumulative time stuck: {} seconds (~{} minutes)",
        context.cumulative_elapsed_secs,
        context.cumulative_elapsed_secs / 60
    ));

    if let Some(msg) = &context.last_assistant_message {
        parts.push(format!(
            "Last assistant message (truncated):\n---\n{}\n---",
            truncate_for_prompt(msg)
        ));
    } else {
        parts.push("Last assistant message: (none)".to_string());
    }

    if let Some(input) = &context.last_tool_call_input {
        parts.push(format!(
            "Last tool call input (truncated):\n---\n{}\n---",
            truncate_for_prompt(input)
        ));
    }

    if let Some(output) = &context.last_tool_call_output {
        parts.push(format!(
            "Last tool call output (truncated):\n---\n{}\n---",
            truncate_for_prompt(output)
        ));
    } else {
        parts.push("Last tool call output: (none — command may still be running)".to_string());
    }

    parts.push(
        "Based on the above, decide: is the agent doing legitimate slow work \
 (\"continue\") or is it truly hung (\"halt\")? Respond as JSON only."
            .to_string(),
    );

    parts.join("\n\n")
}

/// Parse the LLM's JSON response into a `WatchdogDecision`.
///
/// Lenient: tolerates markdown fences, extra whitespace, and missing `reason`.
/// Any parse failure defaults to `Continue` (never kill a worker on a bad
/// parse).
fn parse_decision(raw: &str) -> WatchdogDecision {
    let json_str = extract_json_local(raw);
    #[derive(Deserialize)]
    struct RawDecision {
        action: Option<String>,
        #[serde(default)]
        reason: Option<String>,
    }

    let parsed: RawDecision = match serde_json::from_str(json_str) {
        Ok(d) => d,
        Err(err) => {
            log::warn!(
                "[auto_prompt::watchdog] failed to parse decision JSON: {err}\nraw: {raw}"
            );
            return default_continue("unparseable reasoning response");
        }
    };

    let reason = parsed
        .reason
        .unwrap_or_else(|| "(no reason provided)".to_string());

    match parsed.action.as_deref().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("halt") | Some("stop") | Some("cancel") => WatchdogDecision::Halt { reason },
        Some("continue") | Some("wait") => WatchdogDecision::Continue { reason },
        Some(other) => {
            log::warn!("[auto_prompt::watchdog] unknown action '{other}', defaulting to continue");
            default_continue(&format!("unknown action: {other}"))
        }
        None => default_continue("missing action field"),
    }
}

fn default_continue(reason: &str) -> WatchdogDecision {
    WatchdogDecision::Continue {
        reason: reason.to_string(),
    }
}

/// Truncate to the prompt budget on a UTF-8 char boundary.
fn truncate_for_prompt(s: &str) -> String {
    if s.len() <= CONTEXT_BUDGET_CHARS {
        return s.to_string();
    }
    let mut end = CONTEXT_BUDGET_CHARS;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n…(truncated)");
    truncated
}

/// Shorter truncation for log payloads.
fn truncate_for_log(s: &str) -> String {
    debug_log::truncate(s, 1000)
}

/// Extract the JSON object from a possibly-fenced / prose-wrapped response.
///
/// Same heuristic as `pending_question::extract_json_local`: find the first
/// `{` and the last `}`, inclusive.
fn extract_json_local(text: &str) -> &str {
    let trimmed = text.trim();
    let after_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_start())
        .unwrap_or(trimmed);
    let without_trailing_fence = after_fence
        .strip_suffix("```")
        .map(|rest| rest.trim_end())
        .unwrap_or(after_fence);

    let start = without_trailing_fence.find('{');
    let end = without_trailing_fence.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if s <= e => &without_trailing_fence[s..=e],
        _ => without_trailing_fence,
    }
}

/// Cheap ordinal formatter for 1st, 2nd, 3rd, Nth.
fn ordinal(n: u32) -> String {
    let suffix = match n % 10 {
        1 if n % 100 != 11 => "st",
        2 if n % 100 != 12 => "nd",
        3 if n % 100 != 13 => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_halt() {
        let raw = r#"{"action": "halt", "reason": "git log returned 3 lines 10 min ago"}"#;
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Halt { .. }));
    }

    #[test]
    fn parse_continue() {
        let raw = r#"{"action": "continue", "reason": "cargo build still running"}"#;
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Continue { .. }));
    }

    #[test]
    fn parse_fenced_json() {
        let raw = "```json\n{\"action\": \"halt\", \"reason\": \"test\"}\n```";
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Halt { .. }));
    }

    #[test]
    fn parse_stop_alias() {
        let raw = r#"{"action": "stop"}"#;
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Halt { .. }));
    }

    #[test]
    fn parse_garbage_defaults_to_continue() {
        let raw = "this is not json at all";
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Continue { .. }));
    }

    #[test]
    fn parse_missing_action_defaults_to_continue() {
        let raw = r#"{"reason": "no action field"}"#;
        let decision = parse_decision(raw);
        assert!(matches!(decision, WatchdogDecision::Continue { .. }));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // Em-dash is 3 bytes in UTF-8; cutting at byte 5 would split it.
        let s = "ab—".repeat(1000);
        let truncated = truncate_for_prompt(&s);
        assert!(truncated.ends_with("(truncated)"));
        // Should not panic — that's the main assertion (char boundary safe).
    }

    #[test]
    fn ordinal_formatting() {
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(2), "2nd");
        assert_eq!(ordinal(3), "3rd");
        assert_eq!(ordinal(4), "4th");
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(21), "21st");
    }

    #[test]
    fn build_prompt_includes_all_fields() {
        let ctx = WatchdogContext {
            last_tool_call_input: Some("git log --oneline".to_string()),
            last_tool_call_output: Some("abc123 commit".to_string()),
            last_assistant_message: Some("Let me check the history".to_string()),
            cumulative_elapsed_secs: 600,
            timeout_number: 1,
        };
        let prompt = build_user_prompt(&ctx);
        assert!(prompt.contains("git log --oneline"));
        assert!(prompt.contains("abc123 commit"));
        assert!(prompt.contains("Let me check"));
        assert!(prompt.contains("600 seconds"));
        assert!(prompt.contains("1st"));
    }
}
