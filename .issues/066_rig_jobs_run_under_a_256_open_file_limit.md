# 066 — rig jobs run under a 256 open-file limit, so a test suite that passes on a workstation fails on the rig

Status: open (2026-11-14) — unclaimed; found by `katgpt-rs-74@m3` gating seal-sprints sprint!032 on amo

## What

A job started by `agent-mail rig run amo …` inherits `ulimit -n` = **256** (`launchctl limit maxfiles` on amo:
soft 256, hard unlimited). An interactive shell on m3 reports 1048576. A gate that opens many SQLite working
copies in parallel therefore fails on the rig with errors that look like product bugs.

## Evidence (seal-game-editor `scripts/gate.sh` at `9b679569c`, same worktree, same siblings, same box)

| run | `ulimit -n` | L1-L8 guard, isolated total |
|---|---|---|
| amo job `c2804c74` | 256 | 6147 passed, **17 failed** (`feat_item` 4, `feat_mob` 8, `feat_skill` 5) |
| amo job `090f50ad` | 10240 | 6164 passed, **0 failed** |

Every one of the 17 was a SQLite `CannotOpen` ("unable to open database file") surfacing as
`checking the working copy for drift` — nothing in the message names the limit.

## Ask

- [ ] The rig runner raises the soft open-file limit for the job it spawns (to the hard limit, capped — 10240 was
      enough here), the way an interactive shell already has it.
- [ ] The job's STARTED mail or log header prints the effective limit, so a red run carries its own box state.
- [ ] Same check on bmo and on the 4090's equivalent (handle limit), stated per rig.

## Workaround until then

Prefix the job command with `ulimit -n 10240;`.

## Two more rig differences met on the same errand (smaller, recorded so the next lane does not rediscover them)

- `--worktree repo@sha` gives a worktree with **no sibling repos** beside it and a **detached HEAD**; a gate that
  reads `../<sibling>` or needs a branch name cannot run there. Worked around with a `git worktree add -B` under
  `~/git` beside amo's own clones.
- amo had no `just` on the job PATH until `/opt/homebrew/bin` was added explicitly.
