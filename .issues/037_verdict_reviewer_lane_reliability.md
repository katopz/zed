# 037 — verdict reviewer lane reliability (abort+error 4x over gate)

Status: OPEN — filed 2026-11-11 (amo idle lane) from the bench-001 verdict session (`.benchmarks/001`, 2026-11-11 section): the reviewer lane's failure rate, not the `verdict_ping_pong` flag default, is where the verdict lane loses its value.

## Evidence

Bench 001's 2026-11-11 re-run (`script/verdict_scorer.py`, 118 chains): 9 aborted negotiations + 8 FAILED (all-error) chains out of ~41 verdict attempts ≈ **41% abort+error**, over the < 10% gate by 4x. Observed causes: `claude_code` backend 5-hour rate-limit windows (a verdict attempt during the window returns `rate_limit` until reset), reviewer thread aborts mid-negotiation, and the stale-registration class fixed earlier in `19556c5841` ("connection store is gone"). FAILED chains also pollute the bench cohorts (87.5% of them carry human corrections — an error-class signature, excluded from `matched_bins` for that reason).

## Tasks

- [ ] Rate-limit-aware fallback in the reviewer lane: on `rate_limit` from a backend, fail over to the configured secondary reviewer (`agent.verdict_secondary_model`) or re-park the request with the reset time instead of burning an abort
- [ ] Retry/backoff for transient reviewer failures (one bounded retry with the reset-time delay before the attempt counts as FAILED)
- [ ] Count FAILED attempts separately from honest aborts in `script/verdict_scorer.py` (a 5-hour-window rate_limit is not a negotiation abort) so the gate reads the true failure rate
- [ ] Gate: abort+error rate < 10% at the next bench-001 re-run (checked at every +20 ON chains alongside `matched_bins`); if still over, the tool is unreliable enough that default-on is questionable for that reason alone

## Refs

- `.benchmarks/001_verdict_ping_pong_goat.md` (2026-11-11 section — the verdict session + matched_bins)
- `crates/agent_mail/src/verify.rs` producer lane is unrelated; this lane is `request_verdict` (claude-sub-agent-verdict / claude_code backends)
