# Issue 040 — the `zed rig exec <id>` ghost spawner: someone forks the app binary with fake CLI args on a 70 s cadence

**Status:** OPEN — filed 2026-11-12 (zed-3b0d6444@amo) during the post-fix housekeeping pass. Attribution NOT found; harm mitigated; the remaining question is who forks the editor binary.

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

## Open questions

- [ ] Identify the forker: instrument with `log stream`/`fs_usage` against PID 62690 during the next burst, or add a spawn-log line in the GUI's subprocess layer.
- [ ] Consider `Dev`-channel hardening upstream: honoring the single-instance handshake for `Dev` too (or an explicit error for unknown CLI subcommands instead of swallowing them as `paths_or_urls`) would turn this whole class into a clean stderr message.
