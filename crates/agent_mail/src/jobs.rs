//! Per-rig job queue (plan 047 P1/P2). No daemon: `rig run` enqueues, every
//! probe (each `rig status`, each watchdog tick) and every finishing job runs
//! [`Mailroom::schedule_jobs`], which reaps dead jobs and starts what fits;
//! each job runs under a detached `agent-mail rig exec` that labels poisoning
//! and mails its submitter when it starts and ends.
//!
//! Lanes: `shared` jobs run side by side when the rig's free resources cover
//! what they declared; an `exclusive` job waits for an empty, quiet rig and
//! then holds it alone. Once an exclusive job is first in line nothing starts
//! behind it, so benchmarks cannot be starved by a stream of builds.

use crate::rig::{self, Contention};
use crate::verify::{
    read_result_file, verify_against_baseline, RunEnvironment, VerifyLedgerRow, VerifyOutcome,
    VerifySpec,
};
use crate::{Address, MailMode, Mailroom, validate_name};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const POISON_PROBE_EVERY: Duration = Duration::from_secs(15);
/// Ends the orchestrator's key verdict output over SSH.
pub const KEY_VERDICT_END: &str = "key-verdict-end";
/// Ends `rig log-local` / `rig cancel-local` output over SSH.
pub const END_MARKER: &str = "agent-mail-end";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    Log(usize),
    Cancel,
}
const WAIT_POLL: Duration = Duration::from_secs(1);
const KICK_EVERY: Duration = Duration::from_secs(20);
/// Memory a shared job must leave free for everything else on the rig.
const MEM_HEADROOM_GB: f64 = 2.0;
/// Pause after an exclusive job before the next one starts, so a benchmark
/// does not measure the previous one's heat.
const EXCLUSIVE_COOLDOWN: Duration = Duration::from_secs(60);
const DONE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LOG_TAIL_CHARS: usize = 1500;
const SCHEDULE_LOCK_STALE: Duration = Duration::from_secs(60);
/// A duplicate key whose job has not reported back in this long is free again.
const KEY_STALE: chrono::Duration = chrono::Duration::days(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Needs {
    #[serde(default)]
    pub cores: f64,
    #[serde(default)]
    pub mem_gb: f64,
    #[serde(default)]
    pub gpu_mem_gb: f64,
    /// Rig capabilities the job requires (`cuda`, `metal`, `zed`, ...).
    #[serde(default)]
    pub caps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: String,
    pub lane: Lane,
    pub title: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub needs: Needs,
    /// Duplicate-run key, refereed by the orchestrator rig.
    #[serde(default)]
    pub key: Option<String>,
    /// Who gets the lifecycle mail (`name@rig`).
    #[serde(default)]
    pub notify: Option<String>,
    /// How the recipient delivers mid-turn lifecycle mail (issue 046).
    #[serde(default)]
    pub notify_mode: MailMode,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// `repo@sha`: run in a fresh worktree of `~/git/<repo>` at `sha`, with its
    /// own target dir, so the job never builds a sibling agent's dirty tree.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Set on verifier-lane runs (issue 055): a re-measurement of another
    /// rig's claimed gain, forced exclusive, refused on the producing rig.
    #[serde(default)]
    pub verify: Option<VerifySpec>,
    /// JSON file (relative to the job's cwd) the command must write with its
    /// measured numbers; without it a verify run ends `no-result` — a gain
    /// whose cell is missing is not a result.
    #[serde(default)]
    pub result_path: Option<String>,
    pub submitted_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Poison {
    pub at: String,
    pub pid: u32,
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub spec: JobSpec,
    pub executor_pid: u32,
    #[serde(default)]
    pub child_pid: Option<u32>,
    pub started_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    /// `done`, `failed`, `timeout`, `cancelled`, `executor-died`, `no-result`.
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub poisoned: Vec<Poison>,
    /// Provenance of the machine that ran the job (issue 055).
    #[serde(default)]
    pub environment: Option<RunEnvironment>,
    /// Measured numbers read from `result_path` before the worktree is
    /// cleaned up (a verify run's counted cell).
    #[serde(default)]
    pub measured: Option<BTreeMap<String, f64>>,
    /// Ratio-to-baseline verdicts, set when a verify run finished.
    #[serde(default)]
    pub verify_outcome: Option<VerifyOutcome>,
}

/// A job's private checkout and the repo it belongs to.
struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

/// One line of `rig status` for a managed job. `text` is the preformatted
/// CLI rendering; the rest is structured for UIs that split title and
/// metadata. Structured fields are `#[serde(default)]` so probes from older
/// builds — which send only `id` and `text` — still decode.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobLine {
    pub id: String,
    pub text: String,
    /// The job's title/command, without the id, lane, or target prefixes.
    #[serde(default)]
    pub title: Option<String>,
    /// `Shared` or `Exclusive`.
    #[serde(default)]
    pub lane: Option<String>,
    /// Queue position (1-based), for waiting jobs.
    #[serde(default)]
    pub position: Option<usize>,
    /// Elapsed minutes, for running jobs.
    #[serde(default)]
    pub minutes: Option<i64>,
    /// Who gets the lifecycle mail.
    #[serde(default)]
    pub notify: Option<String>,
    /// Poison-probe strikes against an exclusive run.
    #[serde(default)]
    pub poisoned: usize,
}

pub struct JobSummary {
    pub running: Vec<JobLine>,
    pub queued: Vec<JobLine>,
    /// Executor + child pids of running jobs: their trees are not contenders.
    pub managed_pids: Vec<u32>,
}

/// Picks the rig for `rig run auto`: takes jobs, online, has the required
/// capabilities; an exclusive job prefers a quiet rig with nothing managed
/// running, a shared one the most free cores; fewer waiting jobs first.
/// `exclude` names rigs the job may not run on (a verify run never lands on
/// the rig that produced the gain it re-measures, issue 055).
pub fn pick_rig(
    results: &[(String, Result<rig::RigProbe>, Duration)],
    rigs: &crate::Rigs,
    needs: &Needs,
    lane: Lane,
    exclude: &[String],
) -> Result<String> {
    let mut candidates: Vec<(usize, i64, String)> = results
        .iter()
        .filter_map(|(name, outcome, _)| {
            let probe = outcome.as_ref().ok()?;
            if exclude.iter().any(|rig| rig == name) {
                return None;
            }
            let rig = rigs.rigs.get(name)?;
            if !rig.accepts_jobs || !needs.caps.iter().all(|cap| rig.caps.contains(cap)) {
                return None;
            }
            let free = probe.cores as f64 - probe.load1.unwrap_or(0.0);
            let unfit = match lane {
                Lane::Exclusive => usize::from(!(probe.quiet && probe.running.is_empty())),
                Lane::Shared => usize::from(free < needs.cores),
            };
            Some((
                unfit * 1000 + probe.queued.len() + probe.running.len(),
                -(free as i64),
                name.clone(),
            ))
        })
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .map(|(_, _, name)| name)
        .with_context(|| format!("no online rig takes this job (needs caps {:?})", needs.caps))
}

/// Parses a duration like `90s`, `30m`, `2h`, `1d`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|character: char| !character.is_ascii_digit())
        .with_context(|| format!("{text:?}: add a unit, e.g. {text}m"))?;
    let (amount, unit) = text.split_at(split);
    let amount: u64 = amount
        .parse()
        .with_context(|| format!("invalid duration {text:?}"))?;
    let seconds = match unit {
        "s" => amount,
        "m" => amount * 60,
        "h" => amount * 3600,
        "d" => amount * 86_400,
        _ => bail!("invalid duration {text:?}: use 90s, 30m, 2h or 1d"),
    };
    Ok(Duration::from_secs(seconds))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(8)..).unwrap_or(id)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let staged = path.with_extension("tmp");
    fs::write(&staged, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&staged, path)?;
    Ok(())
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .collect()
        })
        .unwrap_or_default();
    // UUIDv7 names sort by submission time: the queue is FIFO.
    files.sort();
    files
}

/// Held while scheduling; a crashed holder is ignored after a minute.
struct FileLock(PathBuf);

impl FileLock {
    fn acquire(path: &Path) -> Option<Self> {
        let stale = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > SCHEDULE_LOCK_STALE);
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

impl Drop for FileLock {
    fn drop(&mut self) {
        fs::remove_file(&self.0).ok();
    }
}

/// What the scheduler knows about free capacity right now.
#[derive(Debug, Clone)]
pub struct Capacity {
    pub cores: f64,
    pub load1: Option<f64>,
    pub mem_free_gb: Option<f64>,
    pub gpu_free_gb: Option<f64>,
}

/// Whether a shared job fits next to the running ones. Both the measured load
/// and the declared reservations must leave room: load lags a just-started
/// job, and reservations miss work nobody declared.
pub fn fits(needs: &Needs, capacity: &Capacity, reserved: &Needs) -> bool {
    let busy_cores = capacity.load1.unwrap_or(0.0).max(reserved.cores);
    let cores_ok = needs.cores <= (capacity.cores - busy_cores).max(0.0);
    let mem_ok = capacity
        .mem_free_gb
        .is_none_or(|free| needs.mem_gb + MEM_HEADROOM_GB <= free);
    let gpu_ok = needs.gpu_mem_gb <= 0.0
        || capacity
            .gpu_free_gb
            .is_some_and(|free| needs.gpu_mem_gb <= free - reserved.gpu_mem_gb);
    cores_ok && mem_ok && gpu_ok
}

/// Which queued jobs to start, in queue order. Pure, so it is testable.
pub fn admit(
    queue: &[JobSpec],
    running: &[JobSpec],
    capacity: &Capacity,
    quiet: bool,
    cooled: bool,
) -> Vec<String> {
    let mut start = Vec::new();
    if running.iter().any(|job| job.lane == Lane::Exclusive) {
        return start;
    }
    let mut reserved = Needs::default();
    for job in running {
        reserved.cores += job.needs.cores;
        reserved.mem_gb += job.needs.mem_gb;
        reserved.gpu_mem_gb += job.needs.gpu_mem_gb;
    }
    for job in queue {
        match job.lane {
            Lane::Exclusive => {
                // First in line: everything behind waits, even what would fit.
                if running.is_empty() && start.is_empty() && quiet && cooled {
                    start.push(job.id.clone());
                }
                break;
            }
            Lane::Shared => {
                if fits(&job.needs, capacity, &reserved) {
                    reserved.cores += job.needs.cores;
                    reserved.mem_gb += job.needs.mem_gb;
                    reserved.gpu_mem_gb += job.needs.gpu_mem_gb;
                    start.push(job.id.clone());
                }
            }
        }
    }
    start
}

impl Mailroom {
    fn jobs_dir(&self, sub: &str) -> PathBuf {
        self.home().join("jobs").join(sub)
    }

    /// Queues `spec` on this rig (refused where the rig takes no jobs) and
    /// starts whatever can run now.
    pub fn enqueue_job(&self, spec: &JobSpec) -> Result<()> {
        validate_name(&spec.id)?;
        let rigs = self.rigs()?;
        if rigs
            .rigs
            .get(&rigs.self_rig)
            .is_some_and(|rig| !rig.accepts_jobs)
        {
            bail!("rig {} takes no jobs", rigs.self_rig);
        }
        anyhow::ensure!(!spec.argv.is_empty(), "empty command");
        let queue = self.jobs_dir("queue");
        fs::create_dir_all(&queue)?;
        write_json(&queue.join(format!("{}.json", spec.id)), spec)?;
        self.schedule_jobs()?;
        Ok(())
    }

    /// Reaps jobs whose executor died, then starts what fits.
    pub fn schedule_jobs(&self) -> Result<Vec<String>> {
        self.schedule_jobs_inner(true)
    }

    fn schedule_jobs_inner(&self, may_spawn_kicker: bool) -> Result<Vec<String>> {
        let queue_dir = self.jobs_dir("queue");
        if !queue_dir.exists() && !self.jobs_dir("running").exists() {
            return Ok(Vec::new());
        }
        fs::create_dir_all(self.jobs_dir("running"))?;
        let Some(_lock) = FileLock::acquire(&self.home().join("jobs").join("schedule.lock")) else {
            return Ok(Vec::new());
        };
        let mut running = Vec::new();
        for path in json_files(&self.jobs_dir("running")) {
            let Some(record) = read_json::<JobRecord>(&path) else {
                continue;
            };
            match crate::process_alive(record.executor_pid) {
                true => running.push(record.spec),
                false => self.finish(record, "executor-died", None, &path),
            }
        }
        let queue: Vec<JobSpec> = json_files(&queue_dir)
            .iter()
            .filter_map(|path| read_json(path))
            .collect();
        if queue.is_empty() {
            return Ok(Vec::new());
        }
        let needs_quiet = queue
            .iter()
            .find(|job| job.lane == Lane::Exclusive)
            .is_some();
        let quiet = needs_quiet && running.is_empty() && rig::contention(&[]).quiet();
        let cooled = fs::metadata(self.jobs_dir("last_exclusive_end"))
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age >= EXCLUSIVE_COOLDOWN);
        let capacity = Capacity {
            cores: std::thread::available_parallelism().map_or(1.0, |cores| cores.get() as f64),
            load1: rig::load_average(),
            mem_free_gb: rig::memory_free_gb(),
            gpu_free_gb: rig::gpus()
                .iter()
                .map(|gpu| gpu.mem_total_gb - gpu.mem_used_gb)
                .reduce(f64::max),
        };
        let start = admit(&queue, &running, &capacity, quiet, cooled);
        for id in &start {
            self.spawn_executor(id)?;
        }
        if may_spawn_kicker && start.len() < queue.len() {
            // Jobs left waiting (rig busy, not quiet, cooling down): keep one
            // short-lived kicker re-checking until the queue drains, so a
            // waiting job starts within ~20 s of becoming admissible.
            self.ensure_kicker()?;
        }
        Ok(start)
    }

    fn ensure_kicker(&self) -> Result<()> {
        let pid_path = self.home().join("jobs").join("kicker.pid");
        let alive = fs::read_to_string(&pid_path)
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
            .is_some_and(crate::process_alive);
        if alive {
            return Ok(());
        }
        self.spawn_detached(&["rig", "kick"], "kicker")
    }

    /// Body of the kicker: re-run the scheduler until nothing is queued.
    pub fn kick_until_drained(&self) -> Result<()> {
        let pid_path = self.home().join("jobs").join("kicker.pid");
        fs::write(&pid_path, std::process::id().to_string())?;
        loop {
            std::thread::sleep(KICK_EVERY);
            let still_ours = fs::read_to_string(&pid_path)
                .is_ok_and(|pid| pid.trim() == std::process::id().to_string());
            if !still_ours || json_files(&self.jobs_dir("queue")).is_empty() {
                break;
            }
            // Remove our pid first so the scheduler's own ensure_kicker call
            // does not see a live kicker and skip; we re-register right after.
            fs::remove_file(&pid_path).ok();
            if let Err(error) = self.schedule_jobs_inner(false) {
                eprintln!("agent-mail kicker: scheduling failed: {error:#}");
            }
            fs::write(&pid_path, std::process::id().to_string())?;
        }
        if fs::read_to_string(&pid_path)
            .is_ok_and(|pid| pid.trim() == std::process::id().to_string())
        {
            fs::remove_file(&pid_path).ok();
        }
        Ok(())
    }

    fn spawn_executor(&self, id: &str) -> Result<()> {
        // Claim before spawning so a second scheduler never starts it twice.
        let queued = self.jobs_dir("queue").join(format!("{id}.json"));
        let starting = self.jobs_dir("running").join(format!("{id}.starting"));
        fs::rename(&queued, &starting)?;
        self.spawn_detached(&["rig", "exec", id], id)
    }

    /// Starts `agent-mail <args>` outside this process's lifetime.
    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    fn spawn_detached(&self, args: &[&str], label: &str) -> Result<()> {
        let binary = std::env::current_exe()?;
        let logs = self.jobs_dir("logs");
        fs::create_dir_all(&logs)?;
        if cfg!(windows) {
            // Processes started from an sshd session die with it on the 4090;
            // a one-shot scheduled task outlives the session.
            let task = format!("agent-mail-{label}");
            let action = format!("\"{}\" {}", binary.display(), args.join(" "));
            let created = crate::command("schtasks")
                .args([
                    "/create", "/tn", &task, "/tr", &action, "/sc", "once", "/st", "00:00", "/f",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            anyhow::ensure!(created.success(), "schtasks /create failed");
            crate::command("schtasks")
                .args(["/run", "/tn", &task])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
            return Ok(());
        }
        let log = fs::File::create(logs.join(format!("{label}.executor.log")))?;
        let mut command = crate::command(binary);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        command.spawn().context("starting a detached agent-mail")?;
        Ok(())
    }

    /// The detached executor: runs one claimed job to the end.
    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    pub fn execute_job(&self, id: &str) -> Result<()> {
        validate_name(id)?;
        let running_dir = self.jobs_dir("running");
        let starting = running_dir.join(format!("{id}.starting"));
        let record_path = running_dir.join(format!("{id}.json"));
        let spec: JobSpec = read_json(&starting).context("job is not waiting to start")?;
        fs::remove_file(&starting).ok();
        let mut record = JobRecord {
            spec,
            executor_pid: std::process::id(),
            child_pid: None,
            started_at: now(),
            ended_at: None,
            outcome: None,
            exit_code: None,
            poisoned: Vec::new(),
            environment: None,
            measured: None,
            verify_outcome: None,
        };
        let rig_name = self.rigs().map(|rigs| rigs.self_rig).unwrap_or_default();
        let mut environment = RunEnvironment::capture(
            &rig_name,
            record.spec.worktree.as_deref(),
            record.spec.verify.as_ref().map(|verify| verify.manifest_blake3.as_str()),
        );
        // Clean-environment evidence (issue 056): a timed cell that did not
        // start QUIET is suspect however its numbers turn out. Own pid is
        // excluded — the executor itself is not a contender.
        environment.quiet_at_start = Some(rig::contention(&[std::process::id()]).quiet());
        record.environment = Some(environment);
        write_json(&record_path, &record)?;
        self.job_mail(
            &record.spec,
            format!(
                "job {} STARTED on {rig_name} ({:?} lane): {}",
                short(id),
                record.spec.lane,
                record.spec.title
            ),
        );

        let (cwd, worktree) = match self.prepare_worktree(&record.spec) {
            Ok(prepared) => prepared,
            Err(error) => {
                let log_path = self.jobs_dir("logs").join(format!("{id}.log"));
                fs::write(&log_path, format!("worktree setup failed: {error:#}\n")).ok();
                self.finish(record, "failed", None, &record_path);
                return Ok(());
            }
        };
        let log_path = self.jobs_dir("logs").join(format!("{id}.log"));
        let log = fs::File::create(&log_path)?;
        let mut command = shell_command(&record.spec.argv);
        command
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(cwd) = &cwd {
            command.current_dir(cwd);
        }
        if let Some(worktree) = &worktree {
            command.env("CARGO_TARGET_DIR", worktree.path.with_extension("target"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                fs::write(&log_path, format!("could not start: {error}\n")).ok();
                self.cleanup_worktree(worktree.as_ref());
                self.finish(record, "failed", None, &record_path);
                return Ok(());
            }
        };
        record.child_pid = Some(child.id());
        write_json(&record_path, &record)?;

        let started = Instant::now();
        let timeout = record.spec.timeout_secs.map(Duration::from_secs);
        let cancel_path = running_dir.join(format!("{id}.cancel"));
        let mut last_probe = Instant::now();
        let outcome = loop {
            if let Some(status) = child.try_wait()? {
                break match status.success() {
                    true => ("done", status.code()),
                    false => ("failed", status.code()),
                };
            }
            if cancel_path.exists() {
                kill_tree(child.id());
                child.wait().ok();
                fs::remove_file(&cancel_path).ok();
                break ("cancelled", None);
            }
            if timeout.is_some_and(|limit| started.elapsed() > limit) {
                kill_tree(child.id());
                child.wait().ok();
                break ("timeout", None);
            }
            if record.spec.lane == Lane::Exclusive && last_probe.elapsed() >= POISON_PROBE_EVERY {
                last_probe = Instant::now();
                let fresh = new_poison(
                    &rig::contention(&[std::process::id(), child.id()]),
                    &record.poisoned,
                );
                if !fresh.is_empty() {
                    let names = fresh
                        .iter()
                        .map(|poison| format!("{} (pid {})", poison.name, poison.pid))
                        .collect::<Vec<_>>()
                        .join(", ");
                    record.poisoned.extend(fresh);
                    write_json(&record_path, &record)?;
                    self.job_mail(
                        &record.spec,
                        format!(
                            "job {} POISONED on {rig_name}: {names} started during your exclusive run; results from now on are suspect. Still running: decide whether to keep or re-run.",
                            short(id)
                        ),
                    );
                }
            }
            std::thread::sleep(WAIT_POLL);
        };
        // The result file lives in the worktree, which is deleted below —
        // read the measured cell first so a verify run can count it.
        if outcome.0 == "done"
            && let Some(result_path) = &record.spec.result_path
        {
            record.measured = read_result_file(&cwd, result_path);
            write_json(&record_path, &record)?;
        }
        self.cleanup_worktree(worktree.as_ref());
        if record.spec.lane == Lane::Exclusive {
            fs::write(self.jobs_dir("last_exclusive_end"), now()).ok();
        }
        self.finish(record, outcome.0, outcome.1, &record_path);
        if cfg!(windows) {
            crate::command("schtasks")
                .args(["/delete", "/tn", &format!("agent-mail-{id}"), "/f"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok();
        }
        self.schedule_jobs().ok();
        Ok(())
    }

    fn finish(
        &self,
        mut record: JobRecord,
        outcome: &str,
        exit_code: Option<i32>,
        running_path: &Path,
    ) {
        // A verify run only counts when it produced its measured cell: the
        // command exiting cleanly without the numbers is `no-result`, not a
        // gain (issue 055: a cell that lacks its numbers is not a result).
        let outcome = if outcome == "done"
            && record.spec.result_path.is_some()
            && record.measured.is_none()
        {
            "no-result"
        } else {
            outcome
        }
        .to_string();
        if record.spec.verify.is_some() && outcome == "done" {
            let provenance = record
                .environment
                .as_ref()
                .is_none_or(|environment| !environment.complete());
            if let Some(measured) = record.measured.clone()
                && let Some(verify) = record.spec.verify.clone()
            {
                let mut verify_outcome = verify_against_baseline(&measured, &verify.baseline);
                verify_outcome.origin_job = verify.origin_job;
                verify_outcome.produced_on = verify.produced_on;
                verify_outcome.verified_on = record
                    .environment
                    .as_ref()
                    .map(|environment| environment.rig.clone())
                    .unwrap_or_default();
                verify_outcome.provenance_incomplete = provenance;
                verify_outcome.quiet_at_start = record
                    .environment
                    .as_ref()
                    .and_then(|environment| environment.quiet_at_start);
                record.verify_outcome = Some(verify_outcome.clone());
                self.record_verify_ledger(&record, &verify_outcome, &outcome);
            }
        }
        record.outcome = Some(outcome.clone());
        record.exit_code = exit_code;
        record.ended_at = Some(now());
        let id = record.spec.id.clone();
        let done = self.jobs_dir("done");
        fs::create_dir_all(&done).ok();
        if write_json(&done.join(format!("{id}.json")), &record).is_ok() {
            fs::remove_file(running_path).ok();
        }
        prune_done(&done);
        let rig_name = self.rigs().map(|rigs| rigs.self_rig).unwrap_or_default();
        let log_path = self.jobs_dir("logs").join(format!("{id}.log"));
        let poison = match record.poisoned.is_empty() {
            true => String::new(),
            false => format!(
                "\nPOISONED by: {}",
                record
                    .poisoned
                    .iter()
                    .map(|poison| format!("{} (pid {}) at {}", poison.name, poison.pid, poison.at))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        };
        let verify = match &record.verify_outcome {
            None => String::new(),
            Some(outcome) => {
                let mut lines = vec![format!(
                    "\nVERIFY: {} vs {} — {}{}{}",
                    outcome.verified_on, outcome.produced_on, outcome.verdict,
                    if outcome.provenance_incomplete {
                        " (provenance incomplete: does not count)"
                    } else {
                        ""
                    },
                    match outcome.quiet_at_start {
                        Some(false) => " (did NOT start QUIET: suspect cell)",
                        _ => "",
                    }
                )];
                for (name, metric) in &outcome.metrics {
                    lines.push(format!(
                        "  {name}: measured {} vs baseline {} ({:+.1}%, {})",
                        metric.measured,
                        metric.baseline,
                        (metric.improvement - 1.0) * 100.0,
                        metric.verdict
                    ));
                }
                lines.join("\n")
            }
        };
        let summary = format!(
            "job {} {} on {rig_name} (exit {}): {}{poison}{verify}\nlog: {}\n--- log tail ---\n{}",
            short(&id),
            outcome.to_uppercase(),
            exit_code.map_or("-".into(), |code| code.to_string()),
            record.spec.title,
            log_path.display(),
            log_tail(&log_path)
        );
        self.job_mail(&record.spec, summary.clone());
        if let Some(key) = &record.spec.key {
            self.release_key_remote(key, &summary).ok();
        }
    }

    /// Posts a verify run's produced-vs-verified row to the orchestrator's
    /// ledger (issue 055: the referee records which rig verified which
    /// rig's gain). Best-effort: an old orchestrator just drops the row.
    fn record_verify_ledger(
        &self,
        record: &JobRecord,
        verify_outcome: &VerifyOutcome,
        outcome: &str,
    ) {
        let row = VerifyLedgerRow {
            id: record.spec.id.clone(),
            origin_job: verify_outcome.origin_job.clone(),
            produced_on: verify_outcome.produced_on.clone(),
            verified_on: verify_outcome.verified_on.clone(),
            plan: record.spec.verify.as_ref().map(|verify| verify.baseline.plan.clone()),
            baseline_sha: record
                .environment
                .as_ref()
                .and_then(|environment| environment.baseline_sha.clone()),
            manifest_blake3: record
                .spec
                .verify
                .as_ref()
                .map(|verify| verify.manifest_blake3.clone()),
            outcome: outcome.to_string(),
            verdict: verify_outcome.verdict.clone(),
            provenance_incomplete: verify_outcome.provenance_incomplete,
            quiet_at_start: verify_outcome.quiet_at_start,
            arm: record
                .spec
                .verify
                .as_ref()
                .and_then(|verify| verify.baseline.arm.clone()),
            metrics: verify_outcome
                .metrics
                .iter()
                .map(|(name, verdict)| (name.clone(), verdict.clone()))
                .collect(),
            at: now(),
        };
        let payload = match serde_json::to_vec(&row) {
            Ok(bytes) => URL_SAFE_NO_PAD.encode(bytes),
            Err(error) => {
                eprintln!("agent-mail: encoding verify ledger row failed: {error}");
                return;
            }
        };
        self.on_orchestrator(
            &format!("rig verify-ledger {payload}"),
            END_MARKER,
            |room| room.append_verify_ledger(&row).map(|()| Vec::new()),
        )
        .or_else(|error| {
            // No orchestrator configured (single-rig setup): the verifying
            // rig keeps the row locally rather than losing it.
            let no_orchestrator = self
                .rigs()
                .map(|rigs| rigs.orchestrator.is_none())
                .unwrap_or(true);
            if no_orchestrator {
                self.append_verify_ledger(&row).map(|()| Vec::new())
            } else {
                Err(error)
            }
        })
        .ok();
    }

    /// Orchestrator side: appends the row to the append-only verify ledger.
    pub fn append_verify_ledger(&self, row: &VerifyLedgerRow) -> Result<()> {
        use std::io::Write as _;
        let ledger = self.jobs_dir("verify_ledger.jsonl");
        if let Some(parent) = ledger.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&ledger)?;
        writeln!(file, "{}", serde_json::to_string(row)?)?;
        Ok(())
    }

    fn job_mail(&self, spec: &JobSpec, body: String) {
        let Some(notify) = &spec.notify else {
            return;
        };
        let Ok(to) = Address::parse(notify) else {
            return;
        };
        if let Ok(envelope) = self.compose(&to, "jobs", body, None) {
            let mut envelope = envelope;
            envelope.mode = spec.notify_mode;
            self.send(&envelope).ok();
        }
    }

    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    fn prepare_worktree(&self, spec: &JobSpec) -> Result<(Option<PathBuf>, Option<Worktree>)> {
        let Some(target) = &spec.worktree else {
            return Ok((spec.cwd.clone().map(PathBuf::from), None));
        };
        let (repo, sha) = target
            .split_once('@')
            .context("--worktree needs repo@sha")?;
        validate_name(repo)?;
        anyhow::ensure!(
            sha.bytes()
                .all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b'/')),
            "invalid revision {sha:?}"
        );
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("no home directory")?;
        let repo_path = [
            PathBuf::from(&home).join("git").join(repo),
            PathBuf::from("E:/git").join(repo),
        ]
        .into_iter()
        .find(|path| path.join(".git").exists())
        .with_context(|| format!("repo {repo} not found under ~/git"))?;
        let worktree = std::env::temp_dir().join("agent-mail-jobs").join(&spec.id);
        fs::create_dir_all(worktree.parent().context("temp dir")?)?;
        let status = crate::command("git")
            .arg("-C")
            .arg(&repo_path)
            .args(["worktree", "add", "--detach"])
            .arg(&worktree)
            .arg(sha)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        anyhow::ensure!(
            status.success(),
            "git worktree add {sha} failed in {}",
            repo_path.display()
        );
        let cwd = match &spec.cwd {
            Some(sub) => worktree.join(sub),
            None => worktree.clone(),
        };
        Ok((
            Some(cwd),
            Some(Worktree {
                repo: repo_path,
                path: worktree,
            }),
        ))
    }

    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    fn cleanup_worktree(&self, worktree: Option<&Worktree>) {
        let Some(worktree) = worktree else {
            return;
        };
        // Run inside the source repo: outside it git refuses, and the
        // worktree stays registered after its folder is gone.
        crate::command("git")
            .arg("-C")
            .arg(&worktree.repo)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree.path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
        fs::remove_dir_all(&worktree.path).ok();
        fs::remove_dir_all(worktree.path.with_extension("target")).ok();
        crate::command("git")
            .arg("-C")
            .arg(&worktree.repo)
            .args(["worktree", "prune"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }

    /// Cancels a queued or running job on this rig.
    pub fn cancel_job(&self, id: &str) -> Result<String> {
        validate_name(id)?;
        let queued = self.jobs_dir("queue").join(format!("{id}.json"));
        if let Some(spec) = read_json::<JobSpec>(&queued) {
            fs::remove_file(&queued)?;
            if let Some(key) = &spec.key {
                self.release_key_remote(
                    key,
                    &format!("job {} cancelled before it started", short(id)),
                )
                .ok();
            }
            return Ok(format!("cancelled queued job {id}"));
        }
        let running = self.jobs_dir("running").join(format!("{id}.json"));
        anyhow::ensure!(running.exists(), "no queued or running job {id} here");
        fs::write(self.jobs_dir("running").join(format!("{id}.cancel")), now())?;
        Ok(format!("cancelling running job {id}"))
    }

    pub fn job_log(&self, id: &str, lines: usize) -> Result<String> {
        validate_name(id)?;
        let text = fs::read_to_string(self.jobs_dir("logs").join(format!("{id}.log")))
            .with_context(|| format!("no log for job {id}"))?;
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(lines)..].join("\n"))
    }

    /// `rig log` / `rig cancel` against any rig: locally, or over SSH.
    pub fn job_on_rig(&self, rig_name: &str, id: &str, action: JobAction) -> Result<String> {
        let rigs = self.rigs()?;
        if rig_name == rigs.self_rig {
            return match action {
                JobAction::Log(lines) => self.job_log(id, lines),
                JobAction::Cancel => self.cancel_job(id),
            };
        }
        validate_name(id)?;
        let rig = rigs
            .rigs
            .get(rig_name)
            .with_context(|| format!("unknown rig {rig_name:?}"))?;
        let remote = match action {
            JobAction::Log(lines) => format!("rig log-local {id} {lines}"),
            JobAction::Cancel => format!("rig cancel-local {id}"),
        };
        let command = format!("{} {remote}", rig.bin.as_deref().unwrap_or("agent-mail"));
        let mut failures = Vec::new();
        for route in &rig.ssh {
            match self.ssh(route, &command, END_MARKER) {
                Ok(lines) => return Ok(lines.join("\n")),
                Err(error) => failures.push(format!("{route}: {error:#}")),
            }
        }
        Err(anyhow!("{rig_name} unreachable: {}", failures.join("; ")))
    }

    /// Running and queued jobs, for `rig status`.
    pub fn job_summary(&self) -> JobSummary {
        let mut summary = JobSummary {
            running: Vec::new(),
            queued: Vec::new(),
            managed_pids: Vec::new(),
        };
        for path in json_files(&self.jobs_dir("running")) {
            let Some(record) = read_json::<JobRecord>(&path) else {
                continue;
            };
            summary.managed_pids.push(record.executor_pid);
            summary.managed_pids.extend(record.child_pid);
            let elapsed = chrono::DateTime::parse_from_rfc3339(&record.started_at)
                .map(|at| chrono::Utc::now().signed_duration_since(at).num_minutes())
                .unwrap_or(0);
            summary.running.push(JobLine {
                id: record.spec.id.clone(),
                text: format!(
                    "{} {:?} {} min · {} · for {}{}",
                    short(&record.spec.id),
                    record.spec.lane,
                    elapsed,
                    record.spec.title,
                    record.spec.notify.as_deref().unwrap_or("-"),
                    match record.poisoned.len() {
                        0 => String::new(),
                        count => format!(" · POISONED ×{count}"),
                    }
                ),
                title: Some(record.spec.title.clone()),
                lane: Some(format!("{:?}", record.spec.lane)),
                position: None,
                minutes: Some(elapsed),
                notify: record.spec.notify.clone(),
                poisoned: record.poisoned.len(),
            });
        }
        for (position, path) in json_files(&self.jobs_dir("queue")).iter().enumerate() {
            let Some(spec) = read_json::<JobSpec>(path) else {
                continue;
            };
            summary.queued.push(JobLine {
                id: spec.id.clone(),
                text: format!(
                    "#{} {} {:?} · {} · for {}",
                    position + 1,
                    short(&spec.id),
                    spec.lane,
                    spec.title,
                    spec.notify.as_deref().unwrap_or("-")
                ),
                title: Some(spec.title.clone()),
                lane: Some(format!("{:?}", spec.lane)),
                position: Some(position + 1),
                minutes: None,
                notify: spec.notify.clone(),
                poisoned: 0,
            });
        }
        summary
    }

    /// Orchestrator side of job keys: claims `key` for `(rig, id)`, or, when a
    /// live job already holds it, subscribes `notify` to that job's result and
    /// returns where it runs. The exclusive create is the race-free referee
    /// for two agents submitting the same work to different rigs at once.
    pub fn claim_key(&self, claim: &KeyClaim) -> Result<KeyOutcome> {
        let dir = self.jobs_dir("keys");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.json", key_file_name(&claim.key)));
        let record = KeyRecord {
            key: claim.key.clone(),
            rig: claim.rig.clone(),
            id: claim.id.clone(),
            at: now(),
            subscribers: Vec::new(),
        };
        let created = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(|mut file| {
                use std::io::Write as _;
                file.write_all(&serde_json::to_vec(&record).unwrap_or_default())
            });
        if matches!(created, Ok(Ok(()))) {
            return Ok(KeyOutcome::Claimed);
        }
        let mut existing: KeyRecord = read_json(&path).context("unreadable key record")?;
        let stale = chrono::DateTime::parse_from_rfc3339(&existing.at)
            .is_ok_and(|at| chrono::Utc::now().signed_duration_since(at) > KEY_STALE);
        if stale {
            write_json(&path, &record)?;
            return Ok(KeyOutcome::Claimed);
        }
        if let Some(notify) = &claim.notify
            && !existing.subscribers.contains(notify)
        {
            existing.subscribers.push(notify.clone());
            write_json(&path, &existing)?;
        }
        Ok(KeyOutcome::Duplicate {
            rig: existing.rig,
            id: existing.id,
        })
    }

    /// Orchestrator side: the keyed job ended; tell its subscribers and free the key.
    pub fn release_key(&self, key: &str, summary: &str) -> Result<()> {
        let path = self
            .jobs_dir("keys")
            .join(format!("{}.json", key_file_name(key)));
        let Some(record) = read_json::<KeyRecord>(&path) else {
            return Ok(());
        };
        for subscriber in &record.subscribers {
            if let Ok(to) = Address::parse(subscriber)
                && let Ok(envelope) = self.compose(
                    &to,
                    "jobs",
                    format!("(you asked for the same job, key `{key}`)\n{summary}"),
                    None,
                )
            {
                self.send(&envelope).ok();
            }
        }
        fs::remove_file(&path)?;
        Ok(())
    }

    pub fn release_key_after_failed_enqueue(&self, key: &str) {
        self.release_key_remote(key, "the job could not be queued")
            .ok();
    }

    fn release_key_remote(&self, key: &str, summary: &str) -> Result<()> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(key, summary))?);
        self.on_orchestrator(&format!("rig key-release {payload}"), "released", |room| {
            room.release_key(key, summary)
        })
        .map(|_| ())
    }

    pub fn claim_key_remote(&self, claim: &KeyClaim) -> Result<KeyOutcome> {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claim)?);
        let lines = self.on_orchestrator(
            &format!("rig key-claim {payload}"),
            KEY_VERDICT_END,
            |room| {
                room.claim_key(claim)
                    .map(|outcome| vec![serde_json::to_string(&outcome).unwrap_or_default()])
            },
        )?;
        let json = lines
            .iter()
            .rev()
            .find(|line| line.trim_start().starts_with('{') || line.trim_start().starts_with('"'))
            .context("no key verdict from the orchestrator")?;
        serde_json::from_str(json).context("unparseable key verdict")
    }

    /// Runs `local` when this rig is the orchestrator, else `remote` there over SSH.
    fn on_orchestrator<T>(
        &self,
        remote: &str,
        marker: &str,
        local: impl FnOnce(&Mailroom) -> Result<T>,
    ) -> Result<Vec<String>>
    where
        T: IntoLines,
    {
        let rigs = self.rigs()?;
        let orchestrator = rigs
            .orchestrator
            .clone()
            .context("no orchestrator rig in rigs.json")?;
        if orchestrator == rigs.self_rig {
            return local(self).map(IntoLines::into_lines);
        }
        let rig = rigs
            .rigs
            .get(&orchestrator)
            .with_context(|| format!("unknown orchestrator rig {orchestrator:?}"))?;
        let command = format!("{} {remote}", rig.bin.as_deref().unwrap_or("agent-mail"));
        let mut failures = Vec::new();
        for route in &rig.ssh {
            match self.ssh(route, &command, marker) {
                Ok(lines) => return Ok(lines),
                Err(error) => failures.push(format!("{route}: {error:#}")),
            }
        }
        Err(anyhow!(
            "orchestrator {orchestrator} unreachable: {}",
            failures.join("; ")
        ))
    }
}

pub trait IntoLines {
    fn into_lines(self) -> Vec<String>;
}

impl IntoLines for () {
    fn into_lines(self) -> Vec<String> {
        Vec::new()
    }
}

impl IntoLines for Vec<String> {
    fn into_lines(self) -> Vec<String> {
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyClaim {
    pub key: String,
    pub rig: String,
    pub id: String,
    #[serde(default)]
    pub notify: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum KeyOutcome {
    Claimed,
    Duplicate { rig: String, id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KeyRecord {
    key: String,
    rig: String,
    id: String,
    at: String,
    #[serde(default)]
    subscribers: Vec<String>,
}

/// Keys are free text; file names are not. Collisions need two keys that
/// differ only in replaced characters, which job keys never do in practice.
fn key_file_name(key: &str) -> String {
    let safe: String = key
        .chars()
        .map(|character| {
            match character.is_ascii_alphanumeric() || matches!(character, '-' | '.') {
                true => character,
                false => '_',
            }
        })
        .take(120)
        .collect();
    format!("k_{safe}")
}

fn shell_command(argv: &[String]) -> Command {
    match argv {
        [single] if single.contains(char::is_whitespace) => shell(single),
        _ => {
            let mut command = crate::command(&argv[0]);
            command.args(&argv[1..]);
            command
        }
    }
}

#[cfg(windows)]
fn shell(line: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    // cmd.exe parses its own command line and does not understand the \"
    // escaping `arg` applies, which mangled quoted paths ("%USERPROFILE%\…"),
    // so the line is passed through untouched.
    let mut command = crate::command("cmd");
    command.raw_arg("/S /C \"").raw_arg(line).raw_arg("\"");
    command
}

#[cfg(not(windows))]
fn shell(line: &str) -> Command {
    let mut command = crate::command("sh");
    command.arg("-c").arg(line);
    command
}

// agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // The job runs in its own process group (pgid = its pid).
        if let Ok(group) = libc::pid_t::try_from(pid) {
            // SAFETY: signalling a process group we created.
            unsafe {
                libc::kill(-group, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_secs(5));
            // SAFETY: as above; a group already gone just returns ESRCH.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        crate::command("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }
}

/// Contenders not already recorded as poison for this run.
pub fn new_poison(contention: &Contention, known: &[Poison]) -> Vec<Poison> {
    contention
        .contenders
        .iter()
        .filter(|contender| !known.iter().any(|poison| poison.pid == contender.pid))
        .map(|contender| Poison {
            at: now(),
            pid: contender.pid,
            name: contender.name.clone(),
            command: contender.command.clone(),
        })
        .collect()
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let total = text.chars().count();
    match total > LOG_TAIL_CHARS {
        true => format!(
            "…{}",
            text.chars()
                .skip(total - LOG_TAIL_CHARS)
                .collect::<String>()
        ),
        false => text,
    }
}

fn prune_done(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let expired = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > DONE_RETENTION);
        if expired {
            fs::remove_file(entry.path()).ok();
        }
    }
}

pub fn encode_spec(spec: &JobSpec) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(spec)?))
}

pub fn decode<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T> {
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim())
        .context("payload is not base64url")?;
    serde_json::from_slice(&bytes).context("unparseable payload")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{Baseline, BaselineNumber};

    fn job(id: &str, lane: Lane, cores: f64) -> JobSpec {
        JobSpec {
            id: id.into(),
            lane,
            title: id.into(),
            argv: vec!["true".into()],
            cwd: None,
            needs: Needs {
                cores,
                mem_gb: 1.0,
                ..Needs::default()
            },
            key: None,
            notify: None,
            notify_mode: MailMode::Queued,
            timeout_secs: None,
            worktree: None,
            verify: None,
            result_path: None,
            submitted_at: now(),
        }
    }

    fn capacity(cores: f64, load: f64) -> Capacity {
        Capacity {
            cores,
            load1: Some(load),
            mem_free_gb: Some(64.0),
            gpu_free_gb: None,
        }
    }

    #[test]
    fn shared_jobs_admit_by_resources_and_backfill() {
        let queue = [
            job("a", Lane::Shared, 8.0),
            job("b", Lane::Shared, 20.0),
            job("c", Lane::Shared, 4.0),
        ];
        // 16 cores, load 2: a (8) fits, b (20) cannot, c (4) backfills.
        assert_eq!(
            admit(&queue, &[], &capacity(16.0, 2.0), false, true),
            vec!["a", "c"]
        );
        // Declared reservations count even before the load shows them.
        let running = [job("r", Lane::Shared, 12.0)];
        assert_eq!(
            admit(&queue, &running, &capacity(16.0, 0.0), false, true),
            vec!["c"]
        );
    }

    #[test]
    fn exclusive_waits_for_an_empty_quiet_cooled_rig_and_blocks_the_line() {
        let queue = [job("x", Lane::Exclusive, 1.0), job("s", Lane::Shared, 1.0)];
        let idle = capacity(16.0, 0.0);
        assert_eq!(admit(&queue, &[], &idle, true, true), vec!["x"]);
        assert!(
            admit(&queue, &[], &idle, false, true).is_empty(),
            "not quiet: nothing starts, not even the shared job behind it"
        );
        assert!(
            admit(&queue, &[], &idle, true, false).is_empty(),
            "still cooling down"
        );
        let running = [job("r", Lane::Shared, 1.0)];
        assert!(
            admit(&queue, &running, &idle, true, true).is_empty(),
            "waits for running jobs to drain"
        );
        // A shared job AHEAD of the exclusive one may still start.
        let queue = [job("s", Lane::Shared, 1.0), job("x", Lane::Exclusive, 1.0)];
        assert_eq!(admit(&queue, &[], &idle, true, true), vec!["s"]);
        // While an exclusive job runs, nothing else does.
        let running = [job("x0", Lane::Exclusive, 1.0)];
        assert!(admit(&[job("s", Lane::Shared, 1.0)], &running, &idle, true, true).is_empty());
    }

    #[test]
    fn gpu_jobs_need_free_gpu_memory() {
        let mut gpu_job = job("g", Lane::Shared, 1.0);
        gpu_job.needs.gpu_mem_gb = 10.0;
        let mut capacity = capacity(16.0, 0.0);
        assert!(
            !fits(&gpu_job.needs, &capacity, &Needs::default()),
            "no GPU"
        );
        capacity.gpu_free_gb = Some(20.0);
        assert!(fits(&gpu_job.needs, &capacity, &Needs::default()));
        let reserved = Needs {
            gpu_mem_gb: 15.0,
            ..Needs::default()
        };
        assert!(!fits(&gpu_job.needs, &capacity, &reserved));
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn job_lines_decode_from_pre_structured_probes() {
        // Probes from older builds carry only `id` and `text`.
        let line: JobLine = serde_json::from_str(
            r#"{"id":"j1","text":"j1e80 Exclusive 5 min · bench · for a@m3"}"#,
        )
        .unwrap();
        assert_eq!(line.title, None);
        assert_eq!(line.lane, None);
        assert_eq!(line.position, None);
        assert_eq!(line.minutes, None);
        assert_eq!(line.notify, None);
        assert_eq!(line.poisoned, 0);
        // Newer probes carry the structured fields alongside `text`.
        let line: JobLine = serde_json::from_str(
            r#"{"id":"j1","text":"j1e80 Exclusive 5 min · bench · for a@m3",
                "title":"bench","lane":"Exclusive","minutes":5,
                "notify":"a@m3","poisoned":2}"#,
        )
        .unwrap();
        assert_eq!(line.title.as_deref(), Some("bench"));
        assert_eq!(line.lane.as_deref(), Some("Exclusive"));
        assert_eq!(line.minutes, Some(5));
        assert_eq!(line.notify.as_deref(), Some("a@m3"));
        assert_eq!(line.poisoned, 2);
    }

    #[test]
    fn job_keys_dedupe_and_subscribe() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("amo").unwrap();
        let first = KeyClaim {
            key: "bench:riir-ai@abc:prefill".into(),
            rig: "bmo".into(),
            id: "j1".into(),
            notify: Some("a@m3".into()),
        };
        assert_eq!(room.claim_key(&first).unwrap(), KeyOutcome::Claimed);
        let second = KeyClaim {
            rig: "4090".into(),
            id: "j2".into(),
            notify: Some("b@amo".into()),
            ..first.clone()
        };
        assert_eq!(
            room.claim_key(&second).unwrap(),
            KeyOutcome::Duplicate {
                rig: "bmo".into(),
                id: "j1".into()
            }
        );
        let record: KeyRecord = read_json(
            &dir.path()
                .join("jobs/keys")
                .join(format!("{}.json", key_file_name(&first.key))),
        )
        .unwrap();
        assert_eq!(record.subscribers, vec!["b@amo".to_string()]);
        room.release_key(&first.key, "done").unwrap();
        assert_eq!(
            room.claim_key(&second).unwrap(),
            KeyOutcome::Claimed,
            "released keys are free again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn queue_runs_a_job_end_to_end_with_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("bmo").unwrap();
        let mut spec = job("01job000000000000000000000000001", Lane::Shared, 0.0);
        spec.argv = vec!["echo hello-from-job".into()];
        let queue = dir.path().join("jobs/queue");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(dir.path().join("jobs/running")).unwrap();
        write_json(&queue.join(format!("{}.json", spec.id)), &spec).unwrap();
        // Run the executor in-process instead of detached.
        fs::rename(
            queue.join(format!("{}.json", spec.id)),
            dir.path()
                .join("jobs/running")
                .join(format!("{}.starting", spec.id)),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("jobs/logs")).unwrap();
        room.execute_job(&spec.id).unwrap();
        let record: JobRecord = read_json(
            &dir.path()
                .join("jobs/done")
                .join(format!("{}.json", spec.id)),
        )
        .unwrap();
        assert_eq!(record.outcome.as_deref(), Some("done"));
        assert_eq!(record.exit_code, Some(0));
        assert!(
            room.job_log(&spec.id, 5)
                .unwrap()
                .contains("hello-from-job")
        );
    }

    fn baseline() -> Baseline {
        Baseline {
            plan: "031".into(),
            commit: "abc1234".into(),
            numbers: BTreeMap::from([
                (
                    "wall_ms".into(),
                    BaselineNumber {
                        value: 100.0,
                        lower_is_better: true,
                    },
                ),
                (
                    "tok_per_s".into(),
                    BaselineNumber {
                        value: 1000.0,
                        lower_is_better: false,
                    },
                ),
            ]),
            noise_floor: 0.05,
            source: ".benchmarks/031/baseline.json".into(),
            arm: None,
        }
    }

    #[test]
    fn verify_verdicts_follow_the_noise_floor_and_metric_directions() {
        // Both metrics gain 10% in their better direction → verified.
        let measured = BTreeMap::from([("wall_ms".into(), 90.0), ("tok_per_s".into(), 1100.0)]);
        assert_eq!(verify_against_baseline(&measured, &baseline()).verdict, "verified");
        // wall_ms regressed 10% (110 > 100) while tok_per_s gained →
        // refuted: a gain on one metric never buys a regression on another.
        let measured = BTreeMap::from([("wall_ms".into(), 110.0), ("tok_per_s".into(), 1100.0)]);
        assert_eq!(verify_against_baseline(&measured, &baseline()).verdict, "refuted");
        // Inside the 5% noise floor on both → inconclusive.
        let measured = BTreeMap::from([("wall_ms".into(), 97.0), ("tok_per_s".into(), 1030.0)]);
        assert_eq!(
            verify_against_baseline(&measured, &baseline()).verdict,
            "inconclusive"
        );
        // Both directions regress past the floor → refuted.
        let measured = BTreeMap::from([("wall_ms".into(), 120.0), ("tok_per_s".into(), 800.0)]);
        assert_eq!(verify_against_baseline(&measured, &baseline()).verdict, "refuted");
        // An undeclared metric is ignored; a declared-but-unmeasured one
        // marks the row `incomplete` (the claim is only as good as its full
        // cell — never countable as verified).
        let measured = BTreeMap::from([("wall_ms".into(), 90.0), ("extra".into(), 1.0)]);
        let outcome = verify_against_baseline(&measured, &baseline());
        assert_eq!(outcome.verdict, "incomplete", "tok_per_s missing");
        assert!(outcome.metrics.contains_key("wall_ms"));
        assert!(!outcome.metrics.contains_key("extra"));
    }

    #[test]
    fn result_files_parse_and_starve_missing_cells() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("results.json");
        std::fs::write(&path, r#"{"wall_ms": 87.5, "tok_per_s": 1042.0}"#).unwrap();
        let numbers = read_result_file(&Some(dir.path().to_path_buf()), "results.json");
        assert_eq!(numbers.as_ref().unwrap()["wall_ms"], 87.5);
        assert_eq!(numbers.as_ref().unwrap()["tok_per_s"], 1042.0);
        // Missing or malformed cells read as None → the run is `no-result`.
        assert!(read_result_file(&Some(dir.path().to_path_buf()), "absent.json").is_none());
        std::fs::write(&path, "not json").unwrap();
        assert!(read_result_file(&Some(dir.path().to_path_buf()), "results.json").is_none());
    }

    #[test]
    fn pick_rig_never_lands_a_verify_run_on_the_producing_rig() {
        let rigs = crate::Rigs {
            self_rig: "amo".into(),
            orchestrator: None,
            rigs: BTreeMap::from([
                ("amo".into(), crate::Rig::default()),
                ("bmo".into(), crate::Rig::default()),
            ]),
        };
        let probe = |quiet: bool| {
            serde_json::from_str::<rig::RigProbe>(&format!(
                r#"{{"rig":"amo","os":"macos","at":"","cores":8,"load1":0.0,
                    "quiet":{quiet},"agents":[],"contenders":[],"running":[],"queued":[]}}"#
            ))
            .unwrap()
        };
        let results = vec![
            ("amo".to_string(), Ok(probe(true)), Duration::ZERO),
            ("bmo".to_string(), Ok(probe(false)), Duration::ZERO),
        ];
        // Without exclusion the quiet producing rig wins; with it, the only
        // other rig takes the job even though it is not quiet (verified by
        // being the only candidate left).
        assert_eq!(
            pick_rig(&results, &rigs, &Needs::default(), Lane::Exclusive, &[]).unwrap(),
            "amo"
        );
        assert_eq!(
            pick_rig(
                &results,
                &rigs,
                &Needs::default(),
                Lane::Exclusive,
                &["amo".to_string()]
            )
            .unwrap(),
            "bmo"
        );
        // Excluding every candidate refuses the run outright.
        assert!(pick_rig(
            &results,
            &rigs,
            &Needs::default(),
            Lane::Exclusive,
            &["amo".to_string(), "bmo".to_string()]
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn verify_run_computes_ratios_and_writes_the_referee_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("bmo").unwrap();
        let cell = dir.path().join("cell");
        fs::create_dir_all(&cell).unwrap();
        // The measured cell: writes its result file, one metric 10% better,
        // one metric not declared in the results (blocked verified).
        std::fs::write(
            cell.join("measure.sh"),
            "echo '{\"wall_ms\": 90.0}' > results.json",
        )
        .unwrap();
        let mut spec = job("01job000000000000000000000000002", Lane::Exclusive, 0.0);
        spec.argv = vec!["sh measure.sh".into()];
        spec.cwd = Some(cell.display().to_string());
        spec.result_path = Some("results.json".into());
        spec.verify = Some(VerifySpec {
            origin_job: "01job_origin".into(),
            produced_on: "amo".into(),
            manifest_blake3: blake3::hash(b"manifest").to_string(),
            baseline: baseline(),
        });
        let queue = dir.path().join("jobs/queue");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(dir.path().join("jobs/running")).unwrap();
        write_json(&queue.join(format!("{}.json", spec.id)), &spec).unwrap();
        fs::rename(
            queue.join(format!("{}.json", spec.id)),
            dir.path()
                .join("jobs/running")
                .join(format!("{}.starting", spec.id)),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("jobs/logs")).unwrap();
        room.execute_job(&spec.id).unwrap();
        let record: JobRecord = read_json(
            &dir.path()
                .join("jobs/done")
                .join(format!("{}.json", spec.id)),
        )
        .unwrap();
        // wall_ms measured 90 vs 100 (10% better, past the 5% floor), but
        // tok_per_s never arrived → the row is `incomplete`, not countable.
        let outcome = record.verify_outcome.expect("verify outcome recorded");
        assert_eq!(outcome.verdict, "incomplete");
        assert_eq!(outcome.produced_on, "amo");
        assert_eq!(outcome.verified_on, "bmo");
        assert_eq!(record.measured.as_ref().unwrap()["wall_ms"], 90.0);
        // Provenance is complete here: rustc exists on this dev box.
        assert!(record.environment.as_ref().unwrap().complete());
        // The referee ledger row landed, parseable, with the verdict.
        let ledger = std::fs::read_to_string(dir.path().join("jobs/verify_ledger.jsonl")).unwrap();
        let row: VerifyLedgerRow = serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        assert_eq!(row.verified_on, "bmo");
        assert_eq!(row.produced_on, "amo");
        assert_eq!(row.verdict, "incomplete");
        assert_eq!(row.outcome, "done");
        assert_eq!(row.plan.as_deref(), Some("031"));
    }

    #[cfg(unix)]
    #[test]
    fn a_verify_run_without_its_result_file_is_no_result() {
        let dir = tempfile::tempdir().unwrap();
        let room = Mailroom::new(dir.path().to_path_buf());
        room.init("bmo").unwrap();
        let mut spec = job("01job000000000000000000000000003", Lane::Exclusive, 0.0);
        spec.argv = vec!["true".into()];
        spec.result_path = Some("results.json".into());
        spec.verify = Some(VerifySpec {
            origin_job: "01job_origin".into(),
            produced_on: "amo".into(),
            manifest_blake3: blake3::hash(b"manifest").to_string(),
            baseline: baseline(),
        });
        let queue = dir.path().join("jobs/queue");
        fs::create_dir_all(&queue).unwrap();
        fs::create_dir_all(dir.path().join("jobs/running")).unwrap();
        write_json(&queue.join(format!("{}.json", spec.id)), &spec).unwrap();
        fs::rename(
            queue.join(format!("{}.json", spec.id)),
            dir.path()
                .join("jobs/running")
                .join(format!("{}.starting", spec.id)),
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("jobs/logs")).unwrap();
        room.execute_job(&spec.id).unwrap();
        let record: JobRecord = read_json(
            &dir.path()
                .join("jobs/done")
                .join(format!("{}.json", spec.id)),
        )
        .unwrap();
        assert_eq!(
            record.outcome.as_deref(),
            Some("no-result"),
            "a clean exit without the measured cell does not count"
        );
        assert!(record.verify_outcome.is_none());
    }
}
