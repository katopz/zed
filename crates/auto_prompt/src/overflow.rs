//! Context-overflow, max-tokens and output-bloat handling: the deterministic
//! outcomes for a stop caused by a full context window or the output cap.

use crate::remaining_work::{
    PlanRepoFilter, SummaryContinuation, auto_claim_plan, detect_remaining_plan_tasks,
    extract_summary_next_steps, housekeeping_continuation, looks_like_voluntary_summary,
};
use crate::{
    AutoPromptAction, AutoPromptConfig, AutoPromptDecision, AutoPromptDelayReason,
    AutoPromptOutcome, CONTINUE_REMAINS_DECISION, LlmCallData, api_unreachable,
    build_prompt_summary, call_language_model, chain, clear_summary_for_session, debug_log,
    load_config_cached, paused, peer_states, reset_iteration_with_session, set_summary_state,
    summary_state_for, truncate_at_char_boundary, with_first_prompt_context,
};
use std::time::Duration;

/// Phase 1 request sent to the SAME thread — no `## 1/## 2/## 3` headers
/// since the AI already has full conversation context. Raw instruction only.
pub(crate) const CONTEXT_OVERFLOW_SUMMARY_PROMPT: &str = "Stop what you are doing and provide a concise summary of your progress. Include: (1) what was the original task, (2) what was accomplished, (3) what remains to be done, (4) the current state of any active plans (reference by filename). Be thorough — this summary will be used to continue in a fresh context. If nothing remains to be done, end the summary with a single line containing only <stop> so the chain halts instead of continuing.";

/// Same-thread continuation for a MaxTokens stop where the window still has
/// room: the generation hit the model's OUTPUT cap, not the context window,
/// so the conversation is intact and no summary/fork is needed.
pub(crate) const MAX_TOKENS_RESUME_PROMPT: &str = "Your last response was cut off mid-generation at the model's output token limit. The conversation context is intact — resume exactly where you stopped. Do not repeat or re-summarize earlier work.";

/// Headroom a follow-up same-thread request needs (its own prompt plus the
/// provider's `max_tokens` reservation) before we consider the window full.
pub(crate) const MAX_TOKENS_NEXT_REQUEST_RESERVE_TOKENS: u64 = 8_192;

/// Whether a `StopReason::MaxTokens` stop means the model's CONTEXT WINDOW is
/// exhausted (`input + output + reserve >= window`), as opposed to a mere
/// output-cap cut with room left. Without reported usage this returns false —
/// callers keep the legacy behavior — and a zero `model_window` (unknown) is
/// treated as not full.
pub(crate) fn max_tokens_context_window_full(
    input_tokens: Option<u64>,
    output_tokens: u64,
    model_window: u64,
) -> bool {
    let Some(input) = input_tokens else {
        return false;
    };
    if model_window == 0 {
        return false;
    }
    input.saturating_add(output_tokens) + MAX_TOKENS_NEXT_REQUEST_RESERVE_TOKENS >= model_window
}

/// Output-usage fraction (output / max_output) at or above which a handoff
/// or resume prompt carries the bloat-avoidance section.
pub(crate) const OUTPUT_BLOAT_RATIO_THRESHOLD: f64 = 0.6;
/// Smallest estimated entry size (in tokens) worth naming as a bloat source.
/// Keeps the report free of ordinary tool calls.
pub(crate) const OUTPUT_BLOAT_MIN_SOURCE_TOKENS: usize = 8_000;
/// How many bloat sources to name at most — the report rides in every
/// continuation prompt, so it must stay compact.
pub(crate) const OUTPUT_BLOAT_MAX_SOURCES: usize = 3;

pub(crate) const BLOAT_SECTION_HEADER: &str = "## Context bloat avoidance";

/// A thread entry worth naming as a token hog.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BloatSource {
    label: String,
    est_tokens: usize,
}

/// The bloat-avoidance directive for a thread that ran (nearly) out of
/// output-token budget: names the largest space consumers found by scanning
/// the thread from the top and states the avoidance rules. None when there
/// is no evidence worth reporting — continuation prompts stay unpolluted.
pub(crate) fn output_bloat_directive(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    cx: &gpui::App,
) -> Option<String> {
    let usage = thread.read(cx).token_usage();
    if !output_bloat_gate(
        usage.map(|usage| usage.output_tokens),
        usage.and_then(|usage| usage.max_output_tokens),
    ) {
        return None;
    }
    let mut sources: Vec<BloatSource> = thread
        .read(cx)
        .bloat_candidates(cx)
        .into_iter()
        .map(|(label, chars)| BloatSource {
            label,
            est_tokens: chars / 4,
        })
        .filter(|source| source.est_tokens >= OUTPUT_BLOAT_MIN_SOURCE_TOKENS)
        .collect();
    if sources.is_empty() {
        return None;
    }
    sources.sort_by_key(|source| std::cmp::Reverse(source.est_tokens));
    sources.truncate(OUTPUT_BLOAT_MAX_SOURCES);
    Some(format_bloat_directive(
        usage.map(|usage| (usage.output_tokens, usage.max_output_tokens)),
        &sources,
    ))
}

/// Cheap pre-scan gate: only pay for the per-entry measurement pass when the
/// output budget is (nearly) exhausted — or when the provider reports no max
/// at all, in which case the source-size floor alone decides. A low-ratio
/// thread (routine stop, routine chain hop) exits before any scanning.
pub(crate) fn output_bloat_gate(
    output_tokens: Option<u64>,
    max_output_tokens: Option<u64>,
) -> bool {
    match (output_tokens, max_output_tokens) {
        (Some(output), Some(max)) if max > 0 => {
            output as f64 / max as f64 >= OUTPUT_BLOAT_RATIO_THRESHOLD
        }
        // No usage or no known cap: let the source-size floor decide.
        _ => true,
    }
}

pub(crate) fn format_bloat_directive(
    usage: Option<(u64, Option<u64>)>,
    sources: &[BloatSource],
) -> String {
    let usage_note = match usage {
        Some((output, Some(max))) if max > 0 => format!(
            " ({}k of {}k output tokens used)",
            output / 1_000,
            max / 1_000
        ),
        Some((output, None)) => format!(" ({output} output tokens used)"),
        _ => String::new(),
    };
    let mut directive = format!(
        "OUTPUT-BLOAT NOTICE: the previous turn died at the model's output token limit{usage_note}. Bisecting this thread from the top, the largest space consumers are:"
    );
    for source in sources {
        directive.push_str(&format!(
            "\n- `{}` — ~{}k tokens",
            source.label,
            source.est_tokens / 1_000
        ));
    }
    directive.push_str(
        "\nRules for the rest of this task:\n\
        - Never re-run the listed commands at full width; sample with head/tail or line ranges and keep tool outputs small.\n\
        - Keep narration between tool calls to one or two sentences; do not restate file contents.\n\
        - Prefer small targeted edits over whole-file rewrites.\n\
        - If a response nears the limit, stop cleanly and report state instead of being cut off mid-action.",
    );
    directive
}

/// Insert the bloat-avoidance section into an assembled
/// `with_first_prompt_context` payload, before the decision slot so
/// `extract_decision_prompt` (which cuts from the `## N. Decision` header on)
/// never mistakes the section for decision text. Appended at the end when no
/// decision header is present. Idempotent.
pub fn with_bloat_section(prompt: &str, bloat_directive: Option<&str>) -> String {
    let Some(directive) = bloat_directive.map(str::trim).filter(|d| !d.is_empty()) else {
        return prompt.to_string();
    };
    if prompt.contains(BLOAT_SECTION_HEADER) {
        return prompt.to_string();
    }
    let section = format!("{BLOAT_SECTION_HEADER}\n\n{directive}");
    let decision_index = ["## 3. Decision", "## 2. Decision"]
        .iter()
        .filter_map(|marker| prompt.find(marker))
        .min();
    match decision_index {
        Some(index) => {
            let mut output = String::with_capacity(prompt.len() + section.len() + 4);
            output.push_str(&prompt[..index]);
            output.push_str(&section);
            output.push_str("\n\n---\n\n");
            output.push_str(&prompt[index..]);
            output
        }
        None => format!("{prompt}\n\n---\n\n{section}"),
    }
}

/// Last `max_chars` characters of `text`, safe on multi-byte boundaries
/// (emoji, CJK). Used to feed a bounded tail of a window-truncated output to
/// the orchestrator summary call.
pub(crate) fn tail_at_char_boundary(text: &str, max_chars: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    text.chars().skip(char_count - max_chars).collect()
}

/// Same-thread retry after backoff when the worker's turn ended because the
/// provider API was unreachable. Shared by the native and Claude decision
/// paths and checked before context overflow: the Phase 1 summarize request,
/// the orchestrator call, and the rules-based summary handoff all need the
/// same dead API, and none has fresh information to act on.
///
/// A pending Phase 1 (`summary_state == 1`) re-sends the summarize request,
/// so Phase 2 never forks with the error text as the "summary".
pub(crate) fn api_unreachable_decision(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    config: &AutoPromptConfig,
    cx: &gpui::App,
) -> Option<AutoPromptDecision> {
    let thread_ref = thread.read(cx);
    let session_id_str = thread_ref.session_id().to_string();
    let Some(error) = api_unreachable::unreachable_error_from_thread(thread_ref, cx) else {
        api_unreachable::reset_unreachable_streak(&session_id_str);
        return None;
    };
    let streak = api_unreachable::record_unreachable(&session_id_str);
    if streak > api_unreachable::MAX_UNREACHABLE_STREAK {
        api_unreachable::reset_unreachable_streak(&session_id_str);
        clear_summary_for_session(&session_id_str);
        reset_iteration_with_session(&session_id_str);
        log::warn!(
            "[auto_prompt] PATH=api_unreachable: still unreachable after {} retries — stopping chain (session={}): {}",
            api_unreachable::MAX_UNREACHABLE_STREAK,
            thread_ref.session_id(),
            truncate_at_char_boundary(&error, 200)
        );
        debug_log::write_log(
            "no_action",
            serde_json::json!({"reason": "api_unreachable_budget_exhausted"}),
        );
        return Some(AutoPromptDecision::NoAction);
    }
    let delay_ms = config.backoff_delay_ms(streak);
    let session_id = thread_ref.session_id().clone();
    let summary_pending = summary_state_for(&session_id.to_string()) == 1;
    log::warn!(
        "[auto_prompt] PATH=api_unreachable: streak={streak}, summary_pending={summary_pending} — retrying same thread in {delay_ms}ms (session={session_id}): {}",
        truncate_at_char_boundary(&error, 200)
    );
    debug_log::write_log(
        "api_unreachable_backoff",
        serde_json::json!({
            "streak": streak,
            "delay_ms": delay_ms,
            "summary_pending": summary_pending,
            "error": debug_log::truncate(&error, 500),
        }),
    );
    let next_prompt = if summary_pending {
        CONTEXT_OVERFLOW_SUMMARY_PROMPT.to_string()
    } else {
        "The provider API was unreachable (network/DNS) and the connection has been retried. Continue from where you left off.".to_string()
    };
    let last_assistant_message = thread_ref
        .last_assistant_message_text(cx)
        .filter(|message| !api_unreachable::is_unreachable_message(message));
    Some(AutoPromptDecision::DispatchAfterDelay {
        action: AutoPromptAction {
            from_session_id: session_id,
            from_title: thread_ref.title().map(|title| title.to_string()),
            next_prompt,
            work_dirs: thread_ref.work_dirs().map(|list| list.paths().to_vec()),
            original_user_message: None,
            profile_id: None,
            // No token count: `dispatch_action` forks native threads above the
            // same-thread threshold, and a fork here carries only the retry
            // sentence — no summary, no context. Each fork then hit the same
            // dead API and forked again. The retry must land on this thread.
            actual_input_tokens: None,
            approximate_token_count: 0,
            last_assistant_message,
            force_new_thread: false,
            focus_new_thread: false,
        },
        delay_ms,
        // A pending Phase 1 must actually send the summarize request.
        reason: if summary_pending {
            AutoPromptDelayReason::Backoff
        } else {
            AutoPromptDelayReason::ApiUnreachable
        },
    })
}

/// Phase 1/2 context-overflow state machine, shared by the native path and
/// the Claude overflow parity path (plan 023 A2/A3). Never calls the LLM.
///
/// - Phase 1 (`summary_state == 0`, last message not already a summary):
///   returns `ContextOverflow` so the UI sends a "summarize" prompt to the
///   SAME thread, then sets `summary_state = 1`.
/// - Phase 2 (`summary_state == 1`, or a voluntary summary skipped Phase 1):
///   the last assistant message IS the summary — builds the continuation
///   prompt and returns `Continue` with `force_new_thread = true` and reset
///   token counts.
pub(crate) fn context_overflow_outcome(data: &LlmCallData) -> AutoPromptOutcome {
    let session_id_str = data.session_id.to_string();

    // Config kill switch (plan 031 silent stop): drop all overflow handling
    // with only a log line — no Phase 1 summarize request, no Phase 2 fork.
    if paused() {
        log::warn!(
            "[auto_prompt::context_overflow] paused — silently stopping (session={session_id_str})"
        );
        return AutoPromptOutcome::Stopped {
            reason: "auto-prompt paused (config)".to_string(),
        };
    }

    // ── Issue 007 guard: API exhaustion + context overflow ────────────
    //
    // If the source thread's completion request itself failed
    // (`had_api_error=true`: rate limit, network, 5xx, etc.) AND its
    // context has overflowed, Phase 2 would create a new continuation
    // thread whose first turn immediately hits the same exhausted API
    // and fails — a death spiral of doomed threads that burns quota
    // without making progress. Defer instead: `RetryAfterBackoff` makes
    // the caller sleep and re-run the decision.
    //
    // Deliberately checks `had_api_error`, NOT the broader `had_error`.
    // `had_error` is also set by a single failed tool call anywhere in
    // the turn — extremely common in normal agentic work and unrelated
    // to API availability.
    // Also covers Claude Code's synthetic `API Error: … (ENOTFOUND)` message,
    // which ends the turn normally with `had_api_error` false.
    let api_unreachable_message = data
        .last_assistant_message
        .as_deref()
        .is_some_and(api_unreachable::is_unreachable_message);
    if data.had_api_error || api_unreachable_message {
        let config = load_config_cached().unwrap_or_default();
        // failure count is incremented by the caller's retry loop on
        // receipt of this outcome; use the current value to size the
        // initial delay so we don't pile up retries faster than the
        // upstream API can recover.
        let current_failures = chain::llm_failures(&session_id_str);
        let delay_ms = config.backoff_delay_ms(current_failures.saturating_add(1));
        log::warn!(
            "[auto_prompt::context_overflow] overflow + had_api_error — deferring Phase 1/2 by {delay_ms}ms (current_failures={current_failures}, session={session_id_str})"
        );
        return AutoPromptOutcome::RetryAfterBackoff {
            delay_ms,
            reason: "context overflow with source thread error (likely rate limit)".to_string(),
        };
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
            "[auto_prompt::context_overflow] Last message is already a voluntary summary — skipping Phase 1, going straight to Phase 2 (session={session_id_str})"
        );
        // Keep summary_state==0 in the registry: we never asked for a summary,
        // so there's nothing to clear. The Phase 2 branch below handles the
        // handoff directly when `skip_phase_1` is set.
    }

    log::info!(
        "[auto_prompt::context_overflow] session={session_id_str} summary_state={summary_state} skip_phase_1={skip_phase_1}"
    );

    if summary_state == 0 && !skip_phase_1 {
        // Phase 1: Request summarization. Return ContextOverflow so the
        // UI sends a "summarize" message to the current thread.
        let next_prompt = CONTEXT_OVERFLOW_SUMMARY_PROMPT.to_string();
        set_summary_state(&session_id_str, 1);
        // The summarize request lands in the same thread, so the stop that
        // follows it is a chained hop — keep the chain state alive across it.
        chain::arm_continuation(&session_id_str);
        log::info!(
            "[auto_prompt::context_overflow] Returning ContextOverflow — requesting summary from AI (session={session_id_str})"
        );
        return AutoPromptOutcome::ContextOverflow(AutoPromptAction {
            from_session_id: data.session_id.clone(),
            from_title: data.title.clone(),
            next_prompt,
            work_dirs: data.work_dirs.clone(),
            original_user_message: data.original_user_message.clone(),
            profile_id: data.profile_id.clone(),
            actual_input_tokens: data.actual_input_tokens,
            approximate_token_count: data.approximate_token_count,
            last_assistant_message: data.last_assistant_message.clone(),
            force_new_thread: false,
            focus_new_thread: false,
        });
    } else if summary_state == 1 || skip_phase_1 {
        // Phase 2: AI has responded with summary (or already had a voluntary
        // summary, via `skip_phase_1`). The last_assistant_message IS the
        // summary. Create a new thread with the inlined summary.
        clear_summary_for_session(&session_id_str);

        // Phase 2 (P2.2 native summary hook): broadcast the summary to the
        // agent board so peer agents can see what this agent just concluded.
        // Mirrors the claude_agent path's `maybe_broadcast_summary_to_board`.
        // We already know this is a summary (summary_state==1 or voluntary),
        // so no contains_summary check needed.
        if let Some(summary) = data.last_assistant_message.as_deref() {
            peer_states::broadcast_state(&session_id_str, None, summary, "summary");
        }
        log::info!("[auto_prompt::context_overflow] Summary received (session={session_id_str})");

        let prompt_summary = build_prompt_summary(
            None,
            data.title.as_deref(),
            Some("context overflow: continuing in new thread with summary"),
            data.last_assistant_message.as_deref(),
            data.original_user_message.as_deref(),
            data.first_user_message.as_deref(),
        );

        // Plan 027 ("do not process"): the summary already says what remains,
        // so no orchestrator/reasoning pass and no detector chain — the new
        // thread's decision slot is the fixed directive. The ONE piece of
        // processing kept is the termination guard: a summary that declares
        // nothing left only switches to housekeeping when no unclaimed plan
        // tasks remain (plan tasks beat nothing-left); otherwise the fixed
        // directive is used and the worker picks the tasks up itself.
        let summary_continuation = data
            .last_assistant_message
            .as_deref()
            .and_then(extract_summary_next_steps);
        if matches!(summary_continuation, Some(SummaryContinuation::Terminal)) {
            // Plan 031 silent stop: a terminal summary (all remaining work
            // armed/deferred/owner-gated) ends the chain — plan tasks do NOT
            // resurrect it, and the stop-time housekeeping hook is skipped by
            // the caller via summary_declares_terminal.
            log::warn!(
                "auto_prompt: ContextOverflow Phase 2 — summary is terminal (all remaining work armed/deferred/owner-gated) — stopping; plan tasks do not resurrect a terminal chain"
            );
            return AutoPromptOutcome::Stopped {
                reason: "summary is terminal — all remaining work is armed/deferred/owner-gated"
                    .to_string(),
            };
        }
        let continuation = if matches!(summary_continuation, Some(SummaryContinuation::NothingLeft))
        {
            let plan_tasks_remain = detect_remaining_plan_tasks(
                &data.context_json,
                PlanRepoFilter::CurrentRepo,
                data.work_dirs.as_deref(),
            )
            .or_else(|| {
                detect_remaining_plan_tasks(
                    &data.context_json,
                    PlanRepoFilter::OtherRepos,
                    data.work_dirs.as_deref(),
                )
            })
            .is_some();
            if plan_tasks_remain {
                log::warn!(
                    "auto_prompt: ContextOverflow Phase 2 — summary says nothing left but plan tasks remain, continuing with fixed decision"
                );
                CONTINUE_REMAINS_DECISION.to_string()
            } else {
                log::warn!(
                    "auto_prompt: ContextOverflow Phase 2 — summary declares nothing left and no plan tasks remain, switching to housekeeping"
                );
                housekeeping_continuation()
            }
        } else {
            log::warn!(
                "auto_prompt: ContextOverflow Phase 2 — using summary + fixed decision directive, enriched with the summary's next steps when present (no processing)"
            );
            match summary_continuation {
                Some(SummaryContinuation::Steps(steps)) => {
                    format!("{steps}\n\n{CONTINUE_REMAINS_DECISION}")
                }
                _ => CONTINUE_REMAINS_DECISION.to_string(),
            }
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
        return AutoPromptOutcome::Continue(action);
    }

    // Unexpected state — reset and stop
    clear_summary_for_session(&session_id_str);
    let stop_reason = "context overflow: unexpected summary state".to_string();
    reset_iteration_with_session(&data.session_id.to_string());
    AutoPromptOutcome::Stopped {
        reason: stop_reason,
    }
}

/// System prompt for the orchestrator-produced handoff summary when a
/// MaxTokens stop coincided with a full context window. The summarizer cannot
/// see the conversation — only bounded fragments — so it must say so honestly
/// and still emit the four-part handoff format. Reuses the standard
/// `AutoPromptResponse` JSON contract via `thread_summary`.
pub(crate) const CUTOFF_CONTEXT_FULL_SUMMARY_SYSTEM: &str = "# version: cutoff_summary\
    \n\nThe worker's response stream was cut off mid-generation because the conversation filled the model's context window.\
    You cannot see the full conversation — only the fragments below. Write the handoff summary a fresh thread needs to continue the task.\
    \n\nRespond ONLY with valid JSON:\
    \n{\"confidence\": 0.9, \"next_prompt\": null, \"reason\": string, \"thread_summary\": string}\
    \n\nthread_summary MUST contain these four sections:\
    \n- (1) Original task — infer from the fragments; write \"unknown\" only if truly absent\
    \n- (2) What was accomplished up to the cutoff\
    \n- (3) What remains to be done\
    \n- (4) Active plans/state — reference plan filenames when they appear in the fragments";

/// Characters of the truncated final output fed to the cutoff summarizer
/// (~6k tokens — keeps the orchestrator call itself far below the window).
pub(crate) const CUTOFF_SUMMARY_TAIL_CHARS: usize = 24_000;

/// Outcome machine for a MaxTokens stop that exhausted the model's context
/// window (`LlmCallData::cutoff_context_full`). The Phase 1 same-thread
/// summarize request is impossible here — the input alone already fills the
/// window and the API rejects every follow-up — so the summary is produced by
/// a bounded orchestrator call and the thread is forked with it. Falls back to
/// the legacy bare continuation (fresh thread, title-only summary) when the
/// orchestrator call fails, so the chain keeps moving instead of stalling.
pub(crate) async fn cutoff_context_full_outcome(
    data: &LlmCallData,
    cx: &gpui::AsyncApp,
) -> AutoPromptOutcome {
    let session_id_str = data.session_id.to_string();

    // Bounded fragments: the new thread's seed context. The tail of the
    // truncated output is the freshest signal about what the worker was doing
    // when the window filled.
    let tail = data
        .last_assistant_message
        .as_deref()
        .map(|message| tail_at_char_boundary(message, CUTOFF_SUMMARY_TAIL_CHARS));
    let mut context = format!(
        "## Thread title\n{}\n\n## Original task\n{}\n\n## First prompt\n{}\n",
        data.title.as_deref().unwrap_or("(none)"),
        data.original_user_message.as_deref().unwrap_or("(none)"),
        data.first_user_message.as_deref().unwrap_or("(none)"),
    );
    if let Some(tail) = tail.as_deref().filter(|tail| !tail.trim().is_empty()) {
        context.push_str(&format!(
            "\n## Truncated final output (tail — cut off mid-generation by the context window)\n{tail}\n"
        ));
    }

    let mut summary = None;
    for attempt in 1..=2u32 {
        if attempt > 1 {
            cx.background_executor()
                .timer(Duration::from_millis(2_000))
                .await;
        }
        match call_language_model(
            &data.model,
            CUTOFF_CONTEXT_FULL_SUMMARY_SYSTEM,
            &context,
            cx,
        )
        .await
        {
            Ok((_raw, response)) => match response.thread_summary.as_deref() {
                Some(text) if !text.trim().is_empty() => {
                    summary = Some(text.trim().to_string());
                    break;
                }
                _ => log::warn!(
                    "auto_prompt: cutoff summary attempt {attempt} returned no thread_summary"
                ),
            },
            Err(err) => {
                log::warn!("auto_prompt: cutoff summary attempt {attempt} failed: {err:#}")
            }
        }
    }

    let Some(summary) = summary else {
        // Legacy fallback (pre-fix behavior, but forced to fork): the window
        // is full, so a same-thread continuation is guaranteed to be rejected.
        log::warn!(
            "auto_prompt: cutoff summary unavailable — falling back to bare fork (session={session_id_str})"
        );
        let next_prompt = with_first_prompt_context(
            "Context limit reached. Continue from where we left off.".to_string(),
            build_prompt_summary(
                None,
                data.title.as_deref(),
                Some("context limit reached (MaxTokens)"),
                None,
                data.original_user_message.as_deref(),
                data.first_user_message.as_deref(),
            )
            .as_deref(),
            data.title.as_deref(),
            None,
        );
        let mut action = data.make_continue_action(next_prompt);
        action.actual_input_tokens = None;
        action.approximate_token_count = 0;
        action.force_new_thread = true;
        return AutoPromptOutcome::Continue(action);
    };

    log::warn!(
        "auto_prompt: cutoff summary produced ({} chars) — forking with real handoff (session={session_id_str})",
        summary.len()
    );

    // Mirror the Phase 2 fork: terminal summaries stop the chain, nothing-left
    // summaries defer to plan tasks, otherwise continue with the summary's
    // next steps plus the fixed directive. Slash-command originals are
    // preserved so the fresh thread re-activates the skill.
    if matches!(
        extract_summary_next_steps(&summary),
        Some(SummaryContinuation::Terminal)
    ) {
        return AutoPromptOutcome::Stopped {
            reason: "cutoff summary is terminal — all remaining work is armed/deferred/owner-gated"
                .to_string(),
        };
    }
    let continuation = match extract_summary_next_steps(&summary) {
        Some(SummaryContinuation::NothingLeft) => {
            let plan_tasks_remain = detect_remaining_plan_tasks(
                &data.context_json,
                PlanRepoFilter::CurrentRepo,
                data.work_dirs.as_deref(),
            )
            .or_else(|| {
                detect_remaining_plan_tasks(
                    &data.context_json,
                    PlanRepoFilter::OtherRepos,
                    data.work_dirs.as_deref(),
                )
            })
            .is_some();
            if plan_tasks_remain {
                CONTINUE_REMAINS_DECISION.to_string()
            } else {
                housekeeping_continuation()
            }
        }
        Some(SummaryContinuation::Steps(steps)) => {
            format!("{steps}\n\n{CONTINUE_REMAINS_DECISION}")
        }
        _ => CONTINUE_REMAINS_DECISION.to_string(),
    };
    let continuation = match data.original_user_message.as_deref() {
        Some(msg) if msg.trim().starts_with('/') => {
            let cmd = msg.trim();
            format!(
                "{cmd}\n\nContext overflowed mid-task. Pick up where the summary left off. \
                     Do NOT summarize again — continue working immediately."
            )
        }
        _ => continuation,
    };

    let prompt_summary = build_prompt_summary(
        None,
        data.title.as_deref(),
        Some("stream cut off by the context window; continuing in new thread with summary"),
        Some(&summary),
        data.original_user_message.as_deref(),
        data.first_user_message.as_deref(),
    );
    let next_prompt = with_first_prompt_context(
        continuation,
        prompt_summary.as_deref(),
        data.title.as_deref(),
        Some(&summary),
    );

    auto_claim_plan(
        &next_prompt,
        &data.context_json,
        &data.session_id,
        data.title.as_deref(),
    );
    let mut action = data.make_continue_action(next_prompt);
    // The fresh thread starts from the summary, not the bloated old context.
    action.actual_input_tokens = None;
    action.approximate_token_count = 0;
    action.force_new_thread = true;
    AutoPromptOutcome::Continue(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_prompt_tests::overflow_test_data;
    use crate::extract_decision_prompt;

    // --- output bloat detection tests ---

    fn bloat_source(label: &str, est_tokens: usize) -> BloatSource {
        BloatSource {
            label: label.to_string(),
            est_tokens,
        }
    }

    #[test]
    fn test_output_bloat_gate() {
        // High ratio → gate open.
        assert!(output_bloat_gate(Some(128_000), Some(131_072)));
        // At exactly the threshold → open.
        assert!(output_bloat_gate(Some(60), Some(100)));
        // Low ratio → closed.
        assert!(!output_bloat_gate(Some(10_000), Some(131_072)));
        // Unknown cap → open (source-size floor decides).
        assert!(output_bloat_gate(Some(50_000), None));
        // No usage at all → open (source-size floor decides).
        assert!(output_bloat_gate(None, None));
        // Zero max must not divide by zero.
        assert!(output_bloat_gate(Some(50_000), Some(0)));
    }

    #[test]
    fn test_format_bloat_directive_names_sources_and_rules() {
        let sources = vec![
            bloat_source("terminal: cargo check --workspace", 31_000),
            bloat_source("assistant message", 9_500),
        ];
        let directive = format_bloat_directive(Some((128_000, Some(131_072))), &sources);
        assert!(directive.contains("128k of 131k output tokens used"));
        assert!(directive.contains("`terminal: cargo check --workspace` — ~31k tokens"));
        assert!(directive.contains("`assistant message` — ~9k tokens"));
        assert!(directive.contains("Never re-run the listed commands at full width"));
        assert!(directive.contains("stop cleanly and report state"));
    }

    #[test]
    fn test_format_bloat_directive_without_max() {
        let sources = vec![bloat_source("terminal: cargo check", 31_000)];
        let directive = format_bloat_directive(Some((50_000, None)), &sources);
        assert!(directive.contains("50000 output tokens used"));
    }

    #[test]
    fn test_with_bloat_section_inserts_before_decision() {
        let prompt = with_first_prompt_context(
            "continue the task".to_string(),
            Some("summary of the work"),
            None,
            Some("last assistant message"),
        );
        let with_bloat = with_bloat_section(&prompt, Some("OUTPUT-BLOAT NOTICE: ..."));
        let bloat_pos = with_bloat.find(BLOAT_SECTION_HEADER).unwrap();
        let decision_pos = with_bloat.find("## 3. Decision").unwrap();
        let summary_pos = with_bloat.find("## 1. Thread Summary").unwrap();
        assert!(summary_pos < bloat_pos);
        assert!(bloat_pos < decision_pos);
        // Decision extraction is unaffected: cut starts at the decision header.
        let extracted = extract_decision_prompt(&with_bloat).unwrap();
        assert!(extracted.starts_with("continue the task"));
        assert!(!extracted.contains(BLOAT_SECTION_HEADER));
    }

    #[test]
    fn test_with_bloat_section_alternate_decision_header() {
        // The no-summary branch of with_first_prompt_context uses "## 2. Decision".
        let prompt = with_first_prompt_context(
            "continue".to_string(),
            None,
            None,
            Some("last assistant message"),
        );
        let with_bloat = with_bloat_section(&prompt, Some("OUTPUT-BLOAT NOTICE: ..."));
        let bloat_pos = with_bloat.find(BLOAT_SECTION_HEADER).unwrap();
        let decision_pos = with_bloat.find("## 2. Decision").unwrap();
        assert!(bloat_pos < decision_pos);
    }

    #[test]
    fn test_with_bloat_section_noop_and_idempotent() {
        let prompt = "plain prompt".to_string();
        // None / empty directive → unchanged.
        assert_eq!(with_bloat_section(&prompt, None), prompt);
        assert_eq!(with_bloat_section(&prompt, Some("   ")), prompt);
        // No decision header → appended at the end.
        let appended = with_bloat_section(&prompt, Some("NOTICE"));
        assert!(appended.starts_with("plain prompt"));
        assert!(appended.contains(BLOAT_SECTION_HEADER));
        // Already present → unchanged (idempotent).
        let twice = with_bloat_section(&appended, Some("NOTICE"));
        assert_eq!(twice, appended);
    }

    #[test]
    fn test_make_continue_action_applies_bloat_section() {
        let mut data = overflow_test_data("bloat-choke-test", Some("mid-work message"));
        data.bloat_directive = Some("OUTPUT-BLOAT NOTICE: test".to_string());
        let action = data.make_continue_action("continue".to_string());
        assert!(action.next_prompt.contains(BLOAT_SECTION_HEADER));
        // And without the directive the prompt is untouched.
        let mut plain = overflow_test_data("bloat-choke-test-2", Some("mid-work message"));
        plain.bloat_directive = None;
        let plain_action = plain.make_continue_action("continue".to_string());
        assert!(!plain_action.next_prompt.contains(BLOAT_SECTION_HEADER));
    }

    #[test]
    fn tail_at_char_boundary_basics() {
        assert_eq!(tail_at_char_boundary("abcdef", 10), "abcdef");
        assert_eq!(tail_at_char_boundary("abcdef", 3), "def");
        assert_eq!(tail_at_char_boundary("", 5), "");
        assert_eq!(tail_at_char_boundary("ab", 0), "");
    }

    #[test]
    fn tail_at_char_boundary_multibyte() {
        // Emoji + CJK must never be sliced mid-character.
        let text = "🦀 analyis 完了 — done";
        let tail = tail_at_char_boundary(text, 5);
        assert!(text.ends_with(&tail));
        assert_eq!(tail.chars().count(), 5);
        // A longer-than-max multi-byte tail must stay valid UTF-8.
        let emoji = "🦀🦀🦀🦀🦀";
        let tail = tail_at_char_boundary(emoji, 2);
        assert_eq!(tail, "🦀🦀");
    }
}
