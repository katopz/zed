# Bench 001 — Verdict ping-pong GOAT (interim)

**Status:** INTERIM — harness + baseline + FAILED-cohort instrumentation done; verdict-ON cohort still empty (0 chains). GOAT verdict deferred until >= 20 comparable tasks populate it (`.issues/016`).

## Setup

- Feature: `verdict_ping_pong` (default-on; user-invoked only — zero autonomous cost).
- Scorer: `script/verdict_scorer.py` (uv; reads `threads.db`, decompresses zstd thread blobs, groups continuation chains via agent_ui's `sidebar_threads`).
- Measured 2026-09-09 on the develop working tree at `a948a33dd2` + scorer fix.

## Baseline (verdict-OFF cohort)

| metric | value |
|---|---|
| threads scored | 20654 (0 parse failures) |
| chains | 16365 (4366 continuation links) |
| chains with summary | 7234 |
| post-hoc fix rate | **6.4%** (460 corrected) |

GOAT gate (unchanged from `.issues/016`): >= ~30% relative reduction vs this baseline at <= ~2x token overhead, abort rate < 10% of negotiations.

## Scorer fix (this bench)

All-error verdict attempts were silently counted as verdict-OFF, polluting the
baseline and hiding spawn-aborts (the metrics table in `.issues/016` wants
them counted). Chains whose `request_verdict` calls all carry `error` outputs
now form a separate **FAILED** cohort, excluded from both cohorts, reported
with their reviewers.

## FAILED cohort (spawn-aborts, claude_code)

| chains | origin |
|---|---|
| 1 | the user's original "Verdict with Claude" click → `{"error": "agent panel connection store is gone"}` — the stale-registration bug, fixed in `19556c5841` |
| 1 | this bench's live attempt (2026-09-09 19:5x local) → same error text |

Both attempts errored with the **pre-fix** message. The running editor build
predates `19556c5841` — live-verification of the stale-registration fix
requires an app rebuild + restart, after which new attempts should populate
the verdict-ON cohort (or count as real mid-negotiation aborts if they fail
for new reasons).

## Interim verdict

No GOAT call is possible: the ON cohort is empty. The feature stays
default-on (purely user-invoked, no autonomous cost). Next unit: after the
user rebuilds/restarts, accumulate >= 10 verdict-on chains from real usage,
re-run the scorer, and score the gate.
