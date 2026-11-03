# 052 — Agent-mail delivery modes: urgent / steering / queued (issue 046)

Status: DONE — all gates green (agent_mail 41 + agent 832 tests, clippy clean, CLI e2e smoke verified)
**Claimed:** zed-agent@amo, 1793732249, expires 2026-11-06T13:00:00+00:00 (issue 046 carries the lease)

## Goal

`agent-mail send <addr> --mode urgent|steering|queued` (issue 046, filed by
zed-73caa0d7@m3 from the owner's 2026-11-03 handoff incident): a sending agent
chooses how its mail behaves at a mid-turn recipient.

| mode | semantics |
|---|---|
| `urgent` | preempts the recipient's running turn (cancel, then deliver — the shape of the owner typing) |
| `steering` | the recipient's turn ends at the next message boundary, then the mail delivers (Zed's own steering semantics) |
| `queued` (default) | today's behavior — lands when the current turn ends |

Plus: `*@rig` receipts name the resolved recipient (deliver-time resolution via
a liveness registry the Zed poller writes), and `rig run --notify` gains the flag.

## Design

- Wire: `Envelope.mode: MailMode` (`#[serde(default)]`, skipped when queued —
  old senders/receivers stay compatible both ways; serde default reads old
  payloads as queued).
- Resolution of `*@rig` moves from claim-time roulette to deliver-time
  preference: `deliver` consults `~/.agent-mail/live/<name>.json` entries the
  native poller refreshes (status idle/generating, kind thread/subagent, ts),
  ranks idle-threads > idle-subagents > busy-threads > busy-subagents > no
  fresh entry -> the `_any` inbox (claim-time, today's behavior). The resolved
  name rides back to the sender on a `resolved-to <name>` line printed before
  the unchanged `delivered <id>` ssh marker (ssh() returns pre-marker lines).
- Zed inbound (`post_agent_mail`): idle -> deliver now; busy+steering ->
  `Thread::set_end_turn_at_next_boundary(true)` + mailbox (the existing
  `Stopped`-event flow delivers); busy+urgent -> `Thread::cancel()`, wait for
  idle, deliver with a `mode=urgent` preemption note in the prompt
  (`PostedMessage::Preempting` carries a completion receiver so local
  `send_message`'s `wait_for_reply` still works).
- Local `send_message` gains the same `mode` input and dispatch
  (`post_thread_message`), so urgent/steering work for same-machine targets too.
- The native poller refreshes `live/` entries each ~5 s tick and prunes stale
  files; entries go stale (15 s) when Zed dies, so resolution degrades to `_any`.

## Tasks

- [x] `agent_mail`: `MailMode` type + `Envelope.mode` + compose untouched (callers set it), serde default/round-trip
- [x] `agent_mail`: live registry (`live/<name>.json`, freshness, ranking) + `deliver` resolution + `resolved-to` line + `SendReport.resolved_to`
- [x] `agent_mail` CLI: `send --mode`, receipt echoes mode + resolved recipient, usage text
- [x] `agent_mail` jobs: `rig run --notify-mode` threaded into job mail
- [x] `agent_mail` tests: mode round-trip, resolution ranking + staleness + `_any` fallback, deliver files into the resolved inbox
- [x] Zed `thread_mailbox`: `MailboxMessage.urgent`, prompt note, `PostedMessage::Preempting { completion }`, `ThreadMessageRequest.mode`, `Mailed { mode, resolved_to }`
- [x] Zed bridge: poller writes/prunes `live/` entries; `post_agent_mail` three-way dispatch; `send_agent_mail` carries mode + receipt
- [x] `agent_fork`: `Preempting` arm (wait_for_reply composes)
- [x] `send_message_tool`: `mode` input + doc comment
- [x] Zed tests: urgent busy (preempts) + idle; steering busy (boundary) + idle (plain); queued unchanged (existing test)
- [x] gates: `cargo clippy -p agent_mail -p agent` clean; `cargo test -p agent -- thread_mailbox` + agent_mail unit tests green
- [x] docs: issue 046 status + plan checkboxes; commit + push

## Notes

- Subagent-claim race for `*@rig` (the idle-audit-subagent footgun) is fixed by
  idle-thread-first ranking, not by extending the native poller to claim `_any`.
- Claude sessions keep `_any` claiming; rigs without a fresh Zed registry
  resolve exactly as today.
- Broadcasts (`all@rig`) carry the mode per copy — an urgent broadcast preempts
  every live thread, which is what the sender asked for.
