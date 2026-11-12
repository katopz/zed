# 037 — verdict reviewer lane reliability (abort+error 4x over gate)

Status: OPEN — filed 2026-11-11 (amo idle lane) from the bench-001 verdict session (`.benchmarks/001`, 2026-11-11 section): the reviewer lane's failure rate, not the `verdict_ping_pong` flag default, is where the verdict lane loses its value.
**Lease closed:** amo-zed-037acct@amo (epoch 1794477600) — scoped to task 3, which landed 2026-11-11 (the scorer `reliability` block + gate line); reshaped 2026-11-12 when tasks 1/2 were re-claimed.
**Claimed:** amo-zed-037fb@amo, 1794445702, expires 2026-11-14T01:08:22+00:00
Scoped to tasks 1+2 (rate-limit failover/re-park + bounded transient retry); task 4 (gate check) stays with the next bench-001 re-run at +20 ON chains.

## Evidence

Bench 001's 2026-11-11 re-run (`script/verdict_scorer.py`, 118 chains): 9 aborted negotiations + 8 FAILED (all-error) chains out of ~41 verdict attempts ≈ **41% abort+error**, over the < 10% gate by 4x. Observed causes: `claude_code` backend 5-hour rate-limit windows (a verdict attempt during the window returns `rate_limit` until reset), reviewer thread aborts mid-negotiation, and the stale-registration class fixed earlier in `19556c5841` ("connection store is gone"). FAILED chains also pollute the bench cohorts (87.5% of them carry human corrections — an error-class signature, excluded from `matched_bins` for that reason).

## Tasks

- [x] Rate-limit-aware fallback in the reviewer lane: on `rate_limit` from a backend, fail over to the configured secondary reviewer (`agent.verdict_secondary_model`) or re-park the request with the reset time instead of burning an abort — DONE 2026-11-12 (amo-zed-037fb): `classify_reviewer_failure` (rate_limit / transient / other, from the turn error + the reviewer thread's own `last_api_error` so an "empty reply" wrapper can't hide a rate-limit JSON); on RateLimit the orchestrator fails over to `verdict_secondary_model` for the same round (fresh native session, round budget carried over via `verdict::register_with_offset`, failed reviewer session released immediately) or, without a secondary, returns a re-park error carrying the backend's verbatim reset hint ("resets 1:20am …") with the resume path; telemetry gained `Verdict Reviewer Failover` + `Verdict Reviewer Failed` (class-tagged) events
- [x] Retry/backoff for transient reviewer failures (one bounded retry with the reset-time delay before the attempt counts as FAILED) — DONE 2026-11-12 (amo-zed-037fb): external lane only (the native thread already retries internally with retry-after capped at 2 min): one bounded retry on transient spawn blips and transient turn failures (timeout / connection lost / overloaded), delay = parsed "try again in Ns/Nm" hint clamped to `MAX_RETRY_DELAY` (2 min) else a 5s default backoff; wall-clock reset hints (multi-hour windows) are deliberately never slept on — those go through the failover/re-park path above; a second failure surfaces as FAILED (class-tagged telemetry)
- [x] Count FAILED attempts separately from honest aborts in `script/verdict_scorer.py` (a 5-hour-window rate_limit is not a negotiation abort) so the gate reads the true failure rate — DONE 2026-11-11 (amo-zed-037acct): the report gained a `reliability` block (attempts = ON + FAILED chains, aborted negotiations and all-error attempts counted as distinct classes) and the run prints the gate line; first reading **53.1% abort+error (9 + 8 / 32 attempts) — OVER the < 10% gate** (stricter than the bench's loose ~41% over ~41 attempts: chains are the countable unit, one chain can carry several aborts)
- [ ] Gate: abort+error rate < 10% at the next bench-001 re-run (checked at every +20 ON chains alongside `matched_bins`); if still over, the tool is unreliable enough that default-on is questionable for that reason alone

## Refs

- `.benchmarks/001_verdict_ping_pong_goat.md` (2026-11-11 section — the verdict session + matched_bins)
- `crates/agent_mail/src/verify.rs` producer lane is unrelated; this lane is `request_verdict` (claude-sub-agent-verdict / claude_code backends)
