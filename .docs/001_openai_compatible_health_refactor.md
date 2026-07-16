# 001 — OpenAI-compatible provider file-size refactor (RESOLVED)

## Status

Resolved. Pure code move, no behavior change.

## Context

`crates/language_models/src/provider/open_ai_compatible.rs` grew to 2822 lines
across the five-session multi-key rotation arc. User's `AGENTS.md` asks for
`.rs` files under 2048 lines "as possible". The growth was almost entirely one
cohesive subsystem — per-key health tracking, exponential backoff, intra-request
rotation, and on-disk persistence.

## Change

Extracted the health/persistence subsystem to a private submodule
`open_ai_compatible/health.rs`, matching the existing
`anthropic.rs` + `anthropic/telemetry.rs` convention.

| File | Responsibility |
|------|----------------|
| `open_ai_compatible.rs` | Provider config, `State`, credentials, `LanguageModel` impl, `ConfigurationView` UI |
| `open_ai_compatible/health.rs` | `KeyHealth` / `KeyHealthTracker`, backoff math, `retry_stream`, `probe_first_event`, persistence |

## Validation

- `cargo test -p language_models --lib` — 82/82 pass (relocation only).
- `./script/clippy -p language_models` — clean.
- Downstream crates (`edit_prediction`, `edit_prediction_cli`,
  `language_models_cloud`, `open_ai`) compile.
