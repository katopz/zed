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
  │   ├─ Language model configured? No → NoAction
  │   ├─ Determine stop_phase (Working/PreStop) from VERIFICATION_COUNT
  │   ├─ Token overflow (max_context_tokens)? → DispatchNow("continue")
  │   ├─ StopReason::MaxTokens? → DispatchNow("continue")
  │   ├─ Error state or Refusal? → DispatchAfterDelay("continue")
  │   └─ Otherwise → NeedsLlmCall(data)
  │
  └─ decide_with_llm() — async LLM call
      ├─ Call orchestration LLM with context JSON
      ├─ On success:
      │   ├─ Writes decision log to `.logs/` in project root
      │   ├─ Parse response (should_continue, next_prompt, confidence, all_plan_done)
      │   ├─ #ALL_PLAN_DONE in prompt or response.all_plan_done?
      │   │   ├─ Find next plan file → yes → git branch commit + next plan
      │   │   └─ no → should_continue? → gitflow commit : stop
      │   ├─ Confidence < 0.5? → stop chain
      │   ├─ should_continue=false AND no next_prompt?
      │   │   ├─ verification_count=0? → pre-stop verification prompt, increment VERIFICATION_COUNT
      │   │   ├─ verification_count < max_attempts? → stop
      │   │   └─ verification_count >= max_attempts? → stop
      │   ├─ Doc creation prompt + unchecked plan items? → override with checkbox verification
      │   ├─ Continuing during PreStop (verification_count>0)? → reset VERIFICATION_COUNT
      │   └─ Return AutoPromptAction with next_prompt
      └─ On error:
          ├─ Writes error log to `.logs/`
          ├─ Stores LlmCallData for manual retry (if max retries exhausted)
          └─ Returns error
```

### Debug logs

Every LLM decision is logged to `.logs/` in the project root as JSON files:

```
.logs/
├── 2025-01-15T14-30-22.123_1.json       # iteration 1 decision
├── 2025-01-15T14-31-05.456_2.json       # iteration 2 decision
└── 2025-01-15T14-31-10.789_2_error.json # iteration 2 error
```

Each log file contains:

| Field | Description |
|-------|-------------|
| `timestamp` | ISO 8601 timestamp |
| `iteration` | Auto-prompt cycle number |
| `model` | LLM model identifier |
| `request.system_prompt` | The system prompt sent to the LLM |
| `request.context_json` | The full context JSON (messages, plan files, doc files) |
| `raw_response` | Raw text returned by the LLM |
| `parsed_response` | Parsed `should_continue`, `next_prompt`, `reason`, `all_plan_done`, `confidence` |
| `error` | Error message (error logs only) |

Add `.logs/` to `.gitignore` — these are for local debugging only.

### Core loop

The orchestration LLM follows a simple priority order:

1. **Plan steps remain** → continue next unchecked `[ ]` step
2. **New plan without checkboxes** → refine plan to add checkboxes
3. **AI asked a question** → auto-answer (pick option 1 or AI recommendation)
4. **All steps `[x]`** → fix diagnostics/tests, then create docs, then done
5. **No plan but work incomplete** → "continue"
6. **Confidence < 0.5** → stop

### Pre-stop verification

When the LLM indicates work is complete (`should_continue=false` with no prompt), the system enters a pre-stop verification phase:

1. First attempt (`verification_count=0`): Build verification prompt to check:
   - All compiler diagnostics and warnings fixed
   - All tests passing
   - No TODO/mock/placeholder/unwrap() in new code
2. Increment `VERIFICATION_COUNT`
3. If verification fails or LLM continues: Reset `VERIFICATION_COUNT` to 0 (new cycle)
4. Subsequent attempts: Accept the stop condition

### Quality gates

Before marking `all_plan_done=true`, the system enforces:

- Production grade: no mock, no TODO, no placeholder, no `unwrap()`
- Fix all compiler diagnostics and warnings
- Ensure test coverage for new code

### Key types

- `AutoPromptDecision` — sync result: `NoAction`, `DispatchNow`, `DispatchAfterDelay`, `NeedsLlmCall`
- `AutoPromptAction` — data needed to dispatch a follow-up prompt (session ID, title, prompt text)
- `LlmCallData` — data for async LLM call (model, system prompt, context JSON, iteration count); stored on failure for manual retry
- `AutoPromptContext` — serializable context payload sent to the orchestration LLM (includes `plan_files` and `doc_files`)
- `AutoPromptResponse` — expected JSON response from the LLM
- `AutoPromptConfig` — loaded from `~/.config/zed/auto_prompt.json` or env vars

### Files

| File | Purpose |
|------|---------|
| `auto_prompt.rs` | `decide()` (sync), `decide_with_llm()` (async), system prompt, iteration tracking, plan/doc reading, LLM client, verification prompts |
| `config.rs` | Config from `~/.config/zed/auto_prompt.json` or env vars |
| `context.rs` | `AutoPromptContext`, `AutoPromptResponse`, plan/message serialization |

### Bridge in agent_ui

`crates/agent_ui/src/auto_prompt/mod.rs` — thin bridge that:

- Defines `ToggleAutoPrompt` GPUI action (toolbar sparkle button)
- Defines `AutoPromptNewThread` GPUI action (creates follow-up thread)
- `on_thread_stopped()` delegates to `auto_prompt::decide()`, handles async LLM path

Called from `conversation_view.rs` in the `AcpThreadEvent::Stopped` handler.

### User Interface - Retry and Cancel

The auto-prompt toggle button in the agent panel toolbar (sparkle icon) displays four states, each with distinct behavior:

| Button State | Description | Click Behavior |
|--------------|-------------|-----------------|
| **"Auto"** | Auto-prompt is enabled and idle | Toggles to "Off" (disables auto-prompt) |
| **"Off"** | Auto-prompt is disabled | Toggles to "Auto" (enables auto-prompt) |
| **"Processing..."** | Auto-prompt is currently making an LLM decision or dispatching a follow-up prompt | Cancels the current operation (button returns to "Auto") |
| **"Retry"** | LLM call failed after all automatic retry attempts | Manually retries the failed LLM call with the same data |

#### Retry Mechanism

When the orchestration LLM call fails after exhausting all automatic retries (`max_llm_retries`), the system:

1. **Stores retry data**: The `LlmCallData` (model, system prompt, context JSON, etc.) is saved in the ThreadView for potential manual retry
2. **Enters Failed state**: The button displays "Retry" with error color
3. **Enables manual retry**: Clicking "Retry" triggers:
   - LLM failure count reset for fresh retry attempt
   - State changes to "Processing..." (button shows "Processing...")
   - Async task spawned to call `decide_with_llm()` with the stored data
   - On success: State → "Idle", retry data cleared, action dispatched
   - On failure: State → "Failed", retry data restored (allows multiple manual retries)

#### Cancel Mechanism

When auto-prompt is processing (button shows "Processing..."), clicking the button:

1. **Cancels the task**: Drops the current `_auto_prompt_task`, which cancels any ongoing LLM call or pending action dispatch
2. **Resets state**: Sets `auto_prompt_state` to `Idle`
3. **Resets iteration**: Clears the iteration counter via `reset_iteration()`
4. **Stops processing**: The async task's `is_cancelled()` check prevents any further actions from being dispatched

The cancel mechanism is useful for interrupting long-running LLM decisions or stopping the auto-prompt loop when the user wants to take manual control.

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
| `max_llm_retries` | `3` | Max automatic retry attempts for LLM calls before giving up and showing "Retry" button |
| `max_verification_attempts` | `3` | Max verification prompts in PreStop phase before accepting stop |

Environment variable overrides: `ZED_AUTO_PROMPT_ENABLED`, `ZED_AUTO_PROMPT_MAX_ITERATIONS`, `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS`, `ZED_AUTO_PROMPT_BACKOFF_BASE_MS`, `ZED_AUTO_PROMPT_SYSTEM_PROMPT`, `ZED_AUTO_PROMPT_MAX_LLM_RETRIES`.

## E2E Testing

A full end-to-end test exercises the git flow with a helloworld Rust project.

### Setup

```bash
script/test-auto-prompt-e2e setup /tmp/hw-test
```

This creates a Cargo project at `/tmp/hw-test` with a `.plan/01_helloworld_flow.md` plan file, initialized on `main` with a `develop` branch.

### Test with Zed

1. Build Zed:
   ```bash
   cargo build -p zed
   ```

2. Open the test project:
   ```bash
   target/debug/zed /tmp/hw-test
   ```

3. Open Agent Panel (`cmd+i`) and send:
   ```
   Read .plan/01_helloworld_flow.md and execute the plan starting from Step 2.
   ```

4. Watch the auto-prompt loop fire on each `Stopped` event, call the orchestration LLM, and dispatch follow-up prompts until all plan items are complete.

### Verify

```bash
script/test-auto-prompt-e2e verify /tmp/hw-test
```

Runs 12 checks: branches, tags, tests, conventional commits, version bumps, plan progress, function correctness.

### Other commands

```bash
script/test-auto-prompt-e2e status /tmp/hw-test      # show git state
script/test-auto-prompt-e2e inject-bug /tmp/hw-test   # inject bug for Step 7
script/test-auto-prompt-e2e teardown /tmp/hw-test     # cleanup