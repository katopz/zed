use acp_thread::{AcpThread, AcpThreadEvent, ThreadStatus};
use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, anyhow};
use gpui::{App, Context, Entity, SharedString, Task, TaskExt as _};
use itertools::Itertools as _;
use serde::{Deserialize, Serialize};

use crate::{AgentMessageContent, NativeAgent, NativeAgentConnection, Thread};

const UNTITLED_THREAD: &str = "Untitled thread";

/// What a thread is, relative to the thread asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadListingKind {
    /// The thread that made the request.
    #[serde(rename = "self")]
    Current,
    /// A thread spawned by another thread via `spawn_agent`.
    Subagent,
    /// A top-level thread, e.g. created by the user or by `create_thread`.
    Thread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadListingStatus {
    Idle,
    Generating,
}

/// A live native-agent thread that can be addressed by `send_message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadListing {
    pub session_id: acp::SessionId,
    pub title: SharedString,
    pub kind: ThreadListingKind,
    pub status: ThreadListingStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<acp::SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Messages waiting for the thread's current turn to end.
    pub queued_messages: usize,
    /// Where agents on other machines can reach this thread (`agent-mail`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mail_address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ThreadMessageRequest {
    /// Target session ID, or the exact (case-insensitive) title of a thread.
    pub to: String,
    pub message: String,
    pub wait_for_reply: bool,
    /// Schedule agent-mail for later (`30m`, `8h`, `23:45`, RFC 3339).
    pub deliver_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum ThreadMessageDelivery {
    /// The target was idle, so the message started a turn immediately.
    Delivered {
        #[serde(skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
    },
    /// The target is mid-turn; the message is delivered when that turn ends.
    Queued { position: usize },
    /// Sent through agent-mail to an agent on another machine (or a Claude
    /// Code session); its reply, if any, arrives later as a new message.
    Mailed {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        via: Option<String>,
        /// When scheduled mail lands (and wakes the recipient).
        #[serde(skip_serializing_if = "Option::is_none")]
        deliver_at: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMessageOutcome {
    pub session_id: acp::SessionId,
    pub title: SharedString,
    #[serde(flatten)]
    pub delivery: ThreadMessageDelivery,
}

#[derive(Debug, Clone)]
pub(crate) struct MailboxMessage {
    pub from_session_id: acp::SessionId,
    pub from_title: SharedString,
    pub body: String,
}

/// Wraps pending messages into a single user prompt so a burst of messages
/// costs the recipient one turn instead of one turn per message.
pub(crate) fn format_mailbox_prompt(messages: &[MailboxMessage]) -> String {
    let mut prompt = String::new();
    for message in messages {
        prompt.push_str(&format!(
            "<agent_message from=\"{}\" session_id=\"{}\">\n{}\n</agent_message>\n",
            message.from_title, message.from_session_id, message.body
        ));
    }
    prompt.push_str(
        "These messages were sent by other agent threads, not the user. \
         Reply with `send_message` (to: the sender's session_id; a `name@rig` id \
         is an agent on another machine) if a response is needed.",
    );
    prompt
}

/// Resolves `to` against `(session_id, title)` candidates: an exact session ID
/// wins, otherwise a unique case-insensitive title match.
pub(crate) fn resolve_message_target<'a>(
    to: &str,
    candidates: impl IntoIterator<Item = (&'a acp::SessionId, Option<SharedString>)>,
) -> Result<acp::SessionId> {
    let to = to.trim();
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    if let Some((session_id, _)) = candidates
        .iter()
        .find(|(session_id, _)| session_id.0.as_ref() == to)
    {
        return Ok((*session_id).clone());
    }

    let mut title_matches = candidates.iter().filter(|(_, title)| {
        title
            .as_ref()
            .is_some_and(|title| title.trim().eq_ignore_ascii_case(to))
    });
    match (title_matches.next(), title_matches.next()) {
        (Some((session_id, _)), None) => Ok((*session_id).clone()),
        (Some(_), Some(_)) => Err(anyhow!(
            "More than one thread is titled {to:?}; pass its session_id instead (see `list_threads`)"
        )),
        (None, _) => Err(anyhow!(
            "No live thread matches {to:?}; call `list_threads` to see addressable threads"
        )),
    }
}

/// The final text of the thread's last agent message, if it has one.
pub(crate) fn last_agent_message_text(thread: &Thread) -> Option<String> {
    let content = thread
        .last_message()?
        .as_agent_message()?
        .content
        .iter()
        .filter_map(|content| match content {
            AgentMessageContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .join("\n\n");
    (!content.is_empty()).then_some(content)
}

/// Mailbox messages taken for an idle thread, ready to start its next turn.
///
/// Starting the turn goes through [`NativeAgentConnection`], which reads the
/// `NativeAgent` entity, so it must happen after the `NativeAgent` update that
/// produced this delivery has returned.
pub(crate) struct MailboxDelivery {
    acp_thread: Entity<AcpThread>,
    prompt: String,
}

impl MailboxDelivery {
    pub(crate) fn start(self, cx: &mut App) -> Task<Result<acp::StopReason>> {
        let connection = self
            .acp_thread
            .read(cx)
            .connection()
            .clone()
            .into_any()
            .downcast::<NativeAgentConnection>();
        match connection {
            Ok(connection) => connection.send_and_wait_for_completion(
                self.acp_thread,
                vec![self.prompt.into()],
                cx,
            ),
            Err(_) => Task::ready(Err(anyhow!("The target thread is not a native thread"))),
        }
    }
}

pub(crate) enum PostedMessage {
    Delivered {
        delivery: MailboxDelivery,
        thread: Entity<Thread>,
    },
    Queued {
        position: usize,
    },
}

impl NativeAgent {
    pub(crate) fn list_threads(
        &self,
        requester: &acp::SessionId,
        cx: &App,
    ) -> Result<Vec<ThreadListing>> {
        let project_id = self
            .sessions
            .get(requester)
            .context("The requesting thread is not a live session")?
            .project_id;

        let mut listings = self
            .sessions
            .iter()
            .filter(|(_, session)| session.project_id == project_id)
            .map(|(session_id, session)| {
                let thread = session.thread.read(cx);
                let kind = match (session_id == requester, thread.is_subagent()) {
                    (true, _) => ThreadListingKind::Current,
                    (false, true) => ThreadListingKind::Subagent,
                    (false, false) => ThreadListingKind::Thread,
                };
                let status = match session
                    .acp_thread
                    .upgrade()
                    .map(|acp_thread| acp_thread.read(cx).status())
                {
                    None | Some(ThreadStatus::Idle) => ThreadListingStatus::Idle,
                    Some(ThreadStatus::Generating | ThreadStatus::Unconfirmed) => {
                        ThreadListingStatus::Generating
                    }
                };
                ThreadListing {
                    session_id: session_id.clone(),
                    title: thread.title().unwrap_or_else(|| UNTITLED_THREAD.into()),
                    kind,
                    status,
                    parent_session_id: thread.parent_thread_id(),
                    model: thread
                        .model()
                        .map(|model| format!("{}/{}", model.provider_id().0, model.id().0)),
                    queued_messages: self.mailboxes.get(session_id).map_or(0, Vec::len),
                    mail_address: self.agent_mail_address(session_id),
                }
            })
            .collect::<Vec<_>>();
        listings.sort_by(|left, right| {
            (left.kind != ThreadListingKind::Current, &left.title)
                .cmp(&(right.kind != ThreadListingKind::Current, &right.title))
        });
        Ok(listings)
    }

    /// Queues `request` for its target and delivers it right away when the
    /// target is idle. Returns the resolved target and how it was posted.
    pub(crate) fn post_thread_message(
        &mut self,
        sender: &acp::SessionId,
        request: ThreadMessageRequest,
        cx: &mut Context<Self>,
    ) -> Result<(acp::SessionId, SharedString, PostedMessage)> {
        let sender_session = self
            .sessions
            .get(sender)
            .context("The sending thread is not a live session")?;
        let project_id = sender_session.project_id;
        let from_title = sender_session
            .thread
            .read(cx)
            .title()
            .unwrap_or_else(|| UNTITLED_THREAD.into());

        let target = resolve_message_target(
            &request.to,
            self.sessions
                .iter()
                .filter(|(_, session)| session.project_id == project_id)
                .map(|(session_id, session)| (session_id, session.thread.read(cx).title())),
        )?;
        if &target == sender {
            anyhow::bail!("A thread cannot send a message to itself");
        }
        if request.message.trim().is_empty() {
            anyhow::bail!("The message is empty");
        }

        let target_session = self
            .sessions
            .get(&target)
            .context("The target thread is no longer live")?;
        let target_thread = target_session.thread.clone();
        let title = target_thread
            .read(cx)
            .title()
            .unwrap_or_else(|| UNTITLED_THREAD.into());

        self.mailboxes
            .entry(target.clone())
            .or_default()
            .push(MailboxMessage {
                from_session_id: sender.clone(),
                from_title,
                body: request.message,
            });

        let posted = match self.deliver_mailbox(&target, cx) {
            Some(delivery) => PostedMessage::Delivered {
                delivery,
                thread: target_thread,
            },
            None => PostedMessage::Queued {
                position: self.mailboxes.get(&target).map_or(0, Vec::len),
            },
        };
        Ok((target, title, posted))
    }

    /// Takes every pending message for `session_id` as one prompt, but only
    /// while the thread is idle: a new native submission interrupts a running
    /// turn.
    pub(crate) fn deliver_mailbox(
        &mut self,
        session_id: &acp::SessionId,
        cx: &mut Context<Self>,
    ) -> Option<MailboxDelivery> {
        let acp_thread = self.sessions.get(session_id)?.acp_thread.upgrade()?;
        if acp_thread.read(cx).status() != ThreadStatus::Idle {
            return None;
        }
        let messages = self.mailboxes.remove(session_id)?;
        if messages.is_empty() {
            return None;
        }
        Some(MailboxDelivery {
            acp_thread,
            prompt: format_mailbox_prompt(&messages),
        })
    }

    pub(crate) fn handle_acp_thread_event_for_mailbox(
        &mut self,
        acp_thread: Entity<AcpThread>,
        event: &AcpThreadEvent,
        cx: &mut Context<Self>,
    ) {
        // A user cancel means "stop"; keep the mail until the next turn ends
        // rather than immediately restarting the thread behind their back.
        let AcpThreadEvent::Stopped { stop_reason, .. } = event else {
            return;
        };
        if matches!(stop_reason, Some(acp::StopReason::Cancelled)) {
            return;
        }
        let session_id = acp_thread.read(cx).session_id().clone();
        if !self.mailboxes.contains_key(&session_id) {
            return;
        }
        // Deferred so a UI-queued user message dispatched from the same
        // `Stopped` event goes first; the idle check then leaves our mail for
        // the end of that turn instead of cancelling it.
        cx.spawn(async move |this, cx| {
            let delivery = this.update(cx, |this, cx| this.deliver_mailbox(&session_id, cx))?;
            if let Some(delivery) = delivery {
                cx.update(|cx| delivery.start(cx)).await?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub(crate) fn drop_mailbox(&mut self, session_id: &acp::SessionId) {
        if let Some(messages) = self.mailboxes.remove(session_id) {
            log::warn!(
                "Dropping {} undelivered agent message(s) for closed thread {session_id}",
                messages.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> acp::SessionId {
        acp::SessionId::new(id)
    }

    #[test]
    fn resolves_by_session_id_then_unique_title() {
        let first = session("a1");
        let second = session("b2");
        let third = session("c3");
        let candidates = || {
            [
                (&first, Some(SharedString::from("Fix login"))),
                (&second, Some(SharedString::from("Research"))),
                (&third, Some(SharedString::from("research"))),
            ]
        };

        assert_eq!(resolve_message_target("b2", candidates()).unwrap(), second);
        assert_eq!(
            resolve_message_target(" fix LOGIN ", candidates()).unwrap(),
            first
        );
        assert!(resolve_message_target("research", candidates()).is_err());
        assert!(resolve_message_target("missing", candidates()).is_err());
    }

    #[test]
    fn mailbox_prompt_names_every_sender() {
        let prompt = format_mailbox_prompt(&[
            MailboxMessage {
                from_session_id: session("s1"),
                from_title: "Planner".into(),
                body: "Start on step 2".into(),
            },
            MailboxMessage {
                from_session_id: session("s2"),
                from_title: "Reviewer".into(),
                body: "Tests fail".into(),
            },
        ]);

        assert!(prompt.contains("from=\"Planner\" session_id=\"s1\">\nStart on step 2\n"));
        assert!(prompt.contains("from=\"Reviewer\" session_id=\"s2\">\nTests fail\n"));
        assert!(prompt.contains("send_message"));
    }
}
