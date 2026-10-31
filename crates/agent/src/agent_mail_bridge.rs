//! Fork-owned cross-machine mail for native threads (plan 043): each live
//! session gets an `agent-mail` inbox, which a poller drains into the
//! `send_message` mailbox, and `send_message` to `name@rig` goes out through
//! agent-mail. Child module of the crate root, like `agent_fork`.

use super::*;
use ::agent_mail::{Address, Envelope, Mailroom};

/// Same cadence as the Claude Code waiter hook: a directory listing per live
/// session, cheap enough to run continuously.
const MAIL_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) struct AgentMailState {
    mailroom: Arc<Mailroom>,
    self_rig: String,
    _poller: Task<()>,
}

impl AgentMailState {
    /// Mail is on only where agent-mail is set up (`~/.agent-mail/rigs.json`),
    /// so machines without it pay nothing. Tests opt in with [`Self::start`].
    pub(crate) fn start_if_installed(cx: &mut Context<NativeAgent>) -> Option<Self> {
        if cfg!(any(test, feature = "test-support")) {
            return None;
        }
        let mailroom = Mailroom::from_env().ok()?;
        let self_rig = mailroom.rigs().ok()?.self_rig;
        Some(Self::start(mailroom, self_rig, cx))
    }

    pub(crate) fn start(
        mailroom: Mailroom,
        self_rig: String,
        cx: &mut Context<NativeAgent>,
    ) -> Self {
        let mailroom = Arc::new(mailroom);
        let poller = cx.spawn({
            let mailroom = mailroom.clone();
            async move |this, cx| {
                loop {
                    cx.background_executor().timer(MAIL_POLL_INTERVAL).await;
                    let Ok(inboxes) = this.read_with(cx, |agent, _| {
                        agent
                            .sessions
                            .keys()
                            .map(|session_id| {
                                (session_id.clone(), native_thread_mail_name(session_id))
                            })
                            .collect::<Vec<_>>()
                    }) else {
                        break;
                    };
                    if inboxes.is_empty() {
                        continue;
                    }
                    let mailroom = mailroom.clone();
                    let claimed = cx
                        .background_spawn(async move { claim_inboxes(&mailroom, inboxes) })
                        .await;
                    if claimed.is_empty() {
                        continue;
                    }
                    let Ok(deliveries) = this.update(cx, |agent, cx| {
                        claimed
                            .into_iter()
                            .filter_map(|(session_id, mail)| {
                                agent.post_agent_mail(&session_id, mail, cx)
                            })
                            .collect::<Vec<_>>()
                    }) else {
                        break;
                    };
                    for delivery in deliveries {
                        cx.update(|cx| delivery.start(cx).detach_and_log_err(cx));
                    }
                }
            }
        });
        Self {
            mailroom,
            self_rig,
            _poller: poller,
        }
    }
}

/// A native thread's inbox name, stable for the session's lifetime.
pub fn native_thread_mail_name(session_id: &acp_v2::SessionId) -> String {
    let short = session_id
        .0
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>();
    format!("zed-{short}")
}

/// `name@rig` targets go through agent-mail. Thread titles that merely contain
/// an `@` (spaces, punctuation) stay local because they aren't valid addresses.
pub(crate) fn is_mail_address(to: &str) -> bool {
    to.contains('@') && Address::parse(to.trim()).is_ok()
}

fn claim_inboxes(
    mailroom: &Mailroom,
    inboxes: Vec<(acp_v2::SessionId, String)>,
) -> Vec<(acp_v2::SessionId, Vec<Envelope>)> {
    inboxes
        .into_iter()
        .filter_map(|(session_id, name)| {
            let names = [name];
            if !mailroom.has_mail(&names) {
                return None;
            }
            match mailroom.claim(&names) {
                Ok(mail) if !mail.is_empty() => Some((session_id, mail)),
                Ok(_) => None,
                Err(error) => {
                    log::error!("agent-mail: claiming mail for {session_id} failed: {error:#}");
                    None
                }
            }
        })
        .collect()
}

impl NativeAgent {
    pub(crate) fn agent_mail_address(&self, session_id: &acp_v2::SessionId) -> Option<String> {
        let state = self.agent_mail.as_ref()?;
        Some(format!(
            "{}@{}",
            native_thread_mail_name(session_id),
            state.self_rig
        ))
    }

    /// Queues claimed mail like a `send_message` from a local thread, so an
    /// idle thread starts a turn and a busy one gets it when its turn ends.
    fn post_agent_mail(
        &mut self,
        session_id: &acp_v2::SessionId,
        mail: Vec<Envelope>,
        cx: &mut Context<Self>,
    ) -> Option<thread_mailbox::MailboxDelivery> {
        if !self.sessions.contains_key(session_id) {
            log::warn!(
                "agent-mail: dropping {} message(s) for closed thread {session_id}",
                mail.len()
            );
            return None;
        }
        let messages = mail
            .into_iter()
            .map(|envelope| thread_mailbox::MailboxMessage {
                from_title: format!("{} via agent-mail, to {}", envelope.from, envelope.to).into(),
                from_session_id: acp_v2::SessionId::new(envelope.from),
                body: envelope.body,
            });
        self.mailboxes
            .entry(session_id.clone())
            .or_default()
            .extend(messages);
        self.deliver_mailbox(session_id, cx)
    }
}

impl NativeThreadEnvironment {
    pub(crate) fn send_agent_mail(
        &self,
        request: ThreadMessageRequest,
        cx: &mut AsyncApp,
    ) -> Task<Result<ThreadMessageOutcome>> {
        let agent = self.agent.clone();
        let sender_thread = self.thread.clone();
        cx.spawn(async move |cx| {
            let sender = sender_thread.read_with(cx, |thread, _| thread.id().clone())?;
            let mailroom = agent
                .read_with(cx, |agent, _| {
                    agent
                        .agent_mail
                        .as_ref()
                        .map(|state| state.mailroom.clone())
                })?
                .context(
                    "agent-mail is not set up on this machine (no ~/.agent-mail/rigs.json); \
                     `name@rig` targets need it",
                )?;
            let to = Address::parse(request.to.trim())?;
            let from = native_thread_mail_name(&sender);
            let deliver_at = request
                .deliver_at
                .as_deref()
                .map(|when| ::agent_mail::parse_when(when, chrono::Local::now()))
                .transpose()?;
            let report = cx
                .background_spawn(async move {
                    let mut envelope = mailroom.compose(&to, &from, request.message, None)?;
                    envelope.deliver_at = deliver_at.map(|at| at.to_rfc3339());
                    mailroom.send(&envelope)
                })
                .await?;
            let deliver_at = deliver_at.map(|at| at.to_rfc3339());
            Ok(ThreadMessageOutcome {
                session_id: acp_v2::SessionId::new(report.to.clone()),
                title: report.to.into(),
                delivery: ThreadMessageDelivery::Mailed {
                    id: report.id,
                    via: report.via,
                    deliver_at,
                },
            })
        })
    }
}
