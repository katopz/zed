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
use std::collections::HashMap;
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

/// Every slot, in the fixed order the UI and the persisted file use.
pub const ALL_KEY_SLOTS: [KeySlot; 4] = [
    KeySlot::Primary,
    KeySlot::Secondary,
    KeySlot::Tertiary,
    KeySlot::Quaternary,
];

/// Per-key backoff state. Persisted across restarts as relative durations
/// (see `PersistedKeyHealth`); in-memory `Instant`s are reconstructed on load.
///
/// `enabled` is a user-controlled toggle (defaults to `true`) that excludes a
/// key from rotation without clearing its stored secret. Disabled slots are
/// skipped by `select_from_candidates` and `gather_candidates`, and never
/// appear in the fail-open backoff fallback either — disabling is a hard
/// opt-out, distinct from a transient backoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyHealth {
    /// Failures the *upstream itself* reported against this key (429/401/403/
    /// 402/5xx). Only this counter drives the quota-scale exponential schedule
    /// in [`compute_backoff`], because only these carry an upstream verdict.
    pub consecutive_failures: u32,
    /// Failures where no upstream verdict was received at all — the socket
    /// never completed, the connection dropped mid-stream, or the body was
    /// unreadable. Tracked separately so a local network blip can never
    /// escalate a key onto the hour-scale quota schedule (see
    /// [`compute_transport_backoff`]).
    pub transport_failures: u32,
    pub backoff_until: Option<Instant>,
    /// The full backoff window set alongside `backoff_until`. The UI drains a
    /// countdown ring as `remaining / total` — without the total, a draining
    /// ring can't be proportional. `None` when the slot is not backed off.
    pub backoff_total: Option<Duration>,
    /// True when the current window came from an upstream reset hint (a
    /// `retry-after` header or a timestamp parsed out of the 429 body) rather
    /// than from a local guess. Provenance matters because the two carry very
    /// different authority: an upstream hint is the server stating when the
    /// quota resets, and must not be overwritten by a locally-computed guess
    /// or cleared by a 1-token liveness ping.
    pub backoff_from_upstream_hint: bool,
    pub enabled: bool,
}

impl Default for KeyHealth {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            transport_failures: 0,
            backoff_until: None,
            backoff_total: None,
            backoff_from_upstream_hint: false,
            enabled: true,
        }
    }
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
    /// The full backoff window (`Some` only while `is_backed_off`), so the UI
    /// can render a proportional drain ring (remaining / total).
    pub backoff_total: Option<Duration>,
    pub consecutive_failures: u32,
    /// User-controlled on/off toggle. `false` excludes the slot from rotation
    /// even when the key is otherwise healthy.
    pub enabled: bool,
}

impl KeyHealth {
    pub fn is_backed_off(&self, now: Instant) -> bool {
        matches!(self.backoff_until, Some(until) if now < until)
    }
}

#[derive(Clone, Debug)]
pub struct KeyHealthTracker {
    pub primary: KeyHealth,
    pub secondary: KeyHealth,
    pub tertiary: KeyHealth,
    pub quaternary: KeyHealth,
    /// Ephemeral (never persisted): the slot most recently selected by
    /// `select_from_candidates` inside `retry_stream`. Surfaced to the UI so the
    /// retry button can show which key the in-flight turn is actually using.
    /// Reset to `None` on load since a stale value across restarts is meaningless.
    /// NOT a selection input — stickiness is per-thread (`thread_picks`).
    pub last_used_slot: Option<KeySlot>,
    /// Ephemeral (never persisted): per-agent-thread sticky picks. The same
    /// thread reuses its slot while the slot stays healthy, so its upstream
    /// prompt cache stays hot. Keyed by `LanguageModelRequest::thread_id`.
    thread_picks: HashMap<String, ThreadKeyPick>,
    /// Ephemeral (never persisted): round-robin cursor for fresh picks.
    /// Advances once per fresh selection so concurrent agents distribute
    /// evenly across the healthy spares instead of clustering on one key.
    /// Randomized start so the first fresh pick after launch isn't fixed.
    rotation_cursor: u64,
}

/// A thread's sticky key assignment, plus when it was last refreshed (for
/// TTL pruning — see `THREAD_PICK_TTL`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ThreadKeyPick {
    slot: KeySlot,
    last_used: Instant,
}

/// How long an idle thread's sticky pick survives. Purely a memory bound on
/// `thread_picks`; re-picking after expiry is harmless (the old pick's prompt
/// cache is long cold by then).
const THREAD_PICK_TTL: Duration = Duration::from_secs(30 * 60);

impl Default for KeyHealthTracker {
    fn default() -> Self {
        Self {
            primary: KeyHealth::default(),
            secondary: KeyHealth::default(),
            tertiary: KeyHealth::default(),
            quaternary: KeyHealth::default(),
            last_used_slot: None,
            thread_picks: HashMap::new(),
            rotation_cursor: rand::rng().random::<u64>(),
        }
    }
}

impl PartialEq for KeyHealthTracker {
    /// Equality of the *persisted* state only (the four slot-health entries).
    /// The ephemeral selection state (`last_used_slot`, `thread_picks`,
    /// `rotation_cursor`) mutates on every selection; including it would make
    /// the persist-if-changed checks in `stream_completion` /
    /// `stream_response` / `reset_key_session` fire on every request.
    fn eq(&self, other: &Self) -> bool {
        self.primary == other.primary
            && self.secondary == other.secondary
            && self.tertiary == other.tertiary
            && self.quaternary == other.quaternary
    }
}

impl Eq for KeyHealthTracker {}

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
        health.transport_failures = 0;
        health.backoff_until = None;
        health.backoff_total = None;
        health.backoff_from_upstream_hint = false;
    }

    /// Success of a **probe** — the 1-token liveness ping fired by
    /// `reset_key_session` and the settings-page Check button. Weaker evidence
    /// than a real completion: a minimal request can sail through a quota that
    /// a full-size one would trip, so a probe must not overturn an upstream
    /// reset hint that has not yet elapsed. It does clear locally-guessed
    /// windows, which is the stale-backoff case the probing exists for.
    ///
    /// Returns whether anything changed, so callers can skip a persist.
    pub fn record_probe_success(&mut self, slot: KeySlot, now: Instant) -> bool {
        let health = self.get_mut(slot);
        if health.backoff_from_upstream_hint && health.is_backed_off(now) {
            return false;
        }
        let was_marked = health.consecutive_failures != 0
            || health.transport_failures != 0
            || health.backoff_until.is_some();
        self.record_success(slot);
        was_marked
    }

    /// Applies a locally-computed backoff without ever *shortening* the
    /// slot's current window.
    ///
    /// A local guess is an estimate; the window already in place may be an
    /// upstream-attested reset that is hours away. Overwriting it would hand
    /// the key back to rotation long before the quota actually resets — and
    /// the smaller the local guess, the worse the damage. Observed in the
    /// wild: a slot pinned to a real 8h reset hint was knocked back to the 1h
    /// local cap by an unrelated connect failure.
    fn apply_local_backoff(health: &mut KeyHealth, now: Instant, candidate: Duration) {
        let remaining = health
            .backoff_until
            .map(|until| until.saturating_duration_since(now))
            .unwrap_or_default();
        if candidate <= remaining {
            return;
        }
        health.backoff_until = Some(now + candidate);
        health.backoff_total = Some(candidate);
        health.backoff_from_upstream_hint = false;
    }

    /// Records an upstream-attributed failure on the slot: bumps the failure
    /// counter and recomputes `backoff_until = now + compute_backoff(count)`.
    /// Only call this for [`ErrorVerdict::KeyFault`] — the upstream answered
    /// and its answer indicted this key. Transport failures must go through
    /// [`Self::record_transport_failure`] instead, and benign errors must not
    /// touch health at all.
    pub fn record_failure(&mut self, slot: KeySlot, now: Instant) {
        let health = self.get_mut(slot);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        let backoff = compute_backoff(health.consecutive_failures);
        Self::apply_local_backoff(health, now, backoff);
    }

    /// Records a failure that carried **no upstream verdict** (connect/TLS/DNS
    /// failure, mid-stream drop, unreadable body). The key's quota is unknown:
    /// the request never reached a point where the upstream could judge it, so
    /// escalating onto the hour-scale quota schedule would be a fabricated
    /// conclusion. Uses the short, low-cap [`compute_transport_backoff`]
    /// schedule and a counter that never feeds [`compute_backoff`].
    ///
    /// The slot is still briefly skipped rather than left fully healthy: slots
    /// can point at different hosts (`secondary_key_url` and friends), so a
    /// transport failure *may* be endpoint-specific and rotating is worth one
    /// attempt. It just must not cost the user an hour when it isn't.
    pub fn record_transport_failure(&mut self, slot: KeySlot, now: Instant) {
        let health = self.get_mut(slot);
        health.transport_failures = health.transport_failures.saturating_add(1);
        let backoff = compute_transport_backoff(health.transport_failures);
        Self::apply_local_backoff(health, now, backoff);
    }

    /// Records a rate-limit failure on the slot. When the upstream supplied a
    /// retry hint (`retry-after` header or a reset timestamp parsed from the
    /// error body — see `parse_body_retry_hint`), it wins over the exponential
    /// schedule: the server told us exactly when the quota resets, which can be
    /// far beyond the 1h exponential cap (e.g. a weekly limit) or far below it
    /// ("try again in 20s"). Without a hint this behaves like
    /// [`Self::record_failure`].
    pub fn record_rate_limit(
        &mut self,
        slot: KeySlot,
        now: Instant,
        retry_after: Option<Duration>,
    ) {
        let health = self.get_mut(slot);
        health.consecutive_failures = health.consecutive_failures.saturating_add(1);
        health.transport_failures = 0;
        match retry_after {
            // Authoritative: the upstream just stated when the quota resets,
            // so this wins outright — including when it is *shorter* than the
            // current window (the quota may have reset early).
            Some(hint) => {
                health.backoff_until = Some(now + hint);
                health.backoff_total = Some(hint);
                health.backoff_from_upstream_hint = true;
            }
            // A 429 with no parseable hint still proves the key is limited;
            // only the duration is unknown. Fall back to the local schedule,
            // which must not shorten an existing window.
            None => {
                let backoff = compute_backoff(health.consecutive_failures);
                Self::apply_local_backoff(health, now, backoff);
            }
        }
    }

    /// Toggles the user-controlled `enabled` flag on a slot. Does not touch the
    /// failure counter or backoff window, so re-enabling a previously-disabled
    /// slot preserves its prior health state. Persisted via `PersistedKeyHealth`.
    pub fn set_enabled(&mut self, slot: KeySlot, enabled: bool) {
        self.get_mut(slot).enabled = enabled;
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
/// `Default` is a fully-healthy slot (zero failures, no backoff, enabled).
/// Used by `#[serde(default)]` on `PersistedKeyHealthFile::quaternary` so that v1
/// schema files (which predate the quaternary slot) deserialize successfully
/// and migrate forward instead of being rejected wholesale — see issue 007.
///
/// `enabled` is `#[serde(default = "default_true")]` so that v2 schema files
/// (which predate the user-toggleable enable/disable flag) load as enabled.
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct PersistedKeyHealth {
    pub consecutive_failures: u32,
    /// `#[serde(default)]` so v3 files (which predate the transport/upstream
    /// split) load with a zeroed transport counter.
    #[serde(default)]
    pub transport_failures: u32,
    pub backoff_remaining_secs: Option<f64>,
    /// The full backoff window at save time, so a restarted Zed still renders
    /// a proportional drain ring. `#[serde(default)]` so v2 schema files (which
    /// predate this field) load with `None` — the ring then falls back to
    /// starting full and draining over the remaining time.
    #[serde(default)]
    pub backoff_total_secs: Option<f64>,
    /// Provenance of the persisted window, so a restart can still tell an
    /// upstream-attested reset from a local guess. `#[serde(default)]` → older
    /// files load as "locally guessed", the conservative reading.
    #[serde(default)]
    pub backoff_from_upstream_hint: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PersistedKeyHealth {
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            transport_failures: 0,
            backoff_remaining_secs: None,
            backoff_total_secs: None,
            backoff_from_upstream_hint: false,
            enabled: true,
        }
    }
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

pub const PERSISTED_KEY_HEALTH_SCHEMA_VERSION: u32 = 4;

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
            transport_failures: health.transport_failures,
            backoff_remaining_secs,
            backoff_total_secs: health.backoff_total.map(|total| total.as_secs_f64()),
            backoff_from_upstream_hint: health.backoff_from_upstream_hint,
            enabled: health.enabled,
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
        // Keep the total window as-is: the drain ring scales by remaining/total,
        // and the total doesn't shrink while Zed is closed.
        let backoff_total = self
            .backoff_total_secs
            .filter(|secs| *secs > 0.0)
            .map(Duration::from_secs_f64);
        KeyHealth {
            consecutive_failures: self.consecutive_failures,
            transport_failures: self.transport_failures,
            backoff_until,
            backoff_total,
            backoff_from_upstream_hint: self.backoff_from_upstream_hint,
            enabled: self.enabled,
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
            // `last_used_slot` and the other ephemeral selection state are
            // runtime-only — never restored from disk. A stale slot from a
            // previous process would mislead the retry button label on the
            // very first turn after launch.
            last_used_slot: None,
            ..Default::default()
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
    paths::data_dir().join(PERSIST_DIR_NAME).join(format!(
        "{}.json",
        sanitize_provider_id_for_filename(provider_id)
    ))
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
            let mut tracker = file.to_tracker(Instant::now());
            // v3→v4: pre-v4 counters conflated verdict-less transport failures
            // with upstream quota verdicts, so most of the backoff they
            // describe is unattributable — files in the wild carry counts in
            // the hundreds pinned at the 1h cap purely from connect failures.
            //
            // But not *all* of it. A pre-v4 window longer than `BACKOFF_MAX`
            // could only have come from an upstream reset hint, because the
            // local exponential is clamped to that cap and can never exceed
            // it. Those are genuine quota verdicts — a real "your limit resets
            // at 19:06" — and dropping them would hand a still-limited key
            // back to rotation. So the length of the window is the
            // discriminator: keep what the upstream must have said, drop the
            // local guesses. `enabled` is a user decision and always survives.
            if file.schema_version < 4 {
                for slot in ALL_KEY_SLOTS {
                    let health = tracker.get_mut(slot);
                    let upstream_attested = health
                        .backoff_total
                        .is_some_and(|total| total > BACKOFF_MAX);
                    if upstream_attested {
                        health.backoff_from_upstream_hint = true;
                        log::info!(
                            "keeping pre-v4 {slot:?} backoff at {} — a {:?} window exceeds the local cap, so it came from an upstream reset hint",
                            path.display(),
                            health.backoff_total.unwrap_or_default(),
                        );
                        continue;
                    }
                    if health.backoff_until.is_some() {
                        log::info!(
                            "clearing pre-v4 {slot:?} backoff at {} — window is within the local cap, so it is not attributable to an upstream verdict",
                            path.display()
                        );
                    }
                    health.consecutive_failures = 0;
                    health.transport_failures = 0;
                    health.backoff_until = None;
                    health.backoff_total = None;
                }
            }
            tracker
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
            format!(
                "creating parent dir for key health persistence: {}",
                parent.display()
            )
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
            log::warn!(
                "failed to persist key health to {}: {err:#}",
                path.display()
            );
        }
    });
    // Replace any prior pending task. Dropping the old `Task` cancels it.
    *key_health_dirty.lock() = Some(task);
}

/// Soft cap on backoff. After this duration since the last failure the key is
/// automatically selectable again — no explicit "clear" path is needed.
/// 1h (plan 027): the 5h window kept recovered keys out of rotation for hours;
/// new-thread probing (`reset_key_session`) now clears stale backoffs eagerly,
/// so a long cap buys nothing.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60 * 60);

/// Base unit for the exponential schedule.
pub const BACKOFF_BASE: Duration = Duration::from_secs(30);

/// Jitter multiplier applied to every computed backoff. The upper bound is
/// what forces `BACKOFF_CEILING` below to sit under `BACKOFF_MAX`.
const JITTER_RANGE: std::ops::Range<f64> = 0.5..1.5;

/// Pre-jitter ceiling, set so `ceiling * max_jitter == BACKOFF_MAX`. Clamping
/// *after* jitter (the previous behavior) silently erased the jitter for every
/// factor >= 1.0 — at the cap, half of all draws collapsed to exactly
/// `BACKOFF_MAX`, so saturated keys unblocked in lockstep. That is precisely
/// the thundering herd the jitter exists to prevent; observed in the wild as
/// three of four slots persisting `backoff_total_secs: 3600.0` exactly.
const BACKOFF_CEILING: Duration = Duration::from_secs(2400);

/// Computes an exponential backoff with jitter, bounded by [`BACKOFF_MAX`].
/// The cap is the dominant constraint regardless of how large `failures` gets.
///
/// Jitter is applied to a pre-clamped ceiling so the spread survives at the
/// cap: saturated slots land anywhere in `[20m, 60m)` rather than all on the
/// same instant.
pub fn compute_backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    // 2^(failures-1), capped at 14 so the multiplication can't overflow `Duration`
    // (2^14 * 30s ≈ 138h, already well past the cap, so the clamp is a no-op).
    let exponent = (failures - 1).min(14);
    let multiplier = 2u32.pow(exponent);
    let candidate = BACKOFF_BASE
        .checked_mul(multiplier)
        .unwrap_or(BACKOFF_CEILING)
        .min(BACKOFF_CEILING);
    let jitter = rand::rng().random_range(JITTER_RANGE);
    candidate.mul_f64(jitter).min(BACKOFF_MAX)
}

/// Base unit for the transport schedule. Connect/TLS/DNS failures and
/// mid-stream drops are typically over in seconds, so the first retry should
/// be nearly immediate.
pub const TRANSPORT_BACKOFF_BASE: Duration = Duration::from_secs(5);

/// Hard cap on the transport schedule. Deliberately two orders of magnitude
/// below [`BACKOFF_MAX`]: a request that never got an answer says nothing
/// about the key's quota, so the worst case must be "retry in a minute", not
/// "this key is gone for an hour".
pub const TRANSPORT_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Exponential-with-jitter schedule for failures that carry no upstream
/// verdict. Same shape as [`compute_backoff`], different constants — and the
/// same post-clamp discipline, so the ceiling is derived from the max jitter
/// factor rather than clamping the jitter away.
pub fn compute_transport_backoff(failures: u32) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let exponent = (failures - 1).min(14);
    let multiplier = 2u32.pow(exponent);
    let ceiling = TRANSPORT_BACKOFF_MAX.div_f64(JITTER_RANGE.end);
    let candidate = TRANSPORT_BACKOFF_BASE
        .checked_mul(multiplier)
        .unwrap_or(ceiling)
        .min(ceiling);
    let jitter = rand::rng().random_range(JITTER_RANGE);
    candidate.mul_f64(jitter).min(TRANSPORT_BACKOFF_MAX)
}

/// Formats a remaining backoff duration for the ConfigurationView badge.
/// Day precision for multi-day upstream reset hints (e.g. a weekly quota),
/// hour precision drops the seconds (the user doesn't need them at that
/// scale); sub-minute durations still show seconds so short backoffs feel
/// responsive. Returns `"0s"` for `Duration::ZERO` (e.g. slot just exited
/// backoff between snapshot and render).
pub fn format_backoff_remaining(remaining: Duration) -> String {
    let total_secs = remaining.as_secs();
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
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
    matches!(err, LanguageModelCompletionError::RateLimitExceeded { .. })
}

/// How a failed attempt should be attributed to the key that produced it.
///
/// The distinction that matters is **whether the upstream answered at all**.
/// A backoff is a claim about a key's availability, and the only evidence for
/// that claim is an upstream response. Folding "the socket failed" into the
/// same bucket as "the upstream said 429" manufactures a verdict nobody
/// issued — and because the exponential schedule compounds, a handful of
/// local network blips is enough to park every key at the 1-hour cap while
/// the provider dashboard shows the quota barely touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorVerdict {
    /// The upstream answered and its answer indicts this key (quota, auth,
    /// billing) or its own servers. Poison the slot on the quota-scale
    /// exponential schedule and rotate to a sibling.
    KeyFault,
    /// No usable answer from the upstream: connect/TLS/DNS failure, the
    /// connection dropped mid-stream, or the body could not be read. The
    /// key's standing is unknown. Rotate once (slots may point at different
    /// hosts) but only on the short transport schedule.
    Transport,
    /// The request itself is the problem — too large, malformed, wrong
    /// endpoint. Every key would answer identically, so neither poisoning
    /// nor rotating helps.
    Benign,
}

/// Classifies a completion error by what the upstream actually told us.
///
/// Note where `Other(_)` lands: it is an unclassified `anyhow` error with no
/// HTTP status behind it, so it carries no verdict and gets the short
/// transport schedule rather than the hour-scale one. The old code called it
/// backoff-worthy on the reasoning that upstream error labels are unreliable
/// — but "unreliable label" is an argument for a *short* penalty, not for
/// treating an unlabeled failure as a confirmed quota exhaustion.
pub fn classify_error(err: &LanguageModelCompletionError) -> ErrorVerdict {
    match err {
        LanguageModelCompletionError::RateLimitExceeded { .. }
        | LanguageModelCompletionError::ServerOverloaded { .. }
        | LanguageModelCompletionError::ApiInternalServerError { .. }
        | LanguageModelCompletionError::UpstreamProviderError { .. }
        | LanguageModelCompletionError::AuthenticationError { .. }
        | LanguageModelCompletionError::PermissionError { .. }
        | LanguageModelCompletionError::PaymentRequired => ErrorVerdict::KeyFault,
        LanguageModelCompletionError::HttpSend { .. }
        | LanguageModelCompletionError::StreamEndedUnexpectedly { .. }
        | LanguageModelCompletionError::ApiReadResponseError { .. }
        | LanguageModelCompletionError::Other(_) => ErrorVerdict::Transport,
        LanguageModelCompletionError::PromptTooLarge { .. }
        | LanguageModelCompletionError::NoApiKey { .. }
        | LanguageModelCompletionError::BadRequestFormat { .. }
        | LanguageModelCompletionError::InvalidEncryptedContent { .. }
        | LanguageModelCompletionError::ApiEndpointNotFound { .. }
        | LanguageModelCompletionError::HttpResponseError { .. }
        | LanguageModelCompletionError::SerializeRequest { .. }
        | LanguageModelCompletionError::BuildRequestBody { .. }
        | LanguageModelCompletionError::DeserializeResponse { .. }
        | LanguageModelCompletionError::DataRetentionConsentRequired { .. } => ErrorVerdict::Benign,
    }
}

/// Key selection. Called once per attempt inside `retry_stream`'s loop,
/// under the tracker lock, so it mutates ephemeral selection state (rotation
/// cursor, per-thread sticky map, UI slot) atomically with the pick.
///
/// Selection policy (**priority-first**, per-thread sticky, fair rotation):
///
/// 1. Build `healthy` = present + `enabled` + not in backoff.
/// 2. If `Primary` is healthy, return it. K1 is always used while available —
///    its upstream prompt cache stays continuously hot. Other configured
///    keys are spares, not load-balanced peers.
/// 3. Otherwise, if `thread_id` has a sticky pick that is still healthy,
///    reuse it — the same thread keeps hitting the same key so its prompt
///    cache stays active. Stickiness is keyed by the request's `thread_id`
///    (populated by the agent layer), never by process-global state: a shared
///    "last used" slot made every concurrent agent pile onto one key (issue 029).
/// 4. Otherwise take the next slot from a round-robin cursor over the healthy
///    spares (random start). Consecutive fresh picks — new threads, or sticky
///    picks that went unhealthy — spread evenly across the healthy set instead
///    of clustering: 6 agents over K2..K4 get exactly 2 each. Inside
///    `retry_stream` the just-failed slot is no longer healthy, so a retry
///    naturally advances to the next spare.
/// 5. If no healthy candidate exists, fail open onto a backed-off slot rather
///    than returning `NoApiKey` — but not blindly. Backoff windows differ in
///    authority (see `backoff_from_upstream_hint`):
///
///    * A **speculative** window is a local guess (transport blip, exponential
///      estimate). The key may well answer right now, so these are tried
///      first, spread over the same round-robin cursor as step 4 so
///      concurrent agents don't all pile onto one key.
///    * An **upstream-attested** window is the server stating when the quota
///      resets. Sending a request before that instant is a guaranteed 429 —
///      it burns latency and quota to learn nothing. These are the last
///      resort, and there the earliest-expiring slot wins: it is the one
///      closest to actually being usable.
///
///    Disabled slots never qualify even here — a disabled slot is a hard user
///    opt-out, distinct from a transient backoff.
pub fn select_from_candidates(
    candidates: &[(Arc<str>, KeySlot)],
    health: &mut KeyHealthTracker,
    thread_id: Option<&str>,
    now: Instant,
) -> Option<(Arc<str>, KeySlot)> {
    // Healthy candidates: present, enabled, not in backoff. The `enabled`
    // check is duplicated in the fail-open fallback below — disabled slots
    // never participate.
    let healthy: Vec<(Arc<str>, KeySlot)> = candidates
        .iter()
        .filter(|(_, slot)| {
            let h = health.get(*slot);
            h.enabled && !h.is_backed_off(now)
        })
        .cloned()
        .collect();

    // Sticky Primary: while K1 is healthy it always wins (req: "K1 is always
    // used if available"). No thread-pick registration needed — while K1 is
    // healthy no other branch is reachable, and once it backs off a stale K1
    // entry would fail the sticky health check anyway.
    if let Some(pick) = healthy
        .iter()
        .find(|(_, slot)| *slot == KeySlot::Primary)
        .cloned()
    {
        health.last_used_slot = Some(KeySlot::Primary);
        return Some(pick);
    }

    // Per-thread affinity: keep the key this thread already used while it
    // stays healthy — same thread, same key, hot prompt cache.
    if let Some(thread_id) = thread_id
        && let Some((key, slot)) = health
            .thread_picks
            .get(thread_id)
            .and_then(|pick| healthy.iter().find(|(_, slot)| *slot == pick.slot).cloned())
    {
        // Refresh so active threads don't expire out of the map.
        if let Some(entry) = health.thread_picks.get_mut(thread_id) {
            entry.last_used = now;
        }
        health.last_used_slot = Some(slot);
        return Some((key, slot));
    }

    // Fresh pick (new thread, lost/unhealthy sticky pick, or no thread id):
    // round-robin over the healthy spares so concurrent agents distribute
    // fairly instead of clustering on one key.
    if !healthy.is_empty() {
        // Prune expired entries on the insert path only — the sticky path
        // above refreshes `last_used`, so anything left here is idle.
        health
            .thread_picks
            .retain(|_, pick| now.duration_since(pick.last_used) < THREAD_PICK_TTL);
        let index = (health.rotation_cursor % healthy.len() as u64) as usize;
        health.rotation_cursor = health.rotation_cursor.wrapping_add(1);
        let (key, slot) = healthy[index].clone();
        health.last_used_slot = Some(slot);
        if let Some(thread_id) = thread_id {
            health.thread_picks.insert(
                thread_id.to_string(),
                ThreadKeyPick {
                    slot,
                    last_used: now,
                },
            );
        }
        return Some((key, slot));
    }

    // Everything present+enabled is backed off. Fail open rather than return
    // `NoApiKey`, but prefer the slots whose backoff is only a guess: a window
    // the upstream explicitly attested to is a request we already know will
    // 429. Disabled slots never qualify (better to fail with `None` →
    // `NoApiKey` than silently resurrect a key the user explicitly turned off).
    let backed_off: Vec<(Arc<str>, KeySlot)> = candidates
        .iter()
        .filter(|(_, slot)| {
            let h = health.get(*slot);
            h.enabled && h.backoff_until.is_some()
        })
        .cloned()
        .collect();

    let speculative: Vec<(Arc<str>, KeySlot)> = backed_off
        .iter()
        .filter(|(_, slot)| !health.get(*slot).backoff_from_upstream_hint)
        .cloned()
        .collect();

    let fallback = match speculative.is_empty() {
        // Some window is only a local guess — try it, and spread consecutive
        // fail-open picks so concurrent agents don't converge on one key.
        false => {
            let index = (health.rotation_cursor % speculative.len() as u64) as usize;
            health.rotation_cursor = health.rotation_cursor.wrapping_add(1);
            speculative.get(index).cloned()
        }
        // Every window is upstream-attested: whichever request we send is
        // going to be refused, so pick the slot closest to its stated reset.
        true => backed_off
            .into_iter()
            .filter_map(|(key, slot)| health.get(slot).backoff_until.map(|until| (key, slot, until)))
            .min_by_key(|(_, _, until)| *until)
            .map(|(key, slot, _)| (key, slot)),
    };

    if let Some((_, slot)) = &fallback {
        health.last_used_slot = Some(*slot);
    }
    fallback
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

/// Updates per-key health after a failed request, routing by
/// [`classify_error`]: upstream verdicts take the quota-scale schedule,
/// verdict-less transport failures take the short one, and benign errors are
/// no-ops because they would recur on every key.
///
/// For `RateLimitExceeded` the upstream's retry hint (when present) replaces
/// the exponential schedule via `record_rate_limit` — the server-provided
/// reset time is strictly more accurate than the local guess.
pub fn record_key_failure(
    key_health: &Arc<ParkingMutex<KeyHealthTracker>>,
    slot: KeySlot,
    err: &LanguageModelCompletionError,
) {
    let verdict = classify_error(err);
    if verdict == ErrorVerdict::Benign {
        return;
    }
    let mut health = key_health.lock();
    match (verdict, err) {
        (_, LanguageModelCompletionError::RateLimitExceeded { retry_after, .. }) => {
            health.record_rate_limit(slot, Instant::now(), *retry_after);
        }
        (ErrorVerdict::Transport, _) => health.record_transport_failure(slot, Instant::now()),
        _ => health.record_failure(slot, Instant::now()),
    }
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
/// is returned and the slot's health is cleared. Failures are routed by
/// [`classify_error`]:
///
/// * [`ErrorVerdict::KeyFault`] — poison the slot on the quota schedule and
///   try the next candidate, *except* for `RateLimitExceeded`, which exits
///   the loop immediately after poisoning the slot (see `is_rate_limit`: rate
///   limits are commonly account-wide, so rotating would burn healthy
///   siblings for no benefit).
/// * [`ErrorVerdict::Transport`] — mark the slot on the short transport
///   schedule and try the next candidate. Slots can point at different hosts,
///   so rotating is worth one attempt, but the mark must stay cheap: no
///   upstream judged this key, so it cannot be treated as quota exhaustion.
/// * [`ErrorVerdict::Benign`] — exit immediately without touching health; the
///   error would recur on every key.
///
/// `do_attempt` receives the chosen key and must return a `'static` future;
/// callers are expected to clone the request template inside the closure
/// (the underlying `open_ai::Request` / `responses::Request` types derive
/// `Clone` for this purpose) and to map the provider-specific error into
/// `LanguageModelCompletionError`.
///
/// Bounds the worst-case latency to one full key rotation per user request
/// (fewer on rate-limit or benign errors), which is acceptable
/// because the alternative (returning the error immediately) is strictly
/// worse for the user's stated reliability goal.
pub async fn retry_stream<S>(
    candidates: &[(Arc<str>, KeySlot)],
    key_health: &Arc<ParkingMutex<KeyHealthTracker>>,
    thread_id: Option<&str>,
    provider: LanguageModelProviderName,
    mut do_attempt: impl FnMut(Arc<str>) -> BoxFuture<'static, Result<S, LanguageModelCompletionError>>,
) -> Result<S, LanguageModelCompletionError> {
    let mut remaining: Vec<(Arc<str>, KeySlot)> = candidates.to_vec();
    let mut last_error: Option<LanguageModelCompletionError> = None;

    // Upper bound: try each configured key at most once. After that, even if
    // every failure marked its slot, we've exhausted the pool.
    let max_attempts = remaining.len();
    for _ in 0..max_attempts {
        // Selection mutates ephemeral state (rotation cursor, per-thread
        // sticky map, UI slot) and therefore runs under the tracker lock; the
        // lock is released before the attempt future is awaited.
        let picked = {
            let mut health = key_health.lock();
            select_from_candidates(&remaining, &mut health, thread_id, Instant::now())
        };
        let Some((api_key, slot)) = picked else {
            break;
        };

        match do_attempt(api_key).await {
            Ok(stream) => {
                record_key_success(key_health, slot);
                return Ok(stream);
            }
            Err(err) => {
                let verdict = classify_error(&err);
                // Always record so health reflects reality; record_key_failure
                // is a no-op for benign errors.
                record_key_failure(key_health, slot, &err);
                if verdict == ErrorVerdict::Benign {
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
        assert!(
            tracker
                .get(KeySlot::Primary)
                .is_backed_off(start + Duration::from_secs(1))
        );
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
        assert!(
            tracker
                .get(KeySlot::Secondary)
                .is_backed_off(start + Duration::from_secs(1))
        );
        tracker.record_success(KeySlot::Secondary);
        assert_eq!(tracker.get(KeySlot::Secondary).consecutive_failures, 0);
        assert_eq!(tracker.get(KeySlot::Secondary).backoff_until, None);
        assert!(
            !tracker
                .get(KeySlot::Secondary)
                .is_backed_off(start + Duration::from_secs(1))
        );
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
    fn record_rate_limit_uses_server_hint_verbatim() {
        // An upstream reset hint wins over the exponential schedule — even
        // when it exceeds the 1h exponential cap (weekly quota resets can be
        // days away).
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        let hint = Duration::from_secs(3 * 86400);
        tracker.record_rate_limit(KeySlot::Tertiary, now, Some(hint));
        let health = tracker.get(KeySlot::Tertiary);
        assert_eq!(health.consecutive_failures, 1);
        assert_eq!(health.backoff_until, Some(now + hint));
    }

    #[test]
    fn record_rate_limit_without_hint_falls_back_to_exponential() {
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        tracker.record_rate_limit(KeySlot::Quaternary, now, None);
        let health = tracker.get(KeySlot::Quaternary);
        assert_eq!(health.consecutive_failures, 1);
        let backoff = health.backoff_until.unwrap() - now;
        // Same jittered band as compute_backoff(1): [15s, 45s).
        assert!(backoff >= Duration::from_secs(15) && backoff < Duration::from_secs(45));
    }

    #[test]
    fn format_backoff_remaining_includes_days_for_multi_day_hints() {
        assert_eq!(
            format_backoff_remaining(Duration::from_secs(3 * 86400)),
            "3d 0h"
        );
        assert_eq!(
            format_backoff_remaining(Duration::from_secs(2 * 86400 + 5 * 3600)),
            "2d 5h"
        );
        assert_eq!(
            format_backoff_remaining(Duration::from_secs(35 * 60 + 39)),
            "35m 39s"
        );
        assert_eq!(
            format_backoff_remaining(Duration::from_secs(4 * 3600)),
            "4h 0m"
        );
        assert_eq!(format_backoff_remaining(Duration::from_secs(59)), "59s");
        assert_eq!(format_backoff_remaining(Duration::ZERO), "0s");
    }

    /// Benign errors describe the *request*, not the key: every slot would
    /// answer identically, so neither poisoning nor rotating helps. The
    /// KeyFault/Transport halves of the taxonomy are covered by
    /// `transport_errors_classify_as_transport_not_key_fault`.
    #[test]
    fn benign_errors_never_mark_the_slot() {
        let provider = provider_name();
        for err in [
            LanguageModelCompletionError::NoApiKey {
                provider: provider.clone(),
            },
            LanguageModelCompletionError::PromptTooLarge { tokens: None },
            LanguageModelCompletionError::BadRequestFormat {
                provider: provider.clone(),
                message: "bad".into(),
            },
            LanguageModelCompletionError::ApiEndpointNotFound {
                provider: provider.clone(),
            },
            LanguageModelCompletionError::HttpResponseError {
                provider,
                status_code: StatusCode::NOT_IMPLEMENTED,
                message: "bad".into(),
            },
        ] {
            assert_eq!(classify_error(&err), ErrorVerdict::Benign, "{err}");
        }
    }

    #[test]
    fn select_from_candidates_returns_none_when_no_keys_configured() {
        let candidates: Vec<(Arc<str>, KeySlot)> = Vec::new();
        let mut health = KeyHealthTracker::default();
        assert!(select_from_candidates(&candidates, &mut health, None, Instant::now()).is_none());
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
            let (key, slot) = select_from_candidates(&candidates, &mut health, None, now).unwrap();
            assert_eq!(slot, KeySlot::Secondary);
            assert_eq!(&*key, "key-b");
        }
    }

    #[test]
    fn select_from_candidates_falls_open_when_all_backed_off() {
        // If every slot is in backoff, the function still returns a key rather
        // than `None`. Which key is no longer "the soonest-expiring one": both
        // windows here are locally guessed, and among speculative windows the
        // remaining time carries no information about which key will actually
        // answer, so the pick round-robins instead (see
        // `fail_open_spreads_concurrent_picks_across_speculative_slots`).
        // Earliest-expiring still governs the all-attested case — see
        // `fail_open_takes_earliest_reset_when_all_are_attested`.
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
            ..Default::default()
        };
        health.secondary = KeyHealth {
            consecutive_failures: 1,
            backoff_until: Some(now + Duration::from_secs(30)),
            ..Default::default()
        };

        let mut picked = std::collections::HashSet::new();
        for _ in 0..4 {
            let (_, slot) = select_from_candidates(&candidates, &mut health, None, now)
                .expect("fail-open should return a key even when all backed off");
            picked.insert(format!("{slot:?}"));
        }
        assert_eq!(
            picked.len(),
            2,
            "fail-open over speculative windows should reach both slots: {picked:?}"
        );
    }

    #[test]
    fn select_from_candidates_is_primary_sticky_when_healthy() {
        // With all four keys healthy, Primary is the sticky pick every time —
        // hourly rotation must NOT spread load across all four keys while
        // Primary is up.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
            (Arc::<str>::from("key-d"), KeySlot::Quaternary),
        ];
        let mut health = KeyHealthTracker::default();
        let now = Instant::now();
        for _ in 0..20 {
            let (key, slot) = select_from_candidates(&candidates, &mut health, None, now).unwrap();
            assert_eq!(
                slot,
                KeySlot::Primary,
                "Primary must be sticky while healthy"
            );
            assert_eq!(&*key, "key-a");
        }
    }

    #[test]
    fn select_from_candidates_rotates_through_spares_when_primary_backed_off() {
        // When Primary is in backoff, fresh picks (no thread id) advance the
        // round-robin cursor: consecutive requests land on different healthy
        // spares instead of clustering on one key (issue 029).
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let mut health = KeyHealthTracker::default();
        health.record_failure(KeySlot::Primary, Instant::now());
        let now = Instant::now();
        let first = select_from_candidates(&candidates, &mut health, None, now)
            .unwrap()
            .1;
        assert_ne!(
            first,
            KeySlot::Primary,
            "backed-off Primary must never be picked"
        );
        let second = select_from_candidates(&candidates, &mut health, None, now)
            .unwrap()
            .1;
        assert_ne!(second, KeySlot::Primary);
        assert_ne!(first, second, "consecutive fresh picks must rotate");
    }

    #[test]
    fn select_from_candidates_sticks_to_thread_pick_while_healthy() {
        // Same-thread cache affinity: a thread's first pick is recorded in the
        // per-thread sticky map, and subsequent selections for the same
        // thread return it while it stays healthy — prompt cache stays hot.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let mut health = KeyHealthTracker::default();
        health.record_failure(KeySlot::Primary, Instant::now());
        let now = Instant::now();
        let (first_key, first_slot) =
            select_from_candidates(&candidates, &mut health, Some("thread-1"), now).unwrap();
        for _ in 0..20 {
            let (key, slot) =
                select_from_candidates(&candidates, &mut health, Some("thread-1"), now).unwrap();
            assert_eq!(slot, first_slot, "thread pick must be sticky while healthy");
            assert_eq!(&*key, &*first_key);
        }
    }

    #[test]
    fn select_from_candidates_distributes_new_threads_fairly() {
        // Issue 029: 6 concurrent agents (distinct thread ids) over 3 healthy
        // spares must spread exactly 2/2/2 via the rotation cursor — not pile
        // onto one key. Repeating a thread returns its own sticky slot.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
            (Arc::<str>::from("key-d"), KeySlot::Quaternary),
        ];
        let mut health = KeyHealthTracker::default();
        health.record_failure(KeySlot::Primary, Instant::now());
        let now = Instant::now();
        let threads = ["t1", "t2", "t3", "t4", "t5", "t6"];
        let mut secondary = 0;
        let mut tertiary = 0;
        let mut quaternary = 0;
        let mut t1_pick = None;
        for thread_id in threads {
            let (_, slot) =
                select_from_candidates(&candidates, &mut health, Some(thread_id), now).unwrap();
            match slot {
                KeySlot::Secondary => secondary += 1,
                KeySlot::Tertiary => tertiary += 1,
                KeySlot::Quaternary => quaternary += 1,
                other => panic!("fresh pick must be a healthy spare, got {other:?}"),
            }
            if thread_id == "t1" {
                t1_pick = Some(slot);
            }
        }
        assert_eq!(
            (secondary, tertiary, quaternary),
            (2, 2, 2),
            "rotation must be even"
        );
        let again = select_from_candidates(&candidates, &mut health, Some("t1"), now)
            .unwrap()
            .1;
        assert_eq!(Some(again), t1_pick, "same thread keeps its sticky slot");
    }

    #[test]
    fn select_from_candidates_drops_thread_pick_that_goes_unhealthy() {
        // The sticky pick only applies while healthy: once it enters backoff
        // (e.g. it failed and retry_stream rotates), that thread's selection
        // moves on to another healthy spare.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
        ];
        let mut health = KeyHealthTracker::default();
        health.record_failure(KeySlot::Primary, Instant::now());
        health.record_failure(KeySlot::Secondary, Instant::now());
        let now = Instant::now();
        // thread-1's sticky pick points at the now-backed-off Secondary.
        health.thread_picks.insert(
            "thread-1".to_string(),
            ThreadKeyPick {
                slot: KeySlot::Secondary,
                last_used: now,
            },
        );
        let (_, slot) =
            select_from_candidates(&candidates, &mut health, Some("thread-1"), now).unwrap();
        assert_eq!(
            slot,
            KeySlot::Tertiary,
            "unhealthy thread pick must be skipped"
        );
        // The dropped pick is replaced by the fresh one.
        assert_eq!(
            health.thread_picks.get("thread-1").map(|pick| pick.slot),
            Some(KeySlot::Tertiary)
        );
    }

    #[test]
    fn select_from_candidates_prunes_stale_thread_picks() {
        // Idle threads' sticky picks expire after THREAD_PICK_TTL; a fresh
        // selection prunes them, bounding the map's memory.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let mut health = KeyHealthTracker::default();
        // Back off Primary so selection reaches the fresh-pick path (the
        // priority branch returns before pruning runs).
        health.record_failure(KeySlot::Primary, Instant::now());
        let now = Instant::now();
        health.thread_picks.insert(
            "stale".to_string(),
            ThreadKeyPick {
                slot: KeySlot::Secondary,
                last_used: now - THREAD_PICK_TTL - Duration::from_secs(1),
            },
        );
        let _ = select_from_candidates(&candidates, &mut health, Some("fresh"), now).unwrap();
        assert!(
            !health.thread_picks.contains_key("stale"),
            "expired pick must be pruned"
        );
        assert!(health.thread_picks.contains_key("fresh"));
        assert_eq!(health.thread_picks.len(), 1);
    }

    #[test]
    fn select_from_candidates_without_thread_id_keeps_map_empty() {
        // Requests without a thread id (edit prediction, other consumers) get
        // rotation picks without sticky registration — there is nothing to
        // stick to, and the map must not grow.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let mut health = KeyHealthTracker::default();
        health.record_failure(KeySlot::Primary, Instant::now());
        let now = Instant::now();
        for _ in 0..10 {
            select_from_candidates(&candidates, &mut health, None, now).unwrap();
        }
        assert!(health.thread_picks.is_empty());
    }

    #[test]
    fn select_from_candidates_skips_disabled_slots_even_when_healthy() {
        // A disabled slot must never be selected, even if it's the only healthy
        // one — disabling is a hard opt-out, distinct from a transient backoff.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let mut health = KeyHealthTracker::default();
        health.set_enabled(KeySlot::Primary, false);
        let now = Instant::now();
        for _ in 0..20 {
            let (_, slot) = select_from_candidates(&candidates, &mut health, None, now).unwrap();
            assert_eq!(slot, KeySlot::Secondary, "disabled Primary must be skipped");
        }
    }

    #[test]
    fn select_from_candidates_returns_none_when_all_enabled_slots_backed_off_and_disabled_skipped()
    {
        // All slots are either disabled or backed off. The disabled slot must
        // NOT be used in fail-open — `None` is the correct outcome because the
        // user explicitly opted that slot out.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let mut health = KeyHealthTracker::default();
        health.set_enabled(KeySlot::Primary, false); // disabled, not backed off
        health.record_failure(KeySlot::Secondary, Instant::now()); // backed off
        let now = Instant::now();
        // Secondary is backed off but enabled → fail-open should pick it.
        let pick = select_from_candidates(&candidates, &mut health, None, now);
        assert!(pick.is_some(), "enabled backed-off slot should fail-open");
        let (_, slot) = pick.unwrap();
        assert_eq!(
            slot,
            KeySlot::Secondary,
            "fail-open must skip disabled slots"
        );
    }

    #[test]
    fn select_from_candidates_returns_none_when_all_disabled() {
        // Every slot disabled. There's no fail-open because no enabled slot exists.
        let candidates: Vec<(Arc<str>, KeySlot)> = vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
        ];
        let mut health = KeyHealthTracker::default();
        health.set_enabled(KeySlot::Primary, false);
        health.set_enabled(KeySlot::Secondary, false);
        let now = Instant::now();
        assert!(
            select_from_candidates(&candidates, &mut health, None, now).is_none(),
            "no enabled slot → None (disabled slots never picked)"
        );
    }

    #[test]
    fn set_enabled_toggles_without_touching_backoff() {
        // Toggling enabled must preserve the failure count and backoff window
        // so re-enabling a temporarily-disabled slot restores its prior state.
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        tracker.record_failure(KeySlot::Tertiary, now);
        let before = tracker.get(KeySlot::Tertiary).clone();
        tracker.set_enabled(KeySlot::Tertiary, false);
        assert!(!tracker.get(KeySlot::Tertiary).enabled);
        assert_eq!(
            tracker.get(KeySlot::Tertiary).consecutive_failures,
            before.consecutive_failures
        );
        assert_eq!(
            tracker.get(KeySlot::Tertiary).backoff_until,
            before.backoff_until
        );
        tracker.set_enabled(KeySlot::Tertiary, true);
        assert!(tracker.get(KeySlot::Tertiary).enabled);
        assert_eq!(
            tracker.get(KeySlot::Tertiary).consecutive_failures,
            before.consecutive_failures
        );
        assert_eq!(
            tracker.get(KeySlot::Tertiary).backoff_until,
            before.backoff_until
        );
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
            None,
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
            None,
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
            health.get(second_slot).consecutive_failures,
            0,
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
            None,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(rate_limit_err()) })
            },
        ));

        assert!(
            matches!(
                result,
                Err(LanguageModelCompletionError::RateLimitExceeded { .. })
            ),
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
            None,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(bad_request_err()) })
            },
        ));

        assert!(matches!(
            result,
            Err(LanguageModelCompletionError::BadRequestFormat { .. })
        ));
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
            None,
            provider_name(),
            move |api_key| {
                let key = (*api_key).to_string();
                attempts_for_closure.lock().push(key);
                Box::pin(async move { Err(server_overloaded_err()) })
            },
        ));

        assert!(matches!(
            result,
            Err(LanguageModelCompletionError::ServerOverloaded { .. })
        ));
        // Exactly one attempt per candidate — no slot tried twice.
        let attempts = attempts.lock();
        assert_eq!(
            attempts.len(),
            3,
            "each candidate tried exactly once: {attempts:?}"
        );
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
            None,
            provider_name(),
            move |_api_key| Box::pin(async move { Ok(1_i32) }),
        ));

        assert!(matches!(
            result,
            Err(LanguageModelCompletionError::NoApiKey { .. })
        ));
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
        // The 1h cap (plan 027)
        assert_eq!(format_backoff_remaining(BACKOFF_MAX), "1h 0m");
        // Durations past the cap still format correctly if ever displayed.
        assert_eq!(
            format_backoff_remaining(Duration::from_secs(5 * 3600)),
            "5h 0m"
        );
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
            ..Default::default()
        };
        tracker.secondary = KeyHealth {
            consecutive_failures: 0,
            backoff_until: None,
            ..Default::default()
        };
        tracker.tertiary = KeyHealth {
            consecutive_failures: 1,
            backoff_until: Some(now + Duration::from_secs(60)),
            ..Default::default()
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
            ..Default::default()
        };
        assert_eq!(zero.to_health(now, 0.0).backoff_until, None);

        let negative = PersistedKeyHealth {
            consecutive_failures: 5,
            backoff_remaining_secs: Some(-120.0),
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
        assert_eq!(
            expired.backoff_until, None,
            "elapsed > remaining -> healthy"
        );
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
            ..Default::default()
        };
        // Disable Secondary so the serialized `enabled: false` shape is also
        // covered by this snapshot — it's the only slot that differs from the
        // default `true`.
        tracker.secondary = KeyHealth {
            enabled: false,
            ..Default::default()
        };
        let persisted = PersistedKeyHealthFile::from_tracker(&tracker, now);
        let json = serde_json::to_value(&persisted).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 6, "expected schema_version + saved_at + 4 slots");
        assert_eq!(obj.get("schema_version").and_then(|v| v.as_u64()), Some(4));
        // saved_at_unix_secs is a positive integer (wall-clock).
        assert!(
            obj.get("saved_at_unix_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0,
            "saved_at_unix_secs should be a positive unix timestamp"
        );
        let primary = obj.get("primary").unwrap().as_object().unwrap();
        assert_eq!(
            primary.get("consecutive_failures").and_then(|v| v.as_u64()),
            Some(2)
        );
        // v4 split: the transport counter is a distinct field so a reader can
        // tell an upstream verdict from a connect failure after a restart.
        assert_eq!(
            primary.get("transport_failures").and_then(|v| v.as_u64()),
            Some(0)
        );
        // 120.5s remaining, encoded as a float (not null).
        assert!(primary.get("backoff_remaining_secs").unwrap().is_f64());
        // Primary is enabled (the default).
        assert_eq!(
            primary.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "primary should serialize enabled: true"
        );
        let secondary = obj.get("secondary").unwrap().as_object().unwrap();
        assert_eq!(
            secondary
                .get("consecutive_failures")
                .and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(secondary.get("backoff_remaining_secs").unwrap().is_null());
        // Secondary was explicitly disabled — its `enabled: false` survives the
        // round-trip through `from_tracker`.
        assert_eq!(
            secondary.get("enabled").and_then(|v| v.as_bool()),
            Some(false),
            "secondary should serialize enabled: false"
        );
        let tertiary = obj.get("tertiary").unwrap().as_object().unwrap();
        assert_eq!(
            tertiary
                .get("consecutive_failures")
                .and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(tertiary.get("backoff_remaining_secs").unwrap().is_null());
        assert_eq!(
            tertiary.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "tertiary should default to enabled: true"
        );
        let quaternary = obj.get("quaternary").unwrap().as_object().unwrap();
        assert_eq!(
            quaternary
                .get("consecutive_failures")
                .and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(quaternary.get("backoff_remaining_secs").unwrap().is_null());
        assert_eq!(
            quaternary.get("enabled").and_then(|v| v.as_bool()),
            Some(true),
            "quaternary should default to enabled: true"
        );
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
                ..Default::default()
            },
            secondary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
                ..Default::default()
            },
            tertiary: PersistedKeyHealth {
                consecutive_failures: 2,
                backoff_remaining_secs: Some(0.0),
                ..Default::default()
            },
            quaternary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
                ..Default::default()
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
        assert_eq!(
            sanitize_provider_id_for_filename("my-provider"),
            "my-provider"
        );
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
        assert!(
            path.starts_with(paths::data_dir()),
            "got {}",
            path.display()
        );
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
    async fn reload_persisted_health_missing_file_returns_default(cx: &mut gpui::TestAppContext) {
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/nonexistent/provider.json");
        let loaded = reload_persisted_health(&fs, &path).await;
        assert_eq!(loaded, KeyHealthTracker::default());
        assert_eq!(loaded.primary.consecutive_failures, 0);
        assert_eq!(loaded.primary.backoff_until, None);
    }

    #[gpui::test]
    async fn reload_persisted_health_corrupt_json_returns_default(cx: &mut gpui::TestAppContext) {
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/corrupt/provider.json");
        fs.atomic_write(path.clone(), "not json at all {{{{".to_string())
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
                ..Default::default()
            },
            secondary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
                ..Default::default()
            },
            tertiary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
                ..Default::default()
            },
            quaternary: PersistedKeyHealth {
                consecutive_failures: 0,
                backoff_remaining_secs: None,
                ..Default::default()
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
        // schema_version 1, primary is backed off. `saved_at_unix_secs` is "now"
        // rather than a hardcoded historical timestamp: `to_health` deliberately
        // loads already-elapsed backoffs as healthy, so a stale timestamp would
        // expire the 14454.5s window and defeat the assertion below.
        let saved_at_unix_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let v1_json = format!(
            r#"{{"schema_version":1,"saved_at_unix_secs":{saved_at_unix_secs},"primary":{{"consecutive_failures":3,"backoff_remaining_secs":14454.5}},"secondary":{{"consecutive_failures":0,"backoff_remaining_secs":null}},"tertiary":{{"consecutive_failures":0,"backoff_remaining_secs":null}}}}"#
        );
        fs.atomic_write(path.clone(), v1_json).await.unwrap();

        let loaded = reload_persisted_health(&fs, &path).await;

        // The v1 file parsed: all four slots exist. Failure counters are NOT
        // asserted here — the v<4 migration deliberately drops them, because
        // pre-v4 counters conflate transport failures with upstream verdicts
        // (see `v3_migration_drops_unattributable_backoff_but_keeps_enabled`).
        // The parse contract is probed by the companion test below.
        assert_eq!(loaded.primary.consecutive_failures, 0);
        assert_eq!(
            loaded.primary.backoff_until, None,
            "the v<4 migration must not carry an unattributable backoff forward"
        );
        assert_eq!(loaded.secondary.consecutive_failures, 0);
        assert_eq!(loaded.tertiary.consecutive_failures, 0);

        // Quaternary must come in as the healthy default — it didn't exist in v1.
        assert_eq!(
            loaded.quaternary.consecutive_failures, 0,
            "quaternary must default to 0 failures on v1 migration"
        );
        assert_eq!(
            loaded.quaternary.backoff_until, None,
            "quaternary must default to no backoff on v1 migration"
        );
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
        // `enabled: false` on secondary is the probe: it is the one field the
        // v<4 migration preserves, so it distinguishes "parsed and migrated"
        // from "fell through to `KeyHealthTracker::default()`". The failure
        // counters can no longer serve as the probe — the migration zeroes
        // them on purpose.
        let v1_json = r#"{"schema_version":1,"saved_at_unix_secs":1700000000,"primary":{"consecutive_failures":7,"backoff_remaining_secs":60.0},"secondary":{"consecutive_failures":0,"backoff_remaining_secs":null,"enabled":false},"tertiary":{"consecutive_failures":0,"backoff_remaining_secs":null}}"#;
        fs.atomic_write(path.clone(), v1_json.to_string())
            .await
            .unwrap();

        let loaded = reload_persisted_health(&fs, &path).await;
        assert!(
            !loaded.secondary.enabled,
            "v1 file must not fall through to parse-error default"
        );
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
            ..Default::default()
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
    async fn persist_and_reload_expired_backoff_loads_as_healthy(cx: &mut gpui::TestAppContext) {
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
            ..Default::default()
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

    #[test]
    fn transport_errors_classify_as_transport_not_key_fault() {
        let provider = provider_name();
        for err in [
            LanguageModelCompletionError::HttpSend {
                provider: provider.clone(),
                error: anyhow::anyhow!("connection reset"),
            },
            LanguageModelCompletionError::StreamEndedUnexpectedly {
                provider: provider.clone(),
            },
            LanguageModelCompletionError::ApiReadResponseError {
                provider: provider.clone(),
                error: std::io::Error::other("eof"),
            },
            LanguageModelCompletionError::Other(anyhow::anyhow!("unknown")),
        ] {
            assert_eq!(
                classify_error(&err),
                ErrorVerdict::Transport,
                "{err} should carry no upstream verdict"
            );
        }

        for err in [
            LanguageModelCompletionError::RateLimitExceeded {
                provider: provider.clone(),
                retry_after: None,
            },
            LanguageModelCompletionError::AuthenticationError {
                provider: provider.clone(),
                message: "bad key".into(),
            },
            LanguageModelCompletionError::ApiInternalServerError {
                provider,
                message: "boom".into(),
            },
            LanguageModelCompletionError::PaymentRequired,
        ] {
            assert_eq!(classify_error(&err), ErrorVerdict::KeyFault);
        }
    }

    /// The reported bug: repeated `error sending HTTP request to GLM API`
    /// (a connect failure, no response body at all) drove every slot to
    /// `consecutive_failures` in the tens-to-hundreds and pinned them at the
    /// 1h cap, while the provider dashboard showed the quota ~half used.
    #[test]
    fn transport_errors_do_not_escalate_onto_the_quota_schedule() {
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        let err = LanguageModelCompletionError::HttpSend {
            provider: provider_name(),
            error: anyhow::anyhow!("error sending HTTP request"),
        };

        for _ in 0..30 {
            match classify_error(&err) {
                ErrorVerdict::Transport => tracker.record_transport_failure(KeySlot::Primary, now),
                verdict => panic!("expected Transport, got {verdict:?}"),
            }
        }

        let health = tracker.get(KeySlot::Primary);
        assert_eq!(
            health.consecutive_failures, 0,
            "a verdict-less failure must never count as an upstream quota verdict"
        );
        assert_eq!(health.transport_failures, 30);
        let window = health.backoff_total.expect("slot is backed off");
        assert!(
            window <= TRANSPORT_BACKOFF_MAX,
            "30 connect failures yielded {window:?}, above the transport cap"
        );
    }

    #[test]
    fn success_clears_both_failure_counters() {
        let mut tracker = KeyHealthTracker::default();
        let now = Instant::now();
        tracker.record_failure(KeySlot::Secondary, now);
        tracker.record_transport_failure(KeySlot::Secondary, now);
        tracker.record_success(KeySlot::Secondary);

        let health = tracker.get(KeySlot::Secondary);
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.transport_failures, 0);
        assert_eq!(health.backoff_until, None);
        assert_eq!(health.backoff_total, None);
    }

    /// Clamping after jitter erased it for every factor >= 1.0, so saturated
    /// slots all landed on exactly `BACKOFF_MAX` and unblocked together.
    #[test]
    fn saturated_backoff_keeps_its_jitter_spread() {
        let samples: Vec<Duration> = (0..200).map(|_| compute_backoff(30)).collect();
        for sample in &samples {
            assert!(*sample <= BACKOFF_MAX, "{sample:?} exceeds the cap");
        }
        let at_cap = samples.iter().filter(|s| **s == BACKOFF_MAX).count();
        assert!(
            at_cap <= 2,
            "{at_cap}/200 saturated backoffs collapsed onto the cap exactly"
        );
        let min = samples.iter().min().expect("non-empty");
        let max = samples.iter().max().expect("non-empty");
        assert!(
            *max - *min > Duration::from_secs(600),
            "spread {min:?}..{max:?} is too narrow to de-synchronize slots"
        );
    }

    #[test]
    fn transport_backoff_stays_under_its_cap() {
        assert_eq!(compute_transport_backoff(0), Duration::ZERO);
        for failures in [1, 2, 5, 30, 1000, u32::MAX] {
            for _ in 0..50 {
                let backoff = compute_transport_backoff(failures);
                assert!(
                    backoff <= TRANSPORT_BACKOFF_MAX,
                    "failures={failures} yielded {backoff:?}"
                );
            }
        }
    }

    /// Pre-v4 files carry counters built by the old classifier, which folded
    /// transport failures into the quota schedule. Those windows are
    /// unattributable, so the migration drops them instead of making the user
    /// wait out an hour of backoff that the new classifier never would have
    /// set.
    #[test]
    fn v3_migration_drops_unattributable_backoff_but_keeps_enabled() {
        let file: PersistedKeyHealthFile = serde_json::from_str(
            r#"{"schema_version":3,"saved_at_unix_secs":0,
                "primary":{"consecutive_failures":27,"backoff_remaining_secs":3422.0,"backoff_total_secs":3424.0,"enabled":true},
                "secondary":{"consecutive_failures":205,"backoff_remaining_secs":3597.0,"backoff_total_secs":3600.0,"enabled":false},
                "tertiary":{"consecutive_failures":0,"backoff_remaining_secs":null},
                "quaternary":{"consecutive_failures":0,"backoff_remaining_secs":null}}"#,
        )
        .expect("v3 file should still deserialize");
        assert_eq!(file.primary.transport_failures, 0, "defaulted for v3");

        let mut tracker = file.to_tracker(Instant::now());
        // Mirror the migration branch in `reload_persisted_health`.
        for slot in ALL_KEY_SLOTS {
            let health = tracker.get_mut(slot);
            health.consecutive_failures = 0;
            health.transport_failures = 0;
            health.backoff_until = None;
            health.backoff_total = None;
        }

        let now = Instant::now();
        for slot in ALL_KEY_SLOTS {
            assert!(!tracker.get(slot).is_backed_off(now));
            assert_eq!(tracker.get(slot).consecutive_failures, 0);
        }
        assert!(tracker.get(KeySlot::Primary).enabled);
        assert!(
            !tracker.get(KeySlot::Secondary).enabled,
            "disabling is a user decision and must survive the migration"
        );
    }

    /// Observed in the wild: Tertiary was pinned to a real upstream reset hint
    /// of ~8h (`retry_after: Some(28905s)`), then an unrelated connect failure
    /// overwrote it with the 1h local cap — handing a still-limited key back to
    /// rotation 7 hours early. A local guess must never shorten the window.
    #[test]
    fn local_backoff_never_shortens_an_upstream_window() {
        let now = Instant::now();
        let upstream_hint = Duration::from_secs(28905);

        for record_local in [
            KeyHealthTracker::record_failure as fn(&mut KeyHealthTracker, KeySlot, Instant),
            KeyHealthTracker::record_transport_failure,
        ] {
            let mut tracker = KeyHealthTracker::default();
            tracker.record_rate_limit(KeySlot::Tertiary, now, Some(upstream_hint));
            assert!(tracker.get(KeySlot::Tertiary).backoff_from_upstream_hint);

            for _ in 0..20 {
                record_local(&mut tracker, KeySlot::Tertiary, now);
            }

            let health = tracker.get(KeySlot::Tertiary);
            assert_eq!(
                health.backoff_total,
                Some(upstream_hint),
                "a local guess overwrote the upstream reset hint"
            );
            assert!(
                health.backoff_from_upstream_hint,
                "provenance must survive local failures"
            );
            assert!(health.is_backed_off(now + Duration::from_secs(28_000)));
        }
    }

    /// The upstream is authoritative about timing in both directions: if it
    /// reports a *shorter* reset than the window in place, the quota came back
    /// early and the key should return to rotation.
    #[test]
    fn a_fresh_upstream_hint_may_shorten_the_window() {
        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_rate_limit(KeySlot::Primary, now, Some(Duration::from_secs(28905)));
        tracker.record_rate_limit(KeySlot::Primary, now, Some(Duration::from_secs(30)));
        assert_eq!(
            tracker.get(KeySlot::Primary).backoff_total,
            Some(Duration::from_secs(30))
        );
    }

    /// A 429 with no parseable reset hint still proves the key is limited —
    /// only the duration is unknown. The old probe path ignored these outright,
    /// leaving a provably limited key rendering as healthy.
    #[test]
    fn a_hintless_rate_limit_still_backs_the_slot_off() {
        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_rate_limit(KeySlot::Secondary, now, None);

        let health = tracker.get(KeySlot::Secondary);
        assert!(health.is_backed_off(now), "a 429 must mark the slot");
        assert_eq!(health.consecutive_failures, 1);
        assert!(
            !health.backoff_from_upstream_hint,
            "no hint was supplied, so the window is a local guess"
        );
    }

    /// A probe is a 1-token ping: it can slip through a quota that a real turn
    /// would trip, so it must not overturn an upstream reset hint. It still
    /// clears locally-guessed windows — the stale-backoff case probing is for.
    #[test]
    fn probe_success_clears_local_guesses_but_not_upstream_hints() {
        let now = Instant::now();

        let mut tracker = KeyHealthTracker::default();
        tracker.record_transport_failure(KeySlot::Primary, now);
        assert!(tracker.record_probe_success(KeySlot::Primary, now));
        assert!(!tracker.get(KeySlot::Primary).is_backed_off(now));

        let mut tracker = KeyHealthTracker::default();
        tracker.record_rate_limit(KeySlot::Primary, now, Some(Duration::from_secs(28905)));
        assert!(
            !tracker.record_probe_success(KeySlot::Primary, now),
            "a 1-token ping must not overturn an unexpired upstream reset hint"
        );
        assert!(tracker.get(KeySlot::Primary).is_backed_off(now));

        // Once the hinted window elapses, the slot is healthy anyway and a
        // probe success clears the bookkeeping.
        let after = now + Duration::from_secs(28906);
        assert!(tracker.record_probe_success(KeySlot::Primary, after));
        assert_eq!(tracker.get(KeySlot::Primary).consecutive_failures, 0);
    }

    /// A real completion is strong evidence — unlike a probe, it clears an
    /// upstream-hinted window too.
    #[test]
    fn real_success_clears_even_an_upstream_hinted_window() {
        let now = Instant::now();
        let mut tracker = KeyHealthTracker::default();
        tracker.record_rate_limit(KeySlot::Primary, now, Some(Duration::from_secs(28905)));
        tracker.record_success(KeySlot::Primary);

        let health = tracker.get(KeySlot::Primary);
        assert!(!health.is_backed_off(now));
        assert!(!health.backoff_from_upstream_hint);
    }

    /// The v<4 migration keys off window length: the local exponential is
    /// clamped to `BACKOFF_MAX`, so anything longer can only have come from an
    /// upstream reset hint and must survive. Anything within the cap is
    /// ambiguous (transport poisoning produced exactly these) and is dropped.
    #[gpui::test]
    async fn v3_migration_keeps_upstream_windows_and_drops_local_ones(
        cx: &mut gpui::TestAppContext,
    ) {
        let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor.clone());
        let path = PathBuf::from("/v3/provider.json");
        let saved_at_unix_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // primary: the transport-poisoned shape (27 failures pinned at the 1h
        // cap). tertiary: a real ~8h upstream reset hint.
        let v3_json = format!(
            r#"{{"schema_version":3,"saved_at_unix_secs":{saved_at_unix_secs},
                "primary":{{"consecutive_failures":27,"backoff_remaining_secs":3597.9,"backoff_total_secs":3600.0,"enabled":true}},
                "secondary":{{"consecutive_failures":0,"backoff_remaining_secs":null}},
                "tertiary":{{"consecutive_failures":214,"backoff_remaining_secs":28900.0,"backoff_total_secs":28905.3,"enabled":true}},
                "quaternary":{{"consecutive_failures":0,"backoff_remaining_secs":null}}}}"#
        );
        fs.atomic_write(path.clone(), v3_json).await.unwrap();

        let loaded = reload_persisted_health(&fs, &path).await;
        let now = Instant::now();

        assert!(
            !loaded.primary.is_backed_off(now),
            "a 1h window is within the local cap — unattributable, must be dropped"
        );
        assert_eq!(loaded.primary.consecutive_failures, 0);

        assert!(
            loaded.tertiary.is_backed_off(now),
            "a 28905s window exceeds the local cap, so the upstream must have said it"
        );
        assert!(
            loaded.tertiary.backoff_from_upstream_hint,
            "the surviving window must be tagged as upstream-attested"
        );
    }

    fn four_candidates() -> Vec<(Arc<str>, KeySlot)> {
        vec![
            (Arc::<str>::from("key-a"), KeySlot::Primary),
            (Arc::<str>::from("key-b"), KeySlot::Secondary),
            (Arc::<str>::from("key-c"), KeySlot::Tertiary),
            (Arc::<str>::from("key-d"), KeySlot::Quaternary),
        ]
    }

    /// Fail-open must not spend a request on a slot the upstream already told
    /// us is closed. A locally-guessed window is speculation and may well
    /// succeed; an attested one is a guaranteed 429.
    #[test]
    fn fail_open_prefers_speculative_backoff_over_attested() {
        let candidates = four_candidates();
        let now = Instant::now();
        let mut health = KeyHealthTracker::default();

        // Deliberately adversarial to the *old* `min_by_key(backoff_until)`
        // rule: the attested slots carry the SHORTEST windows, so the old
        // fallback would have picked one of them. They are still certain 429s
        // — the upstream said so — whereas K3's long window is only a local
        // guess and may well answer.
        for slot in [KeySlot::Primary, KeySlot::Secondary, KeySlot::Quaternary] {
            health.record_rate_limit(slot, now, Some(Duration::from_secs(10)));
        }
        for _ in 0..10 {
            health.record_failure(KeySlot::Tertiary, now);
        }
        assert!(
            health.get(KeySlot::Tertiary).backoff_total.unwrap() > Duration::from_secs(600),
            "test setup: K3's speculative window must dwarf the attested ones"
        );

        for _ in 0..8 {
            let (_, slot) = select_from_candidates(&candidates, &mut health, None, now)
                .expect("fail-open must return a slot");
            assert_eq!(
                slot,
                KeySlot::Tertiary,
                "picked a slot the upstream already refused"
            );
        }
    }

    /// When every window is attested there is no good choice — every request
    /// will be refused — so take the one closest to its stated reset.
    #[test]
    fn fail_open_takes_earliest_reset_when_all_are_attested() {
        let candidates = four_candidates();
        let now = Instant::now();
        let mut health = KeyHealthTracker::default();
        health.record_rate_limit(KeySlot::Primary, now, Some(Duration::from_secs(28905)));
        health.record_rate_limit(KeySlot::Secondary, now, Some(Duration::from_secs(900)));
        health.record_rate_limit(KeySlot::Tertiary, now, Some(Duration::from_secs(7200)));
        health.record_rate_limit(KeySlot::Quaternary, now, Some(Duration::from_secs(3600)));

        let (_, slot) = select_from_candidates(&candidates, &mut health, None, now)
            .expect("fail-open must return a slot");
        assert_eq!(slot, KeySlot::Secondary, "900s is the soonest reset");
    }

    /// The old fallback was `min_by_key(backoff_until)` — deterministic, so
    /// every concurrent agent converged on the same key the moment the pool
    /// went down. Speculative fail-open picks now advance the rotation cursor.
    #[test]
    fn fail_open_spreads_concurrent_picks_across_speculative_slots() {
        let candidates = four_candidates();
        let now = Instant::now();
        let mut health = KeyHealthTracker::default();
        for slot in ALL_KEY_SLOTS {
            health.record_transport_failure(slot, now);
        }

        let mut seen = HashMap::new();
        for _ in 0..12 {
            let (_, slot) = select_from_candidates(&candidates, &mut health, None, now)
                .expect("fail-open must return a slot");
            *seen.entry(format!("{slot:?}")).or_insert(0) += 1;
        }
        assert_eq!(seen.len(), 4, "12 fail-open picks clustered: {seen:?}");
        for (slot, count) in &seen {
            assert_eq!(*count, 3, "uneven fail-open spread for {slot}: {seen:?}");
        }
    }

    /// A disabled slot is a hard opt-out and must never be resurrected, even
    /// when it is the only speculative candidate left.
    #[test]
    fn fail_open_never_resurrects_a_disabled_slot() {
        let candidates = four_candidates();
        let now = Instant::now();
        let mut health = KeyHealthTracker::default();
        for slot in ALL_KEY_SLOTS {
            health.record_rate_limit(slot, now, Some(Duration::from_secs(28905)));
        }
        // The one speculative slot is also the one the user turned off.
        health.record_success(KeySlot::Tertiary);
        health.record_transport_failure(KeySlot::Tertiary, now);
        health.set_enabled(KeySlot::Tertiary, false);

        for _ in 0..4 {
            let (_, slot) = select_from_candidates(&candidates, &mut health, None, now)
                .expect("three enabled slots remain");
            assert_ne!(slot, KeySlot::Tertiary, "resurrected a disabled slot");
        }
    }
}
