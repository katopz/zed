# 047 — `test_manual_auto_prompt_forks_claude_thread_above_overflow_gate` reads ambient user config

- Status: fixed (commit recorded below)
- Crate: `agent_ui` (fork_tests) / `auto_prompt`
- Type: test hermeticity (pre-existing failure on dev machines, passes on CI)

## Symptom

`cargo test -p agent_ui` fails deterministically on this machine at
`crates/agent_ui/src/conversation_view/tests/fork_tests.rs:834`:

```
a Claude thread above the overflow gate must not continue in place; got: ## User
do work
## User
Continue remaining work ...
```

Verified failing on a pristine HEAD worktree (no local changes), so not caused
by in-flight work.

## Root cause

The test computes the gate from `AutoPromptConfig::default()` (200k, see
`default_max_context_tokens`), but the production dispatch path
(`dispatch_action_with_attempts` in `crates/agent_ui/src/auto_prompt/mod.rs`)
reads `auto_prompt::load_config_cached()`, which loads the **real user config**
`paths::config_dir()/auto_prompt.json`.

This machine has:

```json
{ "max_context_tokens": 320000 }
```

So the test sets `used_tokens = 200_001` (above the default 200k gate) while
the effective gate under the loaded config is 320_000 → no fork → the thread
continues in place → assertion fails. The test is non-hermetic: it passes on
machines without that config file and fails on machines with a larger gate.

## Fix

Derive the gate in the test from the same source production reads:

```rust
auto_prompt::load_config_cached().unwrap_or_default()
    .effective_claude_context_overflow_tokens()
```

so the test is hermetic to any ambient config/env override while still
guarding the original regression (the gate must read `used_tokens`, not the
always-zero-for-Claude `input_tokens` — see the test's doc comment).

## Commit

- fix: `<hash>`
