# 072 — agent-mail timed wakes never fire while the thread is idle on Windows: no inbox scanner runs, due mail sits in `later/` forever

**Status:** LANDED 2026-11-22 (zed @ amo, this lane) — driver + self-heal + one-shot XML in `e3ce8bf...` (agent_mail crate); validation: 108 lib tests green, clippy clean, Windows compile + live driver install verified on the 4090 (see Verification). The addendum's three open items answered: (a) the short-idle scanner was Zed's live mail bridge (its poller runs `promote_due` for every inbox each tick while a session is live); (b) the long-idle gap is real — with Zed closed (or no live session) nothing scanned; closed by the per-minute driver; (c) the `scheduled`-lister mismatch was the stale installed binary — the current `Mailroom::scheduled` has read `later/` since 5a3a20559b, re-verified on the 4090 after reinstall.

- [x] **T1 — Windows wake driver** (root cause + mechanism confirmed as filed; "never fire" corrected by the addendum: it fired only when a live Zed poller happened to scan).
- [x] **T2 — fix shape:** the repeating-driver option of the sketch, not per-wake one-shots (per-wake schtasks would have recreated 069's residue for wakes): `agent-mail-zed-wake` task runs `agent-mail rig zed-wake --once` every minute forever (`windows_task.rs` `RepeatingTask`, PT1M repetition, no EndBoundary, StartWhenAvailable, battery guards off). Self-heal per the sketch's option B: every scheduled delivery (`Mailroom::deliver` with `deliver_at`, covering `send --at` from any rig AND Zed's in-process `send_agent_mail`) re-arms the task via create/overwrite — create works non-admin, so a spent/deleted driver repairs itself at the next wake arming. `rig zed-wake --install` now arms it on Windows (launchd stays macOS). `zed_wake::tick` promotion is already a side effect of its inbox scan (`pending_names` → `promote_due` per inbox); a missing `AGENT_MAIL_WAKE_COMMAND` is now non-fatal (promote-without-launch instead of an error per tick). `tick`'s live-agents early-return untouched: the driver only backfills the no-poller gap.
- [x] One-shot executor/kicker tasks rebuilt on `windows_task::OneShotTask` XML (StartBoundary=now+5m safety net, EndBoundary=now+73h exec / +8h kick, `DeleteExpiredTaskAfter=PT0S`) — the scheduler deletes registrations itself; the admin-gated end-of-job delete is no longer load-bearing (069's create-time fix reused here per the sketch's XML-builder note). A stray trigger re-fire of `rig exec <id>` on a started job is a no-op (its `.starting` claim file is gone → execute_job errors harmlessly); IgnoreNew blocks overlap with a running instance.
- [x] **T3 — stopgap:** `agent-mail-i635-wakes` left as-is — its one-shot time (07:08Z 11-20) passed long ago, it is spent residue and removal is admin-gated (069 T3's elevated sweep can take it).

## Evidence (2026-11-20, shikuwa, non-admin) — unchanged, kept for the record

- `C:/Users/katop/.agent-mail/inbox/zed-989683dc/later/` held the five parked wakes (epochs verified against UTC); delivered after the stopgap fired, not by the platform.
- `schtasks /query` census: 111 `agent-mail-<uuid>` residue entries, none for wakes; `agent-mail-kicker` Next Run Time `N/A` (spent one-shot from the old `/sc once /st 00:00` shape).
- `agent-mail scheduled --as zed-989683dc` printing `no mail` was the stale installed binary: `Mailroom::scheduled` has read `later/` since 5a3a20559b (2026-10-31); verified listing on the 4090 with the reinstalled binary.

## Verification (2026-11-22)

- amo: `cargo test -p agent_mail` 108/108 (incl. the new windows_task XML tests: window + self-delete on one-shots, repetition + no-expiry on the driver, escaping, UTF-16LE+BOM, boundary format); clippy `-D warnings` clean.
- 4090 (shikuwa): clean-worktree build from origin/develop, `agent-mail rig zed-wake --install` armed the driver, `schtasks /query` shows `agent-mail-zed-wake` Ready with a Next Run Time; `scheduled` lists `later/`.

## Routing

Fleet infra — `crates/agent_mail` is shared; every Windows rig's idle wake is affected. Related: 069 (same crate; its T1/T2 landed in the same commit — the XML builder is shared).
