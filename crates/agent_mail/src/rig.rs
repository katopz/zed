//! `agent-mail rig status`: what every rig is busy with, in one command
//! (plan 047 P0). Each rig answers `rig probe` with one JSON line; the asking
//! rig fans out over SSH in parallel and renders the result.
//!
//! "Heavy" processes are classified exactly like riir-ai's benchmark quiet
//! gate (`scripts/perf_rematch.sh::classify_contenders`), reading its
//! vocabulary files at runtime so both tools share one definition of a
//! contender. A missing vocabulary makes the rig BLIND, never quiet.

use crate::{Mailroom, process_alive};
use anyhow::{Context as _, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Printed after the probe's JSON line; the SSH reader stops there.
pub const PROBE_END: &str = "agent-mail-probe-end";
const MAX_CWD_LOOKUPS: usize = 6;
const MAX_COMMAND_CHARS: usize = 120;
/// Where orchestrator escalations for the owner land (`owner@m3`).
pub const OWNER_INBOX: &str = "owner";
/// Same-named contenders at or above this count render as one summary line.
const COLLAPSE_AT: usize = 3;
/// Beyond three default intervals without a tick, orchestration is down.
const STALE_ORCHESTRATOR_MINUTES: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RigProbe {
    pub rig: String,
    pub os: String,
    pub at: String,
    pub cores: usize,
    #[serde(default)]
    pub load1: Option<f64>,
    #[serde(default)]
    pub mem_total_gb: Option<f64>,
    #[serde(default)]
    pub mem_free_gb: Option<f64>,
    #[serde(default)]
    pub disk_free_gb: Option<f64>,
    /// `ac`, `battery 64%`, or absent where there is no battery.
    #[serde(default)]
    pub power: Option<String>,
    /// macOS CPU speed limit; below 100 means thermally throttled.
    #[serde(default)]
    pub cpu_speed_limit: Option<u32>,
    #[serde(default)]
    pub gpus: Vec<Gpu>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub contenders: Vec<Contender>,
    /// `None` when the classifier could run; otherwise why the rig is blind.
    #[serde(default)]
    pub blind: Option<String>,
    pub quiet: bool,
    #[serde(default)]
    pub not_quiet_because: Vec<String>,
    #[serde(default)]
    pub outbox: Vec<(String, usize)>,
    /// HEAD of each tracked repo (`riir-*`, `seal-*`, `katgpt-rs`), for drift.
    #[serde(default)]
    pub repos: Vec<RepoHead>,
    /// Set on the orchestrator rig: when its watchdog last ran a full tick.
    #[serde(default)]
    pub watch_last_tick: Option<String>,
    /// Orchestrator escalations waiting for the owner on this rig.
    #[serde(default)]
    pub owner_mail: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoHead {
    pub name: String,
    pub branch: String,
    pub head: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gpu {
    pub name: String,
    pub util_pct: f64,
    pub mem_used_gb: f64,
    pub mem_total_gb: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub status: String,
    pub cwd: String,
    #[serde(default)]
    pub idle_minutes: Option<f64>,
    /// Last paragraph of its last message (capped), read from its transcript.
    #[serde(default)]
    pub last_paragraph: Option<String>,
    /// A scheduled agent-mail wake is pending for it.
    #[serde(default)]
    pub wake_scheduled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contender {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f64,
    pub rss_gb: f64,
    /// Which riir-ai arm fired: `name`, `gpu-class`, `resource`, `editor-hot`, `sustained`.
    pub arm: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// One row of the process table, normalized like `contender_candidates`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessRow {
    pub pid: u32,
    /// Binary basename; for interpreter-hosted runs, the script basename.
    pub name: String,
    /// Per-core percent (100 = one full core), as `ps` reports it.
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub cputime_min: f64,
    pub command: String,
}

/// riir-ai's thresholds, overridable by the same environment variables.
#[derive(Debug, Clone)]
pub struct Thresholds {
    pub cpu_pct: f64,
    pub rss_bytes: u64,
    pub editor_exempt_cpu_pct: f64,
    pub sustained_cpu_pct: f64,
    pub sustained_cputime_min: f64,
    pub gpu_class_cpu_pct: f64,
}

impl Thresholds {
    pub fn from_env() -> Self {
        let read = |name: &str, default: f64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        Self {
            cpu_pct: read("RIIR_CONTAM_CPU_PCT", 80.0),
            rss_bytes: (read("RIIR_CONTAM_RSS_GB", 2.0) * GIB) as u64,
            editor_exempt_cpu_pct: read("RIIR_EDITOR_EXEMPT_CPU_PCT", 25.0),
            sustained_cpu_pct: read("RIIR_CONTAM_SUSTAINED_CPU_PCT", 100.0),
            sustained_cputime_min: read("RIIR_CONTAM_CPUTIME_MIN", 10.0),
            gpu_class_cpu_pct: read("RIIR_GPU_CLASS_CPU_PCT", 5.0),
        }
    }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The three riir-ai vocabularies, each compiled to one anchored alternation.
pub struct Vocabulary {
    names: Regex,
    gpu: Regex,
    exempt: Option<Regex>,
}

impl Vocabulary {
    /// Same contract as riir-ai's loaders: an empty or missing name or GPU
    /// list is an error (a blind detector must not read as a quiet rig); an
    /// empty exemption list just means no exemptions.
    pub fn load(dir: &Path) -> Result<Self> {
        let names = load_alternation(&dir.join("contender_names.txt"))?
            .context("contender_names.txt is missing or empty")?;
        let gpu = load_alternation(&dir.join("contender_gpu_names.txt"))?
            .context("contender_gpu_names.txt is missing or empty")?;
        let exempt = load_alternation(&dir.join("contender_exempt_names.txt"))?;
        Ok(Self { names, gpu, exempt })
    }

    pub fn from_patterns(names: &str, gpu: &str, exempt: Option<&str>) -> Result<Self> {
        Ok(Self {
            names: anchored(names)?,
            gpu: anchored(gpu)?,
            exempt: exempt.map(anchored).transpose()?,
        })
    }
}

fn anchored(alternation: &str) -> Result<Regex> {
    Regex::new(&format!("^({alternation})$")).context("invalid contender pattern")
}

fn load_alternation(path: &Path) -> Result<Option<Regex>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    };
    let fragments: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    match fragments.is_empty() {
        true => Ok(None),
        false => anchored(&fragments.join("|")).map(Some),
    }
}

/// Port of `classify_contenders`: which arm (if any) makes `row` a contender.
/// Arms are checked in riir-ai's order and the first match wins.
pub fn classify(
    row: &ProcessRow,
    vocabulary: &Vocabulary,
    limits: &Thresholds,
) -> Option<&'static str> {
    if vocabulary.names.is_match(&row.name) {
        return Some("name");
    }
    if vocabulary.gpu.is_match(&row.name) && row.cpu_pct >= limits.gpu_class_cpu_pct {
        return Some("gpu-class");
    }
    if row.cpu_pct >= limits.cpu_pct && row.rss_bytes >= limits.rss_bytes {
        let exempt = vocabulary
            .exempt
            .as_ref()
            .is_some_and(|exempt| exempt.is_match(&row.name));
        return match (exempt, row.cpu_pct < limits.editor_exempt_cpu_pct) {
            (false, _) => Some("resource"),
            (true, true) => None,
            (true, false) => Some("editor-hot"),
        };
    }
    if row.cpu_pct >= limits.sustained_cpu_pct && row.cputime_min >= limits.sustained_cputime_min {
        return Some("sustained");
    }
    None
}

/// The identity riir-ai matches on: the first argv token after wrapper
/// commands and `VAR=` assignments, by basename; for an interpreter, the
/// script it runs.
pub fn process_identity(command: &str) -> Option<String> {
    let mut tokens = command.split_whitespace().skip_while(|token| {
        matches!(
            *token,
            "nohup" | "nice" | "taskset" | "env" | "time" | "timeout" | "sudo" | "stdbuf" | "exec"
        ) || is_assignment(token)
    });
    let binary = basename(tokens.next()?);
    let is_interpreter = matches!(binary, "python" | "python3" | "Python" | "uv" | "uvx")
        || binary
            .strip_prefix("python3.")
            .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|b| b.is_ascii_digit()));
    match (is_interpreter, tokens.next()) {
        (true, Some(script)) => Some(basename(script).to_string()),
        _ => Some(binary.to_string()),
    }
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// `ps` TIME (`[dd-][hh:]mm:ss[.xx]`) in whole minutes of CPU.
pub fn cputime_minutes(time: &str) -> f64 {
    let (days, clock) = match time.split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().unwrap_or(0.0), clock),
        None => (0.0, time),
    };
    let parts: Vec<f64> = clock
        .split(':')
        .map(|part| part.parse::<f64>().unwrap_or(0.0))
        .collect();
    let minutes = match parts.as_slice() {
        [hours, minutes, _] => hours * 60.0 + minutes,
        [minutes, _] => *minutes,
        _ => 0.0,
    };
    days * 24.0 * 60.0 + minutes
}

/// Parses `ps -axo pid=,pcpu=,rss=,time=,args=` (RSS in KiB).
pub fn parse_ps(output: &str) -> Vec<ProcessRow> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let cpu_pct = fields.next()?.parse().ok()?;
            let rss_kib: u64 = fields.next()?.parse().ok()?;
            let time = fields.next()?;
            let command = fields.collect::<Vec<_>>().join(" ");
            Some(ProcessRow {
                pid,
                name: process_identity(&command)?,
                cpu_pct,
                rss_bytes: rss_kib * 1024,
                cputime_min: cputime_minutes(time),
                command,
            })
        })
        .collect()
}

// agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
#[allow(clippy::disallowed_methods)]
fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn vocabulary_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGENT_MAIL_CONTENDER_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    [
        PathBuf::from(&home).join("git/riir-ai/scripts"),
        PathBuf::from("E:/git/riir-ai/scripts"),
    ]
    .into_iter()
    .find(|dir| dir.join("contender_names.txt").exists())
}

/// Probes this rig.
pub fn probe(mailroom: &Mailroom) -> RigProbe {
    let rig = mailroom
        .rigs()
        .map(|rigs| rigs.self_rig)
        .unwrap_or_else(|_| "unknown".into());
    let processes = process_table();
    let (contenders, blind) = match vocabulary_dir().map(|dir| Vocabulary::load(&dir)) {
        Some(Ok(vocabulary)) => {
            let limits = Thresholds::from_env();
            let mut contenders: Vec<Contender> = processes
                .rows
                .iter()
                .filter(|row| row.pid != std::process::id())
                .filter_map(|row| {
                    let arm = classify(row, &vocabulary, &limits)?;
                    Some(Contender {
                        pid: row.pid,
                        name: row.name.clone(),
                        cpu_pct: row.cpu_pct,
                        rss_gb: row.rss_bytes as f64 / GIB,
                        arm: arm.to_string(),
                        command: (!row.command.is_empty())
                            .then(|| truncate_chars(&row.command, MAX_COMMAND_CHARS)),
                        cwd: None,
                    })
                })
                .collect();
            contenders.sort_by(|a, b| b.cpu_pct.total_cmp(&a.cpu_pct));
            for contender in contenders.iter_mut().take(MAX_CWD_LOOKUPS) {
                contender.cwd = process_cwd(contender.pid);
            }
            (contenders, processes.blind)
        }
        Some(Err(error)) => (Vec::new(), Some(format!("{error:#}"))),
        None => (
            Vec::new(),
            Some("riir-ai contender vocabulary not found (set AGENT_MAIL_CONTENDER_DIR)".into()),
        ),
    };
    let (power, cpu_speed_limit) = power_and_thermal();
    let mut not_quiet_because = Vec::new();
    if let Some(reason) = &blind {
        not_quiet_because.push(format!("blind: {reason}"));
    }
    if !contenders.is_empty() {
        not_quiet_because.push(format!("{} contender(s)", contenders.len()));
    }
    if power
        .as_deref()
        .is_some_and(|power| power.starts_with("battery"))
    {
        not_quiet_because.push("on battery".into());
    }
    if cpu_speed_limit.is_some_and(|limit| limit < 100) {
        not_quiet_because.push(format!("throttled to {}%", cpu_speed_limit.unwrap_or(0)));
    }
    RigProbe {
        rig,
        os: std::env::consts::OS.into(),
        at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        cores: std::thread::available_parallelism().map_or(0, |cores| cores.get()),
        load1: load_average(),
        mem_total_gb: processes.mem_total_gb.or_else(memory_total_gb),
        mem_free_gb: processes.mem_free_gb.or_else(memory_free_gb),
        disk_free_gb: processes.disk_free_gb.or_else(disk_free_gb),
        power,
        cpu_speed_limit,
        gpus: gpus(),
        agents: claude_agents(mailroom),
        repos: repo_heads(),
        owner_mail: mailroom
            .peek(&[OWNER_INBOX.to_string()])
            .map_or(0, |mail| mail.len()),
        watch_last_tick: mailroom
            .rigs()
            .ok()
            .filter(|rigs| rigs.orchestrator.as_deref() == Some(rigs.self_rig.as_str()))
            .and_then(|_| mailroom.watch_last_tick())
            .map(|at| at.to_rfc3339()),
        quiet: not_quiet_because.is_empty(),
        not_quiet_because,
        contenders,
        blind,
        outbox: mailroom
            .outbox_status()
            .into_iter()
            .map(|(rig, count, _)| (rig, count))
            .collect(),
    }
}

#[derive(Default)]
struct ProcessTable {
    rows: Vec<ProcessRow>,
    blind: Option<String>,
    mem_total_gb: Option<f64>,
    mem_free_gb: Option<f64>,
    disk_free_gb: Option<f64>,
}

#[cfg(unix)]
fn process_table() -> ProcessTable {
    match run("ps", &["-axo", "pid=,pcpu=,rss=,time=,args="]) {
        Some(output) => ProcessTable {
            rows: parse_ps(&output),
            ..ProcessTable::default()
        },
        None => ProcessTable {
            blind: Some("ps failed".into()),
            ..ProcessTable::default()
        },
    }
}

/// `ps -W` (riir-ai's Windows source) has no CPU column, so its resource and
/// sustained arms are blind there; two native samples give per-core CPU% and
/// close that gap (PowerShell `Get-Process` + WMI took ~3 s per probe).
#[cfg(windows)]
fn process_table() -> ProcessTable {
    use sysinfo::{Disks, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let kind = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_cmd(UpdateKind::OnlyIfNotSet)
        .with_exe(UpdateKind::OnlyIfNotSet);
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(300)));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    system.refresh_memory();
    let rows = system
        .processes()
        .values()
        .map(|process| {
            let command = process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ");
            let name = process_identity(&command)
                .unwrap_or_else(|| process.name().to_string_lossy().into_owned());
            ProcessRow {
                pid: process.pid().as_u32(),
                name: name
                    .strip_suffix(".exe")
                    .map(str::to_string)
                    .unwrap_or(name),
                cpu_pct: f64::from(process.cpu_usage()),
                rss_bytes: process.memory(),
                cputime_min: process.accumulated_cpu_time() as f64 / 60_000.0,
                command,
            }
        })
        .collect();
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let disk_free_gb = Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|disk| {
            home.as_ref()
                .is_some_and(|home| home.starts_with(disk.mount_point()))
        })
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
        .map(|disk| disk.available_space() as f64 / GIB);
    ProcessTable {
        rows,
        blind: None,
        mem_total_gb: Some(system.total_memory() as f64 / GIB),
        mem_free_gb: Some(system.available_memory() as f64 / GIB),
        disk_free_gb,
    }
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<String> {
    let output = run("lsof", &["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])?;
    output
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .map(str::to_string)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn process_cwd(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.display().to_string())
}

#[cfg(windows)]
fn process_cwd(_pid: u32) -> Option<String> {
    None
}

fn load_average() -> Option<f64> {
    if cfg!(target_os = "macos") {
        let text = run("sysctl", &["-n", "vm.loadavg"])?;
        return text
            .trim_matches(|c: char| c == '{' || c.is_whitespace())
            .split_whitespace()
            .next()?
            .parse()
            .ok();
    }
    fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn memory_total_gb() -> Option<f64> {
    if cfg!(target_os = "macos") {
        let bytes: f64 = run("sysctl", &["-n", "hw.memsize"])?.trim().parse().ok()?;
        return Some(bytes / GIB);
    }
    meminfo_kib("MemTotal:").map(|kib| kib * 1024.0 / GIB)
}

fn memory_free_gb() -> Option<f64> {
    if cfg!(target_os = "macos") {
        let text = run("vm_stat", &[])?;
        let page_size: f64 = text
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let pages = |label: &str| -> f64 {
            text.lines()
                .find(|line| line.starts_with(label))
                .and_then(|line| line.rsplit(':').next())
                .and_then(|value| value.trim().trim_end_matches('.').parse().ok())
                .unwrap_or(0.0)
        };
        let free = pages("Pages free") + pages("Pages inactive") + pages("Pages speculative");
        return Some(free * page_size / GIB);
    }
    meminfo_kib("MemAvailable:").map(|kib| kib * 1024.0 / GIB)
}

fn meminfo_kib(label: &str) -> Option<f64> {
    fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find(|line| line.starts_with(label))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn disk_free_gb() -> Option<f64> {
    let home = std::env::var("HOME").ok()?;
    let output = run("df", &["-k", &home])?;
    let available_kib: f64 = output
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()?;
    Some(available_kib * 1024.0 / GIB)
}

fn power_and_thermal() -> (Option<String>, Option<u32>) {
    if !cfg!(target_os = "macos") {
        return (None, None);
    }
    let power = run("pmset", &["-g", "batt"]).and_then(|text| {
        let source = match text.contains("'Battery Power'") {
            true => "battery",
            false if text.contains("'AC Power'") => "ac",
            false => return None,
        };
        let percent = text
            .split('\t')
            .nth(1)
            .and_then(|rest| rest.split('%').next())
            .map(|value| value.trim().to_string());
        Some(match (source, percent) {
            ("battery", Some(percent)) => format!("battery {percent}%"),
            (source, _) => source.to_string(),
        })
    });
    let speed_limit = run("pmset", &["-g", "therm"]).and_then(|text| {
        text.lines()
            .find(|line| line.contains("CPU_Speed_Limit"))
            .and_then(|line| line.split('=').nth(1))
            .and_then(|value| value.trim().parse().ok())
    });
    (power, speed_limit)
}

fn gpus() -> Vec<Gpu> {
    let Some(output) = run(
        "nvidia-smi",
        &[
            "--query-gpu=name,utilization.gpu,memory.used,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            let [name, util, used, total] = fields.as_slice() else {
                return None;
            };
            Some(Gpu {
                name: name.to_string(),
                util_pct: util.parse().ok()?,
                mem_used_gb: used.parse::<f64>().ok()? / 1024.0,
                mem_total_gb: total.parse::<f64>().ok()? / 1024.0,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct SessionFile {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    name: Option<String>,
    status: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "statusUpdatedAt")]
    status_updated_at: Option<i64>,
}

fn claude_agents(mailroom: &Mailroom) -> Vec<Agent> {
    let Some(home) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".claude"))
        })
    else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(home.join("sessions")) else {
        return Vec::new();
    };
    let mut agents: Vec<Agent> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            serde_json::from_slice::<SessionFile>(&fs::read(entry.path()).ok()?).ok()
        })
        .filter(|session| process_alive(session.pid))
        .map(|session| {
            let name = session
                .name
                .unwrap_or_else(|| format!("pid {}", session.pid));
            let cwd = session.cwd.unwrap_or_default();
            let last_paragraph = session.session_id.as_deref().and_then(|id| {
                crate::watch::last_paragraph(&crate::watch::transcript_path(&home, &cwd, id))
            });
            Agent {
                wake_scheduled: mailroom
                    .scheduled(std::slice::from_ref(&name))
                    .is_ok_and(|mail| !mail.is_empty()),
                idle_minutes: session.status_updated_at.map(|at| {
                    (chrono::Utc::now().timestamp_millis() - at).max(0) as f64 / 60_000.0
                }),
                status: session.status.unwrap_or_else(|| "?".into()),
                last_paragraph,
                name,
                cwd,
            }
        })
        .collect();
    agents.sort_by(|a, b| a.name.cmp(&b.name));
    agents
}

/// Owner-chosen drift set (plan 048): `riir-*`, `seal-*`, `katgpt-rs`.
fn tracked_repo(name: &str) -> bool {
    name.starts_with("riir-") || name.starts_with("seal-") || name == "katgpt-rs"
}

/// Reads HEADs straight from `.git` (no `git` process per repo). Worktrees
/// (`.git` is a file) are skipped: they are per-agent scratch, not drift.
fn repo_heads() -> Vec<RepoHead> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let roots = home
        .map(|home| PathBuf::from(home).join("git"))
        .into_iter()
        .chain([PathBuf::from("E:/git")]);
    let mut heads = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !tracked_repo(&name) {
                continue;
            }
            let git = entry.path().join(".git");
            if !git.is_dir() {
                continue;
            }
            if let Some((branch, head)) = read_head(&git) {
                heads.push(RepoHead { name, branch, head });
            }
        }
    }
    heads.sort_by(|a, b| a.name.cmp(&b.name));
    heads
}

fn read_head(git: &Path) -> Option<(String, String)> {
    let head = fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref: ") else {
        return Some(("HEAD".into(), head.to_string()));
    };
    let branch = reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .to_string();
    let loose = fs::read_to_string(git.join(reference))
        .ok()
        .map(|sha| sha.trim().to_string());
    let sha = loose.or_else(|| {
        fs::read_to_string(git.join("packed-refs"))
            .ok()?
            .lines()
            .find_map(|line| {
                let (sha, name) = line.split_once(' ')?;
                (name == reference).then(|| sha.to_string())
            })
    })?;
    Some((branch, sha))
}

/// Probes every rig in parallel: this one directly, the rest over SSH.
pub fn status(
    mailroom: &Mailroom,
    only: &[String],
) -> Result<Vec<(String, Result<RigProbe>, Duration)>> {
    let rigs = mailroom.rigs()?;
    for name in only {
        if *name != rigs.self_rig && !rigs.rigs.contains_key(name) {
            bail!("unknown rig {name:?}");
        }
    }
    let mut names: Vec<String> = rigs.rigs.keys().cloned().collect();
    if !names.contains(&rigs.self_rig) {
        names.push(rigs.self_rig.clone());
    }
    names.retain(|name| only.is_empty() || only.contains(name));
    let handles: Vec<_> = names
        .into_iter()
        .map(|name| {
            let home = mailroom.home().to_path_buf();
            let rigs = rigs.clone();
            std::thread::spawn(move || {
                let mailroom = Mailroom::new(home);
                let started = Instant::now();
                let outcome = match name == rigs.self_rig {
                    true => Ok(probe(&mailroom)),
                    false => probe_remote(&mailroom, &rigs, &name),
                };
                (name, outcome, started.elapsed())
            })
        })
        .collect();
    Ok(handles
        .into_iter()
        .filter_map(|handle| handle.join().ok())
        .collect())
}

fn probe_remote(mailroom: &Mailroom, rigs: &crate::Rigs, name: &str) -> Result<RigProbe> {
    let rig = rigs.rigs.get(name).context("unknown rig")?;
    let command = format!("{} rig probe", rig.bin.as_deref().unwrap_or("agent-mail"));
    let mut failures = Vec::new();
    for target in &rig.ssh {
        match mailroom.ssh(target, &command, PROBE_END) {
            Ok(lines) => {
                let json = lines
                    .iter()
                    .rev()
                    .find(|line| line.trim_start().starts_with('{'))
                    .context("probe printed no JSON (agent-mail too old there?)")?;
                return serde_json::from_str(json).context("unparseable probe");
            }
            Err(error) => failures.push(format!("{target}: {error:#}")),
        }
    }
    bail!("offline: {}", failures.join("; "))
}

fn orchestrator_line(last_tick: Option<&str>) -> String {
    let last = last_tick.and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
    match last {
        Some(at) => {
            let age = chrono::Utc::now().signed_duration_since(at).num_minutes();
            match age > STALE_ORCHESTRATOR_MINUTES {
                true => format!("STALE: last watchdog tick {age} min ago"),
                false => format!("last watchdog tick {age} min ago"),
            }
        }
        None => "STALE: the watchdog has never run".into(),
    }
}

/// Human-readable report, one block per rig.
pub fn render(results: &[(String, Result<RigProbe>, Duration)], rigs: &crate::Rigs) -> String {
    let mut out = String::new();
    let mut sorted: Vec<&(String, Result<RigProbe>, Duration)> = results.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, outcome, elapsed) in sorted {
        let rig = rigs.rigs.get(name);
        let role = rig.and_then(|rig| rig.role.clone()).unwrap_or_default();
        let jobs = match rig.is_none_or(|rig| rig.accepts_jobs) {
            true => "",
            false => " · no jobs",
        };
        let probe = match outcome {
            Ok(probe) => probe,
            Err(error) => {
                out.push_str(&format!(
                    "{name:<5} {role}{jobs} · OFFLINE ({} ms): {}\n",
                    elapsed.as_millis(),
                    truncate_chars(&format!("{error:#}"), 160)
                ));
                continue;
            }
        };
        let quiet = match probe.quiet {
            true => "QUIET".to_string(),
            false => format!("BUSY ({})", probe.not_quiet_because.join(", ")),
        };
        let mut facts = Vec::new();
        if let Some(load) = probe.load1 {
            facts.push(format!("load {load:.1}/{}", probe.cores));
        } else {
            facts.push(format!("{} cores", probe.cores));
        }
        if let (Some(free), Some(total)) = (probe.mem_free_gb, probe.mem_total_gb) {
            facts.push(format!("mem {free:.0}/{total:.0} GB free"));
        }
        if let Some(disk) = probe.disk_free_gb {
            facts.push(format!("disk {disk:.0} GB free"));
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
        out.push_str(&format!(
            "{name:<5} {role}{jobs} · {quiet} · {} · {} ms\n",
            facts.join(" · "),
            elapsed.as_millis()
        ));
        // Many same-named workers (a build's rustc fan-out) collapse into one
        // line so the command that owns them stays visible.
        let mut groups: Vec<(&str, Vec<&Contender>)> = Vec::new();
        for contender in &probe.contenders {
            match groups.iter_mut().find(|(name, _)| *name == contender.name) {
                Some((_, members)) => members.push(contender),
                None => groups.push((&contender.name, vec![contender])),
            }
        }
        for (name, members) in groups {
            if members.len() >= COLLAPSE_AT {
                let cpu: f64 = members.iter().map(|member| member.cpu_pct).sum();
                let rss: f64 = members.iter().map(|member| member.rss_gb).sum();
                out.push_str(&format!(
                    "      heavy  {name} ×{} ({cpu:.0}% cpu, {rss:.1} GB total)\n",
                    members.len()
                ));
                continue;
            }
            for contender in members {
                out.push_str(&format!(
                    "      heavy  {} (pid {}, {:.0}% cpu, {:.1} GB, {}){}{}\n",
                    contender.name,
                    contender.pid,
                    contender.cpu_pct,
                    contender.rss_gb,
                    contender.arm,
                    contender
                        .cwd
                        .as_deref()
                        .map(|cwd| format!(" in {cwd}"))
                        .unwrap_or_default(),
                    contender
                        .command
                        .as_deref()
                        .map(|command| format!(" `{command}`"))
                        .unwrap_or_default(),
                ));
            }
        }
        for agent in &probe.agents {
            out.push_str(&format!(
                "      agent  {} [{}] {}\n",
                agent.name, agent.status, agent.cwd
            ));
        }
        for (target, count) in &probe.outbox {
            out.push_str(&format!(
                "      outbox {count} message(s) waiting for {target}\n"
            ));
        }
        if probe.owner_mail > 0 {
            out.push_str(&format!(
                "      owner  {} escalation(s) waiting: `agent-mail recv --as {OWNER_INBOX}`\n",
                probe.owner_mail
            ));
        }
        if rigs.orchestrator.as_deref() == Some(name.as_str()) {
            out.push_str(&format!(
                "      orchestrator {}\n",
                orchestrator_line(probe.watch_last_tick.as_deref())
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, cpu_pct: f64, rss_gb: f64, cputime_min: f64) -> ProcessRow {
        ProcessRow {
            pid: 1,
            name: name.into(),
            cpu_pct,
            rss_bytes: (rss_gb * GIB) as u64,
            cputime_min,
            command: String::new(),
        }
    }

    fn vocabulary() -> Vocabulary {
        Vocabulary::from_patterns(
            "cargo|rustc|bench_.*",
            "plan545_.*|llama-server",
            Some("zed|Zed"),
        )
        .unwrap()
    }

    #[test]
    fn classifies_like_riir_ai_perf_rematch() {
        let (vocab, limits) = (vocabulary(), Thresholds::from_env());
        // NAME arm fires at any load.
        assert_eq!(
            classify(&row("cargo", 0.0, 0.0, 0.0), &vocab, &limits),
            Some("name")
        );
        assert_eq!(
            classify(&row("bench_prefill", 1.0, 0.1, 0.0), &vocab, &limits),
            Some("name")
        );
        // GPU-class arm: name-gated at >= 5% cpu.
        assert_eq!(
            classify(&row("plan545_x", 25.7, 1.0, 0.0), &vocab, &limits),
            Some("gpu-class")
        );
        assert_eq!(
            classify(&row("llama-server", 1.0, 9.0, 0.0), &vocab, &limits),
            None
        );
        // RESOURCE arm needs both cpu and memory.
        assert_eq!(
            classify(&row("mystery", 90.0, 3.0, 0.0), &vocab, &limits),
            Some("resource")
        );
        assert_eq!(
            classify(&row("mystery", 90.0, 1.0, 0.0), &vocab, &limits),
            None
        );
        // Editor exemption only under its own bound; the resource arm is
        // reached only at >= 80% cpu, so an exempt editor there is editor-hot.
        assert_eq!(
            classify(&row("zed", 184.5, 3.0, 0.0), &vocab, &limits),
            Some("editor-hot")
        );
        // SUSTAINED tier: >= 100% for >= 10 cpu-minutes, any RSS.
        assert_eq!(
            classify(
                &row("qemu-system-xtensa", 169.0, 0.0, 1334.0),
                &vocab,
                &limits
            ),
            Some("sustained")
        );
        assert_eq!(
            classify(&row("qemu-system-xtensa", 169.0, 0.0, 2.0), &vocab, &limits),
            None
        );
    }

    #[test]
    fn identity_skips_wrappers_and_names_interpreted_scripts() {
        assert_eq!(
            process_identity("env RIIR_X=1 nice ./driver --fast").as_deref(),
            Some("driver")
        );
        assert_eq!(
            process_identity("nohup /usr/bin/cargo build").as_deref(),
            Some("cargo")
        );
        assert_eq!(
            process_identity("A=1 B=2 ./bench_prefill").as_deref(),
            Some("bench_prefill")
        );
        assert_eq!(
            process_identity("/opt/homebrew/bin/python3.12 scripts/rapid-mlx.py --x").as_deref(),
            Some("rapid-mlx.py")
        );
        assert_eq!(process_identity("uv run x").as_deref(), Some("run"));
        assert_eq!(
            process_identity("C:\\tools\\bench.exe").as_deref(),
            Some("bench.exe")
        );
    }

    #[test]
    fn parses_ps_rows_and_cpu_time() {
        let rows = parse_ps(
            "  101  230.5  4194304  12:34.56 /usr/bin/cargo build --release\n\
             202   0.0     1024   1-02:03:04 /sbin/launchd\n\
             garbage line\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "cargo");
        assert_eq!(rows[0].rss_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(rows[0].cputime_min, 12.0);
        assert_eq!(rows[1].cputime_min, 24.0 * 60.0 + 2.0 * 60.0 + 3.0);
    }

    #[test]
    fn missing_vocabulary_is_blind_not_quiet() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Vocabulary::load(dir.path()).is_err());
        fs::write(
            dir.path().join("contender_names.txt"),
            "# only comments\n\n",
        )
        .unwrap();
        fs::write(dir.path().join("contender_gpu_names.txt"), "gpu\n").unwrap();
        assert!(Vocabulary::load(dir.path()).is_err());
        fs::write(dir.path().join("contender_names.txt"), "cargo\n").unwrap();
        assert!(
            Vocabulary::load(dir.path()).is_ok(),
            "the exemption list is optional"
        );
    }
}
