//! Fork-owned cross-machine mail for native threads (plan 043): each live
//! session gets an `agent-mail` inbox, which a poller drains into the
//! `send_message` mailbox, and `send_message` to `name@rig` goes out through
//! agent-mail. Child module of the crate root, like `agent_fork`.

use super::*;
use ::agent_mail::{ANY_NAME, Address, Envelope, MailMode, Mailroom};
use acp_thread::ThreadStatus;
use agent_settings::AgentSettings;
use futures::channel::oneshot;
use futures::future::Shared;
use settings::{Settings as _, SettingsStore};

/// Same cadence as the Claude Code waiter hook: a directory listing per live
/// session, cheap enough to run continuously.
const MAIL_POLL_INTERVAL: Duration = Duration::from_millis(500);
const OUTBOX_FLUSH_TICKS: u32 = 120;
/// Refresh liveness entries every ~5 s of poll ticks (issue 046).
const LIVE_REFRESH_TICKS: u32 = 10;
/// Cap on live threads spawned from mail (plan 053): a runaway sender must
/// not fill the sidebar with threads.
const MAX_MAIL_SPAWNED_THREADS: usize = 8;

pub(crate) struct AgentMailState {
    mailroom: Arc<Mailroom>,
    self_rig: String,
    applied_watch_interval: Option<u64>,
    /// When each live session last (re)entered this Zed, in UTC. Broadcasts
    /// older than a session's re-entry are skipped for it: a stopped thread
    /// the user clicks gets its old inbox name back, and without this gate
    /// `take_broadcasts`' ten-minute lookback would hand it every recent
    /// blast it was never part of, starting a turn behind the user's click.
    pub(crate) live_since: HashMap<acp_v2::SessionId, chrono::DateTime<chrono::Utc>>,
    /// Inbox-name overrides for mail-spawned threads (plan 053): the session
    /// → the requested inbox name it answers as. Pruned alongside
    /// `live_since`; closing the thread releases the name, so a later mail
    /// respawns a fresh `[M]` thread for it.
    pub(crate) spawned_names: HashMap<acp_v2::SessionId, String>,
    /// Strong handles keeping mail-spawned threads alive (plan 053): no panel
    /// view owns them, so without this the `AcpThread` release would clean the
    /// session up the moment the spawn returns.
    spawned_threads: HashMap<acp_v2::SessionId, Entity<acp_thread::AcpThread>>,
    _poller: Task<()>,
    _settings: Subscription,
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
                let mut ticks: u32 = 0;
                loop {
                    cx.background_executor().timer(MAIL_POLL_INTERVAL).await;
                    ticks = ticks.wrapping_add(1);
                    // Retry mail parked for unreachable rigs about once a minute.
                    if ticks.is_multiple_of(OUTBOX_FLUSH_TICKS) {
                        let mailroom = mailroom.clone();
                        cx.background_spawn(async move { mailroom.flush_outbox() })
                            .detach();
                    }
                    // Sync the per-session liveness cutoffs with the session
                    // set: sessions that closed are pruned, sessions that
                    // appeared (a new thread, or a stopped thread the user
                    // just clicked back open) re-enter at `now`, so they
                    // claim only broadcasts sent from then on.
                    let Ok(Some((claims, live_entries, inbox_names, spawn_budget))) =
                        this.update(cx, |agent, cx| {
                            let Some(state) = agent.agent_mail.as_mut() else {
                                return None;
                            };
                            let now = chrono::Utc::now();
                            state
                                .live_since
                                .retain(|session_id, _| agent.sessions.contains_key(session_id));
                            state
                                .spawned_names
                                .retain(|session_id, _| agent.sessions.contains_key(session_id));
                            state
                                .spawned_threads
                                .retain(|session_id, _| agent.sessions.contains_key(session_id));
                            let mut claims = Vec::new();
                            let mut live_entries = Vec::new();
                            let mut inbox_names = Vec::new();
                            for (session_id, session) in &agent.sessions {
                                let inbox = state
                                    .spawned_names
                                    .get(session_id)
                                    .cloned()
                                    .unwrap_or_else(|| native_thread_mail_name(session_id));
                                let cutoff =
                                    *state.live_since.entry(session_id.clone()).or_insert(now);
                                let (status, kind) = live_status(session, cx);
                                claims.push((session_id.clone(), inbox.clone(), cutoff));
                                live_entries.push((inbox.clone(), status, kind));
                                inbox_names.push(inbox);
                            }
                            let spawn_budget =
                                if AgentSettings::get_global(cx).fork.agent_mail_spawn_threads
                                    && !agent.sessions.is_empty()
                                    && state.spawned_names.len() < MAX_MAIL_SPAWNED_THREADS
                                {
                                    Some(MAX_MAIL_SPAWNED_THREADS - state.spawned_names.len())
                                } else {
                                    None
                                };
                            Some((claims, live_entries, inbox_names, spawn_budget))
                        })
                    else {
                        break;
                    };
                    // The liveness registry backs deliver-time `*@rig`
                    // resolution (issue 046): refresh every ~5 s, prune what
                    // no poller (here or elsewhere) keeps fresh.
                    let (live_mailroom, entries) = (mailroom.clone(), live_entries);
                    cx.background_spawn(async move {
                        if ticks.is_multiple_of(LIVE_REFRESH_TICKS) {
                            live_mailroom.prune_stale_live_entries();
                        }
                        for (name, status, kind) in entries {
                            live_mailroom.write_live_entry(&name, status, kind);
                        }
                    })
                    .detach();
                    // Plan 053: mail waiting in an inbox no live session
                    // claims is addressed to a thread that does not exist
                    // yet. Discover candidates and claim them only when a
                    // spawn can actually start: enabled, under the cap, a
                    // donor project exists (a live session), and the batch is
                    // not fyi-only, which must never wake anyone. Discovery
                    // precedes the claim, so a spawn that fails on the next
                    // tick's retry never drops mail already claimed.
                    let spawn_targets = if let Some(budget) = spawn_budget {
                        let mailroom = mailroom.clone();
                        cx.background_spawn(async move {
                            discover_spawn_targets(&mailroom, &inbox_names, budget)
                        })
                        .await
                    } else {
                        Vec::new()
                    };
                    let claimed = if claims.is_empty() {
                        Vec::new()
                    } else {
                        let mailroom = mailroom.clone();
                        cx.background_spawn(async move { claim_inboxes(&mailroom, claims) })
                            .await
                    };
                    let claimed_spawns = if spawn_targets.is_empty() {
                        Vec::new()
                    } else {
                        let mailroom = mailroom.clone();
                        cx.background_spawn(async move {
                            spawn_targets
                                .into_iter()
                                .filter_map(|name| {
                                    mailroom
                                        .claim(std::slice::from_ref(&name))
                                        .ok()
                                        .filter(|mail| !mail.is_empty())
                                        .map(|mail| (name, mail))
                                })
                                .collect::<Vec<_>>()
                        })
                        .await
                    };
                    if claimed.is_empty() && claimed_spawns.is_empty() {
                        continue;
                    }
                    let Ok(deliveries) = this.update(cx, |agent, cx| {
                        let mut deliveries = Vec::new();
                        for (name, mail) in claimed_spawns {
                            let message_count = mail.len();
                            match agent.spawn_mail_thread(&name, cx) {
                                Some(session_id) => {
                                    if let Some(delivery) =
                                        agent.post_agent_mail(&session_id, mail, cx)
                                    {
                                        deliveries.push(delivery);
                                    }
                                }
                                None => log::error!(
                                    "agent-mail: could not spawn a thread for inbox {name}; \
                                     dropping {message_count} message(s)"
                                ),
                            }
                        }
                        deliveries.extend(claimed.into_iter().filter_map(|(session_id, mail)| {
                            agent.post_agent_mail(&session_id, mail, cx)
                        }));
                        deliveries
                    }) else {
                        break;
                    };
                    for delivery in deliveries {
                        cx.update(|cx| delivery.start(cx).detach_and_log_err(cx));
                    }
                }
            }
        });
        let settings = cx.observe_global::<SettingsStore>(|agent, cx| {
            if let Some(state) = agent.agent_mail.as_mut() {
                state.apply_watch_interval(cx);
            }
        });
        let mut state = Self {
            mailroom,
            self_rig,
            applied_watch_interval: None,
            live_since: HashMap::default(),
            spawned_names: HashMap::default(),
            spawned_threads: HashMap::default(),
            _poller: poller,
            _settings: settings,
        };
        state.apply_watch_interval(cx);
        state
    }

    /// Pushes `agent_mail_watch_interval_minutes` to the orchestrator rig
    /// when it is set and differs from what this Zed last applied.
    fn apply_watch_interval(&mut self, cx: &mut Context<NativeAgent>) {
        let Some(minutes) = AgentSettings::get_global(cx)
            .fork
            .agent_mail_watch_interval_minutes
        else {
            return;
        };
        if self.applied_watch_interval == Some(minutes) {
            return;
        }
        self.applied_watch_interval = Some(minutes);
        let mailroom = self.mailroom.clone();
        cx.background_spawn(async move { mailroom.set_orchestrator_interval(minutes) })
            .detach_and_log_err(cx);
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

/// The liveness-registry view of a session (issue 046): `"idle"` vs
/// `"generating"`, plus `"thread"` vs `"subagent"` so `*@rig` resolution can
/// prefer free top-level threads over free subagents.
fn live_status(session: &Session, cx: &App) -> (&'static str, &'static str) {
    let status = match session
        .acp_thread
        .upgrade()
        .map(|acp_thread| acp_thread.read(cx).status())
    {
        None | Some(ThreadStatus::Idle) => "idle",
        Some(_) => "generating",
    };
    let kind = if session.thread.read(cx).is_subagent() {
        "subagent"
    } else {
        "thread"
    };
    (status, kind)
}

/// `name@rig` targets go through agent-mail. Thread titles that merely contain
/// an `@` (spaces, punctuation) stay local because they aren't valid addresses.
pub(crate) fn is_mail_address(to: &str) -> bool {
    to.contains('@') && Address::parse(to.trim()).is_ok()
}

fn claim_inboxes(
    mailroom: &Mailroom,
    inboxes: Vec<(acp_v2::SessionId, String, chrono::DateTime<chrono::Utc>)>,
) -> Vec<(acp_v2::SessionId, Vec<Envelope>)> {
    inboxes
        .into_iter()
        .filter_map(|(session_id, name, cutoff)| {
            let reader = name.clone();
            let names = [name];
            if !mailroom.has_mail(&names) && !mailroom.has_broadcast(&reader) {
                return None;
            }
            match mailroom.claim(&names) {
                Ok(mut mail) => {
                    mail.extend(mailroom.take_broadcasts_since(&reader, cutoff));
                    (!mail.is_empty()).then_some((session_id, mail))
                }
                Err(error) => {
                    log::error!("agent-mail: claiming mail for {session_id} failed: {error:#}");
                    None
                }
            }
        })
        .collect()
}

/// Plan 053: inbox names holding mail that no live session claims — each one
/// is a thread that does not exist yet, so spawning is what the mail asks
/// for. Machine addresses never spawn: `zed-*` names a specific session that
/// is gone (its mail stays parked), and `_any` is the shared `*@rig` fallback
/// claimed by whichever agent gets there first. An fyi-only batch never wakes
/// anyone, so it never spawns either.
fn discover_spawn_targets(
    mailroom: &Mailroom,
    live_names: &[String],
    budget: usize,
) -> Vec<String> {
    mailroom
        .pending_names()
        .into_iter()
        .filter(|name| !name.starts_with("zed-") && name != ANY_NAME)
        .filter(|name| !live_names.contains(name))
        .filter(|name| {
            mailroom
                .peek(std::slice::from_ref(name))
                .is_ok_and(|mail| mail.iter().any(|envelope| envelope.mode != MailMode::Fyi))
        })
        .take(budget)
        .collect()
}

impl NativeAgent {
    pub(crate) fn agent_mail_address(&self, session_id: &acp_v2::SessionId) -> Option<String> {
        let state = self.agent_mail.as_ref()?;
        Some(format!(
            "{}@{}",
            self.mail_name_for_session(session_id),
            state.self_rig
        ))
    }

    /// The inbox name a session answers as (plan 053): mail-spawned threads
    /// keep the name their first mail asked for; every other session derives
    /// `zed-<id>` from its session id.
    pub(crate) fn mail_name_for_session(&self, session_id: &acp_v2::SessionId) -> String {
        if let Some(state) = self.agent_mail.as_ref()
            && let Some(name) = state.spawned_names.get(session_id)
        {
            return name.clone();
        }
        native_thread_mail_name(session_id)
    }

    /// Creates the thread an unknown inbox name asks for (plan 053): a fresh
    /// top-level thread titled `[M] <name>` on a live session's project,
    /// answering as `<name>@<this rig>` from then on. `None` when no live
    /// session exists to borrow a project from — normally filtered out by the
    /// budget gate, but possible if the last session closed between the
    /// poller's snapshot and this call, in which case the already-claimed
    /// mail is dropped with an error log.
    fn spawn_mail_thread(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Option<acp_v2::SessionId> {
        let donor = self.sessions.values().next()?;
        let project = self.projects.get(&donor.project_id)?.project.clone();
        let work_dirs = donor
            .acp_thread
            .upgrade()
            .and_then(|acp_thread| acp_thread.read(cx).work_dirs().cloned())
            .unwrap_or_default();
        let acp_thread = self.new_session(project, work_dirs, cx);
        let session_id = acp_thread.read(cx).session_id().clone();
        if let Some(session) = self.sessions.get(&session_id) {
            let title = SharedString::from(format!("[M] {name}"));
            session
                .thread
                .update(cx, |thread, cx| thread.set_title(title, cx));
        }
        if let Some(state) = self.agent_mail.as_mut() {
            state
                .spawned_names
                .insert(session_id.clone(), name.to_string());
            state
                .spawned_threads
                .insert(session_id.clone(), acp_thread);
        }
        log::info!("agent-mail: spawned thread [M] {name} as {session_id}");
        Some(session_id)
    }

    /// Queues claimed mail like a `send_message` from a local thread, so an
    /// idle thread starts a turn and a busy one gets it when its turn ends —
    /// unless the envelope's mode asks otherwise (issue 046): `steering` ends
    /// the running turn at the next message boundary, `urgent` preempts it,
    /// `fyi` never wakes anyone: it parks and is read at the next natural
    /// turn end, so a broadcast costs busy threads a queued slice and idle
    /// threads nothing.
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
        let fyi_only = mail.iter().all(|envelope| envelope.mode == MailMode::Fyi);
        // Mail claimed for this thread means its wait is over (the reply or
        // scheduled wake arrived), whether it is delivered now or queued for
        // the current turn's end — but an unrelated fyi blast must not end a
        // mail wait it has nothing to do with.
        if !fyi_only {
            if let Some(session) = self.sessions.get(session_id)
                && let Some(acp_thread) = session.acp_thread.upgrade()
            {
                acp_thread.update(cx, |thread, cx| thread.set_waiting_for_mail(false, cx));
            }
        }
        let urgent = mail
            .iter()
            .any(|envelope| envelope.mode == MailMode::Urgent);
        let steering = mail
            .iter()
            .any(|envelope| envelope.mode == MailMode::Steering);
        let messages = mail
            .into_iter()
            .map(|envelope| thread_mailbox::MailboxMessage {
                from_title: match envelope.mode {
                    MailMode::Fyi => format!(
                        "{} via agent-mail, to {} (fyi — no reply needed)",
                        envelope.from, envelope.to
                    ),
                    _ => format!("{} via agent-mail, to {}", envelope.from, envelope.to),
                }
                .into(),
                from_session_id: acp_v2::SessionId::new(envelope.from),
                body: envelope.body,
            });
        self.mailboxes
            .entry(session_id.clone())
            .or_default()
            .extend(messages);
        if fyi_only {
            // Delivered by the existing turn-end path: a busy thread reads it
            // at its next boundary (one extra turn per fyi batch — cheaper
            // than a wake, and the prompt's "no reply needed" hint stops the
            // reply round-trips); an idle thread pays nothing until its next
            // turn for any reason.
            return None;
        }
        if let Some(delivery) = self.deliver_mailbox(session_id, cx) {
            return Some(delivery);
        }
        let busy = self
            .sessions
            .get(session_id)
            .and_then(|session| session.acp_thread.upgrade())
            .is_some_and(|acp_thread| acp_thread.read(cx).status() != ThreadStatus::Idle);
        if !busy {
            return None;
        }
        let target = self
            .sessions
            .get(session_id)
            .map(|session| session.thread.clone())?;
        if steering {
            target.update(cx, |thread, _| thread.set_end_turn_at_next_boundary(true));
        } else if urgent {
            let _completion = preempt_and_deliver(target, session_id.clone(), cx.entity(), cx);
        }
        None
    }
}

/// Preempts a mid-turn thread for urgent mail (issue 046): cancels the
/// running turn, waits for the thread to go idle, then delivers the mailbox —
/// the shape of the owner typing over a turn. The completion receiver
/// resolves with the delivered turn's stop reason so a `wait_for_reply`
/// sender still gets its reply.
pub(crate) fn preempt_and_deliver(
    target: Entity<Thread>,
    session_id: acp_v2::SessionId,
    agent: Entity<NativeAgent>,
    cx: &mut Context<NativeAgent>,
) -> Shared<oneshot::Receiver<Result<acp_v2::StopReason, Arc<anyhow::Error>>>> {
    let (completion_tx, completion_rx) =
        oneshot::channel::<Result<acp_v2::StopReason, Arc<anyhow::Error>>>();
    let completion = completion_rx.shared();
    let cancel = target.update(cx, |thread, cx| thread.cancel(cx));
    cx.spawn(async move |_, cx| {
        cancel.await;
        // The cancelled turn's Stopped event lands asynchronously; poll for
        // idle briefly so the delivery starts a turn instead of queueing.
        for _ in 0..50 {
            let idle = agent.read_with(cx, |agent, cx| {
                agent
                    .sessions
                    .get(&session_id)
                    .and_then(|session| session.acp_thread.upgrade())
                    .is_none_or(|acp_thread| acp_thread.read(cx).status() == ThreadStatus::Idle)
            });
            if idle {
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
        }
        let Some(delivery) = agent.update(cx, |agent, cx| {
            agent.deliver_mailbox_preempting(&session_id, true, cx)
        }) else {
            let _ = completion_tx.send(Ok(acp_v2::StopReason::Cancelled));
            return;
        };
        let stop = cx.update(|cx| delivery.start(cx));
        let _ = completion_tx.send(stop.await.map_err(Arc::new));
    })
    .detach();
    completion
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
            agent_mail::ensure_broadcast_mode(&to, request.mode)?;
            let from = agent.read_with(cx, |agent, _| agent.mail_name_for_session(&sender))?;
            let deliver_at = request
                .deliver_at
                .as_deref()
                .map(|when| ::agent_mail::parse_when(when, chrono::Local::now()))
                .transpose()?;
            let mode = request.mode;
            let report = cx
                .background_spawn(async move {
                    let mut envelope = mailroom.compose(&to, &from, request.message, None)?;
                    envelope.deliver_at = deliver_at.map(|at| at.to_rfc3339());
                    envelope.mode = mode;
                    mailroom.send(&envelope)
                })
                .await?;
            // The sender now waits on the reply (or its own scheduled wake):
            // its turn usually ends here, and the sidebar marks it waiting
            // until the mail is claimed back into its inbox.
            agent
                .update(cx, |agent, cx| {
                    if let Some(session) = agent.sessions.get(&sender)
                        && let Some(acp_thread) = session.acp_thread.upgrade()
                    {
                        acp_thread.update(cx, |thread, cx| thread.set_waiting_for_mail(true, cx));
                    }
                })
                .log_err();
            let deliver_at = deliver_at.map(|at| at.to_rfc3339());
            let mode = match mode {
                MailMode::Queued => None,
                mode => Some(mode.as_str().to_string()),
            };
            Ok(ThreadMessageOutcome {
                session_id: acp_v2::SessionId::new(report.to.clone()),
                title: report.to.into(),
                delivery: ThreadMessageDelivery::Mailed {
                    id: report.id,
                    via: report.via,
                    deliver_at,
                    queued: report.queued,
                    mode,
                    resolved_to: report.resolved_to,
                },
            })
        })
    }
}
