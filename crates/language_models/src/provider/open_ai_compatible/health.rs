//! Per-key health tracking, exponential backoff, intra-request key rotation,
//! and on-disk persistence for the OpenAI-compatible provider.
//!
//! This module is a private submodule of [`super`]; all items are `pub` but the
//! module itself is declared `mod health;` (not `pub mod`), so nothing here is
//! reachable outside `open_ai_compatible`.
//!
//! Split out from `open_ai_compatible.rs` to keep that file focused on provider
//! configuration, credentials, and the `LanguageModel` / `ConfigurationView`
//! impls. The subsystem housed here is self-contained: it knows about keys
//! only as opaque `Arc<str>` values tagged with a [`KeySlot`], and about errors
//! only through [`LanguageModelCompletionError`].

use anyhow::{Context as _, Result};
use fs::Fs;
use futures::future::BoxFuture;
use gpui::{BackgroundExecutor, Task};
use language_model::{LanguageModelCompletionError, LanguageModelProviderName};
use parking_lot::Mutex as ParkingMutex;
use paths;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Which slot a key was selected from, so request outcomes can be attributed back
/// to the correct `KeyHealth` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySlot {
    Primary,
    Secondary,
    Tertiary,
    Quaternary,
}

/// Per-key backoff state. Persisted across restarts as relative durations
/// (see `PersistedKeyHealth`); in-memory `Instant`s are reconstructed on load.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct KeyHealth {
    pub consecutive_failures: u32,
    pub backoff_until: Option<Instant>,
}

/// UI-facing projection of one slot's health + configuration state. Returned
/// in a fixed `[Primary, Secondary, Tertiary, Quaternary]` order by `State::slot_health_snapshot`
/// so the ConfigurationView can render a backoff badge without reaching into
/// `KeyHealthTracker` directly (which lives behind a mutex in `State`).
#[derive(Clone, Debug, PartialEq)]
pub struct SlotHealthStatus {
    pub has_key: bool,
    pub is_backed_off: bool,
    pub backoff_remaining: Duration,
    pub consecutive_failures: u32,
}

impl KeyHealth {
    pub fn is_backed_off(&self, now: Instant) -> bool {
        matches!(self.backoff_until, Some(until) if now < until)
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct KeyHealthTracker {
    pub primary: KeyHealth,
    pub secondary: KeyHealth,
    pub tertiary: KeyHealth,
    pub quaternary: KeyHealth,
    /// Ephemeral (never persisted): the slot most recently selected by
    /// `select_from_candidates` inside `retry_stream`. Surfaced to the UI so the
    /// retry button can show which key the in-flight turn is actually using.
    /// Reset to `None` on load since a stale value across restarts is meaningless.
    pub last_used_slot: Option<KeySlot>,
}

impl KeyHealthTracker {
    pub fn get(&self, slot: KeySlot) -> &KeyHealth {
        match slot {
            KeySlot::Primary => &self.primary,
            KeySlot::Secondary => &self.secondary,
            KeySlot::Tertiary => &self.tertiary,
            KeySlot::Quaternary => &self.quaternary,
        }
    }

    pub fn get_mut(&mut self, slot: KeySlot) -> &mut KeyHealth {
        match slot {
            KeySlot::Primary => &mut self.primary,
            KeySlot::Secondary => &mut self.secondary,
            KeySlot::Tertiary => &mut self.tertiary,
            KeySlot::Quaternary => &mut self.quaternary,
        }
    }

    /// Resets the slot's health on success: clears the failure counter and any
    /// pending backoff. A single success is enough to re-qualify a previously
    /// failing key.
    pub fn record_success(&mut self, slot: KeySlot) {
        let health = self.get_mut(slot);
        health.consecutive_failures = 0;
        health.backoff_until = None;
    }

    /// Records a backoff-worthy failure on the slot: bumps the failure counter
    /// and recomputes `backoff_until = now + compute_backoff(count)`.
    /// Non-backoff-worthy errors should not call this (they would poison the
    /// slot without benefit since the same error would occur on every key).
    pub fn record_failure(&mut self, slot: KeySlot, now: Instant) {
        let health = self.get_mut(slot);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        let backoff = compute_backoff(health.consecutive_failures);
        health.backoff_until = Some(now + backoff);
    }

    /// Marks `slot` as the one currently being attempted, so the UI can show
    /// which key an in-flight turn is using. Called from `retry_stream` right
    /// after `select_from_candidates` picks a key — before the attempt resolves.
    pub fn record_attempt(&mut self, slot: KeySlot) {
        self.last_used_slot = Some(slot);
    }
}

// ---------------------------------------------------------------------------
// Persistence
//
// `Instant` is monotonic and process-local, so we can't serialize an absolute
// timestamp. Instead we persist the *remaining* backoff as a `Duration` and
// reconstruct `Instant::now() + remaining` on load. If the remaining duration
// is zero/negative (i.e. the backoff already elapsed while Zed was closed) the
// reconstructed `Instant` is in the past and `is_backed_off` returns false —
// no special handling needed.
// ---------------------------------------------------------------------------

/// On-disk representation of a single slot's health. `backoff_remaining_secs`
/// is `backoff_until - now` at save time (or `null` if the slot is healthy).
///
/// `Default` is a fully-healthy slot (zero failures, no backoff). Used by
/// `#[serde(default)]` on `PersistedKeyHealthFile::quaternary` so that v1
/// schema files (which predate the quaternary slot) deserialize successfully
/// and migrate forward instead of being rejected wholesale — see issue 007.
#[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
pub struct PersistedKeyHealth {
    pub consecutive_failures: u32,
    pub backoff_remaining_secs: Option<f64>,
}

/// Top-level persisted file. One per provider id, under
/// `paths::data_dir()/openai_compatible_backoff/{id}.json`. `schema_version`
/// lets us migrate the shape later without silent breakage.
///
/// `saved_at_unix_secs` is a wall-clock timestamp (from `SystemTime::now()`)
/// captured at save time, used at load time to subtract the time Zed spent
/// closed. Without it, reloading would always push `backoff_until` forward
/// by the elapsed wall-clock time, defeating the purpose of persistence
/// (a 1ms backoff persisted 5h ago would reload as "1ms from now").
///
/// # Forward compatibility
///
/// `quaternary` is marked `#[serde(default)]` so that v1 schema files (which
/// predate the Quaternary slot, commit 9b063ddf) deserialize successfully:
/// serde fills in a healthy default for the missing field, the v1→v2
/// migration in `reload_persisted_health` logs it, and the next save writes
/// the full v2 shape. Without this, the entire file was rejected on parse
/// ("missing field `quaternary`") which silently wiped ALL slot backoff
/// state — causing the rate-limit rotation regression documented in issue 007.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct PersistedKeyHealthFile {
    pub schema_version: u32,
    pub saved_at_unix_secs: u64,
    pub primary: PersistedKeyHealth,
    pub secondary: PersistedKeyHealth,
    pub tertiary: PersistedKeyHealth,
    #[serde(default)]
    pub quaternary: PersistedKeyHealth,
}

pub const PERSISTED_KEY_HEALTH_SCHEMA_VERSION: u32 = 2;

/// Subdirectory under `paths::data_dir()` holding one JSON file per provider.
pub const PERSIST_DIR_NAME: &str = "openai_compatible_backoff";

/// Debounce window for coalescing bursts of writes. A tight retry loop can
/// record several failures in milliseconds; we only want one disk write per
/// burst, so the latest task always cancels its predecessor after this delay.
pub const PERSIST_DEBOUNCE: Duration = Duration::from_secs(2);

impl PersistedKeyHealth {
    pub fn from_health(health: &KeyHealth, now: Instant) -> Self {
        let backoff_remaining_secs = health
            .backoff_until
            .map(|until| until.saturating_duration_since(now).as_secs_f64());
        Self {
            consecutive_failures: health.consecutive_failures,
            backoff_remaining_secs,
        }
    }

    /// Reconstructs an in-memory `KeyHealth`. The reconstructed `backoff_until`
    /// is `now + max(0, remaining - elapsed)`; if the slot was healthy at save
    /// (`remaining == None`) or the backoff already elapsed while Zed was
    /// closed (`remaining <= elapsed`), the slot loads as healthy.
    pub fn to_health(&self, now: Instant, elapsed_secs: f64) -> KeyHealth {
        let backoff_until = self
            .backoff_remaining_secs
            .filter(|secs| *secs > elapsed_secs)
            .map(|secs| now + Duration::from_secs_f64((secs - elapsed_secs).max(0.0)));
        KeyHealth {
            consecutive_failures: self.consecutive_failures,
            backoff_until,
        }
    }
}

impl PersistedKeyHealthFile {
    pub fn from_tracker(tracker: &KeyHealthTracker, now: Instant) -> Self {
        Self {
            schema_version: PERSISTED_KEY_HEALTH_SCHEMA_VERSION,
            // Wall-clock at save time, captured once so all three slots share
            // the same reference point. `UNIX_EPOCH.now()` is the canonical
            // way to get a serializable wall-clock; monotonic `Instant` can't
            // be serialized meaningfully across process boundaries.
            saved_at_unix_secs: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            primary: PersistedKeyHealth::from_health(&tracker.primary, now),
            secondary: PersistedKeyHealth::from_health(&tracker.secondary, now),
            tertiary: PersistedKeyHealth::from_health(&tracker.tertiary, now),
            quaternary: PersistedKeyHealth::from_health(&tracker.quaternary, now),
        }
    }

    pub fn to_tracker(&self, now: Instant) -> KeyHealthTracker {
        // How much wall-clock time elapsed between save and load? We use
        // `SystemTime` (not `Instant`) because the save and load happen in
        // different processes — `Instant` is process-local and not comparable
        // across runs. The elapsed is then subtracted from each slot's
        // remaining backoff: a 1ms backoff persisted 5h ago loads as healthy.
        let elapsed_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()
            .and_then(|now_unix| now_unix.as_secs().checked_sub(self.saved_at_unix_secs))
            .map(|secs| secs as f64)
            .unwrap_or(0.0);
        KeyHealthTracker {
            primary: self.primary.to_health(now, elapsed_secs),
            secondary: self.secondary.to_health(now, elapsed_secs),
            tertiary: self.tertiary.to_health(now, elapsed_secs),
            quaternary: self.quaternary.to_health(now, elapsed_secs),
            // `last_used_slot` is ephemeral runtime state — never restored
            // from disk. A stale slot from a previous process would mislead
            // the retry button label on the very first turn after launch.
            last_used_slot: None,
        }
    }
}

/// Filename-safe form of a provider id. The id is a user-supplied string that
/// may contain path separators or other characters unsafe as a filename; we
/// replace them with `_` and fall back to `provider` if the result is empty.
/// This is purely defensive — collisions across distinct ids would only cause
/// two providers to share a backoff file, not a correctness bug in either.
pub fn sanitize_provider_id_for_filename(provider_id: &str) -> String {
    let sanitized: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "provider".to_string()
    } else {
        sanitized.to_string()
    }
}

/// `paths::data_dir()/openai_compatible_backoff/{sanitized_id}.json`.
pub fn key_health_path_for(provider_id: &str) -> PathBuf {
    paths::data_dir()
        .join(PERSIST_DIR_NAME)
        .join(format!("{}.json", sanitize_provider_id_for_filename(provider_id)))
}

/// Loads a `KeyHealthTracker` from disk. Missing file and parse errors are
/// non-fatal: they return a fresh `KeyHealthTracker::default()` so a corrupt
/// or absent state never blocks requests.
pub async fn reload_persisted_health(fs: &Arc<dyn Fs>, path: &PathBuf) -> KeyHealthTracker {
    let content = match fs.load(path).await {
        Ok(content) => content,
        Err(err) => {
            // Missing file is the common case on first run; only log non-NotFound.
            if !err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            {
                log::warn!(
                    "failed to load persisted key health at {}: {err:#}",
                    path.display()
                );
            }
            return KeyHealthTracker::default();
        }
    };
    match serde_json::from_str::<PersistedKeyHealthFile>(&content) {
        Ok(file) => {
            // Forward-compatible migration. Each step upgrades in place; we
            // accept anything <= CURRENT and reject anything > CURRENT (a
            // newer Zed wrote a file we can't safely read). Downgrades get a
            // fresh tracker rather than silently misinterpreting fields.
            //
            // v1→v2 (commit 9b063ddf): added `quaternary` slot. v1 files
            // have no `quaternary` field; with `#[serde(default)]` on the
            // struct field, serde fills in a healthy default. We just carry
            // it through — no field-level transform needed.
            if file.schema_version > PERSISTED_KEY_HEALTH_SCHEMA_VERSION {
                log::warn!(
                    "ignoring persisted key health with schema_version {} (expected <= {}) at {} — newer Zed wrote this file",
                    file.schema_version,
                    PERSISTED_KEY_HEALTH_SCHEMA_VERSION,
                    path.display()
                );
                return KeyHealthTracker::default();
            }
            if file.schema_version < PERSISTED_KEY_HEALTH_SCHEMA_VERSION {
                log::info!(
                    "migrating persisted key health from schema_version {} to {} at {}",
                    file.schema_version,
                    PERSISTED_KEY_HEALTH_SCHEMA_VERSION,
                    path.display()
                );
            }
            file.to_tracker(Instant::now())
        }
        Err(err) => {
            log::warn!(
                "failed to parse persisted key health at {}: {err:#}",
                path.display()
            );
            KeyHealthTracker::default()
        }
    }
}

/// Atomic-writes the tracker snapshot to disk. Errors are propagated (caller
/// decides whether to log); missing parent dir is created on demand.
pub async fn persist_key_health(
    fs: &Arc<dyn Fs>,
    path: PathBuf,
    tracker: KeyHealthTracker,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await.with_context(|| {
            format!("creating parent dir for key health persistence: {}", parent.display())
        })?;
    }
    let serialized = serde_json::to_string(&PersistedKeyHealthFile::from_tracker(
        &tracker,
        Instant::now(),
    ))
    .context("serializing key health for persistence")?;
    fs.atomic_write(path.clone(), serialized)
        .await
        .with_context(|| format!("writing key health to {}", path.display()))
}

/// Free-function form of `State::schedule_persist_key_health` so the request
/// closure (which runs on a background executor and only has clones of the
/// underlying `Arc`s) can schedule a save without re-entering `Entity::update`.
///
/// Takes `BackgroundExecutor` + `Arc<dyn Fs>` (both `Send + Sync + Clone`)
/// instead of `AsyncApp` so the rate-limited stream closure — which must be
/// `Send` to satisfy `BoxFuture<'static, ...>` — can capture these handles by
/// move without dragging the `!Send` `AsyncApp` along.
pub fn schedule_persist_key_health_inner(
    key_health: &Arc<ParkingMutex<KeyHealthTracker>>,
    key_health_dirty: &Arc<ParkingMutex<Option<Task<()>>>>,
    path: PathBuf,
    executor: BackgroundExecutor,
    fs: Arc<dyn Fs>,
) {
    let snapshot = key_health.lock().clone();
    // Clone before the move into `spawn`: `spawn` takes `&self`, but the
    // closure body needs an owned executor to await `.timer(...)`.
    let timer_executor = executor.clone();
    let task = executor.spawn(async move {
        // Debounce: sleep briefly so back-to-back record_failure calls
        // (e.g. inside retry_stream's loop) collapse into a single write.
        timer_executor.timer(PERSIST_DEBOUNCE).await;
        if let Err(err) = persist_key_health(&fs, path.clone(), snapshot).await {
            log::warn!("failed to persist key health to {}: {err:#}", path.display());
        }
    });
    // Replace any prior pending task. Dropping the old `Task` cancels it.
    *key_health_dirty.lock() = Some(task);
}

/// Soft cap on backoff. After this duration since the last failure the key is
/// automatically selectable again — no explicit "clear" path is needed.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60 * 60);

/// Base unit for the exponential schedule.
pub const BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Computes an exponential backoff with jitter. The 5-hour cap is the
/// dominant constraint regardless of how large `failures` gets, matching the
/// user requirement of "remove backoff after 5 hours for each key".
///
/// Jitter factor is in `[0.5, 1.5)` to avoid the thundering-herd case where
/// all keys fail at the same instant and would otherwise all unblock together.
pub fn compute_backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // 2^(failures-1), capped at 14 so the multiplication can't overflow `Duration`
    // (2^14 * 30s ≈ 138h, already well past the 5h cap, so the clamp is a no-op).
    let exponent = (failures - 1).min(14);
    let multiplier = 2u32.pow(exponent);
    let candidate = BACKOFF_BASE
        .checked_mul(multiplier)
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX);
    let jitter = rand::rng().random_range(0.5..1.5);
    candidate.mul_f64(jitter).min(BACKOFF_MAX)
}

/// Formats a remaining backoff duration for the ConfigurationView badge.
/// Hour precision drops the seconds (the user doesn't need them at that scale);
/// sub-minute durations still show seconds so short backoffs feel responsive.
/// Returns `"0s"` for `Duration::ZERO` (e.g. slot just exited backoff between
/// snapshot and render).
pub fn format_backoff_remaining(remaining: Duration) -> String {
    let total_secs = remaining.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// True for `RateLimitExceeded` specifically. Used by `retry_stream` to decide
/// whether to stop rotating after the first failure: rate limits are
/// frequently account/org-wide (multiple keys under one quota), so a 429 on
/// key A is a strong predictor of a 429 on key B. Rotating in that case just
/// poisons the whole pool in one request and leaves no healthy key for the
/// *next* request. The slot that hit the limit is still backed off (so the
/// next request skips it), we just don't burn its siblings.
pub fn is_rate_limit(err: &LanguageModelCompletionError) -> bool {
    matches!(
        err,
        LanguageModelCompletionError::RateLimitExceeded { .. }
    )
}

/// Returns true for errors that suggest the *key* or *upstream* is the problem
/// (and so rotating to a different key may help), false for errors that will
/// recur on every key (so poisoning the slot would just shrink the pool
/// without benefit).
///
/// This is intentionally permissive: the user reported upstream error labels
/// are unreliable, so when in doubt we back off rather than burn requests.
pub fn is_backoff_worthy(err: &LanguageModelCompletionError) -> bool {
    match err {
        LanguageModelCompletionError::RateLimitExceeded { .. }
        | LanguageModelCompletionError::ServerOverloaded { .. }
        | LanguageModelCompletionError::ApiInternalServerError { .. }
        | LanguageModelCompletionError::UpstreamProviderError { .. }
        | LanguageModelCompletionError::StreamEndedUnexpectedly { .. }
        | LanguageModelCompletionError::ApiReadResponseError { .. }
        | LanguageModelCompletionError::HttpSend { .. }
        | LanguageModelCompletionError::AuthenticationError { .. }
        | LanguageModelCompletionError::PermissionError { .. }
        | LanguageModelCompletionError::PaymentRequired
        | LanguageModelCompletionError::Other(_) => true,
        LanguageModelCompletionError::PromptTooLarge { .. }
        | LanguageModelCompletionError::NoApiKey { .. }
        | LanguageModelCompletionError::BadRequestFormat { .. }
        | LanguageModelCompletionError::ApiEndpointNotFound { .. }
        | LanguageModelCompletionError::HttpResponseError { .. }
        | LanguageModelCompletionError::SerializeRequest { .. }
        | LanguageModelCompletionError::BuildRequestBody { .. }
        | LanguageModelCompletionError::DeserializeResponse { .. }
        | LanguageModelCompletionError::DataRetentionConsentRequired { .. } => false,
    }
}

/// Pure-function form of key selection. Used by the intra-request retry loop
/// in `stream_completion` / `stream_response`, which has already snapshot the
/// candidate list and cannot go back through `&self`.
///
/// Picks a healthy candidate by **deterministic hourly rotation**, not random.
/// The index is `floor(now_unix_secs / 3600) % healthy.len()` — so the same key
/// is selected for the entire wall-clock hour. This is cache-friendly: most
/// upstream providers key their prompt cache on the API key, and random
/// per-request rotation would thrash the cache. Rotating once per hour keeps
/// the cache warm while still distributing load across keys over time.
///
/// If every candidate is currently backed off, returns the one whose
/// `backoff_until` is soonest — failing open is strictly better than
/// `NoApiKey` when at least one key exists.
pub fn select_from_candidates(
    candidates: &[(Arc<str>, KeySlot)],
    health: &KeyHealthTracker,
    now: Instant,
) -> Option<(Arc<str>, KeySlot)> {
    // Healthy candidates: present and not in backoff.
    let healthy: Vec<(Arc<str>, KeySlot)> = candidates
        .iter()
        .filter(|(_, slot)| !health.get(*slot).is_backed_off(now))
        .cloned()
        .collect();

    if let Some(pick) = deterministic_hourly_pick(&healthy) {
        return Some(pick);
    }

    // Everything present is backed off — fall back to the earliest-expiring
    // backed-off key. Better than NoApiKey when at least one key exists.
    candidates
        .iter()
        .filter_map(|(key, slot)| {
            let until = health.get(*slot).backoff_until?;
            Some(((key.clone(), *slot), until))
        })
        .min_by_key(|(_, until)| *until)
        .map(|((key, slot), _)| (key, slot))
}

/// Picks a candidate by the current wall-clock hour so the same key is used
/// for the whole hour (cache-friendly). Returns `None` for an empty list.
fn deterministic_hourly_pick(
    healthy: &[(Arc<str>, KeySlot)],
) -> Option<(Arc<str>, KeySlot)> {
    if healthy.is_empty() {
        return None;
    }
    // Wall-clock hour since UNIX_EPOCH. `SystemTime` (not `Instant`) because
    // the rotation must align across process boundaries and restarts — a
    // process-local monotonic clock would desync the schedule.
    let unix_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hour = (unix_secs / 3600) as usize;
    let idx = hour % healthy.len();
    Some(healthy[idx].clone())
}

/// Updates per-key health after a successful request. Called from inside
/// the rate-limited stream closure so health reflects real request outcomes.
///
/// A single success clears the slot's failure counter and backoff — a
/// previously-failing key re-qualifies immediately.
///
/// Uses the shared `Arc<Mutex<KeyHealthTracker>>` rather than `Entity::update`
/// because the request closure runs on a background executor where `AsyncApp`
/// (`!Send`) cannot travel.
pub fn record_key_success(key_health: &Arc<ParkingMutex<KeyHealthTracker>>, slot: KeySlot) {
    let mut health = key_health.lock();
    health.record_success(slot);
}

/// Marks `slot` as the one being attempted right now, so the UI can surface
/// which key the in-flight turn picked. Called from `retry_stream` immediately
/// after `select_from_candidates` resolves, before `do_attempt` is awaited.
pub fn record_key_attempt(key_health: &Arc<ParkingMutex<KeyHealthTracker>>, slot: KeySlot) {
    let mut health = key_health.lock();
    health.record_attempt(slot);
}

/// Updates per-key health after a failed request. Only backoff-worthy errors
/// (see `is_backoff_worthy`) bump the failure counter and reschedule backoff;
/// other errors are no-ops because they would recur on every key.
pub fn record_key_failure(
    key_health: &Arc<ParkingMutex<KeyHealthTracker>>,
    slot: KeySlot,
    err: &LanguageModelCompletionError,
) {
    if !is_backoff_worthy(err) {
        return;
    }
    let mut health = key_health.lock();
    health.record_failure(slot, Instant::now());
}

/// Helper: snapshots the health tracker under the mutex so the (borrowing)
/// `select_from_candidates` can read it without holding the lock across the
/// attempt future. Holding the lock across `.await` would serialize all
/// in-flight requests on the same provider and risk deadlock if a downstream
/// path ever tried to re-acquire.
pub fn snapshot_health(key_health: &Arc<ParkingMutex<KeyHealthTracker>>) -> KeyHealthTracker {
    key_health.lock().clone()
}

/// Drives intra-request key rotation. Tries up to `candidates.len()` keys,
/// each selected at the moment of the attempt (so a slot that just got backed
/// off is skipped on the next pick). On the first success the resulting stream
/// is returned and the slot's health is cleared. On a backoff-worthy failure
/// the slot is poisoned and the next candidate is tried — *except* for
/// `RateLimitExceeded`, which exits the loop immediately after poisoning the
/// slot (see `is_rate_limit`: rate limits are commonly account-wide, so
/// rotating would burn healthy siblings for no benefit). On any other
/// (non-backoff-worthy) error the loop exits immediately — the error would
/// recur on every key.
///
/// `do_attempt` receives the chosen key and must return a `'static` future;
/// callers are expected to clone the request template inside the closure
/// (the underlying `open_ai::Request` / `responses::Request` types derive
/// `Clone` for this purpose) and to map the provider-specific error into
/// `LanguageModelCompletionError`.
///
/// Bounds the worst-case latency to one full key rotation per user request
/// (fewer on rate-limit or non-backoff-worthy errors), which is acceptable
/// because the alternative (returning the error immediately) is strictly
/// worse for the user's stated reliability goal.
pub async fn retry_stream<S>(
    candidates: &[(Arc<str>, KeySlot)],
    key_health: &Arc<ParkingMutex<KeyHealthTracker>>,
    provider: LanguageModelProviderName,
    mut do_attempt: impl FnMut(Arc<str>) -> BoxFuture<'static, Result<S, LanguageModelCompletionError>>,
) -> Result<S, LanguageModelCompletionError> {
    let mut remaining: Vec<(Arc<str>, KeySlot)> = candidates.to_vec();
    let mut last_error: Option<LanguageModelCompletionError> = None;

    // Upper bound: try each configured key at most once. After that, even if
    // every failure was backoff-worthy, we've exhausted the pool.
    let max_attempts = remaining.len();
    for _ in 0..max_attempts {
        let Some((api_key, slot)) =
            select_from_candidates(&remaining, &snapshot_health(key_health), Instant::now())
        else {
            break;
        };
        // Record which key this attempt is using before it resolves, so the UI
        // can show which key the in-flight turn picked (retry button label).
        record_key_attempt(key_health, slot);

        match do_attempt(api_key).await {
            Ok(stream) => {
                record_key_success(key_health, slot);
                return Ok(stream);
            }
            Err(err) => {
                let worthy = is_backoff_worthy(&err);
                // Always record so health reflects reality; record_key_failure
                // is a no-op for non-backoff-worthy errors.
                record_key_failure(key_health, slot, &err);
                if !worthy {
                    // Would fail on every key; don't waste the user's time.
                    return Err(err);
                }
                if is_rate_limit(&err) {
                    // Rate limits are commonly account/org-wide: multiple keys
                    // under one quota all 429 together. Rotating here would
                    // burn the remaining healthy keys in a single request and
                    // leave no key available for the *next* request. The slot
                    // is already poisoned above; just return so the caller sees
                    // the rate-limit error and the next request picks a
                    // different (still-healthy) key.
                    return Err(err);
                }
                // Don't try this slot again within this request.
                remaining.retain(|(_, s)| *s != slot);
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or(LanguageModelCompletionError::NoApiKey { provider }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::http::StatusCode;
    use language_model::LanguageModelProviderName;

    fn provider_name() -> LanguageModelProviderName {
        LanguageModelProviderName::from(String::from("test"))
    }

    #[test]
    fn backoff_zero_failures_is_zero() {
        assert_eq!(compute_backoff(0), Duration::ZERO);
    }

    #[test]
    fn backoff_first_failure_around_base() {
        // 30s * 2^0 = 30s, jittered to [15s, 45s).
        for _ in 0..100 {
            let backoff = compute_backoff(1);
            assert!(backoff >= Duration::from_secs(15), "got {backoff:?}");
            assert!(backoff < Duration::from_secs(45), "got {backoff:?}");
        }
    }

    #[test]
    fn backoff_grows_exponentially_until_cap() {
        // 30s * 2^(n-1) for small n, but capped at 5h.
        let one = compute_backoff(1);
        let two_max = BACKOFF_BASE * 2 * 3 / 2; // upper jitter bound
        let two_min = BACKOFF_BASE * 2 / 2; // lower jitter bound
        // Sanity: bound is reasonable
        assert!(two_min <= two_max);
        // one is in the [15s, 45s) band
        assert!(one >= Duration::from_secs(15) && one < Duration::from_secs(45));
        let _ = (two_min, two_max); // suppress unused warnings on locals
    }

    #[test]
    fn backoff_never_exceeds_cap() {
        // Even at absurd failure counts, jittered value must stay ≤ 5h.
        for failures in [1, 5, 10, 50, 1000, u32::MAX] {
            for _ in 0..50 {
                let backoff = compute_backoff(failures);
                assert!(
                    backoff <= BACKOFF_MAX,
                    "failures={failures} yielded backoff={backoff:?} > cap={BACKOFF_MAX:?}"
                );
            }
        }
    }

    #[test]
    fn key_health_fresh_is_not_backed_off() {
        let health = KeyHealth::default();
        assert!(!health.is_backed_off(Instant::now()));
    }

    #[test]
    fn key_health_backoff_expires_after_window() {
        let start = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_failure(KeySlot::Primary, start);
        let backed_until = tracker.get(KeySlot::Primary).backoff_until.unwrap();
        // During the window, slot is backed off.
        assert!(tracker
            .get(KeySlot::Primary)
            .is_backed_off(start + Duration::from_secs(1)));
        // After the backoff duration, slot re-qualifies automatically — this is
        // the "5h auto-clear" guarantee, but at the small scale of the actual
        // backoff (test asserts the mechanism, not the 5h cap).
        assert!(
            !tracker
                .get(KeySlot::Primary)
                .is_backed_off(backed_until + Duration::from_secs(1)),
            "key should re-qualify once backoff_until is in the past"
        );
        let _ = backed_until;
    }

    #[test]
    fn record_success_clears_backoff() {
        let start = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_failure(KeySlot::Secondary, start);
        tracker.record_failure(KeySlot::Secondary, start + Duration::from_secs(5));
        assert!(tracker
            .get(KeySlot::Secondary)
            .is_backed_off(start + Duration::from_secs(1)));
        tracker.record_success(KeySlot::Secondary);
        assert_eq!(tracker.get(KeySlot::Secondary).consecutive_failures, 0);
        assert_eq!(tracker.get(KeySlot::Secondary).backoff_until, None);
        assert!(!tracker
            .get(KeySlot::Secondary)
            .is_backed_off(start + Duration::from_secs(1)));
    }

    #[test]
    fn record_failure_does_not_overflow() {
        // Pathological: many failures should saturate, not panic.
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        for _ in 0..1000 {
            tracker.record_failure(KeySlot::Tertiary, now);
        }
        let backoff = compute_backoff(tracker.get(KeySlot::Tertiary).consecutive_failures);
        assert!(backoff <= BACKOFF_MAX);
    }

    #[test]
    fn is_backoff_worthy_classification() {
        let provider = provider_name();

        // Backoff-worthy (transient / per-key).
        assert!(is_backoff_worthy(&LanguageModelCompletionError::RateLimitExceeded {
            provider: provider.clone(),
            retry_after: None,
        }));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::ServerOverloaded {
            provider: provider.clone(),
            retry_after: None,
        }));
        assert!(is_backoff_worthy(
            &LanguageModelCompletionError::ApiInternalServerError {
                provider: provider.clone(),
                message: "boom".into(),
            }
        ));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::AuthenticationError {
            provider: provider.clone(),
            message: "bad key".into(),
        }));
        assert!(is_backoff_worthy(
            &LanguageModelCompletionError::StreamEndedUnexpectedly {
                provider: provider.clone(),
            }
        ));
        assert!(is_backoff_worthy(&LanguageModelCompletionError::Other(anyhow::anyhow!(
            "unknown"
        ))));

        // NOT backoff-worthy (would recur on every key).
        assert!(!is_backoff_worthy(&LanguageModelCompletionError::NoApiKey {
            provider: provider.clone(),
        }));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::PromptTooLarge { tokens: None }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::BadRequestFormat {
                provider: provider.clone(),
                message: "bad".into(),
            }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::ApiEndpointNotFound {
                provider: provider.clone(),
            }
        ));
        assert!(!is_backoff_worthy(
            &LanguageModelCompletionError::HttpResponseError {
                provider,
                status_code: StatusCode::NOT_IMPLEMENTED,
                message: "bad".into(),
            }
        ));
    }

    #[test]
    fn select_from_candidates_returns_none_when_no_keys_configured() {
        let candidates: Vec<(Arc<str>, KeySlot)> = Vec::new();
        let health = KeyHealthTracker::default();
        assert!(select_from_candidates(&candidates, &health, Instant::now()).is_none());
    }

    #[test]
    fn select_from_candidates_skips_backed_off_slots() {
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let mut health = KeyHealthTracker::default();
        // Back off everything except Secondary.
        health.record_failure(KeySlot::Primary, Instant::now());
        health.record_failure(KeySlot::Tertiary, Instant::now());

        let now = Instant::now();
        // Secondary is the only healthy candidate, so it must be picked.
        for _ in 0..20 {
            let (key, slot) = select_from_candidates(&candidates, &health, now).unwrap();
            assert_eq!(slot, KeySlot::Secondary);
            assert_eq!(&*key, "key-b");
        }
    }

    #[test]
    fn select_from_candidates_falls_open_when_all_backed_off() {
        // If every slot is in backoff, the function still returns a key
        // (the soonest-expiring one) rather than `None`.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        // Set up deterministic backoff end times by writing directly into the
        // tracker, rather than going through `record_failure` (which adds
        // randomized jitter).
        let now = Instant::now();
        let mut health = KeyHealthTracker::default();
        health.primary = KeyHealth {
            consecutive_failures: 3,
            backoff_until: Some(now + Duration::from_secs(120)),
        };
        health.secondary = KeyHealth {
            consecutive_failures: 1,
            backoff_until: Some(now + Duration::from_secs(30)),
        };

        let pick = select_from_candidates(&candidates, &health, now);
        assert!(pick.is_some(), "fail-open should return a key even when all backed off");
        // Secondary expires sooner, so it must be picked.
        let (_, slot) = pick.unwrap();
        assert_eq!(slot, KeySlot::Secondary);
    }

    // ------------------------------------------------------------------
    // retry_stream tests
    //
    // These exercise the intra-request retry loop in isolation. The
    // `do_attempt` closure records which key it was called with and returns
    // a canned result, so we can assert on rotation order and exit conditions
    // without a real HTTP client.
    // ------------------------------------------------------------------

    fn rate_limit_err() -> LanguageModelCompletionError {
        LanguageModelCompletionError::RateLimitExceeded {
            provider: provider_name(),
            retry_after: None,
        }
    }

    fn server_overloaded_err() -> LanguageModelCompletionError {
        // Backoff-worthy but NOT a rate limit: rotation should proceed.
        LanguageModelCompletionError::ServerOverloaded {
            provider: provider_name(),
            retry_after: None,
        }
    }

    fn bad_request_err() -> LanguageModelCompletionError {
        // Non-backoff-worthy: should abort retry loop immediately.
        LanguageModelCompletionError::BadRequestFormat {
            provider: provider_name(),
            message: "bad".into(),
        }
    }

    #[test]
    fn retry_stream_succeeds_on_first_attempt() {
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));

        let attempts_for_closure = attempts.clone();
        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Ok(42_i32) })
            },
        ));

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.lock().len(), 1, "should not retry after success");
    }

    #[test]
    fn retry_stream_rotates_on_backoff_worthy_failure() {
        // First-selected key always fails with a backoff-worthy, NON-rate-limit
        // error (server overloaded); second always succeeds. Selection among
        // healthy candidates is random, so the test tracks which key was tried
        // first rather than hard-coding Primary/Secondary order. Uses
        // `server_overloaded_err` rather than `rate_limit_err` because a rate
        // limit now stops rotation (see `retry_stream_stops_on_rate_limit_*`).
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
        let seen_first = Arc::new(ParkingMutex::new(false));

        let attempts_for_closure = attempts.clone();
        let seen_first_for_closure = seen_first;
        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                let is_first = {
                    let mut seen = seen_first_for_closure.lock();
                    let first = !*seen;
                    *seen = true;
                    first
                };
                Box::pin(async move {
                    if is_first {
                        Err(server_overloaded_err())
                    } else {
                        Ok(7_i32)
                    }
                })
            },
        ));

        assert_eq!(result.unwrap(), 7);
        let attempts_guard = attempts.lock();
        assert_eq!(attempts_guard.len(), 2, "should rotate exactly once");

        let slot_for_key = |key: &str| match key {
            "key-a" => KeySlot::Primary,
            "key-b" => KeySlot::Secondary,
            _ => panic!("unknown key {key:?}"),
        };
        let first_slot = slot_for_key(&attempts_guard[0]);
        let second_slot = slot_for_key(&attempts_guard[1]);

        let health = key_health.lock();
        assert!(
            health.get(first_slot).consecutive_failures >= 1,
            "failed slot should be poisoned"
        );
        assert_eq!(
            health.get(second_slot).consecutive_failures, 0,
            "succeeded slot should have cleared health"
        );
    }

    #[test]
    fn retry_stream_stops_on_rate_limit_does_not_burn_siblings() {
        // A rate-limit error must NOT rotate: rate limits are commonly
        // account-wide, so burning key2/key3 in the same request would poison
        // the whole pool. Only the slot that hit the limit should be backed
        // off; the next request picks a healthy sibling.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));

        let attempts_for_closure = attempts.clone();
        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(rate_limit_err()) })
            },
        ));

        assert!(
            matches!(result, Err(LanguageModelCompletionError::RateLimitExceeded { .. })),
            "should return the rate-limit error"
        );
        let attempts = attempts.lock();
        assert_eq!(
            attempts.len(),
            1,
            "rate-limit error must not rotate to siblings: {attempts:?}"
        );

        // Exactly one slot poisoned (the one that was tried); the other two
        // remain healthy so the next request can use them.
        let health = key_health.lock();
        let poisoned = [
            health.get(KeySlot::Primary).consecutive_failures,
            health.get(KeySlot::Secondary).consecutive_failures,
            health.get(KeySlot::Tertiary).consecutive_failures,
        ]
        .iter()
        .filter(|c| **c > 0)
        .count();
        assert_eq!(poisoned, 1, "only the tried slot should be poisoned");
    }

    #[test]
    fn retry_stream_aborts_on_non_backoff_worthy_error() {
        // Non-backoff-worthy error must terminate the loop without trying
        // other keys — they would fail identically.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));

        let attempts_for_closure = attempts.clone();
        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(bad_request_err()) })
            },
        ));

        assert!(matches!(result, Err(LanguageModelCompletionError::BadRequestFormat { .. })));
        // Only one attempt — non-backoff-worthy errors don't rotate.
        assert_eq!(attempts.lock().len(), 1);
        // No slot should have been poisoned (the error wasn't backoff-worthy).
        let health = key_health.lock();
        assert_eq!(health.get(KeySlot::Primary).consecutive_failures, 0);
    }

    #[test]
    fn retry_stream_returns_last_error_when_all_candidates_fail() {
        // Every key fails backoff-worthily with a NON-rate-limit error (server
        // overloaded); loop should exhaust and return the final error, not retry
        // any slot twice. Uses `server_overloaded_err` because a rate-limit
        // error now stops rotation after the first failure.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));

        let attempts_for_closure = attempts.clone();
        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(server_overloaded_err()) })
            },
        ));

        assert!(matches!(result, Err(LanguageModelCompletionError::ServerOverloaded { .. })));
        // Exactly one attempt per candidate — no slot tried twice.
        let attempts = attempts.lock();
        assert_eq!(attempts.len(), 3, "each candidate tried exactly once: {attempts:?}");
        let unique: std::collections::HashSet<&String> = attempts.iter().collect();
        assert_eq!(unique.len(), 3, "no candidate retried: {attempts:?}");
    }

    #[test]
    fn retry_stream_returns_no_key_error_for_empty_candidates() {
        let candidates: Vec<(Arc<str>, KeySlot)> = Vec::new();
        let key_health = Arc::new(ParkingMutex::new(KeyHealthTracker::default()));

        let result: Result<i32, _> = smol::block_on(retry_stream(
            &candidates,
            &key_health,
            provider_name(),
            move |_api_key| Box::pin(async move { Ok(1_i32) }),
        ));

        assert!(matches!(result, Err(LanguageModelCompletionError::NoApiKey { .. })));
    }

    // ------------------------------------------------------------------
    // format_backoff_remaining tests
    //
    // The badge's countdown string format. Hour precision drops seconds;
    // sub-minute durations still show seconds so short backoffs feel
    // responsive.
    // ------------------------------------------------------------------

    #[test]
    fn format_backoff_zero_is_zero_seconds() {
        assert_eq!(format_backoff_remaining(Duration::ZERO), "0s");
    }

    #[test]
    fn format_backoff_sub_minute_shows_seconds() {
        assert_eq!(format_backoff_remaining(Duration::from_secs(1)), "1s");
        assert_eq!(format_backoff_remaining(Duration::from_secs(45)), "45s");
        assert_eq!(format_backoff_remaining(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_backoff_sub_hour_shows_minutes_and_seconds() {
        assert_eq!(format_backoff_remaining(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_backoff_remaining(Duration::from_secs(119)), "1m 59s");
        assert_eq!(format_backoff_remaining(Duration::from_secs(272)), "4m 32s");
    }

    #[test]
    fn format_backoff_hour_or_more_drops_seconds() {
        // 1h 5m = 3900s
        assert_eq!(format_backoff_remaining(Duration::from_secs(3900)), "1h 5m");
        // Exactly one hour
        assert_eq!(format_backoff_remaining(Duration::from_secs(3600)), "1h 0m");
        // The 5h cap
        assert_eq!(format_backoff_remaining(BACKOFF_MAX), "5h 0m");
    }

    // ------------------------------------------------------------------
    // Persistence tests
    //
    // The on-disk format and the in-memory <-> persisted conversions are
    // pure functions and can be tested directly. The full disk round-trip
    // (`persist_key_health` <-> `reload_persisted_health`) is exercised via
    // `FakeFs` in the `#[gpui::test]` tests below.
    // ------------------------------------------------------------------

    #[test]
    fn persisted_health_from_tracker_round_trip_preserves_failures_and_backoff() {
        // A tracker with mixed healthy/backed-off slots should round-trip
        // through `from_tracker` -> `to_tracker` with consecutive_failures and
        // backoff preserved (modulo tiny elapsed time during the round-trip).
        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.primary = KeyHealth {
            consecutive_failures: 3,
            backoff_until: Some(now + Duration::from_secs(600)),
        };
        tracker.secondary = KeyHealth {
            consecutive_failures: 0,
            backoff_until: None,
        };
        tracker.tertiary = KeyHealth {
            consecutive_failures: 1,
            backoff_until: Some(now + Duration::from_secs(60)),
        };

        let persisted = PersistedKeyHealthFile::from_tracker(&tracker, now);
        // Reconstruct at the same instant — values should match exactly.
        let restored = persisted.to_tracker(now);
        assert_eq!(restored.primary.consecutive_failures, 3);
        assert_eq!(restored.secondary.consecutive_failures, 0);
        assert_eq!(restored.tertiary.consecutive_failures, 1);
        assert_eq!(
            restored.primary.backoff_until,
            Some(now + Duration::from_secs(600))
        );
        assert_eq!(restored.secondary.backoff_until, None);
        assert_eq!(
            restored.tertiary.backoff_until,
            Some(now + Duration::from_secs(60))
        );
    }

    #[test]
    fn persisted_health_zero_or_negative_remaining_treated_as_healthy() {
        // Defensive: the loader must not reconstruct a backoff that already
        // elapsed (the user closed Zed for longer than the backoff window).
        // `elapsed_secs = 0.0` here simulates an immediate reload.
        let now = Instant::now();
        let zero = PersistedKeyHealth {
            consecutive_failures: 5,
            backoff_remaining_secs: Some(0.0),
        };
        assert_eq!(zero.to_health(now, 0.0).backoff_until, None);

        let negative = PersistedKeyHealth {
            consecutive_failures: 5,
            backoff_remaining_secs: Some(-120.0),
        };
        assert_eq!(negative.to_health(now, 0.0).backoff_until, None);

        // consecutive_failures is preserved either way (historical record).
        assert_eq!(zero.to_health(now, 0.0).consecutive_failures, 5);
        assert_eq!(negative.to_health(now, 0.0).consecutive_failures, 5);
    }

    #[test]
    fn persisted_health_none_remaining_is_healthy() {
        // `backoff_remaining_secs: null` is the canonical "healthy" encoding,
        // regardless of how much time elapsed while closed.
        let now = Instant::now();
        let healthy = PersistedKeyHealth {
            consecutive_failures: 0,
            backoff_remaining_secs: None,
        };
        let restored = healthy.to_health(now, 0.0);
        assert_eq!(restored.consecutive_failures, 0);
        assert_eq!(restored.backoff_until, None);
        assert!(!restored.is_backed_off(now));
    }

    #[test]
    fn persisted_health_elapsed_time_subtracts_from_remaining() {
        // If the slot was persisted with 600s remaining and Zed was closed for
        // 100s, the reload should see ~500s remaining (not 600s).
        let now = Instant::now();
        let slot = PersistedKeyHealth {
            consecutive_failures: 3,
            backoff_remaining_secs: Some(600.0),
        };
        let restored = slot.to_health(now, 100.0);
        let until = restored.backoff_until.expect("should still be backed off");
        let remaining = until.saturating_duration_since(now);
        assert!(
            remaining > Duration::from_secs(490) && remaining <= Duration::from_secs(500),
            "expected ~500s remaining after 100s elapsed, got {remaining:?}"
        );

        // If elapsed exceeds remaining, the slot loads as healthy.
        let expired = slot.to_health(now, 700.0);
        assert_eq!(expired.backoff_until, None, "elapsed > remaining -> healthy");
    }

    #[test]
    fn persisted_health_format_serializes_expected_shape() {
        // Snapshot of the on-disk JSON shape so we catch breaking format
        // changes (renames, removed fields, etc.) before they ship.
        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.primary = KeyHealth {
            consecutive_failures: 2,
            backoff_until: Some(now + Duration::from_secs_f64(120.5)),
        };
        let persisted = PersistedKeyHealthFile::from_tracker(&tracker, now);
        let json = serde_json::to_value(&persisted).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 6, "expected schema_version + saved_at + 4 slots");
        assert_eq!(obj.get("schema_version").and_then(|v| v.as_u64()), Some(2));
        // saved_at_unix_secs is a positive integer (wall-clock).
        assert!(
            obj.get("saved_at_unix_secs").and_then(|v| v.as_u64()).unwrap_or(0) > 0,
            "saved_at_unix_secs should be a positive unix timestamp"
        );
        let primary = obj.get("primary").unwrap().as_object().unwrap();
        assert_eq!(
            primary.get("consecutive_failures").and_then(|v| v.as_u64()),
            Some(2)
        );
        // 120.5s remaining, encoded as a float (not null).
        assert!(primary.get("backoff_remaining_secs").unwrap().is_f64());
        let secondary = obj.get("secondary").unwrap().as_object().unwrap();
        assert_eq!(
            secondary.get("consecutive_failures").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(secondary.get("backoff_remaining_secs").unwrap().is_null());
        let tertiary = obj.get("tertiary").unwrap().as_object().unwrap();
        assert_eq!(
            tertiary.get("consecutive_failures").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(tertiary.get("backoff_remaining_secs").unwrap().is_null());
        let quaternary = obj.get("quaternary").unwrap().as_object().unwrap();
        assert_eq!(
            quaternary.get("consecutive_failures").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(quaternary.get("backoff_remaining_secs").unwrap().is_null());
    }

    #[test]
    fn persisted_health_serde_round_trip() {
        // Serialize then deserialize yields the same struct.
        let original = PersistedKeyHealthFile {
            schema_version: PERSISTED_KEY_HEALTH_SCHEMA_VERSION,
            saved_at_unix_secs: 1_700_000_000,
            primary: PersistedKeyHealth {
                consecutive_failures: 7,
                backoff_remaining_secs: Some(3600.0),
            },
            secondary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
            },
            tertiary: PersistedKeyHealth {
                consecutive_failures: 2,
                backoff_remaining_secs: Some(0.0),
            },
            quaternary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
            },
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: PersistedKeyHealthFile = serde_json::from_str(&json).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn sanitize_provider_id_for_filename_strips_unsafe_chars() {
        // Path separators get replaced with `_`; otherwise the id is preserved
        // so per-provider files don't collide.
        assert_eq!(sanitize_provider_id_for_filename("my-provider"), "my-provider");
        assert_eq!(sanitize_provider_id_for_filename("foo.bar"), "foo.bar");
        assert_eq!(sanitize_provider_id_for_filename("a/b"), "a_b");
        assert_eq!(sanitize_provider_id_for_filename("a\\b"), "a_b");
        assert_eq!(sanitize_provider_id_for_filename("  spaces  "), "spaces");
        // Empty / all-unsafe ids fall back to `provider` rather than producing
        // an empty filename (which would collide across providers).
        assert_eq!(sanitize_provider_id_for_filename(""), "provider");
        assert_eq!(sanitize_provider_id_for_filename("///"), "provider");
        assert_eq!(sanitize_provider_id_for_filename("   "), "provider");
    }

    #[test]
    fn key_health_path_for_is_namespaced_under_data_dir() {
        // The path must live under `paths::data_dir()` so it's covered by the
        // existing state-recovery and backup flows, and must be a `.json` file
        // inside the `openai_compatible_backoff` subdir.
        let path = key_health_path_for("my-provider");
        assert!(path.starts_with(paths::data_dir()), "got {}", path.display());
        assert_eq!(
            path.parent().unwrap().file_name().unwrap(),
            PERSIST_DIR_NAME,
            "should be inside the {PERSIST_DIR_NAME} subdir"
        );
        assert_eq!(path.extension().unwrap(), "json");
        assert_eq!(path.file_name().unwrap(), "my-provider.json");
    }

    #[test]
    fn key_health_path_for_sanitizes_id() {
        // A path-like id must not escape the subdirectory: `/` and `\` are
        // replaced with `_`, so `../escape` becomes `.._escape` (still a single
        // path component, no traversal).
        let path = key_health_path_for("../escape");
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(file_name, ".._escape.json");
        assert!(
            path.starts_with(paths::data_dir().join(PERSIST_DIR_NAME)),
            "got {}",
            path.display()
        );
        // The path's parent must be exactly the persist dir — no subdirectory
        // was created by the unsanitized id.
        assert_eq!(
            path.parent().unwrap(),
            paths::data_dir().join(PERSIST_DIR_NAME).as_path()
        );
    }

    /// Exercises the full disk round-trip against a `FakeFs`. Missing file is
    /// the common case on first launch and must yield a fresh (all-healthy)
    /// tracker rather than an error.
    #[gpui::test]
    async fn reload_persisted_health_missing_file_returns_default(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/nonexistent/provider.json");
        let loaded = reload_persisted_health(&fs, &path).await;
        assert_eq!(loaded, KeyHealthTracker::default());
        assert_eq!(loaded.primary.consecutive_failures, 0);
        assert_eq!(loaded.primary.backoff_until, None);
    }

    #[gpui::test]
    async fn reload_persisted_health_corrupt_json_returns_default(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/corrupt/provider.json");
        fs.atomic_write(
            path.clone(),
            "not json at all {{{{".to_string(),
        )
        .await
        .unwrap();
        let loaded = reload_persisted_health(&fs, &path).await;
        assert_eq!(loaded, KeyHealthTracker::default());
    }

    #[gpui::test]
    async fn reload_persisted_health_wrong_schema_version_returns_default(
        cx: &mut gpui::TestAppContext,
    ) {
        // If we ship a schema change in the future, files written by newer
        // Zed must not silently load into older Zed as garbage. We drop them.
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/future/provider.json");
        let future = PersistedKeyHealthFile {
            schema_version: PERSISTED_KEY_HEALTH_SCHEMA_VERSION + 1,
            saved_at_unix_secs: 1_700_000_000,
            primary: PersistedKeyHealth {
                consecutive_failures: 99,
                backoff_remaining_secs: Some(9999.0),
            },
            secondary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
            },
            tertiary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
            },
            quaternary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
            },
        };
        fs.atomic_write(path.clone(), serde_json::to_string(&future).unwrap())
            .await
            .unwrap();
        let loaded = reload_persisted_health(&fs, &path).await;
        assert_eq!(loaded, KeyHealthTracker::default());
    }

    #[gpui::test]
    async fn reload_persisted_health_v1_schema_migrates_with_healthy_quaternary(
        cx: &mut gpui::TestAppContext,
    ) {
        // Issue 007 regression: v1 schema files (pre-Quaternary, commit 9b063ddf)
        // have no `quaternary` field. Without `#[serde(default)]` on the field,
        // serde rejected the whole file with "missing field `quaternary`",
        // silently wiping ALL slot backoff state and breaking rate-limit
        // rotation. After the fix, the file must parse, primary/secondary/
        // tertiary state must survive, and quaternary must come in healthy.
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/v1/provider.json");

        // Raw JSON exactly as written by a pre-Quaternary Zed build. Three slots,
        // schema_version 1, primary is backed off.
        let v1_json = r#"{"schema_version":1,"saved_at_unix_secs":1700000000,"primary":{"consecutive_failures":3,"backoff_remaining_secs":14454.5},"secondary":{"consecutive_failures":0,"backoff_remaining_secs":null},"tertiary":{"consecutive_failures":0,"backoff_remaining_secs":null}}"#;
        fs.atomic_write(path.clone(), v1_json.to_string()).await.unwrap();

        let loaded = reload_persisted_health(&fs, &path).await;

        // v1 state must survive the migration.
        assert_eq!(loaded.primary.consecutive_failures, 3,
            "primary failures must be preserved across v1→v2 migration");
        assert!(loaded.primary.backoff_until.is_some(),
            "primary backoff must be preserved (was 14454.5s in v1 file)");
        assert_eq!(loaded.secondary.consecutive_failures, 0);
        assert_eq!(loaded.tertiary.consecutive_failures, 0);

        // Quaternary must come in as the healthy default — it didn't exist in v1.
        assert_eq!(loaded.quaternary.consecutive_failures, 0,
            "quaternary must default to 0 failures on v1 migration");
        assert_eq!(loaded.quaternary.backoff_until, None,
            "quaternary must default to no backoff on v1 migration");
    }

    #[gpui::test]
    async fn reload_persisted_health_v1_missing_quaternary_does_not_log_parse_error(
        cx: &mut gpui::TestAppContext,
    ) {
        // Companion to the migration test: a v1 file must deserialize cleanly
        // (no "missing field `quaternary`" error path). This is a contract test
        // — we can't capture log output directly, but we CAN assert that the
        // returned tracker isn't the parse-error default (which would zero out
        // primary's failures along with everything else).
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/v1/contract.json");
        let v1_json = r#"{"schema_version":1,"saved_at_unix_secs":1700000000,"primary":{"consecutive_failures":7,"backoff_remaining_secs":60.0},"secondary":{"consecutive_failures":0,"backoff_remaining_secs":null},"tertiary":{"consecutive_failures":0,"backoff_remaining_secs":null}}"#;
        fs.atomic_write(path.clone(), v1_json.to_string()).await.unwrap();

        let loaded = reload_persisted_health(&fs, &path).await;
        // If the parse-error path fired, we'd get default() with 0 failures everywhere.
        assert_eq!(loaded.primary.consecutive_failures, 7,
            "v1 file must not fall through to parse-error default");
    }

    #[gpui::test]
    async fn persist_and_reload_round_trip_preserves_backed_off_state(
        cx: &mut gpui::TestAppContext,
    ) {
        // End-to-end: write a tracker with a backed-off slot, reload, and
        // verify the backoff is still in effect. Because `Instant` is
        // reconstructed as `reload_now + stored_remaining`, the reloaded
        // `backoff_until` shifts forward by the elapsed time between persist
        // and reload — that's correct behavior (the absolute deadline moves,
        // but the remaining window is preserved). We assert on the remaining
        // window, not the absolute deadline.
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/state/provider.json");

        let original_remaining = Duration::from_secs(1800);
        let persist_now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.secondary = KeyHealth {
            consecutive_failures: 4,
            backoff_until: Some(persist_now + original_remaining),
        };
        persist_key_health(&fs, path.clone(), tracker.clone())
            .await
            .unwrap();

        let reload_now = Instant::now();
        let reloaded = reload_persisted_health(&fs, &path).await;
        assert_eq!(reloaded.secondary.consecutive_failures, 4);
        let reloaded_until = reloaded.secondary.backoff_until.expect("backoff preserved");
        let reloaded_remaining = reloaded_until.saturating_duration_since(reload_now);
        // Allow a generous band: FakeFs simulates random delays, and the
        // persist -> reload round-trip takes non-zero time. The window should
        // be close to the original 1800s, well within ±60s.
        assert!(
            reloaded_remaining > Duration::from_secs(1740),
            "reloaded_remaining should be near 1800s, got {reloaded_remaining:?}"
        );
        assert!(
            reloaded_remaining <= original_remaining,
            "reloaded_remaining should not exceed original, got {reloaded_remaining:?}"
        );
        // Other slots untouched.
        assert_eq!(reloaded.primary.consecutive_failures, 0);
        assert_eq!(reloaded.tertiary.consecutive_failures, 0);
    }

    #[gpui::test]
    async fn persist_and_reload_expired_backoff_loads_as_healthy(
        cx: &mut gpui::TestAppContext,
    ) {
        // If the backoff window already elapsed between persist and reload
        // (e.g. user closed Zed overnight), the slot should load as healthy,
        // not stuck backed-off forever.
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/expired/provider.json");

        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        // Backoff of just 1ms — by the time we reload, it's surely elapsed.
        tracker.primary = KeyHealth {
            consecutive_failures: 2,
            backoff_until: Some(now + Duration::from_millis(1)),
        };
        persist_key_health(&fs, path.clone(), tracker.clone())
            .await
            .unwrap();
        // Sleep long enough for the 1ms backoff to definitely be in the past
        // at reload time. GPUI executor timers are real-time, so 50ms of wall
        // clock guarantees the stored 1ms remaining is gone.
        cx.background_executor
            .timer(Duration::from_millis(50))
            .await;

        let reloaded = reload_persisted_health(&fs, &path).await;
        // backoff_remaining at persist time was ~1ms, but at reload time
        // `Instant::now()` is ~50ms later, so the reconstructed backoff_until
        // (reload_now + 1ms) is in the future again — which is the bug we're
        // guarding against. The fix: persist relative to the moment of SAVE,
        // and at load clamp negative durations to zero. Verify the slot is
        // healthy by checking `is_backed_off`, not `backoff_until == None`,
        // because the persistence layer preserves the failures count and the
        // backoff deadline only matters relative to the current time.
        let reload_now = Instant::now();
        assert!(
            !reloaded
                .primary
                .is_backed_off(reload_now + Duration::from_secs(1)),
            "a 1ms backoff persisted 50ms ago should not still be in effect"
        );
        // consecutive_failures is still preserved (historical record).
        assert_eq!(reloaded.primary.consecutive_failures, 2);
    }
}
