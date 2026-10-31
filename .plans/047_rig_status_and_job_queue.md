# 047 — Rig status + job queue (shared / exclusive lanes) on agent-mail
Status: DONE — P0 status/outbox, P1 queue + poison labels, P2 keys/auto/worktree/timeout, P3 real e2e on amo/bmo/4090 all pass

## Problem

- "What is rig X busy with?" is answered by guessing from thread titles and
  `ps`. Titles lie (stale, wrong), and work launched from another rig over SSH
  shows up as anonymous processes.
- Quiet-sensitive work (latency benchmarks) gets **poisoned** mid-run by a build
  or batch some other agent started, and the whole result set is lost.
- Two agents can pick the same task and run it on the same rig, or on two rigs.

## Rig roles (owner, 2026-10-31)

| Rig | Role | Availability | Notes |
|---|---|---|---|
| m3 | orchestrator (MacBook) | **intermittent** (travels, sleeps) | never a job target by default; on battery = not quiet |
| amo | stationary **builder**, Zed IDE | always on | natural home for the dedup arbiter |
| bmo | **runner** (no Zed) | always on | quiet benchmarks belong here |
| 4090 | Windows CUDA / heavy, Zed IDE | always on | processes started from an sshd session die with it → detach via `schtasks` (known quirk) |

## Verdict

Build it **into agent-mail** (same binary, same `rigs.json`, same SSH
transport, same mail/wake system); no new daemon, no new port. One command
answers state, one command queues work, and every lifecycle event is mail, so
the submitting agent ends its turn and is woken when its job starts, finishes,
or gets poisoned. No `sleep` loops.

| Option | Verdict |
|---|---|
| **Per-rig lease + queue files, self-propagating executor, mail notifications, amo key arbiter** | **GOAT**: reuses everything shipped in plan 043; nothing to keep running |
| Persistent per-rig daemon (agentd-style) | rejected: agentd is Unix-only; the 4090 needs schtasks anyway; a crashed daemon is a silent queue stall |
| Central scheduler on one rig (Slurm-like) | rejected: m3 is intermittent; a single point that must be up for local runs |
| Git-pushed claim files only (existing issue lease) | keep for *issue* ownership; too slow (fetch/push) and too coarse for *job* slots |

## Design

### Two layers of "claim", deliberately separate
1. **Issue lease** (exists: `**Claimed:**` line + `katgpt-rs/scripts/claim_lease_check.py`):
   who owns the *work item*. Unchanged.
2. **Job key** (new): who is *running* a concrete job (`--key issue-1022:leg2`,
   `bench:riir-ai@<sha>:prefill`). Same key submitted twice → the second
   submit gets the existing job id back and is **subscribed** to its mail
   instead of running a duplicate, on any rig.

### `agent-mail rig status [rig|all]` (one command, parallel SSH fan-out)
Per rig: managed jobs (lease holder, lane, key, title, submitter agent,
started, elapsed, ETA if given), queue (lane, key, submitter, position),
**unmanaged contenders** (heavy processes not started via `rig run`, classified
with riir-ai's vocabulary files `contender_names.txt` / `contender_gpu_names.txt` /
`contender_exempt_names.txt`, read at runtime by configured path, not a code
dep: public-leaf law), live agents (Claude `~/.claude/sessions/*.json` name +
cwd + status; Work Remote sessions via `agentd status`), quiet verdict
(contenders + load + power/battery + thermal), GPU util/mem (`nvidia-smi` on
4090), disk free. `--json` for agents. Unreachable rig = `offline` with last
seen, never an error that hides the others.

### `agent-mail rig run <rig|auto> --shared|--exclusive [--key K] [--needs cuda,metal,zed] [--at WHEN] [--timeout D] [--worktree <repo>@<sha>] -- <cmd>`
- **Shared** lane: runs now if the rig's free resources cover the job's declared
  needs (cores, RAM, GPU memory) after subtracting running managed jobs' reservations,
  and no exclusive job holds or waits; otherwise it queues.
- **Exclusive** lane ("quiet"): waits until no managed job runs AND the quiet
  probe passes (no unmanaged contenders for N s, power admissible, not
  thermally throttled), then holds the rig alone. **Writer-preferring**: once
  an exclusive job waits, no new shared job starts there, so benchmarks
  cannot starve.
- **Poison sampling**: during an exclusive run the executor re-probes every
  15 s. A contender appearing marks the job `POISONED` with the offender (pid,
  name, cwd, who started it if managed), and mails the submitter
  immediately, so results are *labelled* and not silently lost. Default policy:
  keep running + label; the submitter decides (owner decision).
- `--worktree repo@sha` runs in a fresh `git worktree` + per-job
  `CARGO_TARGET_DIR`, so a job never builds a sibling agent's dirty tree and is
  reproducible.
- `auto` picks a rig by `--needs` + role + free resources + queue depth; never m3 (owner decision: m3 takes no jobs, ever).
- `--at` composes with plan 043 scheduled mail (queue at a time).

### No daemon: self-propagating queue + reap-on-touch
- State per rig: `~/.agent-mail/rig/{leases,queue,jobs}/` (atomic rename, same
  maildir discipline as the inbox).
- `rig run` spawns a detached **executor** (setsid on Unix, `schtasks` one-shot
  on the 4090) that waits for its slot, runs, writes the result, mails, then
  **starts the next eligible queued job** before exiting.
- Every `rig` command (and the waiter hook's existing 500 ms poll, cheap
  `stat` only) **reaps** dead leases (pid gone / heartbeat stale) and kicks
  the queue, so a crashed executor never stalls a rig for long.

### Dedup arbiter: amo
Job keys are created on amo with an atomic exclusive create (`O_EXCL`), the
only race-free point across rigs (two agents picking *different* rigs for the
same task at the same instant). If amo is unreachable: fall back to a
target-rig-local key check and print a loud `DEDUP-DEGRADED` warning.

### Notifications (plan 043 mail)
`queued` (with position), `started`, `blocked` (exclusive waiting > N min,
naming the blocker), `poisoned`, `done`/`failed` (exit code, duration, log
tail, log path on the rig). Submitter + subscribers receive each one.

## What the owner missed (fill-ins)

1. **Unmanaged work still poisons.** Leases only bind work launched through
   `rig run`. A hand-typed `cargo build` from another agent is invisible to the
   queue, so the quiet probe + poison sampling are what actually protect a
   benchmark. Agent docs must say: heavy work goes through `rig run`.
2. **m3 offline = mail to m3 fails today** (no outbox, verified: no
   retry path in `agent_mail.rs`). A job submitted from m3 that finishes
   while m3 is closed loses its notification. Needs a sender-side **outbox**
   with retry (flushed by the next `rig`/`send` call or hook tick). Do this
   first: it is also a plan 043 bug.
3. **Power and thermal are part of "quiet".** m3 on battery or any rig
   thermally throttled poisons latency numbers even with zero contenders
   (riir-ai's `perf_m3_prefill_ranking.sh` already folds power into its quiet
   streak; reuse that rule).
4. **Repo state is part of the job.** Running in a shared checkout races
   sibling agents' edits; `--worktree repo@sha` + per-job target dir fixes it.
5. **The 4090 sshd quirk**: anything `rig run` starts there over SSH dies with
   the session unless launched via `schtasks`.
6. **Remote execution trust**: jobs are submitted over SSH (already
   key-authenticated), never by mail body, so mail stays data and never turns
   into a remote-command channel.
7. **Cooldown between quiet jobs** (thermal recovery) should be a per-rig
   setting, so back-to-back benchmarks don't measure each other's heat.

## Owner decisions (2026-10-31)
- [x] amo is the dedup arbiter.
- [x] Poison policy: **label and report back; the submitting agent decides** (no auto-abort, no auto-requeue).
- [x] No fixed slot caps: admission is **resource-based**. A job declares what it needs (cores, RAM, GPU memory); the rig admits it if its free resources minus what running managed jobs reserved cover it, else it queues.
- [x] **m3 never takes jobs** (owner's rig, runs hot). m3 is orchestrator only: `rig status` reports it, `rig run` refuses it, `auto` never picks it.

## Tasks
- [x] P0 outbox: an unreachable rig parks mail in `outbox/<rig>/<next try ms>_<attempts>_<id>.json` (send returns `QUEUED` + reason instead of failing; an unknown rig is still an error). Retried with backoff 30 s → 15 min by `agent-mail flush`, every `rig status`, each idle Claude waiter (every 60 s) and the Zed native poller (every ~60 s); one failure per rig postpones the rest without trying them; rename into `sending/` is the claim and `deliver` drops an id it already holds (at-least-once, no duplicates). Native `send_message` reports `queued`. Tests: `unreachable_rig_parks_mail_and_flush_retries_it`, `unknown_rig_is_an_error_not_parked`, `redelivered_id_is_dropped`
- [x] P0 `agent-mail rig status [RIG...] [--json]` (`crates/agent_mail/src/rig.rs`): parallel SSH fan-out of `rig probe`; contenders by a faithful port of riir-ai `perf_rematch.sh::classify_contenders` (name → gpu-class → resource with editor bound → sustained; same env knobs; same argv identity incl. wrappers/interpreters) reading `riir-ai/scripts/contender_*.txt` at runtime (missing vocabulary = BLIND, never quiet); cwd per contender (lsof / /proc); Claude agents from `~/.claude/sessions`; load, memory, disk, macOS power + CPU speed limit, `nvidia-smi` GPUs; outbox backlog; roles + `accepts_jobs` (m3 `false`) in `rigs.json`; same-named workers collapse to one line. Windows samples natively via `sysinfo` (closes the `ps -W` no-CPU blind spot riir-ai has there; PowerShell+WMI took ~3.3 s, sysinfo ~0.5 s; pinned `>=0.33,<0.39` on the 4090, whose rustc 1.93 cannot build 0.39). Tests: `classifies_like_riir_ai_perf_rematch`, `identity_skips_wrappers_and_names_interpreted_scripts`, `parses_ps_rows_and_cpu_time`, `missing_vocabulary_is_blind_not_quiet`
- [x] P1 `agent-mail rig run <rig|auto> --shared|--exclusive …` (`crates/agent_mail/src/jobs.rs`): per-rig queue `~/.agent-mail/jobs/{queue,running,done,logs}`, FIFO by UUIDv7; `admit()` = shared jobs by declared cores/RAM/GPU vs measured load + reservations (with backfill), exclusive waits for an empty + quiet + cooled (60 s) rig and blocks everything behind it (writer-preferring); detached executor (`process_group(0)` on Unix, one-shot `schtasks` on the 4090 so it outlives the sshd session) claims by rename; reap-on-touch from every probe/finish + a short-lived kicker (every 20 s while jobs wait, exits when the queue drains); lifecycle mail STARTED / POISONED / DONE·FAILED·TIMEOUT·CANCELLED with exit code, log path and log tail; `rig cancel`, `rig log`; m3 refused (`accepts_jobs`); managed job trees reported as jobs, not contenders
- [x] P1 poison sampling: every 15 s during an exclusive run the executor re-runs the contention check excluding its own process tree (new `parent` pid per process row); a new contender → `POISONED` label in the record + immediate mail; the job keeps running and the submitter decides (owner policy)
- [x] P2 job keys: `--key` claimed on the orchestrator (amo) with an exclusive create; a second submit of the same key (any rig) gets `DUPLICATE` + is subscribed; when the job ends the executor releases the key and amo mails every subscriber the result; orchestrator unreachable → `DEDUP-DEGRADED` warning and the job still queues
- [x] P2 `auto`: online, takes jobs, has every `--needs` cap (`caps` per rig in `rigs.json`: amo metal/zed, bmo metal, 4090 cuda/zed), exclusive prefers a quiet empty rig, shared the most free cores, fewer waiting jobs first; `--worktree repo@sha` = fresh `git worktree` + own `CARGO_TARGET_DIR`, removed (and pruned, inside the source repo) after; `--timeout` kills the job's process tree; 60 s cooldown between exclusive jobs
- [x] P3 agent docs: `rig status` + `rig run` lines in CLAUDE.md / Zed AGENTS.md on all rigs (4090 Zed AGENTS.md staged at `agent-mail-build\\AGENTS.md.with-mail`, its Zed holds the file)
- [x] bench: `rig status` over 4 rigs 0.53 s / 1.67 s / ~0.55 s wall (4090 dominates; its probe 0.40-0.60 s). Target < 1.5 s met except one 4090 outlier; hook path untouched (the outbox flush runs on the waiter's side thread, not the per-tool-call hook)
- [x] e2e (real rigs, 2026-10-31):
  - exclusive job to bmo while ANOTHER agent's real benchmark ran there (`bench_809 … flash_cmma`) → stayed queued, never started, cancelled: the protection works and the foreign bench was untouched (no injection on bmo on purpose)
  - amo: exclusive (key e2e-key-1) started at once; same key from a second agent → `DUPLICATE`, subscribed; shared job queued behind it waited and started the same second the exclusive one ended (16:20:37); subscriber got the result mail
  - poison: contender (`python3 /tmp/cargo`, identity `cargo`) injected mid-run → caught 15 s in, `POISONED` mail, run continued, final mail labelled (a copied `/bin/sleep` named cargo is killed by macOS on launch: invalid injector, not a detector miss)
  - kicker: exclusive waiting behind a contender started 15 s after the contender ended with no `rig status` call
  - `--timeout 5s` on `sleep 100` → `TIMEOUT`; `--worktree katgpt-rs@HEAD` → fresh checkout at `2bbaf2640` + own target dir, cleaned up (first run leaked the git registration: fixed by running `git worktree remove/prune` in the source repo)
  - `auto --needs cuda` → 4090 → scheduled-task executor → `win-ok` + `NVIDIA GeForce RTX 4090` → DONE mail
- [x] P3 (partial) agent docs: `rig status` line added to CLAUDE.md / Zed AGENTS.md on every rig

## P0 findings (first real run, 2026-10-31)
- m3 was BUSY with an agent's `flashmemory_decode_gate` (Bonsai-27B, 17.7 GB, `/tmp/i1117_128k`) although m3 takes no jobs; P1 `rig run` must refuse m3, but only agent docs can stop hand-started work there.
- amo: `cargo test -p seal-view` in worktree `seal-remake-wt068` plus a katgpt-rs build (31 rustc workers, collapsed in the human view).
- 4090: `cargo test -p riir-train-gpu --test plan474_*` at 0% CPU: the NAME arm still counts it (riir-ai rule: an idle cargo is about to compile).
- bmo: QUIET, the natural benchmark runner.

