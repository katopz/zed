# OpenAI Compatible: Pre-First-Event Stream Retry

## Goal
Extend intra-request key rotation to catch errors that surface as the first
stream event (e.g., late-detected rate limits, upstream errors returned after
the HTTP connection succeeds). Currently, `retry_stream` only retries errors
that occur during stream setup (the `do_attempt` future). Once the future
resolves to a stream, any error in the stream itself propagates to the
consumer and terminates the request.

## Why
The HTTP request can succeed (200 OK, SSE connection established) while the
first server-sent event is an error. Common cases:
- Provider returns 200 then sends an error event in the SSE stream
- Load balancer accepts the connection but the upstream returns an error
- Rate limit detected after connection establishment (queue full)

These are classified as backoff-worthy (`RateLimitExceeded`,
`UpstreamProviderError`, `ServerOverloaded`, etc.) and should trigger key
rotation just like setup-phase errors. Without this change, the user sees
the error and has to manually retry; with it, the rotation happens
transparently before the consumer ever sees the event.

## Design

### `probe_first_event<T, E>(stream) -> Result<BoxStream<Result<T, E>>, LanguageModelCompletionError>`
A small async helper that pulls exactly one event from the stream and
classifies the outcome:

- `Some(Ok(first))` — re-prepends the event via `stream::once(first).chain(stream).boxed()` and returns `Ok`. The consumer sees the event in its original position; nothing is lost.
- `Some(Err(e))` — converts via `E: Into<LanguageModelCompletionError>` and returns `Err`. The caller (`retry_stream` via the `do_attempt` closure) records the failure and retries on the next key.
- `None` — empty stream; returns `Ok(stream)` unchanged. Treated as success because an empty stream is not an error (it may indicate a content-filter no-op, but that's the provider's decision, not a transport failure).

The re-prepend uses `futures::stream::once(async move { first }).chain(stream)`, then `.boxed()` to return a `BoxStream` matching the input type. This keeps `retry_stream<S>` generic — it still sees `S = BoxStream<Result<T, E>>` and doesn't need a `Stream` bound.

### Integration into `stream_completion` / `stream_response` closures
The `do_attempt` closure in each call site changes from:

```rust
Box::pin(async move {
    stream_completion(...).await.map_err(Into::into)
})
```

to:

```rust
Box::pin(async move {
    let stream = stream_completion(...).await.map_err(Into::into)?;
    probe_first_event(stream).await
})
```

`retry_stream` itself is unchanged — it still records success/failure based on whether `do_attempt` returns `Ok`/`Err`, so the first-event error is recorded exactly like a setup-phase error.

### Why a helper, not a change to `retry_stream`
`retry_stream<S>` is currently generic over an opaque `S` (used in tests with
`S = i32`, `S = ()`). Adding a `Stream` bound would break those tests and
constrain the function unnecessarily. By moving the probe into the closure,
`retry_stream` stays simple and the probe logic is reusable / independently
testable.

## Tasks
- [x] Add `probe_first_event` helper function with generic `T, E: Into<LanguageModelCompletionError>` bounds
- [x] Wire `probe_first_event` into the `stream_completion` closure
- [x] Wire `probe_first_event` into the `stream_response` closure
- [x] Add `futures::stream` import if not already present (used full path `futures::stream::once` instead)
- [x] Add test: first event Ok → returned stream yields it first
- [x] Add test: first event Err (backoff-worthy) → returns Err for retry
- [x] Add test: first event Err (not backoff-worthy) → returns Err
- [x] Add test: empty stream (None) → returns Ok with empty stream
- [x] Add test: probe preserves subsequent events after the first
- [x] `cargo check -p language_models`
- [x] `./script/clippy -p language_models` (incl. cargo-machete)
- [x] `cargo test -p language_models --lib` (68/68 pass)
- [x] Fix flaky `retry_stream_rotates_on_backoff_worthy_failure` test (latent bug exposed by timing change)
- [x] Verify downstream consumers: `cargo check -p edit_prediction -p edit_prediction_cli -p language_models_cloud`

## Validation
- `retry_stream` tests still pass unchanged (the helper is in the closure, not in `retry_stream`).
- A stream whose first event is a rate-limit error now triggers rotation instead of propagating the error.
- A stream whose first event is a valid token preserves that token (no data loss).
- An empty stream is treated as success (no spurious retry).

## Non-goals (still)
- **Mid-stream retry after partial output**: once the consumer has seen at least one event, a later error still terminates the request. Full mid-stream retry requires partial-output reconciliation (dedup of tokens, merging tool-call state), which remains out of scope.
- **Persistence of backoff state across restarts**: still in-memory only.

## Key Files
- `zed/crates/language_models/src/provider/open_ai_compatible.rs` — `probe_first_event`, `stream_completion` closure, `stream_response` closure, tests.

## TL;DR
Probe the first event of each candidate stream before returning it from the
retry closure. If the first event is a backoff-worthy error, the retry loop
rotates to the next key — same as a setup-phase error. The first event is
re-prepended so the consumer sees the full stream. `retry_stream` is
unchanged; all logic lives in a new `probe_first_event` helper called from
the `do_attempt` closures.
