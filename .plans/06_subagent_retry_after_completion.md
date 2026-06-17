# Subagent Retry-After-Completion Fix

## Problem

When a subagent's first turn has already completed and the user retries it:
1. `restart_subagent_tool_call` sets the parent's tool_call status to `InProgress`
2. The subagent's retry turn runs and completes
3. Nobody transitions the parent's tool_call back to a terminal state

Result: the parent's tool_call stays `InProgress` forever:
- Loading spinner never clears ("keep spinning")
- Stop button doesn't work (`cancel()` returns early when no `running_turn`)

## Root Cause

`NativeSubagentHandle::send` (agent.rs) handles retry-**during**-generation by
detecting `turn_id` increments and waiting for the new turn. This works because
the `send` future is still alive.

For retry-**after**-completion, the `send` future has already returned. The
normal `process_tool_result` path (which updates the parent's tool_call via the
event stream) does not fire for the retry. Nothing transitions the parent's
tool_call from `InProgress` to `Completed`/`Failed`.

## Fix (Option A — minimal, recommended)

On retry completion in `ThreadView::retry_generation`, transition the parent's
tool_call to a terminal state with the new subagent output. The parent LLM is
not auto-re-invoked — the user prompts again to use the new result.

### Changes

- [x] `AcpThread::complete_subagent_tool_call` — transitions a subagent's
      tool_call to `Completed`/`Failed` and replaces its content with the new
      output
- [x] `AcpThread::last_assistant_message_text` — extracts the text content of
      the last assistant message (used to get the subagent's retry output)
- [x] `ThreadView::retry_generation` — on retry completion (any outcome),
      calls `complete_subagent_tool_call` on the parent's thread
- [x] Regression test: `test_complete_subagent_tool_call` in acp_thread

### Out of scope

- Auto-re-invoking the parent LLM (Option B)
- Truncating parent state back to the tool call (Option C)
- Fixing `cancel()` early-return when `running_turn` is `None` (separate concern)
