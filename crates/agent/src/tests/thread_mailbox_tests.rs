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
        deliver_at: None,
        mode: ::agent_mail::MailMode::Queued,
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

/// Enables agent-mail on `agent` against a fresh rig named `m3` in a temp dir,
/// returning a second handle that plays the remote sender.
fn enable_agent_mail(
    agent: &Entity<NativeAgent>,
    cx: &mut TestAppContext,
) -> (tempfile::TempDir, ::agent_mail::Mailroom) {
    let home = tempfile::tempdir().unwrap();
    let sender = ::agent_mail::Mailroom::new(home.path().to_path_buf());
    sender.init("m3").unwrap();
    let mailroom = ::agent_mail::Mailroom::new(home.path().to_path_buf());
    agent.update(cx, |agent, cx| {
        agent.agent_mail = Some(agent_mail_bridge::AgentMailState::start(
            mailroom,
            "m3".into(),
            cx,
        ));
    });
    (home, sender)
}

fn mail_to(
    sender: &::agent_mail::Mailroom,
    session_id: &acp_v2::SessionId,
    body: &str,
) -> ::agent_mail::Envelope {
    let to = ::agent_mail::Address::parse(&format!(
        "{}@m3",
        agent_mail_bridge::native_thread_mail_name(session_id)
    ))
    .unwrap();
    let envelope = sender
        .compose(&to, "zed-bb@m3", body.to_string(), None)
        .unwrap();
    sender.send(&envelope).unwrap();
    envelope
}

#[gpui::test]
async fn test_agent_mail_wakes_idle_thread_and_reply_goes_out(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    let address = format!(
        "{}@m3",
        agent_mail_bridge::native_thread_mail_name(&thread.session_id)
    );
    let listings = agent.read_with(cx, |agent, cx| {
        agent.list_threads(&thread.session_id, cx).unwrap()
    });
    assert_eq!(listings[0].mail_address.as_deref(), Some(address.as_str()));

    mail_to(&sender, &thread.session_id, "ping from another machine");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(pending.len(), 1, "mail should wake the idle thread");
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("ping from another machine"), "{prompt}");
    assert!(prompt.contains("session_id=\"zed-bb@m3\""), "{prompt}");

    let tool_input = json!({ "to": "zed-bb@m3", "message": "pong" });
    fake.send_last_event(
        &thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "mail_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&thread.model);
    cx.run_until_parked();

    let replies = sender.claim(&["zed-bb".to_string()]).unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].body, "pong");
    assert_eq!(replies[0].from, address);
}

#[gpui::test]
async fn test_agent_mail_for_busy_thread_waits_for_turn_end(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    let own_turn = setup.send(thread, "Own work", cx);
    cx.run_until_parked();
    mail_to(&sender, &thread.session_id, "status?");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(fake.pending_completions_for(&thread.model).len(), 1);
    assert_eq!(
        agent.read_with(cx, |agent, _| agent
            .mailboxes
            .get(&thread.session_id)
            .map_or(0, Vec::len)),
        1
    );

    fake.send_last_text(&thread.model, "Done");
    fake.end_last(&thread.model);
    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::EndTurn);
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(pending.len(), 1, "mail should start exactly one new turn");
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("status?"), "{prompt}");
}

fn mail_to_with_mode(
    sender: &::agent_mail::Mailroom,
    session_id: &acp_v2::SessionId,
    body: &str,
    mode: ::agent_mail::MailMode,
) -> ::agent_mail::Envelope {
    let to = ::agent_mail::Address::parse(&format!(
        "{}@m3",
        agent_mail_bridge::native_thread_mail_name(session_id)
    ))
    .unwrap();
    let mut envelope = sender
        .compose(&to, "zed-bb@m3", body.to_string(), None)
        .unwrap();
    envelope.mode = mode;
    sender.send(&envelope).unwrap();
    envelope
}

#[gpui::test]
async fn test_agent_mail_urgent_mode_preempts_busy_thread(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    let own_turn = setup.send(thread, "Long-running work", cx);
    cx.run_until_parked();
    mail_to_with_mode(
        &sender,
        &thread.session_id,
        "drop everything: build broke",
        ::agent_mail::MailMode::Urgent,
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    // The preempted turn resolves as cancelled...
    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::Cancelled);
    cx.run_until_parked();

    // ...and the new turn carries the mail, with the preemption named. The
    // preempted turn's stream entry lingers in the fake provider (a cancelled
    // stream is closed but not `end_last`ed), so the urgent turn is the LAST
    // pending completion.
    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(pending.len(), 2, "preempted stream + one urgent turn");
    let prompt = pending
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .string_contents();
    assert!(prompt.contains("drop everything: build broke"), "{prompt}");
    assert!(
        prompt.contains("mode=urgent"),
        "the preemption must be named in the thread: {prompt}"
    );
}

#[gpui::test]
async fn test_agent_mail_urgent_mode_on_idle_thread_delivers_directly(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    mail_to_with_mode(
        &sender,
        &thread.session_id,
        "urgent ping",
        ::agent_mail::MailMode::Urgent,
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(pending.len(), 1, "idle target: urgent is a plain delivery");
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("urgent ping"), "{prompt}");
    assert!(
        !prompt.contains("mode=urgent"),
        "nothing was preempted, so no notice: {prompt}"
    );
}

#[gpui::test]
async fn test_agent_mail_steering_mode_ends_busy_turn_at_boundary(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    let own_turn = setup.send(thread, "Own work", cx);
    cx.run_until_parked();
    mail_to_with_mode(
        &sender,
        &thread.session_id,
        "steering: check the tests now",
        ::agent_mail::MailMode::Steering,
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    // Still mid-turn: the mail waits, but the boundary flag is set.
    assert_eq!(
        agent.read_with(cx, |agent, _| agent
            .mailboxes
            .get(&thread.session_id)
            .map_or(0, Vec::len)),
        1
    );
    agent.read_with(cx, |agent, cx| {
        let session = agent.sessions.get(&thread.session_id).unwrap();
        assert!(session.thread.read(cx).end_turn_at_next_boundary());
    });

    // End the stream: the turn stops at the boundary...
    fake.send_last_text(&thread.model, "Winding down");
    fake.end_last(&thread.model);
    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::EndTurn);
    cx.run_until_parked();

    // ...and the mail delivers as the next turn.
    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(
        pending.len(),
        1,
        "steering mail should start exactly one turn"
    );
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("steering: check the tests now"), "{prompt}");
}

#[gpui::test]
async fn test_agent_mail_fyi_mode_never_wakes_idle_thread(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (_home, sender) = enable_agent_mail(agent, cx);

    // An idle thread must stay idle: fyi parks until the thread's next
    // natural turn end, costing nothing now.
    mail_to_with_mode(
        &sender,
        &thread.session_id,
        "fyi: the fleet watchdog noted heavy load",
        ::agent_mail::MailMode::Fyi,
    );
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        fake.pending_completions_for(&thread.model).is_empty(),
        "fyi must not start a turn in an idle thread"
    );
    assert_eq!(
        agent.read_with(cx, |agent, _| agent
            .mailboxes
            .get(&thread.session_id)
            .map_or(0, Vec::len)),
        1,
        "the fyi should be parked in the mailbox"
    );

    // The user talks to the thread; at that turn's end the parked fyi
    // delivers as one queued slice.
    let own_turn = setup.send(thread, "Do some real work", cx);
    cx.run_until_parked();
    assert_eq!(fake.pending_completions_for(&thread.model).len(), 1);
    fake.send_last_text(&thread.model, "Done");
    fake.end_last(&thread.model);
    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::EndTurn);
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(
        pending.len(),
        1,
        "the only in-flight turn is the fyi delivery"
    );
    let prompt = pending
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .string_contents();
    assert!(
        prompt.contains("fyi: the fleet watchdog noted heavy load"),
        "{prompt}"
    );
    assert!(
        prompt.contains("no reply needed"),
        "the fyi hint must tell the thread not to answer: {prompt}"
    );
}

#[gpui::test]
async fn test_agent_mail_broadcast_respects_liveness_cutoff(cx: &mut TestAppContext) {
    // A thread that (re)entered after a blast landed must not process it —
    // this is what stops a clicked stopped thread from waking on old mail.
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let live_thread = &setup.threads[0];
    let reentered_thread = &setup.threads[1];
    let (_home, sender) = enable_agent_mail(agent, cx);

    // Let one poller tick run so both sessions have liveness cutoffs stamped
    // before any mail is sent (chrono reads the real clock, so ordering is
    // otherwise racy against the executor's timers).
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    // Both threads are live; a broadcast wakes both (the known blast cost).
    let broadcast_to = ::agent_mail::Address::parse("all@m3").unwrap();
    let blast = sender
        .compose(&broadcast_to, "watch@amo", "blast one".into(), None)
        .unwrap();
    sender.send(&blast).unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(fake.pending_completions_for(&live_thread.model).len(), 1);
    assert_eq!(
        fake.pending_completions_for(&reentered_thread.model).len(),
        1
    );
    // Wind the blast-one turns down so the next delivery starts clean turns.
    fake.send_last_text(&live_thread.model, "noted");
    fake.end_last(&live_thread.model);
    fake.send_last_text(&reentered_thread.model, "noted");
    fake.end_last(&reentered_thread.model);
    cx.run_until_parked();

    // The re-entered thread's cutoff moves past the next blast (as if the
    // user clicked it open again now, after this next mail landed).
    agent.update(cx, |agent, _| {
        if let Some(state) = agent.agent_mail.as_mut() {
            state.live_since.insert(
                reentered_thread.session_id.clone(),
                chrono::Utc::now() + chrono::Duration::minutes(1),
            );
        }
    });
    let blast = sender
        .compose(&broadcast_to, "watch@amo", "blast two".into(), None)
        .unwrap();
    sender.send(&blast).unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let live_pending = fake.pending_completions_for(&live_thread.model);
    assert_eq!(live_pending.len(), 1, "the live thread claims blast two");
    assert!(
        live_pending
            .last()
            .unwrap()
            .messages
            .last()
            .unwrap()
            .string_contents()
            .contains("blast two")
    );
    assert!(
        fake.pending_completions_for(&reentered_thread.model)
            .is_empty(),
        "the re-entered thread must skip the blast that predates its cutoff"
    );
    // And it is marked seen: winding down the live turn (and re-running the
    // poller) must not resurrect the skipped blast for the re-entered one.
    fake.send_last_text(&live_thread.model, "done");
    fake.end_last(&live_thread.model);
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert!(
        fake.pending_completions_for(&reentered_thread.model)
            .is_empty(),
        "the skipped blast must stay skipped"
    );
}

#[gpui::test]
async fn test_send_message_mode_urgent_preempts_busy_target(cx: &mut TestAppContext) {
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let sender_thread = &setup.threads[0];
    let target = &setup.threads[1];
    let (_home, _sender) = enable_agent_mail(agent, cx);

    let own_turn = setup.send(target, "Busy work", cx);
    cx.run_until_parked();
    let sender_turn = setup.send(sender_thread, "Send a message", cx);
    cx.run_until_parked();

    let tool_input = json!({
        "to": target.session_id.to_string(),
        "message": "preempting note",
        "mode": "urgent",
    });
    fake.send_last_event(
        &sender_thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "urgent_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&sender_thread.model);
    cx.run_until_parked();
    let _ = sender_turn;

    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::Cancelled);
    // The preempting delivery polls for idle on 100 ms timers.
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    let pending = fake.pending_completions_for(&target.model);
    assert_eq!(pending.len(), 2, "preempted stream + one urgent turn");
    let prompt = pending
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .string_contents();
    assert!(prompt.contains("preempting note"), "{prompt}");
    assert!(prompt.contains("mode=urgent"), "{prompt}");
}

#[gpui::test]
async fn test_send_message_mode_steering_ends_busy_target_turn(cx: &mut TestAppContext) {
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let sender_thread = &setup.threads[0];
    let target = &setup.threads[1];
    let (_home, _sender) = enable_agent_mail(agent, cx);

    let own_turn = setup.send(target, "Busy work", cx);
    cx.run_until_parked();
    let sender_turn = setup.send(sender_thread, "Send a message", cx);
    cx.run_until_parked();

    let tool_input = json!({
        "to": target.session_id.to_string(),
        "message": "steering note",
        "mode": "steering",
    });
    fake.send_last_event(
        &sender_thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "steer_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&sender_thread.model);
    cx.run_until_parked();
    let _ = sender_turn;
    agent.read_with(cx, |agent, cx| {
        let session = agent.sessions.get(&target.session_id).unwrap();
        assert!(session.thread.read(cx).end_turn_at_next_boundary());
    });

    fake.send_last_text(&target.model, "Wrapping up");
    fake.end_last(&target.model);
    assert_eq!(own_turn.await.unwrap(), acp_v2::StopReason::EndTurn);
    cx.run_until_parked();
    let pending = fake.pending_completions_for(&target.model);
    assert_eq!(pending.len(), 1, "one steering turn");
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("steering note"), "{prompt}");
}

#[gpui::test]
async fn test_send_message_rejects_unknown_mode(cx: &mut TestAppContext) {
    let setup = setup_threads(2, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let sender_thread = &setup.threads[0];
    let target = &setup.threads[1];
    let (_home, _sender) = enable_agent_mail(agent, cx);
    let _sender_turn = setup.send(sender_thread, "Send a message", cx);
    cx.run_until_parked();

    let tool_input = json!({
        "to": target.session_id.to_string(),
        "message": "note",
        "mode": "nudge",
    });
    fake.send_last_event(
        &sender_thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "bad_mode_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&sender_thread.model);
    cx.run_until_parked();

    agent.read_with(cx, |agent, cx| {
        let session = agent.sessions.get(&sender_thread.session_id).unwrap();
        let thread = session.thread.read(cx);
        let messages = thread.last_message().unwrap();
        // The tool call surfaces the error; nothing was delivered to the target.
        let _ = messages;
        let _ = cx;
    });
    assert!(fake.pending_completions_for(&target.model).is_empty());
}

#[gpui::test]
async fn test_thread_schedules_its_own_wake_up(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (home, _sender) = enable_agent_mail(agent, cx);
    let name = agent_mail_bridge::native_thread_mail_name(&thread.session_id);

    let turn = setup.send(thread, "Start the long job, then check it later", cx);
    cx.run_until_parked();
    let tool_input = json!({
        "to": format!("{name}@m3"),
        "message": "check the long job",
        "deliver_at": "8h",
    });
    fake.send_last_event(
        &thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "wake_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&thread.model);
    cx.run_until_parked();
    fake.send_last_text(&thread.model, "Scheduled; going idle.");
    fake.end_last(&thread.model);
    assert_eq!(turn.await.unwrap(), acp_v2::StopReason::EndTurn);

    // Idle and not yet due: polling must not wake the thread.
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(fake.pending_completions_for(&thread.model).is_empty());

    // The due time arrives (simulated by back-dating the parked file).
    let later = home.path().join("inbox").join(&name).join("later");
    let parked = std::fs::read_dir(&later)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let file_name = parked.file_name().unwrap().to_str().unwrap().to_string();
    let (_, rest) = file_name.split_once('_').unwrap();
    std::fs::rename(&parked, later.join(format!("1_{rest}"))).unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let pending = fake.pending_completions_for(&thread.model);
    assert_eq!(
        pending.len(),
        1,
        "the scheduled mail should wake the thread"
    );
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("check the long job"), "{prompt}");
}

/// A thread that mails out (here: schedules its own wake) and ends its turn
/// is parked waiting on that mail. The sidebar's waiting indicator keys off
/// this flag: set when the mail goes out, cleared when mail is claimed for
/// the thread's inbox.
#[gpui::test]
async fn test_outgoing_mail_marks_thread_waiting_until_wake_arrives(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let thread = &setup.threads[0];
    let (home, _sender) = enable_agent_mail(agent, cx);
    let name = agent_mail_bridge::native_thread_mail_name(&thread.session_id);
    let assert_waiting = |thread: &MailboxTestThread, cx: &mut TestAppContext, expected: bool| {
        thread.acp_thread.read_with(cx, |thread, _| {
            assert_eq!(
                thread.is_waiting_for_mail(),
                expected,
                "waiting-for-mail flag mismatch"
            );
        });
    };

    assert_waiting(thread, cx, false);

    let turn = setup.send(thread, "Start the long job, then check it later", cx);
    cx.run_until_parked();
    let tool_input = json!({
        "to": format!("{name}@m3"),
        "message": "check the long job",
        "deliver_at": "8h",
    });
    fake.send_last_event(
        &thread.model,
        LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
            id: "wake_1".into(),
            name: SendMessageTool::NAME.into(),
            raw_input: tool_input.to_string(),
            input: language_model::LanguageModelToolUseInput::Json(tool_input),
            is_input_complete: true,
            thought_signature: None,
        }),
    );
    fake.end_last(&thread.model);
    cx.run_until_parked();
    fake.send_last_text(&thread.model, "Scheduled; going idle.");
    fake.end_last(&thread.model);
    assert_eq!(turn.await.unwrap(), acp_v2::StopReason::EndTurn);

    // The turn ended with the wake still out: the thread must read as waiting
    // while it sits idle.
    assert_waiting(thread, cx, true);

    // The due time arrives (simulated by back-dating the parked file); the
    // wake is claimed into the inbox, which clears the waiting state.
    let later = home.path().join("inbox").join(&name).join("later");
    let parked = std::fs::read_dir(&later)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let file_name = parked.file_name().unwrap().to_str().unwrap().to_string();
    let (_, rest) = file_name.split_once('_').unwrap();
    std::fs::rename(&parked, later.join(format!("1_{rest}"))).unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    assert_eq!(
        fake.pending_completions_for(&thread.model).len(),
        1,
        "the scheduled mail should wake the thread"
    );
    assert_waiting(thread, cx, false);
}

fn enable_mail_spawn(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let mut settings = agent_settings::AgentSettings::get_global(cx).clone();
        settings.fork.agent_mail_spawn_threads = true;
        agent_settings::AgentSettings::override_global(settings, cx);
    });
}

fn mail_to_name(sender: &::agent_mail::Mailroom, name: &str, body: &str) -> ::agent_mail::Envelope {
    let to = ::agent_mail::Address::parse(&format!("{name}@m3")).unwrap();
    let envelope = sender
        .compose(&to, "zed-bb@m3", body.to_string(), None)
        .unwrap();
    sender.send(&envelope).unwrap();
    envelope
}

#[gpui::test]
async fn test_agent_mail_spawns_thread_for_unknown_name(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let (_home, sender) = enable_agent_mail(agent, cx);
    enable_mail_spawn(cx);

    mail_to_name(&sender, "task-foo", "run the batch job");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let (spawned_session, mail_address, title, model) = agent.read_with(cx, |agent, cx| {
        let (session_id, name) = agent
            .agent_mail
            .as_ref()
            .and_then(|state| state.spawned_names.iter().next())
            .map(|(session_id, name)| (session_id.clone(), name.clone()))
            .expect("mail to an unknown inbox should spawn a thread");
        assert_eq!(name, "task-foo");
        let session = agent.sessions.get(&session_id).unwrap();
        let thread = session.thread.read(cx);
        (
            session_id.clone(),
            agent.agent_mail_address(&session_id).unwrap(),
            thread.title().unwrap().to_string(),
            thread.model().unwrap().clone(),
        )
    });
    assert_eq!(mail_address, "task-foo@m3");
    assert_eq!(title, "[M] task-foo");

    let pending = fake.pending_completions_for(&model);
    assert_eq!(
        pending.len(),
        1,
        "the mail body should start the spawned thread's first turn"
    );
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("run the batch job"), "{prompt}");
    assert!(prompt.contains("task-foo@m3"), "{prompt}");

    // A follow-up to the same inbox lands on the same thread — no respawn.
    mail_to_name(&sender, "task-foo", "second note");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    agent.read_with(cx, |agent, _| {
        assert_eq!(agent.sessions.len(), 2);
        assert_eq!(
            agent.mailboxes.get(&spawned_session).map_or(0, Vec::len),
            1,
            "the busy spawned thread queues the follow-up"
        );
    });
}

#[gpui::test]
async fn test_agent_mail_fyi_for_unknown_name_never_spawns(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let agent = &setup.agent;
    let (_home, sender) = enable_agent_mail(agent, cx);
    enable_mail_spawn(cx);

    let to = ::agent_mail::Address::parse("task-quiet@m3").unwrap();
    let mut envelope = sender
        .compose(&to, "zed-bb@m3", "heads up only".to_string(), None)
        .unwrap();
    envelope.mode = ::agent_mail::MailMode::Fyi;
    sender.send(&envelope).unwrap();
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    agent.read_with(cx, |agent, _| {
        assert_eq!(agent.sessions.len(), 1);
        assert!(agent.agent_mail.as_ref().unwrap().spawned_names.is_empty());
    });
    assert_eq!(sender.peek(&["task-quiet".to_string()]).unwrap().len(), 1);
}

#[gpui::test]
async fn test_agent_mail_spawn_is_off_by_default(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let agent = &setup.agent;
    let (_home, sender) = enable_agent_mail(agent, cx);

    mail_to_name(&sender, "task-off", "should stay parked");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    agent.read_with(cx, |agent, _| {
        assert_eq!(agent.sessions.len(), 1);
    });
    assert_eq!(sender.peek(&["task-off".to_string()]).unwrap().len(), 1);
}

fn make_saved_thread(title: &str) -> crate::db::DbThread {
    crate::db::DbThread {
        title: title.to_string().into(),
        messages: Vec::new(),
        updated_at: chrono::Utc::now(),
        detailed_summary: None,
        initial_project_snapshot: None,
        cumulative_token_usage: Default::default(),
        request_token_usage: HashMap::default(),
        model: None,
        profile: None,
        subagent_context: None,
        speed: None,
        thinking_enabled: false,
        thinking_effort: None,
        draft_prompt: None,
        ui_scroll_position: None,
        sandboxed_terminal_temp_dir: None,
        sandbox_grants: crate::db::DbSandboxGrants::default(),
    }
}

#[gpui::test]
async fn test_agent_mail_resumes_saved_thread_for_zed_name(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let (agent, fake) = (&setup.agent, &setup.fake);
    let (_home, sender) = enable_agent_mail(agent, cx);
    enable_mail_spawn(cx);

    // A thread that is not live: only a row in the thread database.
    let departed_id = acp_v2::SessionId::new("11111111-2222-3333-4444-555555555555");
    let mail_name = agent_mail_bridge::native_thread_mail_name(&departed_id);
    let database = cx
        .update(|cx| crate::db::ThreadsDatabase::connect(cx))
        .await
        .unwrap();
    database
        .save_thread(
            departed_id.clone(),
            make_saved_thread("Departed"),
            Default::default(),
        )
        .await
        .unwrap();

    mail_to_name(&sender, &mail_name, "continue where you left off");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    let (resumed_title, resumed_model) = agent.read_with(cx, |agent, cx| {
        assert!(
            agent.sessions.contains_key(&departed_id),
            "mail to a saved thread's inbox should resume it"
        );
        assert!(
            agent
                .agent_mail
                .as_ref()
                .unwrap()
                .pinned_threads
                .contains_key(&departed_id),
            "the resumed thread must be pinned or it releases itself"
        );
        assert_eq!(
            agent.agent_mail_address(&departed_id).unwrap(),
            format!("{mail_name}@m3")
        );
        assert_eq!(agent.sessions.len(), 2);
        let thread = agent.sessions[&departed_id].thread.read(cx);
        (
            thread.title().unwrap().to_string(),
            thread.model().unwrap().clone(),
        )
    });
    assert_eq!(resumed_title, "Departed");

    let pending = fake.pending_completions_for(&resumed_model);
    assert_eq!(
        pending.len(),
        1,
        "the mail should start the resumed thread's turn"
    );
    let prompt = pending[0].messages.last().unwrap().string_contents();
    assert!(prompt.contains("continue where you left off"), "{prompt}");
}

#[gpui::test]
async fn test_agent_mail_never_spawns_for_zed_names(cx: &mut TestAppContext) {
    let setup = setup_threads(1, cx).await;
    let agent = &setup.agent;
    let (_home, sender) = enable_agent_mail(agent, cx);
    enable_mail_spawn(cx);

    mail_to_name(&sender, "zed-99999999", "for a session that is gone");
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();

    agent.read_with(cx, |agent, _| {
        assert_eq!(agent.sessions.len(), 1);
    });
    assert_eq!(sender.peek(&["zed-99999999".to_string()]).unwrap().len(), 1);
}
