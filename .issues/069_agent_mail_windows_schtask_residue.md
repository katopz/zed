# Issue 069: agent-mail Windows one-shot schtasks pile up — the self-delete silently fails (admin-gated) and nothing sweeps the residue

**Status:** T1+T2 LANDED 2026-11-22 (zed @ amo, this lane): `windows_task.rs` authors the XML task builder properly in the main tree (the 4090 clone's uncommitted sketch never compiled — E0425 + format!-arg-order, see the note below), `spawn_detached` Windows path now creates self-expiring one-shots (StartBoundary=now+5m safety net, EndBoundary=+8h kick / +73h exec, `DeleteExpiredTaskAfter=PT0S` — the scheduler deletes the registration itself, no admin token), and the end-of-job delete failure is logged loudly instead of swallowed. Validation on the 4090: clean worktree `E:\scratch\zed-fix069` @ `0f7f493ce6` — agent_mail 99+6 tests green on Windows, release build installed to `~\.cargo\bin\agent-mail.exe` (2026-11-23 08:12 +07); every executor/kicker task from now on self-expires, so the pile-up is stopped at the source and residue stop-date = max 73 h after the last old-shape task. T3 (the 111 existing residue entries) still needs one elevated sweep — owner-gated.
**Claimed:** shikuwa-4090-i1223rec, 1794990000, expires 2026-11-20T10:00:00+00:00 (lapsed — lease expired; T1/T2 superseded by the main-tree landing, T3 open)

- [x] T1 — create-time fix LANDED (supersedes the 4090 clone's non-compiling WIP — do not rebase that working tree, discard it): `windows_task()` (as `windows_task::OneShotTask::xml` + `create_task`) in `crates/agent_mail/src/windows_task.rs` builds the task definition (UTF-16LE+BOM, `schtasks /create /xml`) with the same principal/settings the old `/sc once /st 00:00` incantation produced (InteractiveToken/LeastPrivilege, battery guards, PT72H limit, IgnoreNew) plus StartBoundary=now+5m, EndBoundary per action (+73h executor / +8h kicker), `DeleteExpiredTaskAfter=PT0S` — unit-tested in-tree (`windows_task::tests`, runs on every OS), compiled on Windows from a clean worktree on the 4090.
- [x] T2 — LANDED same commit: the end-of-job delete failure now logs loudly (`[jobs] schtasks /delete … failed: <status/error>; the task self-expires via its EndBoundary`, jobs.rs execute_job) instead of the swallowed `.ok()`; the timed-wake/job trigger carries an EndBoundary + ExpiredTaskDeleteAfter so the scheduler removes the entry itself — no admin token needed.
- [ ] T3 — box residue (shikuwa/4090): 109 dead `\agent-mail-<uuid>` one-shot tasks (verified: action `agent-mail.exe rig exec <id>`, Last TaskResult 0, NextRunTime none — delivered mail, pure entry residue, a few KB each) + the spent `agent-mail-i635-wakes` stopgap from 072. New-shape tasks (landed) self-expire; these old ones never do. Deletion is admin-gated (`C:\Windows\System32\Tasks\` entries are Administrators-owned; `katop` has read-only). Run from an ELEVATED PowerShell:
- [ ] T3 — box residue (shikuwa/4090): 109 dead `\agent-mail-<uuid>` one-shot tasks (verified: action `agent-mail.exe rig exec <id>`, Last TaskResult 0, NextRunTime none — delivered mail, pure entry residue, a few KB each). Deletion is admin-gated (`C:\Windows\System32\Tasks\` entries are Administrators-owned; `katop` has read-only). Run from an ELEVATED PowerShell:

```powershell
Get-ScheduledTask 'agent-mail-*' | Where-Object { $_.TaskName -ne 'agent-mail-kicker' -and $_.State -ne 'Running' } | ForEach-Object { $i = $_ | Get-ScheduledTaskInfo; if ($i.LastTaskResult -eq 0 -and -not $i.NextRunTime) { $_ } } | Unregister-ScheduledTask -Confirm:$false
```

(The filter skips the live job's task, `agent-mail-kicker` — agent-mail's own trigger, not residue — and anything pending/failed.)

## Evidence (2026-11-18, shikuwa)

- `schtasks /query` census: 490 tasks total, **111** `agent-mail-<uuid>` entries; verbose samples show Schedule Type "One Time Only", Last Result 0, Next Run Time N/A.
- Every delete attempt (PowerShell `Unregister-ScheduledTask`, `schtasks /delete`) returns Access denied under the normal token; create/overwrite works — only delete is refused.
- Why they accumulated: agent-mail tries the end-of-job delete and hides the error, so the same failed delete repeats for every timed wake on this box.
- The verdict-lane record for this cleanup (disk-relief batch, session `e85d62ff-1041-4c9a-be30-3c53fcc5770a`): E:\tmp scratch deleted (~11.7 GB freed), the task sweep blocked on admin, this issue filed for the create-time fix.

## Route

Fleet infra (`crates/agent_mail` is shared by every rig) — the fix benefits all Windows boxes. Owned by this lane's post-run window (claim above); any Windows-box lane may take it earlier by re-claiming with a fresh lease (push is the arbiter).

**Note 2026-11-22 (katgpt-rs-8d@amo):** the T1/T2 working-tree edit in the 4090 clone `E:\git\zed` does NOT compile — `cargo build --release -p agent_mail` there fails in `jobs.rs` (`windows_task_xml` called as a free fn but not in scope, E0425; a `let` inside a `format!` arg list → "positional arguments cannot follow named arguments", ~line 837–891). It is uncommitted WIP (not on origin/develop), so any build in that clone breaks: the 4090 agent-mail install of zed 076 + sprint!087 had to build from a clean `origin/develop` worktree instead (`E:\scratch\zed-am`). Fix or stash-to-branch the WIP before the next in-clone build.
