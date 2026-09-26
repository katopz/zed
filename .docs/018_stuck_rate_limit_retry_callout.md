Status: DONE — fixed + tested 2026-09-26 (commits below); no separate
issue file was filed (found live via a stuck toast screenshot, fixed
in-thread; this doc is the durable record)

# Stuck rate-limit retry callout + absurd provider retry_after (GLM 429, 9428 s)

Symptom seen 2026-09-26: agent panel showed "GLM's API rate limit
exceeded — Retrying. Next attempt in 9428 seconds (Attempt 1 of 4)"
pinned over a turn that was already running fine, and the callout had
no dismiss button.

## Root cause (two halves)

1. **Toast lifetime had no "superseded" transition.**
   `ThreadView::thread_retry_status` was cleared only by
   `cancel_generation`, or by the `Stopped`/`Error` handlers *when
   `!is_generating`*. A new generation (user send, queued send,
   auto-prompt continuation, checkpoint RETRY) never cleared the
   previous turn's callout, so a huge backoff toast survived while the
   next turn ran clean. The generic callout also auto-hid only when a
   repaint happened *after* its backoff expiry — during a silent
   backoff nothing repaints, so it just sat there.

2. **Provider `retry_after` honored raw.**
   `Thread::retry_strategy_for` used `retry_after.unwrap_or(BASE_RETRY_DELAY)`
   at 5 sites. GLM/OpenAI-compatible upstreams send multi-hour values
   on transient 429s (observed: 9428 s ≈ 2.6 h) — the turn sits in
   silent backoff for hours with the stale callout pinned (see 1).

## Fixes

- `bc157c0a1f` — `start_turn()` (the single funnel every new generation
  passes through: send / queued send / auto-prompt / checkpoint retry)
  now drops any previous turn's retry status; `Stopped`/`Error` handlers
  clear `thread_retry_status` unconditionally (`is_generating` now gates
  only scroll/auto-expand); generic retry callout gained a dismiss
  button (parity with the refusal-fallback variant).
- `33db19f126` — `MAX_RETRY_AFTER = 120 s` cap via a shared
  `retry_after_delay()` helper across all `retry_after`-derived delays;
  regression test `test_send_retry_after_capped_on_rate_limit` proves a
  9428 s hint backs off for the capped delay and recovers (advances the
  clock by only the cap — hangs without the fix).
- `cada333f50` — dedup: the identical 56-line `retry_on_rate_limit`
  eval helper (edit_file/terminal_tool/write_file) hoisted into
  `tools/evals.rs` (mechanical, found in the same thread).

## Tests

- agent: `test_send_retry_after_capped_on_rate_limit` (new),
  `test_send_retry_on_error`, `test_send_max_retries_exceeded` green.
- agent_ui: `test_retry_callout_cleared_when_turn_stops`,
  `test_retry_callout_cleared_by_new_turn` (new) — pin the callout
  lifecycle end-to-end via `StubAgentConnection`.

## Operational note

Users of this fork must restart the dev build to pick the fixes up; a
toast already on screen from an older build clears itself at the next
repaint past the (now capped) backoff, or on the next turn start.
