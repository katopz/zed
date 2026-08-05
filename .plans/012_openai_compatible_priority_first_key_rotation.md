# OpenAI Compatible: Priority-first key rotation + per-slot enable/disable + status icons in editor footer

## Goal

Three changes to the OpenAI-compatible multi-key (GLM) subsystem:

1. **Priority-first selection** — Primary key is always used while healthy;
   only when it is backed off / disabled do the remaining configured keys
   (2, 3, 4, ...) rotate hourly. Currently every healthy key rotates hourly
   regardless of position, which means Primary is only used ~25% of the time
   with 4 keys configured.

2. **Per-slot enable/disable** — Each of the 4 slots gets a persisted
   `enabled` flag (default `true`) that the user can toggle to temporarily
   exclude a key from rotation without deleting it. Disabled keys are
   filtered out of `gather_candidates` and `select_from_candidates`.

3. **Status icons beside the Auto-Prompt button** — Four small `1/2/3/4`
   toggle chips rendered next to the existing Auto-Prompt button at the
   bottom of the message editor. Each chip:
   - shows its index (1-4),
   - color reflects state (Accent=healthy+enabled, Warning=backed off,
     Muted=disabled or no key),
   - tooltip shows status detail (key preview, backoff countdown, failures),
   - click toggles `enabled`.

## Why

- **Priority-first**: GLM prompt cache is keyed on the API key. Hourly
  rotation already keeps the cache warm *for the active key*, but with 4
  keys the Primary cache is only warm 25% of the time. Sticky-Primary keeps
  the Primary cache hot continuously; the other keys only kick in during
  failover (when Primary is rate-limited). This matches the user's mental
  model: "key 1 is my main, 2/3/4 are spares".
- **Enable/disable**: The user sometimes wants to temporarily pull a key
  out of rotation (e.g. quota reset elsewhere, debugging one key, an org
  key shared with another workload). Currently the only escape is to clear
  the key entirely, which loses the secret. A persisted toggle preserves
  the secret while excluding it.
- **Footer status icons**: The settings page status requires digging
  through "Configure Provider" → reveal. Having K1/K2/K3/K4 visible at the
  bottom of the chat next to the Auto-Prompt button gives one-glance
  visibility into the live key pool state and a one-click toggle.

## Approach

### 1. `health.rs`

- `KeyHealth` gets a new `enabled: bool` field (default `true`).
- `SlotHealthStatus` gets a new `enabled: bool` field.
- `select_from_candidates` is restructured:
  1. Filter healthy = present + enabled + not backed off.
  2. If Primary is in `healthy` → return it (sticky).
  3. Else `deterministic_hourly_pick(&healthy)` over the non-Primary keys.
  4. Else (all healthy candidates absent) fall back to earliest-expiring
     backoff among enabled slots only (disabled slots never get picked
     even in fail-open).
- New `KeyHealthTracker::set_enabled(slot, enabled)`.
- `PersistedKeyHealth` gets `enabled: bool` with `#[serde(default =
  "default_true")]` so v2 files load as enabled. No schema bump needed
  (additive, backward-compatible).
- Update doc comments + tests.

### 2. `open_ai_compatible.rs`

- `State::gather_candidates` filters out disabled slots.
- `State::slot_status` includes `enabled` from `KeyHealth`.
- New `State::set_slot_enabled(slot, enabled, cx)` — updates tracker +
  schedules persist + notifies.
- `State::clear_slot_backoff` unchanged (still operates on enabled slots).

### 3. `LanguageModel` trait (`language_model.rs`)

Two new default-`None` / no-op methods:
- `fn key_slot_status(&self, _cx: &App) -> Option<KeySlotStatusSummary>`
- `fn set_key_slot_enabled(&self, _slot_index: usize, _enabled: bool, _cx:
  &mut App)`

`KeySlotStatusSummary` is a new pub struct in `language_model` exposing
`[SlotKeyStatus; 4]` where each entry has: `has_key`, `enabled`,
`is_backed_off`, `backoff_remaining`, `consecutive_failures`. Implemented
by `OpenAiCompatibleLanguageModel` only; all other providers return `None`.

### 4. `thread_view.rs`

- New method `render_key_status_buttons(&self, cx)` returns 0-4 small
  toggle chips. Returns empty when the active model's provider returns
  `None` for `key_slot_status` (so non-OpenAI-compatible providers render
  nothing).
- Inserted into `render_message_editor` after `render_auto_prompt_toggle`.
- Each chip click calls `model.set_key_slot_enabled(idx, !enabled, cx)`.

## Files

| File | Change |
|------|--------|
| `crates/language_models/src/provider/open_ai_compatible/health.rs` | `enabled` field on `KeyHealth`/`SlotHealthStatus`/`PersistedKeyHealth`; priority-first `select_from_candidates`; `set_enabled`; tests |
| `crates/language_models/src/provider/open_ai_compatible.rs` | filter disabled in `gather_candidates`; expose `enabled` in `slot_status`; new `State::set_slot_enabled`; trait impls on `OpenAiCompatibleLanguageModel` |
| `crates/language_model/src/language_model.rs` | new `KeySlotStatusSummary` + two trait methods |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `render_key_status_buttons` + insert into footer |

## Tasks

- [x] Add `enabled` to `KeyHealth` + `Default::default()` keeps `true`
- [x] Add `enabled` to `SlotHealthStatus`
- [x] Add `enabled` to `PersistedKeyHealth` with `#[serde(default = "default_true")]`
- [x] Restructure `select_from_candidates` for priority-first + filter disabled
- [x] Add `KeyHealthTracker::set_enabled`
- [x] Update `State::slot_status` to surface `enabled`
- [x] Add `State::set_slot_enabled`
- [x] Filter disabled slots in `State::gather_candidates`
- [x] Add `ModelKeySlotStatus` + `ModelKeySlotStatusSummary` + trait methods on `LanguageModel`
- [x] Implement trait methods on `OpenAiCompatibleLanguageModel`
- [x] Render footer chips in `render_message_editor`
- [x] Add / update unit tests in `health.rs`
- [x] `cargo clippy` clean (lib) on `language_model`, `language_models`, `agent_ui`
- [x] `./script/clippy` — attempted in isolated worktree at `cf763d1cf4`. The full-tree `--release --all-targets --all-features --deny warnings` gate does NOT pass on `develop`, but every failure is pre-existing and in code this plan did not touch. Validated that **my** code is clean:
  - `cargo clippy -p language_model -p language_models -p agent_ui --lib -- --deny warnings` → clean (default features)
  - `cargo clippy -p language_models --tests -- --deny warnings` → clean (after temp-patching the pre-existing `AnthropicModelMode::Default`/`BedrockModelMode::Default` in the worktree only; reverted by discarding worktree)
  - `cargo test -p language_models --lib health::` → 43 pass / 1 pre-existing fail
  - Pre-existing blockers on `develop` (all verified present on `origin/develop`, none caused by this plan):
    1. `auto_prompt` test binary: `missing field had_api_error` (commit `41e9f9682d` added the field, test fixture `context_helpers_test.rs` not updated)
    2. `language_models` test binary: `AnthropicModelMode::Default` / `BedrockModelMode::Default` (renamed to `Auto` in #57207, test sites not updated)
    3. `remote_connection` lib with `--all-features`: `RemoteConnectionOptions::Mock(_)` not covered (feature-gate `#[cfg(any(test, feature = "test-support"))]` mismatch under `--all-features`)
    4. `agent_ui` test binary: `LanguageModelRegistry` undeclared + `acp_thread::UserMessage.id` / `ContentBlock` / `ContextCompaction.status` mismatches (pre-existing test-code drift)
- [x] Commit on `develop`

## Pre-existing test failures (NOT caused by this plan)

While validating I ran into three pre-existing test failures on `develop`
that block `cargo test --lib` for `language_models`. Each was verified to
fail with this plan's changes stashed. They are unrelated to plan 012:

1. `health::tests::reload_persisted_health_v1_schema_migrates_with_healthy_quaternary`
   — the test's `saved_at_unix_secs=1700000000` (Nov 2023) is now ~3 years
   old, so the persisted backoff (14454s) is correctly treated as expired.
   The assertion `backoff_until.is_some()` is wrong as of 2026.
2. `tests::test_compatible_provider_changes_kind_and_unregisters` — panics
   with `no state of type fs::GlobalFs exists` (test setup regression).
3. `tests::test_compatible_provider_id_collision_resolves_when_one_entry_is_removed`
   — same `fs::GlobalFs` panic.

Additionally, the test binary for `language_models` does not compile on
`develop` because `crates/language_models/src/provider/anthropic.rs:442`
references `AnthropicModelMode::Default` (renamed to `Auto` in #57207) and
`crates/language_models/src/provider/bedrock.rs:2456` references the same
for `BedrockModelMode`. These were temporarily patched locally only to
unblock test runs; the patches were reverted before commit.

Per project rules ("Do not fix unrelated bugs or broken tests"), none of
these are addressed by plan 012.
