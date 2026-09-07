# Issue 021: Claude 5h/weekly usage rings vanish — first poll races claude-acp's OAuth token rotation

Status: FIXED — unconditional 401 re-read + bounded fast retry landed (`20316921a8`); pre-existing clippy blocker cleared in `fe8bbb3eca`. Gate: clippy green, in-tree test execution NOT completed (see Gate section). Live-verify pending: next cold start that opens a Claude thread should show the rings within ~15s instead of blacking out for 300s.

## Symptom

Reported 2026-09-07 ~14:38 local as "Claude ring that show 5 hours limit and
weekly limit is gone". The `5h` / `7d` rings beside the per-thread context ring
render nothing at all. The render path, the `claude-acp` agent gate, and the
store subscription are all intact — the rings are hidden purely because
`ClaudeUsageStore::usage()` is `None`.

## Evidence

From `~/Library/Logs/Zed/Zed.log` (session 13:45:20 → 14:39:14):

```
14:35:35 [agent_servers::acp] [session/create] ... phase=validate-cwd     <- first Claude thread opens
14:35:36 [agent_servers::acp] [session/create] ... phase=sdk-initialize durationMs=643
14:35:37 [agent_ui::claude_usage] could not read Claude usage: Claude usage
         request failed with status 401 Unauthorized
```

And the keychain item's own modification date:

```
security find-generic-password -s "Claude Code-credentials"
  "mdat"<timedate>= "20260907073537Z"   # == 14:35:37 +07, to the second
```

The token was rewritten at the exact second Zed's poll was rejected.

## Root cause

Two defects compound, both present since the feature landed in `97f50e6ddc`:

1. **The 401 retry was gated on `was_cached`.** `fetch_usage` only re-read the
   keychain and retried when the rejected token came from its own cache. On the
   *first* poll of a session the token is freshly read, so `was_cached` is
   `false` and a 401 skipped the retry entirely.

2. **`RETRY_INTERVAL` (300s) is longer than `POLL_INTERVAL` (180s).** A
   transient auth failure therefore blacked the rings out for five minutes.

The race itself is not incidental — it is the common path. Opening a Claude
thread is what both spawns `claude-acp` (whose `sdk-initialize` refreshes the
shared OAuth token) *and* creates `ClaudeUsageStore` (whose first poll fires
immediately). The poll reliably reads the pre-rotation token. Because of (1) it
never retries, and because of (2) it waits 300s — longer than many short
sessions last. Here the session ended at 14:39, before the 14:40:37 retry, so
`usage` stayed `None` for the entire session and the rings never appeared.

Note this is a *startup* failure only. A 401 mid-session does not clear
`this.usage`, so already-fetched rings survive it with stale numbers.

## Fix

- Retry a 401/403 whenever the keychain now holds a *different* token, rather
  than only when the token came from the cache. Comparing tokens covers both
  the rotation race and a long-stale cache, and skips the duplicate request
  when nothing actually rotated.
- Tag the auth-shaped failure with a `TokenRejected` marker error and retry it
  at `AUTH_RETRY_INTERVAL` (15s), bounded to `MAX_AUTH_RETRIES` (3) consecutive
  attempts before falling back to `RETRY_INTERVAL`. The bound matters: API-key
  accounts have no subscription usage and 401 forever, and `9f27f68621` already
  had to raise the poll interval to stay out of this endpoint's 429 bucket.

## Not fixed (deliberate)

The failure is still **silent in the UI** — `render_claude_usage` returns
`None`, so "token expired", "no subscription", and "endpoint down" are
indistinguishable from the user's side, and only the *first* failure per
session logs above `debug`. Surfacing a degraded/stale indicator is a UI design
change, not a bug fix, so it is left out per the repo's "avoid creative
additions" rule. Worth a follow-up if this recurs.

## Tasks

- [x] Reproduce from evidence (Zed.log 401 + keychain `mdat` correlation)
- [x] Identify root cause (first-poll race with `claude-acp` sdk-initialize token rotation)
- [x] Confirm not a render/gate/subscription regression
- [x] Fix the `was_cached` retry gate
- [x] Bound the fast auth retry to protect the 429 bucket
- [x] Add regression test for `TokenRejected` surviving `.context()`
- [x] Verify the anyhow downcast assumption against the pinned version (1.0.102)
- [x] Clippy clean (`-p agent_ui --lib --tests -- --deny warnings` → exit 0)
- [x] Commit + push
- [ ] Execute the test suite in-tree (blocked, see Gate)
- [ ] Run the mandated `./script/clippy` release gate (blocked, see Gate)
- [ ] Live-verify the rings appear on a cold start

## Gate actually used

Honest record, because the mandated gate did not run:

- **Clippy: PASSED.** `CARGO_TARGET_DIR=/tmp/issue021 CARGO_PROFILE_DEV_DEBUG=0 cargo clippy
  -p agent_ui --lib --tests -- --deny warnings` → `CLIPPY_EXIT=0`
  (`Finished dev profile in 50.16s`). This also proves the new test compiles.
- **`./script/clippy` (`--release --all-targets --all-features`): NOT RUN.** `target/release`
  does not exist, and the warm 102G `target/aarch64-apple-darwin/release` cache was held for
  30+ minutes by a wedged foreign `cargo check -p agent_ui` (0.2% CPU) belonging to another
  session, which must not be killed. A cold release build was impossible: the boot volume
  backing `/tmp` is at 100% capacity with ~14Gi free.
- **In-tree test execution: NOT COMPLETED.** Repeated attempts stalled with cargo in
  uninterruptible wait at 0% CPU and zero file writes. Cause is environmental, not the code:
  exFAT on the SDXC card does not support hard links, so rustc reports `hard linking files in
  the incremental compilation cache failed. copying files instead` for every unit, while
  XProtect/Spotlight scan the fresh artifacts and a 168%-CPU `qemu-system-xtensa` from another
  session competes for the machine.
- **Compensating evidence for the untested assertion.** The one non-obvious thing the new test
  covers — that `anyhow::Error::downcast_ref::<TokenRejected>()` still resolves through the
  `.context()` layer carrying the status code — was executed standalone against zed's pinned
  anyhow 1.0.102: downcast through context `true`, display retains `401`, negative case `true`.
  The module's other tests cover pure functions untouched by this change.

Anyone picking this up should run the full gate on a machine where the repo is not on exFAT.
