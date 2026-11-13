# Issue 061 — job end-mail notify targets die with their sessions: add a fallback route

**Status:** LANDED 2026-11-13 (zed-02f4db55@amo, zed `__LANDED_HASH__`) — all three fix-direction arms + the owner addendum:
1. **Delivery fallback** (`jobs.rs` `notify_target_lives` + `job_mail`): a job end-mail addressed to an inbox that has NEVER existed on the target rig routes to `jobs-archive@<rig>` with a `[notify fallback: ...]` body disclosure; local liveness = the inbox dir exists, remote = a `test -d` probe over the rig's own SSH routes (first hit wins; a dark rig delivers as addressed — the outbox retry owns reachability). An existing inbox delivers as addressed even when its session is dead (the boss sweep's known food — never silently reroute a real addressee's history).
2. **notify=None warning** (`main.rs` `rig run`): the whoami-default failing now prints a loud `nobody will be mailed` warning at submission.
3. **Owner addendum** (`boss.rs` `housekeeping_prompt`): checklist item 9 — the `~/scratch`/`/tmp` leftover sweep (live-lane cwd keeps it, dead lanes listed for deletion) + the report-only disk audit.
Tests: agent_mail 90/0 + 6/0, clippy 0 warnings.
**Lease closed:** zed-061faf21@amo's claim (exp 2026-11-13T07:50Z) transferred per the standing order; re-claimed **Claimed:** zed-02f4db55@amo, 1794582900, expires 2026-11-13T18:35:00+00:00 — executed to LANDED this session.

## The failure class

Every job in the wave ran to a terminal outcome, but the notification layer
did not survive session death:

- `6a300a3e` (the 26 h i1117 trio, exit 0) notified `zed-95d14db9@m3` — an
  inbox that **does not exist** on m3's agent-mail; the end-mail went nowhere.
  The lane only continued because its lease chain re-took it
  (zed-d565d322@m3 now holds the T4 record claim).
- `c8309173` (canary, exit 0) notified `zed-9665f823@amo` — session-less; the
  end-mail sits unread in `cur/` forever.
- `d6761926` / `178ba44d` (worktree-scoped, instant-fail) had **notify=None** —
  nobody was ever told; discovered only by a manual audit.
- Three MORE notifications of the same class exist rig-wide (t1's five mails,
  t2b's job notices) — consumed only because the boss sweep forced a manual
  drain.

Result: work completes but the loop never closes; the boss stale-mail sweep
keeps re-firing inboxes nobody will read; results sit unprocessed until a
human or a housekeeping run stumbles on them.

## Fix direction

1. **Delivery fallback**: when a notify target's inbox is unreachable or
   unknown on the target rig, fall back to a rig-level catch-all
   (`jobs-archive@<rig>` or `boss@<rig>`) instead of dropping the mail —
   and say so in the mail body ("original addressee unreachable").
2. **notify=None is a spec bug, not a default**: `rig run` should warn (or
   refuse without `--no-notify`) when a job is submitted with no notify
   target — silent rot is never intended.
3. **Liveness check at delivery**: mail to an inbox with no live session
   locally could be flagged in the mail itself (the sender rig knows the
   addressee session list at delivery time) so the recipient-side sweep can
   triage faster.

## Scope notes

- `agent-mail` crate + the executor end path (where end-mails are composed).
- Mixed-version safe: the fallback is sender-side behavior only; receivers
  need nothing new.
- The claim-lease protocol already catches the WORK side (expired lease =
  re-claimable); this issue is about the NOTIFICATION side so results are
  seen without waiting for a sweep.
- **Owner addendum 2026-11-12:** implement together with the housekeeping-contract
  amendment (boss.rs `housekeeping_prompt`): add a scratch//tmp leftover sweep
  + a report-only disk-audit line item so the hourly lane sees `~/scratch`
  grids and `/tmp` lane debris that the `~/git`-scanned checklist never
  touches.
