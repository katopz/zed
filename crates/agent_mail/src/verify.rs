//! The verifier lane's data contracts (issue 055): the pinned baseline
//! manifest a verify run re-measures against, the provenance every result
//! row must carry, and the ratio-to-baseline verdicts. Pure logic lives
//! here so it is testable without a rig; the runner wiring is in `jobs`.

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;

/// The measured-cell contract a verify run re-executes (issue 055). Lives
/// outside the job queue as a file the submitter pins; only its BLAKE3 hash
/// rides in the spec, so the queue carries the pinned baseline but the
/// verified run can still prove which manifest it measured against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyManifest {
    /// The producing rig's job id whose gain is being re-checked.
    pub origin_job: String,
    /// Rig the producing job ran on; the verify run refuses this rig.
    pub produced_on: String,
    /// repo@sha the measurement must rebuild from (the baseline commit).
    pub repo: String,
    pub sha: String,
    /// The measured cell's exact command.
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Same contract as the job spec's `result_path`.
    #[serde(default)]
    pub result_path: Option<String>,
    /// The frozen per-plan baseline the ratios are computed against.
    pub baseline: Baseline,
}

impl VerifyManifest {
    /// The hash that ties a verify run to the exact manifest it measured
    /// against (submitter can re-hash the file to audit any ledger row).
    pub fn blake3(&self) -> Result<String> {
        let bytes = serde_json::to_vec(self).context("serializing the verify manifest")?;
        Ok(blake3::hash(&bytes).to_string())
    }
}

/// Per-plan baseline block: every claim is a ratio to these numbers, never an
/// absolute. `commit` is the baseline measurement's commit; `noise_floor` is
/// a relative fraction (0.03 = 3%) under which a delta is inconclusive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    /// Plan id the claim belongs to (e.g. `056`).
    pub plan: String,
    /// Commit hash the baseline numbers were measured at.
    pub commit: String,
    /// Metric name → value and direction (wall time vs throughput).
    pub numbers: BTreeMap<String, BaselineNumber>,
    /// Relative noise floor; deltas inside it are `inconclusive`.
    pub noise_floor: f64,
    /// Where the numbers came from (`.benchmarks/…` file or manifest).
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BaselineNumber {
    pub value: f64,
    /// Wall-time-like metrics shrink on a gain; throughput-like metrics grow.
    #[serde(default = "default_lower_is_better")]
    pub lower_is_better: bool,
}

fn default_lower_is_better() -> bool {
    true
}

/// What a verify run carries in its job spec: the pinned baseline rides
/// along (the verifying rig needs it at finish); the manifest file itself
/// stays with the submitter and its BLAKE3 hash proves which one was
/// measured against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifySpec {
    pub origin_job: String,
    pub produced_on: String,
    pub manifest_blake3: String,
    pub baseline: Baseline,
}

/// The hardware/software provenance of a finished job (issue 055: a result
/// row without these is not a result).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEnvironment {
    pub rig: String,
    pub cores: usize,
    pub os: String,
    /// `rustc --version` at run time, when a toolchain is installed.
    #[serde(default)]
    pub toolchain: Option<String>,
    /// The worktree sha the job ran at, when it declared one.
    #[serde(default)]
    pub baseline_sha: Option<String>,
    #[serde(default)]
    pub manifest_blake3: Option<String>,
}

impl RunEnvironment {
    /// Captures the provenance of the machine about to run a job. `rustc`
    /// is consulted once per job, off the hot path.
    // agent-mail is a runtime-free std binary; the smol-preferring lint targets async gpui code.
    #[allow(clippy::disallowed_methods)]
    pub fn capture(rig: &str, worktree: Option<&str>, manifest_blake3: Option<&str>) -> Self {
        let toolchain = crate::command("rustc")
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
        Self {
            rig: rig.to_string(),
            cores: std::thread::available_parallelism().map_or(1, |cores| cores.get()),
            os: std::env::consts::OS.to_string(),
            toolchain,
            baseline_sha: worktree
                .and_then(|target| target.split_once('@').map(|(_, sha)| sha.to_string())),
            manifest_blake3: manifest_blake3.map(str::to_string),
        }
    }

    /// A result row is complete only with rig, cores, os, and toolchain;
    /// anything else is provenance-starved and must not count as a result.
    pub fn complete(&self) -> bool {
        self.toolchain.is_some()
    }
}

/// Ratio-to-baseline verdicts for a finished verify run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyOutcome {
    pub origin_job: String,
    pub produced_on: String,
    pub verified_on: String,
    /// Metric name → measured value, baseline, improvement, verdict.
    pub metrics: BTreeMap<String, MetricVerdict>,
    /// `verified` only when every declared metric improved past the noise
    /// floor; `refuted` when any measured metric regressed past it (a gain
    /// on one metric does not buy a regression on another); `incomplete`
    /// when the cell never delivered all declared metrics; `inconclusive`
    /// when everything measured sits inside the noise floor.
    pub verdict: String,
    /// Set when the provenance row was incomplete (no toolchain, …).
    #[serde(default)]
    pub provenance_incomplete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricVerdict {
    pub measured: f64,
    pub baseline: f64,
    /// Gain fraction in the metric's better direction: 1.10 = 10% better.
    pub improvement: f64,
    /// `gain`, `regression`, or `noise`.
    pub verdict: String,
}

/// Compares a finished run's measured numbers against the pinned baseline.
/// Pure, so it is testable without a rig.
pub fn verify_against_baseline(
    measured: &BTreeMap<String, f64>,
    baseline: &Baseline,
) -> VerifyOutcome {
    let mut metrics = BTreeMap::new();
    let (mut gains, mut regressions) = (0usize, 0usize);
    for (name, number) in &baseline.numbers {
        let Some(value) = measured.get(name) else {
            continue;
        };
        let improvement = if number.lower_is_better {
            number.value / value.max(f64::MIN_POSITIVE)
        } else {
            *value / number.value.max(f64::MIN_POSITIVE)
        };
        let verdict = if improvement >= 1.0 + baseline.noise_floor {
            gains += 1;
            "gain"
        } else if improvement <= 1.0 - baseline.noise_floor {
            regressions += 1;
            "regression"
        } else {
            "noise"
        };
        metrics.insert(
            name.clone(),
            MetricVerdict {
                measured: *value,
                baseline: number.value,
                improvement,
                verdict: verdict.to_string(),
            },
        );
    }
    // Guard-set verdicts: ANY measured regression past the noise floor
    // refutes the claim (a throughput gain on a regressing latency cell is
    // not a gain); `verified` needs every declared metric measured and
    // improved; a partially-measured cell stays uncountable as `incomplete`.
    let verdict = if regressions > 0 {
        "refuted"
    } else if gains == baseline.numbers.len() {
        "verified"
    } else if metrics.len() < baseline.numbers.len() {
        "incomplete"
    } else {
        "inconclusive"
    };
    VerifyOutcome {
        origin_job: String::new(),
        produced_on: String::new(),
        verified_on: String::new(),
        metrics,
        verdict: verdict.to_string(),
        provenance_incomplete: false,
    }
}

/// One referee-ledger row (issue 055): the orchestrator's append-only
/// record of which rig verified which rig's claimed gain, and what the
/// ratio-to-baseline said.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifyLedgerRow {
    pub id: String,
    pub origin_job: String,
    pub produced_on: String,
    pub verified_on: String,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub baseline_sha: Option<String>,
    #[serde(default)]
    pub manifest_blake3: Option<String>,
    /// The job outcome the row was recorded at (`done` rows are countable).
    pub outcome: String,
    /// `verified` / `refuted` / `incomplete` / `inconclusive`.
    pub verdict: String,
    pub provenance_incomplete: bool,
    pub metrics: BTreeMap<String, MetricVerdict>,
    pub at: String,
}

/// Reads a finished job's measured numbers: a JSON object mapping metric
/// names to values. `None` when missing or malformed — either way the run
/// has no countable cell.
pub fn read_result_file(cwd: &Option<PathBuf>, result_path: &str) -> Option<BTreeMap<String, f64>> {
    let path = match cwd {
        Some(cwd) => cwd.join(result_path),
        None => PathBuf::from(result_path),
    };
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn verify_verdicts_follow_the_noise_floor_and_metric_directions() {
        // Both metrics gain 10% in their better direction → verified.
        let measured = BTreeMap::from([("wall_ms".into(), 90.0), ("tok_per_s".into(), 1100.0)]);
        assert_eq!(
            verify_against_baseline(&measured, &baseline()).verdict,
            "verified"
        );
        // wall_ms regressed 10% (110 > 100) while tok_per_s gained →
        // refuted: a gain on one metric never buys a regression on another.
        let measured = BTreeMap::from([("wall_ms".into(), 110.0), ("tok_per_s".into(), 1100.0)]);
        assert_eq!(
            verify_against_baseline(&measured, &baseline()).verdict,
            "refuted"
        );
        // Inside the 5% noise floor on both → inconclusive.
        let measured = BTreeMap::from([("wall_ms".into(), 97.0), ("tok_per_s".into(), 1030.0)]);
        assert_eq!(
            verify_against_baseline(&measured, &baseline()).verdict,
            "inconclusive"
        );
        // Both directions regress past the floor → refuted.
        let measured = BTreeMap::from([("wall_ms".into(), 120.0), ("tok_per_s".into(), 800.0)]);
        assert_eq!(
            verify_against_baseline(&measured, &baseline()).verdict,
            "refuted"
        );
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
    fn manifest_hashes_stably() {
        let manifest = VerifyManifest {
            origin_job: "01job".into(),
            produced_on: "bmo".into(),
            repo: "riir-ai".into(),
            sha: "abc".into(),
            argv: vec!["bench".into()],
            cwd: None,
            result_path: None,
            baseline: baseline(),
        };
        let hash = manifest.blake3().unwrap();
        assert_eq!(hash, manifest.blake3().unwrap(), "stable across calls");
        assert_eq!(hash.len(), 64, "BLAKE3 hex length");
    }
}
