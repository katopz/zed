# 010: Claude context-overflow gate never fired — wrong usage field + threshold drift

- **Fix commit**: `a675ece548` (develop)
- **Follows**: `.docs/008` (plan 023 unified overflow flow), `87f48d95c4` (200k native gate)
- **Status**: implemented and closed
- **Date**: 2026-08-24

## Symptom

Claude (ACP) threads never forked a new thread on context overflow — in
**both** manual and auto flows — while native-agent threads forked at the
200k gate. Reported as "claude not create new thread for both manual and
auto when overflow 200k context length, expected the same flow for both
claude/native agent".

## Root causes (two independent bugs)

### 1. Wrong usage field (the gate could never fire)

`claude_context_overflow_decision` read `token_usage().input_tokens`.
But Claude Code reports usage through ACP `SessionUpdate::UsageUpdate`,
which populates (per agent-client-protocol schema):

- `used_tokens` ← `used` — "Tokens currently in context"
- `max_tokens` ← `size` — "Total context window size"

`input_tokens` is only written in `AcpThread::run_turn` from stop-response
usage, gated behind the `acp-beta` feature flag (`cx.has_flag::<AcpBetaFeatureFlag>()`)
— off by default. So the gate evaluated `Some(0) > threshold` forever.

**Fix**: new `claude_effective_context_tokens(&TokenUsage) -> u64` =
`input_tokens.max(used_tokens)` — fires no matter which field the agent
populates.

### 2. Threshold drift (no parity even with the right field)

Claude gate: fixed `claude_context_overflow_tokens` default 320k.
Native gate: `max_context_tokens`, retuned 256k → 200k in `87f48d95c4`.
A 200k-overflowed Claude thread sat 120k tokens below the Claude gate.

**Fix**: `claude_context_overflow_tokens` now defaults to `0` =
"follow `max_context_tokens`" (new
`AutoPromptConfig::effective_claude_context_overflow_tokens()`). A positive
value remains a Claude-specific override; a huge value effectively disables
the gate. Config-load failure now falls back to `default_max_context_tokens()`
(200k) instead of a hardcoded 320k.

### 3. Live-config stale pin

`~/.config/zed/auto_prompt.json` still pinned `max_context_tokens: 256000`
(from the pre-`87f48d95c4` era), silently defeating the native 200k gate on
this machine. Updated to `200000`; also dropped the unknown `enabled` key.

## Resulting flow (claude == native)

Both manual (`on_manual_auto_prompt`) and auto (`on_thread_stopped`) paths
funnel through `run_auto_prompt` → `decide_claude` → the fixed gate →
`NeedsLlmCall { context_exceeds_limit: true }` → `use_native_flow` →
`decide_with_llm` → shared Phase 1 (same-thread summarize) → Phase 2
(`force_new_thread = true`, new thread with inlined summary). Below the
gate: same-thread continuation, unchanged.

## Config

| Key | Default | Env |
|---|---|---|
| `max_context_tokens` | 200000 | `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS` |
| `claude_context_overflow_tokens` | 0 (= follow `max_context_tokens`; was 320000) | `ZED_AUTO_PROMPT_CLAUDE_CONTEXT_OVERFLOW_TOKENS` |

## Validation

- `cargo test -p auto_prompt --lib` — 388/388 (4 new: field selection
  `used_tokens`-when-`input_tokens`=0, input-larger, gate-fires-from-used
  at 200k, default-follows-native-gate, explicit-override-wins).
- `cargo test -p agent_ui --lib auto_prompt` — 19/19; `watchdog` — 3/3
  (`test_watchdog_does_not_fire_during_active_stream` flaked once under
  parallel load, passes in isolation and on re-run — timing-sensitive,
  unrelated to this change).
- `./script/clippy -p auto_prompt -p agent_ui` — clean (release,
  all-targets, all-features, deny warnings) + cargo-machete clean.
