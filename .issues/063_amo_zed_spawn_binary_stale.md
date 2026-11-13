# 063 — amo Zed: mail-spawned `[M]` threads (plan 053) never fire — running binary predates the plan-053 build; restart required
Status: open (2026-11-13) — verdict AGREE round 1 (`<session f6591d04>`); owner-timed restart (kills 14 live fleet threads, so not fired autonomously mid-work); unclaimed until a quiet window

**Question that found this:** can others mail `boss@amo` and have the boss spawn a new thread prompted with the task itself? Answer: the machinery exists (plan 053) and is enabled, but amo's running Zed cannot execute it.

## Evidence (all verified 2026-11-13 ~23:00–23:20 +07)

- Setting on: `~/.config/zed/settings.json` → `agent.fork.agent_mail_spawn_threads: true` (L179, nesting verified by parse).
- Bridge + poller alive in the running process: 14 `zed-*` liveness entries in `~/.agent-mail/live/` all refreshed 0 s old (written by the same poller tick that would spawn).
- Gates that could silently block spawn all ruled out: sessions non-empty (14), `pinned_threads` empty (zero `[M]` threads ever created — checked `threads.db`, no `[M]` titles), probe mail was waking (non-fyi) mode, orphan name not `_any`/`zed-*`.
- Live probe: mail to fresh orphan `task-probe@amo` at 23:11:25 sat unclaimed in `new/` 8+ s (~16 poll ticks @500 ms), no live entry, no thread, and zero `agent-mail:` log lines (the bridge logs spawn failures at `agent_mail_bridge.rs` — silence means the spawn branch never ran).
- **Root cause confirmed:** the running process (pid 62690, launched Tue Nov 10 07:00) maps binary inode `34348485` (size 178,549,472, `lsof` txt FD) while the on-disk build containing plan 053 (`eb06bedf77`, landed Nov 5 22:43) is inode `45987587` (size 178,698,464, mtime Nov 13 08:16). Different image: today's capable build has never been loaded. An older binary that doesn't know the `agent_mail_spawn_threads` key serde-defaults it to `false` → `spawn_budget = None` → silent no-spawn, exactly matching every observation.
- Note: the earlier plan-053 round-3 record ("binary mtime Nov 6 00:02") describes the file that was deployed then; the process launched Nov 10 was later overtaken by the Nov 13 rebuild without a restart. The mapped image predates the current build; its exact vintage is confirmable via About dialog at next launch (expect a pre-`eb06bedf77` commit — treat as confirmation, not a blocker).

## Fix (owner-timed)

- [ ] Restart Zed Dev on amo at a quiet window (kills all live threads incl. any generating lanes — hence not done autonomously; the 2026-11-13 verdict agreed defer-to-quiet-window).
- [ ] Post-restart verification (self-executing probe already in place): the deliberate probe mail sits in `~/.agent-mail/inbox/task-probe/new/` — on first boot the poller should claim it within ~1 s and log `agent-mail: spawned thread [M] task-probe as <session_id>` (the expected line, `agent_mail_bridge.rs:560`), open the `[M] task-probe` thread with the probe text as its prompt, and the thread should reply `PROBE-OK` to `orchestrator@amo`. That is the yes/no check; then delete the `task-probe` inbox dir.
- [ ] After verification: mail to `boss@amo` (or any new inbox name) spawns a self-prompted thread per task — plan 053 live on amo. Mark this issue done and remove it.

## Interim (already live, stopgap only — do not close on this)

The plan-056 drain round (deployed 2026-11-13, `02e767737d`) answers boss mail in bounded housekeeping batches via the watchdog (`boss:unread-mail` armed; first fire ~30 min after deploy). It reads-answers-routes mail but does NOT spawn a thread prompted with the task — it is the backstop for when Zed is closed or mid-restart, not a replacement for this fix.
