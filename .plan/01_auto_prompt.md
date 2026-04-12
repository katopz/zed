# Auto Prompt — Infinite Loop Prevention & Default Prompt Logic

## Problem

When auto_prompt fires on every `AcpThreadEvent::Stopped`, the system can enter an infinite loop:
- LLM completes a task, auto_prompt asks "what's next?", LLM finds something, auto_prompt fires again...
- LLM hits an error, auto_prompt fires, same error, auto_prompt fires...
- Token count grows, LLM hallucinates, generates bad prompts, auto_prompt fires...
- LLM asks a question, auto_prompt fires, LLM asks again, auto_prompt fires...

## Solution: 5-Case Decision Flow

When `AcpThreadEvent::Stopped` fires, the system follows this decision tree:

```
AcpThreadEvent::Stopped
  │
  ├─ Config enabled? ─── No ──→ STOP
  ├─ Used tools? ─────── No ──→ STOP
  ├─ Cancelled? ──────── Yes ─→ STOP (don't reset chain)
  │
  ├─ get_iteration() > max_iterations? ──→ CASE 5: hard STOP, reset counter
  │
  ├─ Collect context (messages, plan, .plan/ files, stop_reason, had_error)
  │
  ├─ Token count > max_context_tokens? ──→ CASE 4: force "continue", skip LLM
  │
  ├─ had_error OR stop_reason is Refusal/MaxTokens? ──→ CASE 3: backoff + "continue", skip LLM
  │
  ├─ Last message indicates completion? ──→ CASE 1: ask LLM to verify plan
  │
  ├─ Last message is a question? ──→ CASE 2: ask LLM to recommend or stop
  │
  └─ Normal: ask LLM to decide naturally
      │
      ├─ Response has #ALL_PLAN_DONE or all_plan_done:true ──→ STOP, reset counter
      ├─ Response confidence < 0.5 ──→ STOP, reset counter
      ├─ Response should_continue=false, no prompt ──→ STOP, reset counter
      ├─ Response has next_prompt ──→ dispatch AutoPromptNewThread
      └─ No prompt determined ──→ STOP, reset counter
```

### CASE 1: Completion Detected

**Trigger**: Last assistant message contains completion markers ("all done", "task complete", "nothing more to do", etc.)

**Action**: Send forced prompt asking LLM to verify plan against `.plan/` folder and code. Include plan stats (pending/in_progress/completed counts).

**LLM is told to**:
- Compare `current_plan` (from thread) against `plan_files` (from `.plan/` folder on disk)
- If ALL items verified done → respond with `#ALL_PLAN_DONE` → chain stops
- If items remain → set `next_prompt` to continue with next pending item

**Why**: The main LLM (Zed's agent) often says "all done" prematurely. The orchestration LLM double-checks against the original plan.

### CASE 2: Question Detected

**Trigger**: Last assistant message ends with `?` or contains question phrases ("should i", "do you", "what would", "how should", etc.)

**Action**: Send forced prompt asking LLM to re-examine plan and recommend.

**LLM is told to**:
- Re-examine `plan_files` and `current_plan` for context
- Choose the option that best aligns with the original plan
- If confidence < 0.5 → set `should_continue: false` with explanation → chain stops

**Why**: The main LLM asking a question means it's stuck. The orchestration LLM acts as the decision-maker using the plan as guidance.

### CASE 3: Error State

**Trigger**: `had_error == true` OR `stop_reason` is `Refusal` or `MaxTokens`

**Action**: Skip the orchestration LLM entirely. Force `next_prompt = "continue"` after an exponential backoff delay.

**Backoff formula**: `backoff_base_ms * 2^min(retry_count, 5)`, capped at 60 seconds.
- Default base: 2000ms
- Iteration 1: 4s, Iteration 2: 8s, Iteration 3: 16s, Iteration 4: 32s, Iteration 5+: 60s

**Why**: Errors are often transient (rate limit, network blip). Retrying with backoff is the standard approach. Skipping the LLM call saves tokens and time.

### CASE 4: Token Overflow

**Trigger**: Approximate token count exceeds `max_context_tokens` (default: 80,000)

**Action**: Skip the orchestration LLM entirely. Force `next_prompt = "continue"` immediately.

**Token estimation**: Total characters from messages + plan + plan_files, divided by 4.

**Why**: Sending massive context to the orchestration LLM causes hallucination and wastes tokens. Forcing "continue" lets the main LLM pick up where it left off in a fresh thread (with summary), which naturally compresses context.

### CASE 5: Max Iterations

**Trigger**: `iteration_count > max_iterations` (default: 20)

**Action**: Hard stop. Reset iteration counter. No prompt, no LLM call.

**Why**: The ultimate safety net. Even if all other checks fail, the chain cannot exceed 20 cycles. Configurable via `max_iterations`.

## Stop Conditions Summary

| Signal | Source | Effect |
|--------|--------|--------|
| `#ALL_PLAN_DONE` in response | Orchestration LLM | Reset counter, stop |
| `all_plan_done: true` | Orchestration LLM | Reset counter, stop |
| `confidence < 0.5` | Orchestration LLM | Reset counter, stop |
| `should_continue: false`, no prompt | Orchestration LLM | Reset counter, stop |
| `StopReason::Cancelled` | User | Skip (don't reset counter) |
| `iteration > max_iterations` | System | Reset counter, stop |
| Empty prompt after cleanup | System | Reset counter, stop |
| No prompt determined | System | Reset counter, stop |

## Iteration Counter & Chain Timeout

The iteration counter is stored in a global `AtomicU32`. It increments on every `on_thread_stopped` call.

**Chain timeout**: If more than 300 seconds (5 min) pass since the last iteration, the counter resets to 0. This means:
- User manually sends a message after a pause → fresh chain
- Auto-prompt chain with long pauses → fresh chain each time

**Counter reset**: The counter resets when the chain stops (any stop condition) or when max iterations is reached. This means the next user-initiated message starts a fresh chain.

## Configuration

File: `~/.config/zed/auto_prompt.json`

```json
{
  "enabled": true,
  "system_prompt": null,
  "max_iterations": 20,
  "max_context_tokens": 80000,
  "backoff_base_ms": 2000
}
```

Environment variables (fallback when no config file):
- `ZED_AUTO_PROMPT_ENABLED=true`
- `ZED_AUTO_PROMPT_SYSTEM_PROMPT=...`
- `ZED_AUTO_PROMPT_MAX_ITERATIONS=20`
- `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS=80000`
- `ZED_AUTO_PROMPT_BACKOFF_BASE_MS=2000`

## LLM Response Format

The orchestration LLM must return JSON:

```json
{
  "should_continue": true,
  "next_prompt": "Now run the tests and fix any failures",
  "reason": "Tests haven't been run yet",
  "all_plan_done": false,
  "confidence": 0.9
}
```

## Context Sent to Orchestration LLM

`AutoPromptContext` JSON includes:

| Field | Description |
|-------|-------------|
| `current_datetime` | ISO 8601 timestamp |
| `current_paths` | Work directories |
| `session_id` | Thread session ID |
| `title` | Thread title |
| `messages` | Serialized conversation (User/Assistant/Tool/Plan roles) |
| `used_tools` | Whether tools were used |
| `entry_count` | Total entries in thread |
| `current_plan` | Plan entries from thread with status/priority |
| `plan_files` | Contents of files from `.plan/` folder |
| `stop_reason` | Why thread stopped (end_turn, max_tokens, etc.) |
| `had_error` | Whether thread had errors |
| `approximate_token_count` | Estimated tokens (chars/4) |
| `iteration_count` | Current auto-prompt iteration |
| `was_truncated` | Whether context was truncated |

## File Structure

```
crates/agent_ui/src/auto_prompt/
├── mod.rs        # Entry point, on_thread_stopped(), decision flow, iteration tracking
├── client.rs     # Calls Zed's LLM via LanguageModel::stream_completion()
├── config.rs     # Loads ~/.config/zed/auto_prompt.json or env vars
└── context.rs    # AutoPromptContext, AutoPromptResponse, plan/message serialization
```

Minimal changes outside this folder:
- `agent_ui.rs`: 1 line `mod auto_prompt;`
- `conversation_view.rs`: 1 line in `AcpThreadEvent::Stopped` handler (passes `stop_reason`)
- `agent_panel.rs`: ~20 lines to register `AutoPromptNewThread` action handler

## Flow Diagram

```
User sends message
    ↓
Zed's Agent LLM processes (tools, edits, etc.)
    ↓
AcpThreadEvent::Stopped(stop_reason)
    ↓
on_thread_stopped() checks:
    ├─ enabled? used_tools? not cancelled?
    ├─ iteration > max_iterations? → STOP
    ├─ token overflow? → force "continue" → new thread
    ├─ error state? → backoff + "continue" → new thread
    └─ otherwise → call orchestration LLM
        ├─ #ALL_PLAN_DONE? → STOP
        ├─ low confidence? → STOP
        ├─ no prompt? → STOP
        └─ has prompt → dispatch AutoPromptNewThread
            ↓
            AgentPanel creates new thread with summary link + prompt
            ↓
            Thread auto-submits
            ↓
            (loop back to top)
```
