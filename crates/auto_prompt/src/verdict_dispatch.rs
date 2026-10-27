use std::time::Duration;

use crate::model_call::{call_language_model, is_synthetic_failure};
use crate::remaining_work::{
    PlanRepoFilter, auto_claim_plan, build_checkbox_verification_prompt,
    build_pre_stop_verification_prompt, detect_remaining_plan_tasks, detect_remaining_work,
    llm_acknowledged_all_tasks_blocked,
};
use crate::{
    AutoPromptOutcome, AutoPromptResponse, EvaluationInput, EvaluationResult, LlmCallData,
    WORKING_CONFIDENCE_THRESHOLD, build_lightweight_retry_context, build_plan_correction_prompt,
    chain, detect_plan_skip, is_decisive_stop, is_doc_creation_prompt,
    is_waiting_for_user_decision, load_config_cached, maybe_clarification_request,
    reset_iteration_with_session, should_defer_after_retry_exhaustion, with_first_prompt_context,
};

const SECOND_OPINION_SYSTEM_PROMPT: &str = "# version: second_opinion\n\
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

const LIGHTWEIGHT_RETRY_SYSTEM_PROMPT: &str = "# version: retry\n\
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

const LIGHTWEIGHT_RETRY_ATTEMPTS: u32 = 3;

/// Turns the orchestrator's evaluated verdict into the chain's next outcome.
pub(crate) async fn dispatch_evaluation(
    evaluation: EvaluationResult,
    input: &EvaluationInput,
    data: &LlmCallData,
    prompt_summary: Option<&str>,
    cx: &gpui::AsyncApp,
) -> AutoPromptOutcome {
    let dispatch = Dispatch {
        data,
        prompt_summary,
    };
    match evaluation {
        EvaluationResult::Continue { prompt, reason } => {
            log::info!("[auto_prompt::decide_with_llm] Evaluation: Continue — {reason}");
            dispatch.continue_verdict(prompt)
        }
        EvaluationResult::NeedsSecondOpinion {
            extracted_section,
            rule_reason,
        } => {
            log::info!(
                "[auto_prompt::decide_with_llm] Evaluation: NeedsSecondOpinion — {rule_reason}"
            );
            dispatch
                .second_opinion(extracted_section, &rule_reason, input, cx)
                .await
        }
        // The full-context call failed (context too large or model error), so
        // there is no real stop decision to verify: ask again with less.
        EvaluationResult::WantsStop { reason } if input.is_synthetic_failure => {
            dispatch.lightweight_retry(reason, cx).await
        }
        EvaluationResult::WantsStop { reason } => dispatch.low_confidence_stop(reason, input),
    }
}

struct Dispatch<'a> {
    data: &'a LlmCallData,
    prompt_summary: Option<&'a str>,
}

impl Dispatch<'_> {
    fn with_context(&self, prompt: String) -> String {
        with_first_prompt_context(
            prompt,
            self.prompt_summary,
            self.data.title.as_deref(),
            self.data.last_assistant_message.as_deref(),
        )
    }

    fn continue_claiming_plan(&self, prompt: String) -> AutoPromptOutcome {
        let next_prompt = self.with_context(prompt);
        auto_claim_plan(
            &next_prompt,
            &self.data.context_json,
            &self.data.session_id,
            self.data.title.as_deref(),
        );
        self.data.make_continue(next_prompt)
    }

    fn stop(&self, reason: String) -> AutoPromptOutcome {
        reset_iteration_with_session(&self.data.session_id.to_string());
        AutoPromptOutcome::Stopped { reason }
    }

    fn continue_verdict(&self, prompt: String) -> AutoPromptOutcome {
        let prompt = if is_doc_creation_prompt(&prompt) {
            match build_checkbox_verification_prompt(&self.data.context_json) {
                Some(verification_prompt) => {
                    log::info!(
                        "auto_prompt: plan has unchecked items, overriding doc creation with checkbox verification"
                    );
                    verification_prompt
                }
                None => prompt,
            }
        } else if let Some(lowest_plan) = detect_plan_skip(&prompt, &self.data.context_json) {
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
        self.continue_claiming_plan(prompt)
    }

    async fn second_opinion(
        &self,
        extracted_section: String,
        rule_reason: &str,
        input: &EvaluationInput,
        cx: &gpui::AsyncApp,
    ) -> AutoPromptOutcome {
        let context = format!(
            "## Main LLM decision\n- confidence: low (below threshold)\n- reason: {}\n\n\
             ## Pattern detection\n- rule: {}\n\n\
             ## Extracted section\n{}\n\n\
             ## Last assistant message (for context)\n{}",
            input.reason.as_deref().unwrap_or("(none)"),
            rule_reason,
            extracted_section,
            self.data
                .last_assistant_message
                .as_deref()
                .unwrap_or("(none)"),
        );

        match call_language_model(&self.data.model, SECOND_OPINION_SYSTEM_PROMPT, &context, cx)
            .await
        {
            Ok((_raw, response)) => {
                let confidence = response.confidence.unwrap_or(0.5);
                if confidence >= WORKING_CONFIDENCE_THRESHOLD {
                    log::info!(
                        "[auto_prompt::decide_with_llm] Second opinion: Continue (confidence={confidence:.2}) — {:?}",
                        response.reason
                    );
                    self.continue_claiming_plan(extracted_section)
                } else {
                    let stop_reason = format!(
                        "second opinion confirmed stop (confidence={confidence:.2}): {}",
                        response.reason.as_deref().unwrap_or("no reason given")
                    );
                    log::info!("[auto_prompt::decide_with_llm] {stop_reason}");
                    self.stop(stop_reason)
                }
            }
            Err(err) => {
                let stop_reason =
                    format!("second opinion LLM call failed: {err:#} — defaulting to stop");
                log::warn!("[auto_prompt::decide_with_llm] {stop_reason}");
                self.stop(stop_reason)
            }
        }
    }

    async fn lightweight_retry(&self, reason: String, cx: &gpui::AsyncApp) -> AutoPromptOutcome {
        log::info!(
            "[auto_prompt::decide_with_llm] Building lightweight retry context — last_assistant_message={} chars, has_title={}, reason for WantsStop: {}",
            self.data
                .last_assistant_message
                .as_ref()
                .map(|m| m.len())
                .unwrap_or(0),
            self.data.title.is_some(),
            reason
        );
        let context = build_lightweight_retry_context(
            &self.data.context_json,
            self.data.last_assistant_message.as_deref(),
            self.data.title.as_deref(),
        );
        log::info!(
            "[auto_prompt::decide_with_llm] Lightweight retry context built ({} chars):\n---\n{}\n---",
            context.len(),
            context.chars().take(800).collect::<String>()
        );

        match self.call_lightweight_retry(&context, cx).await {
            Some(parsed) if parsed.confidence.unwrap_or(0.0) >= WORKING_CONFIDENCE_THRESHOLD => {
                let prompt = parsed
                    .next_prompt
                    .unwrap_or_else(|| "Continue with remaining work.".to_string());
                self.continue_claiming_plan(prompt)
            }
            Some(parsed) => {
                let stop_reason = parsed
                    .reason
                    .unwrap_or_else(|| "lightweight retry says stop".to_string());
                log::info!("auto_prompt: lightweight retry says stop: {stop_reason}");
                self.safety_net_continue("after the retry said stop")
                    .unwrap_or_else(|| self.stop(stop_reason))
            }
            None if should_defer_after_retry_exhaustion(self.data.had_api_error) => {
                // The worker's own turn failed at the API level
                // (`had_api_error`: 405/429/5xx from the provider) and every
                // orchestration retry just failed too — the API is down. The
                // safety nets below would "continue" from the STALE last
                // assistant message straight back into the same blocked API:
                // an infinite send → 405 → decide → send loop (~7 doomed
                // requests per cycle) that also keeps the provider's WAF block
                // alive. Defer instead — the unified retry loop in
                // `run_auto_prompt` applies backoff against the shared failure
                // budget and converts to Stopped once max_llm_retries is spent.
                log::warn!(
                    "auto_prompt: all lightweight retries failed with had_api_error — deferring with backoff instead of safety-net continue"
                );
                AutoPromptOutcome::RetryAfterBackoff {
                    delay_ms: load_config_cached().unwrap_or_default().backoff_delay_ms(1),
                    reason: format!("source turn API error + all retries failed: {reason}"),
                }
            }
            None => self
                .safety_net_continue("after all retries failed")
                .unwrap_or_else(|| self.stop(format!("lightweight retry failed: {reason}"))),
        }
    }

    /// Returns the first usable lightweight-retry verdict; `None` when every
    /// attempt errored or came back as a synthetic failure.
    async fn call_lightweight_retry(
        &self,
        context: &str,
        cx: &gpui::AsyncApp,
    ) -> Option<AutoPromptResponse> {
        for attempt in 1..=LIGHTWEIGHT_RETRY_ATTEMPTS {
            if attempt > 1 {
                let delay = 2000 * 2u64.pow(attempt - 1);
                log::info!("auto_prompt: lightweight retry attempt {attempt}, waiting {delay}ms");
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
            }
            match call_language_model(
                &self.data.model,
                LIGHTWEIGHT_RETRY_SYSTEM_PROMPT,
                context,
                cx,
            )
            .await
            {
                Ok((_raw, parsed)) => {
                    if is_synthetic_failure(&parsed) {
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
                    return Some(parsed);
                }
                Err(err) => {
                    log::warn!("auto_prompt: lightweight retry attempt {attempt} failed: {err:#}");
                }
            }
        }
        log::warn!("auto_prompt: all {LIGHTWEIGHT_RETRY_ATTEMPTS} lightweight retries failed");
        None
    }

    /// Before a retry-path stop is accepted: continue with remaining work the
    /// last message still lists, else with unchecked plan tasks.
    fn safety_net_continue(&self, situation: &str) -> Option<AutoPromptOutcome> {
        if let Some(remaining_prompt) =
            detect_remaining_work(self.data.last_assistant_message.as_deref())
        {
            log::warn!(
                "auto_prompt: SAFETY NET OVERRIDE — detect_remaining_work found actionable work {situation}. Extracted prompt:\n---\n{}\n---",
                remaining_prompt.chars().take(500).collect::<String>()
            );
            return Some(self.continue_claiming_plan(remaining_prompt));
        }
        if let Some(plan_prompt) = detect_remaining_plan_tasks(
            &self.data.context_json,
            PlanRepoFilter::All,
            self.data.work_dirs.as_deref(),
        ) {
            log::warn!(
                "auto_prompt: PLAN TASK FALLBACK — plan files have unchecked tasks {situation}"
            );
            return Some(self.continue_claiming_plan(plan_prompt));
        }
        log::info!(
            "auto_prompt: all safety nets exhausted {situation} — no remaining work patterns and no unchecked plan tasks"
        );
        None
    }

    fn low_confidence_stop(&self, reason: String, input: &EvaluationInput) -> AutoPromptOutcome {
        if is_decisive_stop(input) {
            log::info!(
                "auto_prompt: decisive stop (confidence={:?}), skipping verification",
                input.confidence
            );
            return self.stop(reason);
        }
        if is_waiting_for_user_decision(self.data.last_assistant_message.as_deref()) {
            // Worker explicitly deferred a decision to the user (e.g. "I won't
            // pick for you", "you decide", "need your input"). Any further
            // nudge will produce the same question, so stop cleanly and
            // surface the worker's message to the user. Skipping pre-stop
            // verification here is the whole point — otherwise we inject
            // PRE-STOP VERIFICATION noise that ends in a `stopping`
            // declaration anyway (see plan/bug on Plan 456 close-out where
            // this fired needlessly).
            log::info!(
                "auto_prompt: worker is waiting for user decision — stopping without verification"
            );
            return self.stop(reason);
        }
        if let Some(clarification) = maybe_clarification_request(self.data) {
            // Plan 023 D (req 5): low confidence + the worker's last message
            // presents options without a pros/cons layout — ask for the
            // trade-off analysis once instead of stopping ambiguously.
            // (User-deferral / genuine user-input cases were already handled
            // by `is_waiting_for_user_decision` above.)
            log::warn!(
                "auto_prompt: low confidence + unresolved decision point — requesting pros/cons clarification (once per chain)"
            );
            return clarification;
        }

        // Before accepting stop, check plan files for unchecked tasks. If the
        // LLM explicitly declared ALL tasks blocked (not just some), respect
        // that assessment.
        if let Some(plan_prompt) = detect_remaining_plan_tasks(
            &self.data.context_json,
            PlanRepoFilter::All,
            self.data.work_dirs.as_deref(),
        ) {
            if llm_acknowledged_all_tasks_blocked(self.data.last_assistant_message.as_deref()) {
                log::info!(
                    "auto_prompt: LLM declared all remaining tasks blocked, respecting stop decision"
                );
            } else {
                log::warn!(
                    "auto_prompt: PLAN TASK FALLBACK — confidence below threshold but plan files have unchecked tasks, continuing"
                );
                return self.continue_claiming_plan(plan_prompt);
            }
        }

        self.pre_stop_verification(&reason)
    }

    fn pre_stop_verification(&self, reason: &str) -> AutoPromptOutcome {
        let session_id = self.data.session_id.to_string();
        let verification_count = chain::verification_attempts(&session_id);
        let max_verifications = self.data.max_verification_attempts;

        if verification_count >= max_verifications {
            let stop_reason = format!("max verification attempts ({max_verifications}) exceeded");
            log::warn!("auto_prompt: {stop_reason}");
            return self.stop(stop_reason);
        }

        let attempt = verification_count + 1;
        log::info!(
            "auto_prompt: WantsStop ('{reason}') — initiating pre-stop verification (attempt {attempt}/{max_verifications})"
        );
        chain::record_verification_attempt(&session_id);

        match build_pre_stop_verification_prompt(&self.data.context_json, &self.data.work_dirs) {
            Some(verification_prompt) => {
                log::info!(
                    "auto_prompt: dispatching pre-stop verification prompt: {}...",
                    verification_prompt.chars().take(80).collect::<String>()
                );
                self.data
                    .make_continue(self.with_context(verification_prompt))
            }
            None => {
                log::info!("auto_prompt: no verification needed (no plan files found), stopping");
                self.stop(
                    "confidence below threshold, no plan files found for verification".to_string(),
                )
            }
        }
    }
}
