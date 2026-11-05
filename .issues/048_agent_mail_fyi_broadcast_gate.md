# Issue 048 — agent-mail fyi mode, broadcast gate, and broadcast liveness cutoff

Status: DONE 2026-11-05 — LANDED `eec5d52d88` (feat: fyi mail mode, broadcast gate, liveness cutoff for agent-mail; fmt sweep `d30a4a3a44`); deployed + gate-verified on m3/amo/bmo/4090; Claude verdict AGREE (session 2b0d02b2, rounds 1 REVISE → 2 AGREE with rollout-note fix → 3 AGREE terminal). Deferred follow-up tracked below. Deployment details: `.docs/037_agent_mail_fyi_rollout.md`.

## Observed (the reproduction)

1. One informational blast (`all@m3`, queued mode) from the amo orchestrator
   woke all five live m3 agents into turns; each replied to the sender —
   5×(turn+reply) token burn for an FYI.
2. Clicking a STOPPED thread in the sidebar resumed it with its old inbox
   name; the poller's ten-minute broadcast lookback then handed it every blast
   that landed while it was away, and the thread started processing the blast
   behind the user's click.

Root causes: `all@rig` had no mode gate (queued = wake everyone), and
`take_broadcasts` gave any cursor-less reader the recent lookback window
without asking whether the reader was live when the blast landed.

## Landed

- `MailMode::fyi` — never wakes an idle recipient; parked, read at the next
  natural turn boundary (Zed: turn-end mailbox path; Claude hook: Stop/waiter
  peek-before-claim leaves it parked, PostToolUse delivers as additionalContext).
- `ensure_broadcast_mode` — `all@rig` + queued refused at the CLI and in
  `send_message`; `--fyi` flag; urgent/steering stay for critical incidents.
- `take_broadcasts_since(reader, cutoff)` + per-session `live_since` (Zed
  poller): a (re)entered thread skips blasts older than its re-entry; the
  cursor advances past skipped ones so they never loop.
- `#[serde(other)]` on queued — unknown modes from newer senders decode as
  boundary delivery instead of failing parse.
- `render()` drops the reply instruction on all-fyi batches ("no reply
  expected"); mailbox titles carry "(fyi — no reply needed)".
- Tests: agent_mail 48/48, agent 834/834 (new: gate matrix, fyi round-trip,
  cutoff+seen-advance, fyi-never-wakes, broadcast cutoff live/re-entered/
  skipped-stays-skipped, Stop-hook fyi parking, unknown-mode fallback, render
  header). Clippy + fmt clean.

## Deferred (open follow-up)

- Fold parked fyi into the running turn's context: today a busy agent spends
  one boundary turn per fyi batch; true folding needs a mid-turn injection
  mechanism (steering-like) and is deliberately left out of the landed change.
- Owner-side: AGENTS.md should document `--mode fyi` for broadcasts (owner-
  owned file); Zed Dev restarts on m3/amo to activate the bridge half.
