//! Claude (ACP) agent auto-prompt decision logic.
//!
//! Intentionally simple and isolated from the native-agent path. Claude Code
//! manages its own context window, compaction, and tool loop internally —
//! Zed's auto_prompt layer must never stop, compact, or fork a new thread
//! for it. The only job here is to nudge the same thread to keep working
//! until the task is done or the user cancels.
//!
//! Rules (do not change without explicit owner sign-off):
//!   1. Never return `ContextOverflow` — no token-limit triggers.
//!   2. Never set `force_new_thread = true` — always continue in the same thread.
//!   3. Stop on: cancel, max-iterations, or EndTurn without tool use (task done).
//!      Otherwise continue.
//!   4. The continuation prompt asks Claude to reason about its last paragraph
//!      in the context of the current project state and continue answering.

use agent_client_protocol::schema as acp;
use gpui::App;

use crate::{AutoPromptAction, AutoPromptDecision, get_iteration, load_config_cached};

/// Continuation prompt sent to the same Claude thread.
///
/// Claude already has the full conversation history and project state — this
/// is just a nudge to review its last output and pick up the next step.
const CONTINUATION_PROMPT: &str =
    "Review your last response above in the context of the current project state \
     (files, plans, tests, build). Continue the task from where you left off — \
     do not repeat completed work, pick up the next step.";

/// Retry prompt used after a refusal, sent to the same thread.
const REFUSAL_RETRY_PROMPT: &str =
    "The previous request was refused. Retry the task from where you left off.";

/// Decide the next auto-prompt action for a Claude (ACP) agent thread.
///
/// Returns one of:
/// - `NoAction` — chain should stop (cancelled, max iterations, or EndTurn
///   without tool use — agent is done or waiting for user input).
/// - `DispatchNow(action)` — send `action.next_prompt` to the SAME thread immediately.
/// - `DispatchAfterDelay { action, delay_ms }` — same, after an error backoff.
///
/// This function NEVER returns `NeedsLlmCall` and therefore never participates
/// in the orchestration-LLM / ContextOverflow flow that the native path uses.
pub fn decide_claude(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
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

    let config = match load_config_cached() {
        Ok(c) => c,
        Err(err) => {
            log::warn!("[auto_prompt::claude] config load failed: {err}");
            return AutoPromptDecision::NoAction;
        }
    };

    let iteration_count = get_iteration();
    log::info!(
        "[auto_prompt::claude] iteration {iteration_count} / max {}",
        config.max_iterations
    );

    // Safety stop: max iterations reached. There is no pre-stop verification
    // or context-overflow path for Claude.
    if iteration_count > config.max_iterations {
        log::info!(
            "[auto_prompt::claude] Max iterations ({}) reached — stopping",
            config.max_iterations
        );
        let session_id_str = thread.read(cx).session_id().to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return AutoPromptDecision::NoAction;
    }

    // Agent finished its turn normally but didn't use any tools since the last
    // user message. The agent chose to stop on its own — either the task is
    // complete or it's waiting for user input. Continuing here creates an
    // infinite loop where the agent repeatedly says "I'm done" and gets the
    // same nudge back.
    if matches!(stop_reason, acp::StopReason::EndTurn) && !used_tools {
        log::info!(
            "[auto_prompt::claude] EndTurn without tool use — stopping chain \
             (agent appears done or is waiting for user input)"
        );
        let session_id_str = thread.read(cx).session_id().to_string();
        crate::reset_iteration_with_session(&session_id_str);
        return AutoPromptDecision::NoAction;
    }

    // Build the action. `force_new_thread` is always false — Claude must stay
    // in the same thread so it retains its full conversation context.
    let (next_prompt, delay_ms) = match stop_reason {
        // Refusal: retry after a backoff so we don't hammer a refusing model.
        acp::StopReason::Refusal => {
            let delay = config.backoff_delay_ms(iteration_count);
            log::warn!(
                "[auto_prompt::claude] Refusal — retrying in {delay}ms (iteration {iteration_count})"
            );
            (REFUSAL_RETRY_PROMPT.to_string(), Some(delay))
        }
        // EndTurn with tool use, MaxTokens, MaxTurnRequests, etc.: keep going.
        // EndTurn without tools is already handled above.
        _ => (CONTINUATION_PROMPT.to_string(), None),
    };

    let action = build_claude_action(thread, next_prompt, cx);

    match delay_ms {
        Some(delay_ms) => AutoPromptDecision::DispatchAfterDelay {
            action,
            delay_ms,
        },
        None => AutoPromptDecision::DispatchNow(action),
    }
}

/// Construct an `AutoPromptAction` that always continues in the same thread.
///
/// `force_new_thread` is hard-coded to `false` and token fields are zeroed:
/// Claude's dispatch path in `agent_ui` ignores them anyway (it keys off
/// `is_native_agent == false`), and setting them to neutral values makes the
/// intent explicit and guards against future regressions.
fn build_claude_action(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    next_prompt: String,
    cx: &App,
) -> AutoPromptAction {
    let thread_ref = thread.read(cx);
    AutoPromptAction {
        from_session_id: thread_ref.session_id().clone(),
        from_title: thread_ref.title().map(|t| t.to_string()),
        next_prompt,
        work_dirs: thread_ref
            .work_dirs()
            .map(|pl| pl.paths().to_vec()),
        original_user_message: None,
        profile_id: None,
        actual_input_tokens: None,
        approximate_token_count: 0,
        last_assistant_message: None,
        force_new_thread: false,
    }
}
