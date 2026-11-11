# 038 — closed-issue records (the noise-reduction ledger)

Status: living document — one row per removed `.issues/` file, in removal
order. The file's full life is `git log --follow -- .issues/<file>`; the
durable record lives in this row + the linked commits. Convention per the
workspace's noise-reduction rule: work lands → the record is documented
(here or in its own `.docs/NNN` page) → the issue file is removed.

## Removed 2026-11-06 (zed idle lane, issue 055)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 055 | native agent request-phase hang never armed the 120s stream idle timeout — no auto retry, turn wedged in `Generating` until the watchdog — FIXED in both request paths | turn loop `08f72a5a7a` + issue-file removal `0b19681fe6` + metadata (title/summary) follow-up `2f170a550c` | the fork_tests doc comments (`test_request_phase_idle_timeout_*`, `test_metadata_request_idle_timeout_*`) carry the scenario records; root cause + evidence live in the fix commits' messages |

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

## Removed 2026-11-11 (amo idle lane, issue 056)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 056 | Guard sets + arm isolation — DONE: hard multi-axis gates via the verify lane's ratio verdicts (any regression refutes), `Baseline.arm` one-knob-per-arm labels into `VerifyLedgerRow.arm`, `quiet_at_start` contention evidence on every run (refuse-to-count beyond refuse-to-start) | `34e43f0bda` + retirement commit | this row; the guard-set discipline (declare wall+compile+binary axes per claim) is the manifest contract in `crates/agent_mail/src/verify.rs` docs |

## Removed 2026-11-11 (amo idle lane, issue 055)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 055 | Harness verifier-rig separation — DONE: `rig run --verify-manifest` (Exclusive-forced, producing-rig refused), BLAKE3-pinned manifest carrier, `Baseline` ratio-to-baseline verdicts (verified/refuted/incomplete/inconclusive, guard-set: any regression refutes), `RunEnvironment` provenance on every job, `no-result` outcome for missing cells, orchestrator `verify_ledger.jsonl` | `34e43f0bda` (verify.rs + jobs.rs wiring + main.rs CLI) | the issue file's git life carries the design + manifest example; tests: `verify_verdicts_follow_the_noise_floor_and_metric_directions`, `verify_run_computes_ratios_and_writes_the_referee_ledger`, `a_verify_run_without_its_result_file_is_no_result`, `pick_rig_never_lands_a_verify_run_on_the_producing_rig`, `manifest_hashes_stably` |

## Removed 2026-11-11 (amo idle lane, issue 059)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 059 | `test_agent_mail_broadcast_respects_liveness_cutoff` "reads real `~/.agent-mail`" — NOT REPRODUCIBLE: single test (23/23) and full suite (843/0) pass on live amo at the same tree that failed that morning; dependency audit found no reachable real-home read (temp-dir injection covers sender + bridge; `start_if_installed` is `cfg!(test)`-gated; other `from_env` callers unlinked/off-path). Filed `set_var("AGENT_MAIL_HOME")` fix REJECTED: process-global env mutation under a parallel harness guarding a nonexistent path. Suspected real mechanism: real-clock reads vs the mock executor clock (backward wall-step ⇒ exactly the observed 0). Self-adjudicated while the verdict reviewer was rate-limited | evidence + audit in the issue file's life (`git log --follow`), removal commit | this row; if it recurs, re-file WITH the failing assertion output + `date` + env state captured at failure |

## Removed 2026-11-11 (amo idle lane, issue 054)

| issue | verdict | landed in | durable record |
|---|---|---|---|
| 054 | rig-job runner SSH auth on amo (`Host key verification failed`) — FIXED both halves: stale known_hosts entries on amo+m3 healed, and amo→m3 legs must use `kat@` (m3's `katopz` account has no authorized_keys); full mesh verified 2026-11-06 | fixed 2026-11-06 (ops-only, no repo commit); this removal commit is the record | this row. Carried forward: (a) amo cannot reach m3 LAN IPs (192.168.1.34/.36 — m3 firewall stealth mode drops amo-originated legs); Tailscale legs unaffected, fleet routes over Tailscale by design — LAN acceleration needs an m3 firewall allow-rule for amo. (b) If fresh Tailscale/LAN IP churn re-exposes batch-mode prompt-death, re-file in the agent-mail runner source (outside this repo) for `HostKeyAlias`/`StrictHostKeyChecking=accept-new` on fleet-internal legs |

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
