# 058 — auto_prompt: structured resume contract + crash-loop guards (rsi_loop lessons)

Status: open — distilled from OpenRSI-Index @ 8d170deed4591fc7e4306f2d86ff4d9bcffd1288 (distill verdict: NO-GO rule corpora, harness-lane intel HIGH; see `.plans/055`)

Five patterns from the RSI-Harness inner loop (`rsi_loop`) that map onto auto_prompt and the rig lanes. The plan 055 scoreboard/ledger covers lesson-carry; these are the remaining adoptables.

## Tasks

- [ ] **Resume-message contract**: overflow/interrupt continuations inject a structured report — attempt number, which in-flight rounds counted without a score, what state died, exact budget left — instead of a bare "continue" (their `runtime/resume.py _message` pattern; prevents double-counting and false state beliefs)
- [ ] **Minimum-runtime guard**: a worker turn that exits near-instantly between auto-prompts is a systematic failure, not progress — count it distinctly from idle turns and stop after a short streak (their `MIN_RUNTIME_FOR_RESUME`; complements `steps_repeats`)
- [ ] **Resume-ceiling audit**: verify every fork/resume path (overflow Phase 1/2, cutoff, clarification) shares one hard cap (their `MAX_RESUMES=100` / 20-image-layer analog) — no path with an unbounded loop
- [ ] **Argv hygiene rule candidate**: never pass task text in argv to a spawned agent — prompt via stdin/config; argv is a kill surface via `pkill -f` (their scar: an agent pkilled its own process from task text in argv)
- [ ] **Fairness-manifest pinning** (perf-rematch lane): pin the league fairness manifest inside the scorer and assert before scoring (their `require_policy_ready`); promote baselines only from verified eligible records with recomputed reward (their `freeze_accepted` — "never a Work claim")

## Refs

- Distill verdict + mechanisms: `.plans/055_auto_prompt_tried_steps_ledger.md` (Refs section)
- Source: OpenRSI-Foundation/OpenRSI-Index @ 8d170deed, `rsi_loop` (judge server, resume, noise guards) — clones removed; re-clone to `.raw` on SD card if implementation needs the quote-level detail
