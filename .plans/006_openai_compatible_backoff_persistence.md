# 006: OpenAI-Compatible API Key Backoff Persistence

## Goal

Persist per-key backoff state across Zed restarts so that a restart does not
reset all keys to healthy (which would cause a thundering herd of requests
against a key that the upstream is still throttling).

## Background

Sessions 1-4 built in-memory backoff (`Arc<std::sync::Mutex<KeyHealthTracker>>`)
on `State`. Restart = fresh slate. The user's original complaint was
false-positive rate-limit handling — restoring backed-off state on restart
prevents hammering a still-cooling-down key after a restart.

## Constraints

- `Instant` is monotonic, process-local, and **not serializable as absolute**.
  Persist as **relative duration** from persist-time to `backoff_until`.
  On load: `Instant::now() + stored_remaining`. If already expired, result
  is in the past → effectively not backed off (no special-casing needed).
- File I/O must be off the foreground thread (use `cx.background_spawn`).
- Writes must be **debounced** to avoid write amplification (every failure
  in a tight retry loop could otherwise trigger a disk write).
- Load errors must be **non-fatal** — corrupt/missing file = fresh state.
- Atomic write via `Fs::atomic_write` (already used elsewhere; avoids partial
  reads by other processes).
- The `paths` crate must be added as a dep of `language_models`.
- Must not hold the `Mutex<KeyHealthTracker>` across `.await`.
- File size: current `open_ai_compatible.rs` is ~2101 lines. Keep additions
  focused; do not split unless exceeding ~2400 lines.

## Design

### On-disk format

Path: `paths::data_dir().join("openai_compatible_backoff")/{provider_id}.json`

Per-provider file. `provider_id` is `State.id: Arc<str>` (already URL-sanitized
through `OpenAiCompatibleLanguageModelProvider::new`'s `id` parameter).

File is sanitized — but the `id` is an arbitrary user-configured string.
Use `util::paths::SanitizedPath` (already used by `paths` crate) to strip
path separators. Fallback to a hash if id is empty after sanitization.

```json
{
  "schema_version": 1,
  "slots": {
    "primary":    { "consecutive_failures": 3, "backoff_remaining_secs": 120.5 },
    "secondary":  { "consecutive_failures": 0, "backoff_remaining_secs": null  },
    "tertiary":   { "consecutive_failures": 1, "backoff_remaining_secs": 0.0   }
  }
}
```

`backoff_remaining_secs` is `Instant -> Duration` from **load time** to
`backoff_until`. On save it's `backoff_until.saturating_duration_since(now)`.

### State changes

`State` gains:
- `key_health_dirty: Arc<std::sync::Mutex<Option<Task<()>>>>` — holds the
  latest pending save task. Cancelled + replaced when a new save is scheduled.
- `key_health_path: PathBuf` — computed once in `new()`.

`State::new`:
- Compute the persistence path.
- Spawn a background load task: read file, parse JSON, convert relative
  durations back to `Instant`-absolute, replace `key_health` contents.
  - Errors logged via `.log_err()`, not fatal.
  - Use a `Task<()>` stored on State so it can be awaited if needed for
    tests (kept optional to avoid forcing ordering).

After `record_success` / `record_failure` (the free functions used by
`retry_stream`), call a new `schedule_persist(...)` helper that:
- Clones the current health snapshot under the mutex.
- Cancels any prior pending save task.
- Spawns a new background task that:
  - sleeps `DEBOUNCE` (e.g. 2s) to coalesce bursts
  - serializes snapshot to the on-disk format
  - atomic-writes to the path
  - Errors logged via `.log_err()`.

### Loading

`reload_persisted_health(path) -> KeyHealthTracker`:
- `fs.load(path).await?` → `serde_json::from_str(&content)?`
- Convert each slot: `backoff_until = Some(now + remaining)` if
  `remaining > Duration::ZERO`, else `None`.
- Apply only to slots that actually have a configured key? **No** —
  restore unconditionally. A slot with no key but stale backoff is harmless
  (selection excludes it anyway via `gather_candidates`).

### Tests

- `persisted_health_round_trip` — save then load yields equivalent tracker.
- `reload_persisted_health_missing_file_returns_default` — FileNotFound OK.
- `reload_persisted_health_corrupt_json_returns_default` — parse error OK.
- `reload_persisted_health_expired_backoff_treated_as_not_backed_off` —
  remaining=0 on load → `backoff_until=None`.
- `persisted_health_format_serializes_expected_shape` — snapshot test of JSON.
- `schedule_persist_debounces_bursts` — 5 calls in quick succession produce
  1 save after the debounce window (use a mock-able save function or FakeFs).

## Tasks

- [x] Add `paths` dep to `language_models/Cargo.toml`
- [x] Add `PersistedKeyHealth`, `PersistedKeyHealthFile` serde structs
- [x] Add `reload_persisted_health()` async helper
- [x] Add `persist_key_health()` async helper (serializes + atomic-writes)
- [x] Wire load on `State::new` (background task, log_err on failure)
- [x] Add `schedule_persist()` debouncer on State
- [x] Hook `record_success` / `record_failure` to call `schedule_persist`
- [x] Sanitize provider id for filename
- [x] Add unit tests (14 new: pure conversions + FakeFs round-trips)
- [x] Run `cargo test -p language_models --lib` (82/82 pass, stable across 5 runs)
- [x] Run `./script/clippy -p language_models` (clean, incl. cargo-machete + typos)
- [x] Run `cargo check -p edit_prediction -p edit_prediction_cli -p language_models_cloud` (downstream OK)
- [x] Commit on `develop` (commit `8eae9eb5be`, already on `develop` as ancestor of HEAD)

## Out of Scope

- Mid-stream retry after partial output (token-level reconciliation).
- Cross-machine sync (this is local-only state, like the rest of Zed's prefs).
- Migrating from in-memory to disk-backed primary source (in-memory stays
  authoritative; disk is a write-behind cache).
- File-size cleanup: `open_ai_compatible.rs` is now ~2822 lines, above the
  2048 guideline. A future refactor could extract the persistence module
  + KeyHealthTracker into `open_ai_compatible/health.rs`, but that's
  unrelated to the feature and was deferred to avoid mixing concerns.
