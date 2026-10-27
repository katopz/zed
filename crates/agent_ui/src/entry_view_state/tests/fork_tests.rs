//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

#[gpui::test]
async fn test_board_notification_keeps_entry_slots_dense(cx: &mut TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({})).await;
    let project = Project::test(fs, [Path::new(path!("/project"))], cx).await;

    let (multi_workspace, cx) =
        cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

    let connection = Rc::new(StubAgentConnection::new());
    let thread = cx
        .update(|_, cx| {
            connection.clone().new_session(
                project.clone(),
                PathList::new(&[Path::new(path!("/project"))]),
                cx,
            )
        })
        .await
        .unwrap();
    let session_id = thread.update(cx, |thread, _| thread.session_id().clone());

    cx.update(|_, cx| {
        connection.send_update(
            session_id.clone(),
            acp_v1::SessionUpdate::AgentMessageChunk(acp_v1::ContentChunk::new(
                acp_v1::ContentBlock::Text(acp_v1::TextContent::new("hello")),
            )),
            cx,
        );
    });
    thread.update(cx, |thread, cx| {
        thread.push_agent_board_notification("peer status update", cx)
    });
    let _response_task = thread.update(cx, |thread, cx| {
        thread
            .request_elicitation(
                acp_v2::CreateElicitationRequest::new(
                    acp_v2::ElicitationFormMode::new(
                        acp_v2::ElicitationSessionScope::new(session_id.clone()),
                        acp_v2::ElicitationSchema::new().string("name", true),
                    ),
                    "Provide a name",
                ),
                cx,
            )
            .unwrap()
    });
    assert_eq!(thread.read_with(cx, |thread, _| thread.entries().len()), 3);

    let view_state = cx.new(|_cx| {
        EntryViewState::new(
            workspace.downgrade(),
            project.downgrade(),
            None,
            Arc::new(RwLock::new(SessionCapabilities::default())),
            "Test Agent".into(),
        )
    });

    view_state.update_in(cx, |view_state, window, cx| {
        view_state.sync_entry(0, &thread, window, cx);
        view_state.sync_entry(1, &thread, window, cx);
        view_state.sync_entry(2, &thread, window, cx);
    });

    view_state.read_with(cx, |view_state, _cx| {
        assert!(matches!(
            view_state.entry(0),
            Some(Entry::AssistantMessage(_))
        ));
        assert!(matches!(
            view_state.entry(1),
            Some(Entry::ContextCompaction)
        ));
        assert!(matches!(
            view_state.entry(2),
            Some(Entry::Elicitation { .. })
        ));
    });
}
