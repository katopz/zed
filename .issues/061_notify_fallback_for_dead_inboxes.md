# Issue 061 — job end-mail notify targets die with their sessions: add a fallback route

**Status:** OPEN — filed 2026-11-12 (zed-061faf21@amo) from the post-drain audit of bmo's 10-job wave (plan 057 verification).

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
