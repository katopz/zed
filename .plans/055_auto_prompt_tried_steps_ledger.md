# 055 — auto_prompt: tried-steps ledger + plan scoreboard

Status: done — landed in commit 1d6a031927; deferred follow-ups filed as `.issues/055` (verifier rig), `.issues/056` (guard sets), `.issues/057` (owner-gate batching)

Lessons applied from the OpenRSI × SGLang-Omni writeup (Harnessed RSI): the outer loop must carry every lesson into the next round, and progress needs a scoreboard denser than summary prose.

## Tasks

- [x] T1 tried-steps ledger in `chain.rs`: rolling `Vec<LedgerEntry>` (fingerprint + single-line excerpt, 8 slots, `LEDGER_EXCERPT_CHARS` = 120 via `chars().take()`, never byte slicing). `record_summary_steps` inserts distinct proposals; `tried_steps_excerpts` reads it. Survives differently-worded summaries that reset the consecutive-repeat fingerprint — the loop-bug class.
- [x] T2 ledger injection at the single choke point: `make_continue_action` appends a `TRIED-STEPS LEDGER` block (idempotent via header guard) to every same-thread continuation, so the worker sees what was already proposed. Fresh sessions/forks are unaffected (empty ledger).
- [x] T3 plan scoreboard: `chain::record_plan_progress` classifies the round-over-round delta of (checked, actionable-unchecked) across the session's claimed plan files — `First / Advanced{checked_delta} / Flat / Regressed`. Wired into `decide_precheck` at the `record_turn` site: `Advanced` counts as progress (resets the idle streak) even when the summary wording repeats; `Regressed` logs a warning and never resets the streak. Reader: `plan_registry::claims_for_session`; counters via `lightweight_context::count_checked_tasks` sharing ONE scan with `count_actionable_tasks` so the two can never disagree. Unreadable/missing files give the round no opinion (`None`), never a fake regression.
- [x] T4 tests: ledger dedupe/cap/eviction, excerpt single-line + char bound (CJK), delta classification per session, injection idempotence, scoreboard-through-claims (temp file + claim via shared `lock_for_test`), checked-count scan parity (code blocks, skip sections, uppercase X). 486 lib + 43 integration green; clippy clean (`-D clippy::redundant-clone` caught one in the new test — fixed).

## Deferred (filed, not built here)

- Independent verifier on a different rig + frozen-baseline manifests → `.issues/055`
- Guard sets (wall time AND compile time AND binary size AND no-neighbor-regression) + one-knob arm isolation → `.issues/056`
- Owner-gate batching at `<stop>` + chain KPI counters → `.issues/057`

## Refs

- Commit: 1d6a031927 (feat(auto_prompt): tried-steps ledger + plan scoreboard for chains)
- Blog: https://index.openrsi.foundation/blog/2026/openrsi-sglang-omni/
- Distill verdict on the RSI-Harness source: NO-GO for the riir-refine rule corpora (agent-harness domain, not Rust-perf rule material); harness-lane intel HIGH — see session notes 2026-11-10
