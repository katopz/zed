# OpenAI Compatible: Key Health UI (truncated, clear, check) + rate-limit rotation guard

## Goal
Three UX additions to the per-key cards in the OpenAI-compatible provider
configuration view, plus one rotation-behavior fix:

1. **Truncated key preview** — each configured card shows `foo...bar` (first 3
   + last 3 chars of the secret) so the user can tell which key is which.
2. **Manual "Clear backoff" button per slot** — escape hatch for when the
   upstream quota resets before the 5h backoff window elapses.
3. **"Check" button per slot** — probes the API with that one key and shows
   the outcome on the button face: `check(ok)` / `check(hit)` / `check(err)`.
4. **Rate-limit rotation guard (the bug fix)** — when a key fails with
   `RateLimitExceeded`, stop the intra-request rotation immediately instead of
   burning key2/key3 in the same call. Rate limits are frequently account/org
   wide (multiple keys under one quota), so rotating just poisons the whole
   pool in one request. Only the key that actually hit the limit is backed off;
   the next request picks a healthy key.

## Why
- Truncated preview: with 3 keys configured the cards are visually identical.
  Showing `sk-...x9F` lets the user map a card to a key without exposing it.
- Manual clear: backoff max is 5h. If the upstream limit resets earlier (e.g.
  per-minute tier), the user is stuck waiting. The button gives a one-click
  escape hatch and also overwrites the persisted state so a restart doesn't
  resurrect it.
- Check button: lets the user verify a specific key works *before* relying on
  it, and see *why* it fails (rate limit vs auth vs other). Status renders on
  the button itself so no extra row is needed.
- Rotation guard: the current `retry_stream` rotates to key2/key3 on *any*
  backoff-worthy error including `RateLimitExceeded`. When the 3 keys share an
  account quota (very common for OpenAI-compatible setups where a user buys
  multiple keys under one org), key1's rate limit means key2/key3 will rate
  limit too — and all three end up backed off after a single user request,
  leaving no healthy key for the *next* request. The per-slot *attribution* is
  already correct (`record_key_failure(key_health, slot, &err)` only poisons
  `slot`); the bug is that the retry loop *tries* the other slots at all on a
  rate-limit error. Stopping rotation on `RateLimitExceeded` means only the one
  key that hit the limit is penalized; rotation is preserved for genuinely
  per-key transient errors (`ServerOverloaded`, `HttpSend`, `UpstreamProviderError`,
  etc.) where a different key genuinely might help.

### Design decision: why RateLimitExceeded specifically (not all backoff-worthy)
Per `.plans/003` the intra-request retry was added so a flaky key A doesn't kill
a request when healthy key B exists — correct for *per-key* failures. But
`RateLimitExceeded` is the one error class that is *commonly* account-wide:
multiple keys under one org share one quota, so a 429 on key1 is a strong
predictor of a 429 on key2. Rotating in that case is strictly harmful (burns
the pool). Other backoff-worthy errors (`ServerOverloaded`, `HttpSend`,
`StreamEndedUnexpectedly`, `UpstreamProviderError`) are more likely per-key or
per-request, so rotation still makes sense there. This keeps the
per-key-reliability win from plan 003 for the errors where it matters, while
fixing the account-wide-quota footgun the user hit.

## Design

### 1. Truncated key preview
- New helper `truncate_key_preview(key: &str) -> String`:
  - length <= 8 → return as-is (too short to be distinctive, and showing
    `foo...bar` for an 8-char key would reveal the whole thing).
  - else → `format!("{}...{}", &first3, &last3)` using char-safe slicing
    (`.chars().take(3)` / `.chars().rev().take(3)`), never byte slicing.
- `State::key_preview(slot) -> Option<String>` — reads the key for the slot
  (reuses the same url→key lookup as `gather_candidates`) and maps through
  `truncate_key_preview`. Returns `None` if the slot has no key.
- The card label changes from `"Primary API key configured for {api_url}"` to
  `"Primary: {preview} · {api_url}"` (preview absent for env-var keys, since
  we can't read the env value cheaply/securely — keep the env-var label there).

### 2. Manual clear backoff
- `State::clear_slot_backoff(slot, cx)` — under the mutex, set that slot's
  `KeyHealth` to default (failures=0, backoff_until=None), then
  `schedule_persist_key_health(cx)`. Mirrors `reset_key_health` but per-slot.
- `ConfigurationView::clear_backoff(slot, window, cx)` listener wrapper that
  calls into `State`.
- Button "Clear" appears on each configured card **only when
  `status.is_backed_off`** (no point clearing a healthy slot). Sits next to the
  existing Reset button. Tooltip: `"Clear this key's backoff and re-qualify it
  immediately. Use when the upstream quota has already reset."`

### 3. Check button
- `State::probe_key(slot) -> Task<KeyProbeResult>` — builds a minimal
  `open_ai::Request` (model = first `available_models` entry, messages = one
  user "ping", `max_completion_tokens = Some(1)`, `stream = false`), sends via
  the existing `http_client` + `api_url` + `extra_headers` + that slot's key on
  a background task, classifies the result.
- `KeyProbeResult { Ok, RateLimit, Err(SharedString) }` stored per-slot in
  `ConfigurationView` as `probe_results: [Option<KeyProbeResult>; 3]`. Cleared
  to `None` on the next render of the input path (when the slot has no key) and
  reset to `None` when the user edits/saves a new key for the slot.
- Button label switches:
  - idle → `"Check"`
  - in-flight → `"Check…"` (disabled)
  - last result ok → `"check(ok)"` (success color)
  - last result rate-limit → `"check(hit)"` (warning color)
  - last result other err → `"check(err)"` (error color) + tooltip with message
- Reuses the open_ai completion path (not the responses path) so the probe is a
  single concrete endpoint; if the provider only supports the responses API the
  probe may 404 and report `err` — acceptable for a manual sanity check, and the
  tooltip shows the message so the user can tell.

### 4. Rate-limit rotation guard
- In `retry_stream`, after recording the failure, check if the error is
  `RateLimitExceeded` specifically. If so, return `Err(err)` immediately — do
  not rotate to the next candidate. The slot is already poisoned by
  `record_key_failure`; we just stop burning siblings.
- Other backoff-worthy errors keep rotating (unchanged).
- New free fn `is_rate_limit(err) -> bool` in `health.rs` so the guard is
  testable in isolation.
- Update the `retry_stream_rotates_on_backoff_worthy_failure` test (it uses a
  rate-limit error) to use a non-rate-limit backoff-worthy error
  (`ServerOverloaded`), and add a new test
  `retry_stream_stops_on_rate_limit_does_not_burn_siblings` that verifies only
  the rate-limited slot is poisoned and exactly one attempt is made.

## Tasks
- [x] Add `truncate_key_preview` + `State::key_preview`
- [x] Render truncated preview in each configured card label
- [x] Add `State::clear_slot_backoff`
- [x] Add "Clear" button to each card (visible only when backed off)
- [x] Add `KeyProbeResult` + `State::probe_key`
- [x] Add "Check" button with status label + per-slot result state
- [x] Add `is_rate_limit` + rotation guard in `retry_stream`
- [x] Update rotation tests; add rate-limit-guard test
- [x] `cargo check -p language_models`
- [x] `./script/clippy -p language_models`
- [x] `cargo test -p language_models --lib`

## Validation
- Truncated preview is char-safe (CJK / emoji keys won't panic).
- Clear button only shows when slot is backed off; clicking clears in-memory
  state + schedules persist.
- Check button shows idle/in-flight/ok/hit/err states correctly; result is
  per-slot and isolated.
- Rate-limit error stops rotation after exactly one attempt; only the tried
  slot is poisoned. Non-rate-limit backoff-worthy errors still rotate.
- All existing tests pass (with the one rotation test updated to a
  non-rate-limit error).

## Key Files
- `zed/crates/language_models/src/provider/open_ai_compatible.rs`
- `zed/crates/language_models/src/provider/open_ai_compatible/health.rs`

## TL;DR
Three card-level UX additions (truncated key preview, manual clear-backoff
button, check-with-status button) and one rotation fix: stop burning sibling
keys on `RateLimitExceeded` (commonly account-wide), so key1's rate limit
doesn't drag key2/key3 into backoff in the same request.
