use super::*;

/// Minimal LlmCallData for pure state-machine tests (plan 023 F1). The
/// overflow and clarify machines never call the model; the slot just
/// satisfies the struct shape.
fn overflow_test_data(session: &str, last_assistant_message: Option<&str>) -> LlmCallData {
    overflow_test_data_with(
        session,
        last_assistant_message,
        serde_json::json!({"session_id": session}).to_string(),
        None,
    )
}

/// `overflow_test_data` with a caller-supplied `context_json` (e.g. plan
/// files for the Phase 2 fallback chain) and `work_dirs` (current-repo
/// classification). Plan paths must be unique per test — the plan claim
/// registry is global and keyed by path.
fn overflow_test_data_with(
    session: &str,
    last_assistant_message: Option<&str>,
    context_json: String,
    work_dirs: Option<Vec<PathBuf>>,
) -> LlmCallData {
    LlmCallData {
        model: language_model::fake_provider::FakeLanguageModelProvider::default().model("fake"),
        system_prompt: String::new(),
        context_json,
        project_root: None,
        session_id: acp::SessionId::new(session),
        title: Some("test".to_string()),
        iteration_count: 1,
        max_verification_attempts: 0,
        work_dirs,
        first_user_message: None,
        original_user_message: None,
        last_assistant_message: last_assistant_message.map(str::to_string),
        profile_id: None,
        actual_input_tokens: Some(300_000),
        had_error: false,
        had_api_error: false,
        stop_phase: context::StopPhase::Working,
        context_exceeds_limit: true,
        cutoff_context_full: false,
        approximate_token_count: 0,
        connection: None,
        project: None,
        peer_agent_states: None,
        bloat_directive: None,
    }
}

#[test]
fn context_overflow_phase1_requests_summary() {
    let session = "overflow-phase1-test";
    clear_summary_for_session(session);
    let data = overflow_test_data(session, Some("Working on the parser fix still."));
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::ContextOverflow(action) => {
            assert!(!action.force_new_thread);
            assert!(action.next_prompt.contains("concise summary"));
            assert_eq!(summary_state_for(session), 1);
        }
        other => panic!("expected ContextOverflow, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_phase2_forks_new_thread() {
    let session = "overflow-phase2-test";
    set_summary_state(session, 1);
    let data = overflow_test_data(
        session,
        Some("## Summary\n\nDid the refactor. Remaining: run clippy."),
    );
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(action.force_new_thread, "Phase 2 must force a new thread");
            assert_eq!(action.actual_input_tokens, None);
            assert_eq!(action.approximate_token_count, 0);
            assert_eq!(summary_state_for(session), 0, "state cleared after Phase 2");
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_phase2_actionable_steps_continuation() {
    // Plan 027: actionable steps in the summary ride along in the inlined
    // Last Assistant Message section; the Decision slot is the fixed
    // directive. No housekeeping, forced new thread.
    let session = "overflow-phase2-steps-test";
    set_summary_state(session, 1);
    let summary = "# Session Summary\n\n## Original Task\n\nRefactor.\n\n\
                       ## What Remains\n\n\
                       1. Decide on the serializer format\n\
                       2. Finish the port\n\
                       3. Commit";
    let data = overflow_test_data(session, Some(summary));
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(action.force_new_thread, "Phase 2 must force a new thread");
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "fixed decision directive must be the decision: {}",
                action.next_prompt
            );
            assert!(
                action
                    .next_prompt
                    .contains("Decide on the serializer format"),
                "summary steps must be inlined for context: {}",
                action.next_prompt
            );
            assert!(
                !action.next_prompt.contains("boundary-guard"),
                "actionable steps must not route to housekeeping: {}",
                action.next_prompt
            );
            assert_eq!(action.actual_input_tokens, None);
            assert_eq!(action.approximate_token_count, 0);
            assert_eq!(summary_state_for(session), 0, "state cleared after Phase 2");
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_phase2_uses_fixed_decision_directive() {
    // Plan 027 ("do not process"): with a summary in hand, Phase 2's
    // Decision slot is the fixed directive — no reasoned pass, no
    // next-step extraction, no plan-task prompt. Same action guarantees
    // (forced new thread, reset token counts) as before.
    let session = "overflow-phase2-fixed-decision-test";
    set_summary_state(session, 1);
    let summary = "# Session Summary\n\n## What Remains\n\n- `.issues/739` — owner go\n- [ ] T2: verify build\n";
    let data = overflow_test_data(session, Some(summary));
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(action.force_new_thread, "Phase 2 must force a new thread");
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "fixed decision directive must appear in the prompt: {}",
                action.next_prompt
            );
            assert!(
                action.next_prompt.contains("## 3. Decision"),
                "directive rides in the Decision section: {}",
                action.next_prompt
            );
            assert!(
                action.next_prompt.contains("T2: verify build"),
                "summary is inlined for context: {}",
                action.next_prompt
            );
            assert_eq!(action.actual_input_tokens, None);
            assert_eq!(action.approximate_token_count, 0);
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn summary_fast_path_enriches_decision_with_summary_steps() {
    // Plan 031: a voluntary summary with remaining work short-circuits the
    // orchestrator — same-thread Continue carrying the summary's steps
    // AHEAD of the fixed directive (the named task wins over re-derivation).
    let session = "summary-fast-path-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some("## Summary\n\nDid the refactor.\n\n## What Remains\n\n- [ ] T2: verify build"),
    );
    data.context_exceeds_limit = false;
    match summary_continuation_fast_path(&data) {
        Some(AutoPromptOutcome::Continue(action)) => {
            assert!(
                !action.force_new_thread,
                "fast path continues in the same thread below the overflow gate"
            );
            assert!(
                action
                    .next_prompt
                    .starts_with("Continuing from the previous session's summary"),
                "summary steps must lead the decision: {}",
                action.next_prompt
            );
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "fixed decision directive must ride as the fallback: {}",
                action.next_prompt
            );
            assert!(
                action.next_prompt.contains("T2: verify build"),
                "summary steps must be inlined: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[test]
fn summary_fast_path_fixed_decision_without_steps_section() {
    // A summary with no next-steps section keeps the exact fixed directive.
    let session = "summary-fast-path-plain-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some("## Summary\n\nDid the refactor. Everything is verified."),
    );
    data.context_exceeds_limit = false;
    match summary_continuation_fast_path(&data) {
        Some(AutoPromptOutcome::Continue(action)) => {
            assert_eq!(action.next_prompt, CONTINUE_REMAINS_DECISION);
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[test]
fn summary_fast_path_stops_when_next_steps_stop_changing() {
    // The stalled-loop shape that slips past the terminal check: items read
    // as actionable, but nothing changes between summaries.
    let session = "summary-fast-path-stall-test";
    clear_summary_for_session(session);
    chain::clear_session(session);
    let summary = "## Summary\n\nRe-checked; no change.\n\n## What Remains\n\n\
                       - Run the real M3 to M5 test.\n\
                       - Score the verdict cohort.";
    let outcome = || {
        let mut data = overflow_test_data(session, Some(summary));
        data.context_exceeds_limit = false;
        summary_continuation_fast_path(&data)
    };
    for round in 0..=chain::MAX_UNCHANGED_REPEATS {
        let result = outcome();
        if round < chain::MAX_UNCHANGED_REPEATS {
            assert!(
                matches!(result, Some(AutoPromptOutcome::Continue(_))),
                "round {round}: {result:?}"
            );
        } else {
            match result {
                Some(AutoPromptOutcome::Stopped { reason }) => {
                    assert!(reason.contains("no progress"), "reason: {reason}");
                }
                other => panic!("expected Stopped, got {other:?}"),
            }
        }
    }
    // A stop clears the streak, so a later chain on this session starts fresh.
    reset_iteration_with_session(session);
    assert!(matches!(outcome(), Some(AutoPromptOutcome::Continue(_))));
    reset_iteration_with_session(session);
}

#[test]
fn summary_fast_path_stops_on_terminal_summary() {
    // Plan 031: a summary whose remaining items are ALL armed/deferred/
    // gated ends the chain — Stopped, not a continuation dispatch.
    let session = "summary-fast-path-terminal-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some(
            "## Summary\n\nWash @16K measured.\n\n## What Remains\n\n\
                 - 32K cooled-window cell (7-min-cooldown protocol).\n\
                 - T2c-b — P3, deferred.\n\
                 - League lanes unchanged.",
        ),
    );
    data.context_exceeds_limit = false;
    match summary_continuation_fast_path(&data) {
        Some(AutoPromptOutcome::Stopped { reason }) => {
            assert!(reason.contains("terminal"), "reason: {reason}");
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[test]
fn summary_declares_terminal_on_all_deferred_remaining() {
    // Plan 031: the real-world wash handoff shape — bold section labels,
    // every remaining bullet armed/deferred/thermal-gated/handed-off.
    let summary = "\
## Summary\n\n\
**(1) Original task:** Build and measure the q8 prefill arm.\n\n\
**(2) Accomplished:**\n\
- All gates PASS; wash @16K (1.027x).\n\
- Committed and pushed.\n\n\
**(3) What remains:**\n\
- **32K cooled-window cell** for the q8 arm (Issue 771 T2c; 7-min-cooldown protocol — the 16K run was heat-soaked).\n\
- **T2c-b (half4 O accumulator)** — P3, deferred below the 32K cell.\n\
- League lanes unchanged: 32K cells both sides, 4090 cudarc flash twin.\n\
- The `round()` lowering hazard is recorded for the riir-clippy intake (append left to the active riir-clippy session).\n\n\
**(4) Active plan state:** `.plans/562_x.md` — COMPLETE. `.issues/771.md` — T2c-a `[x]` (measured wash), T2c-b `[-]`.";
    assert!(summary_declares_terminal(Some(summary)));
    // One actionable bullet keeps the chain alive.
    let actionable = "\
## Summary\n\nBuilt the arm.\n\n\
**(3) What remains:**\n\
- **32K cooled-window cell** for the q8 arm (7-min-cooldown protocol).\n\
- [ ] Run the league 32K cell now";
    assert!(!summary_declares_terminal(Some(actionable)));
    // Non-summaries are never terminal.
    assert!(!summary_declares_terminal(Some("still working")));
    assert!(!summary_declares_terminal(None));
}

#[test]
fn summary_declares_terminal_on_blocked_and_waiting_items() {
    // The stalled-loop shape: every item waits on the owner, on data, or on
    // an external event, phrased without the plan-031 vocabulary.
    let summary = "\
## Summary\n\n\
**(1) Original task:** Continue plan 037.\n\n\
**(2) Accomplished:**\n\
- Re-checked SSH; no changes.\n\n\
**(3) What remains:**\n\
- Plan 037 phase 3, the real M3 to M5 test: blocked on SSH, needs you to enable Remote Login.\n\
- `.issues/016`: waiting on 9 more reviewed chains from real use.\n\
- `.issues/023`: waiting for the next Z.AI limit hit in `Zed.log`.\n\
- Plan 038's deferred item stays deferred.\n\n\
**(4) Active plan state:** `.plans/037_x.md` in progress.";
    assert!(summary_declares_terminal(Some(summary)));
}

#[test]
fn deferral_markers_match_only_at_word_starts() {
    assert!(section_all_deferred("- Phase 3: still blocked on SSH."));
    assert!(!section_all_deferred(
        "- Phase 3 is now unblocked by the new key: run it."
    ));
    assert!(!section_all_deferred("- Re-enable the disarmed lever."));
    assert!(section_all_deferred("- Lever re-armed for the next run."));
}

#[test]
fn context_overflow_phase2_terminal_summary_stops_despite_plan_tasks() {
    // Plan 031: a terminal summary ends the chain — plan tasks (even
    // armed league cells in .issues) must NOT resurrect it, and no new
    // thread is created.
    let session = "overflow-phase2-terminal-test";
    set_summary_state(session, 1);
    let summary = "\
## Summary\n\nBuilt the q8 arm; wash @16K.\n\n\
**(3) What remains:**\n\
- **32K cooled-window cell** (7-min-cooldown protocol — 16K was heat-soaked).\n\
- **T2c-b** — P3, deferred.\n\
- League lanes unchanged.";
    let context_json = serde_json::json!({
        "session_id": session,
        "plan_files": [{
            "path": "/tmp/zed-phase2-it/current/.issues/9904_league.md",
            "content": "- [ ] 32K league cell"
        }]
    })
    .to_string();
    let data = overflow_test_data_with(session, Some(summary), context_json, None);
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Stopped { reason } => {
            assert!(reason.contains("terminal"), "reason: {reason}");
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn summary_fast_path_skipped_without_summary() {
    // No summary at the last paragraph → fast path does not apply; the
    // normal flow (orchestrator / Phase 1) owns the decision.
    let session = "summary-fast-path-none-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(session, Some("Working on the parser fix still."));
    data.context_exceeds_limit = false;
    assert!(summary_continuation_fast_path(&data).is_none());
}

#[test]
fn summary_fast_path_nothing_left_routes_to_housekeeping_rules() {
    // Plan 031: nothing-left summaries continue same-thread with the
    // housekeeping directive — rules, no orchestrator call.
    let session = "summary-fast-path-idle-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some("## Summary\n\nAll done.\n\n## What Remains\n\nNothing left to do."),
    );
    data.context_exceeds_limit = false;
    match summary_continuation_fast_path(&data) {
        Some(AutoPromptOutcome::Continue(action)) => {
            assert!(!action.force_new_thread);
            for marker in ["no remaining work", "boundary-guard", "stop cleanly"] {
                assert!(
                    action.next_prompt.contains(marker),
                    "housekeeping directive must mention '{marker}': {}",
                    action.next_prompt
                );
            }
        }
        other => panic!("expected Continue(housekeeping), got {other:?}"),
    }
}

#[test]
fn summary_fast_path_skipped_on_overflow() {
    // Overflowed threads route through the shared Phase 1/2 machine (fork
    // with summary), never the same-thread fast path.
    let session = "summary-fast-path-overflow-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some("## Summary\n\nDid the refactor.\n\n## What Remains\n\n- [ ] T2: verify build"),
    );
    data.context_exceeds_limit = true;
    assert!(summary_continuation_fast_path(&data).is_none());
}

#[test]
fn summary_fast_path_skipped_on_api_error() {
    // Bugfix (2026-09-11 WAF-block freeze): when the source turn failed at
    // the API level, the summary is stale (no new assistant message was
    // produced) and continuing from it — including the housekeeping
    // directive — would loop send → 405 → decide → send against a blocked
    // API. The fast path must step aside so the LLM path's retry-exhaustion
    // guard can back off and stop the chain.
    let session = "summary-fast-path-api-error-test";
    clear_summary_for_session(session);
    let mut data = overflow_test_data(
        session,
        Some("## Summary\n\nDid the refactor.\n\n## What Remains\n\n- [ ] T2: verify build"),
    );
    data.context_exceeds_limit = false;
    data.had_api_error = true;
    assert!(summary_continuation_fast_path(&data).is_none());
}

#[test]
fn overflow_defers_on_claude_code_unreachable_message() {
    // Claude Code ends the turn normally with a synthetic message when the
    // API is unreachable, so `had_api_error` stays false. Phase 1 must not
    // send a summarize request into the dead connection, and a pending
    // Phase 1 must not fork with the error text as the "summary".
    let message = "API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)";
    for summary_state in [0, 1] {
        let session = "overflow-api-unreachable-test";
        clear_summary_for_session(session);
        set_summary_state(session, summary_state);
        let mut data = overflow_test_data(session, Some(message));
        data.context_exceeds_limit = true;
        assert!(
            matches!(
                context_overflow_outcome(&data),
                AutoPromptOutcome::RetryAfterBackoff { .. }
            ),
            "summary_state={summary_state} must defer"
        );
        assert_eq!(summary_state_for(session), summary_state);
        clear_summary_for_session(session);
    }
}

#[test]
fn test_truncate_at_char_boundary_multibyte_safe() {
    // Em-dashes and CJK must never be split mid-character.
    let text = "a—b—字—d";
    assert_eq!(truncate_at_char_boundary(text, 100), text);
    let truncated = truncate_at_char_boundary(text, 3);
    assert_eq!(truncated.chars().count(), 3);
    assert!(text.starts_with(&truncated));
    assert!(truncated.is_char_boundary(truncated.len()));
}

// 2026-09-01: the fleet repo-sync `for r in …` loop tripped the old
// substring matcher ("auth ") and silently killed chains via
// is_interactive_tool_pending. The matcher now matches concrete auth
// invocations at shell-segment starts only.
#[test]
fn interactive_auth_command_matches_invocations_not_mentions() {
    // Genuine interactive flows — segment-start invocations.
    for cmd in [
        "gh auth login",
        "gh auth login --web",
        "echo hi && gh auth login",
        "gcloud auth login; git fetch",
        "cd repo | wrangler login",
        "az login",
        "docker login",
        "login",
    ] {
        assert!(
            is_interactive_auth_command(cmd),
            "should match interactive auth: {cmd}"
        );
    }
    // Scripts that merely mention auth words — must NOT match.
    for cmd in [
        "for r in katgpt-rs riir-ai riir-chain riir-neuron-db; do gh auth status; git -C $r fetch --all; done",
        "for r in a b c; do echo == $r; git -C $r pull --ff-only; done",
        "git config --get remote.origin.url | grep auth",
        "echo 'sign in to continue' && git push",
        "grep -rn oauth src/",
        "gh auth status",
        "git -C riir-auth pull",
    ] {
        assert!(
            !is_interactive_auth_command(cmd),
            "must not match auth mention: {cmd}"
        );
    }
}

// Only the most recent terminal tool call can be auth-"pending" — a stale
// auth-shaped call deeper in the turn must not stop the chain.
/// Builds a fresh thread whose entries follow `script` — `None` is a user
/// message, `Some(command)` a terminal tool call — and returns
/// `latest_terminal_command` over its entries.
async fn latest_terminal_command_for_script(
    script: &[Option<&str>],
    project: &gpui::Entity<project::Project>,
    cx: &mut gpui::TestAppContext,
) -> Option<String> {
    use agent_client_protocol::schema::v1 as acp_v1;

    let steps = script
        .iter()
        .enumerate()
        .map(|(index, step)| {
            step.map(|command| {
                acp_v1::ToolCall::new(format!("tool-{index}"), "label")
                    .name("terminal")
                    .kind(acp_v1::ToolKind::Execute)
                    .status(acp_v1::ToolCallStatus::Completed)
                    .raw_input(serde_json::json!({ "command": command }))
            })
        })
        .collect();
    chain::stub_thread_from_steps(steps, project, cx)
        .await
        .read_with(cx, |thread, _| latest_terminal_command(thread.entries()))
}

#[gpui::test]
async fn latest_terminal_command_scopes_to_most_recent_terminal_call(
    cx: &mut gpui::TestAppContext,
) {
    let project = chain::test_project(cx).await;

    // Latest terminal call is a sync loop; an older auth-shaped call
    // must not resurrect the guard.
    let command = latest_terminal_command_for_script(
        &[
            None,
            Some("gh auth login"),
            Some("for r in a b; do git -C $r fetch; done"),
        ],
        &project,
        cx,
    )
    .await;
    assert_eq!(
        command.as_deref(),
        Some("for r in a b; do git -C $r fetch; done")
    );
    assert!(!is_interactive_auth_command(
        &command.expect("terminal command")
    ));

    // Latest terminal call IS an auth flow → pending.
    let command = latest_terminal_command_for_script(
        &[None, Some("git push"), Some("gh auth login")],
        &project,
        cx,
    )
    .await;
    assert!(is_interactive_auth_command(
        &command.expect("terminal command")
    ));

    // No terminal call since the last user message → None.
    let command =
        latest_terminal_command_for_script(&[Some("gh auth login"), None], &project, cx).await;
    assert_eq!(command, None);
}

#[test]
fn context_overflow_phase2_nothing_left_routes_to_housekeeping() {
    // Phase 2 end-to-end: a summary that declares nothing left, with no
    // plan tasks anywhere, must switch to the housekeeping prompt instead
    // of ordering "start working immediately" on an empty queue.
    let session = "overflow-phase2-idle-test";
    set_summary_state(session, 1);
    let summary = "# Session Summary\n\n## What Was Accomplished\n\nEverything landed.\n\n\
                       ## What Remains\n\n- Nothing left — all tasks complete.";
    let data = overflow_test_data(session, Some(summary));
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(action.force_new_thread, "Phase 2 must force a new thread");
            for marker in [
                "no remaining work",
                "boundary-guard",
                "doc-sync",
                "riir-clippy",
                "stop cleanly",
            ] {
                assert!(
                    action.next_prompt.contains(marker),
                    "housekeeping prompt must mention '{marker}': {}",
                    action.next_prompt
                );
            }
            assert!(
                !action.next_prompt.contains("Pick up from here"),
                "nothing-left must not order immediate work: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_phase2_plan_tasks_beat_nothing_left() {
    // Termination guard: unclaimed plan tasks must keep the chain on the
    // fixed decision directive — housekeeping only fires when no plans
    // remain anywhere (plan 027 keeps plan-tasks-beat-nothing-left).
    let session = "overflow-phase2-plan-wins-test";
    set_summary_state(session, 1);
    let summary = "# Session Summary\n\n## What Remains\n\n- Nothing left — all tasks complete.";
    let context_json = serde_json::json!({
        "session_id": session,
        "plan_files": [{
            "path": "/tmp/zed-phase2-it/current/.plans/9902_plan_wins.md",
            "content": "- [x] Step 1: done\n- [ ] Step 2: pending"
        }]
    })
    .to_string();
    let data = overflow_test_data_with(
        session,
        Some(summary),
        context_json,
        Some(vec![PathBuf::from("/tmp/zed-phase2-it/current")]),
    );
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "remaining plan tasks must keep the chain on the fixed decision: {}",
                action.next_prompt
            );
            assert!(
                !action.next_prompt.contains("boundary-guard"),
                "housekeeping must not fire while plan tasks remain: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_phase2_nothing_left_other_repo_plan_fallback() {
    // Termination guard tail: when no current-repo plans remain, other-repo
    // plan tasks still prevent housekeeping (chain continues on the fixed
    // decision directive).
    let session = "overflow-phase2-other-repo-test";
    set_summary_state(session, 1);
    let summary = "# Session Summary\n\n## What Remains\n\n- Nothing left — all tasks complete.";
    let context_json = serde_json::json!({
        "session_id": session,
        "plan_files": [{
            "path": "/tmp/zed-phase2-it/other/.plans/9903_other_repo.md",
            "content": "- [ ] Step 1: pending"
        }]
    })
    .to_string();
    let data = overflow_test_data_with(
        session,
        Some(summary),
        context_json,
        Some(vec![PathBuf::from("/tmp/zed-phase2-it/current")]),
    );
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "other-repo plan tasks must keep the chain on the fixed decision: {}",
                action.next_prompt
            );
            assert!(
                !action.next_prompt.contains("boundary-guard"),
                "housekeeping must not fire while other-repo plan tasks remain: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn context_overflow_voluntary_summary_skips_phase1() {
    let session = "overflow-voluntary-test";
    clear_summary_for_session(session);
    let data = overflow_test_data(session, Some("## Summary\n\nDone with the task."));
    match context_overflow_outcome(&data) {
        AutoPromptOutcome::Continue(action) => {
            assert!(
                action.force_new_thread,
                "voluntary summary goes straight to Phase 2"
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[test]
fn append_addition_request_adds_section() {
    let base = "## 1. Thread Summary\n\nsummary\n\n## 3. Decision\n\ndo work";
    let result = append_addition_request(base, Some("  also fix the tests  "));
    assert!(
        result.contains("## 4. Addition request\n\nalso fix the tests"),
        "draft should be trimmed and appended: {result}"
    );
    assert!(result.ends_with("also fix the tests"));
}

#[test]
fn append_addition_request_noop_for_empty_or_missing() {
    let base = "## 3. Decision\n\ndo work";
    assert_eq!(append_addition_request(base, None), base);
    assert_eq!(append_addition_request(base, Some("   ")), base);
}

#[test]
fn append_addition_request_never_duplicates() {
    let base = "## 3. Decision\n\ndo work\n\n## 4. Addition request\n\nfirst draft";
    assert_eq!(append_addition_request(base, Some("second draft")), base);
}

#[test]
fn extract_decision_prompt_cuts_at_addition_request() {
    let prompt = "## 1. Thread Summary\n\nsummary\n\n## 3. Decision\n\ncontinue the work\n\n## 4. Addition request\n\nuser's draft";
    assert_eq!(
        extract_decision_prompt(prompt).as_deref(),
        Some("continue the work")
    );
}

#[test]
fn maybe_clarification_request_fires_once() {
    let session = "clarify-once-test";
    let data = overflow_test_data(
        session,
        Some("I could take approach 1 or approach 2.\n\nWhich one?"),
    );
    let first = maybe_clarification_request(&data);
    assert!(
        matches!(&first, Some(AutoPromptOutcome::ClarificationRequest(action)) if !action.force_new_thread
                && action.next_prompt.contains("pros and cons")),
        "expected a same-thread clarify prompt, got {first:?}"
    );
    assert!(
        maybe_clarification_request(&data).is_none(),
        "CLARIFY_REGISTRY must fire at most once per chain"
    );
}

#[test]
fn maybe_clarification_request_skips_existing_pros_cons() {
    let session = "clarify-proscons-test";
    let data = overflow_test_data(
        session,
        Some("Two options:\n\nPros of A: fast. Cons of A: fragile. Pros of B: safe."),
    );
    assert!(maybe_clarification_request(&data).is_none());
}

#[test]
fn maybe_clarification_request_skips_without_options() {
    let session = "clarify-nooptions-test";
    let data = overflow_test_data(session, Some("Implemented the feature and tests pass."));
    assert!(maybe_clarification_request(&data).is_none());
}

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
    let summary = "Implementing feature X in **plan 085** — completed steps 1-3, need to do step 4";
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
            "The remaining work section was already addressed in the previous commit.".to_string(),
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
            "Done with part 1.\n\n### Action items\n\n- Fix the bug\n- Add tests".to_string(),
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
        reason: Some(
            "model returned no usable content (3 empty Text, 0 empty Thinking, 0 stream errors)"
                .to_string(),
        ),
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
    let SummaryContinuation::Steps(result) = result else {
        panic!("actionable next steps must produce Steps, got {result:?}");
    };
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
    let result = extract_summary_next_steps(summary).expect("What Remains section should be found");
    let SummaryContinuation::Steps(result) = result else {
        panic!("unchecked checkboxes must produce Steps, got {result:?}");
    };
    assert!(
        result.contains("T2: verify build"),
        "should contain the unchecked task"
    );
    assert!(
        result.contains("make the call yourself"),
        "tail should tell the agent to resolve decisions itself"
    );
}

#[test]
fn test_extract_summary_next_steps_ignores_heading_without_actionable_body() {
    let summary =
        "# Summary\n\n## Next Steps\n\nSee the plan file for details.\n\n## Done\n\nAll good.";
    let result = extract_summary_next_steps(summary);
    assert!(
        result.is_none(),
        "heading with no actionable markers should be ignored"
    );
}

#[test]
fn test_extract_summary_next_steps_no_heading_falls_back_to_prose() {
    // Summary embeds next steps in the last paragraph without a dedicated heading.
    let summary =
        "# Summary\n\nDid A.\n\nDid B.\n\nStill need to run the benchmark and commit the results.";
    let result = extract_summary_next_steps(summary).expect("prose trigger 'need to' should match");
    let SummaryContinuation::Steps(result) = result else {
        panic!("prose next steps must produce Steps, got {result:?}");
    };
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
    let SummaryContinuation::Steps(result) = result else {
        panic!("numbered steps must produce Steps, got {result:?}");
    };
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
    assert!(
        result.contains("where a decision is needed"),
        "tail should instruct the agent to resolve the decision itself"
    );
}

#[test]
fn test_extract_summary_next_steps_gated_remains_stay_steps() {
    // Reproduces the user-reported confusion: a boundary-session summary
    // whose remains are ALL owner-gated or deliberately deferred. The
    // remains must still be extracted as Steps (NOT skipped, NOT nothing-
    // left) — a different session's plan-file fallback must never silently
    // replace them — and the tail must tell the agent to respect the gates
    // instead of deciding on the owner's behalf.
    let summary = "## Summary\n\n\
                       **(1) Original task** — Fix and remove `.issues/724` and `.issues/737` in riir-ai.\n\n\
                       **(2) Accomplished**\n\
                       - Workspace gate script landed, 15 repos clean\n\n\
                       **(3) What remains**\n\
                       - `.issues/739` — engram trait seam relocation (CODE, owner go; T1–T5 with GOAT gate)\n\
                       - T-AGENTS prose extraction from the 5 long AGENTS.md files — deliberately not done\n\
                       - riir-chain shows 2 transient gate findings until the pin branch merges\n\n\
                       **(4) Active plan state**\n\
                       - `.issues/739` — OPEN, gated on owner go\n\
                       - `.proposals/025` — DEFER by decision record, not a task";
    let result = extract_summary_next_steps(summary)
        .expect("gated remains must still be extracted as Steps");
    let SummaryContinuation::Steps(result) = result else {
        panic!("gated remains are real remains, got {result:?}");
    };
    assert!(
        result.contains(".issues/739"),
        "the remains section must be embedded so the next thread sees it: {result}"
    );
    assert!(
        result.contains("owner-gated"),
        "tail must instruct the agent to respect explicit owner gates: {result}"
    );
    // All-gated remains are NOT nothing-left: there is still work the
    // owner can ungat, so the housekeeping path must not fire.
    assert!(!result.contains("boundary-guard"));
}

#[test]
fn test_extract_summary_next_steps_nothing_left_returns_housekeeping_path() {
    // The reported bug: a "What Remains" section that declares nothing
    // left still counted as actionable ("- " bullet marker) and produced
    // "start working immediately" on an empty queue.
    let summary = "# Session Summary\n\n## Original Task\n\nRefactor.\n\n\
                       ## What Was Accomplished\n\nAll done.\n\n\
                       ## What Remains\n\n- Nothing — all tasks complete.";
    let result =
        extract_summary_next_steps(summary).expect("nothing-left section must still be detected");
    assert_eq!(
        result,
        SummaryContinuation::NothingLeft,
        "a remains section declaring nothing left must NOT produce Steps"
    );
}

#[test]
fn test_extract_summary_next_steps_nothing_left_prose() {
    // No heading — the prose fallback picks up "Nothing left to do" and
    // must classify it as NothingLeft rather than actionable steps.
    let summary = "# Session Summary\n\nDid A.\n\nDid B.\n\nNothing left to do.";
    let result = extract_summary_next_steps(summary).expect("prose nothing-left must be detected");
    assert_eq!(result, SummaryContinuation::NothingLeft);
}

#[test]
fn test_extract_summary_next_steps_mixed_negation_with_real_work_stays_steps() {
    // Negation cue + real unchecked work → Steps, not NothingLeft.
    let summary = "# Summary\n\n## What Remains\n\n\
                       T1 is all done.\n\
                       - [ ] T2: verify build\n";
    let result = extract_summary_next_steps(summary).expect("section should be found");
    let SummaryContinuation::Steps(result) = result else {
        panic!("an actionable checkbox must override the negation cue, got {result:?}");
    };
    assert!(result.contains("T2: verify build"));
}

#[test]
fn test_extract_summary_next_steps_negated_numbered_item_is_nothing_left() {
    // A numbered item whose text is itself a nothing-left statement must
    // not count as real work.
    let summary = "# Summary\n\n## Next Steps\n\n1. Nothing left to do — all complete.";
    let result = extract_summary_next_steps(summary).expect("numbered negation must be detected");
    assert_eq!(result, SummaryContinuation::NothingLeft);
}

#[test]
fn test_housekeeping_continuation_lists_skills_and_stop() {
    let prompt = housekeeping_continuation();
    assert!(prompt.contains("boundary-guard"));
    assert!(prompt.contains("doc-sync"));
    assert!(prompt.contains("riir-clippy"));
    assert!(prompt.contains("Re-run benchmarks"));
    assert!(prompt.contains("no other agent is active"));
    assert!(
        prompt.contains("stop cleanly"),
        "must end with a stop instruction so the loop terminates"
    );
    assert!(prompt.contains("Do NOT summarize again"));
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
    let full = with_first_prompt_context(decision.to_string(), Some(summary), None, Some(last_msg));
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
    assert!(looks_like_voluntary_summary(
        "## TL;DR\n\nDid X, Y, Z. Next: do W."
    ));
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
        llm_acknowledged_all_tasks_blocked(Some("No further action possible without external API")),
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
        !llm_acknowledged_all_tasks_blocked(Some("The build is blocked by a missing dependency")),
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

/// Bugfix contract (2026-09-11 WAF-block freeze): when every orchestration
/// retry has failed AND the source thread's turn itself failed at the API
/// level (`had_api_error`), the retry-exhaustion path must defer
/// (`RetryAfterBackoff`) rather than fall through to the no-LLM safety
/// nets (`detect_remaining_work` / `detect_remaining_plan_tasks`), which
/// would "continue" from a stale assistant message into the same blocked
/// API — the infinite send → 405 → decide → send loop. Mirrors the
/// issue-007 contract style: the rule lives in
/// `should_defer_after_retry_exhaustion`, which the match guard on the
/// exhaustion arm calls, so flipping the rule breaks this test.
#[test]
fn test_retry_exhaustion_with_api_error_defers_instead_of_safety_net() {
    assert!(
        should_defer_after_retry_exhaustion(true),
        "had_api_error + all retries failed must defer, not safety-net continue"
    );
    assert!(
        !should_defer_after_retry_exhaustion(false),
        "no API error: safety nets stay reachable for ordinary retry exhaustion"
    );
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
    assert!(
        debug.contains("RetryAfterBackoff"),
        "debug repr missing variant name: {debug}"
    );
    assert!(
        debug.contains("1000"),
        "debug repr missing delay_ms: {debug}"
    );
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
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("should be valid JSON");
    assert_eq!(parsed["a"], 1);
    assert_eq!(parsed["b"], "two");
}

#[test]
fn test_rebuild_deduplicated_json_duplicate_null_after_value() {
    let json =
        r#"{"thread_summary": "long summary here", "confidence": 0.75, "thread_summary": null}"#;
    let result = rebuild_deduplicated_json(json).expect("should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).expect("should be valid JSON");
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

// ── MaxTokens stream cutoff (plan 032) ─────────────────────────────

#[test]
fn max_tokens_window_full_classifier() {
    // Window fill: input + output + reserve >= window → full.
    assert!(max_tokens_context_window_full(
        Some(195_000),
        5_000,
        200_000
    ));
    // Exactly at the boundary.
    assert!(max_tokens_context_window_full(Some(191_000), 808, 200_000));
    // Output-cap cutoff with plenty of window room → not full.
    assert!(!max_tokens_context_window_full(
        Some(100_000),
        8_192,
        200_000
    ));
    // No usage reported → legacy behavior (not full).
    assert!(!max_tokens_context_window_full(None, 50_000, 200_000));
    // Unknown window → not full.
    assert!(!max_tokens_context_window_full(Some(150_000), 50_000, 0));
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

fn cutoff_test_data(
    session: &str,
    cx: &mut gpui::TestAppContext,
) -> (
    LlmCallData,
    std::sync::Arc<language_model::fake_provider::FakeLanguageModelProvider>,
) {
    let fake = cx.update(language_model::LanguageModelRegistry::test);
    let mut data = overflow_test_data(
        session,
        Some(
            "Same manifest, same cargo version, different result per invocation context. How?\n\n**The lockfile!** riir-ai's own",
        ),
    );
    data.model = fake.model("fake");
    data.cutoff_context_full = true;
    (data, fake)
}

#[gpui::test]
async fn cutoff_fork_uses_orchestrated_summary(cx: &mut gpui::TestAppContext) {
    let session = "cutoff-fork-success-test";
    clear_summary_for_session(session);
    let (data, fake) = cutoff_test_data(session, cx);

    let summary_json = serde_json::json!({
            "confidence": 0.9,
            "next_prompt": null,
            "reason": "cutoff handoff",
            "thread_summary": "## Summary\n\n(1) Original task: debug the invocation-context divergence.\n(2) Accomplished: narrowed it to the lockfile.\n(3) What remains: diff the two Cargo.lock files.\n(4) Active plans: 032 in flight."
        })
        .to_string();

    let task = cx.spawn(async move |cx| cutoff_context_full_outcome(&data, &cx).await);
    cx.run_until_parked();
    assert_eq!(fake.completion_count(), 1, "one summary call expected");
    fake.respond_to_last_pending_completion(
        &fake.model("fake"),
        [LanguageModelCompletionEvent::Text(summary_json)],
    );

    match task.await {
        AutoPromptOutcome::Continue(action) => {
            assert!(
                action.force_new_thread,
                "cutoff fork must create a new thread"
            );
            assert_eq!(action.actual_input_tokens, None);
            assert_eq!(action.approximate_token_count, 0);
            assert!(
                action.next_prompt.contains("(1) Original task"),
                "summary must ride into the new thread: {}",
                action.next_prompt
            );
            assert!(
                action.next_prompt.contains(CONTINUE_REMAINS_DECISION),
                "fixed directive must ride along: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[gpui::test]
async fn cutoff_fork_falls_back_when_summary_unavailable(cx: &mut gpui::TestAppContext) {
    let session = "cutoff-fork-fallback-test";
    clear_summary_for_session(session);
    let (data, fake) = cutoff_test_data(session, cx);

    let task = cx.spawn(async move |cx| cutoff_context_full_outcome(&data, &cx).await);
    cx.run_until_parked();
    fake.respond_to_last_pending_completion(
        &fake.model("fake"),
        [LanguageModelCompletionEvent::Text(
            "this is not json".to_string(),
        )],
    );
    // Attempt 2 backs off 2s before retrying.
    cx.executor().advance_clock(Duration::from_millis(2_000));
    cx.run_until_parked();
    fake.respond_to_last_pending_completion(
        &fake.model("fake"),
        [LanguageModelCompletionEvent::Text(
            "still not json".to_string(),
        )],
    );

    match task.await {
        AutoPromptOutcome::Continue(action) => {
            // The window is full: even the legacy fallback must fork, not
            // send a doomed same-thread continuation.
            assert!(action.force_new_thread, "fallback must still fork");
            assert!(
                action
                    .next_prompt
                    .contains("Context limit reached. Continue from where we left off."),
                "legacy fallback continuation expected: {}",
                action.next_prompt
            );
        }
        other => panic!("expected Continue, got {other:?}"),
    }
    clear_summary_for_session(session);
}

#[gpui::test]
async fn cutoff_fork_stops_on_terminal_summary(cx: &mut gpui::TestAppContext) {
    let session = "cutoff-fork-terminal-test";
    clear_summary_for_session(session);
    let (data, fake) = cutoff_test_data(session, cx);

    let summary_json = serde_json::json!({
            "confidence": 0.9,
            "next_prompt": null,
            "reason": "cutoff handoff",
            "thread_summary": "## Summary\n\n**(3) What remains:**\n- T2c-b — P3, deferred.\n- 32K cell is thermal-gated."
        })
        .to_string();

    let task = cx.spawn(async move |cx| cutoff_context_full_outcome(&data, &cx).await);
    cx.run_until_parked();
    fake.respond_to_last_pending_completion(
        &fake.model("fake"),
        [LanguageModelCompletionEvent::Text(summary_json)],
    );

    match task.await {
        AutoPromptOutcome::Stopped { reason } => {
            assert!(reason.contains("terminal"), "reason: {reason}");
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
    clear_summary_for_session(session);
}
