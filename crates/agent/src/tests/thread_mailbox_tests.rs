use super::*;
use pretty_assertions::assert_eq;

struct MailboxTestThread {
    session_id: acp::SessionId,
    acp_thread: Entity<AcpThread>,
    model: Arc<FakeLanguageModel>,
}

async fn setup_threads(
    count: usize,
    cx: &mut TestAppContext,
) -> (Entity<NativeAgent>, Vec<MailboxTestThread>) {
    init_test(cx);
    cx.update(LanguageModelRegistry::test);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/", json!({ "a": { "b.md": "Lorem" } }))
        .await;
    let project = Project::test(fs.clone(), [path!("/a").as_ref()], cx).await;
    let thread_store = cx.new(|cx| ThreadStore::new(cx));
    let agent =
        cx.update(|cx| NativeAgent::new(thread_store.clone(), Templates::new(), fs.clone(), cx));
    let connection = Rc::new(NativeAgentConnection(agent.clone()));

    let mut threads = Vec::new();
    for _ in 0..count {
        let acp_thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project.clone(), PathList::new(&[Path::new("")]), cx)
            })
            .await
            .unwrap();
        let session_id = acp_thread.read_with(cx, |thread, _| thread.session_id().clone());
        let model = Arc::new(FakeLanguageModel::default());
        agent.update(cx, |agent, cx| {
            agent
                .sessions
                .get(&session_id)
                .unwrap()
                .thread
                .update(cx, |thread, cx| thread.set_model(model.clone(), cx));
        });
        threads.push(MailboxTestThread {
            session_id,
            acp_thread,
            model,
        });
    }
    cx.run_until_parked();
    (agent, threads)
}

fn message_request(to: &acp::SessionId, message: &str) -> ThreadMessageRequest {
    ThreadMessageRequest {
        to: to.to_string(),
        message: message.to_string(),
        wait_for_reply: false,
    }
}

#[gpui::test]
async fn test_message_to_busy_thread_waits_for_turn_end(cx: &mut TestAppContext) {
    let (agent, threads) = setup_threads(2, cx).await;
    let [sender, recipient] = threads.as_slice() else {
        panic!("expected two threads");
    };

    let recipient_turn = recipient
        .acp_thread
        .update(cx, |thread, cx| thread.send_raw("Own work", cx));
    cx.run_until_parked();
    assert_eq!(recipient.model.pending_completions().len(), 1);

    let posted = agent.update(cx, |agent, cx| {
        agent
            .post_thread_message(
                &sender.session_id,
                message_request(&recipient.session_id, "Please review"),
                cx,
            )
            .map(|(_, _, posted)| posted)
    });
    assert!(matches!(
        posted,
        Ok(thread_mailbox::PostedMessage::Queued { position: 1 })
    ));

    let listings = agent.read_with(cx, |agent, cx| {
        agent.list_threads(&sender.session_id, cx).unwrap()
    });
    assert_eq!(listings[0].session_id, sender.session_id);
    assert_eq!(listings[0].kind, ThreadListingKind::Current);
    let recipient_listing = listings
        .iter()
        .find(|listing| listing.session_id == recipient.session_id)
        .unwrap();
    assert_eq!(recipient_listing.status, ThreadListingStatus::Generating);
    assert_eq!(recipient_listing.queued_messages, 1);

    // The queued mail must not cancel the turn already in flight.
    recipient
        .model
        .send_last_completion_stream_text_chunk("Done");
    recipient.model.end_last_completion_stream();
    recipient_turn.await.unwrap();
    cx.run_until_parked();

    let pending = recipient.model.pending_completions();
    assert_eq!(pending.len(), 1, "mail should start exactly one new turn");
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("Please review"), "{prompt}");
    assert!(
        prompt.contains(&format!("session_id=\"{}\"", sender.session_id)),
        "{prompt}"
    );
    assert!(agent.read_with(cx, |agent, _| agent.mailboxes.is_empty()));
}

#[gpui::test]
async fn test_send_message_tool_waits_for_idle_reply(cx: &mut TestAppContext) {
    let (_agent, threads) = setup_threads(2, cx).await;
    let [sender, recipient] = threads.as_slice() else {
        panic!("expected two threads");
    };

    let sender_turn = sender
        .acp_thread
        .update(cx, |thread, cx| thread.send_raw("Ask the other thread", cx));
    cx.run_until_parked();

    let tool_input = json!({
        "to": recipient.session_id.to_string(),
        "message": "ping",
        "wait_for_reply": true,
    });
    sender
        .model
        .send_last_completion_stream_event(LanguageModelCompletionEvent::ToolUse(
            LanguageModelToolUse {
                id: "send_1".into(),
                name: SendMessageTool::NAME.into(),
                raw_input: tool_input.to_string(),
                input: language_model::LanguageModelToolUseInput::Json(tool_input),
                is_input_complete: true,
                thought_signature: None,
            },
        ));
    sender.model.end_last_completion_stream();
    cx.run_until_parked();

    // The idle recipient started a turn immediately.
    assert_eq!(recipient.model.pending_completions().len(), 1);
    recipient
        .model
        .send_last_completion_stream_text_chunk("pong");
    recipient.model.end_last_completion_stream();
    cx.run_until_parked();

    let follow_up = sender.model.pending_completions();
    assert_eq!(follow_up.len(), 1);
    let tool_result = follow_up[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            MessageContent::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("send_message tool result");
    let tool_result = tool_result
        .iter()
        .map(|content| match content {
            language_model::LanguageModelToolResultContent::Text(text) => text.to_string(),
            other => panic!("unexpected tool result {other:?}"),
        })
        .collect::<String>();
    assert!(
        tool_result.contains("\"delivery\":\"delivered\""),
        "{tool_result}"
    );
    assert!(tool_result.contains("\"reply\":\"pong\""), "{tool_result}");

    sender.model.end_last_completion_stream();
    sender_turn.await.unwrap();
}

#[gpui::test]
async fn test_list_threads_tool_runs_during_turn(cx: &mut TestAppContext) {
    let (agent, threads) = setup_threads(2, cx).await;
    let [sender, recipient] = threads.as_slice() else {
        panic!("expected two threads");
    };

    let sender_turn = sender
        .acp_thread
        .update(cx, |thread, cx| thread.send_raw("List the threads", cx));
    cx.run_until_parked();

    let tool_input = json!({});
    sender
        .model
        .send_last_completion_stream_event(LanguageModelCompletionEvent::ToolUse(
            LanguageModelToolUse {
                id: "list_1".into(),
                name: ListThreadsTool::NAME.into(),
                raw_input: tool_input.to_string(),
                input: language_model::LanguageModelToolUseInput::Json(tool_input),
                is_input_complete: true,
                thought_signature: None,
            },
        ));
    sender.model.end_last_completion_stream();
    cx.run_until_parked();

    let follow_up = sender.model.pending_completions();
    assert_eq!(follow_up.len(), 1);
    let tool_result = follow_up[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            MessageContent::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("list_threads tool result");
    let tool_result = tool_result
        .iter()
        .map(|content| match content {
            language_model::LanguageModelToolResultContent::Text(text) => text.to_string(),
            other => panic!("unexpected tool result {other:?}"),
        })
        .collect::<String>();
    assert!(
        tool_result.contains(&format!("\"session_id\":\"{}\"", sender.session_id)),
        "{tool_result}"
    );
    assert!(tool_result.contains("\"kind\":\"self\""), "{tool_result}");
    assert!(
        tool_result.contains(&format!("\"session_id\":\"{}\"", recipient.session_id)),
        "{tool_result}"
    );

    sender.model.end_last_completion_stream();
    sender_turn.await.unwrap();
    assert!(agent.read_with(cx, |agent, _| agent.mailboxes.is_empty()));
}

#[gpui::test]
async fn test_thread_cannot_message_itself(cx: &mut TestAppContext) {
    let (agent, threads) = setup_threads(1, cx).await;
    let thread = &threads[0];
    let result = agent.update(cx, |agent, cx| {
        agent
            .post_thread_message(
                &thread.session_id,
                message_request(&thread.session_id, "hi"),
                cx,
            )
            .map(|_| ())
    });
    assert!(result.is_err());
    assert!(agent.read_with(cx, |agent, _| agent.mailboxes.is_empty()));
}
