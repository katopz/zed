use agent_client_protocol as acp;
use gpui::Window;

/// Action dispatched when the external LLM returns a next_prompt.
///
/// Registered in `agent_panel.rs` — creates a new thread with summary link + prompt, auto-submits.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
    gpui::Action,
)]
#[action(namespace = agent)]
#[serde(deny_unknown_fields)]
pub struct AutoPromptNewThread {
    /// Session ID of the previous thread (for summary link).
    pub from_session_id: acp::SessionId,
    /// Title of the previous thread.
    pub from_title: Option<String>,
    /// The follow-up prompt text from the external LLM.
    pub next_prompt: String,
}

fn dispatch_action(
    action: auto_prompt::AutoPromptAction,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    window.dispatch_action(
        Box::new(AutoPromptNewThread {
            from_session_id: action.from_session_id,
            from_title: action.from_title,
            next_prompt: action.next_prompt,
        }),
        cx,
    );
}

/// Entry point — called from `ConversationView::handle_thread_event`
/// when `AcpThreadEvent::Stopped` fires.
///
/// Delegates decision logic to the `auto_prompt` crate and handles
/// GPUI action dispatch for the results.
pub fn on_thread_stopped(
    thread: &gpui::Entity<acp_thread::AcpThread>,
    used_tools: bool,
    stop_reason: &acp::StopReason,
    window: &mut Window,
    cx: &mut gpui::Context<crate::ConversationView>,
) {
    let decision = auto_prompt::decide(thread, used_tools, stop_reason, cx);

    match decision {
        auto_prompt::AutoPromptDecision::NoAction => {}

        auto_prompt::AutoPromptDecision::DispatchNow(action) => {
            dispatch_action(action, window, cx);
        }

        auto_prompt::AutoPromptDecision::DispatchAfterDelay { action, delay_ms } => {
            let _ = cx.spawn_in(window, async move |_view, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(delay_ms))
                    .await;

                _view
                    .update_in(cx, |_view, window, cx| {
                        dispatch_action(action, window, cx);
                    })
                    .ok();
            });
        }

        auto_prompt::AutoPromptDecision::NeedsLlmCall(data) => {
            let _ = cx.spawn_in(window, async move |_view, cx| {
                let action = auto_prompt::decide_with_llm(data, cx).await;

                if let Some(action) = action {
                    _view
                        .update_in(cx, |_view, window, cx| {
                            dispatch_action(action, window, cx);
                        })
                        .ok();
                }
            });
        }
    }
}
