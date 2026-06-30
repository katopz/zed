//! Pending-question fast path.
//!
//! When the worker agent ends its turn by asking the user a direct question
//! ("Which do you want? Option A or Option B?", "Want me to do that?"), the
//! normal `decide_with_llm` flow wastes a cycle: the orchestration LLM cannot
//! "continue work" (there is none — there is a question), so it returns
//! `should_continue=false` and the chain drifts into pre-stop verification or
//! the ContextOverflow summary dance. That summary drains tokens and throws
//! away the agent's actual question.
//!
//! This module detects that situation and answers the question directly via a
//! targeted LLM call on the last 2-3 paragraphs, dispatching the answer as the
//! next prompt. On any failure (no question detected, LLM unreachable, low
//! confidence) it signals fall-through so the caller resumes the existing
//! decision flow — per the user's explicit requirement that uncertain cases
//! still reach the stop/summary path.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use serde::Deserialize;

use crate::{
    AutoPromptOutcome, AutoPromptResponse, LlmCallData, with_first_prompt_context,
    is_auto_prompt_summary_response,
};

// `AutoPromptAction` is constructed via `LlmCallData::make_continue_action` so
// we don't need to name it directly here — keep the import list minimal.

/// Minimum confidence for the answerer's response to short-circuit the chain.
/// Below this we fall through to the normal decide flow instead of guessing —
/// the user explicitly authorised this: "if reasoning … not confidence it
/// should [then] allow to stop and ask for summary".
const ANSWER_CONFIDENCE_THRESHOLD: f64 = 0.6;

/// Maximum paragraphs of `last_assistant_message` to feed the answerer. Matches
/// the "last 2-3 paragraphs" window the user asked for, and keeps the targeted
/// LLM call cheap regardless of overall context size.
const ANSWERER_CONTEXT_PARAGRAPHS: usize = 3;

/// Result of scanning the last assistant message for a question to the user.
///
/// Carries the extracted question plus the preceding context paragraphs so the
/// answerer can reason about the options being offered (e.g. "Option A" / "Option B"
/// lives in the paragraph before the question).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingQuestion {
    /// The paragraph(s) containing the question itself.
    pub question_text: String,
    /// Up to `ANSWERER_CONTEXT_PARAGRAPHS` trailing paragraphs of the message,
    /// including the question — the full window the answerer reasons over.
    pub context_window: String,
}

/// Detect whether the worker's last message ends with a question to the user.
///
/// Returns `None` when:
/// - the message is empty,
/// - the message is an auto_prompt summary response (would re-loop),
/// - no question-to-user pattern is found in the last 3 paragraphs.
///
/// Pure and allocation-light — safe to call on every `decide_with_llm` entry.
pub(crate) fn detect_pending_question(last_assistant_message: Option<&str>) -> Option<PendingQuestion> {
    let msg = last_assistant_message?.trim();
    if msg.is_empty() {
        return None;
    }

    // Same guard as detect_remaining_work: never fire on our own summary
    // responses, or we'd loop the ContextOverflow Phase 1 ↔ Phase 2 dance.
    if is_auto_prompt_summary_response(msg) {
        return None;
    }

    let paragraphs: Vec<&str> = msg
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if paragraphs.is_empty() {
        return None;
    }

    // Scan the last 3 paragraphs (same window as extract_remaining_section).
    let scan_count = ANSWERER_CONTEXT_PARAGRAPHS.min(paragraphs.len());
    let scan_start = paragraphs.len() - scan_count;

    for i in (scan_start..paragraphs.len()).rev() {
        let paragraph = paragraphs[i];
        if let Some(pattern) = match_question_pattern(paragraph) {
            // Include the preceding paragraph when it looks like an option list
            // or a header ("Option A" / "Path 1" / "### My recommendation") so
            // the answerer sees what it's choosing between.
            let context_start = if i > scan_start && looks_like_option_context(paragraphs[i - 1]) {
                i - 1
            } else {
                i
            };
            let context_window = paragraphs[context_start..].join("\n\n");
            log::info!(
                "[auto_prompt::pending_question] Detected pending question ({pattern}) at paragraph {i}"
            );
            return Some(PendingQuestion {
                question_text: paragraph.to_string(),
                context_window,
            });
        }
    }

    None
}

/// Classify a paragraph as a question-to-user, returning a short label for logging.
fn match_question_pattern(paragraph: &str) -> Option<&'static str> {
    let lower = paragraph.to_lowercase();

    // Explicit option/choice request — strongest signal. The worker is
    // literally asking the user to pick between alternatives.
    let option_patterns: &[(&str, &str)] = &[
        ("which do you want", "option_request"),
        ("which do you prefer", "option_request"),
        ("which one", "option_request"),
        ("which would you", "option_request"),
        ("which path", "option_request"),
        ("option a", "option_request"),
        ("option b", "option_request"),
        ("option 1", "option_request"),
        ("option 2", "option_request"),
        ("path a", "option_request"),
        ("path b", "option_request"),
        ("a or b", "option_request"),
        ("1 or 2", "option_request"),
    ];
    for (needle, label) in option_patterns {
        if lower.contains(needle) {
            return Some(label);
        }
    }

    // Permission / proceed request.
    let permission_patterns: &[(&str, &str)] = &[
        ("want me to", "permission_request"),
        ("want me to do that", "permission_request"),
        ("should i", "permission_request"),
        ("shall i", "permission_request"),
        ("do you want", "permission_request"),
        ("do you prefer", "permission_request"),
        ("do you wish", "permission_request"),
        ("would you like", "permission_request"),
        ("would you prefer", "permission_request"),
        ("may i", "permission_request"),
        ("can i proceed", "permission_request"),
        ("ok to", "permission_request"),
        ("okay to", "permission_request"),
        ("proceed with", "permission_request"),
    ];
    for (needle, label) in permission_patterns {
        if lower.contains(needle) {
            return Some(label);
        }
    }

    // Fallback: a paragraph that ENDS with '?' and addresses the user in the
    // second person. Catches "Want me to file the follow-up?" without an
    // explicit permission verb, while avoiding rhetorical questions in docs
    // (which rarely use "you" + a trailing '?').
    let trimmed = paragraph.trim_end();
    if trimmed.ends_with('?') {
        let has_second_person = lower.contains(" you ") || lower.contains("your ")
            || lower.starts_with("you ") || lower.contains("\nyou ");
        if has_second_person {
            return Some("direct_question");
        }
    }

    None
}

/// Heuristic: does this paragraph look like the option list / recommendation
/// header that the question paragraph is referring to?
fn looks_like_option_context(paragraph: &str) -> bool {
    let lower = paragraph.to_lowercase();
    let trimmed = paragraph.trim_start();

    // Markdown header preceding the question.
    if trimmed.starts_with('#') {
        return true;
    }
    // Explicit option enumeration.
    if lower.contains("option a")
        || lower.contains("option b")
        || lower.contains("option 1")
        || lower.contains("option 2")
        || lower.contains("path a")
        || lower.contains("path b")
    {
        return true;
    }
    // Numbered/bulleted list of choices — either opens with a list item...
    if trimmed.starts_with("1.") || trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    // ...or is a prose-introduced list ("Plan 335 Phase 2 is the remaining GOAT work:\n- T2.1...").
    // Detect by counting list-item line starts anywhere in the paragraph.
    if paragraph.lines().filter(|line| is_list_item_line(line)).count() >= 2 {
        return true;
    }
    // Short label-style line ending with ':' ("My recommendation:", "Option A:").
    if paragraph.len() < 120 && trimmed.ends_with(':') {
        return true;
    }

    false
}

/// Returns true for a line that opens a markdown list item: `- `, `* `, or `N.`.
/// Used by `looks_like_option_context` to spot prose-introduced task lists
/// (e.g. a paragraph starting with "Plan ...:" then listing `- T2.1 ...`).
fn is_list_item_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    // `N.` where N is digits — split on the first '.' and check the prefix.
    if let Some(dot) = trimmed.find('.') {
        let prefix = &trimmed[..dot];
        !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit())
    } else {
        false
    }
}

/// System prompt for the targeted answerer call. Instructs the model to reason
/// over the agent's question + preceding paragraphs and emit a JSON answer
/// reusing the standard `AutoPromptResponse` shape (`next_prompt` carries the
/// answer text).
const ANSWERER_SYSTEM_PROMPT: &str = r#"You are the auto-prompt orchestration layer for an AI coding agent.

The worker agent has just STOPPED to ask the user a direct question — it is
blocked waiting for a decision. Your job is to ANSWER that question on the
user's behalf so the chain can continue without human intervention.

Read the worker's question and the preceding 2-3 paragraphs carefully. Then:

1. If the worker offered options (e.g. "Option A" vs "Option B"), pick the
   one that is most defensible given the worker's own reasoning. Prefer the
   option the worker explicitly recommended or leaned toward. If the worker
   did not lean either way, prefer the safer / more conservative option
   (revert drifts, keep existing behaviour, don't delete data).
2. If the worker asked a yes/no permission question (e.g. "Want me to do
   that?"), answer YES and let it proceed — the user enabled auto-prompt
   precisely to keep the chain moving. Only answer NO if the requested action
   is destructive and irreversible (force-push, drop a table, delete uncommitted
   work) AND the worker did not already propose a safe alternative.
3. If the worker asked a clarifying question you genuinely cannot answer from
   the context, return confidence <= 0.4 so the chain falls through to the
   normal flow instead of guessing.

Respond with a SINGLE JSON object, no prose, no markdown fences:

{
  "next_prompt": "<the imperative instruction to send back to the worker, e.g. 'Go with Option A: revert the no-op reset() so the shared crate stays safe.'>",
  "reason": "<one sentence on why you picked this>",
  "confidence": 0.0
}

The `next_prompt` MUST be a direct instruction the worker can act on
immediately — not a restatement of the question. Confidence is your
calibrated certainty that this is the right call (0.0 = guessing, 1.0 =
the worker explicitly recommended this option)."#;

/// Try to answer a pending question and dispatch the answer as the next prompt.
///
/// Returns:
/// - `Ok(Some(Continue))` — a pending question was detected AND the answerer \
///   returned a confident answer; the chain continues with that answer.
/// - `Ok(None)` — no pending question, the LLM call failed, or confidence was \
///   below threshold. The caller MUST fall through to its normal decision flow.
/// - `Err(_)` — unexpected error; the caller should propagate.
///
/// This function NEVER stops the chain on its own. Stopping is the caller's
/// job — the whole point is that answering is a fast path that either wins or
/// gets out of the way.
pub(crate) async fn try_answer_pending_question(
    data: &LlmCallData,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<Option<AutoPromptOutcome>> {
    let pending = match detect_pending_question(data.last_assistant_message.as_deref()) {
        Some(p) => p,
        None => return Ok(None),
    };

    log::warn!(
        "[auto_prompt::pending_question] Fast path triggered — answering pending question ({} chars of context)",
        pending.context_window.len()
    );

    let raw_response = match call_answerer(&data.model, &pending.context_window, cx).await {
        Ok(text) => text,
        Err(err) => {
            // Per the user's requirement: "if reasoning fail to fetch … allow
            // to stop and ask for summary". We don't stop here — we fall
            // through so the caller's normal flow (which may summarise) runs.
            log::warn!(
                "[auto_prompt::pending_question] Answerer LLM call failed: {err:#} — falling through to normal flow"
            );
            return Ok(None);
        }
    };

    let parsed: AutoPromptResponse = match parse_answer(&raw_response) {
        Ok(p) => p,
        Err(err) => {
            log::warn!(
                "[auto_prompt::pending_question] Answerer response unparseable: {err:#} — falling through. Raw: {}",
                raw_response.chars().take(300).collect::<String>()
            );
            return Ok(None);
        }
    };

    let confidence = parsed.confidence.unwrap_or(0.0);
    let answer = parsed
        .next_prompt
        .as_deref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let answer = match answer {
        Some(a) => a,
        None => {
            log::info!(
                "[auto_prompt::pending_question] Answerer returned no `next_prompt` — falling through"
            );
            return Ok(None);
        }
    };

    if confidence < ANSWER_CONFIDENCE_THRESHOLD {
        log::info!(
            "[auto_prompt::pending_question] Answerer confidence {confidence:.2} < {ANSWER_CONFIDENCE_THRESHOLD} — falling through (user: not confident → allow summary)"
        );
        return Ok(None);
    }

    log::warn!(
        "[auto_prompt::pending_question] Dispatching answer (confidence {confidence:.2}): {}",
        answer.chars().take(200).collect::<String>()
    );

    // Wrap with the chain's summary/title context — same pattern as every
    // other Continue in auto_prompt.rs, so the next thread inherits the
    // running thread summary instead of starting fresh.
    let next_prompt = with_first_prompt_context(
        answer,
        None,
        data.title.as_deref(),
        data.last_assistant_message.as_deref(),
    );

    let mut action = data.make_continue_action(next_prompt);
    // Answering a question re-enters the SAME thread context — no need to
    // force a new thread. Keep token counts so the dispatch logic can still
    // pick new-thread if the context really is full.
    action.force_new_thread = false;
    Ok(Some(AutoPromptOutcome::Continue(action)))
}

/// Streaming LLM call specialised for the answerer prompt.
///
/// Mirrors `auto_prompt::call_language_model` but uses the answerer system
/// prompt and a plain-text (non-JSON) user turn carrying the context window.
/// Kept local rather than reusing `call_language_model` so the system prompt
/// and the user-turn wrapping stay cohesive with the answer shape.
async fn call_answerer(
    model: &Arc<dyn LanguageModel>,
    context_window: &str,
    cx: &gpui::AsyncApp,
) -> anyhow::Result<String> {
    let user_turn = format!(
        "The worker agent stopped to ask the user a question. \
         Reason about the question and the preceding context, then answer it \
         per the system prompt rules.\n\n\
         --- worker's last paragraphs ---\n\
         {context_window}\n\
         --- end ---"
    );

    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![ANSWERER_SYSTEM_PROMPT.into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![user_turn.into()],
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
            .context("pending_question: failed to start completion stream")?;

        let mut text_parts: Vec<String> = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(LanguageModelCompletionEvent::Text(text)) => text_parts.push(text),
                Ok(_) => {}
                Err(err) => {
                    log::warn!("[auto_prompt::pending_question] stream error: {err:#}");
                }
            }
        }
        let text = text_parts.concat();
        if text.trim().is_empty() {
            anyhow::bail!("pending_question: model returned no text events");
        }
        anyhow::Ok(text)
    };

    let timeout_future = cx.background_executor().timer(Duration::from_secs(45));
    pin_mut!(completion_future, timeout_future);

    match future::select(completion_future, timeout_future).await {
        future::Either::Left((Ok(text), _)) => Ok(text),
        future::Either::Left((Err(err), _)) => Err(err),
        future::Either::Right(_) => {
            anyhow::bail!("pending_question: answerer LLM call timed out after 45 seconds")
        }
    }
}

/// Lenient JSON extractor for the answerer response.
///
/// Reuses the same fence/brace heuristic as `auto_prompt::extract_json` but
/// lives here to keep the module self-contained and avoid widening the
/// `pub(crate)` surface more than necessary.
fn parse_answer(text: &str) -> anyhow::Result<AutoPromptResponse> {
    let json_str = extract_json_local(text);
    match serde_json::from_str::<AutoPromptResponse>(json_str) {
        Ok(response) => Ok(response),
        Err(strict_err) => {
            // Models occasionally wrap the answer in a smaller object without
            // all the standard fields — tolerate missing fields via serde defaults.
            #[derive(Deserialize)]
            struct LooseAnswer {
                #[serde(default)]
                next_prompt: Option<String>,
                #[serde(default)]
                confidence: Option<f64>,
                #[serde(default)]
                reason: Option<String>,
            }
            match serde_json::from_str::<LooseAnswer>(json_str) {
                Ok(loose) => Ok(AutoPromptResponse {
                    next_prompt: loose.next_prompt,
                    reason: loose.reason,
                    confidence: loose.confidence,
                    thread_summary: None,
                }),
                Err(loose_err) => anyhow::bail!(
                    "parse_answer: strict parse failed ({strict_err}); loose parse also failed ({loose_err})"
                ),
            }
        }
    }
}

fn extract_json_local(text: &str) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── detect_pending_question ───────────────────────────────────────────

    #[test]
    fn detects_option_request_a_vs_b() {
        let msg = "\
I did the audit. Here are the options.

**Option A (safe):** revert the no-op reset.
**Option B (finish the swap):** run the tests.

Which do you want? I'd lean Option A.";
        let pending = detect_pending_question(Some(msg)).expect("should detect");
        assert!(pending.question_text.contains("Which do you want"));
        // Context window includes the Option A / Option B paragraphs so the
        // answerer can see what it's choosing between.
        assert!(pending.context_window.contains("Option A"));
        assert!(pending.context_window.contains("Option B"));
    }

    #[test]
    fn detects_permission_request() {
        let msg = "I finished T1 and T2. Want me to file the follow-up as a prioritized issue?";
        let pending = detect_pending_question(Some(msg)).expect("should detect");
        assert!(pending.question_text.contains("Want me to"));
    }

    #[test]
    fn detects_real_world_katgpt_phase2_question() {
        // Verbatim trailing paragraphs from a real auto_prompt chain where the
        // worker asked a continue/pivot question at the end of a long summary.
        // This is the exact case the fast path was built to unblock — the old
        // flow stopped+summarized instead of answering.
        let msg = "\
### Commit

`866a21f0` on `develop` (not pushed; branch is 2 commits ahead of origin/develop). Used a message file for the amend to avoid shell backtick mangling.

### Next steps (Phase 2)

Plan 335 Phase 2 is the remaining GOAT work (G2 perf bench + G4 gain fixture):
- **T2.1** — `benches/paired_loss_bench.rs` (L=8192, target < 1us)
- **T2.2** — G2 zero-alloc confirmation (already true via iterator folds; may not need `FilterScratch`)
- **T2.3** — Build the G4 micro-GPT A/B fixture (ac_prefix on/off per Plan 313)
- **T2.4** — G4 gain gate (TOP-K intersect NO-COPY amplifies gap >= 1.5x vs ALL_TOKENS)
- **T2.5** — Document in `.benchmarks/335_paired_loss_goat.md`

Want me to continue with Phase 2 (G2 perf bench first, then the G4 A/B fixture), or pick up a different task?";
        let pending = detect_pending_question(Some(msg)).expect("should detect the trailing permission question");
        assert!(
            pending.question_text.contains("Want me to continue with Phase 2"),
            "question_text was: {}",
            pending.question_text
        );
        // Context window must include the Phase 2 task list so the answerer can
        // pick 'G2 perf bench first' with full context, not guess blindly.
        assert!(
            pending.context_window.contains("T2.1"),
            "context window should include the preceding task list, got: {}",
            pending.context_window
        );
        assert!(pending.context_window.contains("T2.5"));
    }

    #[test]
    fn detects_direct_question_with_you() {
        let msg = "Done with the refactor. Are you sure you want the ArrayVec port too?";
        let pending = detect_pending_question(Some(msg)).expect("should detect");
        assert!(pending.question_text.ends_with('?'));
    }

    #[test]
    fn detects_which_one_without_options_keyword() {
        let msg = "Two layouts ready. Which one looks right to you?";
        let pending = detect_pending_question(Some(msg)).expect("should detect");
        assert!(pending.question_text.contains("Which one"));
    }

    #[test]
    fn no_question_returns_none() {
        let msg = "I committed the fix on develop. All tests pass.";
        assert!(detect_pending_question(Some(msg)).is_none());
    }

    #[test]
    fn empty_message_returns_none() {
        assert!(detect_pending_question(None).is_none());
        assert!(detect_pending_question(Some("")).is_none());
        assert!(detect_pending_question(Some("   \n\n  ")).is_none());
    }

    #[test]
    fn skips_auto_prompt_summary_response() {
        // Phase 1 summary prompt structure — must NOT trigger question detection
        // (would loop the overflow dance).
        let msg = "\
Stop what you are doing and provide a concise summary of your progress.

Include: (1) what was the original task, (2) what was accomplished, \
(3) what remains to be done, (4) the current state of any active plan state.";
        assert!(detect_pending_question(Some(msg)).is_none());
    }

    #[test]
    fn rhetorical_doc_question_without_you_is_not_matched() {
        // A doc-style rhetorical question with no second-person address
        // should not trigger — avoids false positives in code comments.
        let msg = "## Why this design?\n\nThe DEC operators encode the boundary. What does this mean in practice?";
        assert!(detect_pending_question(Some(msg)).is_none());
    }

    #[test]
    fn only_scans_last_three_paragraphs() {
        // A question in paragraph 1 (out of 4) should NOT match — only the
        // trailing window is consulted.
        let msg = "\
Which do you want? Option A or B?

Then I did a bunch of work.

Then more work happened here.

All done now, committed on develop.";
        assert!(detect_pending_question(Some(msg)).is_none());
    }

    #[test]
    fn trailing_question_in_long_message_is_matched() {
        let mut paragraphs = String::new();
        for i in 0..10 {
            paragraphs.push_str(&format!("Paragraph {i} of context.\n\n"));
        }
        paragraphs.push_str("That's the full audit. Want me to do that?");
        let pending = detect_pending_question(Some(&paragraphs)).expect("should detect trailing q");
        assert!(pending.question_text.contains("Want me to"));
        // Context window is capped to the last 3 paragraphs.
        let para_count = pending.context_window.split("\n\n").count();
        assert!(para_count <= 4, "context window should be ≤ 4 paragraphs, got {para_count}");
    }

    // ── looks_like_option_context ─────────────────────────────────────────

    #[test]
    fn header_preceding_question_is_option_context() {
        assert!(looks_like_option_context("### My recommendation"));
        assert!(looks_like_option_context("## Option A: revert"));
    }

    #[test]
    fn numbered_list_is_option_context() {
        assert!(looks_like_option_context("1. Revert the no-op"));
        assert!(looks_like_option_context("- Option A"));
        assert!(looks_like_option_context("* Option B"));
    }

    #[test]
    fn short_label_colon_is_option_context() {
        assert!(looks_like_option_context("Option A:"));
        assert!(looks_like_option_context("My rec:"));
    }

    #[test]
    fn long_prose_is_not_option_context() {
        assert!(!looks_like_option_context(
            "This is a long prose paragraph that rambles on about the design \
             without being a header or a list or anything like that at all."
        ));
    }

    #[test]
    fn prose_introduced_task_list_is_option_context() {
        // The Phase 2 pattern from a real chain: prose intro + bullet list of
        // tasks the worker is offering to do next. Must count as context so the
        // answerer sees what it's choosing between.
        let para = "Plan 335 Phase 2 is the remaining GOAT work:\n\
            - **T2.1** — benches/paired_loss_bench.rs (L=8192, target < 1us)\n\
            - **T2.2** — G2 zero-alloc confirmation\n\
            - **T2.3** — Build the G4 micro-GPT A/B fixture";
        assert!(looks_like_option_context(para));
    }

    #[test]
    fn single_bullet_is_not_enough_to_be_option_context() {
        // A single stray bullet in a prose paragraph shouldn't trigger — avoids
        // pulling in unrelated context. Requires >= 2 list items.
        let para = "Here is a thought about the design that happens to mention - one bullet mid-sentence and then rambles on without any more list items at all whatsoever.";
        assert!(!looks_like_option_context(para));
    }

    #[test]
    fn numbered_list_lines_detected() {
        assert!(is_list_item_line("- T2.1 do the thing"));
        assert!(is_list_item_line("  * alt bullet"));
        assert!(is_list_item_line("1. first"));
        assert!(is_list_item_line("42. deep"));
        assert!(!is_list_item_line("regular prose line"));
        assert!(!is_list_item_line("-no space after dash"));
        assert!(!is_list_item_line("v1.2 version string"));
    }

    // ── extract_json_local / parse_answer ─────────────────────────────────

    #[test]
    fn parse_answer_fenced_json() {
        let raw = "Here is my answer:\n\n```json\n{\"next_prompt\": \"Go with A\", \"confidence\": 0.8}\n```";
        let parsed = parse_answer(raw).expect("should parse");
        assert_eq!(parsed.next_prompt.as_deref(), Some("Go with A"));
        assert_eq!(parsed.confidence, Some(0.8));
    }

    #[test]
    fn parse_answer_bare_json() {
        let raw = r#"{"next_prompt": "Yes proceed", "confidence": 0.9, "reason": "safe"}"#;
        let parsed = parse_answer(raw).expect("should parse");
        assert_eq!(parsed.next_prompt.as_deref(), Some("Yes proceed"));
    }

    #[test]
    fn parse_answer_tolerates_missing_fields() {
        let raw = r#"{"next_prompt": "Just the answer"}"#;
        let parsed = parse_answer(raw).expect("should parse via loose fallback");
        assert_eq!(parsed.next_prompt.as_deref(), Some("Just the answer"));
        assert_eq!(parsed.confidence, None);
    }

    #[test]
    fn parse_answer_rejects_garbage() {
        assert!(parse_answer("totally not json at all").is_err());
    }
}
