# Transport failures poison GLM key backoff as if they were quota verdicts

status: fixed 2026-09-22 — three rounds: classifier split (26bb155267), upstream-hint
provenance (fefcf6bfd7), fail-open verdict-awareness; no open follow-ups

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

- [x] The fail-open fallback picked the *earliest-expiring* backed-off slot
      deterministically, so when every slot was down all concurrent threads piled
      onto the same key. Fixed in the third round below — and the provenance flag
      added in round two turned this from a load-spreading tweak into a
      wasted-request fix.


## Follow-up round: the inverse failure — limited keys rendering as healthy

Reported right after the first fix shipped: "when new thread it appear all
usable but i dont think so, some should hot limit rn". Correct. Evidence from
`Zed.log`:

```
11:04:06  reset_key_session probe: slot=Tertiary result=RateLimit { retry_after: Some(28905.334231s) }
12:08:39  GLM.json tertiary: consecutive_failures 214, backoff_total_secs 3600.0
12:20:27  migrating persisted key health from schema_version 3 to 4
12:20:27  clearing pre-v4 key backoff
```

Tertiary held a **genuine ~8h upstream reset hint** (reset ≈ 19:06 local). Three
separate defects destroyed it:

1. **A local guess could shorten an upstream-attested window.** Between 11:04
   and 12:08 a transport failure ran `record_failure`, which unconditionally
   assigned `backoff_until = now + compute_backoff(n)` — overwriting the 8h hint
   with the 1h local cap. The first fix made this *worse*: transport failures now
   compute a 60s window, so the same path would have cut 8h down to a minute.
   **Regression introduced by 26bb155267.**
2. **The v3→v4 migration was too blunt**, dropping every window including
   attributable ones.
3. **Probes ignored hintless 429s.** `KeyProbeResult::RateLimit { retry_after:
   None }` fell into `_ => {}` in both `reset_key_session` and the settings-page
   Check button. The probe *observed a 429* — the strongest possible evidence the
   key is limited — and recorded nothing, leaving the slot rendering healthy.
   Only the duration was ambiguous, not the fact of the limit.

Also latent: `reset_key_session` cleared backoff on any probe `Ok`, but a probe
is a 1-token ping — it can slip through a quota a real 400k-token turn would
trip, so it was silently overturning real limits.

### Fix

- `KeyHealth::backoff_from_upstream_hint` records provenance (persisted).
- `apply_local_backoff` — all locally-computed schedules (`record_failure`,
  `record_transport_failure`, hintless `record_rate_limit`) now **extend only,
  never shorten**. A fresh upstream hint still wins outright in both directions:
  if the quota came back early, the shorter hint applies.
- Probes record hintless 429s via the local schedule instead of dropping them.
- A probe-success path that was later folded away — see round four; the
  upstream-hint guard it carried was wrong and the wrapper went with it.
- Migration discriminator: a pre-v4 window **longer than `BACKOFF_MAX`** can only
  have come from an upstream hint, since the local exponential is clamped to that
  cap. Those survive and are tagged; windows within the cap are dropped.

6 more tests (13 total for this issue), including
`local_backoff_never_shortens_an_upstream_window` built from the exact observed
values (28905s hint vs. the 1h clobber).

### Known data loss

Tertiary's real 19:06 reset is unrecoverable — it was clobbered to 3600.0 by the
old binary at ~12:08, *before* the migration ran, so there is no longer a
>`BACKOFF_MAX` window for the new rule to rescue. The next real request to that
slot will 429 and re-record the hint correctly. Cost is one failed request, not
an ongoing condition.


## Third round: fail-open was spending requests on keys already known to be closed

With `backoff_from_upstream_hint` in place, the fail-open fallback could finally
tell two very different situations apart, and it was treating them identically:

* A **speculative** window is a local guess — a transport blip or an exponential
  estimate. The key may well answer right now.
* An **upstream-attested** window is the server stating when the quota resets.
  A request sent before that instant is a guaranteed 429: it costs latency and a
  round trip to learn something already known.

The old rule was `min_by_key(backoff_until)` across both classes, which picks by
*remaining time* — a quantity that says nothing about which key will answer. In
the shape actually observed on this machine (K3 attested closed until 19:06,
K1/K2/K4 carrying short local windows) it could route every fail-open request to
the one key guaranteed to refuse it.

### Fix

`select_from_candidates` step 5 now partitions the enabled backed-off slots:

1. If any window is **speculative**, pick among those, advancing the same
   round-robin cursor step 4 uses. This both avoids the guaranteed-429 slots and
   spreads concurrent agents instead of converging them on one key — the
   original follow-up.
2. Only when **every** window is attested does earliest-expiring apply: every
   request will be refused, so the slot closest to its stated reset is the best
   available guess.

Disabled slots still never qualify.

4 new tests (17 total for this issue). Two are built to fail against the old
rule: `fail_open_prefers_speculative_backoff_over_attested` gives the attested
slots the *shortest* windows (so `min_by_key` would have chosen one of them), and
`fail_open_spreads_concurrent_picks_across_speculative_slots` asserts 12 picks
land 3-3-3-3 rather than 12-0-0-0.

`select_from_candidates_falls_open_when_all_backed_off` was retargeted: its
"soonest-expiring must win" assertion encoded the behavior being replaced, and
that property now lives in `fail_open_takes_earliest_reset_when_all_are_attested`
where it still holds.


## Fourth round: correcting an over-reach from round two

Round two gave `record_probe_success` a guard: a 1-token probe could clear a
locally-guessed window but not an upstream-hinted one, on the theory that a
minimal request might slip through a quota a full turn would trip. Reviewing the
selection path afterwards showed that guard was wrong on both counts.

**It was never evidence-backed.** The one probe round in the retained logs is
the opposite result — K3, genuinely limited, correctly returned `RateLimit`; the
three healthy keys returned `Ok`. No probe ever falsely reported `Ok`. The
guard was added on a hypothetical.

**It silently broke a documented invariant.** `parse_body_retry_hint` reads a
Z.AI timestamp that carries **no timezone marker** and assumes local time. Its
doc comment states the correctness argument outright:

> A mis-interpretation self-heals: the backoff clears on the first successful
> request, the settings-page Check probe, or new-thread key probing.

Round two disabled the third of those three. Z.AI is UTC+8 and this machine is
UTC+7, so a one-hour overestimate is a live possibility — and with the guard in
place there was no longer anything to correct it.

Two further points settle the direction:

- The limits this endpoint emits (1308 five-hour, 1310 weekly/monthly) are
  *usage quotas*. While one is active it refuses every request, a 1-token ping
  included. A probe returning `Ok` is therefore direct evidence from the
  endpoint that the quota is open — not weaker evidence than a real turn.
- The cost asymmetry is decisive. Clearing too eagerly costs **one request**,
  which immediately re-records the correct hint. Clearing too reluctantly
  strands a working key for **hours**.

With the guard gone, `record_probe_success` was identical to `record_success`
apart from an unused return value, so it was folded away and its reasoning moved
onto `record_success`. The provenance flag itself stays — it is well-supported where it was actually derived from evidence
(`apply_local_backoff`'s never-shorten rule, from the observed 28905s → 3600s
clobber) and in fail-open ranking.

### Open question, deliberately not guessed

Whether Z.AI stamps that reset timestamp in UTC+8, UTC, or the account's
timezone is **unresolved** — the raw 429 bodies are not in the retained logs, so
there is no evidence here to settle it. Left as-is rather than guessed at: the
parse is self-healing again, so a wrong timezone costs one probe cycle rather
than a stranded key. Worth confirming against a captured 429 body the next time
one appears, and worth an explicit timezone if Z.AI ever documents one.
