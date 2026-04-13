# Auto-Prompt Debug Logging

This document describes the comprehensive debug logging added to the auto-prompt system for troubleshooting and monitoring.

## Overview

The auto-prompt system now includes extensive logging at every key decision point, prefixed with `[auto_prompt]` for easy filtering. Logs are categorized by module and function to help trace the execution flow.

## Recent Improvements

### Error Handling & Rate Limits
- Auto-prompt now triggers on **both** `Stopped` and `Error` events
- Rate limit errors are automatically retried with exponential backoff
- Error state is logged with `stop_reason` for easy debugging

### Config Caching
- Config is cached in memory to avoid repeated file reads
- Cache is invalidated when config file is modified
- Cache status is logged (HIT, MISS, or STALE)

## Log Categories

### Entry Point (`on_thread_stopped`)

Located in `crates/agent_ui/src/auto_prompt/mod.rs`

```log
[auto_prompt] on_thread_stopped called: used_tools=true, stop_reason=EndTurn
[auto_prompt] decision result: NeedsLlmCall(LlmCallData { ... })
[auto_prompt] NoAction - taking no action
[auto_prompt] DispatchNow - dispatching action with prompt: continue
[auto_prompt] DispatchAfterDelay - scheduling action in 2000ms with prompt: continue
[auto_prompt] NeedsLlmCall - spawning task to call LLM with model: ProviderId(OpenAI)
[auto_prompt] LLM returned action - dispatching with prompt: Continue with Step 3
[auto_prompt] LLM returned no action
```

**What to look for:**
- `used_tools` must be `true` for auto-prompt to proceed
- `stop_reason` should not be `Cancelled`
- Decision result tells you which path was taken
- For errors: `stop_reason=MaxTokens` indicates rate limit or error

### Decision Logic (`decide`)

Located in `crates/auto_prompt/src/auto_prompt.rs`

```log
[auto_prompt::decide] Starting decision process
[auto_prompt::config] Config cache HIT
# OR (first load):
[auto_prompt::config] Config cache MISS
[auto_prompt::config] Config path: "/Users/user/.config/zed/auto_prompt.json"
[auto_prompt::config] Config file exists, loading from file
[auto_prompt::config] Config loaded and cached
# OR (file modified):
[auto_prompt::config] Config cache STALE (file modified), reloading
[auto_prompt::config] Config loaded and cached
```
[auto_prompt::decide] Auto-prompt is ENABLED
[auto_prompt::decide] Tools were used, continuing evaluation
[auto_prompt::decide] Stop reason: EndTurn
[auto_prompt::decide] Current iteration: 1
[auto_prompt::decide] Using model: ProviderId(OpenAI)
[auto_prompt::read_plan_files] Starting to read plan files
[auto_prompt::read_plan_files] Found 1 work directory/ies
[auto_prompt::read_plan_files] Checking for plan directory: "/tmp/hw-test/.plan"
[auto_prompt::read_plan_files] Found plan directory
[auto_prompt::read_plan_files] Loaded 2 plan file(s): [...]
[auto_prompt::decide] Approximate token count: 1234
[auto_prompt::decide] Had error: false
[auto_prompt::decide] Normal state, will call LLM for decision
[auto_prompt::decide] Context serialized successfully (5678 chars)
[auto_prompt::decide] Returning NeedsLlmCall decision
```

**Common early exits:**
- `Auto-prompt is DISABLED in config` - Check config file or environment
- `No tools were used, skipping auto-prompt` - Expected behavior for non-tool conversations
- `Thread was cancelled, skipping auto-prompt` - User cancelled, correct behavior
- `No language model configured in Zed` - Configure a model in Zed settings
- `Max iterations (20) reached, stopping chain` - Prevents infinite loops

### LLM Call (`decide_with_llm`)

```log
[auto_prompt::decide_with_llm] Starting LLM call, iteration=1, model=ProviderId(OpenAI), session_id=SessionId("abc123")
[auto_prompt::decide_with_llm] Forced prompt: None
[auto_prompt::decide_with_llm] LLM call completed with result: Ok
[auto_prompt::decide_with_llm] Response received: should_continue=true, has_next_prompt=true, all_plan_done=false, confidence=Some(0.9)
[auto_prompt::decide_with_llm] Reason: Step 2 is complete, continuing to Step 3
[auto_prompt::decide_with_llm] Next prompt: Continue with Step 3: Add Greet-by-Name Feature
[auto_prompt] NeedsLlmCall - spawning task to call LLM with model: ...
[auto_prompt] LLM returned action - dispatching with prompt: Continue with Step 3
```

**LLM response patterns:**
- `should_continue=false` - LLM decided to stop the chain
- `all_plan_done=true` - All plan items completed
- `confidence < 0.5` - LLM uncertain, stopping for safety
- `has_next_prompt=false` - No next action needed

### Config Loading (Cached)

```log
# First load:
[auto_prompt::config] Config cache MISS
[auto_prompt::config] Config path: "/Users/user/.config/zed/auto_prompt.json"
[auto_prompt::config] Config file exists, loading from file
[auto_prompt::config] Config loaded and cached

# Subsequent loads (cache hit):
[auto_prompt::config] Config cache HIT

# When file is modified:
[auto_prompt::config] Config cache STALE (file modified), reloading
[auto_prompt::config] Config loaded and cached

# When config is saved manually:
[auto_prompt::config] Config cache invalidated
```

**Config sources (in order of precedence):**
1. `~/.config/zed/zed/auto_prompt.json`
2. Environment variables (`ZED_AUTO_PROMPT_ENABLED`, etc.)
3. Default values (`enabled=true`)

**Note:** Config is cached in memory to avoid repeated file reads. The cache is automatically invalidated when the config file is modified (detected via file modification time).

### Plan File Reading

```log
[auto_prompt::read_plan_files] Starting to read plan files
[auto_prompt::read_plan_files] Found 1 work directory/ies
[auto_prompt::read_plan_files] Checking for plan directory: "/path/to/project/.plan"
[auto_prompt::read_plan_files] Plan directory does not exist
# OR:
[auto_prompt::read_plan_files] Found plan directory
[auto_prompt::read_plan_files] Loaded 2 plan file(s): ["/path/.plan/01_plan.md", "/path/.plan/02_e2e.md"]
# OR:
[auto_prompt::read_plan_files] No plan files found in any .plan directory
```

## How to View Logs

### All Auto-Prompt Logs

```bash
RUST_LOG=info zed
# Or if Zed is already running, filter with:
journalctl -f | grep "\[auto_prompt\]"
```

### Specific Categories

```bash
# Only decision logic
RUST_LOG=info zed | grep "\[auto_prompt::decide\]"

# Only LLM calls
RUST_LOG=info zed | grep "\[auto_prompt::decide_with_llm\]"

# Only config loading
RUST_LOG=info zed | grep "\[auto_prompt::config\]"

# Only plan file reading
RUST_LOG=info zed | grep "\[auto_prompt::read_plan_files\]"
```

### Log Levels

- `info` - Normal operation flow
- `warn` - Potential issues (e.g., config load failed, no model configured)
- `debug` - Detailed info (rarely used in this crate)

## Troubleshooting Guide

### Auto-Prompt Not Triggering

1. **Check if enabled:**
   ```bash
   grep "Auto-prompt is ENABLED\|Auto-prompt is DISABLED" zed.log
   ```

2. **Verify tools were used:**
   ```bash
   grep "used_tools=" zed.log
   ```

3. **Check stop reason:**
   ```bash
   grep "Stop reason:" zed.log
   ```

### LLM Not Called

1. **Model configured:**
   ```bash
   grep "Using model:" zed.log
   ```

2. **Context serialized:**
   ```bash
   grep "Context serialized successfully" zed.log
   ```

3. **Decision result:**
   ```bash
   grep "decision result:" zed.log
   ```

### Plan Files Not Found

1. **Work directories:**
   ```bash
   grep "Found.*work directory" zed.log
   ```

2. **Plan directory:**
   ```bash
   grep "Checking for plan directory:" zed.log
   ```

3. **Files loaded:**
   ```bash
   grep "Loaded.*plan file" zed.log
   ```

### Chain Stops Unexpectedly

1. **Max iterations:**
   ```bash
   grep "Max iterations.*reached" zed.log
   ```

2. **Low confidence:**
   ```bash
   grep "Confidence too low" zed.log
   ```

3. **All plan done:**
   ```bash
   grep "#ALL_PLAN_DONE" zed.log
   ```

4. **LLM says stop:**
   ```bash
   grep "LLM says stop" zed.log
   ```

5. **Rate limit errors:**
   ```bash
   grep "Error/Rate Limit detected" zed.log
   grep "backing off.*ms" zed.log
   ```

## Example Full Flow

Here's a complete log trace of a successful auto-prompt cycle:

```log
[agent] Received prompt request for session: abc123
[auto_prompt] on_thread_stopped called: used_tools=true, stop_reason=EndTurn
[auto_prompt::decide] Starting decision process
[auto_prompt::config] Config cache HIT
[auto_prompt::decide] Auto-prompt is ENABLED
[auto_prompt::config] Config file exists, loading from file
[auto_prompt::config] Config loaded and cached
[auto_prompt::decide] Auto-prompt is ENABLED
[auto_prompt::decide] Tools were used, continuing evaluation
[auto_prompt::decide] Stop reason: EndTurn
[auto_prompt::decide] Current iteration: 1
[auto_prompt::decide] Using model: ProviderId(OpenAI)
[auto_prompt::read_plan_files] Starting to read plan files
[auto_prompt::read_plan_files] Found 1 work directory/ies
[auto_prompt::read_plan_files] Found plan directory
[auto_prompt::read_plan_files] Loaded 1 plan file(s): ["/tmp/hw-test/.plan/01_helloworld_flow.md"]
[auto_prompt::decide] Approximate token count: 1234
[auto_prompt::decide] Had error: false
[auto_prompt::decide] Normal state, will call LLM for decision
[auto_prompt::decide] Context serialized successfully (5678 chars)
[auto_prompt::decide] Returning NeedsLlmCall decision
[auto_prompt] decision result: NeedsLlmCall(LlmCallData { model: ProviderId(OpenAI), ... })
[auto_prompt] NeedsLlmCall - spawning task to call LLM with model: ProviderId(OpenAI)
[auto_prompt::decide_with_llm] Starting LLM call, iteration=1, model=ProviderId(OpenAI), session_id=SessionId("abc123")
[auto_prompt::decide_with_llm] Forced prompt: None
[auto_prompt::decide_with_llm] LLM call completed with result: Ok
[auto_prompt::decide_with_llm] Response received: should_continue=true, has_next_prompt=true, all_plan_done=false, confidence=Some(0.9)
[auto_prompt::decide_with_llm] Reason: Step 2 is complete, continuing to Step 3
[auto_prompt::decide_with_llm] Next prompt: Continue with Step 3: Add Greet-by-Name Feature
[auto_prompt] LLM returned action - dispatching with prompt: Continue with Step 3: Add Greet-by-Name Feature
[agent] Received prompt request for session: xyz789 (new thread)
```

### Rate Limit Retry Example

```log
[auto_prompt] on_thread_stopped called: used_tools=true, stop_reason=MaxTokens
[auto_prompt] Error/Rate Limit detected - stop_reason=MaxTokens, will apply backoff retry
[auto_prompt::decide] Starting decision process
[auto_prompt::config] Config cache HIT
[auto_prompt::decide] Config loaded: enabled=true
[auto_prompt::decide] Auto-prompt is ENABLED
[auto_prompt::decide] Tools were used, continuing evaluation
[auto_prompt::decide] Stop reason: MaxTokens
[auto_prompt::decide] Had error: true
[auto_prompt::decide] Error state detected, backing off 2000ms
[auto_prompt] decision result: DispatchAfterDelay { action: ..., delay_ms: 2000 }
[auto_prompt] DispatchAfterDelay - scheduling action in 2000ms with prompt: continue
```

## Error Handling & Retries

### Automatic Error Detection
Auto-prompt detects errors in two ways:
1. **Thread-level errors** (via `AcpThreadEvent::Error`) - e.g., rate limits, network errors
2. **Thread-level stop reasons** - `MaxTokens`, `Refusal`

### Exponential Backoff
When an error is detected, auto-prompt applies exponential backoff:
- Base delay: 2000ms (configurable via `backoff_base_ms`)
- Retry 1: 2000ms
- Retry 2: 4000ms
- Retry 3: 8000ms
- Retry 4+: 16000ms (capped at 60s)

### Error States
- **Rate limits**: `stop_reason=MaxTokens` with `had_error=true`
- **Network errors**: `AcpThreadEvent::Error` emitted
- **Token overflow**: Context exceeds `max_context_tokens`
- **Refusals**: `stop_reason=Refusal` with `had_error=true`

All error states result in `DispatchAfterDelay` with a "continue" prompt, allowing the chain to recover.

## Config File Format

The config file at `~/.config/zed/zed/auto_prompt.json`:

```json
{
  "enabled": true,
  "system_prompt": null,
  "max_iterations": 20,
  "max_context_tokens": 80000,
  "backoff_base_ms": 2000
}
```

## Environment Variables

- `ZED_AUTO_PROMPT_ENABLED` - "1" or "true" to enable
- `ZED_AUTO_PROMPT_SYSTEM_PROMPT` - Custom system prompt
- `ZED_AUTO_PROMPT_MAX_ITERATIONS` - Max iterations (default: 20)
- `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS` - Token limit (default: 80000)
- `ZED_AUTO_PROMPT_BACKOFF_BASE_MS` - Backoff delay base (default: 2000)