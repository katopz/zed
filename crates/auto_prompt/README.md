# Auto Prompt

Intercepts AI agent stop events, calls a configured LLM via Zed's built-in language model infrastructure, and decides whether a follow-up prompt should be dispatched automatically.

Enabled by default. Toggle from the agent panel message editor toolbar — the sparkle icon next to "Follow the Zed Agent".

## Architecture

This crate contains decision logic only. The caller (`agent_ui`) handles actual GPUI action dispatch.

```
AcpThreadEvent::Stopped(stop_reason)
  │
  ├─ decide() — sync pre-check
  │   ├─ Config enabled? No → NoAction
  │   ├─ Used tools? No → NoAction
  │   ├─ Cancelled? Yes → NoAction
  │   ├─ Iteration > max? → NoAction
  │   ├─ Token overflow? → DispatchNow("continue")
  │   ├─ Error state? → DispatchAfterDelay("continue")
  │   └─ Otherwise → NeedsLlmCall(data)
  │
  └─ decide_with_llm() — async LLM call
      ├─ Sends context JSON to orchestration LLM
      ├─ Parses JSON response (should_continue, next_prompt, confidence)
      ├─ #ALL_PLAN_DONE or confidence < 0.5 → stop chain
      └─ Returns AutoPromptAction with next prompt
```

### Key types

- `AutoPromptDecision` — sync result: `NoAction`, `DispatchNow`, `DispatchAfterDelay`, `NeedsLlmCall`
- `AutoPromptAction` — data needed to dispatch a follow-up prompt (session ID, title, prompt text)
- `AutoPromptContext` — serializable context payload sent to the orchestration LLM
- `AutoPromptResponse` — expected JSON response from the LLM
- `AutoPromptConfig` — loaded from `~/.config/zed/auto_prompt.json` or env vars

### Files

| File | Purpose |
|------|---------|
| `auto_prompt.rs` | `decide()` (sync), `decide_with_llm()` (async), system prompt, iteration tracking, plan reading, LLM client |
| `config.rs` | Config from `~/.config/zed/auto_prompt.json` or env vars |
| `context.rs` | `AutoPromptContext`, `AutoPromptResponse`, plan/message serialization |

### Bridge in agent_ui

`crates/agent_ui/src/auto_prompt/mod.rs` — thin bridge that:
- Defines `ToggleAutoPrompt` GPUI action (toolbar sparkle button)
- Defines `AutoPromptNewThread` GPUI action (creates follow-up thread)
- `on_thread_stopped()` delegates to `auto_prompt::decide()`, handles async LLM path

Called from `conversation_view.rs` in the `AcpThreadEvent::Stopped` handler.

## Configuration

Config file: `~/.config/zed/auto_prompt.json`

```json
{
  "enabled": true,
  "max_iterations": 20,
  "max_context_tokens": 80000,
  "backoff_base_ms": 2000
}
```

| Field | Default | Description |
|-------|---------|-------------|
| `enabled` | `true` | Enable/disable auto-prompt |
| `system_prompt` | built-in | Override the orchestration LLM system prompt |
| `max_iterations` | `20` | Hard stop after this many auto-prompt cycles |
| `max_context_tokens` | `80000` | Token threshold to force "continue" without LLM |
| `backoff_base_ms` | `2000` | Base delay for exponential backoff on errors |

Environment variable overrides: `ZED_AUTO_PROMPT_ENABLED`, `ZED_AUTO_PROMPT_MAX_ITERATIONS`, `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS`, `ZED_AUTO_PROMPT_BACKOFF_BASE_MS`, `ZED_AUTO_PROMPT_SYSTEM_PROMPT`.

## E2E Testing

A full end-to-end test exercises the git flow with a helloworld Rust project.

### Setup

```bash
script/test-auto-prompt-e2e setup ./tmp/hw-test
```

This creates a Cargo project at `./tmp/hw-test` with a `.plan/01_helloworld_flow.md` plan file, initialized on `main` with a `develop` branch.

### Test with Zed

1. Build Zed:
   ```bash
   cargo build -p zed
   ```

2. Open the test project:
   ```bash
   target/debug/zed ./tmp/hw-test
   ```

3. Open Agent Panel (`cmd+i`) and send:
   ```
   Read .plan/01_helloworld_flow.md and execute the plan starting from Step 2.
   ```

4. Watch the auto-prompt loop fire on each `Stopped` event, call the orchestration LLM, and dispatch follow-up prompts until all plan items are complete.

### Verify

```bash
script/test-auto-prompt-e2e verify ./tmp/hw-test
```

Runs 12 checks: branches, tags, tests, conventional commits, version bumps, plan progress, function correctness.

### Other commands

```bash
script/test-auto-prompt-e2e status ./tmp/hw-test      # show git state
script/test-auto-prompt-e2e inject-bug ./tmp/hw-test   # inject bug for Step 7
script/test-auto-prompt-e2e teardown ./tmp/hw-test     # cleanup
```

## Conventions

The built-in system prompt enforces:

- **Git Flow**: `main`, `develop`, `feature/NN_description`, `hotfix/NN_description`, `release/vX.Y.Z`
- **Conventional Commits**: `feat:`, `fix:`, `chore:`, `refactor:`, `test:`, `docs:`
- **Plan Status Tracking**: `[ ]` / `[x]` checkboxes in `.plan/` folder files
