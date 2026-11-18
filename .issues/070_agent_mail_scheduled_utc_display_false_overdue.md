# 070 — agent-mail scheduled display shows UTC-only times; a due wake reads past-due on a +0700 box (diagnosis record: the wake pipeline itself verified healthy)

Status: open (2026-11-18, zed-eb3b7571@amo) — no functional defect found; one display papercut + one documented silent-sit edge

**Trigger:** the owner asked "the wake is bug? why it not wake anyone at all even time pass by?" — a plan-019 T4p adjudication wake armed `scheduled_for="2026-11-18T22:20:00+00:00"` was read against the LOCAL wall clock (23:xx +0700) and looked 1+ h overdue. It was 5.5 h in the future.

## Verified-healthy wake pipeline (amo, 2026-11-18 16:20-17:1xZ)

- **Delivery:** scheduled mail sits at `~/.agent-mail/inbox/<name>/later/<due_ms>_<id>.json`; Zed's bridge poller (500 ms tick, `agent_mail_bridge.rs` MAIL_POLL_INTERVAL) calls `pending_files()` -> `promote_due()` (agent_mail.rs:811) which renames due files `later/` -> `new/`; `claim()` -> `post_agent_mail()` starts the turn. No scheduler process needed (the doc comment names this design: "whatever already polls the inbox ... delivers it on time").
- **Empirical census:** 5 scheduled mails pending, all future-dated; **0 past-due files stuck in later/**; **265 claimed `cur/` envelopes carrying `deliver_at`** — the pipeline has fired hundreds of times on this box.
- **Fallback (Zed closed):** `dev.agent-mail.zed-wake` launchd job (60 s tick, LastExitStatus 0, log mtime fresh) — `tick()` requires a FRESH live entry to skip (`live_agents()` filters by LIVE_FRESH, so stale entries after a crash do NOT block the launch) and `open -ga "Zed Dev"` + AGENT_MAIL_WAKE_COMMAND override.
- **Sleep:** `pmset -g sleep` = 0 (sleep prevented by zed/powerd/coreaudiod/RustDesk) — overnight wakes fire on time on amo.
- **Closed-inbox wakes** (`zed-*` inbox with no live session): spawn path skips `zed-*` by design; the RESUME path (`discover_resume_targets`, `eb06bedf77` 2026-11-05) targets exactly those, looks the thread up in `threads.db` (parent_id NULL required) and reopens it — gates: `agent_mail_spawn_threads` true (settings L179), donor session with a project, `pinned_threads` < MAX_MAIL_SPAWNED_THREADS (8). The running binary satisfies the lineage (built Nov 13 23:46 > Nov 5 landing; process started Nov 14 21:18 > build — issue 063's stale-binary class is fixed).

## Finding 1 (papercut, this issue's ask): UTC-only display

`agent-mail scheduled` prints `scheduled_for="2026-11-18T22:20:00+00:00"` and the envelope header carries UTC; on a +0700 box "22:20" reads past-due against a 23:xx local clock. A due-in-5.5-h wake generated a false bug report today. Ask: print local time alongside UTC in `agent-mail scheduled` output (and ideally in the M-box envelope header render, `mail_prompt.rs`).

## Finding 2 (edge, document-only): silent-sit window for closed-inbox wakes

A `zed-*` orphan wake whose resume gates ALL fail (no donor native session with a project / thread row absent from threads.db / pinned cap at 8) is promoted to `new/` and then claimed by nobody — it sits silently with no incident signal (the `log::error!` arms cover only post-claim failures). Low priority: every gate was satisfied in the observed posture, and the orchestrator watchdog's unread-mail lane is the existing backstop. If a hardening pass ever touches this lane, a watchdog incident for "orphan wake unclaimed > N h" would close the loop.

## Verification recipe (for the next report of this shape)

1. `date -u` vs the envelope's `scheduled_for` — compare in the SAME timezone (+00:00 suffix).
2. `ls ~/.agent-mail/inbox/*/later/` — filename prefix is due-epoch-ms; anything with prefix < now is a MISSED delivery (none found here).
3. `grep -l deliver_at ~/.agent-mail/inbox/*/cur/*.json | wc -l` — historical fired-scheduled count (265 here).
4. `launchctl list dev.agent-mail.zed-wake` + `tail ~/.agent-mail/watch/zed-wake.log` — fallback aliveness.
5. `pmset -g sleep` — a sleeping box defers delivery to wake-up (late, never lost: promote_due compares due <= now).
