# 038 — agent-mail `.starting`-limbo claims are invisible and silently lost

Status: OPEN — filed 2026-11-12 (amo idle lane) after today's calibration job was eaten by the wedge this fixes.
**Claimed:** zed-11bec7de@amo, 1794529000, expires 2026-11-14T17:30:00+00:00

## Evidence (amo rig, 2026-11-12)

`spawn_executor` renames `queue/{id}.json` -> `running/{id}.starting` and THEN spawns the executor. If the spawn fails — or the kicker dies between the two steps, or the executor dies before `execute_job` consumes the marker — the job is stranded in `.starting` forever. Nothing scans for it:

1. **The reaper is blind to it.** `schedule_jobs_inner` reaps dead executors by scanning `running/*.json` only; a `.starting` marker carries no executor pid and is never re-checked.
2. **`rig cancel` refuses it.** `cancel_job` checks `queue/{id}.json` then `running/{id}.json` — a stranded claim answers `no queued or running job {id} here` (hit verbatim today: job `01a1cb8b`, a 5 h calibration boot, submitted 16:57Z, marker created, no executor ever spawned, unkillable via the CLI).
3. **`rig status` doesn't list it.** The job vanishes from every surface while still holding a claimed dedup key.

Fleet-wide debris from this class, all confirmed dead (no `rig exec` process, executor log 0 bytes): amo `running/` held 8 stranded markers from 2026-11-02 to 2026-11-12 (jobs `01a19c9f`, `01a19dcd`, `01a1a045`, `01a1a09f`, `01a1b85f`, `01a1c8b5`*, `01a1c8dd`*, `01a1cb8b`) — each a job that never ran and never notified its submitter. (*the two 11-11 markers are a related-but-different class: held by LIVE `zed rig exec` processes 13h+ without ever consuming the marker — the holder check below deliberately leaves held claims alone; the Zed-side runner lifecycle is its own lane.)

## Fix (all in `crates/agent_mail`)

- [ ] `spawn_executor` rolls the claim back on spawn failure (marker -> queue, next tick retries) instead of stranding it
- [ ] Recovery in `schedule_jobs_inner`: a `.starting` marker older than a grace window (120 s — the real executor consumes it in milliseconds) whose id no live process command line names as `rig exec {id}` is requeued to `queue/{id}.json`; unreadable/invalid-id markers are removed as debris; a blind process table skips recovery (never act as if a holder is absent)
- [ ] `cancel_job` sees a stranded claim: holder check, then remove the marker + release the dedup key, answering `cancelled stranded job {id}`; blind table refuses loudly (assume held)
- [ ] Tests: holder matcher (pure), grace window, recovery requeue + debris removal, stranded cancel + key release

## Refs

- Manual debris sweep 2026-11-12 (amo): the five 11-02 to 11-08 markers removed after verifying zero live executors; `01a1cb8b` removed + its job resubmitted as `01a1cbb7` (which then queued normally, confirming the wedge is spawn-time, not admission-time).
- Root-cause read: `spawn_executor` / `schedule_jobs_inner` / `cancel_job` in `crates/agent_mail/src/jobs.rs` at `620d3be29c`.
