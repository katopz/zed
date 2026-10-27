use super::*;
use pretty_assertions::assert_eq;

struct MailboxTestThread {
    session_id: acp_v2::SessionId,
    acp_thread: Entity<AcpThread>,
    model: LanguageModel,
}

struct MailboxTestSetup {
    agent: Entity<NativeAgent>,
    connection: Rc<NativeAgentConnection>,
    fake: Arc<FakeLanguageModelProvider>,
    threads: Vec<MailboxTestThread>,
}

impl MailboxTestSetup {
    fn send(
        &self,
        thread: &MailboxTestThread,
        message: &str,
        cx: &mut TestAppContext,
    ) -> Task<Result<acp_v2::StopReason>> {
        cx.update(|cx| {
            self.connection.send_and_wait_for_completion(
                thread.acp_thread.clone(),
                vec![message.into()],
                cx,
            )
        })
    }
}

async fn setup_threads(count: usize, cx: &mut TestAppContext) -> MailboxTestSetup {
    let fake = init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/", json!({ "a": { "b.md": "Lorem" } }))
        .await;
    let project = Project::test(fs.clone(), [path!("/a").as_ref()], cx).await;
    let thread_store = cx.new(|cx| ThreadStore::new(cx));
    let agent =
        cx.update(|cx| NativeAgent::new(thread_store.clone(), Templates::new(), fs.clone(), cx));
    let connection = Rc::new(NativeAgentConnection(agent.clone()));

    let mut threads = Vec::new();
    for index in 0..count {
        let acp_thread = cx
            .update(|cx| {
                connection
                    .clone()
                    .new_session(project.clone(), PathList::new(&[Path::new("")]), cx)
            })
            .await
            .unwrap();
        let session_id = acp_thread.read_with(cx, |thread, _| thread.session_id().clone());
        // Each thread gets its own model id so pending completions can be
        // told apart per thread.
        let model = fake.model(&format!("mailbox-{index}"));
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
    MailboxTestSetup {
        agent,
        connection,
        fake,
        threads,
    }
}

fn message_request(to: &acp_v2::SessionId, message: &str) -> ThreadMessageRequest {
    ThreadMessageRequest {
        to: to.to_string(),
        message: message.to_string(),
        wait_for_reply: false,
    }
}

#[gpui::test]
async fn test_message_to_busy_thread_waits_for_turn_end(cx: &mut TestAppContext) {
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let [sender, recipient] = setup.threads.as_slice() else {
        panic!("expected two threads");
    };

    let recipient_turn = setup.send(recipient, "Own work", cx);
    cx.run_until_parked();
    assert_eq!(fake.pending_completions_for(&recipient.model).len(), 1);

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
    fake.send_last_text(&recipient.model, "Done");
    fake.end_last(&recipient.model);
    assert_eq!(recipient_turn.await.unwrap(), acp_v2::StopReason::EndTurn);
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&recipient.model);
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
    let setup = setup_threads(2, cx).await;
    let fake = &setup.fake;
    let [sender, recipient] = setup.threads.as_slice() else {
        panic!("expected two threads");
    };

    let sender_turn = setup.send(sender, "Ask the other thread", cx);
    cx.run_until_parked();

    let tool_input = json!({
        "to": recipient.session_id.to_string(),
        "message": "ping",
        "wait_for_reply": true,
    });
    fake.send_last_event(
        &sender.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "send_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&sender.model);
    cx.run_until_parked();

    // The idle recipient started a turn immediately.
    assert_eq!(fake.pending_completions_for(&recipient.model).len(), 1);
    fake.send_last_text(&recipient.model, "pong");
    fake.end_last(&recipient.model);
    cx.run_until_parked();

    let follow_up = fake.pending_completions_for(&sender.model);
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

    fake.end_last(&sender.model);
    sender_turn.await.unwrap();
}

#[gpui::test]
async fn test_list_threads_tool_runs_during_turn(cx: &mut TestAppContext) {
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let [sender, recipient] = setup.threads.as_slice() else {
        panic!("expected two threads");
    };

    let sender_turn = setup.send(sender, "List the threads", cx);
    cx.run_until_parked();

    let tool_input = json!({});
    fake.send_last_event(
        &sender.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "list_1".into(),
            name: ListThreadsTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&sender.model);
    cx.run_until_parked();

    let follow_up = fake.pending_completions_for(&sender.model);
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

    fake.end_last(&sender.model);
    sender_turn.await.unwrap();
    assert!(agent.read_with(cx, |agent, _| agent.mailboxes.is_empty()));
}

#[gpui::test]
async fn test_thread_cannot_message_itself(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let agent = &setup.agent;
    let thread = &setup.threads[0];
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
