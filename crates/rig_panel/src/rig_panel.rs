//! Fork-owned "Rigs" dock panel + status-bar badge (plan 049 T3/T4): what
//! every rig is doing, its managed jobs with a live log tail, and this rig's
//! mail, read from the same agent-mail data as `agent-mail top`.
//!
//! Registered only where agent-mail is set up (`~/.agent-mail/rigs.json`), so
//! no stray icon shows elsewhere (the retired war room's lesson). Polls only
//! while the panel is open; the badge refreshes at most once a minute. Mail
//! is only peeked, never claimed.

use agent_mail::jobs::JobAction;
use agent_mail::rig::{self, RigProbe};
use agent_mail::{Envelope, MailboxSummary, Mailroom, Rigs};
use gpui::{
    Action, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Task, Window, actions, div, px,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::{ActiveTheme, Color, Disclosure, IconButton, IconName, Label, LabelSize, prelude::*};
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
    confirm_cancel: bool,
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
            confirm_cancel: false,
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
        let toggle_name = name.to_string();
        let mut rows = v_flex().w_full().child(
            h_flex()
                .gap_1()
                .px_2()
                .py_1()
                .child(
                    Disclosure::new(SharedString::from(format!("rig-{name}")), open).on_click(
                        cx.listener(move |panel, _, _, cx| {
                            if !panel.collapsed.remove(&toggle_name) {
                                panel.collapsed.insert(toggle_name.clone());
                            }
                            cx.notify();
                        }),
                    ),
                )
                .child(Label::new(name.to_string()).size(LabelSize::Small))
                .child(Label::new(state).size(LabelSize::Small).color(state_color))
                .child(
                    Label::new(match no_jobs {
                        true => format!("{role} · no jobs"),
                        false => role,
                    })
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        );
        if !open {
            return rows.into_any_element();
        }
        let probe = match outcome {
            Ok(probe) => probe,
            Err(error) => {
                return rows
                    .child(detail(truncate(error, 160), Color::Error))
                    .into_any_element();
            }
        };
        rows = rows.child(detail(facts(probe), Color::Muted));
        if !probe.quiet {
            rows = rows.child(detail(
                format!("not quiet: {}", probe.not_quiet_because.join(", ")),
                Color::Warning,
            ));
        }
        for job in probe
            .running
            .iter()
            .map(|job| (job, true))
            .chain(probe.queued.iter().map(|job| (job, false)))
        {
            let (job, running) = job;
            let selected = self.selected_job.as_ref() == Some(&(name.to_string(), job.id.clone()));
            let (rig_name, id) = (name.to_string(), job.id.clone());
            rows = rows.child(
                div()
                    .id(SharedString::from(format!("job-{}", job.id)))
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(cx.theme().colors().element_selected))
                    .hover(|row| row.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.select_job(rig_name.clone(), id.clone(), cx)
                    }))
                    .child(
                        Label::new(format!("{} {}", if running { "▶" } else { "…" }, job.text))
                            .size(LabelSize::XSmall)
                            .color(if running {
                                Color::Accent
                            } else {
                                Color::Default
                            }),
                    ),
            );
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
            rows = rows.child(detail(
                truncate(&format!("heavy {label}  {where_}"), 120),
                Color::Warning,
            ));
        }
        for agent in &probe.agents {
            rows = rows.child(detail(
                truncate(
                    &format!(
                        "agent {} [{}] {}",
                        agent.name,
                        agent.status,
                        short_path(&agent.cwd)
                    ),
                    120,
                ),
                Color::Muted,
            ));
        }
        for (target, count) in &probe.outbox {
            rows = rows.child(detail(
                format!("outbox {count} waiting for {target}"),
                Color::Muted,
            ));
        }
        if probe.owner_mail > 0 {
            rows = rows.child(detail(
                format!(
                    "{} escalation(s) for you: agent-mail recv --as owner",
                    probe.owner_mail
                ),
                Color::Accent,
            ));
        }
        rows.into_any_element()
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
            .child(Label::new(format!("log {} on {rig_name}", short_id(id))).size(LabelSize::Small))
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
        let mut section = v_flex()
            .w_full()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .child(
                        ui::Icon::new(IconName::Envelope)
                            .size(ui::IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("Mail on {rig_name} (read-only)"))
                            .size(LabelSize::Small),
                    ),
            );
        if self.mail.inboxes.is_empty() && self.mail.broadcasts.is_empty() {
            return section
                .child(detail("nothing pending".into(), Color::Muted))
                .into_any_element();
        }
        for inbox in &self.mail.inboxes {
            section = section.child(detail(
                format!(
                    "{}: {} new{}",
                    match inbox.name.as_str() {
                        agent_mail::ANY_NAME => "*",
                        name => name,
                    },
                    inbox.pending,
                    match inbox.scheduled {
                        0 => String::new(),
                        scheduled => format!(", {scheduled} scheduled"),
                    }
                ),
                Color::Default,
            ));
        }
        let messages: Vec<&Envelope> = self
            .mail
            .pending
            .iter()
            .chain(self.mail.broadcasts.iter())
            .collect();
        for envelope in messages {
            let open = self.open_message.as_deref() == Some(envelope.id.as_str());
            let id = envelope.id.clone();
            let first = envelope.body.lines().next().unwrap_or("").to_string();
            section = section.child(
                v_flex()
                    .id(SharedString::from(format!("mail-{}", envelope.id)))
                    .pl_6()
                    .pr_2()
                    .py_0p5()
                    .cursor_pointer()
                    .hover(|row| row.bg(cx.theme().colors().element_hover))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.open_message =
                            match panel.open_message.as_deref() == Some(id.as_str()) {
                                true => None,
                                false => Some(id.clone()),
                            };
                        cx.notify();
                    }))
                    .child(
                        Label::new(truncate(
                            &format!(
                                "{} {} → {}: {first}",
                                envelope.sent_at.get(11..16).unwrap_or(""),
                                envelope.from,
                                envelope.to
                            ),
                            120,
                        ))
                        .size(LabelSize::XSmall),
                    )
                    .when(open, |row| {
                        row.child(
                            Label::new(envelope.body.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            );
        }
        section.into_any_element()
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
            body = body.child(detail(error.clone(), Color::Error));
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

fn detail(text: String, color: Color) -> impl IntoElement {
    div()
        .pl_6()
        .pr_2()
        .child(Label::new(text).size(LabelSize::XSmall).color(color))
}

fn facts(probe: &RigProbe) -> String {
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
    facts.join(" · ")
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
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
    fn truncation_is_char_safe() {
        assert_eq!(truncate("é—日本語", 2), "é—…");
        assert_eq!(short_id("01a19485-eeff-702f-8abd-fab50e6ffdbe"), "0e6ffdbe");
        assert_eq!(short_path("/Users/kat/git/riir-ai"), "riir-ai");
    }
}
