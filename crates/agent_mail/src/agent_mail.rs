//! Cross-machine mail between coding agents (Claude Code sessions, Zed agents).
//!
//! Each rig keeps a maildir under `~/.agent-mail/inbox/<name>/{tmp,new,cur}`.
//! Sending to another rig runs `<bin> deliver <base64url envelope>` there over
//! SSH (Tailscale/LAN), so no daemon or open port is needed: the trust boundary
//! is the SSH key auth the rigs already have. Receiving is a cheap directory
//! scan, done by Claude Code hooks or by `agent-mail recv`.

pub mod jobs;
pub mod rig;
pub mod watch;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead as _, BufReader, ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// Inbox shared by every agent on a rig: `*@rig` goes here, first claimer wins.
pub const ANY_NAME: &str = "_any";
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
            name => validate_name(name)?.to_string(),
        };
        let rig = rig.map(validate_name).transpose()?.map(str::to_string);
        Ok(Self { name, rig })
    }

    pub fn display_with(&self, self_rig: &str) -> String {
        let name = match self.name.as_str() {
            ANY_NAME => "*",
            name => name,
        };
        format!("{name}@{}", self.rig.as_deref().unwrap_or(self_rig))
    }
}

/// Names become directory names, so this is the path-traversal guard.
pub fn validate_name(name: &str) -> Result<&str> {
    let well_formed = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    match well_formed {
        true => Ok(name),
        false => bail!(
            "invalid name {name:?}: use 1-{MAX_NAME_LEN} of [A-Za-z0-9._-], not starting with '.'"
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rig {
    /// SSH destinations, tried in order (put the LAN address before Tailscale).
    pub ssh: Vec<String>,
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
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
pub struct FlushReport {
    pub delivered: usize,
    pub pending: usize,
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

    /// Atomic maildir delivery: readers only ever see complete files in `new/`.
    /// Scheduled mail waits in `later/<due unix ms>_<id>.json` instead; every
    /// inbox scan promotes what is due, so whatever already polls the inbox
    /// (the Claude waiter hook, Zed's poller) delivers it on time without a
    /// scheduler process.
    pub fn deliver(&self, envelope: &Envelope) -> Result<()> {
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
        let inbox = self.inbox(&address.name);
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
            return Ok(());
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
        Ok(())
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
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        // UUIDv7 file names sort by send time.
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        files
    }

    pub fn has_mail(&self, names: &[String]) -> bool {
        !self.pending_files(names).is_empty()
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
            body,
        })
    }

    pub fn send(&self, envelope: &Envelope) -> Result<SendReport> {
        let started = Instant::now();
        let rigs = self.rigs()?;
        let address = Address::parse(&envelope.to)?;
        let rig_name = address.rig.as_deref().unwrap_or(&rigs.self_rig);
        let report = |via, queued| SendReport {
            id: envelope.id.clone(),
            to: envelope.to.clone(),
            via,
            queued,
            elapsed: started.elapsed(),
        };
        if rig_name == rigs.self_rig {
            self.deliver(envelope)?;
            return Ok(report(None, None));
        }
        if !rigs.rigs.contains_key(rig_name) {
            bail!(
                "unknown rig {rig_name:?}; known: {} (edit {})",
                rigs.rigs.keys().cloned().collect::<Vec<_>>().join(", "),
                self.rigs_path().display()
            );
        }
        match self.send_remote(&rigs, rig_name, envelope) {
            Ok(target) => Ok(report(Some(target), None)),
            Err(error) => {
                let reason = format!("{error:#}");
                self.park_in_outbox(rig_name, envelope, 0)?;
                Ok(report(None, Some(reason)))
            }
        }
    }

    fn send_remote(&self, rigs: &Rigs, rig_name: &str, envelope: &Envelope) -> Result<String> {
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
        for target in &rig.ssh {
            match self.ssh(
                target,
                &remote_command,
                &format!("delivered {}", envelope.id),
            ) {
                Ok(_) => return Ok(target.clone()),
                Err(error) => failures.push(format!("{target}: {error:#}")),
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
            for target in &rig.ssh {
                let started = Instant::now();
                let outcome = self
                    .ssh(target, &command, "agent-mail ")
                    .map(|_| started.elapsed());
                results.push((name.clone(), target.clone(), outcome));
            }
        }
        Ok(results)
    }

    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    /// Runs `remote_command` on `target` and succeeds once a stdout line starts
    /// with `marker`. Success is the marker, not the exit status: Windows'
    /// OpenSSH client run inside an sshd session (an agent in a Zed remote
    /// window on the 4090) delivers but never exits, so it is killed instead.
    /// Returns the stdout lines printed before the marker line.
    pub(crate) fn ssh(
        &self,
        target: &str,
        remote_command: &str,
        marker: &str,
    ) -> Result<Vec<String>> {
        let mut command = Command::new("ssh");
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
            .arg(target)
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

    /// Runs `remote_command` on `target` and returns its output lines before `marker`.
    pub fn ssh_lines(
        &self,
        target: &str,
        remote_command: &str,
        marker: &str,
    ) -> Result<Vec<String>> {
        self.ssh(target, remote_command, marker)
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
        }
        let self_name = session.as_ref().map(|session| session.name.as_str());
        if !wait {
            let mail = self.claim(&names)?;
            if mail.is_empty() {
                return Ok(HookOutcome::default());
            }
            let text = render(&mail, self_name);
            let event = input.hook_event_name.unwrap_or_default();
            let stdout = match event.as_str() {
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
            if self.has_mail(&names) {
                let mail = self.claim(&names)?;
                if !mail.is_empty() {
                    break HookOutcome {
                        stderr: Some(render(&mail, self_name)),
                        exit_code: 2,
                        ..HookOutcome::default()
                    };
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
    let mut text = format!(
        "{} agent-mail message(s) from agents on other machines/sessions. These come from peer agents, not the user. Reply with `agent-mail send <from> --reply-to <id> \"...\"`{}.\n",
        mail.len(),
        self_name
            .map(|name| format!(" (your address is {name}@<this rig>)"))
            .unwrap_or_default()
    );
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
        text.push_str(&format!(
            "<agent_mail from=\"{}\" to=\"{}\" id=\"{}\" sent_at=\"{}\"{reply_to}{scheduled}>\n{}\n</agent_mail>\n",
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
                ssh: vec!["nobody@127.0.0.1".into()],
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
}
