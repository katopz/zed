pub use gpui::GpuSpecs;
use gpui::{App, AppContext as _, Task, Window, actions};
use human_bytes::human_bytes;
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use semver::Version;
use serde::Serialize;
use std::{env, fmt::Display};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};

actions!(
    zed,
    [
        /// Copies system specifications to the clipboard for bug reports.
        CopySystemSpecsIntoClipboard,
    ]
);

#[derive(Clone, Debug, Serialize)]
pub struct SystemSpecs {
    app_version: String,
    release_channel: &'static str,
    os_name: String,
    os_version: String,
    memory: u64,
    architecture: &'static str,
    commit_sha: Option<String>,
    bundle_type: Option<String>,
    gpu_specs: Option<String>,
}

impl SystemSpecs {
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        os_name: String,
        os_version: String,
    ) -> Task<Self> {
        let app_version = AppVersion::global(cx).to_string();
        let release_channel = ReleaseChannel::global(cx);
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
        );
        let memory = system.total_memory();
        let architecture = env::consts::ARCH;
        let commit_sha = match release_channel {
            ReleaseChannel::Dev | ReleaseChannel::Nightly => {
                AppCommitSha::try_global(cx).map(|sha| sha.full())
            }
            _ => None,
        };
        let bundle_type = bundle_type();

        let gpu_specs = window.gpu_specs().map(|specs| {
            format!(
                "{} || {} || {}",
                specs.device_name, specs.driver_name, specs.driver_info
            )
        });

        cx.background_spawn(async move {
            SystemSpecs {
                app_version,
                release_channel: release_channel.display_name(),
                bundle_type,
                os_name,
                os_version,
                memory,
                architecture,
                commit_sha,
                gpu_specs,
            }
        })
    }

    pub fn new_stateless(
        app_version: Version,
        app_commit_sha: Option<AppCommitSha>,
        release_channel: ReleaseChannel,
        os_name: String,
        os_version: String,
    ) -> Self {
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::everything()),
        );
        let memory = system.total_memory();
        let architecture = env::consts::ARCH;
        let commit_sha = match release_channel {
            ReleaseChannel::Dev | ReleaseChannel::Nightly => app_commit_sha.map(|sha| sha.full()),
            _ => None,
        };
        let bundle_type = bundle_type();

        Self {
            app_version: app_version.to_string(),
            release_channel: release_channel.display_name(),
            os_name,
            os_version,
            memory,
            architecture,
            commit_sha,
            bundle_type,
            gpu_specs: try_determine_available_gpus(),
        }
    }
}

impl Display for SystemSpecs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let os_information = format!("OS: {} {}", self.os_name, self.os_version);
        let app_version_information = format!(
            "Zed: v{} ({}) {}{}",
            self.app_version,
            match &self.commit_sha {
                Some(commit_sha) => format!("{} {}", self.release_channel, commit_sha),
                None => self.release_channel.to_string(),
            },
            if let Some(bundle_type) = &self.bundle_type {
                format!("({bundle_type})")
            } else {
                "".to_string()
            },
            if cfg!(debug_assertions) {
                "(Taylor's Version)"
            } else {
                ""
            },
        );
        let system_specs = [
            app_version_information,
            os_information,
            format!("Memory: {}", human_bytes(self.memory as f64)),
            format!("Architecture: {}", self.architecture),
        ]
        .into_iter()
        .chain(
            self.gpu_specs
                .as_ref()
                .map(|specs| format!("GPU: {}", specs)),
        )
        .collect::<Vec<String>>()
        .join("\n");

        write!(f, "{system_specs}")
    }
}

fn try_determine_available_gpus() -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        #[allow(
            clippy::disallowed_methods,
            reason = "we are not running in an executor"
        )]
        std::process::Command::new("vulkaninfo")
            .args(&["--summary"])
            .output()
            .ok()
            .map(|output| {
                [
                    "<details><summary>`vulkaninfo --summary` output</summary>",
                    "",
                    "```",
                    String::from_utf8_lossy(&output.stdout).as_ref(),
                    "```",
                    "</details>",
                ]
                .join("\n")
            })
            .or(Some("Failed to run `vulkaninfo --summary`".to_string()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        None
    }
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, Clone)]
pub struct GpuInfo {
    pub device_name: Option<String>,
    pub device_pci_id: u16,
    pub vendor_name: Option<String>,
    pub vendor_pci_id: u16,
    pub driver_version: Option<String>,
    pub driver_name: Option<String>,
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn read_gpu_info_from_sys_class_drm() -> anyhow::Result<Vec<GpuInfo>> {
    use anyhow::Context as _;
    use pciid_parser;
    let dir_iter = std::fs::read_dir("/sys/class/drm").context("Failed to read /sys/class/drm")?;
    let mut pci_addresses = vec![];
    let mut gpus = Vec::<GpuInfo>::new();
    let pci_db = pciid_parser::Database::read().ok();
    for entry in dir_iter {
        let Ok(entry) = entry else {
            continue;
        };

        let device_path = entry.path().join("device");
        let Some(pci_address) = device_path.read_link().ok().and_then(|pci_address| {
            pci_address
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::trim)
                .map(str::to_string)
        }) else {
            continue;
        };
        let Ok(device_pci_id) = read_pci_id_from_path(device_path.join("device")) else {
            continue;
        };
        let Ok(vendor_pci_id) = read_pci_id_from_path(device_path.join("vendor")) else {
            continue;
        };
        let driver_name = std::fs::read_link(device_path.join("driver"))
            .ok()
            .and_then(|driver_link| {
                driver_link
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .map(str::trim)
                    .map(str::to_string)
            });
        let driver_version = driver_name
            .as_ref()
            .and_then(|driver_name| {
                std::fs::read_to_string(format!("/sys/module/{driver_name}/version")).ok()
            })
            .as_deref()
            .map(str::trim)
            .map(str::to_string);

        let already_found = gpus
            .iter()
            .zip(&pci_addresses)
            .any(|(gpu, gpu_pci_address)| {
                gpu_pci_address == &pci_address
                    && gpu.driver_version == driver_version
                    && gpu.driver_name == driver_name
            });

        if already_found {
            continue;
        }

        let vendor = pci_db
            .as_ref()
            .and_then(|db| db.vendors.get(&vendor_pci_id));
        let vendor_name = vendor.map(|vendor| vendor.name.clone());
        let device_name = vendor
            .and_then(|vendor| vendor.devices.get(&device_pci_id))
            .map(|device| device.name.clone());

        gpus.push(GpuInfo {
            device_name,
            device_pci_id,
            vendor_name,
            vendor_pci_id,
            driver_version,
            driver_name,
        });
        pci_addresses.push(pci_address);
    }

    Ok(gpus)
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn read_pci_id_from_path(path: impl AsRef<std::path::Path>) -> anyhow::Result<u16> {
    use anyhow::Context as _;
    let id = std::fs::read_to_string(path)?;
    let id = id
        .trim()
        .strip_prefix("0x")
        .context("Not a device ID")
        .context(id.clone())?;
    anyhow::ensure!(
        id.len() == 4,
        "Not a device id, expected 4 digits, found {}",
        id.len()
    );
    u16::from_str_radix(id, 16).context("Failed to parse device ID")
}

/// Returns value of `ZED_BUNDLE_TYPE` set at compiletime or else at runtime.
///
/// The compiletime value is used by flatpak since it doesn't seem to have a way to provide a
/// runtime value.
/// The runtime value is used by snap since the Zed snaps use release binaries directly, so they
/// cannot have this baked in.
fn bundle_type() -> Option<String> {
    option_env!("ZED_BUNDLE_TYPE")
        .map(|bundle_type| bundle_type.to_string())
        .or_else(|| env::var("ZED_BUNDLE_TYPE").ok())
}

// ─── Live machine context for auto_prompt continuation prompts ────
//
// auto_prompt stamps a one-line machine snapshot (CPU/RAM load, power state,
// GPU) onto every continuation prompt so the worker agent can make
// resource-aware decisions without spending tool calls probing the machine.
//
// CPU usage requires two `refresh_cpu_usage` calls one
// `MINIMUM_CPU_UPDATE_INTERVAL` apart, which must not run on the main thread.
// Samples are therefore taken by a background task (spawned by auto_prompt at
// dispatch time) and published into `LIVE_MACHINE`; prompt building only ever
// reads the latest cached snapshot and never blocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

/// Latest machine sample published by [`sample_live_machine`].
#[derive(Clone, Debug)]
pub struct LiveMachine {
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub cpu_brand: Option<String>,
    pub physical_cores: Option<usize>,
    pub cpu_usage_percent: f32,
    pub ram_used_bytes: u64,
    pub ram_total_bytes: u64,
    /// Preformatted, e.g. `AC plugged (battery 98%, charging)`.
    pub power: Option<String>,
}

static LIVE_MACHINE: RwLock<Option<Arc<LiveMachine>>> = RwLock::new(None);
static SAMPLING: AtomicBool = AtomicBool::new(false);

/// Interval between periodic machine samples. Continuation prompts read the
/// latest sample, so this bounds how stale "CPU 42%" can be.
const PERIODIC_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// The power probe shells out (`pmset` on macOS) — throttle it to once per
/// minute; the battery/AC state changes slowly.
const POWER_PROBE_EVERY_N_SAMPLES: u64 = 4;

static PERIODIC_SAMPLER_STARTED: AtomicBool = AtomicBool::new(false);

/// Spawn the once-per-process periodic machine sampler on the background
/// executor. Call from app init; later calls are no-ops. Keeps
/// [`machine_context_line`] fed so every continuation prompt carries
/// near-current CPU/RAM numbers without any prompt-time work.
pub fn spawn_periodic_sampler(executor: gpui::BackgroundExecutor) {
    if PERIODIC_SAMPLER_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let loop_executor = executor.clone();
    executor
        .spawn(async move {
            let mut tick: u64 = 0;
            loop {
                sample_live_machine_inner(tick.is_multiple_of(POWER_PROBE_EVERY_N_SAMPLES)).await;
                tick = tick.wrapping_add(1);
                loop_executor.timer(PERIODIC_SAMPLE_INTERVAL).await;
            }
        })
        .detach();
}

// Persistent sampler instance: `refresh_cpu_usage` computes usage as the delta
// between consecutive refreshes, so a long-lived `System` yields a real average
// without any sleeping. The first-ever sample reports 0%; every later one is
// accurate.
static SAMPLER_SYSTEM: std::sync::Mutex<Option<System>> = std::sync::Mutex::new(None);

/// Take one machine sample (with a fresh power probe) and publish it. Must not
/// run on the main thread (power probe + process refresh); callers spawn it on
/// a background executor. Cheap no-op when a sample is already in flight;
/// concurrent samples coalesce.
pub async fn sample_live_machine() {
    sample_live_machine_inner(true).await;
}

async fn sample_live_machine_inner(probe_power: bool) {
    if SAMPLING.swap(true, Ordering::AcqRel) {
        return;
    }
    let power = if probe_power {
        read_power_state().await
    } else {
        // Between probes, carry the last known power state forward.
        LIVE_MACHINE
            .read()
            .ok()
            .and_then(|cache| cache.as_ref().map(|machine| machine.power.clone()))
            .flatten()
    };
    let snapshot = {
        let mut guard = SAMPLER_SYSTEM
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let system = guard.get_or_insert_with(System::new);
        system.refresh_cpu_usage();
        system.refresh_memory();
        LiveMachine {
            hostname: System::host_name(),
            os: System::long_os_version(),
            cpu_brand: system
                .cpus()
                .first()
                .map(|cpu| cpu.brand().trim().to_string())
                .filter(|brand| !brand.is_empty()),
            physical_cores: System::physical_core_count(),
            cpu_usage_percent: system.global_cpu_usage(),
            ram_used_bytes: system.used_memory(),
            ram_total_bytes: system.total_memory(),
            power,
        }
    };
    if let Ok(mut cache) = LIVE_MACHINE.write() {
        *cache = Some(Arc::new(snapshot));
    }
    SAMPLING.store(false, Ordering::Release);
}

/// The latest cached machine sample, if one has landed yet.
pub fn live_machine() -> Option<Arc<LiveMachine>> {
    LIVE_MACHINE.read().ok()?.clone()
}

/// One-line machine context for continuation prompts, e.g.
/// `Machine: host (macOS 15.5) — Apple M3 Max, 16 cores, CPU 42%, RAM 45/128 GB, GPU Apple M3 Max, power: AC plugged (battery 100%)`.
/// `gpu_device_name` comes from `window.gpu_specs()` on the main thread.
/// Returns `None` while no sample has landed yet (first call is always a
/// miss; the background sample spawned alongside it fills the cache).
pub fn machine_context_line(gpu_device_name: Option<&str>) -> Option<String> {
    let machine = live_machine()?;
    Some(format_machine_line(&machine, gpu_device_name))
}

fn format_machine_line(machine: &LiveMachine, gpu_device_name: Option<&str>) -> String {
    let mut parts = Vec::with_capacity(6);
    let mut host = String::new();
    if let Some(hostname) = machine.hostname.as_deref().filter(|h| !h.is_empty()) {
        host.push_str(hostname);
    }
    if let Some(os) = machine.os.as_deref().filter(|os| !os.is_empty()) {
        if !host.is_empty() {
            host.push_str(" · ");
        }
        host.push_str(os);
    }
    if !host.is_empty() {
        parts.push(host);
    }
    if let Some(brand) = machine.cpu_brand.as_deref().filter(|b| !b.is_empty()) {
        match machine.physical_cores {
            Some(cores) => parts.push(format!("{brand}, {cores} cores")),
            None => parts.push(brand.to_string()),
        }
    }
    parts.push(format!("CPU {:.0}%", machine.cpu_usage_percent));
    if machine.ram_total_bytes > 0 {
        parts.push(format!(
            "RAM {}/{}",
            human_bytes(machine.ram_used_bytes as f64),
            human_bytes(machine.ram_total_bytes as f64)
        ));
    }
    if let Some(gpu) = gpu_device_name.filter(|gpu| !gpu.is_empty()) {
        parts.push(format!("GPU {gpu}"));
    }
    if let Some(power) = machine.power.as_deref() {
        parts.push(format!("power: {power}"));
    }
    format!("Machine: {}", parts.join(", "))
}

/// Power/AC state, best effort per platform. Runs on the sampling background
/// task only — the macOS probe shells out to `pmset` via the async (smol)
/// command API, which never blocks the executor thread.
async fn read_power_state() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = smol::process::Command::new("pmset")
            .arg("-g")
            .arg("ps")
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        let ac = text.contains("'AC Power'");
        let battery_line = text
            .lines()
            .find(|line| line.contains('%') && line.to_lowercase().contains("battery"));
        let (percent, charge_state) = battery_line
            .map(|line| {
                let percent = line
                    .split(';')
                    .next()
                    .and_then(|head| {
                        head.trim_end()
                            .trim_end_matches('%')
                            .rsplit(|c: char| !c.is_ascii_digit())
                            .next()
                    })
                    .and_then(|digits| digits.parse::<u32>().ok());
                let state = if line.contains("charging") {
                    "charging"
                } else if line.contains("discharging") {
                    "discharging"
                } else {
                    "charged"
                };
                (percent, state)
            })
            .unwrap_or((None, ""));
        return Some(match (ac, percent) {
            (true, Some(percent)) => format!(
                "AC plugged (battery {percent}%{charge_state_suffix})",
                charge_state_suffix = if charge_state.is_empty() {
                    String::new()
                } else {
                    format!(", {charge_state}")
                }
            ),
            (true, None) => "AC plugged".to_string(),
            (false, Some(percent)) => format!(
                "On battery ({percent}%{charge_state_suffix})",
                charge_state_suffix = if charge_state.is_empty() {
                    String::new()
                } else {
                    format!(", {charge_state}")
                }
            ),
            (false, None) => "On battery".to_string(),
        });
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        // /sys/class/power_supply/A*/online — AC present; BAT*/status carries
        // Charging/Discharging/Full, BAT*/capacity the percent.
        let mut ac = None;
        let mut status = None;
        let mut capacity = None;
        if let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('A') {
                    if let Ok(online) = std::fs::read_to_string(entry.path().join("online")) {
                        ac = Some(online.trim() == "1");
                    }
                } else if name.starts_with("BAT") {
                    status = std::fs::read_to_string(entry.path().join("status"))
                        .ok()
                        .map(|s| s.trim().to_lowercase());
                    capacity = std::fs::read_to_string(entry.path().join("capacity"))
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok());
                }
            }
        }
        let detail = match (capacity, status.as_deref()) {
            (Some(percent), Some(state)) if !state.is_empty() => {
                format!(" (battery {percent}%, {state})")
            }
            (Some(percent), _) => format!(" (battery {percent}%)"),
            _ => String::new(),
        };
        return Some(match ac {
            Some(true) => format!("AC plugged{detail}"),
            Some(false) => format!("On battery{detail}"),
            None => "AC plugged (desktop)".to_string(),
        });
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status = SYSTEM_POWER_STATUS::default();
        if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
            return None;
        }
        // ACLineStatus: 0 = on battery, 1 = on AC, 255 = unknown.
        let ac = match status.ACLineStatus {
            0 => false,
            1 => true,
            _ => return None,
        };
        // BatteryFlag bit 2 = charging. BatteryLifePercent 255 = unknown
        // (desktop without a battery reports that, typically).
        let charging = status.BatteryFlag & 4 != 0;
        let detail = if status.BatteryLifePercent == 255 {
            String::new()
        } else {
            let percent = status.BatteryLifePercent as u32;
            let state = if charging { ", charging" } else { "" };
            format!(" (battery {percent}%{state})")
        };
        return Some(if ac {
            format!("AC plugged{detail}")
        } else {
            format!("On battery{detail}")
        });
    }
    #[allow(unreachable_code)]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_line_includes_present_fields_only() {
        let machine = LiveMachine {
            hostname: Some("m3max".to_string()),
            os: Some("macOS 15.5".to_string()),
            cpu_brand: Some("Apple M3 Max".to_string()),
            physical_cores: Some(16),
            cpu_usage_percent: 41.6,
            ram_used_bytes: 48_318_382_080,
            ram_total_bytes: 137_438_953_472,
            power: Some("AC plugged (battery 100%, charged)".to_string()),
        };
        let line = format_machine_line(&machine, Some("Apple M3 Max"));
        assert_eq!(
            line,
            "Machine: m3max · macOS 15.5, Apple M3 Max, 16 cores, CPU 42%, RAM 45 GiB/128 GiB, GPU Apple M3 Max, power: AC plugged (battery 100%, charged)"
        );
    }

    #[test]
    fn machine_line_skips_missing_fields() {
        let machine = LiveMachine {
            hostname: None,
            os: None,
            cpu_brand: None,
            physical_cores: None,
            cpu_usage_percent: 0.0,
            ram_used_bytes: 0,
            ram_total_bytes: 0,
            power: None,
        };
        let line = format_machine_line(&machine, None);
        assert_eq!(line, "Machine: CPU 0%");
    }
}
