//! `agent-mail top`: the rigs and their mail in a terminal (plan 049), for
//! rigs without an IDE (bmo) or an SSH session anywhere. Everything is read
//! from the same data `rig status --json` uses; mail is only peeked, never
//! claimed. Refresh is deliberately slow (fleet every 15 s, a job log every
//! 3 s only while it is shown): the retired war room died of per-view request
//! amplification, and probes are cached per rig anyway.

use crate::jobs::JobAction;
use crate::rig::{self, RigProbe};
use crate::{Envelope, MailboxSummary, Mailroom, Rigs};
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const LOG_EVERY: Duration = Duration::from_secs(3);
const LOG_LINES: usize = 40;
const MAIL_RECENT: usize = 30;

type Fleet = Vec<(String, std::result::Result<RigProbe, String>, Duration)>;

enum Request {
    Fleet,
    Log { rig: String, id: String },
    Cancel { rig: String, id: String },
}

enum Update {
    Fleet(Fleet),
    Log {
        rig: String,
        id: String,
        text: String,
    },
    Notice(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Rigs,
    Mail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Rigs,
    Jobs,
}

struct JobRow {
    id: String,
    text: String,
    running: bool,
}

struct App {
    tab: Tab,
    focus: Focus,
    rigs: Rigs,
    fleet: Fleet,
    fleet_at: Option<Instant>,
    rig_state: ListState,
    job_state: ListState,
    mail: MailboxSummary,
    mail_state: ListState,
    log: Option<(String, String, String)>,
    notice: String,
    confirm_cancel: Option<(String, String)>,
}

impl App {
    fn selected_probe(&self) -> Option<(&String, Option<&RigProbe>)> {
        let (name, outcome, _) = self.fleet.get(self.rig_state.selected()?)?;
        Some((name, outcome.as_ref().ok()))
    }

    fn jobs(&self) -> Vec<JobRow> {
        let Some((_, Some(probe))) = self.selected_probe() else {
            return Vec::new();
        };
        probe
            .running
            .iter()
            .map(|job| JobRow {
                id: job.id.clone(),
                text: format!("▶ {}", job.text),
                running: true,
            })
            .chain(probe.queued.iter().map(|job| JobRow {
                id: job.id.clone(),
                text: format!("… {}", job.text),
                running: false,
            }))
            .collect()
    }

    fn selected_job(&self) -> Option<(String, JobRow)> {
        let (rig, _) = self.selected_probe()?;
        let rig = rig.clone();
        let index = self.job_state.selected()?;
        self.jobs().into_iter().nth(index).map(|job| (rig, job))
    }

    fn mail_items(&self) -> Vec<&Envelope> {
        self.mail
            .pending
            .iter()
            .chain(self.mail.broadcasts.iter())
            .collect()
    }
}

/// Runs the terminal UI until `q`.
pub fn run(mailroom: &Mailroom, fleet_every: Duration) -> Result<()> {
    let rigs = mailroom.rigs()?;
    let (request_sender, request_receiver) = mpsc::channel();
    let (update_sender, update_receiver) = mpsc::channel();
    spawn_worker(
        mailroom.home().to_path_buf(),
        request_receiver,
        update_sender,
    );
    let mut app = App {
        tab: Tab::Rigs,
        focus: Focus::Rigs,
        rigs,
        fleet: Vec::new(),
        fleet_at: None,
        rig_state: ListState::default().with_selected(Some(0)),
        job_state: ListState::default(),
        mail: mailroom.mailbox_summary(MAIL_RECENT),
        mail_state: ListState::default().with_selected(Some(0)),
        log: None,
        notice: "loading rigs…".into(),
        confirm_cancel: None,
    };
    request_sender.send(Request::Fleet).ok();
    let mut terminal = ratatui::init();
    let result = event_loop(
        &mut terminal,
        &mut app,
        mailroom,
        &request_sender,
        &update_receiver,
        fleet_every,
    );
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    mailroom: &Mailroom,
    requests: &Sender<Request>,
    updates: &Receiver<Update>,
    fleet_every: Duration,
) -> Result<()> {
    let mut last_fleet_request = Instant::now();
    let mut last_log_request = Instant::now() - LOG_EVERY;
    loop {
        while let Ok(update) = updates.try_recv() {
            match update {
                Update::Fleet(fleet) => {
                    app.fleet = fleet;
                    app.fleet_at = Some(Instant::now());
                    app.mail = mailroom.mailbox_summary(MAIL_RECENT);
                    if app
                        .rig_state
                        .selected()
                        .is_none_or(|index| index >= app.fleet.len())
                    {
                        app.rig_state.select(Some(0));
                    }
                    app.notice.clear();
                }
                Update::Log { rig, id, text } => app.log = Some((rig, id, text)),
                Update::Notice(text) => app.notice = text,
            }
        }
        if last_fleet_request.elapsed() >= fleet_every {
            last_fleet_request = Instant::now();
            requests.send(Request::Fleet).ok();
        }
        // Tail only the job on screen, and only while it runs.
        if let Some((rig, job)) = app.selected_job()
            && job.running
            && last_log_request.elapsed() >= LOG_EVERY
        {
            last_log_request = Instant::now();
            requests.send(Request::Log { rig, id: job.id }).ok();
        }
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some((rig, id)) = app.confirm_cancel.take() {
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                requests.send(Request::Cancel { rig, id }).ok();
                app.notice = "cancelling…".into();
            } else {
                app.notice = "cancel aborted".into();
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('1') => app.tab = Tab::Rigs,
            KeyCode::Char('2') | KeyCode::Char('m') => app.tab = Tab::Mail,
            KeyCode::Char('r') => {
                last_fleet_request = Instant::now();
                requests.send(Request::Fleet).ok();
                app.notice = "refreshing…".into();
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Rigs if !app.jobs().is_empty() => {
                        app.job_state.select(Some(0));
                        Focus::Jobs
                    }
                    _ => Focus::Rigs,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => move_selection(app, 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(app, -1),
            KeyCode::Enter | KeyCode::Char('l') => {
                if let Some((rig, job)) = app.selected_job() {
                    last_log_request = Instant::now();
                    requests.send(Request::Log { rig, id: job.id }).ok();
                }
            }
            KeyCode::Char('c') => {
                if let Some((rig, job)) = app.selected_job() {
                    app.notice = format!("cancel job {} on {rig}? (y/n)", short(&job.id));
                    app.confirm_cancel = Some((rig, job.id));
                }
            }
            _ => {}
        }
    }
}

fn move_selection(app: &mut App, delta: i32) {
    let (state, length) = match (app.tab, app.focus) {
        (Tab::Mail, _) => (
            &mut app.mail_state,
            app.mail.pending.len() + app.mail.broadcasts.len(),
        ),
        (Tab::Rigs, Focus::Rigs) => {
            app.log = None;
            app.job_state.select(None);
            (&mut app.rig_state, app.fleet.len())
        }
        (Tab::Rigs, Focus::Jobs) => {
            let length = app.jobs().len();
            app.log = None;
            (&mut app.job_state, length)
        }
    };
    if length == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as i32;
    let next = (current + delta).rem_euclid(length as i32) as usize;
    state.select(Some(next));
}

fn spawn_worker(home: std::path::PathBuf, requests: Receiver<Request>, updates: Sender<Update>) {
    std::thread::spawn(move || {
        let mailroom = Mailroom::new(home);
        for request in requests {
            let update = match request {
                Request::Fleet => match rig::status(&mailroom, &[]) {
                    Ok(results) => Update::Fleet(
                        results
                            .into_iter()
                            .map(|(name, outcome, elapsed)| {
                                (name, outcome.map_err(|error| format!("{error:#}")), elapsed)
                            })
                            .collect(),
                    ),
                    Err(error) => Update::Notice(format!("status failed: {error:#}")),
                },
                Request::Log { rig, id } => {
                    let text = mailroom
                        .job_on_rig(&rig, &id, JobAction::Log(LOG_LINES))
                        .unwrap_or_else(|error| format!("(log unavailable: {error:#})"));
                    Update::Log { rig, id, text }
                }
                Request::Cancel { rig, id } => Update::Notice(
                    mailroom
                        .job_on_rig(&rig, &id, JobAction::Cancel)
                        .unwrap_or_else(|error| format!("cancel failed: {error:#}")),
                ),
            };
            if updates.send(update).is_err() {
                return;
            }
        }
    });
}

fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn state_style(probe: Option<&RigProbe>) -> (&'static str, Color) {
    match probe {
        None => ("OFFLINE", Color::Red),
        Some(probe) if probe.quiet => ("QUIET", Color::Green),
        Some(_) => ("BUSY", Color::Yellow),
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());
    let refreshed = app
        .fleet_at
        .map(|at| format!("refreshed {} s ago", at.elapsed().as_secs()))
        .unwrap_or_else(|| "loading".into());
    let tabs = Tabs::new(vec!["1 Rigs", "2 Mail"])
        .select(match app.tab {
            Tab::Rigs => 0,
            Tab::Mail => 1,
        })
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        )
        .block(Block::default().borders(Borders::ALL).title(format!(
            " agent-mail top · {} · {refreshed} ",
            app.rigs.self_rig
        )));
    frame.render_widget(tabs, header);
    match app.tab {
        Tab::Rigs => draw_rigs(frame, app, body),
        Tab::Mail => draw_mail(frame, app, body),
    }
    let help = match (&app.confirm_cancel, app.notice.is_empty()) {
        (Some(_), _) | (None, false) => app.notice.clone(),
        _ => "q quit · 1/2 tabs · Tab rigs⇄jobs · ↑↓ select · Enter log · c cancel job · r refresh"
            .into(),
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn draw_rigs(frame: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .areas(area);
    let items: Vec<ListItem> = app
        .fleet
        .iter()
        .map(|(name, outcome, _)| {
            let probe = outcome.as_ref().ok();
            let (state, color) = state_style(probe);
            let role = app
                .rigs
                .rigs
                .get(name)
                .and_then(|rig| rig.role.clone())
                .unwrap_or_default();
            let jobs = probe.map_or(0, |probe| probe.running.len() + probe.queued.len());
            ListItem::new(Line::from(vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::styled(
                    format!("{name:<5} "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{state:<7}"), Style::default().fg(color)),
                Span::raw(match jobs {
                    0 => format!(" {role}"),
                    count => format!(" {role} · {count} job(s)"),
                }),
            ]))
        })
        .collect();
    let rig_border = match app.focus {
        Focus::Rigs => Color::Cyan,
        Focus::Jobs => Color::DarkGray,
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" rigs ")
                .border_style(Style::default().fg(rig_border)),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, left, &mut app.rig_state);

    let [details, jobs_area, log_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Length(6),
            Constraint::Min(4),
        ])
        .areas(right);
    let width = details.width.saturating_sub(4) as usize;
    let lines: Vec<Line> = match app.selected_probe() {
        None => vec![Line::from("no rig selected")],
        Some((name, None)) => {
            let error = app
                .fleet
                .iter()
                .find(|(rig, _, _)| rig == name)
                .and_then(|(_, outcome, _)| outcome.as_ref().err().cloned())
                .unwrap_or_default();
            vec![Line::styled(
                format!("{name} offline: {}", clip(&error, width * 2)),
                Style::default().fg(Color::Red),
            )]
        }
        Some((_, Some(probe))) => rig_lines(probe, width),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" state ")),
        details,
    );

    let jobs = app.jobs();
    let job_items: Vec<ListItem> = match jobs.is_empty() {
        true => vec![ListItem::new("no managed jobs (agent-mail rig run …)")],
        false => jobs
            .iter()
            .map(|job| ListItem::new(clip(&job.text, width)))
            .collect(),
    };
    let job_border = match app.focus {
        Focus::Jobs => Color::Cyan,
        Focus::Rigs => Color::DarkGray,
    };
    frame.render_stateful_widget(
        List::new(job_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" jobs (Tab) ")
                    .border_style(Style::default().fg(job_border)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        jobs_area,
        &mut app.job_state,
    );

    let (title, text) = match &app.log {
        Some((rig, id, text)) => (format!(" log {} on {rig} ", short(id)), text.clone()),
        None => (
            " log ".into(),
            "select a job (Tab, ↑↓) and press Enter".into(),
        ),
    };
    let visible = log_area.height.saturating_sub(2) as usize;
    let tail: Vec<&str> = text.lines().collect();
    let tail = tail[tail.len().saturating_sub(visible)..].join("\n");
    frame.render_widget(
        Paragraph::new(tail).block(Block::default().borders(Borders::ALL).title(title)),
        log_area,
    );
}

fn rig_lines(probe: &RigProbe, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut facts = vec![match probe.load1 {
        Some(load) => format!("load {load:.1}/{}", probe.cores),
        None => format!("{} cores", probe.cores),
    }];
    if let (Some(free), Some(total)) = (probe.mem_free_gb, probe.mem_total_gb) {
        facts.push(format!("mem {free:.0}/{total:.0} GB free"));
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
    lines.push(Line::from(facts.join(" · ")));
    if !probe.quiet {
        lines.push(Line::styled(
            format!("not quiet: {}", probe.not_quiet_because.join(", ")),
            Style::default().fg(Color::Yellow),
        ));
    }
    let mut seen: Vec<(&str, usize)> = Vec::new();
    for contender in &probe.contenders {
        match seen.iter_mut().find(|(name, _)| *name == contender.name) {
            Some((_, count)) => *count += 1,
            None => seen.push((&contender.name, 1)),
        }
    }
    for (name, count) in seen {
        let first = probe
            .contenders
            .iter()
            .find(|contender| contender.name == name);
        let detail = first
            .and_then(|contender| contender.cwd.clone().or_else(|| contender.command.clone()))
            .unwrap_or_default();
        let label = match count {
            1 => name.to_string(),
            count => format!("{name} ×{count}"),
        };
        lines.push(Line::styled(
            clip(&format!("heavy {label}  {detail}"), width),
            Style::default().fg(Color::Yellow),
        ));
    }
    for agent in &probe.agents {
        lines.push(Line::from(clip(
            &format!("agent {} [{}] {}", agent.name, agent.status, agent.cwd),
            width,
        )));
    }
    for (target, count) in &probe.outbox {
        lines.push(Line::from(format!("outbox {count} waiting for {target}")));
    }
    if probe.owner_mail > 0 {
        lines.push(Line::styled(
            format!("owner: {} escalation(s) waiting", probe.owner_mail),
            Style::default().fg(Color::Magenta),
        ));
    }
    lines
}

fn draw_mail(frame: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .areas(area);
    let mut summary: Vec<Line> = vec![Line::styled(
        format!("{} (read-only view)", app.rigs.self_rig),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    match app.mail.inboxes.is_empty() {
        true => summary.push(Line::from("no pending mail")),
        false => {
            for inbox in &app.mail.inboxes {
                summary.push(Line::from(format!(
                    "{:<20} {} new{}",
                    match inbox.name.as_str() {
                        crate::ANY_NAME => "*",
                        name => name,
                    },
                    inbox.pending,
                    match inbox.scheduled {
                        0 => String::new(),
                        scheduled => format!(", {scheduled} scheduled"),
                    }
                )));
            }
        }
    }
    summary.push(Line::from(format!(
        "broadcasts {}",
        app.mail.broadcasts.len()
    )));
    for (target, count) in &app.mail.outbox {
        summary.push(Line::from(format!("outbox → {target}: {count}")));
    }
    frame.render_widget(
        Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title(" mailboxes ")),
        left,
    );

    let [list_area, body_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .areas(right);
    let width = list_area.width.saturating_sub(4) as usize;
    let messages = app.mail_items();
    let items: Vec<ListItem> = match messages.is_empty() {
        true => vec![ListItem::new("nothing pending")],
        false => messages
            .iter()
            .map(|envelope| {
                let when = envelope.sent_at.get(11..16).unwrap_or("");
                ListItem::new(clip(
                    &format!(
                        "{when} {} → {}  {}",
                        envelope.from,
                        envelope.to,
                        envelope.body.lines().next().unwrap_or("")
                    ),
                    width,
                ))
            })
            .collect(),
    };
    let body = app
        .mail_state
        .selected()
        .and_then(|index| messages.get(index))
        .map(|envelope| {
            format!(
                "from {}\nto {}\nsent {}{}\n\n{}",
                envelope.from,
                envelope.to,
                envelope.sent_at,
                envelope
                    .deliver_at
                    .as_deref()
                    .map(|at| format!(" (scheduled {at})"))
                    .unwrap_or_default(),
                envelope.body
            )
        })
        .unwrap_or_default();
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" messages (pending + broadcasts) "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut app.mail_state,
    );
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" message ")),
        body_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app() -> App {
        let busy: RigProbe = serde_json::from_str(
            r##"{"rig":"bmo","os":"macos","at":"","cores":30,"load1":2.5,"quiet":false,
                "not_quiet_because":["1 contender(s)"],
                "contenders":[{"pid":7,"name":"bench_809","cpu_pct":95.0,"rss_gb":8.8,"arm":"name","cwd":"/Users/k/git/riir-ai"}],
                "agents":[{"name":"katgpt-rs-30","status":"idle","cwd":"/Users/k/git/katgpt-rs"}],
                "running":[{"id":"01a19485-eeff-702f-8abd-fab50e6ffdbe","text":"0e6ffdbe Shared 9 min · zed release build — 日本 · for zed-bb@m3"}],
                "queued":[{"id":"01a19485-0000-702f-8abd-000000000001","text":"#1 00000001 Exclusive · bench"}]}"##,
        )
        .unwrap();
        let rigs: Rigs = serde_json::from_str(
            r#"{"self":"bmo","rigs":{"bmo":{"ssh":[],"role":"runner"},"4090":{"ssh":[],"role":"cuda/heavy"}}}"#,
        )
        .unwrap();
        App {
            tab: Tab::Rigs,
            focus: Focus::Jobs,
            rigs,
            fleet: vec![
                ("bmo".into(), Ok(busy), Duration::ZERO),
                (
                    "4090".into(),
                    Err("offline: timed out".into()),
                    Duration::ZERO,
                ),
            ],
            fleet_at: Some(Instant::now()),
            rig_state: ListState::default().with_selected(Some(0)),
            job_state: ListState::default().with_selected(Some(0)),
            mail: MailboxSummary::default(),
            mail_state: ListState::default().with_selected(Some(0)),
            log: Some((
                "bmo".into(),
                "01a19485-eeff-702f-8abd-fab50e6ffdbe".into(),
                "Compiling ui\nCompiling auto_prompt".into(),
            )),
            notice: String::new(),
            confirm_cancel: None,
        }
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn rigs_tab_shows_state_jobs_heavy_work_and_log() {
        let mut app = app();
        let text = screen(&mut app, 120, 32);
        assert!(text.contains("BUSY"), "{text}");
        assert!(text.contains("OFFLINE"), "{text}");
        assert!(text.contains("bench_809"), "{text}");
        assert!(text.contains("zed release build"), "{text}");
        assert!(text.contains("Compiling auto_prompt"), "{text}");
        assert_eq!(
            app.selected_job().map(|(rig, job)| (rig, job.running)),
            Some(("bmo".into(), true))
        );
    }

    #[test]
    fn tiny_terminals_and_the_mail_tab_do_not_panic() {
        let mut app = app();
        screen(&mut app, 20, 8);
        app.tab = Tab::Mail;
        app.mail.broadcasts.push(Envelope {
            id: "x".into(),
            from: "g10-audit@m3".into(),
            to: "all@m3".into(),
            sent_at: "2026-11-01T04:10:00.000Z".into(),
            reply_to: None,
            deliver_at: None,
            mode: Default::default(),
            body: "HEADS-UP: repos vanished — clone them back".into(),
        });
        let text = screen(&mut app, 100, 24);
        assert!(text.contains("g10-audit@m3"), "{text}");
        assert!(text.contains("HEADS-UP"), "{text}");
        screen(&mut app, 12, 5);
    }
}
