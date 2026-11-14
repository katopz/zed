# 065 — amo: one-command zed dev hot-swap (census → rebuild → snapshot → kill → swap → relaunch → resume native+claude+boss) + spawn threads into a chosen workspace

Status: open (2026-11-14) — owner-directed ("currently we do this manually, i think we can automated this"); routed `boss@amo`; unclaimed; executing lanes fresh-context only (never a near-limit lane)

**The relation to 063:** this is the unlock 063 is parked on. 063's fix is an owner-timed restart precisely because the manual restart "kills 14 live fleet threads" — with the automated census → snapshot → resume cycle below, that objection dissolves: restart becomes a routine command that carries every live thread across. **The relation to 064:** the census half extends the same live-registry view. **Context from the seal fleet (cross-repo, not a dependency):** the workspace-scope routing failure (three consecutive yields on seal-sprints sprint!047 — a zed-scoped lane spawned a carrier, the carrier inherited zed scope, riir-ai work needs riir-ai scope) is the concrete instance of what spawn-into-workspace fixes.

## The operator loop today (manual)

When the zed-scoped lanes on amo modify the zed editor code, a human does it by hand:

1. rebuild the zed dev dmg;
2. work out which agents are running (native agent threads, Claude sessions, the boss agent);
3. kill the zed dev process;
4. replace the dmg;
5. reopen zed dev;
6. resume the previously running sessions — native and claude agents, and the boss agent.

This is the same loop 063 needs (restart at a quiet window) and the same loop every "the running binary is stale" moment needs (063's root cause: the running process predates the on-disk build — see 063's inode evidence).

## Required

1. **Agent census.** Know what is running BEFORE touching anything: zed dev instances (pid + mapped binary inode, the 063 verification shape), every native agent thread (workspace + thread id + title), every Claude session, the boss lane. `agent-mail rig status amo` + `~/.agent-mail/live/` are the existing partial censuses — extend, don't duplicate. The snapshot lands on disk before anything is killed (the snapshot IS the recovery artifact).
2. **One-command hot-swap.** Rebuild dev dmg → snapshot roster → kill zed dev → swap dmg → relaunch → resume every snapshotted session: native threads through zed's session resume, Claude sessions through their resume path, the boss lane back with its mail inbox (`boss@amo`) intact.
3. **Post-resume liveness verdict.** Each resumed thread answers a probe (the 063 self-executing probe-mail pattern is the precedent); the run prints the resumed-roster verdict and names anything that did not come back.
4. **Spawn-into-workspace (zed editor mod).** zed should allow spawning an agent thread into a CHOSEN workspace — native and claude — instead of every thread inheriting the spawning session's workspace. The plan-053 machinery (`agent_mail_spawn_threads`, 063) already spawns from mail; the mod adds a target-workspace parameter to the spawn path (settings/env/mail-body field — implementer's choice per the repo's conventions) so the boss and any lane can get "a thread scoped to `<repo>`" without a human opening the right window.

## Why it matters (owner's words, 2026-11-14)

- The scope routing failure recurs across the fleet (the sprint!047 class): lanes keep landing with the wrong workspace and yielding.
- Carriers die of context exhaustion mid-coordination (one answered at >320k this session) — **resume is the normal path, not a recovery**, and hot-swap must never lose a live thread.
- The swap loop is manual today; it should be one command.

## Constraints

- Never kill before the roster snapshot is on disk.
- Executing lanes for THIS work are fresh-context spawns — never the near-limit lanes.
- The boss inbox (`boss@amo`) must survive the swap: it is the fleet's router of record.
- The automation lives beside the fleet tooling (`agent-mail` family), not inside seal-* repos.

## Acceptance

1. One command on amo performs the full cycle — census → rebuild → snapshot → kill → swap → relaunch → resume — with zero manual steps, and prints the resumed-roster verdict.
2. A boss-spawned thread lands in a NAMED workspace (riir-ai as the working example; katgpt-rs as the owner's cited example) and can act in that workspace without a human opening a window — proven by a round-trip on a scratch item.
3. When both land: fire 063's pending restart through the new command (its probe mail verifies the plan-053 spawn on the fresh binary), then remove 063 per its own instruction.
