# 037 — agent-mail fyi mode + broadcast gate rollout

Status: done 2026-11-05 — landed `eec5d52d88` (feat: fyi mail mode, broadcast gate, liveness cutoff) + fmt sweep `d30a4a3a44`; deployed to all four rigs; Claude verdict AGREE (session 2b0d02b2, 3 rounds).

## What changed (why)

Two fleet-cost bugs from the `all@rig` blast path:

1. A queued broadcast woke every live agent into a turn, and every reply came
   back — one advisory cost 5 turns + 5 replies across the fleet.
2. Clicking a STOPPED thread in the sidebar resumed it, and the poller's
   ten-minute broadcast lookback handed it every blast that landed while it
   was away, starting a turn behind the user's click.

Fixes: `MailMode::fyi` (never wakes an idle recipient; parked and read at the
next natural turn boundary), `all@rig` + queued mode refused at the CLI and in
`send_message` (fyi for advisories, urgent/steering only for real incidents),
per-session liveness cutoff (`live_since` + `take_broadcasts_since`) so
re-entered threads skip blasts older than their re-entry, `#[serde(other)]` on
queued so unknown modes from newer senders decode instead of dropping, and
`render()`/mailbox titles stop prompting replies on all-fyi batches.

## Deployment matrix (verified)

| rig | agent-mail CLI | gate verified | Zed Dev app |
|---|---|---|---|
| m3  | installed (cargo install --path crates/agent_mail) | refusal confirmed | rebuilt + installed via `script/bundle-mac -i`; DMG refreshed at `d30a4a3a44` (`target/aarch64-apple-darwin/release/Zed-aarch64.dmg`) |
| amo | installed (git pull + cargo install) | refusal confirmed | rebuilt + installed via `script/bundle-mac -i` over ssh |
| bmo | scp'd binary + `codesign --force --sign -` | refusal confirmed | no zed checkout; n/a |
| 4090 | installed (zed pulled via one-shot schtasks `zedpull`, then cargo install) | refusal confirmed | n/a (Windows) |

Restart Zed Dev on m3 and amo to activate the bridge-side fixes (fyi parking,
liveness cutoff, clicked-stopped-thread guard) — the CLI gate alone does not
cover those.

## Gotchas learned

- **bmo binary transfer**: a scp'd macOS binary is SIGKILLed (rc 137) on Apple
  Silicon until re-signed — `codesign --force --sign -` on the target fixes it.
- **4090 sync contract**: `zed` was not in `E:\git\sync-repos.cmd`'s repo list,
  so its checkout lagged silently. Pull via a one-shot schtasks task (in-sshd
  `git pull` still hangs); adding zed to the contract list keeps it fresh.
- **ssh pipes on bmo**: `cmd | grep` over ssh returns empty (the pipe breaks in
  that shell) — redirect to a file, then read the file.
- **agent-mail rig run quoting**: tilde paths and `&&` get mangled by the
  runner's quoting; prefer direct ssh + `nohup` for long one-shot builds.
- **dmv note**: `script/bundle-mac -i` prints a cosmetic `mv ... dmg/` error —
  the `dmg/` staging dir is only created on the non-`-i` path; install already
  succeeded by then.

## Known trade-offs (verdict-accepted)

- A busy agent still spends one boundary turn per fyi batch (folding fyi into
  the running turn's context is the deferred follow-up).
- A parked fyi on an idle Claude session expires past the ten-minute lookback
  without delivery — fine for advisory mail.
- 4090 before its install completed: old binary can't parse fyi (drops it) and
  its queued `all@` sends were ungated; closed by the install on 2026-11-05.
