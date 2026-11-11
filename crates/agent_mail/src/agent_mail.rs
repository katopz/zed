//! Cross-machine mail between coding agents (Claude Code sessions, Zed agents).
//!
//! Each rig keeps a maildir under `~/.agent-mail/inbox/<name>/{tmp,new,cur}`.
//! Sending to another rig runs `<bin> deliver <base64url envelope>` there over
//! SSH (Tailscale/LAN), so no daemon or open port is needed: the trust boundary
//! is the SSH key auth the rigs already have. Receiving is a cheap directory
//! scan, done by Claude Code hooks or by `agent-mail recv`.

pub mod boss;
pub mod jobs;
pub mod rig;
#[cfg(feature = "tui")]
pub mod tui;
pub mod watch;
pub mod zed_wake;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// Inbox shared by every agent on a rig: `*@rig` goes here, first claimer wins.
pub const ANY_NAME: &str = "_any";
/// Broadcast inbox: `all@rig` reaches every agent on the rig. Never claimed;
/// each reader keeps its own cursor, so each agent sees each broadcast once.
pub const ALL_NAME: &str = "_all";
/// A reader with no cursor yet (a new session) still sees broadcasts this recent.
const BROADCAST_LOOKBACK: chrono::Duration = chrono::Duration::minutes(10);
pub const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_NAME_LEN: usize = 64;
const CLAIMED_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const WAIT_POLL: Duration = Duration::from_millis(500);
// Long enough to sleep through a multi-day scheduled wake; the waiter also
// exits as soon as its session dies.
const WAIT_MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const SSH_TIMEOUT: Duration = Duration::from_secs(20);
const OUTBOX_FIRST_RETRY: Duration = Duration::from_secs(30);
const OUTBOX_MAX_RETRY: Duration = Duration::from_secs(15 * 60);
const OUTBOX_FLUSH_EVERY: Duration = Duration::from_secs(60);
/// A `sending/` claim older than this belongs to a flusher that died mid-send.
const STALE_SEND: Duration = Duration::from_secs(5 * 60);
/// A liveness entry older than this is stale (the native poller refreshes its
/// entries well inside this window; a dead Zed goes stale and `*@rig`
/// resolution falls back to the shared `_any` inbox).
const LIVE_FRESH: Duration = Duration::from_secs(15);
/// How often a poller should refresh its liveness entries.
pub const LIVE_REFRESH: Duration = Duration::from_secs(5);

/// A `Command` that never opens a console window. Zed (a GUI app) calls into
/// this crate for rig probes, and on Windows every console child of a GUI
/// process (ssh, tasklist, git, ...) otherwise flashes its own window.
// agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
pub(crate) fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// How a recipient's poller should deliver mail that arrives mid-turn
/// (agent-mail issue 046): `urgent` preempts the running turn, `steering` ends
/// it at the next message boundary, `queued` waits for the turn to finish.
/// `fyi` additionally never wakes an idle recipient: it is parked and read at
/// the recipient's next natural turn end, so broadcasts stay cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MailMode {
    Urgent,
    Steering,
    Fyi,
    /// Also the fallback for mode values an older binary doesn't know (a
    /// newer sender's enum variant): boundary delivery instead of a dropped
    /// envelope while rigs run mixed versions.
    #[default]
    #[serde(other)]
    Queued,
}

impl MailMode {
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "urgent" => Ok(Self::Urgent),
            "steering" => Ok(Self::Steering),
            "fyi" => Ok(Self::Fyi),
            "queued" => Ok(Self::Queued),
            other => bail!("unknown mode {other:?}; expected urgent, steering, fyi or queued"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::Steering => "steering",
            Self::Fyi => "fyi",
            Self::Queued => "queued",
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::Queued
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub from: String,
    pub to: String,
    pub sent_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// RFC 3339; set for scheduled mail, which stays out of the inbox until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_at: Option<String>,
    /// How the recipient delivers mid-turn mail (issue 046). Queued is the
    /// default and is left off the wire, so old payloads decode unchanged.
    #[serde(default, skip_serializing_if = "MailMode::is_default")]
    pub mode: MailMode,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub name: String,
    pub rig: Option<String>,
}

impl Address {
    /// `name@rig`, `name` (this rig), `*@rig` or `@rig` (any agent on that rig).
    pub fn parse(text: &str) -> Result<Self> {
        let (name, rig) = match text.rsplit_once('@') {
            Some((name, rig)) => (name, Some(rig)),
            None => (text, None),
        };
        let name = match name {
            "" | "*" => ANY_NAME.to_string(),
            "all" => ALL_NAME.to_string(),
            name => validate_name(name)?.to_string(),
        };
        let rig = rig.map(validate_name).transpose()?.map(str::to_string);
        Ok(Self { name, rig })
    }

    pub fn display_with(&self, self_rig: &str) -> String {
        let name = match self.name.as_str() {
            ANY_NAME => "*",
            ALL_NAME => "all",
            name => name,
        };
        format!("{name}@{}", self.rig.as_deref().unwrap_or(self_rig))
    }

    /// Whether this address broadcasts to every agent on a rig (`all@rig`).
    pub fn is_broadcast(&self) -> bool {
        self.name == ALL_NAME
    }
}

/// `all@rig` is an N-agent blast: every live agent claims it and starts a
/// turn, so a fleet of five pays five turns for one FYI and another N turns
/// replying. Wake-capable broadcasts must say so explicitly; the default
/// (queued) blast is refused so nobody burns a fleet by accident — fyi covers
/// "when you're free, everyone read this" without waking anyone.
pub fn ensure_broadcast_mode(address: &Address, mode: MailMode) -> Result<()> {
    if !address.is_broadcast() || mode != MailMode::Queued {
        return Ok(());
    }
    bail!(
        "refusing all@ broadcast with mode queued: every live agent on the rig \
         would wake and start a turn. Re-send with `--mode fyi` (no wake: each \
         agent reads it at its next turn boundary — right for FYIs), or \
         `--mode urgent`/`--mode steering` (only for critical fleet-wide \
         incidents), or target one agent (`*@rig` takes a single free agent)"
    )
}

/// One way to reach a rig. A plain string is a direct SSH destination;
/// `{"target": ..., "jump": ...}` dials `target` through a jump host
/// (`ssh -o ProxyJump=<jump>`), for rigs reachable only via a neighbor
/// (bmo's thunderbolt bridge through amo). Routes are tried in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SshRoute {
    Direct(String),
    Jumped { target: String, jump: String },
}

impl SshRoute {
    pub fn target(&self) -> &str {
        match self {
            SshRoute::Direct(target) | SshRoute::Jumped { target, .. } => target,
        }
    }

    pub fn jump(&self) -> Option<&str> {
        match self {
            SshRoute::Direct(_) => None,
            SshRoute::Jumped { jump, .. } => Some(jump),
        }
    }

    /// Short label for humans: `host` or `host via <rig>`, user@ stripped and
    /// the jump host replaced by the rig name that owns it when known.
    pub fn label(&self, rigs: &Rigs) -> String {
        let host = |target: &str| {
            target
                .rsplit_once('@')
                .map(|(_, host)| host)
                .unwrap_or(target)
                .to_string()
        };
        match self {
            SshRoute::Direct(target) => host(target),
            SshRoute::Jumped { target, .. } => {
                format!(
                    "{} via {}",
                    host(target),
                    self.via_label(rigs).unwrap_or_default()
                )
            }
        }
    }

    /// The jump this route rides, resolved to the rig that owns the jump host
    /// (`amo`) when one does; `None` for direct routes.
    pub fn via_label(&self, rigs: &Rigs) -> Option<String> {
        let jump = self.jump()?;
        let owner = rigs
            .rigs
            .iter()
            .find(|(_, rig)| {
                rig.ssh
                    .iter()
                    .any(|route| route.target() == jump || route.to_string() == *jump)
            })
            .map(|(name, _)| name.as_str());
        Some(owner.unwrap_or(jump).to_string())
    }
}

impl std::fmt::Display for SshRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshRoute::Direct(target) => f.write_str(target),
            SshRoute::Jumped { target, jump } => write!(f, "{target} via {jump}"),
        }
    }
}

impl AsRef<str> for SshRoute {
    fn as_ref(&self) -> &str {
        self.target()
    }
}

impl AsRef<OsStr> for SshRoute {
    fn as_ref(&self) -> &OsStr {
        self.target().as_ref()
    }
}

/// Names become directory names, so this is the path-traversal guard.
pub fn validate_name(name: &str) -> Result<&str> {
    let well_formed = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    match well_formed {
        true => Ok(name),
        false => bail!(
            "invalid name {name:?}: use 1-{MAX_NAME_LEN} of [A-Za-z0-9._-], not starting with '.' or '-'"
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rig {
    /// SSH routes, tried in order (LAN before Tailscale before jump-hosted).
    /// A plain string dials directly; `{"target", "jump"}` goes through a
    /// jump host (see [`SshRoute`]).
    pub ssh: Vec<SshRoute>,
    /// `agent-mail` path on that rig; non-interactive SSH often lacks ~/.cargo/bin on PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// What the rig is for (shown by `rig status`), e.g. `builder`, `runner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `false` keeps every job off this rig (an orchestrator laptop).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub accepts_jobs: bool,
    /// What jobs may require of it (`cuda`, `metal`, `zed`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
}

impl Default for Rig {
    fn default() -> Self {
        Self {
            ssh: Vec::new(),
            bin: None,
            role: None,
            accepts_jobs: true,
            caps: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rigs {
    #[serde(rename = "self")]
    pub self_rig: String,
    #[serde(default)]
    pub rigs: BTreeMap<String, Rig>,
    /// The rig whose watchdog orchestrates the others (plan 048).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestrator: Option<String>,
}

#[derive(Debug)]
pub struct SendReport {
    pub id: String,
    pub to: String,
    pub via: Option<String>,
    /// Set when the rig was unreachable: the message waits in the outbox and
    /// is retried by [`Mailroom::flush_outbox`]; this is why it could not go now.
    pub queued: Option<String>,
    /// For `*@rig` sends: the agent whose inbox took the mail (deliver-time
    /// resolution, issue 046). `None` when the mail went to the shared `_any`
    /// inbox to be claimed, or the target named an agent directly.
    pub resolved_to: Option<String>,
    pub elapsed: Duration,
}

/// One liveness-registry entry (issue 046): a poller's statement that
/// `name` is a live agent thread right now, with how it is behaving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveAgent {
    pub name: String,
    /// `"idle"` or `"generating"`.
    pub status: String,
    /// `"thread"` (top-level) or `"subagent"`.
    pub kind: String,
    /// RFC 3339 write time; entries older than [`LIVE_FRESH`] are stale.
    pub updated_at: String,
}

fn live_rank(agent: &LiveAgent) -> u8 {
    match (agent.status.as_str(), agent.kind.as_str()) {
        ("idle", "thread") => 0,
        ("idle", _) => 1,
        (_, "thread") => 2,
        _ => 3,
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn resolved_from_lines(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| {
        line.strip_prefix("resolved-to ")
            .map(str::to_string)
            .filter(|name| validate_name(name).is_ok())
    })
}

#[derive(Debug, Default)]
pub struct FlushReport {
    pub delivered: usize,
    pub pending: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxCount {
    pub name: String,
    pub pending: usize,
    pub scheduled: usize,
    /// The pending envelopes themselves (newest first), so read-only viewers
    /// (the Zed panel) can show an inbox's mail without claiming it.
    pub messages: Vec<Envelope>,
}

/// Read-only view of a rig's mail for the terminal UI and the Zed panel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MailboxSummary {
    pub inboxes: Vec<InboxCount>,
    /// Newest pending (unclaimed) messages across inboxes.
    pub pending: Vec<Envelope>,
    /// Newest broadcasts (`all@rig`).
    pub broadcasts: Vec<Envelope>,
    pub outbox: Vec<(String, usize)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HookOutcome {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: i32,
}

#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
    hook_event_name: Option<String>,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

pub struct Mailroom {
    home: PathBuf,
}

impl Mailroom {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var_os("AGENT_MAIL_HOME") {
            Some(home) => Ok(Self::new(PathBuf::from(home))),
            None => Ok(Self::new(home_dir()?.join(".agent-mail"))),
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn inbox(&self, name: &str) -> PathBuf {
        self.home.join("inbox").join(name)
    }

    fn rigs_path(&self) -> PathBuf {
        self.home.join("rigs.json")
    }

    pub fn rigs(&self) -> Result<Rigs> {
        let path = self.rigs_path();
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {} (run `agent-mail init <this-rig-name>` first)",
                path.display()
            )
        })?;
        let mut rigs: Rigs =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if let Ok(rig) = std::env::var("AGENT_MAIL_RIG") {
            rigs.self_rig = rig;
        }
        validate_name(&rigs.self_rig)?;
        Ok(rigs)
    }

    pub fn init(&self, self_rig: &str) -> Result<PathBuf> {
        validate_name(self_rig)?;
        let path = self.rigs_path();
        let mut rigs = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == ErrorKind::NotFound => Rigs::default(),
            Err(error) => return Err(error).context(format!("reading {}", path.display())),
        };
        rigs.self_rig = self_rig.to_string();
        fs::create_dir_all(&self.home)?;
        fs::write(&path, serde_json::to_string_pretty(&rigs)? + "\n")?;
        Ok(path)
    }

    fn live_dir(&self) -> PathBuf {
        self.home.join("live")
    }

    /// Records that `name` is a live agent thread right now (issue 046
    /// `*@rig` resolution). Pollers call this every [`LIVE_REFRESH`]; a dead
    /// poller's entries age past [`LIVE_FRESH`] and stop resolving.
    pub fn write_live_entry(&self, name: &str, status: &str, kind: &str) {
        let Ok(name) = validate_name(name) else {
            return;
        };
        let entry = LiveAgent {
            name: name.to_string(),
            status: status.to_string(),
            kind: kind.to_string(),
            updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        if fs::create_dir_all(self.live_dir()).is_ok()
            && let Ok(json) = serde_json::to_vec(&entry)
        {
            let _ = fs::write(self.live_dir().join(format!("{name}.json")), json);
        }
    }

    /// Removes liveness entries stale past [`LIVE_FRESH`] (their poller died).
    pub fn prune_stale_live_entries(&self) {
        let Ok(entries) = fs::read_dir(self.live_dir()) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > LIVE_FRESH)
                || entry.metadata().is_err();
            if stale {
                fs::remove_file(entry.path()).ok();
            }
        }
    }

    /// Fresh liveness entries, best `*@rig` candidate first: idle top-level
    /// threads, idle subagents, busy top-level threads, busy subagents;
    /// newest write first within a class.
    pub fn live_agents(&self) -> Vec<LiveAgent> {
        let Ok(entries) = fs::read_dir(self.live_dir()) else {
            return Vec::new();
        };
        let mut agents: Vec<LiveAgent> = entries
            .flatten()
            .filter_map(|entry| parse_json::<LiveAgent>(&entry.path()))
            .filter(|agent| {
                validate_name(&agent.name).is_ok()
                    && chrono::DateTime::parse_from_rfc3339(&agent.updated_at)
                        .map(|written| {
                            chrono::Utc::now()
                                .signed_duration_since(written)
                                .num_milliseconds() as u128
                                <= LIVE_FRESH.as_millis()
                        })
                        .unwrap_or(false)
            })
            .collect();
        agents.sort_by(|left, right| {
            live_rank(left)
                .cmp(&live_rank(right))
                .then(right.updated_at.cmp(&left.updated_at))
        });
        agents
    }

    /// Atomic maildir delivery: readers only ever see complete files in `new/`.
    /// Scheduled mail waits in `later/<due unix ms>_<id>.json` instead; every
    /// inbox scan promotes what is due, so whatever already polls the inbox
    /// (the Claude waiter hook, Zed's poller) delivers it on time without a
    /// scheduler process.
    /// Files a `*@rig` envelope into the best live inbox when the liveness
    /// registry has a fresh entry, else into the shared `_any` inbox to be
    /// claimed (today's behavior). Returns the resolved inbox name when
    /// `*@rig` mail landed somewhere specific (issue 046 receipts).
    pub fn deliver(&self, envelope: &Envelope) -> Result<Option<String>> {
        let address = Address::parse(&envelope.to)?;
        validate_name(&envelope.id)?;
        let due = envelope
            .deliver_at
            .as_deref()
            .map(|at| {
                chrono::DateTime::parse_from_rfc3339(at)
                    .with_context(|| format!("invalid deliver_at {at:?}"))
            })
            .transpose()?
            .filter(|due| *due > chrono::Utc::now());
        let inbox_name = match address.name.as_str() {
            ANY_NAME => self
                .live_agents()
                .first()
                // Registry names are re-validated: they become directory names.
                .map(|agent| validate_name(&agent.name).map(|name| name.to_string()))
                .transpose()?
                .unwrap_or_else(|| ANY_NAME.to_string()),
            name => name.to_string(),
        };
        let resolved = (inbox_name != address.name).then_some(inbox_name.clone());
        // `*@rig` may resolve to a different inbox on each attempt, so the
        // at-least-once outbox retry dedups across every inbox, not just the
        // resolved one.
        if inbox_name != address.name
            && self.envelope_held_anywhere(&format!("{}.json", envelope.id))
        {
            return Ok(resolved);
        }
        let inbox = self.inbox(&inbox_name);
        for sub in ["tmp", "new", "cur", "later"] {
            fs::create_dir_all(inbox.join(sub))?;
        }
        let file_name = format!("{}.json", envelope.id);
        // Outbox retries are at-least-once (a send can land just before its
        // connection drops), so a redelivered id is dropped here.
        let already_held = inbox.join("new").join(&file_name).exists()
            || inbox.join("cur").join(&file_name).exists()
            || fs::read_dir(inbox.join("later")).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.ends_with(&format!("_{file_name}")))
                })
            });
        if already_held {
            return Ok(resolved);
        }
        let staged = inbox.join("tmp").join(&file_name);
        fs::write(&staged, serde_json::to_vec(envelope)?)?;
        let target = match due {
            Some(due) => inbox
                .join("later")
                .join(format!("{}_{file_name}", due.timestamp_millis())),
            None => inbox.join("new").join(&file_name),
        };
        fs::rename(&staged, target)?;
        prune_claimed(&inbox.join("cur"));
        if address.name == ALL_NAME {
            // Broadcasts stay in new/ (never claimed), so age them out here.
            prune_claimed(&inbox.join("new"));
        }
        Ok(resolved)
    }

    /// Whether any inbox on this rig already holds `file_name` (new/, cur/
    /// or a scheduled later/ copy).
    fn envelope_held_anywhere(&self, file_name: &str) -> bool {
        let Ok(inboxes) = fs::read_dir(self.home.join("inbox")) else {
            return false;
        };
        inboxes.flatten().any(|inbox| {
            let dir = inbox.path();
            ["new", "cur"]
                .into_iter()
                .chain(std::iter::once("later"))
                .any(|sub| {
                    fs::read_dir(dir.join(sub)).is_ok_and(|entries| {
                        entries.flatten().any(|entry| {
                            entry.file_name().to_str().is_some_and(|name| {
                                name == file_name || name.ends_with(&format!("_{file_name}"))
                            })
                        })
                    })
                })
        })
    }

    /// What is waiting in this rig's mailboxes, read-only: viewers (the
    /// terminal UI, the Zed panel) must never claim mail meant for agents.
    pub fn mailbox_summary(&self, recent: usize) -> MailboxSummary {
        let mut inboxes = Vec::new();
        let mut latest = Vec::new();
        if let Ok(entries) = fs::read_dir(self.home.join("inbox")) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name == ALL_NAME {
                    continue;
                }
                let names = [name.clone()];
                let pending = self.peek(&names).unwrap_or_default();
                let scheduled = self.scheduled(&names).map_or(0, |mail| mail.len());
                if !pending.is_empty() || scheduled > 0 {
                    let mut messages = pending.clone();
                    messages.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
                    inboxes.push(InboxCount {
                        name: name.clone(),
                        pending: pending.len(),
                        scheduled,
                        messages,
                    });
                }
                latest.extend(pending);
            }
        }
        inboxes.sort_by(|a, b| a.name.cmp(&b.name));
        let mut broadcasts: Vec<Envelope> = self
            .pending_files(&[ALL_NAME.to_string()])
            .iter()
            .filter_map(|path| read_envelope(path).ok())
            .collect();
        broadcasts.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
        broadcasts.truncate(recent);
        latest.sort_by(|a, b| b.sent_at.cmp(&a.sent_at));
        latest.truncate(recent);
        MailboxSummary {
            inboxes,
            pending: latest,
            broadcasts,
            outbox: self
                .outbox_status()
                .into_iter()
                .map(|(rig, count, _)| (rig, count))
                .collect(),
        }
    }

    /// Broadcasts `reader` has not seen yet, oldest first, plus the newest
    /// `sent_at` among them (the cursor to store once they are delivered).
    fn unseen_broadcasts(&self, reader: &str) -> (Vec<Envelope>, Option<String>) {
        let inbox = self.inbox(ALL_NAME);
        let cursor = fs::read_to_string(inbox.join("seen").join(reader))
            .ok()
            .map(|cursor| cursor.trim().to_string())
            .unwrap_or_else(|| {
                (chrono::Utc::now() - BROADCAST_LOOKBACK)
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            });
        let mut unseen: Vec<Envelope> = self
            .pending_files(&[ALL_NAME.to_string()])
            .iter()
            .filter_map(|path| read_envelope(path).ok())
            .filter(|envelope| envelope.sent_at > cursor)
            .collect();
        unseen.sort_by(|a, b| a.sent_at.cmp(&b.sent_at));
        let newest = unseen.last().map(|envelope| envelope.sent_at.clone());
        let own = format!("{reader}@");
        unseen.retain(|envelope| !envelope.from.starts_with(&own));
        (unseen, newest)
    }

    pub fn peek_broadcasts(&self, reader: &str) -> Vec<Envelope> {
        match validate_name(reader) {
            Ok(reader) => self.unseen_broadcasts(reader).0,
            Err(_) => Vec::new(),
        }
    }

    pub fn has_broadcast(&self, reader: &str) -> bool {
        !self.peek_broadcasts(reader).is_empty()
    }

    /// Returns `reader`'s unseen broadcasts and advances its cursor past them.
    pub fn take_broadcasts(&self, reader: &str) -> Vec<Envelope> {
        let Ok(reader) = validate_name(reader) else {
            return Vec::new();
        };
        let (unseen, newest) = self.unseen_broadcasts(reader);
        if let Some(newest) = newest {
            let seen = self.inbox(ALL_NAME).join("seen");
            if fs::create_dir_all(&seen).is_ok() {
                fs::write(seen.join(reader), newest).ok();
            }
        }
        unseen
    }

    /// Like [`Self::take_broadcasts`], but only envelopes sent at or after
    /// `cutoff` are returned. The cursor still advances past every unseen
    /// broadcast, so skipped ones are permanently marked seen for this reader:
    /// a thread that was not live when a blast landed must not have it wake
    /// in its lap later (Zed resumes a clicked stopped thread with its old
    /// inbox name, and without this gate the resumed thread would claim and
    /// process blasts it was never part of).
    pub fn take_broadcasts_since(
        &self,
        reader: &str,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Vec<Envelope> {
        let Ok(reader) = validate_name(reader) else {
            return Vec::new();
        };
        let (unseen, newest) = self.unseen_broadcasts(reader);
        if let Some(newest) = newest {
            let seen = self.inbox(ALL_NAME).join("seen");
            if fs::create_dir_all(&seen).is_ok() {
                fs::write(seen.join(reader), newest).ok();
            }
        }
        unseen
            .into_iter()
            .filter(|envelope| {
                // `sent_at` is the sender rig's clock, `cutoff` ours: a
                // cross-clock comparison that trusts NTP. Unparseable stamps
                // are delivered rather than dropped.
                chrono::DateTime::parse_from_rfc3339(&envelope.sent_at)
                    .map(|sent_at| sent_at.with_timezone(&chrono::Utc) >= cutoff)
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Moves scheduled mail whose time has come from `later/` into `new/`.
    fn promote_due(&self, names: &[String]) {
        let now = chrono::Utc::now().timestamp_millis();
        for name in names {
            let inbox = self.inbox(name);
            let Ok(entries) = fs::read_dir(inbox.join("later")) else {
                continue;
            };
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let Some((due, rest)) = file_name.to_str().and_then(|name| name.split_once('_'))
                else {
                    continue;
                };
                if due.parse::<i64>().is_ok_and(|due| due <= now) {
                    // A concurrent poller may win this rename; either way it lands once.
                    fs::rename(entry.path(), inbox.join("new").join(rest)).ok();
                }
            }
        }
    }

    /// Scheduled mail not yet due, soonest first.
    pub fn scheduled(&self, names: &[String]) -> Result<Vec<Envelope>> {
        let mut files: Vec<PathBuf> = names
            .iter()
            .filter_map(|name| fs::read_dir(self.inbox(name).join("later")).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        files.iter().map(|path| read_envelope(path)).collect()
    }

    fn pending_files(&self, names: &[String]) -> Vec<PathBuf> {
        self.promote_due(names);
        let mut files: Vec<PathBuf> = names
            .iter()
            .filter_map(|name| fs::read_dir(self.inbox(name).join("new")).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension.as_encoded_bytes() == b"json")
            })
            .collect();
        // UUIDv7 file names sort by send time.
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        files
    }

    pub fn has_mail(&self, names: &[String]) -> bool {
        !self.pending_files(names).is_empty()
    }

    /// Inbox names that currently hold at least one pending (unclaimed, due)
    /// message, sorted by name. The broadcast inbox (`_all`) is never
    /// included: broadcasts are cursor-based, not claim-based.
    pub fn pending_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = fs::read_dir(self.home.join("inbox")) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name == ALL_NAME {
                    continue;
                }
                if !self.pending_files(std::slice::from_ref(&name)).is_empty() {
                    names.push(name);
                }
            }
        }
        names.sort();
        names
    }

    pub fn peek(&self, names: &[String]) -> Result<Vec<Envelope>> {
        self.pending_files(names)
            .iter()
            .map(|path| read_envelope(path))
            .collect()
    }

    /// Moves each message `new/` → `cur/`; the rename is the lock, so concurrent
    /// claimers (two hooks, a hook and `recv`) never both receive one message.
    pub fn claim(&self, names: &[String]) -> Result<Vec<Envelope>> {
        let mut claimed = Vec::new();
        for path in self.pending_files(names) {
            let (Some(new_dir), Some(file_name)) = (path.parent(), path.file_name()) else {
                continue;
            };
            let Some(inbox) = new_dir.parent() else {
                continue;
            };
            let target = inbox.join("cur").join(file_name);
            match fs::rename(&path, &target) {
                Ok(()) => claimed.push(read_envelope(&target)?),
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context(format!("claiming {}", path.display())),
            }
        }
        Ok(claimed)
    }

    pub fn compose(
        &self,
        to: &Address,
        from: &str,
        body: String,
        reply_to: Option<String>,
    ) -> Result<Envelope> {
        if body.trim().is_empty() {
            bail!("empty message body");
        }
        if body.len() > MAX_BODY_BYTES {
            bail!(
                "message body is {} bytes; the limit is {MAX_BODY_BYTES} (send a file path instead)",
                body.len()
            );
        }
        let rigs = self.rigs()?;
        let from = match from.contains('@') {
            true => from.to_string(),
            false => format!("{}@{}", validate_name(from)?, rigs.self_rig),
        };
        Ok(Envelope {
            id: uuid::Uuid::now_v7().to_string(),
            from,
            to: to.display_with(&rigs.self_rig),
            sent_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            reply_to,
            deliver_at: None,
            mode: MailMode::Queued,
            body,
        })
    }

    pub fn send(&self, envelope: &Envelope) -> Result<SendReport> {
        let started = Instant::now();
        let rigs = self.rigs()?;
        let address = Address::parse(&envelope.to)?;
        let rig_name = address.rig.as_deref().unwrap_or(&rigs.self_rig);
        let report = |via, queued, resolved_to| SendReport {
            id: envelope.id.clone(),
            to: envelope.to.clone(),
            via,
            queued,
            resolved_to,
            elapsed: started.elapsed(),
        };
        if rig_name == rigs.self_rig {
            let resolved_to = self.deliver(envelope)?;
            return Ok(report(None, None, resolved_to));
        }
        if !rigs.rigs.contains_key(rig_name) {
            bail!(
                "unknown rig {rig_name:?}; known: {} (edit {})",
                rigs.rigs.keys().cloned().collect::<Vec<_>>().join(", "),
                self.rigs_path().display()
            );
        }
        match self.send_remote(&rigs, rig_name, envelope) {
            Ok((target, resolved_to)) => Ok(report(Some(target), None, resolved_to)),
            Err(error) => {
                let reason = format!("{error:#}");
                self.park_in_outbox(rig_name, envelope, 0)?;
                Ok(report(None, Some(reason), None))
            }
        }
    }

    fn send_remote(
        &self,
        rigs: &Rigs,
        rig_name: &str,
        envelope: &Envelope,
    ) -> Result<(String, Option<String>)> {
        let rig = rigs.rigs.get(rig_name).with_context(|| {
            format!(
                "unknown rig {rig_name:?}; known: {} (edit {})",
                rigs.rigs.keys().cloned().collect::<Vec<_>>().join(", "),
                self.rigs_path().display()
            )
        })?;
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(envelope)?);
        let remote_command = format!(
            "{} deliver {payload}",
            rig.bin.as_deref().unwrap_or("agent-mail")
        );
        let mut failures = Vec::new();
        for route in &rig.ssh {
            match self.ssh(
                route,
                &remote_command,
                &format!("delivered {}", envelope.id),
            ) {
                Ok(lines) => {
                    return Ok((route.to_string(), resolved_from_lines(&lines)));
                }
                Err(error) => failures.push(format!("{route}: {error:#}")),
            }
        }
        Err(anyhow!(
            "could not reach rig {rig_name:?}:\n  {}",
            failures.join("\n  ")
        ))
    }

    fn outbox(&self) -> PathBuf {
        self.home.join("outbox")
    }

    /// Parks `envelope` for `rig` as `outbox/<rig>/<next try ms>_<attempts>_<id>.json`.
    fn park_in_outbox(&self, rig: &str, envelope: &Envelope, attempts: u32) -> Result<()> {
        let dir = self.outbox().join(validate_name(rig)?);
        fs::create_dir_all(&dir)?;
        let backoff = OUTBOX_FIRST_RETRY
            .saturating_mul(1 << attempts.min(10))
            .min(OUTBOX_MAX_RETRY);
        let next = chrono::Utc::now().timestamp_millis()
            + i64::try_from(backoff.as_millis()).unwrap_or(i64::MAX);
        let staged = dir.join(format!(".{}.tmp", envelope.id));
        fs::write(&staged, serde_json::to_vec(envelope)?)?;
        fs::rename(
            &staged,
            dir.join(format!("{next}_{attempts}_{}.json", envelope.id)),
        )?;
        Ok(())
    }

    /// Messages still waiting for their rig: `(rig, count, oldest sent_at)`.
    pub fn outbox_status(&self) -> Vec<(String, usize, Option<String>)> {
        let Ok(rigs) = fs::read_dir(self.outbox()) else {
            return Vec::new();
        };
        rigs.flatten()
            .filter_map(|rig| {
                let name = rig.file_name().to_str()?.to_string();
                let files = outbox_files(&rig.path());
                let oldest = files
                    .iter()
                    .filter_map(|(_, _, path)| read_envelope(path).ok())
                    .map(|envelope| envelope.sent_at)
                    .min();
                (!files.is_empty()).then_some((name, files.len(), oldest))
            })
            .collect()
    }

    /// Retries parked mail whose backoff has elapsed. One failure per rig
    /// postpones the rest of that rig's mail without trying it, so an offline
    /// rig costs one connect timeout per flush, not one per message. The
    /// rename into `sending/` is the claim: concurrent flushers never both
    /// send a message, and `deliver` drops an id it already holds.
    pub fn flush_outbox(&self) -> FlushReport {
        let mut report = FlushReport::default();
        let Ok(rig_dirs) = fs::read_dir(self.outbox()) else {
            return report;
        };
        let Ok(rigs) = self.rigs() else {
            return report;
        };
        let now = chrono::Utc::now().timestamp_millis();
        for rig_dir in rig_dirs.flatten() {
            let Some(rig) = rig_dir.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let sending = rig_dir.path().join("sending");
            recover_stale_sends(&sending, &rig_dir.path());
            let mut rig_down = None;
            for (due, attempts, path) in outbox_files(&rig_dir.path()) {
                if due > now {
                    report.pending += 1;
                    continue;
                }
                let Some(file_name) = path.file_name() else {
                    continue;
                };
                if fs::create_dir_all(&sending).is_err() {
                    continue;
                }
                let claimed = sending.join(file_name);
                if fs::rename(&path, &claimed).is_err() {
                    continue;
                }
                let Ok(envelope) = read_envelope(&claimed) else {
                    fs::remove_file(&claimed).ok();
                    continue;
                };
                let outcome = match &rig_down {
                    Some(reason) => Err(anyhow!("{reason}")),
                    None => self.send_remote(&rigs, &rig, &envelope),
                };
                match outcome {
                    Ok(_) => {
                        fs::remove_file(&claimed).ok();
                        report.delivered += 1;
                    }
                    Err(error) => {
                        rig_down.get_or_insert_with(|| format!("{error:#}"));
                        if self.park_in_outbox(&rig, &envelope, attempts + 1).is_ok() {
                            fs::remove_file(&claimed).ok();
                        }
                        report.pending += 1;
                    }
                }
            }
        }
        report
    }

    /// Pings every rig by running `<bin> version` there.
    pub fn ping_rigs(&self) -> Result<Vec<(String, String, Result<Duration>)>> {
        let rigs = self.rigs()?;
        let mut results = Vec::new();
        for (name, rig) in &rigs.rigs {
            if *name == rigs.self_rig {
                continue;
            }
            let command = format!("{} version", rig.bin.as_deref().unwrap_or("agent-mail"));
            for route in &rig.ssh {
                let started = Instant::now();
                let outcome = self
                    .ssh(route, &command, "agent-mail ")
                    .map(|_| started.elapsed());
                results.push((name.clone(), route.to_string(), outcome));
            }
        }
        Ok(results)
    }

    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    /// Runs `remote_command` on `route` and succeeds once a stdout line starts
    /// with `marker`. Success is the marker, not the exit status: Windows'
    /// OpenSSH client run inside an sshd session (an agent in a Zed remote
    /// window on the 4090) delivers but never exits, so it is killed instead.
    /// Returns the stdout lines printed before the marker line.
    pub(crate) fn ssh(
        &self,
        route: &SshRoute,
        remote_command: &str,
        marker: &str,
    ) -> Result<Vec<String>> {
        let mut command = command("ssh");
        // accept-new only trusts a rig's first-seen host key (Tailscale already
        // authenticates the node); a changed key is still rejected.
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(jump) = route.jump() {
            command.args(["-o", &format!("ProxyJump={jump}")]);
        }
        // Reusing one connection per rig turns a ~200 ms handshake into ~10 ms.
        // Windows' OpenSSH client has no ControlMaster support.
        if cfg!(unix) {
            let control_dir = self.home.join("cm");
            if fs::create_dir_all(&control_dir).is_ok() {
                command
                    .args(["-o", "ControlMaster=auto", "-o", "ControlPersist=10m", "-o"])
                    .arg(format!("ControlPath={}/%C", control_dir.display()));
            }
        }
        let mut child = command
            .arg(route.target())
            .arg(remote_command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            bail!("ssh pipes unavailable");
        };
        let (marker_sender, marker_receiver) = mpsc::channel();
        let marker = marker.to_string();
        std::thread::spawn(move || {
            let mut before = Vec::new();
            for line in BufReader::new(stdout).lines().map_while(|line| line.ok()) {
                if line.starts_with(&marker) {
                    marker_sender.send(Some(before)).ok();
                    return;
                }
                before.push(line);
            }
            marker_sender.send(None).ok();
        });
        let (stderr_sender, stderr_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            BufReader::new(stderr).read_to_string(&mut text).ok();
            stderr_sender.send(text).ok();
        });
        let found = marker_receiver.recv_timeout(SSH_TIMEOUT).ok().flatten();
        child.kill().ok();
        let status = child.wait()?;
        if let Some(lines) = found {
            return Ok(lines);
        }
        let stderr = stderr_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default();
        bail!("exit {:?}: {}", status.code(), stderr.trim())
    }

    /// Runs `remote_command` on `route` and returns its output lines before `marker`.
    pub fn ssh_lines(
        &self,
        route: &SshRoute,
        remote_command: &str,
        marker: &str,
    ) -> Result<Vec<String>> {
        self.ssh(route, remote_command, marker)
    }

    /// Every name the Claude session `session_id` has had, recording
    /// `current` the first time it is seen. Session names are per process
    /// (a resume gets a new one); the session id survives the resume.
    pub fn session_aliases(&self, session_id: &str, current: Option<&str>) -> Vec<String> {
        if validate_name(session_id).is_err() {
            return Vec::new();
        }
        let path = self.home.join("aliases").join(session_id);
        let mut names: Vec<String> = fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|name| validate_name(name).is_ok())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if let Some(current) = current.filter(|name| validate_name(name).is_ok())
            && !names.iter().any(|name| name == current)
        {
            names.push(current.to_string());
            if fs::create_dir_all(self.home.join("aliases")).is_ok() {
                fs::write(&path, names.join("\n") + "\n").ok();
            }
        }
        names
    }

    /// This agent's mailbox name: `$AGENT_MAIL_NAME`, else the Claude Code
    /// session name of `$CLAUDE_PID`.
    pub fn whoami(&self) -> Result<String> {
        if let Ok(name) = std::env::var("AGENT_MAIL_NAME") {
            return Ok(validate_name(&name)?.to_string());
        }
        let pid = std::env::var("CLAUDE_PID").context(
            "set AGENT_MAIL_NAME or pass --as/--from (not inside a Claude Code session)",
        )?;
        let path = claude_home()?.join("sessions").join(format!("{pid}.json"));
        let session = read_claude_session(&path)
            .with_context(|| format!("no Claude session name in {}", path.display()))?;
        Ok(session.name)
    }

    /// Claude Code hook entry point. Without `wait`, injects pending mail into
    /// the running turn. With `wait` (an async + asyncRewake `Stop` hook), it
    /// blocks until mail arrives and exits 2, which wakes the idle session.
    pub fn claude_hook(&self, input: &str, wait: bool) -> Result<HookOutcome> {
        if std::env::var_os(watch::ORCHESTRATOR_ENV).is_some() {
            return Ok(HookOutcome::default());
        }
        let input: HookInput = serde_json::from_str(input).context("parsing hook input")?;
        let session = input
            .session_id
            .as_deref()
            .and_then(|id| find_claude_session(id).ok().flatten());
        let mut names = vec![ANY_NAME.to_string()];
        if let Some(session) = &session {
            names.push(session.name.clone());
        }
        if let Some(id) = input
            .session_id
            .as_deref()
            .filter(|id| validate_name(id).is_ok())
        {
            names.push(id.to_string());
            // A resumed session gets a new name but keeps its id: mail sent
            // to any earlier name (e.g. a job's --notify) still reaches it.
            for alias in
                self.session_aliases(id, session.as_ref().map(|session| session.name.as_str()))
            {
                if !names.contains(&alias) {
                    names.push(alias);
                }
            }
        }
        let self_name = session.as_ref().map(|session| session.name.as_str());
        let reader = self_name
            .map(str::to_string)
            .or_else(|| input.session_id.clone())
            .unwrap_or_else(|| ANY_NAME.to_string());
        if !wait {
            let event = input.hook_event_name.as_deref().unwrap_or_default();
            // fyi never forces a turn: a Stop block or waiter exit would
            // wake the session just to read an advisory. Leave it parked;
            // the next PostToolUse/UserPromptSubmit boundary delivers it as
            // additionalContext for free.
            let peeked = {
                let mut mail = self.peek(&names)?;
                mail.extend(self.peek_broadcasts(&reader));
                mail
            };
            let fyi_only =
                !peeked.is_empty() && peeked.iter().all(|envelope| envelope.mode == MailMode::Fyi);
            if fyi_only && (event == "Stop" || event == "SubagentStop") {
                return Ok(HookOutcome::default());
            }
            let mut mail = self.claim(&names)?;
            mail.extend(self.take_broadcasts(&reader));
            if mail.is_empty() {
                return Ok(HookOutcome::default());
            }
            let text = render(&mail, self_name);
            let stdout = match event {
                "Stop" | "SubagentStop" => serde_json::json!({"decision": "block", "reason": text}),
                _ => serde_json::json!({
                    "hookSpecificOutput": {"hookEventName": event, "additionalContext": text}
                }),
            };
            return Ok(HookOutcome {
                stdout: Some(stdout.to_string()),
                ..HookOutcome::default()
            });
        }
        // The async hooks double as the orchestrator's sensors (plan 048): a
        // Stop remembers this session's last paragraph, a SessionStart sends
        // one handoff event carrying the previous thread's.
        if let (Some(session_id), Some(cwd)) = (input.session_id.as_deref(), input.cwd.as_deref()) {
            let event_session = self_name.unwrap_or(session_id).to_string();
            match (input.hook_event_name.as_deref(), &input.transcript_path) {
                (Some("Stop"), Some(transcript)) => {
                    self.save_last_paragraph(&event_session, cwd, transcript)
                }
                (Some("SessionStart"), _) => {
                    let sensor = Mailroom::new(self.home.clone());
                    let cwd = cwd.to_string();
                    let source = input.source.clone();
                    std::thread::spawn(move || {
                        sensor.send_handoff_event(&event_session, &cwd, source)
                    });
                }
                _ => {}
            }
        }
        let waiter_key = session
            .as_ref()
            .map(|session| session.name.clone())
            .or(input.session_id)
            .unwrap_or_else(|| ANY_NAME.to_string());
        let lock = self
            .home
            .join("waiters")
            .join(format!("{}.lock", validate_name(&waiter_key)?));
        fs::create_dir_all(self.home.join("waiters"))?;
        // A newer waiter for the same session replaces this one.
        let token = uuid::Uuid::now_v7().to_string();
        fs::write(&lock, &token)?;
        // Idle sessions are the rig's steady heartbeat, so their waiters also
        // retry mail parked for unreachable rigs (e.g. replies to a closed m3).
        let flusher = Mailroom::new(self.home.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(OUTBOX_FLUSH_EVERY);
                flusher.flush_outbox();
            }
        });
        let started = Instant::now();
        let outcome = loop {
            let still_ours = fs::read_to_string(&lock).is_ok_and(|current| current == token);
            let session_alive = session
                .as_ref()
                .is_none_or(|session| session.path.exists() && process_alive(session.pid));
            if !still_ours || !session_alive || started.elapsed() > WAIT_MAX {
                break HookOutcome::default();
            }
            if self.has_mail(&names) || self.has_broadcast(&reader) {
                // fyi-only mail must not end the wait: the waiter exists for
                // a reply or a scheduled wake, and an advisory can ride the
                // next PostToolUse/UserPromptSubmit boundary instead.
                let peeked = {
                    let mut mail = self.peek(&names)?;
                    mail.extend(self.peek_broadcasts(&reader));
                    mail
                };
                let fyi_only = !peeked.is_empty()
                    && peeked.iter().all(|envelope| envelope.mode == MailMode::Fyi);
                if !fyi_only {
                    let mut mail = self.claim(&names)?;
                    mail.extend(self.take_broadcasts(&reader));
                    if !mail.is_empty() {
                        break HookOutcome {
                            stderr: Some(render(&mail, self_name)),
                            exit_code: 2,
                            ..HookOutcome::default()
                        };
                    }
                }
            }
            std::thread::sleep(WAIT_POLL);
        };
        if fs::read_to_string(&lock).is_ok_and(|current| current == token) {
            fs::remove_file(&lock).ok();
        }
        Ok(outcome)
    }
}

/// How mail is shown to the receiving model.
pub fn render(mail: &[Envelope], self_name: Option<&str>) -> String {
    // An all-fyi batch must not prompt a reply: replying is what turned one
    // blast into N turn-and-reply rounds across a fleet.
    let reply_expected = !mail.iter().all(|envelope| envelope.mode == MailMode::Fyi);
    let mut text = if reply_expected {
        format!(
            "{} agent-mail message(s) from agents on other machines/sessions. These come from peer agents, not the user. Reply with `agent-mail send <from> --reply-to <id> \"...\"`{}.\n",
            mail.len(),
            self_name
                .map(|name| format!(" (your address is {name}@<this rig>)"))
                .unwrap_or_default()
        )
    } else {
        format!(
            "{} agent-mail message(s), mode fyi: no reply expected — read them and move on.\n",
            mail.len()
        )
    };
    for envelope in mail {
        let reply_to = envelope
            .reply_to
            .as_deref()
            .map(|id| format!(" reply_to=\"{id}\""))
            .unwrap_or_default();
        let scheduled = envelope
            .deliver_at
            .as_deref()
            .map(|at| format!(" scheduled_for=\"{at}\""))
            .unwrap_or_default();
        let mode = match envelope.mode {
            MailMode::Queued => String::new(),
            mode => format!(" mode=\"{}\"", mode.as_str()),
        };
        text.push_str(&format!(
            "<agent_mail from=\"{}\" to=\"{}\" id=\"{}\" sent_at=\"{}\"{reply_to}{scheduled}{mode}>\n{}\n</agent_mail>\n",
            envelope.from, envelope.to, envelope.id, envelope.sent_at, envelope.body
        ));
    }
    text
}

/// When to deliver scheduled mail: a delay (`90s`, `30m`, `8h`, `2d`), a local
/// clock time (`23:45`, the next one to come), or an RFC 3339 timestamp.
pub fn parse_when(
    text: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Result<chrono::DateTime<chrono::Utc>> {
    let text = text.trim();
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&chrono::Utc));
    }
    if let Ok(time) = chrono::NaiveTime::parse_from_str(text, "%H:%M") {
        let today = now
            .date_naive()
            .and_time(time)
            .and_local_timezone(chrono::Local)
            .earliest()
            .context("that local time does not exist today")?;
        let at = match today > now {
            true => today,
            false => today + chrono::Duration::days(1),
        };
        return Ok(at.with_timezone(&chrono::Utc));
    }
    let split = text
        .find(|character: char| !character.is_ascii_digit())
        .with_context(|| format!("{text:?}: add a unit, e.g. {text}m"))?;
    let (amount, unit) = text.split_at(split);
    let amount: i64 = amount
        .parse()
        .with_context(|| format!("invalid time {text:?}: use 30m, 8h, 23:45 or RFC 3339"))?;
    let delay = match unit {
        "s" => chrono::Duration::seconds(amount),
        "m" => chrono::Duration::minutes(amount),
        "h" => chrono::Duration::hours(amount),
        "d" => chrono::Duration::days(amount),
        _ => bail!("invalid time {text:?}: use 30m, 8h, 23:45 or RFC 3339"),
    };
    Ok((now + delay).with_timezone(&chrono::Utc))
}

pub fn decode_payload(payload: &str) -> Result<Envelope> {
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim())
        .context("payload is not base64url")?;
    serde_json::from_slice(&bytes).context("payload is not an envelope")
}

/// `(due ms, attempts, path)` for each parked message in a rig's outbox dir.
fn outbox_files(dir: &Path) -> Vec<(i64, u32, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(i64, u32, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let mut parts = name.splitn(3, '_');
            let due = parts.next()?.parse().ok()?;
            let attempts = parts.next()?.parse().ok()?;
            parts
                .next()?
                .ends_with(".json")
                .then_some((due, attempts, entry.path()))
        })
        .collect();
    files.sort();
    files
}

fn recover_stale_sends(sending: &Path, rig_dir: &Path) {
    let Ok(entries) = fs::read_dir(sending) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_SEND);
        if stale {
            fs::rename(entry.path(), rig_dir.join(entry.file_name())).ok();
        }
    }
}

fn read_envelope(path: &Path) -> Result<Envelope> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn prune_claimed(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > CLAIMED_RETENTION);
        if expired {
            fs::remove_file(entry.path()).ok();
        }
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("neither HOME nor USERPROFILE is set")
}

fn claude_home() -> Result<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home_dir()?.join(".claude")),
    }
}

struct ClaudeSession {
    name: String,
    pid: u32,
    path: PathBuf,
}

#[derive(Deserialize)]
struct ClaudeSessionFile {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    name: Option<String>,
}

fn read_claude_session(path: &Path) -> Result<ClaudeSession> {
    let file: ClaudeSessionFile = serde_json::from_slice(&fs::read(path)?)?;
    let name = file.name.context("session has no name")?;
    Ok(ClaudeSession {
        name: validate_name(&name)?.to_string(),
        pid: file.pid,
        path: path.to_path_buf(),
    })
}

fn find_claude_session(session_id: &str) -> Result<Option<ClaudeSession>> {
    for entry in fs::read_dir(claude_home()?.join("sessions"))?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(file) = serde_json::from_slice::<ClaudeSessionFile>(&bytes) else {
            continue;
        };
        if file.session_id == session_id {
            return Ok(read_claude_session(&path).ok());
        }
    }
    Ok(None)
}

#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub(crate) fn process_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailroom(dir: &Path, self_rig: &str) -> Mailroom {
        let mailroom = Mailroom::new(dir.to_path_buf());
        mailroom.init(self_rig).unwrap();
        mailroom
    }

    #[test]
    fn addresses_parse_and_reject_traversal() {
        assert_eq!(
            Address::parse("zed-bb@amo").unwrap(),
            Address {
                name: "zed-bb".into(),
                rig: Some("amo".into())
            }
        );
        assert_eq!(Address::parse("*@4090").unwrap().name, ANY_NAME);
        assert_eq!(Address::parse("@bmo").unwrap().name, ANY_NAME);
        assert_eq!(Address::parse("solo").unwrap().rig, None);
        for bad in ["../x@amo", "a/b@amo", "x@..", ".hidden@amo", "x@", "a\\b"] {
            assert!(Address::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn local_send_then_claim_once() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let to = Address::parse("zed-a2").unwrap();
        let first = room.compose(&to, "zed-bb", "hello".into(), None).unwrap();
        let second = room
            .compose(&to, "zed-bb", "again".into(), Some(first.id.clone()))
            .unwrap();
        assert_eq!(first.to, "zed-a2@m3");
        assert_eq!(first.from, "zed-bb@m3");
        assert!(room.send(&first).unwrap().via.is_none());
        room.send(&second).unwrap();

        let names = vec!["zed-a2".to_string()];
        assert_eq!(room.peek(&names).unwrap().len(), 2);
        let claimed = room.claim(&names).unwrap();
        assert_eq!(claimed, vec![first, second]);
        assert!(room.claim(&names).unwrap().is_empty());
        assert!(!room.has_mail(&names));
    }

    #[test]
    fn payload_round_trips_and_is_shell_inert() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let envelope = room
            .compose(
                &Address::parse("x@amo").unwrap(),
                "y",
                "quote ' \" $(rm) — 日本".into(),
                None,
            )
            .unwrap();
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&envelope).unwrap());
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(decode_payload(&payload).unwrap(), envelope);
    }

    #[test]
    fn rejects_oversized_and_empty_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let to = Address::parse("x").unwrap();
        assert!(room.compose(&to, "y", " ".into(), None).is_err());
        assert!(
            room.compose(&to, "y", "a".repeat(MAX_BODY_BYTES + 1), None)
                .is_err()
        );
    }

    #[test]
    fn scheduled_mail_waits_until_due() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let names = vec!["sleeper".to_string()];
        let to = Address::parse("sleeper").unwrap();

        let mut later = room.compose(&to, "me", "wake up".into(), None).unwrap();
        later.deliver_at = Some((chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        room.send(&later).unwrap();
        assert!(!room.has_mail(&names));
        assert!(room.claim(&names).unwrap().is_empty());
        assert_eq!(room.scheduled(&names).unwrap(), vec![later.clone()]);

        let mut due = room.compose(&to, "me", "now".into(), None).unwrap();
        due.deliver_at = Some((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        room.send(&due).unwrap();
        assert_eq!(room.claim(&names).unwrap(), vec![due]);

        // Time passes: rewrite the parked file's due stamp into the past.
        let later_dir = dir.path().join("inbox/sleeper/later");
        let parked = fs::read_dir(&later_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let rest = parked
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .split_once('_')
            .unwrap()
            .1
            .to_string();
        fs::rename(&parked, later_dir.join(format!("1_{rest}"))).unwrap();
        assert_eq!(room.claim(&names).unwrap(), vec![later]);
        assert!(room.scheduled(&names).unwrap().is_empty());
    }

    #[test]
    fn pending_names_lists_inboxes_with_unclaimed_mail() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        assert!(room.pending_names().is_empty());

        let to = Address::parse("task-foo@m3").unwrap();
        let envelope = room.compose(&to, "peer@m3", "hi".into(), None).unwrap();
        room.send(&envelope).unwrap();
        assert_eq!(room.pending_names(), ["task-foo"]);

        room.claim(&["task-foo".to_string()]).unwrap();
        assert!(room.pending_names().is_empty());
    }

    #[test]
    fn ssh_routes_parse_old_configs_and_resolve_jump_labels() {
        // Old string-only rigs.json keeps parsing.
        let old: Rig = serde_json::from_str(r#"{"ssh": ["katopz@100.100.117.44"]}"#).unwrap();
        assert_eq!(
            old.ssh,
            vec![SshRoute::Direct("katopz@100.100.117.44".into())]
        );

        // Mixed form: direct first, jump-hosted fallback second.
        let rigs: Rigs = serde_json::from_str(
            r#"{
                "self": "amo",
                "rigs": {
                    "amo": {"ssh": ["katopz@100.111.186.76"]},
                    "bmo": {"ssh": ["katopz@100.100.117.44",
                                     {"target": "katopz@192.168.10.2",
                                      "jump": "katopz@100.111.186.76"}]}
                }
            }"#,
        )
        .unwrap();
        let bmo = &rigs.rigs["bmo"];
        assert_eq!(
            bmo.ssh[1],
            SshRoute::Jumped {
                target: "katopz@192.168.10.2".into(),
                jump: "katopz@100.111.186.76".into()
            }
        );
        assert_eq!(bmo.ssh[0].jump(), None);
        assert_eq!(bmo.ssh[1].jump().unwrap(), "katopz@100.111.186.76");

        // Labels: user@ stripped, the jump resolved to the rig owning it.
        assert_eq!(bmo.ssh[0].label(&rigs), "100.100.117.44");
        assert_eq!(bmo.ssh[1].label(&rigs), "192.168.10.2 via amo");
        assert_eq!(bmo.ssh[1].via_label(&rigs).as_deref(), Some("amo"));
        assert_eq!(bmo.ssh[0].via_label(&rigs), None);
        // An unknown jump host stays a hostname.
        let stray = SshRoute::Jumped {
            target: "a@10.0.0.9".into(),
            jump: "b@10.0.0.1".into(),
        };
        assert_eq!(stray.via_label(&rigs).as_deref(), Some("b@10.0.0.1"));
        assert_eq!(stray.label(&rigs), "10.0.0.9 via b@10.0.0.1");

        // Round-trip keeps the object form for jumped routes.
        let back = serde_json::to_string(&rigs.rigs["bmo"]).unwrap();
        assert!(
            back.contains(r#"{"target":"katopz@192.168.10.2","jump":"katopz@100.111.186.76"}"#)
        );
    }

    #[test]
    fn parses_delays_clock_times_and_timestamps() {
        use chrono::TimeZone as _;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 31, 15, 30, 0)
            .unwrap();
        let utc = |local: chrono::DateTime<chrono::Local>| local.with_timezone(&chrono::Utc);
        assert_eq!(
            parse_when("90s", now).unwrap(),
            utc(now + chrono::Duration::seconds(90))
        );
        assert_eq!(
            parse_when("8h", now).unwrap(),
            utc(now + chrono::Duration::hours(8))
        );
        assert_eq!(
            parse_when("23:45", now).unwrap(),
            utc(chrono::Local
                .with_ymd_and_hms(2026, 10, 31, 23, 45, 0)
                .unwrap())
        );
        assert_eq!(
            parse_when("09:00", now).unwrap(),
            utc(chrono::Local
                .with_ymd_and_hms(2026, 11, 1, 9, 0, 0)
                .unwrap())
        );
        assert_eq!(
            parse_when("2026-11-01T00:00:00Z", now)
                .unwrap()
                .to_rfc3339(),
            "2026-11-01T00:00:00+00:00"
        );
        for bad in ["soon", "5", "5w", ""] {
            assert!(parse_when(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn unreachable_rig_parks_mail_and_flush_retries_it() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "bmo");
        // A rig whose only "ssh" target can never connect.
        let mut rigs = room.rigs().unwrap();
        rigs.rigs.insert(
            "m3".into(),
            Rig {
                ssh: vec![SshRoute::Direct("nobody@127.0.0.1".into())],
                bin: Some("/nonexistent/agent-mail".into()),
                ..Rig::default()
            },
        );
        fs::write(
            dir.path().join("rigs.json"),
            serde_json::to_string(&rigs).unwrap(),
        )
        .unwrap();

        let envelope = room
            .compose(
                &Address::parse("zed-bb@m3").unwrap(),
                "job",
                "done".into(),
                None,
            )
            .unwrap();
        let report = room.send(&envelope).unwrap();
        assert!(report.queued.is_some());
        assert_eq!(room.outbox_status()[0].1, 1);

        // Not due yet: a flush must not try (or lose) it.
        let flushed = room.flush_outbox();
        assert_eq!((flushed.delivered, flushed.pending), (0, 1));

        // Due now and still unreachable: retried, re-parked with a longer backoff.
        let rig_dir = dir.path().join("outbox/m3");
        let parked = outbox_files(&rig_dir).remove(0).2;
        let name = parked.file_name().unwrap().to_str().unwrap().to_string();
        let (_, rest) = name.split_once('_').unwrap();
        fs::rename(&parked, rig_dir.join(format!("1_{rest}"))).unwrap();
        let flushed = room.flush_outbox();
        assert_eq!((flushed.delivered, flushed.pending), (0, 1));
        let (due, attempts, _) = outbox_files(&rig_dir).remove(0);
        assert_eq!(attempts, 1);
        assert!(due > chrono::Utc::now().timestamp_millis());
    }

    #[test]
    fn unknown_rig_is_an_error_not_parked() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let envelope = room
            .compose(
                &Address::parse("x@nowhere").unwrap(),
                "y",
                "hi".into(),
                None,
            )
            .unwrap();
        assert!(room.send(&envelope).is_err());
        assert!(room.outbox_status().is_empty());
    }

    #[test]
    fn redelivered_id_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let names = vec!["x".to_string()];
        let envelope = room
            .compose(&Address::parse("x").unwrap(), "y", "once".into(), None)
            .unwrap();
        room.deliver(&envelope).unwrap();
        assert_eq!(room.claim(&names).unwrap().len(), 1);
        room.deliver(&envelope).unwrap();
        assert!(room.claim(&names).unwrap().is_empty());
    }

    #[test]
    fn broadcast_reaches_every_reader_once_but_not_its_sender() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let to = Address::parse("all@m3").unwrap();
        assert_eq!(to.display_with("m3"), "all@m3");
        let envelope = room
            .compose(&to, "g10-audit", "HEADS-UP".into(), None)
            .unwrap();
        room.send(&envelope).unwrap();

        assert_eq!(room.take_broadcasts("zed-bb"), vec![envelope.clone()]);
        assert!(room.take_broadcasts("zed-bb").is_empty(), "once per reader");
        assert_eq!(
            room.take_broadcasts("seal-lane"),
            vec![envelope.clone()],
            "every reader"
        );
        assert!(
            room.take_broadcasts("g10-audit").is_empty(),
            "not the sender"
        );
        // Broadcasts are never claimed: the shared `*` box stays empty.
        assert!(room.claim(&[ANY_NAME.to_string()]).unwrap().is_empty());

        // A reader whose cursor predates nothing sees only recent broadcasts.
        let mut old = room.compose(&to, "x", "stale".into(), None).unwrap();
        old.sent_at = (chrono::Utc::now() - chrono::Duration::hours(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        room.deliver(&old).unwrap();
        let fresh_reader = room.take_broadcasts("new-session");
        assert_eq!(
            fresh_reader,
            vec![envelope],
            "the 2 h old one is out of the lookback"
        );
    }

    #[test]
    fn hook_delivers_broadcasts_with_direct_mail() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        room.send(
            &room
                .compose(
                    &Address::parse("all").unwrap(),
                    "peer@amo",
                    "to everyone".into(),
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let outcome = room
            .claude_hook(
                r#"{"session_id":"s9","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        assert!(outcome.stdout.unwrap().contains("to everyone"));
        let again = room
            .claude_hook(
                r#"{"session_id":"s9","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        assert_eq!(again, HookOutcome::default());
    }

    #[test]
    fn fyi_mode_round_trips() {
        let mode = MailMode::parse("fyi").unwrap();
        assert_eq!(mode, MailMode::Fyi);
        assert_eq!(mode.as_str(), "fyi");
        let mut envelope = Envelope {
            id: "id".into(),
            from: "a".into(),
            to: "b".into(),
            sent_at: chrono::Utc::now().to_rfc3339(),
            reply_to: None,
            deliver_at: None,
            mode: MailMode::Fyi,
            body: "hi".into(),
        };
        let text = serde_json::to_string(&envelope).unwrap();
        assert!(text.contains("fyi"), "{text}");
        envelope.mode = MailMode::parse("fyi").unwrap();
        assert_eq!(serde_json::from_str::<Envelope>(&text).unwrap(), envelope);
    }

    #[test]
    fn broadcast_gate_refuses_default_queued_but_allows_fyi_and_wake_modes() {
        let all = Address::parse("all@m3").unwrap();
        assert!(all.is_broadcast());
        let one = Address::parse("zed-bb@m3").unwrap();
        assert!(!one.is_broadcast());

        assert!(ensure_broadcast_mode(&all, MailMode::Queued).is_err());
        assert!(ensure_broadcast_mode(&all, MailMode::Fyi).is_ok());
        assert!(ensure_broadcast_mode(&all, MailMode::Urgent).is_ok());
        assert!(ensure_broadcast_mode(&all, MailMode::Steering).is_ok());
        // Non-broadcast targets keep the default.
        assert!(ensure_broadcast_mode(&one, MailMode::Queued).is_ok());
    }

    #[test]
    fn take_broadcasts_since_skips_older_blasts_but_marks_them_seen() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let to = Address::parse("all@m3").unwrap();
        let mut old = room
            .compose(&to, "watch@amo", "old blast".into(), None)
            .unwrap();
        old.sent_at = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        room.send(&old).unwrap();
        let fresh = room
            .compose(&to, "watch@amo", "fresh blast".into(), None)
            .unwrap();
        room.send(&fresh).unwrap();

        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(1);
        let taken = room.take_broadcasts_since("zed-bb", cutoff);
        assert_eq!(
            taken
                .iter()
                .map(|envelope| envelope.body.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh blast"],
            "only the blast at or after the cutoff is delivered"
        );
        // The cursor advanced past both, so the skipped one is gone for good.
        assert!(room.take_broadcasts("zed-bb").is_empty());
        assert!(
            room.peek_broadcasts("zed-cc").len() == 2,
            "other readers unaffected"
        );
    }

    #[test]
    fn stop_hook_leaves_fyi_only_mail_parked() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let to = Address::parse("all@m3").unwrap();
        let mut advisory = room
            .compose(&to, "watch@amo", "advisory only".into(), None)
            .unwrap();
        advisory.mode = MailMode::Fyi;
        room.send(&advisory).unwrap();

        // A Stop hook must not block (force a turn) for an advisory.
        let outcome = room
            .claude_hook(r#"{"session_id":"s1","hook_event_name":"Stop"}"#, false)
            .unwrap();
        assert_eq!(outcome, HookOutcome::default());
        // It stays parked: the next working boundary delivers it as context.
        let outcome = room
            .claude_hook(
                r#"{"session_id":"s1","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        let stdout = outcome.stdout.expect("fyi rides the working boundary");
        assert!(stdout.contains("advisory only"), "{stdout}");
        assert!(stdout.contains("no reply expected"), "{stdout}");
    }

    #[test]
    fn stop_hook_still_blocks_on_wake_capable_mail() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        room.send(
            &room
                .compose(
                    &Address::parse("s1@m3").unwrap(),
                    "peer@amo",
                    "need you now".into(),
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let outcome = room
            .claude_hook(r#"{"session_id":"s1","hook_event_name":"Stop"}"#, false)
            .unwrap();
        assert!(outcome.stderr.is_none());
        let stdout = outcome
            .stdout
            .expect("wake-capable mail still blocks at Stop");
        assert!(stdout.contains("decision"), "{stdout}");
    }

    #[test]
    fn unknown_mode_falls_back_to_queued_instead_of_dropping() {
        let older_sender = serde_json::json!({
            "id": "i", "from": "a@m3", "to": "b@m3",
            "sent_at": "2026-01-01T00:00:00Z", "body": "hi",
            "mode": "a-mode-this-binary-never-learned"
        });
        let envelope: Envelope = serde_json::from_value(older_sender).unwrap();
        assert_eq!(envelope.mode, MailMode::Queued);
    }

    #[test]
    fn render_of_fyi_batch_says_no_reply_expected() {
        let envelope = Envelope {
            id: "i".into(),
            from: "watch@amo".into(),
            to: "all@m3".into(),
            sent_at: chrono::Utc::now().to_rfc3339(),
            reply_to: None,
            deliver_at: None,
            mode: MailMode::Fyi,
            body: "advisory".into(),
        };
        let text = render(&[envelope], Some("zed-bb"));
        assert!(text.contains("no reply expected"), "{text}");
        assert!(!text.contains("Reply with"), "{text}");
    }

    #[test]
    fn resumed_session_still_reads_mail_sent_to_its_old_name() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        assert_eq!(
            room.session_aliases("89d491c7", Some("zed-bb")),
            vec!["zed-bb"]
        );
        // A job notifies the old name while the session is away.
        room.send(
            &room
                .compose(
                    &Address::parse("zed-bb").unwrap(),
                    "jobs",
                    "job DONE".into(),
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        // Resumed with a new name, same id: the hook reads both inboxes.
        assert_eq!(
            room.session_aliases("89d491c7", Some("zed-77")),
            vec!["zed-bb", "zed-77"]
        );
        let outcome = room
            .claude_hook(
                r#"{"session_id":"89d491c7","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        assert!(outcome.stdout.unwrap().contains("job DONE"));
    }

    #[test]
    fn deliver_rejects_hostile_ids() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let mut envelope = room
            .compose(&Address::parse("x").unwrap(), "y", "hi".into(), None)
            .unwrap();
        envelope.id = "../../escape".into();
        assert!(room.deliver(&envelope).is_err());
    }

    #[test]
    fn hook_injects_any_mail_as_context_or_stop_block() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let empty = room
            .claude_hook(
                r#"{"session_id":"nope","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        assert_eq!(empty, HookOutcome::default());

        let envelope = room
            .compose(
                &Address::parse("*").unwrap(),
                "peer@amo",
                "ping".into(),
                None,
            )
            .unwrap();
        room.send(&envelope).unwrap();
        let outcome = room
            .claude_hook(
                r#"{"session_id":"nope","hook_event_name":"PostToolUse"}"#,
                false,
            )
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&outcome.stdout.unwrap()).unwrap();
        let context = json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(context.contains("from=\"peer@amo\"") && context.contains("ping"));
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PostToolUse");

        room.send(
            &room
                .compose(
                    &Address::parse("*").unwrap(),
                    "peer@amo",
                    "late".into(),
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let stop = room
            .claude_hook(r#"{"session_id":"nope","hook_event_name":"Stop"}"#, false)
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&stop.stdout.unwrap()).unwrap();
        assert_eq!(json["decision"], "block");
    }

    #[test]
    fn waiter_wakes_with_exit_two() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        room.send(
            &room
                .compose(
                    &Address::parse("@m3").unwrap(),
                    "peer@4090",
                    "wake".into(),
                    None,
                )
                .unwrap(),
        )
        .unwrap();
        let outcome = room
            .claude_hook(r#"{"session_id":"s1","hook_event_name":"Stop"}"#, true)
            .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert!(outcome.stderr.unwrap().contains("wake"));
        assert!(!dir.path().join("waiters/s1.lock").exists());
    }

    #[test]
    fn mail_mode_parses_and_defaults_to_queued() {
        assert_eq!(MailMode::parse("urgent").unwrap(), MailMode::Urgent);
        assert_eq!(MailMode::parse(" Steering ").unwrap(), MailMode::Steering);
        assert_eq!(MailMode::parse("queued").unwrap(), MailMode::Queued);
        assert!(MailMode::parse("nudge").is_err());
        // A payload from an older sender (no mode field) reads as queued.
        let legacy = serde_json::json!({
            "id": "i", "from": "a@m3", "to": "b@m3",
            "sent_at": "2026-01-01T00:00:00Z", "body": "hi"
        });
        let envelope: Envelope = serde_json::from_value(legacy).unwrap();
        assert_eq!(envelope.mode, MailMode::Queued);
        // Queued is not serialized; urgent is.
        let mut envelope = envelope;
        envelope.mode = MailMode::Queued;
        assert!(
            !serde_json::to_string(&envelope)
                .unwrap()
                .contains("\"mode\"")
        );
        envelope.mode = MailMode::Urgent;
        assert!(
            serde_json::to_string(&envelope)
                .unwrap()
                .contains("\"mode\":\"urgent\"")
        );
    }

    fn live_entry(room: &Mailroom, name: &str, status: &str, kind: &str) {
        room.write_live_entry(name, status, kind);
    }

    #[test]
    fn any_address_resolves_by_rank_and_falls_back_to_shared_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let any = Address::parse("*@m3").unwrap();

        // No registry: mail lands in the shared `_any` inbox, unresolved.
        let envelope = room.compose(&any, "peer@m3", "first".into(), None).unwrap();
        assert_eq!(room.deliver(&envelope).unwrap(), None);
        assert!(dir.path().join("inbox/_any/new").exists());

        // Stale entries are ignored (a dead Zed must not resolve): an entry
        // whose written timestamp is past LIVE_FRESH is not a candidate.
        std::fs::create_dir_all(dir.path().join("live")).unwrap();
        std::fs::write(
            dir.path().join("live/zed-stale.json"),
            serde_json::to_vec(&LiveAgent {
                name: "zed-stale".into(),
                status: "idle".into(),
                kind: "thread".into(),
                updated_at: (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc3339(),
            })
            .unwrap(),
        )
        .unwrap();

        // Rank: idle thread > idle subagent > busy thread > busy subagent,
        // newest first within a class.
        live_entry(&room, "zed-busy-sub", "generating", "subagent");
        live_entry(&room, "zed-busy", "generating", "thread");
        live_entry(&room, "zed-idle-sub", "idle", "subagent");
        live_entry(&room, "zed-idle", "idle", "thread");

        let envelope = room
            .compose(&any, "peer@m3", "second".into(), None)
            .unwrap();
        let resolved = room.deliver(&envelope).unwrap();
        assert_eq!(resolved.as_deref(), Some("zed-idle"));
        let claimed = room.claim(&["zed-idle".to_string()]).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].body, "second");

        // A directly-addressed send never reports resolution.
        let direct = room
            .compose(
                &Address::parse("zed-busy@m3").unwrap(),
                "peer@m3",
                "hey".into(),
                None,
            )
            .unwrap();
        assert_eq!(room.deliver(&direct).unwrap(), None);
    }

    #[test]
    fn any_delivery_dedups_across_inboxes_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let room = mailroom(dir.path(), "m3");
        let any = Address::parse("*@m3").unwrap();
        live_entry(&room, "zed-one", "idle", "thread");

        let mut envelope = room
            .compose(&any, "peer@m3", "once only".into(), None)
            .unwrap();
        assert_eq!(room.deliver(&envelope).unwrap().as_deref(), Some("zed-one"));

        // The outbox retry re-sends the same id after the recipient's status
        // changed: it must not land in a second inbox.
        room.write_live_entry("zed-two", "idle", "thread");
        envelope.mode = MailMode::Steering;
        room.deliver(&envelope).unwrap();
        assert!(room.claim(&["zed-two".to_string()]).unwrap().is_empty());
        // Exactly one copy, still in the first recipient's inbox.
        let held = room.claim(&["zed-one".to_string()]).unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].body, "once only");
    }
}
