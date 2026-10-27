use super::*;

/// Verifies the native-agent branch path: `seed_history` forks another
/// thread's message history into a fresh session as *real rendered turns*
/// (separate user/assistant entries), not a flattened transcript. This is
/// the automated stand-in for the manual smoke item "native Branch forks
/// real history with working tool cards" from `.plans/009`.
///
/// `iterations` because the path is scheduler-sensitive (async session
/// creation + replay event draining); verified stable across seeds.
#[gpui::test(iterations = 8)]
async fn test_seed_history_forks_real_turns(cx: &mut TestAppContext) {
    let fake = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/", json!({})).await;
    let project = Project::test(fs.clone(), [path!("/").as_ref()], cx).await;
    let thread_store = cx.new(|cx| ThreadStore::new(cx));
    let agent =
        cx.update(|cx| NativeAgent::new(thread_store.clone(), Templates::new(), fs.clone(), cx));
    let connection = Rc::new(NativeAgentConnection(agent.clone()));

    // Session A: drive one real user -> assistant exchange so A carries
    // genuine rendered history (not just pushed messages).
    let acp_thread_a = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&[Path::new("")]), cx)
        })
        .await
        .unwrap();
    let session_a_id = acp_thread_a.read_with(cx, |thread, _| thread.session_id().clone());
    let thread_a = agent.read_with(cx, |agent, _| {
        agent.sessions.get(&session_a_id).unwrap().thread.clone()
    });

    let model = fake.model("fake");
    thread_a.update(cx, |thread, cx| thread.set_model(model.clone(), cx));

    let send_a = cx.update(|cx| {
        connection.send_and_wait_for_completion(
            acp_thread_a.clone(),
            vec!["Hello from A".into()],
            cx,
        )
    });
    cx.run_until_parked();
    fake.send_last_text(&model, "Hi back from A.");
    fake.end_last(&model);
    cx.run_until_parked();
    send_a.await.unwrap();

    // Snapshot A's source-of-truth messages and its rendered markdown.
    let source_messages = thread_a.read_with(cx, |thread, _| thread.messages().to_vec());
    let markdown_a = acp_thread_a.read_with(cx, |thread, cx| thread.to_markdown(cx));
    assert!(
        markdown_a.contains("Hello from A") && markdown_a.contains("Hi back from A."),
        "session A should have the exchange, got: {markdown_a}"
    );

    // Session B: freshly created, empty.
    let acp_thread_b = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&[Path::new("")]), cx)
        })
        .await
        .unwrap();
    let session_b_id = acp_thread_b.read_with(cx, |thread, _| thread.session_id().clone());
    assert_eq!(
        acp_thread_b.read_with(cx, |thread, _| thread.entries().len()),
        0,
        "session B should start empty"
    );

    // Fork A's history into B, then let the replayed events drain.
    let seed = cx.update(|cx| connection.seed_history(&session_b_id, source_messages, cx));
    seed.await.unwrap();
    cx.run_until_parked();

    // B now renders the SAME turns as A — proof the fork preserved
    // user/assistant boundaries rather than flattening to a transcript.
    let markdown_b = acp_thread_b.read_with(cx, |thread, cx| thread.to_markdown(cx));
    assert_eq!(
        markdown_b, markdown_a,
        "forked session B should render identically to source A"
    );
    assert_eq!(
        acp_thread_b.read_with(cx, |thread, _| thread.entries().len()),
        acp_thread_a.read_with(cx, |thread, _| thread.entries().len()),
        "forked session B should have the same entry count as A"
    );

    // And B's underlying thread carries the messages verbatim.
    let thread_b = agent.read_with(cx, |agent, _| {
        agent.sessions.get(&session_b_id).unwrap().thread.clone()
    });
    assert_eq!(
        thread_b.read_with(cx, |thread, _| thread.messages().len()),
        thread_a.read_with(cx, |thread, _| thread.messages().len()),
        "session B's native thread should hold the same message count as A"
    );
}

#[gpui::test]
async fn test_thread_summary_falls_back_when_summary_unavailable(cx: &mut TestAppContext) {
    let fake = init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/",
        json!({
            "a": {
                "file.txt": "hello"
            }
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/a").as_ref()], cx).await;
    let thread_store = cx.new(|cx| ThreadStore::new(cx));
    let agent =
        cx.update(|cx| NativeAgent::new(thread_store.clone(), Templates::new(), fs.clone(), cx));
    let connection = Rc::new(NativeAgentConnection(agent.clone()));

    let acp_thread = cx
        .update(|cx| {
            connection
                .clone()
                .new_session(project.clone(), PathList::new(&[Path::new("")]), cx)
        })
        .await
        .unwrap();
    let session_id = acp_thread.read_with(cx, |thread, _| thread.session_id().clone());
    let thread = agent.read_with(cx, |agent, _| {
        agent.sessions.get(&session_id).unwrap().thread.clone()
    });

    let model = fake.model("fake");
    // Force no summarization model: register_session sets one from the
    // global registry (see init_test -> LanguageModelRegistry::test), so
    // we override it to None afterwards. thread.summary() should then
    // return None, and thread_summary should fall back to an empty
    // SharedString instead of erroring.
    thread.update(cx, |thread, cx| {
        thread.set_model(model.clone(), cx);
        thread.set_summarization_model(None, cx);
    });

    let send = acp_thread.update(cx, |thread, cx| thread.send(vec!["hello".into()], cx));
    let send = cx.foreground_executor().spawn(send);
    cx.run_until_parked();

    let request = fake.pending_completions_for(&model).pop().unwrap();
    fake.send_text(&model, &request, "world");
    fake.end_stream(&model, &request);
    send.await.unwrap();
    cx.run_until_parked();

    let summary = agent.update(cx, |agent, cx| {
        agent.thread_summary(session_id.clone(), project.clone(), cx)
    });
    cx.run_until_parked();

    // No "Failed to generate summary" error: caller keeps the @thread link.
    let summary = summary.await.expect("thread_summary should not error");
    assert_eq!(
        summary.as_ref(),
        "",
        "thread_summary should fall back to empty summary when thread.summary() returns None"
    );

    drop(thread);
    drop(acp_thread);
    release_dropped_entities(cx);
}
