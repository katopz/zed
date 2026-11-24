# Issue 080 — CLI-scheduled timed wakes (`agent-mail send --at`) arm neither the sidebar schedule icon nor the keep-alive pin

**Status:** OPEN 2026-11-24 (zed-35c73065@m3) — filed from the sprint!071 lane: two self-wakes were CLI-scheduled, and the owner noticed the thread history row showed no schedule icon while the thread was parked waiting for them (owner report, same day).

## Symptom

A Zed native thread ends its turn after scheduling a wake through the CLI lane — `agent-mail send --from <own-inbox>@<rig> --to <own-inbox>@<rig> --at "..." "..."` run in the terminal tool. The thread is genuinely parked waiting for its own scheduled mail, but:

- The sidebar thread row shows **Completed** — no Waiting icon — for as long as the wake is pending (hours).
- `list_threads` reports `queued_messages: 0`; Zed has no knowledge of the pending wake at all.
- `agent-mail scheduled --as <inbox>` (outside Zed) shows the wake correctly queued — the spool knows, the editor does not.

The icon machinery EXISTS and works on the other path: `AgentThreadStatus::Waiting` renders `IconName::CountdownTimer` + "Waiting for mail (reply or scheduled wake)" on the metadata line. It just never arms for CLI-scheduled mail.

## Root cause

The waiting-for-mail state is armed in exactly ONE place — the agent-mail bridge's **send** path, which runs only for mails sent through the native `send_message` TOOL:

```rust
// crates/agent/src/agent_mail_bridge.rs:823-838
let expects_mail_back = request.wait_for_reply || request.deliver_at.is_some();
...
acp_thread.update(cx, |thread, cx| {
    thread.set_waiting_for_mail(expects_mail_back, cx)
});
if expects_mail_back {
    agent.pin_waiting_thread(&sender, acp_thread);
}
```

The CLI lane is a subprocess writing to the agent-mail spool directly (`~/.agent-mail`); the bridge never sees it, so:

1. `set_waiting_for_mail(true)` is never called → the sidebar maps Idle straight to Completed (`crates/sidebar/src/sidebar.rs:8805-8817` — the `ThreadStatus::Idle if thread.is_waiting_for_mail() => AgentThreadStatus::Waiting` arm never fires).
2. `pin_waiting_thread` is never called → **no keep-alive pin** (the issue-075 machinery). The poller's `waiting_pins.retain(...)` loop (`agent_mail_bridge.rs:109`) only holds sessions it pinned. If the thread's view closes and the session drops, the wake lands in an inbox no live session claims — **the wake silently never fires** (orphaned mail), which is worse than the missing icon.

The CLI lane is the house-DOCUMENTED workflow — the workspace AGENTS.md instructs exactly this shape ("Timed wake instead of sleep/poll: `agent-mail send --from <your-inbox>@<rig> --to <your-inbox>@<rig> --at 23:45 "check 1022"`, then END your turn — mail lands and wakes you"), so every documented self-wake produced from a terminal hits both halves of the gap.

## Evidence (file:line at this writing)

- Waiting variant + its doc ("parked waiting for an external event (an agent-mail reply or a scheduled wake)"): `crates/ui/src/components/ai/thread_item.rs:19-23`; the CountdownTimer render: `thread_item.rs:786-794`.
- Sidebar mapping: `crates/sidebar/src/sidebar.rs:8805-8817` (`Idle if is_waiting_for_mail() => Waiting`, else Idle => Completed); test vocabulary "waiting-for-mail": `crates/sidebar/src/sidebar_tests.rs:718`.
- Sole arming site: `crates/agent/src/agent_mail_bridge.rs:823` (`expects_mail_back`), `:832` (`set_waiting_for_mail`), `:835-837` (`pin_waiting_thread`); the single clear site `:673` (`post_agent_mail` claim). No other callers of either (`grep set_waiting_for_mail crates/agent/src` → bridge 673/832 + tests only).
- Pin retention: `agent_mail_bridge.rs:109` (`waiting_pins.retain(... is_waiting_for_mail() || status != Idle)`).
- The tool path's own docs promise the behavior: `crates/agent/src/tools/send_message_tool.rs:21-23` ("To wake yourself at a time ... send to your own `mail_address` ... with `deliver_at` and end your turn").
- Repro: in a native thread, terminal-run `agent-mail send --from <thread-inbox>@<rig> --to <thread-inbox>@<rig> --at "1h" ping`, end the turn → row reads Completed (no CountdownTimer), `queued_messages: 0`. Compare: `send_message` with `deliver_at` → the icon arms and the pin holds.

## Proposed fix — (a) the poller reconciles the spool (preferred)

The bridge poller already loops per `MAIL_POLL_INTERVAL` tick over every live session's inbox. Add a cheap reconcile leg (every ~12 ticks ≈ once a minute, the `flush_outbox` cadence):

- One spool read (a shared `Mailroom::scheduled()` over `~/.agent-mail`, or one `agent-mail scheduled --as <inbox>` subprocess per reconcile window — same cost class as the existing outbox flush).
- For each live session: if any pending row addresses that session's inbox with a future `deliver_at`, arm `set_waiting_for_mail(true)` + `pin_waiting_thread`. The existing claim path (`post_agent_mail`, `:673`) already disarms when the wake lands; the retain loop then prunes the pin on the tick after delivery.
- The sidebar needs NO change — it renders Waiting correctly once the flag arms.

## Proposed fix — (b) docs-only fallback

Change the workspace AGENTS.md timed-wake rows to mandate the `send_message{deliver_at}` tool path for self-wakes and demote the CLI lane. Rejected as the primary fix: plain-terminal sessions (no Zed tool) lose timed wakes entirely, and the AGENTS.md text across the fleet's 30+ repos would all need the same edit — (a) fixes every lane at once.

## Tasks

- [ ] Fix (a): the reconcile leg in the poller, arms flag + pin on CLI-scheduled self-mail, disarms via the existing claim path.
- [ ] Tests both directions: a CLI-scheduled wake arms Waiting + the pin (spool fixture row, future `deliver_at`); the pin survives a view close until delivery; delivery clears flag + pin on the next tick.
- [ ] Sidebar: expected untouched (assert the existing Waiting render covers the armed path).
- [ ] If (b) ever chosen instead: the workspace AGENTS.md timed-wake rows change in the same commit as the demotion, never separately.
