//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

/// A provider that accepts the request but never delivers SSE events must
/// not wedge the thread in `Generating` forever: the idle timeout in
/// `run_turn_internal` turns the dead stream into a retryable error, the
/// retry re-opens the stream, and once the extended idle-timeout retry
/// budget is exhausted the turn ends with the error instead of hanging
/// (issue 003).
#[gpui::test]
async fn test_stream_idle_timeout_recovers_hung_stream(cx: &mut TestAppContext) {
    let (thread, _event_stream, fake) = setup_thread_for_test(cx).await;
    let model = fake.model("fake");

    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.set_model(model.clone(), cx);
        });
    });

    // The fake model's stream stays open but never yields events — the
    // exact failure mode from issue 003.
    let mut events = cx
        .update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.send(ClientUserMessageId::new(), vec!["hello"], cx)
            })
        })
        .unwrap();
    cx.run_until_parked();

    // Default `stream_idle_timeout_secs` is 120; idle timeouts retry up to
    // 4 times with a 30s fixed delay, so the full cascade (5 attempts) needs
    // ~720s of virtual time. Advance in chunks so each due timer's task
    // runs before the next clock jump.
    for _ in 0..14 {
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
    }

    assert_eq!(
        fake.pending_completions_for(&model).len(),
        5,
        "initial attempt plus four retries should have opened five streams"
    );

    let mut saw_idle_timeout_error = false;
    while let Some(event) = events.next().await {
        if let Err(error) = event {
            assert!(
                format!("{error:#}").contains("stream idle timeout"),
                "unexpected turn error: {error:#}"
            );
            saw_idle_timeout_error = true;
        }
    }
    assert!(
        saw_idle_timeout_error,
        "the turn must end with the idle-timeout error instead of wedging"
    );

    thread.read_with(cx, |thread, _| {
        assert!(
            thread.running_turn.is_none(),
            "the turn must have finished, not stayed Generating forever"
        );
    });
}

/// A provider hang that outlasts the first attempt must recover
/// automatically, without surfacing an error or waiting for the user to
/// click retry: the idle timeout turns the dead stream into a retryable
/// error, and when a later retry's stream delivers events the turn simply
/// continues to completion.
#[gpui::test]
async fn test_stream_idle_timeout_recovers_mid_retry_cascade(cx: &mut TestAppContext) {
    let (thread, _event_stream, fake) = setup_thread_for_test(cx).await;
    let model = fake.model("fake");

    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.set_model(model.clone(), cx);
        });
    });

    let mut events = cx
        .update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.send(ClientUserMessageId::new(), vec!["hello"], cx)
            })
        })
        .unwrap();
    cx.run_until_parked();

    // Two attempts hang; each burns the 120s idle timeout plus a 30s
    // retry delay. Advance in 60s chunks until the third stream opens.
    for _ in 0..10 {
        if fake.pending_completions_for(&model).len() >= 3 {
            break;
        }
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
    }
    assert_eq!(
        fake.pending_completions_for(&model).len(),
        3,
        "two hung attempts plus the retry should have opened three streams"
    );

    // The provider recovers on the third attempt. The abandoned attempts'
    // senders are still queued with identical requests, so respond via
    // the last-entry helper rather than equality-based lookup.
    fake.respond_to_last_pending_completion(
        &model,
        [LanguageModelCompletionEvent::Text(
            "Recovered after retry.".into(),
        )],
    );
    cx.run_until_parked();

    let mut saw_recovered_text = false;
    let mut ended_normally = false;
    let mut saw_error = false;
    while let Some(event) = events.next().await {
        match event {
            Ok(ThreadEvent::AgentText(text)) => {
                saw_recovered_text |= text.contains("Recovered after retry.");
            }
            Ok(ThreadEvent::Stop(_)) => ended_normally = true,
            Err(error) => {
                saw_error = true;
                log::error!("unexpected turn error: {error:#}");
            }
            _ => {}
        }
    }
    assert!(
        saw_recovered_text,
        "the recovered attempt's text must reach the event stream"
    );
    assert!(
        ended_normally,
        "the turn must end with a stop event instead of an error"
    );
    assert!(!saw_error, "no error may be surfaced when a retry recovers");

    thread.read_with(cx, |thread, _| {
        assert!(
            thread.running_turn.is_none(),
            "the turn must have finished cleanly"
        );
    });
}

/// A compaction stream that accepts the request but never delivers
/// events must not wedge the turn: the idle timeout in `stream_compaction`
/// turns it into a retryable error (initial + 4 retries against the
/// compaction model), then the turn ends with a visible error instead of
/// hanging forever (`.docs/009` follow-up to issue 003).
#[gpui::test]
async fn test_compaction_stream_idle_timeout_recovers_hung_stream(cx: &mut TestAppContext) {
    let (thread, _event_stream, fake) = setup_thread_for_test(cx).await;
    let thread_model = fake.model("fake");
    let compaction_model = fake.model("compaction");
    let old_user_message_id = ClientUserMessageId::new();

    cx.update(|cx| {
        thread.update(cx, |thread, cx| {
            thread.set_model(thread_model.clone(), cx);
            thread
                .messages
                .push(user_text_message(old_user_message_id.clone(), "old user"));
            thread.messages.push(agent_text_message("old assistant"));
            thread.request_token_usage.insert(
                old_user_message_id.clone(),
                TokenUsage {
                    input_tokens: u64::MAX,
                    ..Default::default()
                },
            );
        });
        set_auto_compact_settings(
            cx,
            agent_settings::AutoCompactSettings {
                enabled: true,
                threshold: agent_settings::AutoCompactThreshold::Percentage(0.5),
            },
        );
        set_registry_compaction_model(cx, Some(compaction_model.clone()));
    });

    // The compaction model's stream stays open but never yields events.
    let mut events = cx
        .update(|cx| {
            thread.update(cx, |thread, cx| {
                thread.send(ClientUserMessageId::new(), vec!["new prompt"], cx)
            })
        })
        .unwrap();
    cx.run_until_parked();

    // Five compaction attempts (initial + 4 retries) at 120s idle + 30s
    // delay each (~720s virtual time total); advance in chunks so each
    // due timer's task runs first.
    for _ in 0..14 {
        cx.executor().advance_clock(Duration::from_secs(60));
        cx.run_until_parked();
    }

    assert_eq!(
        fake.pending_completions_for(&compaction_model).len(),
        5,
        "initial compaction attempt plus four retries should have opened five streams"
    );
    assert_eq!(
        fake.pending_completions_for(&thread_model).len(),
        0,
        "the main model must never be hit while compaction cannot complete"
    );

    let mut saw_idle_timeout_error = false;
    while let Some(event) = events.next().await {
        if let Err(error) = event {
            assert!(
                format!("{error:#}").contains("stream idle timeout"),
                "unexpected turn error: {error:#}"
            );
            saw_idle_timeout_error = true;
        }
    }
    assert!(
        saw_idle_timeout_error,
        "the turn must end with the compaction idle-timeout error instead of wedging"
    );

    thread.read_with(cx, |thread, _| {
        assert!(
            thread.running_turn.is_none(),
            "the turn must have finished, not stayed Generating forever"
        );
    });
}
