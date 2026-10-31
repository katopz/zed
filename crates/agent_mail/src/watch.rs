//! The orchestrator watchdog (plan 048 W1): a zero-token tick on the
//! orchestrator rig that turns `rig status` into incidents, and only when an
//! incident is new (or still unresolved after its cooldown) hands a small
//! digest to the LLM orchestrator.
//!
//! Thread handoffs trigger it immediately: every Claude `Stop` saves that
//! session's last paragraph locally, and `SessionStart` sends one event to the
//! orchestrator carrying the previous thread's last paragraph in the same
//! folder, which is the handoff summary.

use crate::rig::{self, RigProbe};
use crate::{Address, Mailroom, validate_name};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Inbox on the orchestrator rig that receives handoff events.
pub const WATCH_INBOX: &str = "_watch";
/// Inbox on the orchestrator rig that receives digests when no
/// `orchestrator_command` is configured.
pub const ORCHESTRATOR_INBOX: &str = "orchestrator";
const TRANSCRIPT_TAIL_BYTES: u64 = 128 * 1024;
const PARAGRAPH_MAX_CHARS: usize = 400;
const PARAGRAPH_MIN_CHARS: usize = 80;
/// A previous thread that ended longer ago than this is not a handoff.
const HANDOFF_WINDOW: chrono::Duration = chrono::Duration::minutes(60);
const LAST_RETENTION: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const DIGEST_MAX_CHARS: usize = 6000;
const ESCALATE_AFTER_ROUNDS: u32 = 3;
/// Drift must persist this many ticks: agents push all day, so a passing
/// difference between rigs is normal.
const DRIFT_TICKS: u32 = 6;
const TICK_LOCK_STALE: Duration = Duration::from_secs(10 * 60);
const BACKLOG_ITEMS: usize = 8;
const BACKLOG_ITEM_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    #[serde(default = "default_interval")]
    pub interval_minutes: u64,
    /// Same incident re-wakes the orchestrator at most this often.
    #[serde(default = "default_cooldown")]
    pub cooldown_minutes: u64,
    /// Runs once per digest with the digest on stdin (runtime A:
    /// `claude -p ...`). Without it, digests are mailed to `orchestrator@<this rig>`.
    #[serde(default)]
    pub orchestrator_command: Option<Vec<String>>,
    /// Extra environment for the orchestrator, e.g. Claude Code's
    /// `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`: the digest carries every
    /// instruction, so the owner's global CLAUDE.md is pure token cost there.
    #[serde(default)]
    pub orchestrator_env: BTreeMap<String, String>,
}

fn default_interval() -> u64 {
    10
}

fn default_cooldown() -> u64 {
    30
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            interval_minutes: default_interval(),
            cooldown_minutes: default_cooldown(),
            orchestrator_command: None,
            orchestrator_env: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WatchState {
    last_tick_ms: i64,
    #[serde(default)]
    incidents: BTreeMap<String, IncidentState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IncidentState {
    first_seen_ms: i64,
    ticks: u32,
    rounds: u32,
    last_woke_ms: i64,
    detail: String,
}

/// One thing the orchestrator should look at.
#[derive(Debug, Clone, PartialEq)]
pub struct Incident {
    /// Stable identity: the same problem keeps its fingerprint across ticks.
    pub fingerprint: String,
    pub detail: String,
    /// Ticks the condition must persist before it counts.
    pub min_ticks: u32,
}

/// A thread handoff, sent by the SessionStart hook to the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HandoffEvent {
    pub rig: String,
    pub cwd: String,
    pub session: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub previous: Option<SavedParagraph>,
}

/// The last paragraph a session wrote, saved by its Stop hook.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedParagraph {
    pub session: String,
    pub cwd: String,
    pub at: String,
    pub paragraph: String,
}

/// The final assistant text of a Claude transcript (`.jsonl`), reduced to its
/// last paragraph (extended backwards when that is too short to say anything),
/// read from the file's tail only.
pub fn last_paragraph(transcript: &Path) -> Option<String> {
    let mut file = fs::File::open(transcript).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(TRANSCRIPT_TAIL_BYTES),
    ))
    .ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let text = tail.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value.get("type")?.as_str()? != "assistant" {
            return None;
        }
        let text = value
            .get("message")?
            .get("content")?
            .as_array()?
            .iter()
            .filter(|block| block.get("type").and_then(|kind| kind.as_str()) == Some("text"))
            .filter_map(|block| block.get("text")?.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.trim().is_empty()).then_some(text)
    })?;
    Some(tail_paragraphs(&text))
}

fn tail_paragraphs(text: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut length = 0;
    for paragraph in text.rsplit("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if length >= PARAGRAPH_MIN_CHARS {
            break;
        }
        length += paragraph.chars().count();
        kept.push(paragraph);
    }
    kept.reverse();
    let joined = kept.join("\n\n");
    let total = joined.chars().count();
    match total > PARAGRAPH_MAX_CHARS {
        true => {
            let skip = total - PARAGRAPH_MAX_CHARS;
            format!("…{}", joined.chars().skip(skip).collect::<String>())
        }
        false => joined,
    }
}

/// Claude stores a session transcript under its working directory with `/`
/// (and `.`) mapped to `-`.
pub fn transcript_path(claude_home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    let project = cwd.replace(['/', '\\', '.', ':'], "-");
    claude_home
        .join("projects")
        .join(project)
        .join(format!("{session_id}.jsonl"))
}

impl Mailroom {
    fn watch_dir(&self) -> PathBuf {
        self.home().join("watch")
    }

    /// Stop hook: remember this session's last paragraph (local only).
    pub(crate) fn save_last_paragraph(&self, session: &str, cwd: &str, transcript: &Path) {
        let Some(paragraph) = last_paragraph(transcript) else {
            return;
        };
        let Ok(session) = validate_name(session).map(str::to_string) else {
            return;
        };
        let dir = self.watch_dir().join("last");
        if fs::create_dir_all(&dir).is_err() {
            return;
        }
        let saved = SavedParagraph {
            session: session.clone(),
            cwd: cwd.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            paragraph,
        };
        if let Ok(bytes) = serde_json::to_vec(&saved) {
            let staged = dir.join(format!(".{session}.tmp"));
            if fs::write(&staged, bytes).is_ok() {
                fs::rename(&staged, dir.join(format!("{session}.json"))).ok();
            }
        }
    }

    /// SessionStart hook: one event per new thread, carrying the previous
    /// thread's last paragraph from the same folder (the handoff summary).
    pub(crate) fn send_handoff_event(&self, session: &str, cwd: &str, source: Option<String>) {
        let Ok(rigs) = self.rigs() else {
            return;
        };
        let Some(orchestrator) = rigs.orchestrator.clone() else {
            return;
        };
        let previous = self.previous_paragraph(session, cwd);
        let event = HandoffEvent {
            rig: rigs.self_rig,
            cwd: cwd.to_string(),
            session: session.to_string(),
            source,
            previous,
        };
        let Ok(body) = serde_json::to_string(&event) else {
            return;
        };
        let Ok(to) = Address::parse(&format!("{WATCH_INBOX}@{orchestrator}")) else {
            return;
        };
        if let Ok(envelope) = self.compose(&to, "hook", body, None) {
            // Unreachable orchestrator: parked in the outbox, retried later.
            self.send(&envelope).ok();
        }
    }

    fn previous_paragraph(&self, session: &str, cwd: &str) -> Option<SavedParagraph> {
        let dir = self.watch_dir().join("last");
        let now = chrono::Utc::now();
        let mut newest: Option<SavedParagraph> = None;
        for entry in fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            let expired = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > LAST_RETENTION);
            if expired {
                fs::remove_file(&path).ok();
                continue;
            }
            let Ok(saved) = serde_json::from_slice::<SavedParagraph>(&fs::read(&path).ok()?) else {
                continue;
            };
            let recent = chrono::DateTime::parse_from_rfc3339(&saved.at)
                .is_ok_and(|at| now.signed_duration_since(at) < HANDOFF_WINDOW);
            if saved.session == session || saved.cwd != cwd || !recent {
                continue;
            }
            if newest.as_ref().is_none_or(|newest| saved.at > newest.at) {
                newest = Some(saved);
            }
        }
        newest
    }

    pub fn watch_config(&self) -> WatchConfig {
        fs::read(self.home().join("watch.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn set_watch_interval(&self, minutes: u64) -> Result<PathBuf> {
        anyhow::ensure!(minutes >= 1, "interval must be at least 1 minute");
        let mut config = self.watch_config();
        config.interval_minutes = minutes;
        let path = self.home().join("watch.json");
        fs::create_dir_all(self.home())?;
        fs::write(&path, serde_json::to_string_pretty(&config)? + "\n")?;
        Ok(path)
    }

    /// Sets the watchdog interval on the orchestrator rig, wherever it is.
    pub fn set_orchestrator_interval(&self, minutes: u64) -> Result<()> {
        let rigs = self.rigs()?;
        let orchestrator = rigs
            .orchestrator
            .clone()
            .context("no orchestrator rig in rigs.json")?;
        if orchestrator == rigs.self_rig {
            return self.set_watch_interval(minutes).map(|_| ());
        }
        let rig = rigs
            .rigs
            .get(&orchestrator)
            .with_context(|| format!("unknown orchestrator rig {orchestrator:?}"))?;
        let command = format!(
            "{} rig watch --interval {minutes}",
            rig.bin.as_deref().unwrap_or("agent-mail")
        );
        let mut failures = Vec::new();
        for target in &rig.ssh {
            match self.ssh(target, &command, "wrote ") {
                Ok(_) => return Ok(()),
                Err(error) => failures.push(format!("{target}: {error:#}")),
            }
        }
        anyhow::bail!("could not reach {orchestrator}: {}", failures.join("; "))
    }

    /// When the watchdog last ran a full tick here (shown by `rig status`).
    pub fn watch_last_tick(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let state: WatchState =
            serde_json::from_slice(&fs::read(self.watch_dir().join("state.json")).ok()?).ok()?;
        chrono::DateTime::from_timestamp_millis(state.last_tick_ms)
    }
}

/// What a tick did, for the log line.
#[derive(Debug, Default)]
pub struct TickReport {
    pub ran: bool,
    pub events: usize,
    pub incidents: usize,
    pub woke: usize,
    pub escalated: usize,
}

/// One watchdog tick. Exits immediately unless the interval elapsed, a
/// handoff event arrived, or `force` is set; costs no tokens unless an
/// incident is new or due again.
pub fn tick(mailroom: &Mailroom, force: bool) -> Result<TickReport> {
    let config = mailroom.watch_config();
    let dir = mailroom.watch_dir();
    fs::create_dir_all(&dir)?;
    let state_path = dir.join("state.json");
    let mut state: WatchState = fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();
    let interval_ms = i64::try_from(config.interval_minutes)
        .unwrap_or(10)
        .saturating_mul(60_000);
    let watch_names = [WATCH_INBOX.to_string()];
    let due = force || now - state.last_tick_ms >= interval_ms;
    if !due && !mailroom.has_mail(&watch_names) {
        return Ok(TickReport::default());
    }
    let _lock = match TickLock::acquire(&dir.join("tick.lock")) {
        Some(lock) => lock,
        None => return Ok(TickReport::default()),
    };
    let events: Vec<HandoffEvent> = mailroom
        .claim(&watch_names)?
        .into_iter()
        .filter_map(|envelope| serde_json::from_str(&envelope.body).ok())
        .collect();
    let rigs = mailroom.rigs()?;
    let results = rig::status(mailroom, &[])?;
    let incidents = detect(&results, &rigs, &events, interval_ms);

    let cooldown_ms = i64::try_from(config.cooldown_minutes)
        .unwrap_or(30)
        .saturating_mul(60_000);
    let mut report = TickReport {
        ran: true,
        events: events.len(),
        incidents: incidents.len(),
        ..TickReport::default()
    };
    let mut to_wake = Vec::new();
    let mut to_escalate = Vec::new();
    let current: Vec<&str> = incidents
        .iter()
        .map(|incident| incident.fingerprint.as_str())
        .collect();
    state
        .incidents
        .retain(|fingerprint, _| current.contains(&fingerprint.as_str()));
    for incident in &incidents {
        let entry = state
            .incidents
            .entry(incident.fingerprint.clone())
            .or_insert_with(|| IncidentState {
                first_seen_ms: now,
                ..IncidentState::default()
            });
        entry.ticks += 1;
        entry.detail = incident.detail.clone();
        if entry.ticks < incident.min_ticks {
            continue;
        }
        // Escalated once: stay silent until the incident clears, so a stuck
        // problem never keeps waking agents (and spending their tokens).
        if entry.rounds > ESCALATE_AFTER_ROUNDS {
            continue;
        }
        // Details drift every tick (idle minutes, heads), so only a new
        // fingerprint wakes immediately; a known one waits out its cooldown.
        if entry.rounds > 0 && now - entry.last_woke_ms < cooldown_ms {
            continue;
        }
        entry.rounds += 1;
        entry.last_woke_ms = now;
        match entry.rounds > ESCALATE_AFTER_ROUNDS {
            true => to_escalate.push(incident.clone()),
            false => to_wake.push(incident.clone()),
        }
    }
    state.last_tick_ms = now;
    fs::write(&state_path, serde_json::to_vec_pretty(&state)?)?;

    if !to_wake.is_empty() {
        let mut digest = digest(&to_wake, &results, &rigs.self_rig);
        // Work to hand out, only when someone is idle: zero cost otherwise.
        if to_wake
            .iter()
            .any(|incident| incident.fingerprint.starts_with("stalled:"))
        {
            let items = backlog(&repo_roots(), chrono::Utc::now());
            if !items.is_empty() {
                digest.push_str(
                    "\n## Backlog (unclaimed `- [ ]` items, newest plans first; suggest one to an idle \
                     agent in the same repo, it claims it with the `**Claimed:**` lease)\n",
                );
                for item in items {
                    digest.push_str(&format!("- {item}\n"));
                }
            }
        }
        fs::write(dir.join("last_digest.md"), &digest).ok();
        deliver_digest(mailroom, &config, &rigs.self_rig, digest)?;
        report.woke = to_wake.len();
    }
    if !to_escalate.is_empty() {
        let body = format!(
            "Orchestrator escalation: {} incident(s) still unresolved after {ESCALATE_AFTER_ROUNDS} rounds.\n{}",
            to_escalate.len(),
            to_escalate
                .iter()
                .map(|incident| format!("- {}", incident.detail))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if let Ok(to) = Address::parse(&format!("{}@m3", rig::OWNER_INBOX))
            && let Ok(envelope) = mailroom.compose(&to, "orchestrator", body, None)
        {
            mailroom.send(&envelope).ok();
        }
        report.escalated = to_escalate.len();
    }
    Ok(report)
}

struct TickLock(PathBuf);

impl TickLock {
    fn acquire(path: &Path) -> Option<Self> {
        let stale = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > TICK_LOCK_STALE);
        if stale {
            fs::remove_file(path).ok();
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .ok()
            .map(|_| Self(path.to_path_buf()))
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        fs::remove_file(&self.0).ok();
    }
}

/// The deterministic rules. Pure over the fleet snapshot so it is testable.
pub fn detect(
    results: &[(String, Result<RigProbe>, Duration)],
    rigs: &crate::Rigs,
    events: &[HandoffEvent],
    interval_ms: i64,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    let accepts_jobs = |name: &str| rigs.rigs.get(name).is_none_or(|rig| rig.accepts_jobs);
    let online: Vec<(&String, &RigProbe)> = results
        .iter()
        .filter_map(|(name, outcome, _)| outcome.as_ref().ok().map(|probe| (name, probe)))
        .collect();

    for (name, outcome, _) in results {
        // m3 travels and sleeps; its absence is normal.
        if outcome.is_err() && accepts_jobs(name) {
            incidents.push(Incident {
                fingerprint: format!("offline:{name}"),
                detail: format!("rig {name} is offline"),
                min_ticks: 2,
            });
        }
    }
    for (name, probe) in &online {
        if let Some(reason) = &probe.blind {
            incidents.push(Incident {
                fingerprint: format!("blind:{name}"),
                detail: format!("rig {name} cannot classify processes: {reason}"),
                min_ticks: 1,
            });
        }
        if !accepts_jobs(name) && !probe.contenders.is_empty() {
            let commands = summarize_contenders(probe);
            incidents.push(Incident {
                fingerprint: format!("no-jobs-rig-busy:{name}"),
                detail: format!("{name} takes no jobs but runs heavy work: {commands}"),
                min_ticks: 1,
            });
        }
        for (target, count) in &probe.outbox {
            incidents.push(Incident {
                fingerprint: format!("outbox:{name}->{target}"),
                detail: format!("{count} message(s) on {name} waiting for unreachable {target}"),
                min_ticks: 3,
            });
        }
        for agent in &probe.agents {
            let Some(idle) = agent.idle_minutes else {
                continue;
            };
            let asks_owner = agent
                .last_paragraph
                .as_deref()
                .is_some_and(|paragraph| paragraph.trim_end().ends_with('?'));
            let stalled = agent.status == "idle"
                && idle * 60_000.0 >= interval_ms as f64
                && !asks_owner
                && !agent.wake_scheduled;
            if stalled {
                incidents.push(Incident {
                    fingerprint: format!("stalled:{name}:{}", agent.name),
                    detail: format!(
                        "agent {}@{name} idle {idle:.0} min in {}, nothing scheduled; last said: {}",
                        agent.name,
                        agent.cwd,
                        agent.last_paragraph.as_deref().unwrap_or("(nothing)")
                    ),
                    min_ticks: 1,
                });
            }
        }
    }

    // The same heavy command on two rigs is duplicated work.
    let mut commands: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, probe) in &online {
        for contender in &probe.contenders {
            if contender.name == "rustc" {
                continue;
            }
            if let Some(command) = contender.command.as_deref().map(normalize_command) {
                let rigs_running = commands.entry(command).or_default();
                if !rigs_running.contains(name) {
                    rigs_running.push((*name).clone());
                }
            }
        }
    }
    for (command, running) in commands {
        if running.len() > 1 {
            incidents.push(Incident {
                fingerprint: format!("duplicate:{command}"),
                detail: format!("same command on {}: `{command}`", running.join(" and ")),
                min_ticks: 1,
            });
        }
    }

    // Drift: one repo + branch at different commits on different rigs. One
    // aggregate incident: agents push all day, so per-repo incidents would
    // flood the orchestrator with noise it cannot fix by mail.
    let mut heads: BTreeMap<(String, String), BTreeMap<String, String>> = BTreeMap::new();
    for (name, probe) in &online {
        for repo in &probe.repos {
            heads
                .entry((repo.name.clone(), repo.branch.clone()))
                .or_default()
                .insert((*name).clone(), repo.head.clone());
        }
    }
    let drifted: Vec<String> = heads
        .into_iter()
        .filter(|(_, by_rig)| {
            let mut distinct: Vec<&String> = by_rig.values().collect();
            distinct.sort();
            distinct.dedup();
            distinct.len() > 1
        })
        .map(|((repo, branch), by_rig)| {
            format!(
                "{repo}@{branch} ({})",
                by_rig
                    .iter()
                    .map(|(rig, head)| format!("{rig} {}", short(head)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    if !drifted.is_empty() {
        incidents.push(Incident {
            fingerprint: "drift".into(),
            detail: format!(
                "{} repo checkout(s) differ across rigs (pull/push needed): {}",
                drifted.len(),
                drifted.join("; ")
            ),
            min_ticks: DRIFT_TICKS,
        });
    }

    for event in events {
        let summary = event
            .previous
            .as_ref()
            .map(|previous| {
                format!(
                    "; previous thread ({}) ended with: {}",
                    previous.session, previous.paragraph
                )
            })
            .unwrap_or_default();
        incidents.push(Incident {
            fingerprint: format!(
                "handoff:{}:{}:{}",
                event.rig,
                event.session,
                mailroom_now_ms()
            ),
            detail: format!(
                "new thread {} on {} in {}{summary}",
                event.session, event.rig, event.cwd
            ),
            min_ticks: 1,
        });
    }
    incidents
}

fn mailroom_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn short(head: &str) -> &str {
    head.get(..10).unwrap_or(head)
}

fn summarize_contenders(probe: &RigProbe) -> String {
    let mut names: Vec<String> = probe
        .contenders
        .iter()
        .filter(|contender| contender.name != "rustc")
        .map(|contender| {
            contender
                .command
                .clone()
                .unwrap_or_else(|| contender.name.clone())
        })
        .collect();
    names.dedup();
    match names.is_empty() {
        true => format!("{} compiler worker(s)", probe.contenders.len()),
        false => names.join("; "),
    }
}

fn repo_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(|home| PathBuf::from(home).join("git"))
        .into_iter()
        .chain([PathBuf::from("E:/git")])
        .filter(|root| root.is_dir())
        .collect()
}

/// Unclaimed open work: the first `- [ ]` of each `.plans`/`.issues` file in
/// the tracked repos whose status is not done and that carries no live
/// `**Claimed:**` lease, newest file first.
pub fn backlog(roots: &[PathBuf], now: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    for root in roots {
        let Ok(repos) = fs::read_dir(root) else {
            continue;
        };
        for repo in repos.flatten() {
            let Some(repo_name) = repo.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !(repo_name.starts_with("riir-")
                || repo_name.starts_with("seal-")
                || repo_name == "katgpt-rs")
            {
                continue;
            }
            for folder in [".plans", ".issues"] {
                let Ok(files) = fs::read_dir(repo.path().join(folder)) else {
                    continue;
                };
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().is_none_or(|extension| extension != "md") {
                        continue;
                    }
                    let Ok(text) = fs::read_to_string(&path) else {
                        continue;
                    };
                    let Some(item) = open_item(&text, now) else {
                        continue;
                    };
                    let modified = file
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    let line = format!(
                        "{repo_name}/{folder}/{}: {item}",
                        file.file_name().to_string_lossy()
                    );
                    found.push((modified, truncate(&line, BACKLOG_ITEM_CHARS)));
                }
            }
        }
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found
        .into_iter()
        .take(BACKLOG_ITEMS)
        .map(|(_, line)| line)
        .collect()
}

/// The first open task of a plan/issue that is still open and unclaimed.
pub fn open_item(text: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let status = text
        .lines()
        .take(4)
        .find(|line| line.trim_start().to_ascii_lowercase().starts_with("status"))
        .unwrap_or("")
        .to_ascii_uppercase();
    let finished = [
        "DONE",
        "CLOSED",
        "COMPLETE",
        "SUPERSEDED",
        "OBSOLETE",
        "WONTFIX",
    ];
    if finished.iter().any(|word| {
        status
            .split(|c: char| !c.is_ascii_alphanumeric())
            .take(4)
            .any(|token| token == *word)
    }) {
        return None;
    }
    let lease_closed = text.contains("**Lease closed:**");
    let live_claim = !lease_closed
        && text.lines().any(|line| {
            line.trim_start().starts_with("**Claimed:**")
                && line
                    .split("expires ")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|at| {
                        chrono::DateTime::parse_from_rfc3339(at.trim_end_matches([',', '.'])).ok()
                    })
                    .is_some_and(|expires| expires > now)
        });
    if live_claim {
        return None;
    }
    text.lines()
        .map(str::trim_start)
        .find_map(|line| line.strip_prefix("- [ ]"))
        .map(|task| task.trim().to_string())
        .filter(|task| !task.is_empty())
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// Makes the same command on two machines compare equal: home directories
/// and the toolchain path prefix are dropped.
pub fn normalize_command(command: &str) -> String {
    let mut words: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    if let Some(first) = words.first_mut() {
        let binary = first.rsplit(['/', '\\']).next().unwrap_or(first);
        *first = binary.trim_end_matches(".exe").to_string();
    }
    words
        .into_iter()
        .map(|word| strip_home(&word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_home(word: &str) -> String {
    for prefix in ["/Users/", "/home/", "C:\\Users\\", "C:/Users/"] {
        if let Some(rest) = word.strip_prefix(prefix) {
            let after_user = rest.split_once(['/', '\\']).map_or("", |(_, rest)| rest);
            return format!("~/{}", after_user.replace('\\', "/"));
        }
    }
    word.to_string()
}

/// The only text the LLM orchestrator sees: the incidents plus a one-line
/// state of each rig involved.
pub fn digest(
    incidents: &[Incident],
    results: &[(String, Result<RigProbe>, Duration)],
    self_rig: &str,
) -> String {
    let mut text = format!(
        "You are the rig orchestrator on {self_rig}. Resolve these incidents by mailing the agents \
         involved: one Bash call per mail, exactly `agent-mail send <agent>@<rig> \"<one-line \
         message>\"`, no `&&` chains or heredocs. Advisory only: they decide; never kill \
         processes or edit repos; never start or move jobs onto m3 (agents already running \
         there may still be nudged about their own work); skip agents waiting on the owner. \
         Then end your turn.\n\n## Incidents\n"
    );
    for incident in incidents {
        text.push_str(&format!("- {}\n", incident.detail));
    }
    text.push_str("\n## Rigs\n");
    for (name, outcome, _) in results {
        let line = match outcome {
            Ok(probe) => format!(
                "{name}: {} · {} agent(s) · {}",
                match probe.quiet {
                    true => "QUIET".to_string(),
                    false => format!("BUSY ({})", probe.not_quiet_because.join(", ")),
                },
                probe.agents.len(),
                match probe.contenders.is_empty() {
                    true => "no heavy work".to_string(),
                    false => summarize_contenders(probe),
                }
            ),
            Err(_) => format!("{name}: offline"),
        };
        text.push_str(&format!("- {line}\n"));
    }
    match text.char_indices().nth(DIGEST_MAX_CHARS) {
        Some((end, _)) => format!("{}\n…(truncated)", &text[..end]),
        None => text,
    }
}

// agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn deliver_digest(
    mailroom: &Mailroom,
    config: &WatchConfig,
    self_rig: &str,
    digest: String,
) -> Result<()> {
    match config.orchestrator_command.as_deref() {
        Some([program, args @ ..]) => {
            use std::io::Write as _;
            let log = fs::File::create(mailroom.watch_dir().join("orchestrator.log"))?;
            let mut child = std::process::Command::new(program)
                .args(args)
                .current_dir(mailroom.watch_dir())
                // The orchestrator is itself a Claude session: this tells its
                // agent-mail hooks to stand down, so it never triggers the
                // watchdog that started it.
                .env(ORCHESTRATOR_ENV, "1")
                .envs(&config.orchestrator_env)
                .stdin(std::process::Stdio::piped())
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()
                .with_context(|| format!("starting orchestrator {program}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(digest.as_bytes())?;
            }
            // Not awaited: the orchestrator runs its turn while launchd moves on.
            Ok(())
        }
        _ => {
            let to = Address::parse(&format!("{ORCHESTRATOR_INBOX}@{self_rig}"))?;
            let envelope = mailroom.compose(&to, "watchdog", digest, None)?;
            mailroom.send(&envelope).map(|_| ())
        }
    }
}

/// macOS: install the launchd job that runs the tick every 60 s and whenever
/// a handoff event lands. Absolute paths throughout: launchd runs with a bare
/// environment (a `cargo: command not found` swap failure taught this).
/// `AbandonProcessGroup`: the tick exits right after starting the
/// orchestrator, and launchd would otherwise kill it with the job.
pub fn install_launchd(mailroom: &Mailroom) -> Result<PathBuf> {
    let binary = std::env::current_exe()?;
    let home = std::env::var("HOME").context("HOME unset")?;
    let inbox = mailroom.home().join("inbox").join(WATCH_INBOX).join("new");
    fs::create_dir_all(&inbox)?;
    fs::create_dir_all(mailroom.watch_dir())?;
    let log = mailroom.watch_dir().join("launchd.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>rig</string><string>watch</string><string>--once</string></array>
  <key>StartInterval</key><integer>60</integer>
  <key>WatchPaths</key><array><string>{}</string></array>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>AbandonProcessGroup</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>{home}</string>
    <key>PATH</key><string>{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        binary.display(),
        inbox.display(),
        log.display(),
        log.display()
    );
    let path = PathBuf::from(&home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    fs::create_dir_all(path.parent().context("plist dir")?)?;
    fs::write(&path, plist)?;
    Ok(path)
}

pub const LAUNCHD_LABEL: &str = "dev.agent-mail.watch";
/// Set in the orchestrator's environment; agent-mail hooks ignore sessions carrying it.
pub const ORCHESTRATOR_ENV: &str = "AGENT_MAIL_ORCHESTRATOR";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rig::{Agent, Contender, RepoHead};

    fn probe(rig: &str) -> RigProbe {
        RigProbe {
            rig: rig.into(),
            os: "macos".into(),
            at: String::new(),
            cores: 8,
            load1: None,
            mem_total_gb: None,
            mem_free_gb: None,
            disk_free_gb: None,
            power: None,
            cpu_speed_limit: None,
            gpus: Vec::new(),
            agents: Vec::new(),
            contenders: Vec::new(),
            blind: None,
            quiet: true,
            not_quiet_because: Vec::new(),
            outbox: Vec::new(),
            repos: Vec::new(),
            watch_last_tick: None,
            owner_mail: 0,
            running: Vec::new(),
            queued: Vec::new(),
        }
    }

    fn rigs() -> crate::Rigs {
        serde_json::from_str(
            r#"{"self":"amo","orchestrator":"amo","rigs":{"amo":{"ssh":[]},"bmo":{"ssh":[]},"m3":{"ssh":[],"accepts_jobs":false}}}"#,
        )
        .unwrap()
    }

    fn heavy(command: &str) -> Contender {
        Contender {
            pid: 1,
            name: "cargo".into(),
            cpu_pct: 90.0,
            rss_gb: 1.0,
            arm: "name".into(),
            command: Some(command.into()),
            cwd: None,
        }
    }

    #[test]
    fn last_paragraph_reads_the_final_assistant_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Old answer."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Intro paragraph that is long enough to stand on its own as context for the reader here.\n\n## Summary\nDone: shipped 9df6737acf; next is P1."}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
        ];
        fs::write(&path, lines.join("\n")).unwrap();
        let paragraph = last_paragraph(&path).unwrap();
        assert!(paragraph.ends_with("next is P1."), "{paragraph}");
        // The last paragraph alone is under the minimum, so one more is kept.
        assert!(paragraph.starts_with("Intro paragraph"), "{paragraph}");
    }

    #[test]
    fn paragraphs_are_capped_on_a_char_boundary() {
        let long = "—".repeat(PARAGRAPH_MAX_CHARS + 50);
        let capped = tail_paragraphs(&long);
        assert_eq!(capped.chars().count(), PARAGRAPH_MAX_CHARS + 1);
    }

    #[test]
    fn transcript_path_maps_cwd_like_claude() {
        assert_eq!(
            transcript_path(Path::new("/h/.claude"), "/Users/kat/git/zed", "abc"),
            PathBuf::from("/h/.claude/projects/-Users-kat-git-zed/abc.jsonl")
        );
    }

    #[test]
    fn normalizes_commands_across_homes() {
        assert_eq!(
            normalize_command(
                "/Users/katopz/.rustup/toolchains/x/bin/cargo test -p a /Users/katopz/git/r"
            ),
            normalize_command(
                "C:\\Users\\katop\\.rustup\\toolchains\\x\\bin\\cargo.exe test -p a C:\\Users\\katop\\git\\r"
            )
        );
    }

    #[test]
    fn detects_duplicates_drift_stalls_and_no_job_rigs() {
        let mut amo = probe("amo");
        amo.contenders
            .push(heavy("/Users/katopz/bin/cargo test -p seal-view"));
        amo.repos.push(RepoHead {
            name: "katgpt-rs".into(),
            branch: "develop".into(),
            head: "aaaa".into(),
        });
        amo.agents.push(Agent {
            name: "katgpt-rs-30".into(),
            status: "idle".into(),
            cwd: "/Users/katopz/git/katgpt-rs".into(),
            idle_minutes: Some(25.0),
            last_paragraph: Some("Done; nothing left.".into()),
            wake_scheduled: false,
        });
        amo.agents.push(Agent {
            name: "asks".into(),
            status: "idle".into(),
            cwd: String::new(),
            idle_minutes: Some(25.0),
            last_paragraph: Some("Should I proceed?".into()),
            wake_scheduled: false,
        });
        let mut bmo = probe("bmo");
        bmo.contenders
            .push(heavy("/Users/katopz/bin/cargo test -p seal-view"));
        bmo.repos.push(RepoHead {
            name: "katgpt-rs".into(),
            branch: "develop".into(),
            head: "bbbb".into(),
        });
        let mut m3 = probe("m3");
        m3.contenders.push(heavy("/tmp/flashmemory_decode_gate"));
        let results = vec![
            ("amo".to_string(), Ok(amo), Duration::ZERO),
            ("bmo".to_string(), Ok(bmo), Duration::ZERO),
            ("m3".to_string(), Ok(m3), Duration::ZERO),
        ];
        let incidents = detect(&results, &rigs(), &[], 10 * 60_000);
        let fingerprints: Vec<&str> = incidents
            .iter()
            .map(|incident| incident.fingerprint.as_str())
            .collect();
        assert!(
            fingerprints.contains(&"duplicate:cargo test -p seal-view"),
            "{fingerprints:?}"
        );
        assert!(fingerprints.contains(&"drift"));
        assert!(fingerprints.contains(&"stalled:amo:katgpt-rs-30"));
        assert!(
            !fingerprints.iter().any(|f| f.contains("asks")),
            "agents asking the owner are left alone"
        );
        assert!(fingerprints.contains(&"no-jobs-rig-busy:m3"));
        let drift = incidents
            .iter()
            .find(|incident| incident.fingerprint.starts_with("drift"))
            .unwrap();
        assert_eq!(drift.min_ticks, DRIFT_TICKS);
    }

    #[test]
    fn offline_m3_is_normal_but_offline_runner_is_not() {
        let results = vec![
            (
                "m3".to_string(),
                Err(anyhow::anyhow!("offline")),
                Duration::ZERO,
            ),
            (
                "bmo".to_string(),
                Err(anyhow::anyhow!("offline")),
                Duration::ZERO,
            ),
        ];
        let incidents = detect(&results, &rigs(), &[], 600_000);
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].fingerprint, "offline:bmo");
    }

    #[test]
    fn handoff_carries_the_previous_threads_last_paragraph() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("m3").unwrap();
        let transcript = dir.path().join("old.jsonl");
        fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Summary: P0 shipped in 9df6737acf, next P1 queue; plan 047 in flight."}]}}"#,
        )
        .unwrap();
        room.save_last_paragraph("old-session", "/w/zed", &transcript);
        let previous = room.previous_paragraph("new-session", "/w/zed").unwrap();
        assert_eq!(previous.session, "old-session");
        assert!(previous.paragraph.contains("9df6737acf"));
        assert!(room.previous_paragraph("new-session", "/w/other").is_none());
        assert!(room.previous_paragraph("old-session", "/w/zed").is_none());
    }

    #[test]
    fn backlog_skips_done_and_claimed_items() {
        let now = chrono::Utc::now();
        let future = (now + chrono::Duration::hours(5)).to_rfc3339();
        let past = (now - chrono::Duration::hours(5)).to_rfc3339();
        assert_eq!(
            open_item(
                "# P\nStatus: IN PROGRESS\n- [x] a\n- [-] b\n- [ ] c next\n",
                now
            )
            .as_deref(),
            Some("c next")
        );
        assert_eq!(
            open_item("# P\nStatus: DONE — 9df\n- [ ] stale\n", now),
            None
        );
        assert_eq!(
            open_item(
                &format!("# P\nStatus: open\n**Claimed:** s, 1, expires {future}\n- [ ] x\n"),
                now
            ),
            None,
            "a live lease hides the item"
        );
        assert_eq!(
            open_item(
                &format!("# P\nStatus: open\n**Claimed:** s, 1, expires {past}\n- [ ] x\n"),
                now
            )
            .as_deref(),
            Some("x"),
            "an expired lease does not"
        );

        let dir = tempfile::tempdir().unwrap();
        let plans = dir.path().join("riir-ai/.plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(
            plans.join("001_a.md"),
            "# A\nStatus: open\n- [ ] do the thing\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("other/.plans")).unwrap();
        fs::write(
            dir.path().join("other/.plans/001.md"),
            "Status: open\n- [ ] untracked repo\n",
        )
        .unwrap();
        let items = backlog(&[dir.path().to_path_buf()], now);
        assert_eq!(
            items,
            vec!["riir-ai/.plans/001_a.md: do the thing".to_string()]
        );
    }

    #[test]
    fn tick_is_free_until_the_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("amo").unwrap();
        room.set_watch_interval(10).unwrap();
        let state = WatchState {
            last_tick_ms: chrono::Utc::now().timestamp_millis(),
            ..WatchState::default()
        };
        fs::create_dir_all(dir.path().join("watch")).unwrap();
        fs::write(
            dir.path().join("watch/state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        let report = tick(&room, false).unwrap();
        assert!(
            !report.ran,
            "no event and interval not elapsed: no probe, no tokens"
        );
    }
}
