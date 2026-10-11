# Unreachable-API retry forked context-less threads and stacked continuation prompts

status: fixed 2026-10-11 in 24f7b22eb4 (auto_prompt in-place retry, per-session streak, Resume dedupe, Cancelled early-exit)

## Symptom

During a ~1h outage (2026-10-11 11:19–12:24 local, every request failed with
`dns error: failed to lookup address information`) GLM threads looked stuck in
"infinite retry", new threads appeared with titles like `## 1. Thread Summary`,
and the threads that did retry in place filled up with dozens of
`The provider API was unreachable … Continue from where you left off.` +
`## Start context` blocks. Interrupting a Claude thread also showed
"Processing…" over the user's follow-up.

## Verdict: not a GLM outage, not a key-backoff regression

- `curl` to `https://api.z.ai/api/coding/paas/v4/chat/completions` → 401 in 0.3s
  (endpoint up). Every failure in `Zed.log` / `Zed.log.old` was a local DNS
  resolver failure (`client error (Connect): dns error`, 75 + 477 hits, plus a
  single `operation timed out`). Zed uses reqwest's getaddrinfo resolver with no
  in-process negative cache, so this was the machine's resolver, not Zed state.
- Recovery: every request from 12:42 onward succeeded, both new threads and the
  resumed `ddadff23` thread. "New thread works, resume fails" was timing (DNS
  came back between 12:24 and 12:42), not thread-specific state.
- Key health behaved as designed: transport failures use the 5s–60s transport
  schedule (`K1 … 56s window, 0 failure(s)` in the screenshot) and never touch
  the quota counter (issue 023).

## Real bugs (ours)

1. **Fork without context.** `api_unreachable_decision` passed the thread's
   `input_tokens`; above the 200k same-thread threshold `dispatch_action` forked
   a *new* native thread whose only content was the 117-char retry sentence.
   The fork hit the same dead API, forked again, and fired 4 key probes each time.
2. **Prompt spam.** Each in-place retry sent a fresh user message plus a
   `## Start context` block; failed turns never produce a reply, so they stacked.
3. **Global streak.** `UNREACHABLE_STREAK` was one process-wide counter shared by
   every retrying thread (observed streak 43→81+ across 4 sessions), so the
   240-retry budget drained N× faster and one thread's clean turn reset all others.
4. **Claude interrupt → Processing.** `run_auto_prompt` set `Processing` before
   the Claude path awaited `warm_plan_snapshots` (up to ~10s of git fetches) and
   only then returned NoAction for `StopReason::Cancelled`.

## Fix

- [x] `api_unreachable_decision`: `actual_input_tokens: None` so the retry always
  lands on the same thread.
- [x] New `AutoPromptDelayReason::ApiUnreachable`; the delay handler re-runs the
  failed turn via `ThreadView::retry_generation` (same as the callout's Retry
  button) when the agent supports retry, otherwise falls back to `dispatch_action`.
  A pending Phase 1 summarize keeps `Backoff` so the summarize request is sent.
- [x] `Thread::resume` no longer pushes a second `Message::Resume` when the last
  one was never answered (test `test_repeated_resume_after_failure_does_not_stack`).
- [x] Per-session unreachable streaks (`api_unreachable::record_unreachable(session_id)`,
  test `streaks_are_tracked_per_session`).
- [x] `run_auto_prompt` returns early on automatic `Cancelled` stops before
  setting `Processing`.
