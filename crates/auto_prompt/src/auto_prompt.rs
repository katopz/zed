//! Auto-prompt module: intercepts AI stop events, calls a configured LLM
//! via Zed's built-in language model infrastructure, and decides whether
//! a follow-up prompt should be dispatched.
//!
//! This crate contains the decision logic only. The caller (agent_ui)
//! handles the actual GPUI action dispatch.

mod config;
pub mod context;

pub use config::AutoPromptConfig;
pub use context::{AutoPromptContext, AutoPromptResponse, PlanFileContent};

use agent_client_protocol as acp;
use anyhow::Context as _;
use futures::{StreamExt, future, pin_mut};
use gpui::App;
use language_model::{
    LanguageModel, LanguageModelCompletionEvent, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Seconds of inactivity before an auto-prompt chain is considered stale.
const CHAIN_TIMEOUT_SECS: u64 = 300;

/// Iteration counter for the current auto-prompt chain.
static AUTO_PROMPT_ITERATION: AtomicU32 = AtomicU32::new(0);

/// UNIX timestamp of the last auto-prompt iteration.
static LAST_ITERATION_SECS: AtomicU64 = AtomicU64::new(0);

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
pub struct LlmCallData {
    pub model: Arc<dyn LanguageModel>,
    pub system_prompt: String,
    pub context_json: String,
    pub forced_prompt: Option<String>,
    pub session_id: acp::SessionId,
    pub title: Option<String>,
    pub iteration_count: u32,
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
            .field("forced_prompt", &self.forced_prompt)
            .field("session_id", &self.session_id)
            .field("title", &self.title)
            .field("iteration_count", &self.iteration_count)
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
    stop_reason: &acp::StopReason,
    cx: &App,
) -> AutoPromptDecision {
    log::info!("[auto_prompt::decide] Starting decision process");

    let config = match load_config_cached() {
        Ok(c) => {
            log::info!("[auto_prompt::decide] Config loaded: enabled={}", c.enabled);
            c
        }
        Err(err) => {
            log::warn!("[auto_prompt::decide] config load failed: {err}");
            return AutoPromptDecision::NoAction;
        }
    };

    if !config.enabled {
        log::info!("[auto_prompt::decide] Auto-prompt is DISABLED in config");
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Auto-prompt is ENABLED");

    if !used_tools {
        log::info!("[auto_prompt::decide] No tools were used, skipping auto-prompt");
        return AutoPromptDecision::NoAction;
    }

    log::info!("[auto_prompt::decide] Tools were used, continuing evaluation");

    if matches!(stop_reason, acp::StopReason::Cancelled) {
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
    let Some(configured_model) = registry
        .thread_summary_model()
        .or_else(|| registry.default_model())
    else {
        log::warn!("[auto_prompt::decide] No language model configured in Zed");
        return AutoPromptDecision::NoAction;
    };
    let model = configured_model.model;
    log::info!("[auto_prompt::decide] Using model: {:?}", model.id());

    let (auto_prompt_ctx, session_id, thread_title) = {
        let thread_ref = thread.read(cx);
        let stop_reason_str = format!("{stop_reason:?}").to_lowercase();
        let plan_files = read_plan_files(thread_ref);
        let doc_files = read_doc_files(thread_ref);
        let ctx = AutoPromptContext::collect(
            thread_ref,
            cx,
            stop_reason_str,
            plan_files,
            doc_files,
            iteration_count,
        );
        let sid = thread_ref.session_id().clone();
        let title = thread_ref.title().map(|t| t.to_string());
        (ctx, sid, title)
    };

    let make_action = |prompt: String| AutoPromptAction {
        from_session_id: session_id.clone(),
        from_title: thread_title.clone(),
        next_prompt: prompt,
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
        return AutoPromptDecision::DispatchNow(make_action("continue".to_string()));
    }

    log::info!(
        "[auto_prompt::decide] Had error: {}",
        auto_prompt_ctx.had_error
    );

    if auto_prompt_ctx.had_error
        || matches!(
            stop_reason,
            acp::StopReason::Refusal | acp::StopReason::MaxTokens
        )
    {
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
            action: make_action("continue".to_string()),
            delay_ms: delay,
        };
    }

    let forced_prompt = if auto_prompt_ctx.last_message_indicates_completion() {
        log::info!("[auto_prompt::decide] Last message indicates completion");
        let (pending, in_progress, completed) = auto_prompt_ctx.plan_stats();
        Some(format!(
            "The AI indicates the task is complete. Verify against the plan:\n\
             - Plan stats: {pending} pending, {in_progress} in progress, {completed} completed\n\
             - Check current_plan against plan_files from .plan/ folder\n\
             - If plan_files have steps that are done but still marked [ ] AND remaining pending steps exist, your next_prompt must update checkboxes AND continue the next pending step in the same prompt.\n\
             - If ALL items are done AND no checkboxes need fixing AND doc_files is empty, set next_prompt to create documentation at .doc/ summarizing what was implemented\n\
             - If ALL items are done AND doc_files already exist, respond with #ALL_PLAN_DONE\n\
             - If items remain (and checkboxes are correct), continue with the next pending item"
        ))
    } else if auto_prompt_ctx.last_message_is_question() {
        log::info!("[auto_prompt::decide] Last message is a question");
        let (pending, in_progress, completed) = auto_prompt_ctx.plan_stats();
        let remaining = auto_prompt_ctx.remaining_plan_files();
        let has_plan_context = !auto_prompt_ctx.plan_files.is_empty();
        let has_remaining_steps = !remaining.is_empty() || pending > 0 || in_progress > 0;
        Some(format!(
            "The AI asked a question (possibly with multiple options). Project state:\n\
             - Plan stats: {pending} pending, {in_progress} in progress, {completed} completed\n\
             - Plan files exist: {has_plan_context}\n\
             - Remaining plan files with unchecked steps: {}\n\
             - Has remaining work: {has_remaining_steps}\n\
             \n\
             Rules (apply in order):\n\
             1. If plan_files exist with unchecked [ ] steps → set should_continue=true, \
             next_prompt to the next unchecked step. Confidence must be >= 0.7.\n\
             2. If the question is a permission/confirmation (\"proceed?\", \"go ahead?\", \
             \"continue?\") → answer yes. Set should_continue=true, confidence >= 0.7.\n\
             3. If the question lists numbered or lettered options → pick option 1 \
             (\"continue\"/\"proceed\"/the first listed option). Set should_continue=true, \
             confidence >= 0.6, next_prompt to execute that option.\n\
             4. If no plan and no clear default but the conversation has work in progress → \
             set should_continue=true with next_prompt=\"continue\" and confidence >= 0.5.\n\
             5. ONLY set should_continue=false if the question requires external information \
             you cannot infer (API keys, credentials, user preferences never mentioned before).\n\
             \n\
             NEVER stop just because plan_files is empty. ALWAYS pick a default and continue.",
            remaining.len(),
        ))
    } else {
        log::info!("[auto_prompt::decide] Normal state, will call LLM for decision");
        None
    };

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

    log::info!("[auto_prompt::decide] Returning NeedsLlmCall decision");
    AutoPromptDecision::NeedsLlmCall(LlmCallData {
        model,
        system_prompt,
        context_json,
        forced_prompt,
        session_id,
        title: thread_title,
        iteration_count,
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

    log::info!(
        "[auto_prompt::decide_with_llm] Forced prompt: {:?}",
        data.forced_prompt
    );

    let result =
        call_language_model(&data.model, &data.system_prompt, &data.context_json, cx).await;

    log::info!(
        "[auto_prompt::decide_with_llm] LLM call completed with result: {:?}",
        result.is_ok()
    );

    match result {
        Ok(response) => {
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

            let all_done = response.all_plan_done
                || response
                    .next_prompt
                    .as_ref()
                    .is_some_and(|p| p.contains("#ALL_PLAN_DONE"));

            if all_done {
                if let Some(next_plan_prompt) = find_next_plan_prompt(&data.context_json) {
                    log::info!(
                        "[auto_prompt::decide_with_llm] Current plan done, transitioning to next plan"
                    );
                    return Ok(Some(AutoPromptAction {
                        from_session_id: data.session_id,
                        from_title: data.title,
                        next_prompt: next_plan_prompt,
                    }));
                }

                log::info!(
                    "[auto_prompt::decide_with_llm] #ALL_PLAN_DONE detected, all plans complete, stopping chain"
                );
                reset_iteration();
                return Ok(None);
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
                log::info!("auto_prompt: LLM says stop, no next_prompt");
                reset_iteration();
                return Ok(None);
            }

            let next_prompt = if has_prompt {
                let prompt = response.next_prompt.unwrap();
                prompt.replace("#ALL_PLAN_DONE", "").trim().to_string()
            } else if let Some(forced) = data.forced_prompt {
                forced
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

            log::info!(
                "auto_prompt: dispatching new thread with prompt: {}...",
                next_prompt.chars().take(80).collect::<String>()
            );

            Ok(Some(AutoPromptAction {
                from_session_id: data.session_id,
                from_title: data.title,
                next_prompt,
            }))
        }
        Err(err) => {
            log::warn!("auto_prompt: language model call failed: {err}");
            Err(err)
        }
    }
}

fn default_system_prompt() -> String {
    indoc::indoc! {r#"
        You are an orchestration assistant embedded in the Zed editor.
        You receive the full context of a conversation that just finished \
        and decide whether a follow-up action is needed.

        Respond ONLY with valid JSON in this exact format:
        {"should_continue": bool, "next_prompt": string | null, "reason": string | null, "all_plan_done": bool, "confidence": float}

        ## Cases to handle:

        ### Case 1: Starting a new plan (CRITICAL - priority check)
        If is_starting_new_plan is true in context:

        **First, check plan_has_checkboxes in context**:
        - If plan_has_checkboxes is false (narrative format):
          - Generate a REFINE prompt first:
            'Read .plan/{first_plan_filename} and add a task checklist at the top with checkboxes for all **Tasks** and **Deliverables**. Keep all existing content below. Format each item as "- [ ] Task description". Include git branch creation as the first task: "- [ ] Create feature branch feature/{plan_number}_description from develop".'
          - Set should_continue to true
          - DO NOT proceed to implementation yet
        - If plan_has_checkboxes is true:
          - Proceed directly to implementation starting with first [ ] task

        ### Case 2: Plan refinement just completed
        If the last assistant message just finished refining a plan (added checkboxes):
        - Extract the first task from the refined plan
        - Set next_prompt to: 'Start implementing: "- [ ] Create feature branch feature/{plan_number}_description from develop"'
        - Set should_continue to true

        ### Case 3: Task completion detected
        If the last assistant message indicates task completion (e.g. 'all done', 'task complete', 'completed', 'finished'):
        - MANDATORY FIRST ACTION: Mark the just-completed step as [x] in the plan file. Every next_prompt below MUST begin with this checkbox update before any other instruction.
        - Compare current_plan entries against plan_files (the original plan from .plan/ folder)
        - Check the code changes against each plan item to verify completion
        - If ALL items in the CURRENT plan are completed AND verified:
          - Check ALL other plan files in plan_files for remaining [ ] items (multi-plan support)
          - If another plan has unchecked [ ] items, transition to it:
            Set next_prompt to: 'Read .plan/{next_plan_filename} and execute the plan starting from the first unchecked step.'
            Set should_continue to true, keep all_plan_done as false
          - If NO other plans have remaining [ ] items:
            Set all_plan_done to true
            Set next_prompt to: 'Rebase feature branch feature/{plan_number}_description onto develop, then merge to develop'
        - If some items remain in the current plan:
          - Set next_prompt to mark the completed step as [x] then proceed with the next pending [ ] task
          - ALWAYS set should_continue to true to automatically proceed without asking
        - You may include #ALL_PLAN_DONE in next_prompt ONLY when every plan file has all steps [x]
        - NEVER ask the user for permission - automatic execution is required

        ### Case 4: Question detected
        If the last assistant message asks a question (possibly with multiple options):
        - Apply rules in order:
          1. plan_files exist with unchecked [ ] → auto-continue with next step (confidence >= 0.7)
          2. Permission/confirmation question → auto-answer yes (confidence >= 0.7)
          3. Numbered/lettered options → pick option 1 (confidence >= 0.6)
          4. No plan but work in progress → continue (confidence >= 0.5)
          5. Requires external info (keys, credentials, unknown preferences) → stop (confidence < 0.5)
        - NEVER stop just because plan_files is empty. ALWAYS pick a default and continue.
        - The goal is to keep the chain moving unless the question is truly unanswerable.

        ### Case 5: Normal continuation
        If the conversation ended normally without completion or questions:
        - Check if there are pending plan items in current_plan
        - If yes, set next_prompt to proceed with the next pending [ ] task
        - If no plan items remain, check if the overall goal seems achieved
        - If achieved, set should_continue to false

        ## Multi-Plan Execution (MANDATORY):
        - The .plan/ folder may contain MULTIPLE plan files (e.g., 01_core.md, 02_bugfix.md)
        - Plans are executed IN ORDER by filename (01 before 02, etc.)
        - When one plan completes (all [x]), immediately transition to the next plan with [ ] items
        - all_plan_done must be true ONLY when every plan file in .plan/ has all checkboxes [x]
        - The transition prompt must reference the next plan filename: 'Read .plan/{filename} and execute the plan starting from the first unchecked step.'
        - Never stop the chain between plans — keep going until all plans are done

        ## Rules:
        - MANDATORY: Every next_prompt MUST include 'Mark step N as [x] in .plan/{filename}' as the first instruction when a step completes. This is per-step — never defer checkbox updates to a later transition. The agent must update the plan file on disk immediately after each step.
        - Keep next_prompt concise and actionable
        - confidence ranges from 0.0 (not sure at all) to 1.0 (very confident)
        - If confidence < 0.5, always set should_continue to false
        - Permission/confirmation questions MUST be auto-answered yes when plan_files exists
        - When working on plans with checkboxes, ALWAYS move to the next [ ] task without stopping
        - Never repeat the same prompt that was just executed
        - iteration_count tells you how many auto-prompt cycles have run; consider stopping if > 15

        ## Git Flow Automation (always apply):
        - When starting a new feature: Always instruct to create feature/{plan_number}_description branch from develop
        - When starting a hotfix: Always instruct to create hotfix/{plan_number}_description branch from main
        - When feature completes (all tasks [x]): Always instruct to rebase onto develop, then merge to develop
        - When hotfix completes: Always instruct to merge to main AND develop
        - After merging to develop: Generate documentation prompt (if doc_files is empty)
        - Branch naming: Use plan_number from context as prefix
        - Use git rebase: Always prefer rebase over merge for linear history
        - Never force push: To shared branches (main, develop)

        ## Conventional Commits (always apply):
        - feat: for new features
        - fix: for bug fixes
        - refactor: for code restructuring
        - test: for test additions/changes
        - chore: for maintenance tasks
        - docs: for documentation

        ## Plan Status Tracking (MANDATORY - always apply):
        - Plan files live in .plan/ folder (accessible via plan_files in context)
        - There may be MULTIPLE plan files — process them in filename order
        - Plans MUST have a status checklist at the top using checkboxes: [ ] pending, [x] done
        - Check plan_has_checkboxes in context - if false, trigger Case 1 to refine it first
        - NEVER assume the user will manually update checkboxes — the agent must do it
        - Mark each step as [x] IMMEDIATELY when it completes — do not batch or defer checkbox updates. Every next_prompt after a completed step must include the [x] update for that step.
        - When all steps in a plan are [x], check other plan files before setting all_plan_done
        - all_plan_done is true ONLY when every plan file has all steps [x]
        - When suggesting next_prompt for a new task, reference the next [ ] step by number

        ## Documentation Generation (MANDATORY on completion of ALL plans):
        - Documentation is generated ONLY AFTER every plan file in .plan/ is fully complete (all [x])
        - MANDATORY: Before generating any documentation prompt, re-read ALL .plan/ files and verify each '- [ ]' step against the actual code changes. If a step was completed but not marked [x], update it FIRST. Only proceed with documentation when every step in every plan file is verified as [x].
        - When all plans are done and no doc_files exist in context, generate a next_prompt that instructs the agent to create documentation
        - Documentation goes in .doc/ folder in the project root
        - Format: .doc/{NN}_{descriptive_name}.md (use sequential numbering like .plan/ files)
        - Documentation should cover: what was implemented, key architectural decisions, file changes summary, how to test/use the feature
        - When doc_files already exist and all plans are done, set all_plan_done to true
        - The doc creation prompt is the FINAL step before all_plan_done — never skip it
    "#}
    .to_string()
}

fn reset_iteration() {
    AUTO_PROMPT_ITERATION.store(0, Ordering::Relaxed);
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
        let plan_dir = work_dir.join(".plan");
        log::info!(
            "[auto_prompt::read_plan_files] Checking for plan directory: {}",
            plan_dir.display()
        );
        if !plan_dir.is_dir() {
            log::info!("[auto_prompt::read_plan_files] Plan directory does not exist");
            continue;
        }
        log::info!("[auto_prompt::read_plan_files] Found plan directory");

        let entries = match std::fs::read_dir(&plan_dir) {
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
        let doc_dir = work_dir.join(".doc");
        if !doc_dir.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&doc_dir) {
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
) -> anyhow::Result<AutoPromptResponse> {
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
        future::Either::Left((Ok(response_text), _)) => parse_response(&response_text),
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
/// Returns a prompt to start the next plan if found, or None if all plans are complete.
fn find_next_plan_prompt(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ContextPlanFiles {
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: ContextPlanFiles = serde_json::from_str(context_json)
        .inspect_err(|e| {
            log::warn!("[auto_prompt::find_next_plan_prompt] Failed to parse context JSON: {e}");
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
                log::info!("[auto_prompt::find_next_plan_prompt] Found remaining plan: {filename}");
                return Some(format!(
                    "Read .plan/{filename} and execute the plan starting from the first unchecked step."
                ));
            }
        }
    }

    log::info!("[auto_prompt::find_next_plan_prompt] No remaining plans found");
    None
}

fn is_doc_creation_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("documentation") || lower.contains(".doc/")
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
                log::info!(
                    "[auto_prompt::build_checkbox_verification_prompt] Found unchecked items in {filename}"
                );
                return Some(format!(
                    "MANDATORY CHECKPOINT: Verify plan checkboxes before documentation.\n\n\
                     Re-read all .plan/ files and verify every '- [ ]' step against the actual code changes:\n\
                     1. Read each plan file in .plan/\n\
                     2. For each '- [ ]' item, check if the code already implements it\n\
                     3. Mark completed items as '- [x]' — do NOT re-execute completed work\n\
                     4. If any item is truly incomplete, continue working on it\n\
                     5. Only after ALL items in ALL plan files are '- [x]', create documentation at .doc/\n\n\
                     Unchecked items found in: {filename}"
                ));
            }
        }
    }

    None
}
