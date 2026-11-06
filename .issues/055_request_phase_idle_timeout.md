# 055: Request-phase hang never triggers the stream idle timeout (no auto retry)

Status: in-progress (fix + tests)

## Symptom

Native agent turns wedge in `Generating` forever when the provider accepts the
HTTP connection but never delivers a response. The configured
`agent.stream_idle_timeout_secs` (default 120) never fires and no auto retry
happens. Observed 2026-11-06 00:52–01:03: two threads stuck generating for
20+ minutes, saturating the auto_prompt stream cap
("stream cap saturated (2 generating, cap 2)"), until the 1800s watchdog
cancelled them. No "No completion events" warn line in either Zed.log — the
idle timer never fired at all.

## Root cause

`Thread::run_turn_internal` awaits `provider.stream_completion(...)` bare:

```rust
let events = match provider {
    Ok(provider) => provider.stream_completion(&model, request, cx).await,
    ...
};
let mut stream_idle_timeout = Self::stream_idle_timeout_future(...); // armed only after
```

The idle timer guards the *event stream*, so it only exists once
`stream_completion` resolves — which for `open_ai_compatible` (and every other
provider) includes the HTTP request + response-header round trip
(`with_key_rotation`'s `send` returns the stream "right after HTTP setup").
A provider that accepts the connection and never sends headers hangs before
any timer is armed; Zed's app HTTP client is built with
`proxy_and_user_agent` → `read_timeout = None`, so nothing else bounds it.

The compaction path already races its request phase against the same timer
(`request_idle_timeout` in `stream_compaction`); the main turn never got the
same treatment.

## Fix

Race the turn's request phase against `stream_idle_timeout_future`, mirroring
`stream_compaction`. A timeout produces the typed `StreamIdleTimeout` error, so
the existing `STREAM_IDLE_RETRY_STRATEGY` (4 retries × 30s) drives the auto
retry, and once the budget is exhausted the turn ends with a visible error
instead of hanging. Cancel-during-hang is still honored by the existing gates
(the post-loop `cancellation_rx.borrow()` check and `retry_completion_error`'s
select).

## Tests

- Fake provider gains a `hang_stream_completions` mode: registers the pending
  completion (so attempts are countable) and returns a never-resolving future
  — the exact request-phase hang.
- `test_request_phase_idle_timeout_retries_hung_request`: 5 attempts
  (initial + 4 retries), then the turn ends with the idle-timeout error.
- `test_request_phase_idle_timeout_recovers_mid_cascade`: two hung requests,
  provider recovers on the third, turn completes without surfacing an error.
