# OpenAI Compatible: Triple API Key with Backoff Rotation

## Goal
Extend the existing dual-key load balancing to support THREE API keys, and add
per-key exponential backoff so a flaky/rate-limited key is automatically
removed from rotation temporarily. Backoff auto-clears after at most 5 hours
per key.

## Why
User reports the upstream error labels are unreliable: "sometime it's error,
sometime it api limit and sometime i got API limit error but actually it's
not". So we cannot trust error semantics to make hard "ban" decisions. Use a
soft backoff strategy: any backoff-worthy error pushes the key out of
rotation for a bounded duration, successes immediately re-qualify it.

## Design

### Slots & identifiers
| Slot      | Keychain ID              | Env var                  |
|-----------|--------------------------|--------------------------|
| Primary   | `{api_url}`              | `{PROVIDER}_API_KEY`     |
| Secondary | `{api_url}#secondary`    | `{PROVIDER}_API_KEY_2`   |
| Tertiary  | `{api_url}#tertiary`     | `{PROVIDER}_API_KEY_3`   |

### Key selection (`State::select_key`)
1. Gather candidates: each present key + its `KeyHealth`.
2. Filter out keys whose `backoff_until > now`.
3. If healthy candidates exist, pick uniformly at random.
4. If ALL present keys are in backoff, pick the one with the earliest
   `backoff_until` (better than returning `NoApiKey` — let the next request
   try the soonest-available key).
5. If no key is present at all, return `None` → caller emits `NoApiKey`.

### Backoff computation
- Base: 30 seconds
- Multiplier: 2x per consecutive failure
- Cap: 5 hours (`18_000s`)
- Jitter: multiply final value by random factor in `[0.5, 1.5)` to avoid
  thundering-herd between keys failing simultaneously.

Formula: `min(30s * 2^(failures-1), 5h) * jitter`.

After 5 hours since the last failure, `backoff_until` is naturally in the
past, so the key re-enters rotation without any explicit "clear" code path.

### Outcome reporting
- On request success → `record_key_success(slot)`: reset
  `consecutive_failures = 0`, clear `backoff_until`.
- On backoff-worthy error → `record_key_failure(slot)`: increment
  `consecutive_failures`, recompute `backoff_until = now + backoff`.
- On non-backoff-worthy error (client-side bug: PromptTooLarge,
  BadRequestFormat, etc.) → no health change (will fail identically on
  every key, no point poisoning slots).

### Backoff-worthy errors
`RateLimitExceeded`, `ServerOverloaded`, `ApiInternalServerError`,
`UpstreamProviderError` (any status — could be transient upstream noise),
`StreamEndedUnexpectedly`, `ApiReadResponseError`, `HttpSend`,
`AuthenticationError`, `PermissionError`, `Other`.

Not backoff-worthy: `PromptTooLarge`, `NoApiKey`, `BadRequestFormat`,
`ApiEndpointNotFound`, `SerializeRequest`, `BuildRequestBody`,
`DeserializeResponse`, `HttpResponseError` (4xx non-auth, request-side
issue that will recur on every key).

Note: `HttpResponseError` is the catch-all for HTTP statuses not matched by
the typed variants above, so by definition it covers things like 400/404
that would fail on every key — excluding it from backoff is intentional.

## Tasks

- [x] Add `KeySlot` enum and `KeyHealth` struct (consecutive_failures + backoff_until)
- [x] Add `api_key_state_3` field to `State` + `key_health` tracker (3 slots)
- [x] Add `tertiary_key_url` helper (`{api_url}#tertiary`)
- [x] Update `State::is_authenticated` to consider all three keys
- [x] Add `State::set_api_key_3`
- [x] Update `State::authenticate` to load all three keys
- [x] Replace `available_keys()` with `select_key(health, now)` returning `(Arc<str>, KeySlot)`
- [x] Add `KeyHealthTracker::record_success(slot)` and `record_failure(slot, now)`
- [x] Add free function `compute_backoff(failures) -> Duration`
- [x] Update URL-change observer to handle all three slots
- [x] Update `reset_credentials` to clear all three keys
- [x] Update `OpenAiCompatibleLanguageModelProvider::new` to init env var `_API_KEY_3`
- [x] Update `stream_completion` to use `select_key` + report outcome
- [x] Update `stream_response` to use `select_key` + report outcome
- [x] Add `is_backoff_worthy(err)` free function
- [x] Store `key_health` as `Arc<std::sync::Mutex<KeyHealthTracker>>` (AsyncApp is !Send, can't enter background rate-limited closure)
- [x] Add tertiary section to `ConfigurationView` (input + configured/reset states)
- [x] Add unit tests for `compute_backoff`, `KeyHealth`, `KeyHealthTracker`, `is_backoff_worthy`, `select_key`
- [x] `cargo check -p language_models`
- [x] `./script/clippy -p language_models` (incl. cargo-machete)

## Validation
- Backoff never exceeds 5h regardless of failure count.
- 5h after last failure, key is automatically selectable again.
- All-three-backed-off case returns the soonest-available key, not `NoApiKey`.
- `cargo check` and `./script/clippy` clean.

## Non-goals
- No automatic intra-request retry (try next key on same call). Next request
  picks a different healthy key — simpler, less surprise.
- No persistence of backoff state across restarts. Restart = fresh slate.
  (Lives in `Entity<State>`, in-memory only.)
