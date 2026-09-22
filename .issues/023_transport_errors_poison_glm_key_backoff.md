# Transport failures poison GLM key backoff as if they were quota verdicts

status: fixed 2026-09-22 — classifier split + v3→v4 migration; self-heals on next launch

## Symptom

K1 rendered "51m remain" in the footer chip while the Z.AI dashboard reported
the key at **51% used** — i.e. fully available. All four GLM slots were parked
at the 1h cap simultaneously, so rotation had no healthy candidate and every
request fell through the fail-open path. Reported as affecting "all GLM api",
with the correct intuition that backoff *should* be judged from the error
response.

## Evidence

Persisted state at `~/Library/Application Support/Zed/openai_compatible_backoff/GLM.json`
(schema v3):

```json
"primary":   {"consecutive_failures":27,  "backoff_remaining_secs":3422.1, "backoff_total_secs":3424.3}
"secondary": {"consecutive_failures":27,  "backoff_remaining_secs":3597.8, "backoff_total_secs":3600.0}
"tertiary":  {"consecutive_failures":205, "backoff_remaining_secs":3597.9, "backoff_total_secs":3600.0}
"quaternary":{"consecutive_failures":27,  "backoff_remaining_secs":3597.9, "backoff_total_secs":3600.0}
```

`Zed.log` + `Zed.log.old` contain **zero real 429s** (every `429` match is a
plan filename; every `rate limit` match is auto_prompt's own speculative
wording). What they do contain, 15×:

```
ERROR [agent::thread] Turn execution failed: error sending HTTP request to GLM API
```

That string is `LanguageModelCompletionError::HttpSend`
(`language_model_core.rs:186`) — a connect/TLS/DNS failure. **The request never
reached the upstream, so no error response existed to judge.**

## Root cause

Three defects compounding:

1. **`is_backoff_worthy` returned `true` for `HttpSend`** (and
   `StreamEndedUnexpectedly`, `ApiReadResponseError`, `Other(_)`). A verdict-less
   transport failure was recorded via `record_failure`, bumping
   `consecutive_failures` onto the same hour-scale exponential schedule used for
   real quota exhaustion. The comment justified this as "intentionally
   permissive… upstream error labels are unreliable" — but an unreliable *label*
   argues for a short penalty, not for treating an *unlabeled* failure as
   confirmed quota exhaustion.

2. **One blip poisoned the whole pool.** `HttpSend` is backoff-worthy but not
   `is_rate_limit`, so `retry_stream` rotated through every remaining slot,
   poisoning each in turn. A single network interruption cost all four keys —
   hence identical `consecutive_failures: 27` across three slots.

3. **The counter never decays** — only a success or a manual clear resets it. At
   27 failures the exponential is long saturated, so every subsequent blip
   re-armed the full 1h window.

Bonus defect found while reading `compute_backoff`: jitter was applied and *then*
clamped (`candidate.mul_f64(jitter).min(BACKOFF_MAX)`), so every draw ≥ 1.0
collapsed to exactly `BACKOFF_MAX`. That is why three slots persisted
`backoff_total_secs: 3600.0` to the decimal — the anti-thundering-herd jitter was
dead at exactly the point it was needed, and saturated keys unblocked in lockstep.

## Fix

`crates/language_models/src/provider/open_ai_compatible/health.rs`

- Replaced the `is_backoff_worthy` boolean with a three-way `ErrorVerdict`
  (`KeyFault` / `Transport` / `Benign`) and `classify_error`. The axis is
  *did the upstream answer*: 429/401/403/402/5xx are `KeyFault`; `HttpSend`,
  `StreamEndedUnexpectedly`, `ApiReadResponseError` and `Other(_)` are
  `Transport`; request-shaped errors stay `Benign`.
- Added `KeyHealth::transport_failures`, a counter that never feeds
  `compute_backoff`, plus `record_transport_failure` on
  `compute_transport_backoff` (base 5s, **cap 60s**). Slots are still briefly
  skipped — they can point at different hosts, so rotating is worth one attempt
  — but the worst case is "retry in a minute", not "gone for an hour".
- `record_success` clears both counters; `record_rate_limit` zeroes the
  transport streak (an upstream verdict supersedes it).
- Fixed the jitter clamp: `BACKOFF_CEILING = 2400s` is pre-clamped so
  `ceiling × max_jitter == BACKOFF_MAX`, restoring a real `[20m, 60m)` spread at
  saturation instead of a lockstep pile-up on 3600.0.
- Schema v3 → v4 with a migration that **drops pre-v4 counters and windows**:
  they conflate transport failures with quota verdicts, so the backoff they
  describe is unattributable. `enabled` is a user decision and survives. This is
  what unblocks the already-stuck state — no manual file surgery needed, it
  clears on next launch.

7 new tests, including `transport_errors_do_not_escalate_onto_the_quota_schedule`
(30 consecutive `HttpSend` must leave `consecutive_failures == 0` and stay under
the transport cap) and `saturated_backoff_keeps_its_jitter_spread`.

## Follow-ups

- [ ] The fail-open fallback picks the *earliest-expiring* backed-off slot
      deterministically, ignoring the priority/sticky policy — so when every slot
      is down, all concurrent threads pile onto the same key. Lower priority now
      that transport blips no longer drive the pool to that state.
