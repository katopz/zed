# 081 · 4090: rebuild agent-mail for jobs-aware quiet (rig verdict → facts fix)

- [ ] Rebuild `agent_mail` on the 4090 and replace the installed binary (unblocks nothing else)

**Status:** open · amo has the fix landing in `29921a0ba2` (2026-11-24); 4090 still runs the old binary, so its probe JSON keeps the old quiet semantics (`quiet=true` while a managed job runs) until this lands. amo's renderers already show the truth from the structured fields (`cpu 169% (load 50.6/30) · jobs 1 run (1 excl) · 5 qd`), but boss/watch routing reading 4090's probe directly still sees the stale `quiet` flag.

## What changed upstream

`crates/agent_mail/src/rig.rs` (commit `29921a0ba2`, develop):

- `probe.quiet` now counts running managed jobs (`quiet_reasons`): QUIET once meant "no unmanaged contenders" — managed jobs are excluded from contenders by design — so a rig deep in an exclusive benchmark read as free everywhere.
- Status/summary lines replaced the QUIET/BUSY verdict with raw facts: `cpu 169% (load 50.6/30) · gpu 15% · jobs 1 run (1 excl) · 5 qd` + `not quiet: <reasons>` when contention exists.
- Benchmark cleanliness is untouched (`contention()`-based, as before).

## Steps (Windows box, PowerShell via ssh or direct session)

1. `schtasks /run /tn gitsync`, poll `E:\git\sync-repos.log` for `SYNC DONE` (brings develop@`29921a0ba2` to `E:\git\zed`).
2. `cd E:\git\zed ; cargo build -p agent_mail --release` (use an isolated target dir if disk is tight).
3. Replace `%USERPROFILE%\.cargo\bin\agent-mail.exe` with `target\release\agent-mail.exe` (safe to swap while agents run — the running processes hold the old inode/handle).
4. Verify: `agent-mail rig probe` prints JSON whose `not_quiet_because` names managed jobs when the queue holds one.
