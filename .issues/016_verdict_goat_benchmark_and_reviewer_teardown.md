# Verdict ping-pong GOAT benchmark + reviewer session teardown

Status: OPEN — blocks promoting `agent.verdict_ping_pong` to default (proposal 001 phase 5)

## Context

`.proposals/001_claude_sub_agent_verdict.md` shipped phases 1-6 behind the
`verdict_ping_pong` GOAT gate (default off). Promotion to default requires the
phase 5 benchmark: measurable evidence that a reviewer verdict reduces rework
enough to justify 2N extra LLM calls per negotiation (N = rounds, default cap 3).

Also tracked here: the phase 6 teardown limitation below.

## Part 1 — GOAT benchmark design

### Hypothesis

A `#Verdict` ping-pong with an independent reviewer reduces post-summary
corrections by enough to pay for the extra tokens.

### Metrics (per thread, verdict on vs verdict off)

| Metric | Source | Notes |
|---|---|---|
| post-hoc fix rate | thread history: user correction/continuation messages within the same session after the final `## Summary` (e.g. "you missed", "actually", "fix") | primary metric; needs a scorer pass over saved threads |
| rounds used | `Verdict Subagent Completed` telemetry (`round`, `max_rounds`, `reviewer`) | already emitted per round |
| token cost | thread token usage vs the no-verdict baseline | reviewer + parent reasoning calls |
| negotiation aborts | `request_verdict` error outputs (budget-exhausted / expired) | quality signal for the protocol itself |

### Procedure

1. Run >= 20 comparable tasks (mixed: doc-sync, bugfix, refactor) on develop,
   half with `verdict_ping_pong: true`, half without; otherwise identical setup.
2. Score post-hoc fix rate with the keyword/heuristic scorer above; manually
   review flagged threads.
3. GOAT verdict: promote to default only if fix rate drops materially (>= ~30%
   relative) at a token overhead <= ~2x baseline turn cost, with abort rate
   < 10% of negotiations.
4. Record in `.benchmarks/{NNN}_verdict_ping_pong_goat.md`.

### Harness work

- [ ] Persist `reviewer` label + round counters alongside thread records so the
      scorer can join telemetry to threads (telemetry is currently log-only).
- [ ] Scorer script for post-hoc fix detection over thread history.

## Part 2 — reviewer session teardown limitation (phase 6)

`acp_thread::verdict::prune_expired` runs on lock without `cx`, so a
TTL-expired external (claude_code) reviewer session drops the registry handle
but cannot send `close_session` — the ACP process idles until app exit.
Bounded: at most one idle session per abandoned negotiation (parent never sent
`final_round` and never hit the round cap).

Mitigation options, in preference order:

- [ ] Drain pattern: prune collects expired reviewer threads into a
      pending-close list; `drain_pending_closes(cx)` invoked from the panel's
      existing 10s notification drain loop.
- [ ] Pass an `App` handle into prune paths that have one and close inline.

## Refs

- `.proposals/001_claude_sub_agent_verdict.md`
- `.plans/029_claude_code_verdict_reviewer.md`
- commits: e5b496bf46 (phases 1-5), phase 6 commit (this issue's sibling)
