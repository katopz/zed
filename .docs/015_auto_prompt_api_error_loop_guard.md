Status: DONE (fixed, tests green, commit ref below)

# Auto-prompt infinite retry loop on API outage (GLM WAF 405 block)

Date: 2026-09-11

## Symptom

Zed Dev froze (main thread spinning, ~154% CPU, log stops mid-stream, terminal
PTY resize errors on closed channel) while the GLM API endpoint was blocked by
the Aliyun WAF (HTTP 405 with an HTML block page). Force-quit + restart cleared
it; the same signature happened before during a GLM retry storm.

Log evidence (pre-freeze window 07:08:40-07:09:47):

- `Thread::send` + `metadata generation call failed (attempt 1/2, 2/2)` pairs
  every ~5s (thread-title generation, 500ms retry apart)
- `Turn execution failed` / `Error in model response stream` / `Error in run
  turn` — all 405 from the provider
- `[auto_prompt] on_thread_stopped` firing right after each failed turn

## Root cause

Two no-LLM "continue" paths in auto_prompt converted an API outage into an
infinite send loop. Each cycle burned ~7 doomed requests (1 worker turn + 2
title-gen + 1 primary orchestration + 3 lightweight retries), which also kept
the WAF block alive:

1. **Retry-exhaustion safety nets** (`decide_with_llm`): when the orchestration
   call failed (synthetic failure → 3 lightweight retries → all failed), the
   `None` arm ran `detect_remaining_work` / `detect_remaining_plan_tasks`.
   Both can `Continue` without any LLM call, acting on a STALE last assistant
   message (the errored turn produced none). With unchecked `- [ ]` plan tasks
   present (always, in this workspace), the chain re-sent forever.

2. **Summary fast path** (`summary_continuation_fast_path`, called at the top
   of `decide_with_llm`): a stale voluntary-summary message short-circuited to
   `Continue` (including the housekeeping directive) with no LLM call and no
   `had_api_error` check.

The loop driver: `conversation_view.rs` error-path chaining calls
`on_thread_stopped(EndTurn)` on `AcpThreadEvent::Error` (needed for MaxTokens),
so every 405 turn error re-enters the decide pipeline, which "continued" back
into the same blocked API.

Note: the unified retry loop's `max_llm_retries` budget never engaged because
these paths returned `Ok(Continue)` (which also RESET the shared failure
counter) instead of an error or `RetryAfterBackoff`.

## Fix (commit refs in "Summary")

- `decide_with_llm` retry-exhaustion arm: new guard
  `None if should_defer_after_retry_exhaustion(data.had_api_error)` returns
  `RetryAfterBackoff` (config-backed delay) instead of running safety nets.
  The unified retry loop then applies backoff against the shared budget and
  converts to `Stopped` once `max_llm_retries` is spent — same philosophy as
  the issue-007 overflow guard, keyed on `had_api_error` (not `had_error`,
  which any failed tool call sets).
- `summary_continuation_fast_path`: skip when `data.had_api_error` — the
  summary is stale by definition (the turn errored before producing a new
  assistant message); fall through to the LLM path whose exhaustion guard
  defers.

Both keyed on `had_api_error` (set only by `run_turn`'s Err branch: network /
HTTP / stream failures), never on `had_error` (set by ordinary failed tool
calls) so healthy threads with flaky tools are unaffected.

## Loop budget after fix (persistent outage, per chain)

~3 decide rounds × (1 primary + 3 lightweight calls) + pre-call backoff
delays, then `Stopped` with reason surfaced. Global failure counter shared
across concurrent chains acts as a workspace-wide circuit breaker; a
successful decision resets it.

## Tests

- `test_retry_exhaustion_with_api_error_defers_instead_of_safety_net`
  (contract: exhaustion + had_api_error must defer)
- `summary_fast_path_skipped_on_api_error` (fast path steps aside on API error)
- Existing issue-007 + summary fast-path suites still green (406 passed).
