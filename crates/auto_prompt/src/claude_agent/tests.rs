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
    use agent_client_protocol::schema::v1 as acp_v1;
    use std::rc::Rc;

    /// Minimal LlmCallData for the hidden-thread path. Only `connection`,
    /// `project`, `system_prompt`, `context_json`, `session_id`,
    /// `last_assistant_message`, and `work_dirs` are read by
    /// `decide_claude_with_hidden_thread`; the rest are zeroed/filled with
    /// a fake provider model placeholder.
    fn build_test_data(
        connection: Rc<dyn acp_thread::AgentConnection>,
        project: gpui::Entity<project::Project>,
        worker_output: &str,
    ) -> LlmCallData {
        LlmCallData {
            model: language_model::fake_provider::FakeLanguageModelProvider::default()
                .model("fake"),
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
            cutoff_context_full: false,
            approximate_token_count: 0,
            connection: Some(connection),
            project: Some(project),
            peer_agent_states: None,
            bloat_directive: None,
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": true, "confidence": 0.8, "next_prompt": "Run the failing test.", "reason": "Test still failing."}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection, project, "Tests are still failing.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "Task complete."}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection, project, "Done. All tests pass.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new("I read the file and the bug is on line 42.".into()),
        )]);

        let data = build_test_data(connection, project, "Investigating the bug in file.rs.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
    async fn test_hidden_thread_tool_use_stops_even_with_valid_json(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = fs::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;

        let connection = Rc::new(StubAgentConnection::new());
        // The hidden session used a tool (ToolCall entry) AND returned valid
        // JSON that would normally produce a Continue verdict. The guard
        // must stop BEFORE parsing, because the tool use is the violation.
        let valid_continue_json = r#"{"continue": true, "confidence": 0.9, "next_prompt": "Keep going.", "reason": "More work."}"#;
        connection.set_next_prompt_updates(vec![
            acp_v1::SessionUpdate::ToolCall(
                acp_v1::ToolCall::new("leaked-read", "Read file.rs")
                    .kind(acp_v1::ToolKind::Fetch)
                    .status(acp_v1::ToolCallStatus::Completed),
            ),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                valid_continue_json.into(),
            )),
        ]);

        let data = build_test_data(connection, project, "Investigating the bug in file.rs.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": true, "confidence": 0.2, "next_prompt": "maybe continue?", "reason": "unsure"}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection, project, "Not sure if done.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": true, "confidence": 0.9, "next_prompt": null, "reason": "keep going"}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection, project, "Still working on it.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": true, "next_prompt": "do something", "reason": "unsure"}"#.into(),
            ),
        )]);

        let data = build_test_data(connection, project, "Not sure if done.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "done"}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection.clone(), project, "All done.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": false, "confidence": 0.9, "next_prompt": null, "reason": "done"}"#
                    .into(),
            ),
        )]);

        let data = build_test_data(connection.clone(), project, "All done.");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
    /// This test captures the fake provider and asserts completion_count
    /// stays 0 after a full Continue verdict roundtrip — if the hidden path
    /// ever regressed to calling the model (e.g. by accidentally routing to
    /// decide_claude_with_llm), this count would be 1.
    #[gpui::test]
    async fn test_hidden_thread_never_calls_language_model(cx: &mut gpui::TestAppContext) {
        init_test(cx);

        let fs = fs::FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;

        let connection = Rc::new(StubAgentConnection::new());
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
                r#"{"continue": true, "confidence": 0.9, "next_prompt": "Keep going.", "reason": "More work."}"#
                    .into(),
            ),
        )]);

        let provider = cx.update(language_model::LanguageModelRegistry::test);
        let mut data = build_test_data(connection, project, "Investigating the bug in file.rs.");
        data.model = provider.model("fake");

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
            .await
            .expect("hidden-thread decision succeeded");

        assert!(
            matches!(outcome, AutoPromptOutcome::Continue { .. }),
            "expected Continue, got {outcome:?}"
        );
        assert_eq!(
            provider.completion_count(),
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
        let connection_a = Rc::new(StubAgentConnection::new().with_supports_close_session(true));
        let connection_b = Rc::new(StubAgentConnection::new().with_supports_close_session(true));

        // Decision A: continue, run the failing test.
        connection_a.set_next_prompt_updates(vec![
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                r#"{"continue": true, "confidence": 0.9, "next_prompt": "Run test A.", "reason": "A still failing."}"#.into(),
            )),
        ]);
        // Decision B: stop, task complete.
        connection_b.set_next_prompt_updates(vec![
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
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
        connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
            acp_v1::ContentChunk::new(
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
        })
        .to_string();

        let outcome = cx
            .update(|cx| cx.spawn(async move |cx| decide_claude_with_hidden_thread(data, cx).await))
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
