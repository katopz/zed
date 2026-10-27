//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

#[gpui::test]
async fn test_thread_for_session_prefix_resolves_active_thread(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_visible_panel(cx).await;
    open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);
    cx.run_until_parked();

    let session_id = active_session_id(&panel, &cx).to_string();
    assert!(
        !session_id.is_empty(),
        "stub session id should be non-empty"
    );

    // Full session id and its first char both resolve to the active thread.
    // (Stub session ids are short numeric strings like "0", so we use a
    // 1-char prefix instead of the production 4-char prefix.)
    let prefix = &session_id[..1];
    panel.read_with(&cx, |panel, cx| {
        let found = panel.thread_for_session_prefix(&session_id, cx);
        assert!(
            found.is_some(),
            "full session id lookup should find the thread"
        );
        let found = found.unwrap();
        assert_eq!(
            found.read(cx).session_id().to_string(),
            session_id,
            "resolved thread should match the active session"
        );

        let found_prefix = panel.thread_for_session_prefix(prefix, cx);
        assert!(
            found_prefix.is_some(),
            "prefix lookup should find the active thread"
        );

        // Non-existent prefix returns None (no crash).
        assert!(
            panel
                .thread_for_session_prefix("zzz-not-a-session", cx)
                .is_none(),
            "unknown prefix should resolve to None"
        );
    });
}

// Regression: auto-prompt start-context gathering runs inside the
// dispatching ConversationView's `update`, so it must not read that view
// (the entity is leased while being updated — reading it panics with
// "cannot read ... while it is already being updated").
#[gpui::test]
async fn test_active_thread_activity_skips_leased_dispatching_view(cx: &mut TestAppContext) {
    init_test(cx);
    cx.update(|cx| {
        agent::ThreadStore::init_global(cx);
        language_model::LanguageModelRegistry::test(cx);
    });

    let fs = FakeFs::new(cx.executor());
    cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
    fs.insert_tree("/project", json!({ "file.txt": "" })).await;
    let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

    let multi_workspace =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace
        .read_with(cx, |mw, _cx| mw.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(multi_workspace.into(), cx);
    let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
        cx.new(|cx| AgentPanel::new(workspace, window, cx))
    });

    // Sibling thread: the stub holds the prompt response open, so this
    // thread stays Generating and is retained when the next thread opens.
    open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);
    send_message(&panel, &mut cx);

    // Dispatching thread: now the displayed conversation view.
    open_thread_with_connection(&panel, StubAgentConnection::new(), &mut cx);

    let dispatching_view = panel.read_with(&cx, |panel, _cx| {
        panel.active_conversation_view().unwrap().clone()
    });
    let dispatching_view_id = dispatching_view.entity_id();

    let activity = dispatching_view.update(&mut cx, |_view, cx| {
        panel
            .read(cx)
            .active_thread_activity(cx, Some(dispatching_view_id))
    });
    assert_eq!(activity.len(), 1, "only the generating sibling is reported");
}

/// Regression test: auto_prompt creates a new thread that starts in
/// Loading state. If the user clicks another thread in the sidebar before
/// the new thread's connection establishes, `cleanup_retained_threads`
/// must not remove the loading thread (which would cancel its async load
/// task and lose the thread entirely).
#[gpui::test]
async fn test_loading_thread_retained_and_not_cleaned_up(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;

    // Create a thread that's fully connected and idle, then retain it.
    let connection_a = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());
    let (_session_id_a, thread_id_a) =
        open_generating_thread_with_loadable_connection(&panel, &connection_a, &mut cx);
    let session_id_a = active_session_id(&panel, &cx);

    // End turn so thread A becomes idle.
    cx.update(|_, _cx| {
        connection_a.end_turn(session_id_a.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    // Open a second thread — this pushes thread A into retained_threads.
    let connection_b = StubAgentConnection::new();
    open_thread_with_custom_connection(&panel, connection_b, &mut cx);
    cx.run_until_parked();

    // Confirm thread A is retained.
    panel.read_with(&cx, |panel, _cx| {
        assert!(panel.retained_threads.contains_key(&thread_id_a));
    });

    // Now create a new thread C, but DON'T let it connect yet.
    // We do this by calling external_thread without running_until_parked.
    let thread_id_c = panel.update_in(&mut cx, |panel, window, cx| {
        panel.external_thread(
            None,
            None,
            None,
            None,
            None,
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
        panel.active_thread_id(cx).unwrap()
    });
    // Do NOT call cx.run_until_parked() — thread C stays in Loading.

    // Verify the loading view C has no root_thread_view.
    panel.read_with(&cx, |panel, cx| {
        let base = panel.active_conversation_view().unwrap();
        assert!(base.read(cx).is_loading());
        assert!(base.read(cx).root_thread_view().is_none());
    });

    // Set max_idle to 0 for aggressive cleanup.
    cx.update(|_, cx| {
        AgentSettings::override_global(
            AgentSettings {
                max_idle_retained_threads: 0,
                ..AgentSettings::get_global(cx).clone()
            },
            cx,
        );
    });

    // Move the loading thread C into retained_threads by switching to thread A.
    panel.update_in(&mut cx, |panel, window, cx| {
        panel.load_agent_thread(
            crate::Agent::NativeAgent,
            thread_id_a,
            None,
            None,
            true,
            AgentThreadSource::Sidebar,
            window,
            cx,
        );
    });
    // Don't run_until_parked to keep thread C loading.

    // Thread C should now be in retained_threads.
    panel.read_with(&cx, |panel, _cx| {
        assert!(
            panel.retained_threads.contains_key(&thread_id_c),
            "loading thread C should be in retained_threads after navigating away"
        );
    });

    // Run cleanup — the loading thread C should NOT be removed.
    panel.update(&mut cx, |panel, cx| {
        panel.cleanup_retained_threads(cx);
    });

    panel.read_with(&cx, |panel, _cx| {
        assert!(
            panel.retained_threads.contains_key(&thread_id_c),
            "loading thread C should survive cleanup_retained_threads"
        );
    });
}

/// Regression test for the "new thread disappears while summary is loading"
/// bug. The previous fixes only protected threads in `ServerState::Loading`
/// (no `root_thread_view`) or with native-agent background flags
/// (title/summary/queued). They missed `ThreadView::is_loading_contents` —
/// the window between `send()` and the turn starting, where the editor is
/// resolving mention content (e.g. the ThreadSummary follow-up) into blocks.
/// During this window the thread reports `ThreadStatus::Idle` with no
/// background flags, so `cleanup_retained_threads` would evict it,
/// cancelling the in-flight content resolution and losing the thread.
/// This test simulates that exact state by flipping `is_loading_contents`
/// on a retained thread and asserting cleanup leaves it alone.
#[gpui::test]
async fn test_loading_contents_thread_not_cleaned_up(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;

    // Thread A: connected, idle, loadable — will become the navigation
    // target so the loading-contents thread B can be retained.
    let connection_a = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());
    let (_session_id_a, thread_id_a) =
        open_generating_thread_with_loadable_connection(&panel, &connection_a, &mut cx);
    let session_id_a = active_session_id(&panel, &cx);
    cx.update(|_, _cx| {
        connection_a.end_turn(session_id_a.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    // Thread B: also connected, idle, loadable. This is the thread that
    // would be created by auto_prompt's ThreadSummary dispatch.
    let (_session_id_b, thread_id_b) =
        open_generating_thread_with_loadable_connection(&panel, &connection_a, &mut cx);
    let session_id_b = active_session_id(&panel, &cx);
    cx.update(|_, _cx| {
        connection_a.end_turn(session_id_b.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    // Simulate the "summary loading" window: the thread is connected and
    // Idle, but `is_loading_contents` is true because its message (with a
    // ThreadSummary mention) is being resolved into content blocks.
    //
    // Clear any stale auto_prompt_task so is_loading_contents is the only
    // relevant state differing from a plain idle thread.
    panel.update(&mut cx, |panel, cx| {
        let view = panel
            .active_conversation_view()
            .expect("thread B should be active");
        let thread_view = view
            .read(cx)
            .root_thread_view()
            .expect("thread B should be connected");
        thread_view.update(cx, |tv, _cx| {
            tv._auto_prompt_task = None;
            tv.is_loading_contents = true;
        });
    });

    // Force aggressive cleanup so any non-protected idle thread is dropped.
    cx.update(|_, cx| {
        AgentSettings::override_global(
            AgentSettings {
                max_idle_retained_threads: 0,
                ..AgentSettings::get_global(cx).clone()
            },
            cx,
        );
    });

    // Navigate away to thread A — this retains thread B.
    panel.update_in(&mut cx, |panel, window, cx| {
        panel.load_agent_thread(
            crate::Agent::NativeAgent,
            thread_id_a,
            None,
            None,
            true,
            AgentThreadSource::Sidebar,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    panel.read_with(&cx, |panel, _cx| {
        assert!(
            panel.retained_threads.contains_key(&thread_id_b),
            "loading-contents thread B should be retained after navigating away"
        );
    });

    // Run cleanup — thread B must survive because is_loading_contents is true.
    panel.update(&mut cx, |panel, cx| {
        panel.cleanup_retained_threads(cx);
    });

    panel.read_with(&cx, |panel, _cx| {
        assert!(
            panel.retained_threads.contains_key(&thread_id_b),
            "loading-contents thread B should survive cleanup_retained_threads"
        );
    });

    // Sanity check: prove the protection was specifically due to
    // is_loading_contents, not some other quirk. Clear is_loading_contents
    // and re-evaluate the exact predicate from cleanup_retained_threads;
    // the thread should now be a cleanup candidate given it is idle,
    // loadable, and not native.
    panel.update(&mut cx, |panel, cx| {
        let view = panel
            .retained_threads
            .get(&thread_id_b)
            .expect("thread B still present")
            .clone();
        let thread_view_entity = view
            .read(cx)
            .root_thread_view()
            .expect("thread B connected");
        thread_view_entity.update(cx, |tv, _cx| {
            tv.is_loading_contents = false;
        });
        let tv = thread_view_entity.read(cx);
        let acp_thread = tv.thread.read(cx);
        let is_candidate = acp_thread.connection().supports_load_session()
            && acp_thread.status() == ThreadStatus::Idle
            && !tv.is_loading_contents;
        assert!(
            is_candidate,
            "after clearing is_loading_contents, idle loadable stub thread should be a cleanup candidate (status={:?}, supports_load={}, loading_contents={})",
            acp_thread.status(),
            acp_thread.connection().supports_load_session(),
            tv.is_loading_contents
        );
    });
}

/// Regression test: a thread with an in-flight `_auto_prompt_task` SHOULD
/// be cleanable — the task is the auto_prompt orchestration LLM call, and
/// cancelling it on eviction merely stops the auto_prompt chain for an
/// otherwise idle thread; it does not lose user data. This test pins that
/// contract so a future "protect everything" change can't silently break
/// cleanup of idle threads whose `on_thread_stopped` planted a pending task.
#[gpui::test]
async fn test_auto_prompt_task_thread_is_cleanable(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;

    let connection_a = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());
    let (_session_id_a, thread_id_a) =
        open_generating_thread_with_loadable_connection(&panel, &connection_a, &mut cx);
    let session_id_a = active_session_id(&panel, &cx);
    cx.update(|_, _cx| {
        connection_a.end_turn(session_id_a.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    let (_session_id_b, thread_id_b) =
        open_generating_thread_with_loadable_connection(&panel, &connection_a, &mut cx);
    let session_id_b = active_session_id(&panel, &cx);
    cx.update(|_, _cx| {
        connection_a.end_turn(session_id_b.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    // Plant a dummy non-completing auto_prompt task to simulate an in-flight
    // orchestration call, and ensure is_loading_contents is false so the
    // ONLY potential guard would be _auto_prompt_task (which we deliberately
    // do NOT guard on).
    panel.update(&mut cx, |panel, cx| {
        let view = panel.active_conversation_view().expect("thread B active");
        let thread_view = view
            .read(cx)
            .root_thread_view()
            .expect("thread B connected");
        thread_view.update(cx, |tv, cx| {
            tv.is_loading_contents = false;
            tv._auto_prompt_task = Some(cx.background_executor().spawn(async {
                std::future::pending::<()>().await;
            }));
        });
    });

    cx.update(|_, cx| {
        AgentSettings::override_global(
            AgentSettings {
                max_idle_retained_threads: 0,
                ..AgentSettings::get_global(cx).clone()
            },
            cx,
        );
    });

    panel.update_in(&mut cx, |panel, window, cx| {
        panel.load_agent_thread(
            crate::Agent::NativeAgent,
            thread_id_a,
            None,
            None,
            true,
            AgentThreadSource::Sidebar,
            window,
            cx,
        );
    });
    cx.run_until_parked();

    panel.update(&mut cx, |panel, cx| {
        panel.cleanup_retained_threads(cx);
    });

    panel.read_with(&cx, |panel, _cx| {
        assert!(
            !panel.retained_threads.contains_key(&thread_id_b),
            "idle thread with only an in-flight auto_prompt_task should still be cleaned up"
        );
    });
}

/// End the active stub thread's turn and clear its loading flag so it
/// becomes fully evictable (Idle + loadable + not loading contents).
fn park_idle_thread(
    panel: &Entity<AgentPanel>,
    connection: &StubAgentConnection,
    session_id: &acp_v2::SessionId,
    cx: &mut VisualTestContext,
) {
    cx.update(|_, _cx| {
        connection.end_turn(session_id.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();
    panel.update(cx, |panel, cx| {
        let view = panel
            .active_conversation_view()
            .expect("thread still active when parking");
        let thread_view = view.read(cx).root_thread_view().expect("thread connected");
        thread_view.update(cx, |tv, _cx| {
            tv.is_loading_contents = false;
        });
    });
}

/// Regression test: when the retained-thread hard cap is exceeded, the
/// backstop must evict the oldest EVICTABLE (idle) thread — never a
/// Generating one. Evicting a generating thread drops its
/// ConversationView, which cancels the watchdog and auto_prompt tasks
/// that are the only recovery paths for a wedged stream (issue 003).
#[gpui::test]
async fn test_hard_cap_evicts_idle_thread_not_generating(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;
    let connection = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());

    // Threads 1-5: opened, ended, parked idle in the background.
    let mut idle_thread_ids = Vec::new();
    for _ in 0..5 {
        let (session_id, thread_id) =
            open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
        park_idle_thread(&panel, &connection, &session_id, &mut cx);
        idle_thread_ids.push(thread_id);
    }

    // Threads 6-9: opened and left Generating in the background.
    let mut generating_thread_ids = Vec::new();
    for _ in 0..4 {
        let (_session_id, thread_id) =
            open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
        generating_thread_ids.push(thread_id);
    }

    // Retained = 5 idle + 4 generating = 9 threads (cap is 8); opening
    // the 10th moves thread 9 into retained and trips the hard cap.
    let (_session_id_10, _thread_id_10) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);

    panel.read_with(&cx, |panel, _cx| {
        assert_eq!(
            panel.retained_threads.len(),
            MAX_RETAINED_THREADS,
            "the oldest idle thread should have been evicted to satisfy the cap"
        );
        assert!(
            !panel.retained_threads.contains_key(&idle_thread_ids[0]),
            "the oldest idle thread is the eviction victim"
        );
        for thread_id in &idle_thread_ids[1..] {
            assert!(
                panel.retained_threads.contains_key(thread_id),
                "remaining idle threads stay retained"
            );
        }
        for thread_id in &generating_thread_ids {
            assert!(
                panel.retained_threads.contains_key(thread_id),
                "generating threads must never be evicted by the hard cap"
            );
        }
    });
}

/// When every retained thread is busy, the hard-cap backstop must exceed
/// the cap rather than kill active work.
#[gpui::test]
async fn test_hard_cap_exceeds_cap_when_all_threads_busy(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;
    let connection = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());

    let mut thread_ids = Vec::new();
    for _ in 0..10 {
        let (_session_id, thread_id) =
            open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
        thread_ids.push(thread_id);
    }

    panel.read_with(&cx, |panel, _cx| {
        assert_eq!(
            panel.retained_threads.len(),
            MAX_RETAINED_THREADS + 1,
            "with no evictable thread the cap is exceeded, not enforced by killing work"
        );
        for thread_id in &thread_ids[..MAX_RETAINED_THREADS] {
            assert!(
                panel.retained_threads.contains_key(thread_id),
                "generating threads must survive the hard-cap backstop"
            );
        }
    });
}

/// Issue 006 P2: the generating-thread count (the auto_prompt dispatch
/// cap's input) must span both the active view and retained threads.
#[gpui::test]
async fn test_generating_thread_count_mixed_states(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;
    let connection = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());

    // Thread A: open, then parked idle.
    let (session_a, _id_a) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
    park_idle_thread(&panel, &connection, &session_a, &mut cx);

    // Threads B and C: left Generating (B parks in retained_threads when
    // C opens and becomes active).
    let (_session_b, _id_b) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
    let (session_c, _id_c) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);

    panel.read_with(&cx, |panel, cx| {
        assert_eq!(
            panel.generating_thread_count(cx, None),
            2,
            "generating threads must be counted across retained + active views"
        );
    });

    // Park the active thread; the count must drop to the retained one.
    park_idle_thread(&panel, &connection, &session_c, &mut cx);
    panel.read_with(&cx, |panel, cx| {
        assert_eq!(
            panel.generating_thread_count(cx, None),
            1,
            "only the retained generating thread remains"
        );
    });
}

/// Issue 006 P2: when the concurrent-stream cap is reached, a background
/// chain (focus_new_thread == false) must NOT create its continuation
/// thread — it queues and retries. Once a slot frees, the retry creates
/// the thread. The dispatching view itself is excluded from the count
/// (it is leased during dispatch and, in real flows, has just stopped).
#[gpui::test]
async fn test_stream_cap_defers_background_continuation_until_slot_frees(cx: &mut TestAppContext) {
    let (panel, mut cx) = setup_panel(cx).await;
    let connection = StubAgentConnection::new()
        .with_supports_load_session(true)
        .with_agent_id("loadable-stub".into())
        .with_telemetry_id("loadable-stub".into());

    // Three generating threads: A and B park in retained_threads as C
    // opens and becomes active. Register the panel with the workspace so
    // dispatch's panel lookup resolves (setup_panel keeps it standalone;
    // production panels are registered at startup).
    let (session_a, _id_a) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
    let (_session_b, _id_b) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);
    let (session_c, _id_c) =
        open_generating_thread_with_loadable_connection(&panel, &connection, &mut cx);

    let active_view = panel.update(&mut cx, |panel, _cx| {
        panel
            .active_conversation_view()
            .expect("active view")
            .clone()
    });
    let workspace = active_view.read_with(&cx, |view, _| {
        view.workspace().upgrade().expect("workspace")
    });
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_panel(panel.clone(), window, cx);
    });

    // Sanity: two streams counted (active C excluded from the cap budget).
    panel.read_with(&cx, |panel, cx| {
        assert_eq!(
            panel.generating_thread_count(cx, Some(active_view.entity_id())),
            2,
            "A and B must count against the cap; the leased dispatching view must not"
        );
    });

    let view_count_at_cap = panel.read_with(&cx, |panel, _cx| panel.conversation_views().len());
    assert_eq!(view_count_at_cap, 3);

    let action = auto_prompt::AutoPromptAction {
        from_session_id: session_c,
        from_title: None,
        next_prompt: "continue the work".to_string(),
        work_dirs: None,
        original_user_message: None,
        profile_id: None,
        actual_input_tokens: None,
        approximate_token_count: 0,
        last_assistant_message: None,
        force_new_thread: true,
        focus_new_thread: false,
    };

    // Dispatch from the active conversation view: at cap, this must
    // queue (no new thread, retry timer pending).
    active_view.update_in(&mut cx, |view, window, cx| {
        crate::auto_prompt::dispatch_action(action, view, window, cx);
    });
    cx.run_until_parked();

    panel.read_with(&cx, |panel, _cx| {
        assert_eq!(
            panel.conversation_views().len(),
            view_count_at_cap,
            "at cap the background continuation must be queued, not created"
        );
    });

    // Free a slot: end the retained thread A's turn.
    cx.update(|_, _cx| {
        connection.end_turn(session_a.clone(), acp::StopReason::EndTurn);
    });
    cx.run_until_parked();

    panel.read_with(&cx, |panel, cx| {
        assert_eq!(
            panel.generating_thread_count(cx, Some(active_view.entity_id())),
            1,
            "ending the retained thread's turn must free a stream slot"
        );
    });

    // Advance past the retry delay; the queued dispatch must now create
    // the continuation thread.
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(5_000));
    cx.run_until_parked();

    panel.read_with(&cx, |panel, _cx| {
        assert_eq!(
            panel.conversation_views().len(),
            view_count_at_cap + 1,
            "queued continuation must be created once a stream slot frees"
        );
    });
}
