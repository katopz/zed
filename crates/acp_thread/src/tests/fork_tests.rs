use super::*;

/// Regression test for retry-after-completion: when a subagent's first turn
/// has already completed and the user retries it, the parent's tool_call is
/// set back to InProgress by `restart_subagent_tool_call`. Once the retry
/// finishes, `complete_subagent_tool_call` must transition it to Completed
/// and replace the content with the new output. Without this, the parent's
/// tool_call stays InProgress forever — the loading spinner never clears
/// and the stop button cannot fix it (cancel returns early when there is no
/// running_turn on the parent).
#[gpui::test]
async fn test_complete_subagent_tool_call(cx: &mut TestAppContext) {
    init_test(cx);
    let thread = new_test_thread(cx).await;
    let subagent_session_id = acp_v2::SessionId::new("subagent-1");
    let status_of = |thread: &AcpThread| {
        thread
            .tool_call_for_subagent(&subagent_session_id)
            .expect("tool_call should exist for subagent")
            .status()
    };

    thread.update(cx, |thread, cx| {
        thread
            .upsert_tool_call(
                acp_v1::ToolCall::new(acp_v1::ToolCallId::new("spawn_agent_1"), "Subagent")
                    .kind(acp_v1::ToolKind::Other)
                    .status(acp_v1::ToolCallStatus::Completed)
                    .meta(acp_v1::Meta::from_iter([(
                        SUBAGENT_SESSION_INFO_META_KEY.into(),
                        json!({"session_id": "subagent-1", "message_start_index": 0}),
                    )]))
                    .raw_output(serde_json::Value::String("first output".into())),
                cx,
            )
            .expect("tool call");
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(status_of(thread), ToolCallStatus::Completed);
    });

    // Simulate a retry: restart_subagent_tool_call sets it back to InProgress.
    thread.update(cx, |thread, cx| {
        thread.restart_subagent_tool_call(&subagent_session_id, cx);
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(status_of(thread), ToolCallStatus::InProgress);
    });

    // Simulate the retry completing with a new output.
    thread.update(cx, |thread, cx| {
        thread.complete_subagent_tool_call(
            &subagent_session_id,
            Some("second output".to_string()),
            false,
            cx,
        );
    });
    cx.run_until_parked();
    thread.read_with(cx, |thread, cx| {
        assert_eq!(status_of(thread), ToolCallStatus::Completed);
        let tool_call = thread
            .tool_call_for_subagent(&subagent_session_id)
            .expect("tool_call should still exist");
        assert_eq!(
            tool_call.raw_output.as_ref(),
            Some(&serde_json::Value::String("second output".into())),
        );
        let content_md = tool_call.to_markdown(cx);
        assert!(
            content_md.contains("second output"),
            "rendered content should contain the new output, got: {content_md}"
        );
        assert!(
            !content_md.contains("first output"),
            "rendered content should NOT contain the old output, got: {content_md}"
        );
    });

    // Verify the error case transitions to Failed.
    thread.update(cx, |thread, cx| {
        thread.restart_subagent_tool_call(&subagent_session_id, cx);
        thread.complete_subagent_tool_call(&subagent_session_id, None, true, cx);
    });
    thread.read_with(cx, |thread, _| {
        assert_eq!(status_of(thread), ToolCallStatus::Failed);
    });
}
