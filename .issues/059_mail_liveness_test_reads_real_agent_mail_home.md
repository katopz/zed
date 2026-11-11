# 059 — test_agent_mail_broadcast_respects_liveness_cutoff reads the real ~/.agent-mail
Status: open — filed from plan 056 verdict round 2 (AGREE, non-blocking follow-up)
**Claimed:** amo-verdict-lane, 1794375792, expires 2026-11-13T00:00:00+00:00

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
