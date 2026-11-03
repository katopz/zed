# Issue 046 — agent-mail delivery modes: urgent (inject now) / steering (steer + wait) / queued (today's busy behavior)

Status: OPEN — filed 2026-11-03 by zed-73caa0d7@m3 (owner-reported reproduction). Extends plan 043's poller (`.plans/043_cross_rig_agent_mail.md` phase 2).
**Claimed:** zed-agent@amo, 1793732249, expires 2026-11-06T13:00:00+00:00

## Observed (the reproduction)

A cross-rig handoff mail sent to a RUNNING native-agent thread (`zed-fb9a28a0@m3`, mid-turn, `status: generating`) produced **no visible delivery** — the owner watched that agent's window and saw nothing. The mail surfaced only after the owner **manually stopped the thread** (a follow-up probe delivered normally once the thread was stopped/idle). This is plan 043 phase 2's documented busy behavior ("idle → new turn, busy → after the turn ends") — correct as far as it goes, but there is no way for a SENDING agent to choose anything else. A remote coordinator cannot:

- get time-sensitive information into a running agent's CURRENT turn (today: it silently queues until the turn ends — which can be many minutes on a long lane), or
- even know its mail is queued (the sender sees `delivery: "mailed"` with no per-thread queue state).

Secondary routing footgun observed in the same arc: `*@rig` routes to the first **IDLE** agent on the rig — a handoff aimed at "the agent working on m3" landed in an idle audit subagent's inbox instead of the active thread. At minimum the send output should say WHICH agent accepted a `*@rig` mail.

## Requested

A delivery-mode axis on `agent-mail send` (and `rig run --notify`), surfacing the three behaviors the Zed side already has primitives for:

| mode | semantics | Zed-side mechanics |
|---|---|---|
| `urgent` | inject NOW, like an owner typing at the agent — PREEMPTS the running turn (the owner's "user send msg, not steering": a new turn starts immediately, the current stream is interrupted) | the poller posts to the plan-035 mailbox with the new-turn path REGARDLESS of thread state, and the active turn is cancelled/superseded — the exact shape of a user message arriving mid-stream |
| `steering` | deliver INTO the running turn and wait for the stream to end (Zed's steering semantics) | the poller posts as steering input on the busy thread; on an idle thread it degrades to a plain new turn |
| `queued` (name TBD — alternatives: `usual`, `turn-end`, `patient`) | today's behavior: lands when the thread's current turn ends | plan 043 phase 2 verbatim; the DEFAULT (back-compat) |

Wire shape: `agent-mail send <addr> --mode urgent|steering|queued "..."` (default `queued`); the `deliver` payload carries the mode through the plan-043 poller to the mailbox post; `rig run --notify` gains the same flag. The sender's receipt should echo the resolved mode +, for `*@rig`, the resolved recipient name.

## Acceptance

1. `--mode urgent` to a mid-turn thread starts a new turn immediately (the running stream stops or is superseded — same as the owner typing), with the pre-emption named in the thread (not a silent swallow).
2. `--mode steering` to a mid-turn thread lands as steering input; the SENDER can wait for the turn to end (`send_message`'s existing `wait_for_reply` composes with it).
3. `--mode queued` (default) is byte-identical to today's busy behavior.
4. A `queued` mail is DISCLOSED: the recipient thread's inbox shows the pending count (or the sender's receipt names the queue state) — the "did it pass through?" confusion this issue was filed from must stay answerable.
5. `*@rig` sends report the resolved recipient agent in the delivery receipt.
6. Tests mirror plan 043 phase 2's (`test_agent_mail_for_busy_thread_waits_for_turn_end` family): one per mode, busy + idle postures.

## Notes

- The owner's verdict on the reproduction: "not a bug — the agent was running; the mail shows when the thread stops" — the feature ask is the MODE, not a fix to the default.
- The m3 incident this came from: a seal-remake rebase + tutorial-quest e2e handoff (2026-11-03) that sat unseen while the target lane ran; the owner had to relay it by hand.
- Related surface, same repo: `agent-mail` itself was built here (plan 043, commit `5a9284e93`); the CLI lives beside the poller, so both halves of the mode axis are in this repo's hands.
