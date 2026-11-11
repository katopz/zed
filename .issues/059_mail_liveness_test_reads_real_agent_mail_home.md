# 059 — test_agent_mail_broadcast_respects_liveness_cutoff reads the real ~/.agent-mail
Status: open — filed from plan 056 verdict round 2 (AGREE, non-blocking follow-up)
**Claimed:** amo-verdict-lane, 1794375792, expires 2026-11-13T00:00:00+00:00

## Reproduction attempt 2026-11-11 (amo, same tree, ~5 h after the filing run)

- Single test: `cargo test -p agent thread_mailbox -- --nocapture` → 23 passed, 0 failed.
- Full suite: `cargo test -p agent` → 843 passed, 0 failed, 11 ignored.
- No commits touched `crates/agent_mail`, the bridge, or the test file since the
  morning failing run — same code, different outcome → the mechanism is
  environmental, not a deterministic real-home read.
- Dependency audit: `enable_agent_mail` already injects a `tempfile::tempdir()`
  home into BOTH the sender and the bridge; the poller, `claim_inboxes`,
  `take_broadcasts_since`, and the liveness writes all scope to `mailroom.home`.
  `start_if_installed` (the only startup `from_env` consumer) is `cfg!(test)`-gated;
  the other `Mailroom::from_env()` callers are not linked into the agent test
  binary or not on the poll path. No real-`~/.agent-mail` read is reachable from
  this test.
- The filed fix (`set_var("AGENT_MAIL_HOME", temp)`) is process-global env
  mutation under a parallel harness — the exact hazard this issue itself flags —
  and would guard against a path that does not exist. REJECTED as written.
- Suspected real mechanism: real-clock reads (`chrono::Utc::now()` for liveness
  cutoffs and envelope `sent_at`) vs the executor's mock clock — a backward
  wall-clock step between cutoff stamping and compose produces exactly the
  observed `pending == 0` on both threads, and would explain morning-vs-now
  flakiness without any home read. Unproven after the fact.
- Disposition pending a Claude verdict (reviewer rate-limited at filing of this
  note; wake scheduled): close as not-reproducible + keep the hermetic-by-
  injection design, or re-open with a capture recipe if it recurs.

- [ ] Point the test's agent-mail home at a temp dir: `Mailroom::from_env()`
      honors `AGENT_MAIL_HOME` (`crates/agent_mail/src/agent_mail.rs:445`) —
      set it (and restore after) in `enable_agent_mail` or the test setup in
      `crates/agent/src/tests/thread_mailbox_tests.rs`
- [ ] Verify the test passes on a rig with real fleet state (amo) and on a
      clean box; note: parallel gpui tests sharing process env need a
      race-safe approach (env vars are process-global — prefer a
      `Mailroom::with_home` / setup-scoped override over `set_var` if tests
      run in parallel threads)

## Context

`tests::thread_mailbox_tests::test_agent_mail_broadcast_respects_liveness_cutoff`
(`crates/agent/src/tests/thread_mailbox_tests.rs:602-630`) sends an
`all@m3` broadcast through the real mailroom resolved from this machine's
`~/.agent-mail`. On the live `amo` rig (real rigs.json + liveness registry)
the blast is routed/consumed differently and the assertion
`pending_completions == 1` fails with 0. Verified pre-existing on clean HEAD
(stash → still fails → pop). Same machine-dependence presumably applies to
the other `thread_mailbox_tests` cases that go through `enable_agent_mail`.

Ref: plan `.plans/056_boss_hourly_fleet_housekeeping_watchdog.md` (verdict
round 2), validation run 2026-11-11.
