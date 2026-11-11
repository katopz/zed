# 058 — auto_prompt: structured resume contract + crash-loop guards (rsi_loop lessons)

Status: open — distilled from OpenRSI-Index @ 8d170deed4591fc7e4306f2d86ff4d9bcffd1288 (distill verdict: NO-GO rule corpora, harness-lane intel HIGH; see `.plans/055`). Progress 2026-11-11: tasks 1+2 LANDED; three remain.
**Claimed:** amo-idle-lane, 1794391000, expires 2026-11-13T00:00:00+00:00

Five patterns from the RSI-Harness inner loop (`rsi_loop`) that map onto auto_prompt and the rig lanes. The plan 055 scoreboard/ledger covers lesson-carry; these are the remaining adoptables.

## Tasks

- [x] **Resume-message contract**: LANDED — `chain::resume_state_section(session, max_iterations)` builds the structured report (attempt number, idle streak + exact runaway budget left, unchanged-steps repeats, near-instant hops, verdict negotiations); `with_resume_state_section` PREPENDS it in `make_continue_action` — the single choke point every same-thread continuation AND the overflow Phase 2 fork flows through (overflow.rs builds its fork action via `make_continue_action`), so a forked context carries its chain's facts even though the chain state itself stays on the old session. Skipped when the prompt already carries the section (rebuilt actions never stack a stale copy) or when no chain exists. Their `runtime/resume.py _message` analog. Tests: `make_continue_action_prepends_resume_state_without_stacking`, `resume_state_section_absent_without_a_chain`
- [x] **Minimum-runtime guard**: LANDED — `chain::MIN_TURN_RUNTIME_SECS` (10s) / `NEAR_INSTANT_LIMIT` (3): a CHAINED hop whose gap since the previous turn's end is under 10s stacks `near_instant_turns` (retry-id-only counting does not apply here; the streak is per-hop); real runtime or made-progress resets; unchained turns never count (a fresh stop after quiet is not a crash loop). Both deciders halt the chain at the limit (`near_instant_turn_crash_loop` in debug_log, before the idle cap). Their `MIN_RUNTIME_FOR_RESUME` analog; complements `steps_repeats`, which a re-wording loop can outrun. Tests: `near_instant_chained_hops_accumulate_until_real_work_resets`
- [ ] **Resume-ceiling audit**: verify every fork/resume path (overflow Phase 1/2, cutoff, clarification) shares one hard cap (their `MAX_RESUMES=100` / 20-image-layer analog) — no path with an unbounded loop
- [ ] **Argv hygiene rule candidate**: never pass task text in argv to a spawned agent — prompt via stdin/config; argv is a kill surface via `pkill -f` (their scar: an agent pkilled its own process from task text in argv)
- [ ] **Fairness-manifest pinning** (perf-rematch lane): pin the league fairness manifest inside the scorer and assert before scoring (their `require_policy_ready`); promote baselines only from verified eligible records with recomputed reward (their `freeze_accepted` — "never a Work claim"). Note: issue 055's verify lane already pins the baseline side (BLAKE3'd manifest asserted at submit); what remains here is the riir-ai scorer-side pin

## Refs

- Distill verdict + mechanisms: `.plans/055_auto_prompt_tried_steps_ledger.md` (Refs section)
- Source: OpenRSI-Foundation/OpenRSI-Index @ 8d170deed, `rsi_loop` (judge server, resume, noise guards) — clones removed; re-clone to `.raw` on SD card if implementation needs the quote-level detail
