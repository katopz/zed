# Issue 075 — stopped thread ignores wake mail until the user clicks it: resume named inboxes without the spawn gate, keep waiting threads alive

**Status:** LANDED — filed 2026-11-21 (zed @ amo, user-directed). Reported: a thread that stopped (turn ended waiting on mail) does not accept the wake-up call nor agent's mail until the user clicks the thread in the panel — clicking resurrects the session and the parked mail is then claimed and processed.

## Mechanism (verified in code)

1. Mail delivery keys off LIVE sessions: the poller (`crates/agent/src/agent_mail_bridge.rs` `AgentMailState::start`) claims inboxes only for `agent.sessions`, and a session disappears when its `AcpThread` is released (`agent.rs` `release_session`, fired by `observe_release` — i.e. the panel view closed and nothing else holds a handle).
2. Scheduled wakes (`deliver_at`) do land in the inbox on time for LIVE inboxes (`Mailroom::pending_files` → `promote_due`; `pending_names` scans every inbox each tick, so orphan inboxes surface too) — the mail is there, nothing claims it.
3. The resume path for orphaned `zed-*` inboxes (`discover_resume_targets` → `lookup_saved_threads` → `open_saved_mail_thread`) exists but is gated behind `agent_mail_spawn_threads` (default **false**), the same budget that gates spawning brand-new `[M]` threads. With the default, mail to `zed-<id>@rig` parks forever → the user must click the thread to bring the session back, which is exactly the reported symptom.
4. `set_waiting_for_mail(true)` is set on every outgoing mail but the flag is (a) lost on release (never persisted) and (b) set even for fire-and-forget sends nothing will ever answer — so the sidebar shows "waiting" forever and there is no keep-alive.

## Fix (both of the user's suggested options)

- [x] **Boss-wake (resume without clicking):** decouple resume from `agent_mail_spawn_threads` — a `zed-*` orphan inbox names a specific saved thread, so waking it is what the mail asks for. Spawn budget (unknown-name `[M]` threads) stays opt-in; resume gets its own always-on budget under the same `MAX_MAIL_SPAWNED_THREADS` cap and the same "needs a donor session" gate.
- [x] **Keep-alive on the waiting icon:** new `waiting_pins` map in `AgentMailState`; a thread that sent mail expecting a reply/wake (`wait_for_reply` or `deliver_at`) pins its `AcpThread` so closing the panel view cannot release the session — the poller keeps claiming its inbox and the wake arrives on time. Pin drops when the mail is claimed (`post_agent_mail` clears the flag) and the poller prunes pins whose thread stopped waiting.
- [x] **Truthful waiting flag:** `send_agent_mail` now sets `waiting_for_mail` only when the send actually expects mail back (`wait_for_reply || deliver_at.is_some()`), instead of on every outgoing mail (fire-and-forget senders used to read "waiting" forever).
- [x] Update the `agent_mail_spawn_threads` setting docs in `agent_settings/fork.rs` + `settings_content/agent/fork.rs` (spawning opt-in; `zed-*` resume always-on).
- [x] Tests: resume works without the spawn setting; a waiting thread still receives its scheduled wake after its view handle drops (pre-fix: parked forever); a fire-and-forget sender neither waits nor pins (session releases).

## Notes

- Mail to a `zed-*` inbox with NO matching saved thread still parks (`test_agent_mail_never_spawns_for_zed_names` unchanged); fyi-only batches still never wake anyone; unknown-name `[M]` spawning stays behind `agent_mail_spawn_threads`.
- With zero live sessions there is no donor project, so resume stays parked in that case (unchanged); the keep-alive pin prevents the common "last waiting thread released" case from reaching it.
