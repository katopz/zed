# Issue 040 — the `zed rig exec <id>` ghost spawner: someone forks the app binary with fake CLI args on a 70 s cadence

**Status:** OPEN — filed 2026-11-12 (zed-3b0d6444@amo) during the post-fix housekeeping pass. Attribution NOT found; harm mitigated; the remaining question is who forks the editor binary. 2026-11-12 12:20 evidence pass (zed-061faf21@amo): regenerated post-FYI and now known to be lifecycle-bound to a single queued job (see evidence section below).

## Symptom

While job `01a1ccaa-c425` (846 re-pin, exclusive) sat queued on amo, processes kept appearing:

```
/Applications/Zed Dev.app/Contents/MacOS/zed rig exec 01a1ccaa-c425-7a40-89fe-a31aa66197f2
```

- **PPID is always 62690 — the running Zed GUI itself.** No intermediate shell ever appears (agent terminal commands would parent to a `login → zsh` chain; Bash-tool children would parent to the agent runtime pid). The GUI process itself fork+execs the app binary with these args.
- **Cadence ~70 s**, persisting for hours; observed bursts for ids `01a1c8b5`, `01a1c8dd`, `01a1cc95`, `01a1cb8b`, `01a1ccaa-c425` (the queued jobs of the moment). A manual test (`zed rig exec <bogus-id>`) reproduced the shape: the invocation **hangs forever** — with `ReleaseChannel::Dev`, `failed_single_instance_check` is always `false` (`crates/zed/src/main.rs`), so there is no redirect to the running instance and no rig handling: `paths_or_urls` (trailing_var_arg) swallows `rig exec <id>` as three file paths and a full second editor instance launches, sitting near-idle with no windows.
- ~1–2 % CPU / small RSS each, but they accumulate (42 killed in one pass) and they **poison quiet detection** (a spawned editor counts as a contender), which delays every queued exclusive job.

## What it is NOT

- **Not agent-mail.** `spawn_detached` uses `current_exe()` (= the agent-mail binary) — every admitted job ran as `agent-mail rig exec <id>`, verified end-to-end (done records + logs). agent-mail never spawns the zed binary.
- **Not rig_panel** — read-only (status/log/cancel only).
- **Not config** — no task, MCP `context_servers`, `agent_servers`, or keymap entry references the zed binary or `rig exec`; nothing in `~/.config/zed` or `~/Library/Application Support/Zed` mentions `rig exec`.
- **Not a mail instruction** — no mail in any amo inbox contains `rig exec`.
- **Not launchd** — `dev.agent-mail.watch` / `zed-wake` plists and their logs are clean; `AGENT_MAIL_WAKE_COMMAND` unset.

## Suspected origin

An agent thread inside the GUI misreading the pre-fix `holder_alive` doc comment ("a `zed rig exec` runner" — a runner that never existed; corrected in `29b0e20464`) and inventing `zed rig exec <id>` as the way to run a claimed job. The 70 s cadence matches a retry loop; bursts aligned with the queued jobs of the moment. The direct-GUI ppid remains unexplained (a Zed task with shell disabled, or an internal `Command::new(current_exe)`-style path would fit). If someone rediscovers the invocation from inside an agent session, it will regenerate.

## Mitigations in place

1. `29b0e20464` — claim recovery/cancel are age-only past `STARTING_GRACE`; ghost helpers can no longer deadlock the queue or block `rig cancel`.
2. Ghost helpers are kill-on-sight: `pkill -9 -f 'Zed Dev.app.*rig exec'` (they hold no state).
3. Fleet FYI sent (`zed rig exec` is not a command; jobs admit automatically via the kicker).

## Evidence pass 2026-11-12 12:20 +07 (zed-061faf21@amo)

- Ghost pid 66953 found live: spawned **11:40:59**, PPID 62690 (GUI), argv `zed rig exec 01a1cde9-326d-7bb0-bac5-f34619ab6e25`. Killed per kill-on-sight; no ghosts remain.
- Target id decoded (UUIDv7: first 48 bits = ms timestamp): job `01a1cde9` was created ~11:15 +07 — it is the attempt-5 bonsai-decode rematch cell (`agent-mail rig log` confirms: sandbox `/Users/katopz/riir932/sb-amo/` staging, perf_rematch contender gate, REFUSED 04:47:23Z when grafts pid 73096 started mid-run, Issue 1134 F4 fail-fast). The rig log carries no requester line — job owner unattributed.
- **Lifecycle binding (new):** ghost fired ~25 min into the job's queued-wait and the spawner STOPPED when the job left the queued state (~11:41 admission / 11:47 refusal — no ghosts after 11:40:59 in a 35-min window). Model: a loop owned by the job's requester session — `while job is queued: exec zed rig exec <id>; sleep ~70s` — trying to force-start an exclusive job stuck on the quiet gate. It stops on its own; kill-on-sight only cleans the lingering last helper.
- Harm re-check: `rig status` showed amo QUIET with the ghost alive (35+ min old, past `STARTING_GRACE`) — mitigation `29b0e20464` verified live; ghosts no longer delay admission.
- GUI sample (8 s of pid 62690 during a burst-silent window): only standard spawn machinery, no smoking gun. Live attribution still requires `fs_usage`/`log stream` on 62690 during a NEXT burst; the burst trigger is predictable now — watch while any exclusive job sits queued on amo.
- Logs/config still clean: no `rig exec` hits in `~/Library/Logs/Zed/*`.

## Open questions

- [ ] Identify the forker: instrument with `log stream`/`fs_usage` against PID 62690 during the next burst (trigger: an exclusive job queued-waiting on amo), or add a spawn-log line in the GUI's subprocess layer.
- [ ] Consider `Dev`-channel hardening upstream: honoring the single-instance handshake for `Dev` too (or an explicit error for unknown CLI subcommands instead of swallowing them as `paths_or_urls`) would turn this whole class into a clean stderr message.
