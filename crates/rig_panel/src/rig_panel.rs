//! Fork-owned "Rigs" dock panel + status-bar badge (plan 049 T3/T4): what
//! every rig is doing, its managed jobs with a live log tail, and this rig's
//! mail, read from the same agent-mail data as `agent-mail top`.
//!
//! Registered only where agent-mail is set up (`~/.agent-mail/rigs.json`), so
//! no stray icon shows elsewhere (the retired war room's lesson). Polls only
//! while the panel is open; the badge refreshes at most once a minute. Mail
//! is only peeked, never claimed.

use agent_mail::jobs::{JobAction, JobLine};
use agent_mail::rig::{self, RigProbe};
use agent_mail::{Envelope, MailboxSummary, Mailroom, Rigs};
use gpui::{
    Action, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Task, Window, actions, div, px,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{
    ActiveTheme, Color, CommonAnimationExt, Icon, IconButton, IconName, IconSize, Label, LabelSize,
    Tooltip, prelude::*,
};
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::{StatusItemView, Workspace};

actions!(
    rig_panel,
    [
        /// Toggle the Rigs panel: rig state, managed jobs, and agent mail.
        Toggle,
        /// Focus the Rigs panel.
        ToggleFocus,
    ]
);

const RIG_PANEL_KEY: &str = "RigPanel";
/// Panel tick: the selected running job's log refreshes at this pace.
const TICK: Duration = Duration::from_secs(3);
/// Fleet refresh = this many ticks (15 s): slow on purpose, SSH to every rig.
const FLEET_TICKS: u32 = 5;
const BADGE_EVERY: Duration = Duration::from_secs(60);
const LOG_LINES: usize = 40;
const MAIL_RECENT: usize = 20;

/// The mailroom, when agent-mail is set up on this machine.
fn installed() -> Option<Arc<Mailroom>> {
    let mailroom = Mailroom::from_env().ok()?;
    mailroom.rigs().ok()?;
    Some(Arc::new(mailroom))
}

pub fn init(cx: &mut App) {
    let Some(mailroom) = installed() else {
        return;
    };
    cx.observe_new(
        move |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            let Some(window) = window else {
                return;
            };
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if !workspace.toggle_panel_focus::<RigPanel>(window, cx) {
                    workspace.close_panel::<RigPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                workspace.toggle_panel_focus::<RigPanel>(window, cx);
            });
            let panel = cx.new(|cx| RigPanel::new(mailroom.clone(), cx));
            workspace.add_panel(panel, window, cx);
            let badge = cx.new(|cx| RigBadge::new(mailroom.clone(), cx));
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_right_item(badge, window, cx)
            });
        },
    )
    .detach();
}

type Fleet = Vec<(String, Result<RigProbe, String>)>;

/// Fetches every rig plus this rig's mail, off the UI thread.
async fn fetch(
    mailroom: Arc<Mailroom>,
    cx: &gpui::AsyncApp,
) -> (Result<Fleet, String>, Option<Rigs>, MailboxSummary) {
    cx.background_spawn(async move {
        let fleet = rig::status(&mailroom, &[])
            .map(|results| {
                results
                    .into_iter()
                    .map(|(name, outcome, _)| (name, outcome.map_err(|error| format!("{error:#}"))))
                    .collect()
            })
            .map_err(|error| format!("{error:#}"));
        (
            fleet,
            mailroom.rigs().ok(),
            mailroom.mailbox_summary(MAIL_RECENT),
        )
    })
    .await
}

pub struct RigPanel {
    focus_handle: FocusHandle,
    mailroom: Arc<Mailroom>,
    rigs: Option<Rigs>,
    fleet: Fleet,
    fleet_at: Option<Instant>,
    error: Option<String>,
    mail: MailboxSummary,
    collapsed: HashSet<String>,
    selected_job: Option<(String, String)>,
    log: Option<String>,
    open_message: Option<String>,
    /// The inbox folder whose mail is expanded in the Mail section.
    open_inbox: Option<String>,
    confirm_cancel: bool,
    hovered_job: Option<(String, String)>,
    mail_open: bool,
    poll: Option<Task<()>>,
}

impl EventEmitter<PanelEvent> for RigPanel {}

impl RigPanel {
    fn new(mailroom: Arc<Mailroom>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            mailroom,
            rigs: None,
            fleet: Vec::new(),
            fleet_at: None,
            error: None,
            mail: MailboxSummary::default(),
            collapsed: HashSet::new(),
            selected_job: None,
            log: None,
            open_message: None,
            open_inbox: None,
            confirm_cancel: false,
            hovered_job: None,
            mail_open: true,
            poll: None,
        }
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.poll.is_some() {
            return;
        }
        let mailroom = self.mailroom.clone();
        self.poll = Some(cx.spawn(async move |this, cx| {
            let mut tick: u32 = 0;
            loop {
                if tick.is_multiple_of(FLEET_TICKS) {
                    let (fleet, rigs, mail) = fetch(mailroom.clone(), cx).await;
                    let applied = this.update(cx, |panel, cx| {
                        match fleet {
                            Ok(fleet) => {
                                panel.fleet = fleet;
                                panel.error = None;
                            }
                            Err(error) => panel.error = Some(error),
                        }
                        panel.rigs = rigs.or(panel.rigs.take());
                        panel.mail = mail;
                        panel.fleet_at = Some(Instant::now());
                        cx.notify();
                    });
                    if applied.is_err() {
                        break;
                    }
                }
                let tailing = this
                    .read_with(cx, |panel, _| panel.tailing_job())
                    .ok()
                    .flatten();
                if let Some((rig_name, id)) = tailing {
                    let room = mailroom.clone();
                    let log = cx
                        .background_spawn(async move {
                            room.job_on_rig(&rig_name, &id, JobAction::Log(LOG_LINES))
                        })
                        .await;
                    let applied = this.update(cx, |panel, cx| {
                        panel.log = Some(
                            log.unwrap_or_else(|error| format!("(log unavailable: {error:#})")),
                        );
                        cx.notify();
                    });
                    if applied.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(TICK).await;
                tick = tick.wrapping_add(1);
            }
        }));
    }

    /// The selected job, while it is still running (finished logs don't change).
    fn tailing_job(&self) -> Option<(String, String)> {
        let (rig_name, id) = self.selected_job.clone()?;
        let running = self
            .probe(&rig_name)?
            .running
            .iter()
            .any(|job| job.id == id);
        (running || self.log.is_none()).then_some((rig_name, id))
    }

    fn probe(&self, rig_name: &str) -> Option<&RigProbe> {
        self.fleet
            .iter()
            .find(|(name, _)| name == rig_name)
            .and_then(|(_, outcome)| outcome.as_ref().ok())
    }

    fn select_job(&mut self, rig_name: String, id: String, cx: &mut Context<Self>) {
        let same = self.selected_job.as_ref() == Some(&(rig_name.clone(), id.clone()));
        self.selected_job = (!same).then_some((rig_name, id));
        self.log = None;
        self.confirm_cancel = false;
        cx.notify();
    }

    fn cancel_selected(&mut self, cx: &mut Context<Self>) {
        let Some((rig_name, id)) = self.selected_job.clone() else {
            return;
        };
        self.confirm_cancel = false;
        let room = self.mailroom.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { room.job_on_rig(&rig_name, &id, JobAction::Cancel) })
                .await;
            this.update(cx, |panel, cx| {
                panel.log = Some(match outcome {
                    Ok(text) => text,
                    Err(error) => format!("cancel failed: {error:#}"),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_rig(
        &self,
        name: &str,
        outcome: &Result<RigProbe, String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rig = self.rigs.as_ref().and_then(|rigs| rigs.rigs.get(name));
        let role = rig.and_then(|rig| rig.role.clone()).unwrap_or_default();
        let no_jobs = rig.is_some_and(|rig| !rig.accepts_jobs);
        let (state, state_color) = match outcome {
            Err(_) => ("OFFLINE", Color::Error),
            Ok(probe) if probe.quiet => ("QUIET", Color::Success),
            Ok(_) => ("BUSY", Color::Warning),
        };
        let open = !self.collapsed.contains(name);
        let job_count = outcome
            .as_ref()
            .ok()
            .map(|probe| probe.running.len() + probe.queued.len())
            .unwrap_or(0);
        let toggle_name = name.to_string();

        // Folded-group indicators, like the thread sidebar's collapsed headers:
        // a spinner while busy, an accent dot for owner escalations, an error
        // icon when the probe failed.
        let mut indicators: Vec<AnyElement> = Vec::new();
        if !open {
            match outcome {
                Err(_) => indicators.push(
                    Icon::new(IconName::Close)
                        .size(IconSize::XSmall)
                        .color(Color::Error)
                        .into_any_element(),
                ),
                Ok(probe) => {
                    if !probe.quiet {
                        indicators.push(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted)
                                .with_rotate_animation(2)
                                .into_any_element(),
                        );
                    }
                    if probe.owner_mail > 0 {
                        indicators.push(
                            Icon::new(IconName::Circle)
                                .size(IconSize::XSmall)
                                .color(Color::Accent)
                                .into_any_element(),
                        );
                    }
                }
            }
        }

        let header = h_flex()
            .id(SharedString::from(format!("rig-header-{name}")))
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |panel, _, _, cx| {
                if !panel.collapsed.remove(&toggle_name) {
                    panel.collapsed.insert(toggle_name.clone());
                }
                cx.notify();
            }))
            .child(
                Icon::new(match open {
                    true => IconName::ChevronDown,
                    false => IconName::ChevronRight,
                })
                .size(IconSize::Small)
                .color(Color::Muted),
            )
            .child(Label::new(name.to_string()).size(LabelSize::Small))
            .child(Label::new(state).size(LabelSize::XSmall).color(state_color))
            .child(
                Label::new(match no_jobs {
                    true => format!("{role} · no jobs"),
                    false => role,
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when(!open, |header| header.children(indicators));

        let mut section = v_flex().w_full().child(header);
        if !open {
            if job_count > 0 {
                section = section.child(
                    div().px_2().pb_1().child(
                        Label::new(format!(
                            "{job_count} job{}",
                            if job_count == 1 { "" } else { "s" }
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
                );
            }
            return section.into_any_element();
        }
        let probe = match outcome {
            Ok(probe) => probe,
            Err(error) => {
                return section
                    .child(
                        h_flex()
                            .pl_6()
                            .pr_2()
                            .py_0p5()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Close)
                                    .size(IconSize::XSmall)
                                    .color(Color::Error),
                            )
                            .child(
                                h_flex().min_w_0().child(
                                    Label::new(error.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Error)
                                        .truncate(),
                                ),
                            ),
                    )
                    .into_any_element();
            }
        };
        section = section.child(facts_row(probe, cx));
        if !probe.quiet {
            section = section.child(
                h_flex()
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(format!("not quiet: {}", probe.not_quiet_because.join(", ")))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
            );
        }
        for job in probe
            .running
            .iter()
            .map(|job| (job, true))
            .chain(probe.queued.iter().map(|job| (job, false)))
        {
            let (job, running) = job;
            section = section.child(self.render_job(name, job, running, cx));
        }
        let mut seen: Vec<(&str, usize)> = Vec::new();
        for contender in &probe.contenders {
            match seen
                .iter_mut()
                .find(|(seen_name, _)| *seen_name == contender.name)
            {
                Some((_, count)) => *count += 1,
                None => seen.push((&contender.name, 1)),
            }
        }
        for (contender_name, count) in seen {
            let where_ = probe
                .contenders
                .iter()
                .find(|contender| contender.name == contender_name)
                .and_then(|contender| contender.cwd.clone().or_else(|| contender.command.clone()))
                .unwrap_or_default();
            let label = match count {
                1 => contender_name.to_string(),
                count => format!("{contender_name} ×{count}"),
            };
            section = section.child(
                Row::new(
                    SharedString::from(format!("contender-{name}-{contender_name}")),
                    IconName::Flame,
                    label,
                )
                .icon_color(Color::Warning)
                .title_color(Color::Warning)
                .meta(vec![(where_, Color::Muted)])
                .tooltip(Some("Heavy process — the rig is not quiet".into()))
                .render(cx),
            );
        }
        for agent in &probe.agents {
            let waiting_for_wake = agent.wake_scheduled;
            section = section.child(
                Row::new(
                    SharedString::from(format!("agent-{}-{}", name, agent.name)),
                    if waiting_for_wake {
                        IconName::CountdownTimer
                    } else {
                        IconName::Person
                    },
                    agent.name.clone(),
                )
                .icon_color(if waiting_for_wake {
                    Color::Accent
                } else {
                    Color::Muted
                })
                .meta(vec![
                    (agent.status.clone(), Color::Muted),
                    (short_path(&agent.cwd).to_string(), Color::Muted),
                ])
                .tooltip(agent.last_paragraph.clone())
                .render(cx),
            );
        }
        for (target, count) in &probe.outbox {
            section = section.child(
                Row::new(
                    SharedString::from(format!("outbox-{name}-{target}")),
                    IconName::Send,
                    target.clone(),
                )
                .meta(vec![(format!("{count} waiting"), Color::Muted)])
                .render(cx),
            );
        }
        if probe.owner_mail > 0 {
            section = section.child(
                Row::new(
                    SharedString::from(format!("owner-mail-{name}")),
                    IconName::Envelope,
                    format!("{} escalation(s) for you", probe.owner_mail),
                )
                .icon_color(Color::Accent)
                .title_color(Color::Accent)
                .meta(vec![(
                    "agent-mail recv --as owner".to_string(),
                    Color::Muted,
                )])
                .render(cx),
            );
        }
        section.into_any_element()
    }

    /// A managed job as a thread-list row: spinner (or poison warning) with
    /// the job title, and a muted `lane · id · for target` metadata line.
    /// Hovering reveals a cancel action; clicking selects it for the log tail.
    fn render_job(
        &self,
        rig_name: &str,
        job: &JobLine,
        running: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected_job.as_ref() == Some(&(rig_name.to_string(), job.id.clone()));
        let hovered = self.hovered_job.as_ref() == Some(&(rig_name.to_string(), job.id.clone()));
        let poisoned = job.poisoned;
        let (icon, icon_color, spinning) = match (running, poisoned > 0) {
            (true, false) => (IconName::LoadCircle, Color::Muted, true),
            (_, true) => (IconName::Warning, Color::Warning, false),
            (false, false) => (IconName::CountdownTimer, Color::Muted, false),
        };
        let mut meta: Vec<(String, Color)> = Vec::new();
        if let Some(position) = job.position {
            meta.push((format!("#{position}"), Color::Muted));
        }
        if job.urgent {
            meta.push(("URGENT".to_string(), Color::Warning));
        }
        if let Some(lane) = &job.lane {
            meta.push((lane.clone(), Color::Muted));
        }
        if let Some(est_secs) = job.est_secs {
            meta.push((
                format!(
                    "{} · est {}",
                    agent_mail::jobs::job_tier(job.est_secs).as_str(),
                    agent_mail::jobs::format_secs(est_secs)
                ),
                Color::Muted,
            ));
            // Real time used vs the proposal: past the estimate on a running
            // job is the boss's earliest signal to re-tier or cancel it.
            if running && job.minutes.is_some_and(|minutes| minutes.saturating_mul(60) > est_secs as i64)
            {
                meta.push(("over est".to_string(), Color::Warning));
            }
        } else {
            // A missing proposal is a discipline gap the boss should see and
            // fill with `rig set --est`.
            meta.push(("no est".to_string(), Color::Muted));
        }
        meta.push((short_id(&job.id).to_string(), Color::Muted));
        if let Some(notify) = &job.notify {
            meta.push((format!("for {notify}"), Color::Muted));
        }
        if poisoned > 0 {
            meta.push((format!("POISONED ×{poisoned}"), Color::Warning));
        }
        let (select_rig, select_id) = (rig_name.to_string(), job.id.clone());
        let (stop_rig, stop_id) = (rig_name.to_string(), job.id.clone());
        let (hover_rig, hover_id) = (rig_name.to_string(), job.id.clone());
        let row = Row::new(
            SharedString::from(format!("job-{rig_name}-{}", job.id)),
            icon,
            job.title.clone().unwrap_or_else(|| job.text.clone()),
        )
        .icon_color(icon_color)
        .spinning(spinning)
        .title_color(if poisoned > 0 {
            Color::Warning
        } else {
            Color::Default
        })
        .meta(meta)
        .timestamp(job.minutes.map(|minutes| format!("{minutes} min")))
        .tooltip(Some(job.text.clone()))
        .selected(selected)
        .hovered(hovered)
        .action(
            h_flex()
                .child(
                    IconButton::new(
                        SharedString::from(format!("stop-{rig_name}-{}", job.id)),
                        IconName::Stop,
                    )
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Cancel this job"))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.selected_job = Some((stop_rig.clone(), stop_id.clone()));
                        panel.log = None;
                        panel.confirm_cancel = true;
                        cx.notify();
                    })),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .into_any_element(),
        )
        .render(cx);
        row.cursor_pointer()
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.select_job(select_rig.clone(), select_id.clone(), cx)
            }))
            .on_hover(cx.listener(move |panel, is_hovered: &bool, _, cx| {
                let key = (hover_rig.clone(), hover_id.clone());
                match *is_hovered {
                    true => panel.hovered_job = Some(key),
                    false => {
                        if panel.hovered_job.as_ref()
                            == Some(&(hover_rig.clone(), hover_id.clone()))
                        {
                            panel.hovered_job = None;
                        }
                    }
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_log(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (rig_name, id) = self.selected_job.as_ref()?;
        let text = self.log.clone().unwrap_or_else(|| "loading log…".into());
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(LOG_LINES)..].to_vec();
        let header = h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .child(
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Terminal)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("log {} on {rig_name}", short_id(id)))
                            .size(LabelSize::Small),
                    ),
            )
            .child(match self.confirm_cancel {
                false => IconButton::new("cancel-job", IconName::Stop)
                    .icon_size(ui::IconSize::Small)
                    .tooltip(ui::Tooltip::text("Cancel this job"))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.confirm_cancel = true;
                        cx.notify();
                    }))
                    .into_any_element(),
                true => h_flex()
                    .gap_1()
                    .child(
                        Label::new("cancel?")
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        ui::Button::new("confirm-cancel", "Yes")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|panel, _, _, cx| panel.cancel_selected(cx))),
                    )
                    .child(
                        ui::Button::new("keep-job", "No")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.confirm_cancel = false;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            });
        Some(
            v_flex()
                .w_full()
                .border_t_1()
                .border_color(cx.theme().colors().border)
                .child(header)
                .child(
                    v_flex()
                        .px_2()
                        .pb_2()
                        .font_buffer(cx)
                        .children(tail.into_iter().map(|line| {
                            Label::new(line.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_mail(&self, cx: &mut Context<Self>) -> AnyElement {
        let rig_name = self
            .rigs
            .as_ref()
            .map(|rigs| rigs.self_rig.clone())
            .unwrap_or_default();
        let open = self.mail_open;
        let pending: usize = self.mail.inboxes.iter().map(|inbox| inbox.pending).sum();
        let mut section = v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .id("mail-header")
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.mail_open = !panel.mail_open;
                        cx.notify();
                    }))
                    .child(
                        Icon::new(match open {
                            true => IconName::ChevronDown,
                            false => IconName::ChevronRight,
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::Envelope)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new("Mail").size(LabelSize::Small))
                    .child(
                        Label::new(format!("on {rig_name} · read-only"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when(pending > 0, |row| {
                        row.child(
                            Label::new(format!("{pending} new"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    }),
            );
        if !open {
            return section.into_any_element();
        }
        let inboxes = self
            .mail
            .inboxes
            .iter()
            .filter(|inbox| inbox.pending > 0 || inbox.scheduled > 0);
        if inboxes.clone().count() == 0
            && self.mail.pending.is_empty()
            && self.mail.broadcasts.is_empty()
        {
            return section
                .child(
                    h_flex()
                        .pl_6()
                        .pr_2()
                        .py_1()
                        .gap_1p5()
                        .child(
                            Icon::new(IconName::Circle)
                                .size(IconSize::Small)
                                .color(Color::Placeholder),
                        )
                        .child(
                            Label::new("nothing pending")
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        ),
                )
                .into_any_element();
        }
        for inbox in inboxes {
            let name = match inbox.name.as_str() {
                agent_mail::ANY_NAME => "*",
                name => name,
            };
            let mut meta = vec![(
                format!("{} new", inbox.pending),
                if inbox.pending > 0 {
                    Color::Accent
                } else {
                    Color::Muted
                },
            )];
            if inbox.scheduled > 0 {
                meta.push((format!("{} scheduled", inbox.scheduled), Color::Muted));
            }
            let open_inbox = self.open_inbox.as_deref() == Some(inbox.name.as_str());
            let click_name = inbox.name.clone();
            let folder = Row {
                icon: if open_inbox {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                },
                ..Row::new(
                    SharedString::from(format!("inbox-{}", inbox.name)),
                    IconName::Folder,
                    name,
                )
                .meta(meta)
            }
            .render(cx)
            .cursor_pointer()
            .hover(|row| row.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.open_inbox = match panel.open_inbox.as_deref() {
                    Some(open) if open == click_name => None,
                    _ => Some(click_name.clone()),
                };
                cx.notify();
            }));
            section = section.child(folder);
            if open_inbox {
                for envelope in &inbox.messages {
                    section = section.child(self.message_row(envelope, false, cx));
                }
            }
        }
        let messages = self
            .mail
            .pending
            .iter()
            .map(|envelope| (envelope, false))
            .chain(self.mail.broadcasts.iter().map(|envelope| (envelope, true)));
        for (envelope, broadcast) in messages {
            section = section.child(self.message_row(envelope, broadcast, cx));
        }
        section.into_any_element()
    }

    /// One mail message row: click to open/close the body (read-only).
    fn message_row(
        &self,
        envelope: &Envelope,
        broadcast: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_message = self.open_message.as_deref() == Some(envelope.id.as_str());
        let first = envelope.body.lines().next().unwrap_or("").to_string();
        let mut meta: Vec<(String, Color)> = Vec::new();
        if broadcast {
            meta.push(("broadcast".to_string(), Color::Accent));
        }
        meta.push((format!("{} → {}", envelope.from, envelope.to), Color::Muted));
        let click_id = envelope.id.clone();
        let mut row = Row::new(
            SharedString::from(format!("mail-{}", envelope.id)),
            if broadcast {
                IconName::Send
            } else {
                IconName::Envelope
            },
            first,
        )
        .icon_color(Color::Muted)
        .meta(meta)
        .timestamp(Some(envelope.sent_at.get(11..16).unwrap_or("").to_string()))
        .selected(open_message)
        .render(cx)
        .cursor_pointer()
        .hover(|row| row.bg(cx.theme().colors().ghost_element_hover))
        .on_click(cx.listener(move |panel, _, _, cx| {
            panel.open_message = match panel.open_message.as_deref() == Some(click_id.as_str()) {
                true => None,
                false => Some(click_id.clone()),
            };
            cx.notify();
        }));
        if open_message {
            row = row.child(
                div()
                    .mx_1p5()
                    .mb_1()
                    .px_2()
                    .py_1p5()
                    .rounded_sm()
                    .bg(cx.theme().colors().element_background)
                    .child(
                        Label::new(envelope.body.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            );
        }
        row.into_any_element()
    }
}

impl Focusable for RigPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RigPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refreshed = match self.fleet_at {
            Some(at) => format!("refreshed {} s ago", at.elapsed().as_secs()),
            None => "loading…".into(),
        };
        let header = h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(Label::new("Rigs").size(LabelSize::Small))
            .child(
                Label::new(refreshed)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            );
        let fleet = self.fleet.clone();
        let mut body = v_flex()
            .id("rig-panel-body")
            .size_full()
            .overflow_y_scroll();
        if let Some(error) = &self.error {
            body = body.child(
                div().px_2().py_1().child(
                    Label::new(error.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                ),
            );
        }
        for (name, outcome) in &fleet {
            body = body.child(self.render_rig(name, outcome, cx));
        }
        if let Some(log) = self.render_log(cx) {
            body = body.child(log);
        }
        body = body.child(self.render_mail(cx));
        v_flex()
            .key_context("RigPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(header)
            .child(body)
    }
}

impl Panel for RigPanel {
    fn persistent_name() -> &'static str {
        RIG_PANEL_KEY
    }

    fn panel_key() -> &'static str {
        RIG_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(360.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Server)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Rigs")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Toggle.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        11
    }

    /// Poll only while visible: every open view costs SSH to every rig.
    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        match active {
            true => self.start_polling(cx),
            false => self.poll = None,
        }
    }
}

/// Status-bar summary: busy rigs, running jobs, escalations waiting.
pub struct RigBadge {
    text: Option<String>,
    _poll: Task<()>,
}

impl RigBadge {
    fn new(mailroom: Arc<Mailroom>, cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let (fleet, _, _) = fetch(mailroom.clone(), cx).await;
                let text = fleet.ok().map(|fleet| badge_text(&fleet));
                if this
                    .update(cx, |badge, cx| {
                        badge.text = text;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(BADGE_EVERY).await;
            }
        });
        Self {
            text: None,
            _poll: poll,
        }
    }
}

/// "2 busy · 1 job · 1 for you", or `None`-like empty when all is quiet.
pub fn badge_text(fleet: &Fleet) -> String {
    let busy = fleet
        .iter()
        .filter(|(_, outcome)| outcome.as_ref().is_ok_and(|probe| !probe.quiet))
        .count();
    let offline = fleet.iter().filter(|(_, outcome)| outcome.is_err()).count();
    let jobs: usize = fleet
        .iter()
        .filter_map(|(_, outcome)| outcome.as_ref().ok())
        .map(|probe| probe.running.len())
        .sum();
    let escalations: usize = fleet
        .iter()
        .filter_map(|(_, outcome)| outcome.as_ref().ok())
        .map(|probe| probe.owner_mail)
        .sum();
    let mut parts = vec![format!("{busy} busy")];
    if offline > 0 {
        parts.push(format!("{offline} offline"));
    }
    if jobs > 0 {
        parts.push(format!("{jobs} job{}", if jobs == 1 { "" } else { "s" }));
    }
    if escalations > 0 {
        parts.push(format!("{escalations} for you"));
    }
    parts.join(" · ")
}

impl Render for RigBadge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = self.text.clone() else {
            return div().into_any_element();
        };
        ui::Button::new("rig-badge", text)
            .label_size(LabelSize::Small)
            .start_icon(ui::Icon::new(IconName::Server).size(ui::IconSize::XSmall))
            .tooltip(ui::Tooltip::text("Rigs: open the panel"))
            .on_click(|_, window, cx| window.dispatch_action(Toggle.boxed_clone(), cx))
            .into_any_element()
    }
}

impl StatusItemView for RigBadge {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Only present where agent-mail is set up, which is the user-facing switch.
    fn hide_setting(&self, _cx: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

/// One list row in the thread-history grammar: a title line (status icon,
/// truncated title, hover-only action) over a muted, dot-separated metadata
/// line. Visual twin of `ui::ThreadItem` without the thread-specific slots.
struct Row {
    id: SharedString,
    icon: IconName,
    icon_color: Color,
    spinning: bool,
    title: String,
    title_color: Color,
    meta: Vec<(String, Color)>,
    timestamp: Option<String>,
    tooltip: Option<String>,
    selected: bool,
    hovered: bool,
    action: Option<AnyElement>,
}

impl Row {
    fn new(id: impl Into<SharedString>, icon: IconName, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            icon,
            icon_color: Color::Muted,
            spinning: false,
            title: title.into(),
            title_color: Color::Default,
            meta: Vec::new(),
            timestamp: None,
            tooltip: None,
            selected: false,
            hovered: false,
            action: None,
        }
    }

    fn icon_color(mut self, color: Color) -> Self {
        self.icon_color = color;
        self
    }

    fn spinning(mut self, spinning: bool) -> Self {
        self.spinning = spinning;
        self
    }

    fn title_color(mut self, color: Color) -> Self {
        self.title_color = color;
        self
    }

    fn meta(mut self, meta: Vec<(String, Color)>) -> Self {
        self.meta = meta;
        self
    }

    fn timestamp(mut self, timestamp: Option<String>) -> Self {
        self.timestamp = timestamp;
        self
    }

    fn tooltip(mut self, tooltip: Option<String>) -> Self {
        self.tooltip = tooltip;
        self
    }

    fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    fn action(mut self, action: AnyElement) -> Self {
        self.action = Some(action);
        self
    }

    fn render(self, cx: &App) -> Stateful<Div> {
        let colors = cx.theme().colors();
        let icon = if self.spinning {
            Icon::new(IconName::LoadCircle)
                .size(IconSize::Small)
                .color(self.icon_color)
                .with_rotate_animation(2)
                .into_any_element()
        } else {
            Icon::new(self.icon)
                .size(IconSize::Small)
                .color(self.icon_color)
                .into_any_element()
        };
        let meta_elements = meta_elements(self.meta, self.timestamp, cx);
        let title_line = h_flex()
            .min_w_0()
            .w_full()
            .h_6()
            .gap_1p5()
            .justify_between()
            .child(
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1p5()
                    .child(icon)
                    .child(Label::new(self.title).color(self.title_color).truncate()),
            )
            .when(self.hovered, |line| {
                line.when_some(self.action, |line, action| {
                    line.child(
                        h_flex()
                            .pr_1()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(action),
                    )
                })
            });
        v_flex()
            .id(self.id)
            .w_full()
            .px_1p5()
            .py_1()
            .overflow_hidden()
            .when(self.selected, |row| row.bg(colors.ghost_element_selected))
            .hover(|row| row.bg(colors.ghost_element_hover))
            .child(title_line)
            .when(!meta_elements.is_empty(), |row| {
                row.child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(div().size_4().flex_none())
                        .children(meta_elements),
                )
            })
            .when_some(self.tooltip, |row, tooltip| {
                row.tooltip(Tooltip::text(tooltip))
            })
    }
}

/// Muted, dot-separated metadata labels as they appear under a thread-list
/// title, with an optional trailing timestamp.
fn meta_elements(
    pieces: Vec<(String, Color)>,
    timestamp: Option<String>,
    cx: &App,
) -> Vec<AnyElement> {
    let separator_color = Color::Custom(cx.theme().colors().text_muted.opacity(0.4));
    let mut elements: Vec<AnyElement> = Vec::new();
    for (index, (text, color)) in pieces.into_iter().enumerate() {
        if index > 0 {
            elements.push(
                Label::new("•")
                    .size(LabelSize::Small)
                    .color(separator_color)
                    .into_any_element(),
            );
        }
        elements.push(
            Label::new(text)
                .size(LabelSize::Small)
                .color(color)
                .truncate()
                .into_any_element(),
        );
    }
    if let Some(timestamp) = timestamp {
        if !elements.is_empty() {
            elements.push(
                Label::new("•")
                    .size(LabelSize::Small)
                    .color(separator_color)
                    .into_any_element(),
            );
        }
        elements.push(
            Label::new(timestamp)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        );
    }
    elements
}

fn facts_row(probe: &RigProbe, cx: &App) -> AnyElement {
    let pieces = facts(probe)
        .into_iter()
        .map(|fact| (fact, Color::Muted))
        .collect();
    h_flex()
        .pl_6()
        .pr_2()
        .py_0p5()
        .min_w_0()
        .children(meta_elements(pieces, None, cx))
        .into_any_element()
}

fn facts(probe: &RigProbe) -> Vec<String> {
    let mut facts = vec![match probe.load1 {
        Some(load) => format!("load {load:.1}/{}", probe.cores),
        None => format!("{} cores", probe.cores),
    }];
    if let (Some(free), Some(total)) = (probe.mem_free_gb, probe.mem_total_gb) {
        facts.push(format!("mem {free:.0}/{total:.0} GB"));
    }
    if let Some(disk) = probe.disk_free_gb {
        facts.push(format!("disk {disk:.0} GB"));
    }
    if let Some(power) = &probe.power {
        facts.push(power.clone());
    }
    for gpu in &probe.gpus {
        facts.push(format!(
            "GPU {:.0}% {:.1}/{:.0} GB",
            gpu.util_pct, gpu.mem_used_gb, gpu.mem_total_gb
        ));
    }
    facts
}

fn short_id(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn short_path(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(json: &str) -> RigProbe {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn badge_counts_busy_offline_jobs_and_escalations() {
        let fleet: Fleet = vec![
            (
                "bmo".into(),
                Ok(probe(
                    r#"{"rig":"bmo","os":"macos","at":"","cores":30,"quiet":false,
                        "running":[{"id":"a","text":"x"},{"id":"b","text":"y"}]}"#,
                )),
            ),
            (
                "amo".into(),
                Ok(probe(
                    r#"{"rig":"amo","os":"macos","at":"","cores":30,"quiet":true,"owner_mail":1}"#,
                )),
            ),
            ("4090".into(), Err("offline".into())),
        ];
        assert_eq!(
            badge_text(&fleet),
            "1 busy · 1 offline · 2 jobs · 1 for you"
        );
        let quiet: Fleet = vec![(
            "amo".into(),
            Ok(probe(
                r#"{"rig":"amo","os":"macos","at":"","cores":30,"quiet":true}"#,
            )),
        )];
        assert_eq!(badge_text(&quiet), "0 busy");
    }

    #[test]
    fn ids_and_paths_are_shortened() {
        assert_eq!(short_id("01a19485-eeff-702f-8abd-fab50e6ffdbe"), "0e6ffdbe");
        assert_eq!(short_path("/Users/kat/git/riir-ai"), "riir-ai");
    }
}
