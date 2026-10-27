//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

fn test_retry_status(duration: Duration) -> acp_thread::RetryStatus {
    acp_thread::RetryStatus {
        last_error: "GLM's API rate limit exceeded".into(),
        attempt: 1,
        max_attempts: 4,
        started_at: Instant::now(),
        duration,
        meta: None,
    }
}

#[gpui::test]
async fn test_retry_callout_cleared_when_turn_stops(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("Hello", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
        view.send(window, cx);
    });
    cx.run_until_parked();

    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| view.thread.read(cx).session_id().clone());

    // A mid-turn provider retry surfaces the callout...
    active_thread(&conversation_view, cx).update(cx, |view, cx| {
        view.thread.update(cx, |thread, cx| {
            thread.update_retry_status(test_retry_status(Duration::from_secs(9428)), cx);
        });
    });
    active_thread(&conversation_view, cx).read_with(cx, |view, _cx| {
        assert!(
            view.thread_retry_status.is_some(),
            "retry callout should be visible while backing off"
        );
    });

    // ...and must not outlive the turn it belonged to.
    connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    cx.run_until_parked();
    active_thread(&conversation_view, cx).read_with(cx, |view, _cx| {
        assert!(
            view.thread_retry_status.is_none(),
            "retry callout must be cleared when the turn stops"
        );
    });
}

#[gpui::test]
async fn test_retry_callout_cleared_by_new_turn(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;

    // A retry status can outlive its turn (external agents control their
    // own retry semantics). While the thread is idle nothing clears it —
    // only starting a new generation should.
    active_thread(&conversation_view, cx).update(cx, |view, cx| {
        view.thread.update(cx, |thread, cx| {
            thread.update_retry_status(test_retry_status(Duration::from_secs(9428)), cx);
        });
    });
    active_thread(&conversation_view, cx).read_with(cx, |view, _cx| {
        assert!(view.thread_retry_status.is_some());
    });

    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("Next message", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
        view.send(window, cx);
    });
    cx.run_until_parked();

    active_thread(&conversation_view, cx).read_with(cx, |view, _cx| {
        assert!(
            view.thread_retry_status.is_none(),
            "starting a new turn must drop a stale retry callout"
        );
    });

    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| view.thread.read(cx).session_id().clone());
    connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    cx.run_until_parked();
}

/// Covers the `[up]`/`[down]` bookend wiring in `render_user_prompt_jumps`
/// and the `scroll_to_prev_user_prompt` / `scroll_to_next_user_prompt`
/// helpers (still used by the ScrollOutputToPrevious/NextMessage actions
/// even though the prompt-jump strip's prev/next bookends were replaced by
/// cross-thread `from`/`to` navigation). The test pins that those methods
/// actually move the `ListState` to the expected ends of a multi-turn
/// conversation. The list is drawn first so the `ListState` item count is
/// synced during the `list()` layout pass (without a draw, `scroll_to_end`
/// has no measured extent to scroll to).
#[gpui::test]
async fn test_prompt_jump_bookends_scroll_to_ends(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Response 1".into()),
    )]);

    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    let thread = conversation_view
        .read_with(cx, |view, cx| {
            view.active_thread().map(|r| r.read(cx).thread.clone())
        })
        .unwrap();

    // Two turns: entries layout is [User1, Assistant1, User2, Assistant2].
    thread
        .update(cx, |thread, cx| thread.send_raw("Prompt 1", cx))
        .await
        .unwrap();
    cx.run_until_parked();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Response 2".into()),
    )]);
    thread
        .update(cx, |thread, cx| thread.send_raw("Prompt 2", cx))
        .await
        .unwrap();
    cx.run_until_parked();

    let thread_view = active_thread(&conversation_view, cx);
    let entry_count = thread_view.read_with(cx, |view, cx| view.thread.read(cx).entries().len());
    assert_eq!(
        entry_count, 4,
        "expected two user/assistant turns; got {entry_count} entries"
    );

    // Draw so the ListState measures its items and scroll_to_end has an extent.
    draw_thread_list_at(
        &thread_view,
        ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        },
        cx,
    );

    // BOTTOM bookend wiring: scroll_to_end lands at or past the last
    // thread entry (the list may render extra items such as turn-end
    // separators, so the exact index is not pinned to entry_count - 1).
    thread_view.update(cx, |view, cx| {
        view.scroll_to_end(cx);
    });
    let bottom = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());
    assert!(
        bottom.item_ix >= entry_count - 1,
        "scroll_to_end should reach at least the last thread entry ({}), got {}",
        entry_count - 1,
        bottom.item_ix
    );

    // TOP bookend wiring: scroll_to_top returns to the first entry.
    thread_view.update(cx, |view, cx| {
        view.scroll_to_top(cx);
    });
    let top = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());
    assert_eq!(
        top.item_ix, 0,
        "scroll_to_top should land on the first entry"
    );
    assert!(
        bottom.item_ix > top.item_ix,
        "scroll_to_end and scroll_to_top should land on different ends"
    );

    // scroll_to_prev/next_user_prompt wiring (used by the
    // ScrollOutputToPrevious/NextMessage actions): stepping next jumps to
    // the 2nd user prompt (entry index 2 in [U0, A1, U2, A3]), and stepping
    // prev returns to the 1st user prompt (entry index 0).
    thread_view.update(cx, |view, cx| {
        view.scroll_to_next_user_prompt(cx);
    });
    let after_next = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());
    assert_eq!(
        after_next.item_ix, 2,
        "scroll_to_next_user_prompt should land on the second user prompt (entry 2)"
    );

    thread_view.update(cx, |view, cx| {
        view.scroll_to_prev_user_prompt(cx);
    });
    let after_prev = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());
    assert_eq!(
        after_prev.item_ix, 0,
        "scroll_to_prev_user_prompt should return to the first user prompt (entry 0)"
    );
}

/// Regression test for a bug where `render_user_prompt_jumps` returned
/// `None` whenever a thread had exactly one user prompt, hiding the entire
/// `[TOP] ... [BOTTOM]` strip — so a single-prompt thread (which may still
/// contain a long assistant response) had no end-to-end navigation at all.
///
/// This test sets up a one-prompt thread, draws the real `ThreadView` (so
/// the full render path including `render_user_prompt_jumps` runs without
/// panicking), and pins that the TOP/BOTTOM scroll targets still function.
#[gpui::test]
async fn test_prompt_jump_strip_visible_with_single_prompt(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("A single long response".into()),
    )]);

    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    let thread = conversation_view
        .read_with(cx, |view, cx| {
            view.active_thread().map(|r| r.read(cx).thread.clone())
        })
        .unwrap();

    // Exactly one user/assistant turn.
    thread
        .update(cx, |thread, cx| thread.send_raw("Only prompt", cx))
        .await
        .unwrap();
    cx.run_until_parked();

    let thread_view = active_thread(&conversation_view, cx);
    let entry_count = thread_view.read_with(cx, |view, cx| view.thread.read(cx).entries().len());
    assert_eq!(
        entry_count, 2,
        "expected one user/assistant turn; got {entry_count} entries"
    );

    // Draw the real ThreadView so the render path (including
    // render_user_prompt_jumps) executes; this would panic if the
    // single-prompt branch were broken.
    draw_real_thread_view(&thread_view, cx);

    // The strip must be functional: TOP and BOTTOM must move the list.
    thread_view.update(cx, |view, cx| {
        view.scroll_to_end(cx);
    });
    let bottom = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());

    thread_view.update(cx, |view, cx| {
        view.scroll_to_top(cx);
    });
    let top = thread_view.read_with(cx, |view, _cx| view.list_state.logical_scroll_top());

    assert_eq!(
        top.item_ix, 0,
        "scroll_to_top should land on the first entry even with a single prompt"
    );
    assert!(
        bottom.item_ix >= entry_count - 1,
        "scroll_to_end should reach the last entry even with a single prompt, got {}",
        bottom.item_ix
    );
}

/// Smoke-test the full `ThreadView` render path. No existing test draws the
/// real `ThreadView` (the permission-row tests draw a stub `TestListView`
/// that shares only the `ListState`); this renders the actual view so the
/// full `render` → `render_entries` → `render_entry` path executes for every
/// visible entry. Catches panics/regressions in entry rendering — the same
/// code path the checkpoint-row Branch button and the turn-end Branch button
/// live in.
///
/// Note: verifying the checkpoint-row Branch button's *presence* in the tree
/// additionally requires a user message with `checkpoint.show == true`, which
/// only happens when the git working tree actually changes between
/// checkpoints (`compare_checkpoints`). `FakeFs` does not provide real git
/// operations, and `Checkpoint.git_checkpoint` is private, so a checkpoint-
/// bearing entry cannot be constructed from this crate's tests. The boundary
/// *logic* the button triggers is covered by `branch_boundary_tests`.
#[gpui::test]
async fn test_render_full_thread_view_smoke(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Response 1".into()),
    )]);

    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    let thread = conversation_view
        .read_with(cx, |view, cx| {
            view.active_thread().map(|r| r.read(cx).thread.clone())
        })
        .unwrap();

    thread
        .update(cx, |thread, cx| thread.send_raw("Prompt 1", cx))
        .await
        .unwrap();
    cx.run_until_parked();

    let thread_view = active_thread(&conversation_view, cx);

    // Draw the real ThreadView with one turn present. This forces the full
    // `render` → `render_entries` → `render_entry` path for a user message
    // + assistant message entry. Asserting `bounds_for_item` proves the list
    // callback actually executed (items were laid out), not just that the
    // list state has a count.
    draw_real_thread_view(&thread_view, cx);
    let entry_count_after_first =
        thread_view.read_with(cx, |view, _cx| view.list_state.item_count());
    assert!(
        entry_count_after_first > 0,
        "list should have entries after a turn"
    );
    assert!(
        thread_view.read_with(cx, |view, _cx| view.list_state.bounds_for_item(0).is_some()),
        "first entry should have measured bounds after drawing (render_entry executed)"
    );

    // Render a multi-turn conversation to exercise render_entry across
    // user/assistant entry types and the turn-end separator.
    connection.set_next_prompt_updates(vec![acp_v1::SessionUpdate::AgentMessageChunk(
        acp_v1::ContentChunk::new("Response 2".into()),
    )]);
    thread
        .update(cx, |thread, cx| thread.send_raw("Prompt 2", cx))
        .await
        .unwrap();
    cx.run_until_parked();

    // Re-render with the expanded conversation — exercises render_entry for
    // both turns plus any turn-end separators.
    draw_real_thread_view(&thread_view, cx);
    assert_eq!(
        thread_view.read_with(cx, |view, cx| view.thread.read(cx).entries().len()),
        4,
        "expected two user/assistant turns"
    );
}

#[gpui::test]
async fn test_conversation_auto_allows_pending_tool_call_after_timeout(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.auto_allow_permissions_after_seconds = Some(2);
        AgentSettings::override_global(settings, cx);
    });

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

    let session_id = acp_v2::SessionId::new("session-auto");
    let (thread, conversation) = cx.update(|cx| {
        let thread = create_test_acp_thread(
            None,
            "session-auto",
            connection.clone(),
            project.clone(),
            cx,
        );
        let conversation = cx.new(|cx| {
            let mut conversation = Conversation::default();
            conversation.register_thread(thread.clone(), cx);
            conversation
        });
        (thread, conversation)
    });

    let request = request_test_tool_authorization(&thread, "tc-auto", "allow-auto", cx);
    cx.run_until_parked();

    let countdown_started = cx.read(|cx| {
        conversation
            .read(cx)
            .auto_allow_remaining_seconds(&session_id, &acp_v2::ToolCallId::new("tc-auto"))
    });
    assert_eq!(countdown_started, Some(2));

    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    let outcome = request.await;
    match outcome {
        acp_thread::RequestPermissionOutcome::Selected(outcome) => {
            assert_eq!(outcome.option_id.0.as_ref(), "allow-auto");
            assert_eq!(outcome.option_kind, acp_v2::PermissionOptionKind::AllowOnce);
        }
        other => panic!("expected auto-allow, got {other:?}"),
    }

    cx.read(|cx| {
        assert!(
            conversation
                .read(cx)
                .pending_tool_call(&session_id, cx)
                .is_none(),
            "Expected no pending tool call after auto-allow fired"
        );
    });
}

/// Like [`Self::request_test_tool_authorization`] but with caller-supplied
/// options and authorization kind — used by the plan-027 auto-allow tests
/// (ActionChoice prompts, sandbox-fallback option sets).
fn request_test_tool_authorization_with(
    thread: &Entity<AcpThread>,
    tool_call_id: &str,
    options: PermissionOptions,
    kind: acp_thread::AuthorizationKind,
    cx: &mut TestAppContext,
) -> Task<acp_thread::RequestPermissionOutcome> {
    let tool_call_id = acp_v2::ToolCallId::new(tool_call_id);
    let label = format!("Tool {tool_call_id}");
    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread
                .request_tool_call_authorization(
                    acp_v1::ToolCall::new(acp_v1::ToolCallId::new(tool_call_id.0), label)
                        .kind(acp_v1::ToolKind::Edit)
                        .into(),
                    options,
                    kind,
                    cx,
                )
                .unwrap()
        })
    })
}

/// Plan 027: ActionChoice prompts that offer an allow option are
/// auto-answered too — the carve-out only remains for prompts with no
/// allow option at all.
#[gpui::test]
async fn test_conversation_auto_allows_action_choice_with_allow_option(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.auto_allow_permissions_after_seconds = Some(2);
        AgentSettings::override_global(settings, cx);
    });

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

    // (No session_id binding needed — assertions go through the request.)
    let (thread, _conversation) = cx.update(|cx| {
        let thread = create_test_acp_thread(
            None,
            "session-action-choice",
            connection.clone(),
            project.clone(),
            cx,
        );
        let conversation = cx.new(|cx| {
            let mut conversation = Conversation::default();
            conversation.register_thread(thread.clone(), cx);
            conversation
        });
        (thread, conversation)
    });

    let request = request_test_tool_authorization_with(
        &thread,
        "tc-action-choice",
        PermissionOptions::Flat(vec![
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new("discard"),
                "Discard",
                acp_v2::PermissionOptionKind::RejectOnce,
            ),
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new("save"),
                "Save",
                acp_v2::PermissionOptionKind::AllowOnce,
            ),
        ]),
        acp_thread::AuthorizationKind::ActionChoice,
        cx,
    );
    cx.run_until_parked();

    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    match request.await {
        acp_thread::RequestPermissionOutcome::Selected(outcome) => {
            assert_eq!(outcome.option_id.0.as_ref(), "save");
            assert_eq!(outcome.option_kind, acp_v2::PermissionOptionKind::AllowOnce);
        }
        other => panic!("expected auto-allow of the allow option, got {other:?}"),
    }
}

/// Plan 027: the sandbox-fallback prompt auto-picks the least-privileged
/// allow ("Run without sandbox once") — never Retry and never a persistent
/// allow.
#[gpui::test]
async fn test_conversation_auto_allows_sandbox_fallback_with_once_allow(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);
        let mut settings = AgentSettings::get_global(cx).clone();
        settings.auto_allow_permissions_after_seconds = Some(2);
        AgentSettings::override_global(settings, cx);
    });

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

    // (No session_id binding needed — assertions go through the request.)
    let (thread, _conversation) = cx.update(|cx| {
        let thread = create_test_acp_thread(
            None,
            "session-sandbox-fallback",
            connection.clone(),
            project.clone(),
            cx,
        );
        let conversation = cx.new(|cx| {
            let mut conversation = Conversation::default();
            conversation.register_thread(thread.clone(), cx);
            conversation
        });
        (thread, conversation)
    });

    let request = request_test_tool_authorization_with(
        &thread,
        "tc-sandbox-fallback",
        PermissionOptions::Flat(vec![
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new(acp_thread::SANDBOX_FALLBACK_RETRY_OPTION_ID),
                "Retry",
                acp_v2::PermissionOptionKind::RejectAlways,
            ),
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new(acp_thread::SandboxPermission::AllowOnce.as_id()),
                "Run without sandbox once",
                acp_v2::PermissionOptionKind::AllowOnce,
            ),
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new(acp_thread::SandboxPermission::AllowAlways.as_id()),
                "Always run without sandbox",
                acp_v2::PermissionOptionKind::AllowAlways,
            ),
            acp_v2::PermissionOption::new(
                acp_v2::PermissionOptionId::new("deny"),
                "Deny",
                acp_v2::PermissionOptionKind::RejectOnce,
            ),
        ]),
        acp_thread::AuthorizationKind::ActionChoice,
        cx,
    );
    cx.run_until_parked();

    cx.executor().advance_clock(Duration::from_secs(3));
    cx.run_until_parked();

    match request.await {
        acp_thread::RequestPermissionOutcome::Selected(outcome) => {
            assert_eq!(
                outcome.option_id.0.as_ref(),
                acp_thread::SandboxPermission::AllowOnce.as_id(),
                "must pick 'Run without sandbox once', got {:?}",
                outcome.option_id
            );
            assert_eq!(outcome.option_kind, acp_v2::PermissionOptionKind::AllowOnce);
        }
        other => panic!("expected auto-allow of the once-allow option, got {other:?}"),
    }
}

#[gpui::test]
async fn test_conversation_does_not_auto_allow_by_default(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);
    });

    let fs = FakeFs::new(cx.executor());
    let project = Project::test(fs, [], cx).await;
    let connection: Rc<dyn AgentConnection> = Rc::new(StubAgentConnection::new());

    let session_id = acp_v2::SessionId::new("session-manual");
    let (thread, conversation) = cx.update(|cx| {
        let thread = create_test_acp_thread(
            None,
            "session-manual",
            connection.clone(),
            project.clone(),
            cx,
        );
        let conversation = cx.new(|cx| {
            let mut conversation = Conversation::default();
            conversation.register_thread(thread.clone(), cx);
            conversation
        });
        (thread, conversation)
    });

    let _request = request_test_tool_authorization(&thread, "tc-manual", "allow-manual", cx);
    cx.run_until_parked();

    cx.executor().advance_clock(Duration::from_secs(60));
    cx.run_until_parked();

    let still_waiting = cx.read(|cx| {
        thread
            .read(cx)
            .tool_call(&acp_v2::ToolCallId::new("tc-manual"))
            .is_some_and(|(_, tool_call)| {
                matches!(tool_call.status(), ToolCallStatus::WaitingForConfirmation)
            })
    });
    assert!(
        still_waiting,
        "expected prompt to wait indefinitely when the setting is unset"
    );
    assert_eq!(
        conversation.read_with(cx, |conversation, _| {
            conversation
                .auto_allow_remaining_seconds(&session_id, &acp_v2::ToolCallId::new("tc-manual"))
        }),
        None
    );
}

/// Draw the *real* `ThreadView` (not the stub `TestListView`) into the test
/// window. This forces the full render path — `ThreadView::render` →
/// `render_entries` → `render_entry` for every visible entry — so any panic
/// or regression in the entry-rendering code (including the checkpoint-row
/// Branch button path) surfaces here. The stub `draw_thread_list_at` only
/// exercises `ListState` mechanics, not content rendering.
fn draw_real_thread_view(thread_view: &Entity<ThreadView>, cx: &mut VisualTestContext) {
    cx.draw(
        point(px(0.0), px(0.0)),
        size(px(400.0), px(400.0)),
        |_window, _cx| thread_view.clone().into_any_element(),
    );
}

/// When a queued user message exists and the thread stops,
/// the queued message should be sent instead of auto_prompt firing.
#[gpui::test]
async fn test_queued_message_takes_priority_over_auto_prompt(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    // Enable auto_prompt
    active_thread(&conversation_view, cx).update(cx, |thread, cx| {
        thread.auto_prompt_enabled = true;
        cx.notify();
    });

    // Send initial message to start a turn
    let msg_editor = message_editor(&conversation_view, cx);
    msg_editor.update_in(cx, |editor, window, cx| {
        editor.set_text("Start work", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));

    cx.run_until_parked();

    // Get session ID after the turn started
    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, cx| thread.thread.read(cx).session_id().clone());

    // Queue a user message while generating
    active_thread(&conversation_view, cx).update_in(cx, |thread, window, cx| {
        thread.add_to_queue(
            vec![acp_v2::ContentBlock::Text(acp_v2::TextContent::new(
                "User follow-up".to_string(),
            ))],
            vec![],
            window,
            cx,
        );
    });

    cx.run_until_parked();

    // Verify the message is queued
    let queue_len = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, _cx| thread.message_queue.len());
    assert_eq!(queue_len, 1, "Message should be queued");

    // End the turn — this triggers the Stopped event
    cx.update(|_, _cx| {
        connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    });

    cx.run_until_parked();

    // Verify: the queued message was consumed (sent), not auto_prompt
    let queue_len = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, _cx| thread.message_queue.len());
    assert_eq!(queue_len, 0, "Queued message should have been sent");

    // Verify: no auto_prompt task was started
    let has_auto_prompt_task = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, _cx| thread._auto_prompt_task.is_some());
    assert!(
        !has_auto_prompt_task,
        "auto_prompt task should NOT have been created when queued message exists"
    );
}

/// Clicking the manual auto-prompt button must NOT consult the
/// orchestration LLM — the static continue directive is dispatched
/// immediately (the orchestrator's verdict on a manual click was
/// effectively always this fixed decision anyway). The stub is left
/// unanswered after the click, so no `Stopped` event can fire the automatic
/// chain either: any orchestrator completion below would be a regression.
#[gpui::test]
async fn test_manual_auto_prompt_dispatches_static_continuation(cx: &mut TestAppContext) {
    let fake = init_test(cx);

    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("do work", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, cx| thread.thread.read(cx).session_id().clone());
    cx.update(|_, _cx| {
        connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    });
    cx.run_until_parked();

    // auto_prompt is off, so the completed turn must not have consulted the
    // orchestrator on its own — anything below is the manual click's doing.
    let completions_before = fake.completion_count();

    active_thread(&conversation_view, cx)
        .update_in(cx, |view, window, cx| view.manual_auto_prompt(window, cx));
    cx.run_until_parked();

    assert_eq!(
        fake.completion_count(),
        completions_before,
        "manual auto-prompt must not consult the orchestration LLM"
    );

    let markdown = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| view.thread.read(cx).to_markdown(cx));
    assert!(
        markdown.contains("Continue remaining work"),
        "the static continuation directive must be sent immediately; got: {markdown}"
    );
}

/// When a user sends a direct message while auto_prompt is processing,
/// the auto_prompt task should be cancelled.
#[gpui::test]
async fn test_direct_message_cancels_auto_prompt_task(cx: &mut TestAppContext) {
    init_test(cx);

    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    // Enable auto_prompt
    active_thread(&conversation_view, cx).update(cx, |thread, cx| {
        thread.auto_prompt_enabled = true;
        cx.notify();
    });

    // Send initial message to start a turn
    let msg_editor = message_editor(&conversation_view, cx);
    msg_editor.update_in(cx, |editor, window, cx| {
        editor.set_text("Start work", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));

    cx.run_until_parked();

    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, cx| thread.thread.read(cx).session_id().clone());

    // End the turn — this triggers Stopped and may start auto_prompt
    cx.update(|_, _cx| {
        connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    });

    cx.run_until_parked();

    // Simulate user typing and sending a direct message
    let msg_editor = message_editor(&conversation_view, cx);
    msg_editor.update_in(cx, |ed, window, cx| {
        ed.set_text("User direct message", window, cx);
    });

    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));

    cx.run_until_parked();

    // Verify: auto_prompt task was cancelled by the direct send
    let has_auto_prompt_task = active_thread(&conversation_view, cx)
        .read_with(cx, |thread, _cx| thread._auto_prompt_task.is_some());
    assert!(
        !has_auto_prompt_task,
        "auto_prompt task should be cancelled after user sends a direct message"
    );

    // Verify: auto_prompt state reset to Idle
    let is_idle = active_thread(&conversation_view, cx).read_with(cx, |thread, _cx| {
        matches!(
            thread.auto_prompt_state,
            crate::auto_prompt::AutoPromptState::Idle
        )
    });
    assert!(
        is_idle,
        "auto_prompt state should be Idle after cancellation"
    );
}

/// Override the watchdog config for THIS test's app only. The env-var
/// route is process-global: tests running in parallel threads of one
/// binary race their config loads against a sibling's `set_var` and arm
/// the wrong timeout (reproduced as flakes in both directions), so tests
/// use the per-app override instead.
fn set_watchdog_override(cx: &mut TestAppContext, timeout_secs: u64) {
    cx.update(|cx| {
        cx.set_global(crate::auto_prompt::WatchdogConfigOverride {
            enabled: true,
            timeout_secs,
        });
    });
}

/// End-to-end test for the stuck-thread watchdog HALT flow.
///
/// Covers the full recovery path: a worker that hangs forever (its `prompt()`
/// never returns) → watchdog timer fires → reasoning LLM returns `halt` →
/// worker is cancelled → timeout-notice prompt is injected into the same
/// thread → a new generation begins.
///
/// This also implicitly verifies that the watchdog is armed on `send_content`
/// (the bug1.md fix) — not just after `on_thread_stopped`.
#[gpui::test]
async fn test_watchdog_halts_stuck_thread(cx: &mut TestAppContext) {
    // 1s watchdog window so the test advances the clock only a little.
    set_watchdog_override(cx, 1);

    let fake = init_test(cx);

    // Grab the registered fake provider so we can feed the reasoning
    // response.
    let fake_model = fake.model("fake");

    // A stub connection with no `next_prompt_updates` hangs forever until
    // `end_turn`/`cancel` resolves its oneshot channel — the stuck-worker
    // scenario.
    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    // Enable auto_prompt — the watchdog only arms when it is on.
    active_thread(&conversation_view, cx).update(cx, |view, cx| {
        view.auto_prompt_enabled = true;
        cx.notify();
    });

    // Send a message. The worker enters Generating and hangs.
    let message_editor = message_editor(&conversation_view, cx);
    message_editor.update_in(cx, |editor, window, cx| {
        editor.set_text("do something", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    // Pre-timeout: thread is stuck Generating and the watchdog is armed.
    let (status, watchdog_armed) = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| {
            (view.thread.read(cx).status(), view._watchdog_task.is_some())
        });
    assert_eq!(status, acp_thread::ThreadStatus::Generating);
    assert!(watchdog_armed, "watchdog should be armed while generating");

    // Advance the mock clock past the 1s window so the timer fires.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();

    // The watchdog's reasoning call should now be parked inside
    // `FakeLanguageModel::stream_completion`.
    assert_eq!(
        fake.completion_count(),
        1,
        "watchdog reasoning call should be pending"
    );

    // Feed a HALT decision.
    fake.send_last_text(
        &fake_model,
        r#"{"action":"halt","reason":"test: simulated hang"}"#,
    );
    fake.end_last(&fake_model);
    cx.run_until_parked();

    // Post-HALT: the worker was cancelled and the timeout-notice prompt was
    // dispatched to the same thread, starting a new (also stuck) generation.
    let (status, markdown) = active_thread(&conversation_view, cx).read_with(cx, |view, cx| {
        (
            view.thread.read(cx).status(),
            view.thread.read(cx).to_markdown(cx),
        )
    });
    assert_eq!(
        status,
        acp_thread::ThreadStatus::Generating,
        "new generation should be running after timeout-notice dispatch"
    );
    assert!(
        markdown.contains("Watchdog timeout"),
        "timeout-notice prompt should be in the thread; got: {markdown}"
    );
}

/// Regression test for issue 004: the watchdog timer must reset for each
/// new generation, and cancellation must target the thread that stopped
/// (by session_id), not whichever thread happens to be active.
///
/// Before the fix, `cancel_watchdog_for_thread` cancelled
/// `active_thread()` instead of the stopped thread, and `arm_watchdog`
/// refused to re-arm when a stale task existed. Combined, a stale watchdog
/// survived across generation boundaries and accumulated elapsed time.
///
/// This test drives two sequential generations on the same thread and
/// asserts the watchdog task lifecycle: armed → cancelled (None) on stop →
/// re-armed (Some, a fresh task) on the next send.
#[gpui::test]
async fn test_watchdog_resets_across_generations(cx: &mut TestAppContext) {
    // This test does NOT override the watchdog config. The watchdog is
    // enabled by default with a 1800s window — long enough that the timer
    // never fires during this test (we never advance the clock that far).
    //
    // We only assert the arm/cancel lifecycle, not the timeout itself.

    init_test(cx);

    // Use the default-response stub: each `prompt` sends one message chunk
    // then ends the turn, so generations complete normally (non-hanging).
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::default_response(), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    // Enable auto_prompt so the watchdog arms.
    active_thread(&conversation_view, cx).update(cx, |view, cx| {
        view.auto_prompt_enabled = true;
        cx.notify();
    });

    // --- Generation 1: send, arm watchdog, then let it complete ---
    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("first", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    // Generation 1 ran: thread is back to Idle and the watchdog was
    // cancelled by session_id via the Stopped handler.
    let (status, watchdog_armed) = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| {
            (view.thread.read(cx).status(), view._watchdog_task.is_some())
        });
    assert_eq!(
        status,
        acp_thread::ThreadStatus::Idle,
        "generation 1 should have completed"
    );
    assert!(
        !watchdog_armed,
        "watchdog should be cancelled after normal stop (fix for Bug A)"
    );

    // --- Generation 2: a fresh watchdog must re-arm (fix for Bug B) ---
    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("second", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    // While generating, the watchdog is armed again. With the pre-fix code,
    // the stale task guard would skip this and `_watchdog_task` could hold
    // a task from generation 1 whose `started_at` predated this send.
    // After the fix, `arm_watchdog` drops the stale task (already None here)
    // and creates a fresh one with `Instant::now()`.
    let (status, watchdog_armed) = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| {
            (view.thread.read(cx).status(), view._watchdog_task.is_some())
        });
    assert_eq!(
        status,
        acp_thread::ThreadStatus::Generating,
        "generation 2 should be running"
    );
    assert!(
        watchdog_armed,
        "a fresh watchdog should be armed for generation 2 (fix for Bug B)"
    );
}

/// The watchdog must NOT fire while the worker is actively streaming output.
///
/// Before this fix, the watchdog measured elapsed time from a single
/// `started_at = Instant::now()` captured when the generation began. A
/// long-running generation that was actively producing content (tool calls,
/// streaming text) would still trip the watchdog after `timeout_secs`,
/// producing false "stuck thread" cancellations.
///
/// After the fix, every `NewEntry` / `EntryUpdated` event bumps an activity
/// generation counter. The watchdog captures the counter before sleeping;
/// if it changed when the timer expires, the thread is active and the
/// watchdog re-sleeps instead of firing.
#[gpui::test]
async fn test_watchdog_does_not_fire_during_active_stream(cx: &mut TestAppContext) {
    // 2s watchdog window — short enough for the test, long enough that we
    // can advance the clock in steps and inject streaming chunks between.
    set_watchdog_override(cx, 2);

    let fake = init_test(cx);

    // Use a stub connection that we control: we'll feed it streaming
    // updates manually so the thread stays Generating with periodic output.
    let connection = StubAgentConnection::new();
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    active_thread(&conversation_view, cx).update(cx, |view, cx| {
        view.auto_prompt_enabled = true;
        cx.notify();
    });

    // Send a message. The worker enters Generating.
    let message_editor = message_editor(&conversation_view, cx);
    message_editor.update_in(cx, |editor, window, cx| {
        editor.set_text("do something slowly", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    let session_id = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| view.thread.read(cx).session_id().clone());

    // Sanity: thread is generating, watchdog armed.
    let (status, watchdog_armed) = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| {
            (view.thread.read(cx).status(), view._watchdog_task.is_some())
        });
    assert_eq!(status, acp_thread::ThreadStatus::Generating);
    assert!(watchdog_armed);

    // Advance the clock 1s (halfway to the 2s timeout), then inject a
    // streaming chunk via send_update (set_next_prompt_updates only
    // applies to the NEXT prompt call, but this session is already
    // hanging). This bumps the activity counter.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                "working...".into(),
            )),
            cx,
        );
    });
    cx.run_until_parked();

    // Advance past the original 2s window (total 3s). Without the activity
    // reset, the watchdog would have fired by now.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(2));
    cx.run_until_parked();

    // The watchdog should NOT have called the reasoning LLM — the stream
    // was active.
    assert_eq!(
        fake.completion_count(),
        0,
        "watchdog should not fire while the worker is actively streaming"
    );

    // Now stop feeding updates and advance past the timeout again.
    // With no activity for a full window, the watchdog should fire.
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(3));
    cx.run_until_parked();

    assert_eq!(
        fake.completion_count(),
        1,
        "watchdog should fire after a full timeout window with no activity"
    );
}

/// `retry_generation` must reset the turn stopwatch (`turn_started_at`) so
/// the elapsed-time counter beside the inline RETRY button restarts from
/// zero for the retried turn, rather than continuing to count from the
/// original turn's start. Regression test for the "count time beside RETRY
/// button still bug" report.
///
/// Flow: send a message (turn 1 starts, stopwatch = T0), end the turn, then
/// call retry_generation. After retry, `turn_started_at` must be > T0
/// (reset to a fresh Instant::now()).
#[gpui::test]
async fn test_retry_generation_resets_turn_stopwatch(cx: &mut TestAppContext) {
    init_test(cx);

    // StubAgentConnection must advertise retry support, otherwise
    // `can_retry` returns false and `retry_generation` early-returns
    // before reaching `start_turn`.
    let connection = StubAgentConnection::new().with_supports_retry(true);
    let (conversation_view, cx) =
        setup_conversation_view(StubAgentServer::new(connection.clone()), cx).await;
    add_to_workspace(conversation_view.clone(), cx);

    // --- Turn 1: send a message, capture turn_started_at, end the turn ---
    message_editor(&conversation_view, cx).update_in(cx, |editor, window, cx| {
        editor.set_text("first", window, cx);
    });
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| view.send(window, cx));
    cx.run_until_parked();

    let session_id = conversation_view.read_with(cx, |view, cx| {
        view.active_thread()
            .unwrap()
            .read(cx)
            .thread
            .read(cx)
            .session_id()
            .clone()
    });

    // End turn 1 so the thread returns to Idle and `turn_started_at` is
    // cleared by `stop_turn` (via send_content's defer).
    connection.end_turn(session_id, acp_v1::StopReason::EndTurn);
    cx.run_until_parked();

    // Sanity: thread is Idle after turn 1.
    let status = active_thread(&conversation_view, cx)
        .read_with(cx, |view, cx| view.thread.read(cx).status());
    assert_eq!(status, acp_thread::ThreadStatus::Idle);

    // --- Turn 2: retry. start_turn must reset turn_started_at. ---
    let t_before_retry = std::time::Instant::now();
    active_thread(&conversation_view, cx).update_in(cx, |view, window, cx| {
        view.retry_generation(window, cx);
    });
    cx.run_until_parked();

    let turn_started_at = active_thread(&conversation_view, cx)
        .read_with(cx, |view, _cx| view.turn_fields.turn_started_at);
    let turn_started_at = turn_started_at
        .expect("turn_started_at must be Some after retry_generation (start_turn reset)");

    // turn_started_at was just set by start_turn inside retry_generation,
    // so it must be at or after the Instant we captured just before the
    // retry call. Before the fix, retry bypassed start_turn and this would
    // be None (or a stale value from turn 1).
    assert!(
        turn_started_at >= t_before_retry,
        "retry_generation must reset turn_started_at to a fresh Instant::now()"
    );
}
