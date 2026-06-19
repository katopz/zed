# OpenAI Compatible: Intra-Request Key Rotation (Retry on Failure)

## Goal
When a request fails on key A with a backoff-worthy error, retry the SAME user
request on key B (and C if needed) instead of failing the user's request
entirely. This is the natural follow-up to the per-key backoff work in plan 002:
backoff prevents FUTURE requests from picking a poisoned key, but the CURRENT
request still fails unless we rotate within the call.

## Why
The user's core complaint — "sometime it's error, sometime it api limit and
sometime i got API limit error but actually it's not" — means false-positive
rate-limit errors will routinely kill user requests if we don't rotate. With
multiple keys configured, there's no reason a flaky key A should prevent a
request from succeeding on healthy key B. The retry is bounded to
`candidates.len()` attempts, so worst-case latency is one full rotation per
request — strictly better than failing immediately.

## Design

### Where the retry lives
Inside `request_limiter.stream(...)` — the rate-limit semaphore is held across
all retry attempts for a single user request, so we don't release and re-acquire
on each attempt. The retry loop runs in the background-executor closure where
`Arc<Mutex<KeyHealthTracker>>` is already accessible (the `!Send` `AsyncApp`
constraint from plan 002 still applies).

### `retry_stream` helper
Generic over the stream type `S`. Takes:
- `candidates: &[(Arc<str>, KeySlot)]` — snapshot taken before entering the
  background closure.
- `key_health: &Arc<Mutex<KeyHealthTracker>>` — shared with `State`.
- `provider: LanguageModelProviderName` — for the `NoApiKey` fallback error.
- `do_attempt: impl FnMut(Arc<str>) -> BoxFuture<'static, Result<S, LanguageModelCompletionError>>`
  — caller-provided closure that performs one HTTP attempt with the chosen key.

Loop:
1. `select_from_candidates(&remaining, &snapshot, now)` — pick a healthy key,
   or fail-open to the soonest-expiring backed-off key.
2. Call `do_attempt(api_key).await`.
3. On `Ok(stream)` → `record_key_success(slot)`, return `Ok(stream)`.
4. On `Err(err)`:
   - Always `record_key_failure(slot, &err)` (no-op if not backoff-worthy).
   - If NOT backoff-worthy → return `Err(err)` immediately (would fail on every
     key, don't waste the user's time).
   - If backoff-worthy → `remaining.retain(|(_, s)| *s != slot)`, save
     `last_error`, continue loop.
5. If loop exhausts (no remaining candidates) → return `last_error`, or
   `NoApiKey` if never entered the loop.

### Bounded retries
`max_attempts = candidates.len()` at loop entry. Each iteration removes the
attempted slot from `remaining`, so no slot is tried twice within one request.
Worst case = 3 sequential HTTP attempts (one per configured key).

### Why the closure returns `BoxFuture<'static, ...>`
The open_ai `stream_completion` / `stream_response` functions are `async fn`
that borrow from their arguments (`&api_url`, `&api_key`, etc.). Those borrows
are tied to the async function's stack frame and can't escape a `FnMut` closure
body. So the closure clones `http_client`, `api_url`, `extra_headers`,
`provider_name`, and `request` into each attempt's owned `async move` block,
then `Box::pin`s it. The cloning is cheap (all `Arc` or small structs except
the request, which only clones on the retry path — the success path returns
after one attempt).

### `Clone` derives added to `open_ai` crate
To enable per-attempt request cloning:
- `open_ai::Request`, `open_ai::StreamOptions`, `open_ai::ToolChoice`
- `responses::Request`, `responses::ToolDefinition`, `responses::ReasoningConfig`,
  `responses::ResponseInputItem`, `responses::ResponseMessageItem`,
  `responses::ResponseFunctionCallItem`, `responses::ResponseFunctionCallOutputItem`,
  `responses::ResponseFunctionCallOutputContent`

All are pure serializable data; adding `Clone` is non-breaking.

### Key selection refactored
- `State::gather_candidates(&self) -> Vec<(Arc<str>, KeySlot)>` — collects
  configured keys (replaces inlined logic in old `select_key`).
- `select_from_candidates(candidates, health, now) -> Option<(Arc<str>, KeySlot)>`
  — pure function, callable from the background closure without `&self`.
- Removed `State::select_key` — was only used by tests; tests now call
  `gather_candidates` + `select_from_candidates` directly (exercises the real
  production code path).
- Replaced generic `record_outcome<T>` with two focused helpers:
  `record_key_success(key_health, slot)` and
  `record_key_failure(key_health, slot, &err)` (the latter is a no-op for
  non-backoff-worthy errors).

## Tasks
- [x] Add `Clone` derives to `open_ai::Request`, `StreamOptions`, `ToolChoice`
- [x] Add `Clone` derives to `responses::Request`, `ToolDefinition`, `ReasoningConfig`, `ResponseInputItem`, `ResponseMessageItem`, `ResponseFunctionCallItem`, `ResponseFunctionCallOutputItem`, `ResponseFunctionCallOutputContent`
- [x] Verify `open_ai` crate compiles + tests pass with the new derives
- [x] Extract `State::gather_candidates()` from `select_key`
- [x] Extract `select_from_candidates()` as a free function
- [x] Replace `record_outcome<T>` with `record_key_success` + `record_key_failure`
- [x] Remove `State::select_key` (only test-used); update tests
- [x] Add `retry_stream<S>` helper with bounded rotation loop
- [x] Add `snapshot_health()` helper (clone-under-lock to avoid holding mutex across await)
- [x] Rewrite `stream_completion` to call `retry_stream` with a request-cloning closure
- [x] Rewrite `stream_response` to call `retry_stream` with a request-cloning closure
- [x] Add tests for `select_from_candidates` (no-keys, skips-backed-off, fail-open)
- [x] Add tests for `gather_candidates` (empty fixture)
- [x] Add tests for `retry_stream` (first-try success, rotates on backoff-worthy, aborts on non-backoff-worthy, exhausts candidates, empty candidates)
- [x] `cargo check -p language_models -p open_ai`
- [x] `./script/clippy -p language_models -p open_ai` (incl. cargo-machete)
- [x] `cargo test -p language_models --lib` — 57/57 pass
- [x] `cargo test -p open_ai --lib` — 39/39 pass
- [x] `cargo check -p edit_prediction -p edit_prediction_cli -p language_models_cloud` (downstream open_ai consumers)

## Validation
- Retry loop is bounded by `candidates.len()` — no infinite loops even when
  fail-open returns a backed-off key.
- Each candidate tried at most once per request (verified by
  `retry_stream_returns_last_error_when_all_candidates_fail`).
- Non-backoff-worthy errors abort immediately without poisoning slots (verified
  by `retry_stream_aborts_on_non_backoff_worthy_error`).
- Backoff-worthy failures poison the slot and rotate to next candidate (verified
  by `retry_stream_rotates_on_backoff_worthy_failure`).
- Success clears the slot's health (verified by same test).
- All existing backoff/compute tests from plan 002 still pass unchanged.

## Non-goals (still)
- No persistence of backoff state across restarts. Restart = fresh slate.
  (Lives in `Arc<Mutex<KeyHealthTracker>>`, in-memory only.)
- No UI badge showing "in backoff" status in the ConfigurationView. The
  configured-key card still shows green check regardless of backoff state.
- No retry of STREAMING errors mid-stream. The retry only covers the
  request-setup phase (the future returned by `stream_completion`/`stream_response`
  that resolves to a stream). Once the stream starts yielding tokens, a
  mid-stream error terminates the request as before. Mid-stream retry would
  require partial-output reconciliation and is out of scope.

## Key Files
- `zed/crates/language_models/src/provider/open_ai_compatible.rs` —
  `retry_stream`, `select_from_candidates`, `State::gather_candidates`,
  `snapshot_health`, `record_key_success`, `record_key_failure`,
  `OpenAiCompatibleLanguageModel::stream_completion`,
  `OpenAiCompatibleLanguageModel::stream_response`.
- `zed/crates/open_ai/src/open_ai.rs` — `Clone` derives on `Request`,
  `StreamOptions`, `ToolChoice`.
- `zed/crates/open_ai/src/responses.rs` — `Clone` derives on `Request` and
  related input/tool types.

## TL;DR
Added intra-request key rotation: if the chosen key fails with a backoff-worthy
error, automatically retry the same user request on the next healthy candidate,
up to `candidates.len()` attempts. Non-backoff-worthy errors abort immediately.
Bounded, no infinite loops, no extra latency on the success path. Required
adding `Clone` derives to 11 request-related types in the `open_ai` crate
(non-breaking). 9 new unit tests, all passing.
