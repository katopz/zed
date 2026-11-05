# 038 — closed-issue records (the noise-reduction ledger)

Status: living document — one row per removed `.issues/` file, in removal
order. The file's full life is `git log --follow -- .issues/<file>`; the
durable record lives in this row + the linked commits. Convention per the
workspace's noise-reduction rule: work lands → the record is documented
(here or in its own `.docs/NNN` page) → the issue file is removed.

## Removed 2026-11-05 (amo-idle hygiene pass)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 044 | agent-mail m3 inbound + native wake — DONE (plan 043 phase 2) | `569e787480` (retirement commit) | `.plans/043_cross_rig_agent_mail.md` + git history |
| 040 | close noted in plan 039 | `efaa26633e` | git history |
| 041 | closed | `606c3760c4` | git history |
| 023 | transport failures poisoned key backoff as quota verdicts — FIXED, 3 rounds (classifier split, upstream-hint provenance, fail-open verdict-awareness) | `26bb155267` + `fefcf6bfd7` + `4e33ed2386` | this row (the only record; no `.docs` page existed); the open timezone watch item (evidence-gated defer, promoted 2026-09-23) moves to `.issues/007`'s watch list → see below |
| 045 | Windows MSVC real-LiveKit + crash triage — DONE (livekit cfg aligned, PDB archival, cdb workflow, rustc.exe ICE triaged one-off) | `e67ac117eb` + tooling `655bc14343`/`4245c05cad`/`af839a6598` + docs `5b98612959`/`93abc30aa4` | this row; the two owner-gated observation arms (first-restart dump watch, RAM-ballooning check) carry here: symbolize a fresh `CrashDumps` hit with `pdb-archive/655bc14343.pdb`; check Zed.log for LSP notification loops (upstream #57477 pattern) before suspecting LiveKit |
| 046 | agent-mail delivery modes (urgent/steering/queued) — DONE, plan 052 all `[x]` | `ff5546b8a4` | `.plans/052_agent_mail_delivery_modes.md` (DONE) |
| 047 | `test_manual_auto_prompt_forks_claude_thread_above_overflow_gate` ambient-config hermeticity — FIXED | `7c2f1fba27` | this row (one-line test fix; the fix commit's message + this row are the record) |
| 048 | agent-mail fyi mode + broadcast gate + liveness cutoff — DONE, deployed 4 rigs, Claude verdict AGREE ×2 | `eec5d52d88` + fmt `d30a4a3a44` | `.docs/037_agent_mail_fyi_rollout.md`; the deferred fyi-folding follow-up + the owner-side AGENTS.md row move here |

## Carried-forward watch/deferred items (from the removed files)

- **023's timezone watch**: confirm which timezone Z.AI's reset timestamp
  carries (UTC+8 / UTC / account-local). Unblock: a captured raw 429 body in
  `Zed.log` (`body retry hint: reset at ...` line since `0670cffb71`), or Z.AI
  documenting it. Parse stays self-healing meanwhile.
- **045's owner-gated restart watch**: the first restart of the real-LiveKit
  msvc build; fresh dump → symbolize with `pdb-archive/655bc14343.pdb`;
  RAM-ballooning → check LSP notification loops first.
- **048's deferred follow-ups**: (a) fold parked fyi into the running turn's
  context (needs a mid-turn injection mechanism — steering-like); (b) the
  owner's AGENTS.md documents `--mode fyi` (owner-owned file).
