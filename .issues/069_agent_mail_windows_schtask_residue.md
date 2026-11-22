# Issue 069: agent-mail Windows one-shot schtasks pile up — the self-delete silently fails (admin-gated) and nothing sweeps the residue

**Status:** open
Implementation deferred to this lane's post-run window (the i1223 deciding run holds the box Exclusive until ~2026-11-19; zero-cargo posture — code work waits for the rig to go quiet):
**Claimed:** shikuwa-4090-i1223rec, 1794990000, expires 2026-11-20T10:00:00+00:00

- [-] T1 — create-time fix: AUTHORED 2026-11-18 (working tree, uncommitted): `windows_task_xml()` in `crates/agent_mail/src/jobs.rs` builds the task definition (UTF-16LE+BOM, `schtasks /create /xml`) with the same principal/settings the old `/sc once /st 00:00` incantation produced (InteractiveToken/LeastPrivilege, battery guards, PT72H limit, IgnoreNew) plus StartBoundary=now+1m, EndBoundary=now+16m, `DeleteExpiredTaskAfter=PT0S` — validated in-shell by a throwaway probe task (settings registered, scheduler-side expiry armed; UTC `Z` boundaries normalize to local). COMPILE+TEST+COMMIT PENDING the exclusive-window end (zero-cargo posture on this box until ~2026-11-19T12:30Z; never push unverified code on the fleet-critical spawn path).
- [-] T2 — AUTHORED same session: the end-of-job delete failure now logs loudly (`[jobs] schtasks /delete … failed`) instead of the swallowed `.ok()`; also pending compile+commit. the timed-wake trigger is created with no end boundary, so Task Scheduler never expires the one-shot (`crates/agent_mail/src/jobs.rs`, the task-creation path). Set the trigger's `EndBoundary` to the wake time + a grace window and the scheduled-task `ExpiredTaskDeleteAfter` setting so the scheduler removes the entry itself — this works WITHOUT the admin token the end-of-job delete needs.
- [ ] T2 — the end-of-job task delete currently hides its failure (`jobs.rs:1035-1042`): log it loudly (a warn line with the OS error) so boxes like this one surface the residue instead of accumulating it silently.
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
