# Bench 001 — Verdict ping-pong GOAT (interim)

**Status:** RE-SCORED at the 20-chain bar 2026-11-11 (see the last section) — demotion rule fires on the corrections axis, held by the gating-law conflict recorded there; default stays on pending adjudication.

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

## Re-run 2026-10-27 (M3, fresh `threads.db`)

`uv run script/verdict_scorer.py`: 120 threads / 48 chains, 0 parse failures,
77 continuation links. This machine's database is new (the 2026-09 runs read
~21k threads on the old host), so the OFF baseline is small too.

| cohort | chains | with summary | post-hoc fix rate | rounds | aborts | avg chain tokens in / out |
|---|---|---|---|---|---|---|
| ON | 11 | 11 | 0.0% | 1:2, 2:5, 3:4 | 1 | 4.08 M / 339 k |
| OFF | 37 | 34 | 0.0% | — | 0 | 1.21 M / 73 k |
| FAILED | 0 | — | — | — | — | — |

Reading: the mechanism works (11 negotiations, 82% converge in 2-3 rounds,
1 abort), but the primary metric has **no signal** here: 0 corrections in
either cohort, so a relative reduction is undefined. The only measured
difference is cost: an ON chain uses 3.4× the input and 4.6× the output
tokens of an OFF chain. Still under the >= 20 ON chain threshold.

Interim verdict: unchanged. Stays default-on because it is purely
user-invoked, so it costs nothing until clicked. If the 20-chain run also
shows 0 vs 0, the post-hoc keyword scorer is too weak to decide the gate and
the next step is a blinded quality sample (rate N ON vs N OFF final
summaries), not more waiting.

## Re-run 2026-11-11 (amo, 24 ON chains — the >= 20 bar reached)

`uv run script/verdict_scorer.py` (threads.db on amo): 558 threads / 118
chains, 0 parse failures, 487 continuation links.

| cohort | chains | post-hoc fix rate | human correction rate | cited commits resolved | aborts | avg chain tokens in / out |
|---|---|---|---|---|---|---|
| ON | 24 | 37.5% (9/24) | 58.3% (14/24 with human turns) | 88.2% (1807/2048) | 9 | 11.7 M / 1.17 M |
| OFF | 86 (73 w/ summary) | 1.4% (1/86) | 10.5% (2/19 with human turns) | 85.7% (233/272) | 0 | 528 k / 50 k |
| FAILED | 8 | 62.5% (5/8) | 87.5% (7/8) | 95.0% (643/677) | 0 | 7.1 M / 882 k |

The pre-registered rule ("demote only if ON is then *worse* on corrections
or commit resolution") fires on the corrections axis: ON is 5.6x worse on
human corrections and 26x worse on post-hoc fix rate; commit resolution is
+2.5 pp in ON's favor (the only pro-ON signal).

**Held — default stays on. Two recorded reasons, deviation from the rule is
intentional:**

1. **Gating-law conflict (2026-11-11, owner decree):** `request_verdict` is
   now the delegated decision instrument for the fleet ("decide it via a
   Claude verdict, record the verdict, implement"); amo/m3 run the tool on
   the default (settings.json does not set the flag). Demoting unregisters
   the tool by default and breaks gating-law workflows on every box that
   relies on the default. The bench rule predates the decree.
2. **Selection confound, now dominant:** the ON cohort is no longer owner
   clicks on ordinary work — it is fleet verdict traffic (agent-negotiated
   REVISE/AGREE loops, plan 056 boss rounds; avg 11.7 M tokens/chain vs
   528 k OFF = 22x). Big, contested chains attract corrections by nature,
   so the corrections axis measures task contentiousness at least as much
   as summary quality. The 10-27(b) confound note about token cost applies
   to the correction metric too.

Also noted: reviewer-lane reliability is now the bigger problem — 9 aborts
plus 8 FAILED (all-error) chains out of ~41 attempts (~41%), including
claude_code 5-hour rate-limit failures. Over the < 10% abort gate by 4x.
That, not the flag default, is where the verdict lane loses its value.

Resolution path: a verdict session on the conflict itself (demote vs keep
vs demote-with-fleet-explicit-settings). Until then the feature stays
default-on; it remains user/agent-invoked with zero autonomous cost.

## Re-run 2026-10-27 (b): metric replaced, scorer can now see corrections

**Why the 0 vs 0 was structural:** the post-hoc metric only scores a user
message that follows the final `## Summary`, or a continuation's first
message. On an auto-prompt-driven host both positions are always filled by
machine-written text (the `## 1. Thread Summary` handoff, the context-overflow
summary prompt), and none of that text matches a correction pattern. The
human corrections that do exist ("fix it", "still lock", "i still not see
resource selector") land mid-thread after ordinary replies, where the metric
never looked. Waiting for 20 ON chains would have kept reading 0 vs 0, so the
fallback named above was taken now instead of after 8 more chains.

**Scorer changes (`script/verdict_scorer.py`):**
- *Human correction rate:* corrective human messages anywhere in the chain,
  per chain with at least one human turn. A user-role message is human
  unless it starts with a known machine prefix (handoff, overflow-summary
  prompt, cutoff-resume prompt, `<agent_message` relay, slash command, pasted
  `## Summary`) or exceeds 1000 chars (subagent briefs, pasted reports).
  Patterns gained `\bstill\b`, lost `one more` (it reads as a new request),
  and `fix` no longer matches after "we" ("shall we fix X?" proposes new work).
- *Cited-commit resolution rate:* backticked hashes in summary messages,
  resolved with `git cat-file --batch-check` against every repo under
  `~/git` (`--repos-root` to override). An objective summary-quality check
  that needs no LLM judge. Unresolved means not on this host (4090-only
  repos, rebased and pruned), not proven hallucinated, but it is comparable
  across cohorts.

| cohort | chains | human correction rate (auto) | hand-labelled | cited commits resolved | avg chain tokens in / out |
|---|---|---|---|---|---|
| ON | 12 | 25.0% (3/12) | 16.7% (2/12) | 95.8% (250/261) | 3.81 M / 317 k |
| OFF | 36 (18 with human turns) | 27.8% (5/18) | 22.2% (4/18) | 94.2% (179/190) | 1.23 M / 75 k |

Hand labels drop one automatic hit per cohort: ON `6974c461` (a "we still…"
context line in a new request) and OFF `beb62b26` (a pasted agent report).

**Verdict: NOT GOAT on current evidence; feature stays default-on.**
- Quality: ON is better on both measures but within noise: 2 vs 4 corrected
  chains (25% relative, below the >= 30% bar even taken at face value) and
  +1.6 pp commit resolution. No measurable quality gain.
- Cost: ON chains use 3.1× input / 4.2× output tokens, over the <= 2× gate.
  Partly confounded (verdict gets clicked on bigger tasks), so this bounds
  the overhead from above rather than measuring it.
- Default stays on because the gate was never about autonomous spend: the
  feature runs only when the user clicks it, so "demote" would only hide a
  button. Revisit at >= 20 ON chains with the two new measures; demote only
  if ON is then *worse* on corrections or commit resolution.
