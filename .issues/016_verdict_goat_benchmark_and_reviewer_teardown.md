# Verdict ping-pong GOAT benchmark + reviewer session teardown

Status: PART 1 harness SHIPPED (persistence + scorer), Part 2 teardown FIXED (drain pattern); remaining: run the >= 20-task benchmark and record the GOAT verdict

## Context

`.proposals/001_claude_sub_agent_verdict.md` shipped phases 1-6 behind the
`verdict_ping_pong` GOAT gate — now defaulting ON (the feature is purely
user-invoked via button/right-click, so there is no autonomous cost). The
benchmark plus real-usage data decides whether it STAYS enabled or is demoted
(flag set to false). The benchmark measures evidence that a reviewer verdict
reduces rework enough to justify 2N extra LLM calls per negotiation
(N = rounds, default cap 3).

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

- [x] Persist `reviewer` label + round counters alongside thread records so the
      scorer can join telemetry to threads. Shipped as a `reviewer` field on
      `RequestVerdictToolOutput` (both variants, `#[serde(default)]` so old
      threads replay) — the structured output already persists into
      `threads.db` via `LanguageModelToolResult.output`, so no schema change
      was needed and the join is by construction (`tool_name ==
      "request_verdict"`).
- [x] Scorer script: `script/verdict_scorer.py` (uv + zstandard). Reads
      `threads.db`, decompresses the zstd thread blobs, splits verdict-on/off
      cohorts, and reports post-hoc fix rate / rounds distribution / aborts /
      token averages. Also links continuation chains from agent_ui's
      `sidebar_threads` (`continued_from_session_id`) so post-hoc fixes in
      follow-up threads count against the originating thread. Validated
      against 1550 local threads (0 parse failures, 486 continuation links).

### Findings from the local dry run

0 of ~880 summary-bearing threads had ANY user message after the final
`## Summary` in the same session — corrections happen in follow-up threads.
Addressed in the scorer: threads are grouped into continuation chains, and a
continuation whose FIRST user message matches the correction heuristics
("fix diag error", "you missed", ...) counts as a post-hoc fix of the chain.

Baseline on this machine (all verdict-off, feature never enabled locally):
43/483 chains flagged = **8.9% post-hoc fix rate**. The verdict-on cohort is
empty until the 20-task benchmark populates it; the GOAT gate stays
">= ~30% relative reduction vs 8.9% baseline".

## Part 2 — reviewer session teardown limitation (phase 6) — FIXED

`acp_thread::verdict::prune_expired` runs on lock without `cx`, so a
TTL-expired external (claude_code) reviewer session dropped the registry handle
but could not send `close_session` — the ACP process idles until app exit.

Fixed with the drain pattern: `prune_expired` moves expired entries' reviewer
threads into a pending-close list (`drain_pending_closes` pumps it with an
`App` handle), and the agent panel's existing 10s notification drain loop
invokes it — no new timer. Covered by
`expired_reviewer_sessions_defer_close_and_drain_closes_them` (local
close-counting connection asserted end-to-end).

- [x] Drain pattern: prune collects expired reviewer threads into a
      pending-close list; `drain_pending_closes(cx)` invoked from the panel's
      existing 10s notification drain loop.
- [-] Pass an `App` handle into prune paths that have one and close inline.
      (Superseded — the drain pattern covers the abandoned-negotiation case;
      every path that already has `cx` closes inline via `complete_reviewer`.)

## Refs

- `.proposals/001_claude_sub_agent_verdict.md`
- `.plans/029_claude_code_verdict_reviewer.md`
- commits: e5b496bf46 (phases 1-5), phase 6 commit (this issue's sibling)
