# 001 — OpenAI-compatible provider file-size refactor

- [x] Create issue
- [x] Extract health/persistence subsystem to `open_ai_compatible/health.rs`
- [x] Move health-specific unit tests alongside the code they test
- [x] Verify: `cargo test -p language_models --lib`
- [x] Verify: `./script/clippy -p language_models`
- [x] Verify: `cargo check` on downstream crates
- [x] Commit

## Context

`crates/language_models/src/provider/open_ai_compatible.rs` grew to **2822 lines**
across the five-session multi-key rotation arc (commits `79ea863242` → `8eae9eb5be`).
The user's `AGENTS.md` asks for `.rs` files under 2048 lines "as possible".

The growth is almost entirely one cohesive subsystem — **per-key health tracking,
exponential backoff, intra-request rotation, and on-disk persistence** — that was
bolted onto a file originally about provider configuration and credential
management. Extracting it yields two files that each have a single clear
responsibility:

| File                        | Responsibility                                           |
| --------------------------- | -------------------------------------------------------- |
| `open_ai_compatible.rs`     | Provider config, `State`, credentials, `LanguageModel` impl, `ConfigurationView` UI |
| `open_ai_compatible/health.rs` | `KeyHealth` / `KeyHealthTracker`, backoff math, `retry_stream`, `probe_first_event`, persistence layer |

This is a **pure code move** — no behavior change, no API change, no new feature.
The extracted module is a private submodule (`mod health;`), matching the existing
`anthropic.rs` + `anthropic/telemetry.rs` convention in the same directory.

## Why this is an issue, not a plan

Per the user's `AGENTS.md`: "Create issue at `./issues` for optimization task, do
not create plan." This is a refactor (code organization), not a feature.

## Scope

### Moves to `open_ai_compatible/health.rs`

**Types**: `KeySlot`, `KeyHealth`, `KeyHealthTracker`, `SlotHealthStatus`,
`PersistedKeyHealth`, `PersistedKeyHealthFile`.

**Constants**: `BACKOFF_BASE`, `BACKOFF_MAX`, `PERSISTED_KEY_HEALTH_SCHEMA_VERSION`,
`PERSIST_DIR_NAME`, `PERSIST_DEBOUNCE`.

**Functions**: `compute_backoff`, `format_backoff_remaining`, `is_backoff_worthy`,
`select_from_candidates`, `record_key_success`, `record_key_failure`,
`snapshot_health`, `retry_stream`, `probe_first_event`,
`sanitize_provider_id_for_filename`, `key_health_path_for`,
`reload_persisted_health`, `persist_key_health`,
`schedule_persist_key_health_inner`.

**Tests**: everything that exercises health internals in isolation (backoff math,
classification, selection, retry loop, probe, format, persistence round-trips).
Tests that exercise `State` (`slot_health_snapshot_*`, `gather_candidates_*`)
stay in the parent — they belong with the provider code.

### Stays in `open_ai_compatible.rs`

`OpenAiCompatibleSettings`, `OpenAiCompatibleLanguageModelProvider` + impls,
`State` + impl, `OpenAiCompatibleLanguageModel` + impls,
`ConfigurationView` + impls, `secondary_key_url`, `tertiary_key_url`,
`stream_completion` / `stream_response` methods, and the `State`-level tests.

## Validation gate

- `cargo test -p language_models --lib` — 82/82 must still pass (no test logic
  changes, only relocation).
- `./script/clippy -p language_models` — clean.
- `cargo check -p edit_prediction -p edit_prediction_cli -p language_models_cloud
  -p open_ai` — downstream crates still compile (they don't reach into the
  private submodule, so this is a sanity check).
