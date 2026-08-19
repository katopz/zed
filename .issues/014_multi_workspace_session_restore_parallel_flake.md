# 014 — `test_multi_workspace_session_restore` fails under default parallel test threads

## Evidence (2026-08-19, plan 024 validation session)

- `cargo test -p zed` (default threads): `zed::tests::test_multi_workspace_session_restore`
  FAILS deterministically at `crates/zed/src/zed.rs:7421` — restored window A's
  `project_group_keys()` contains an extra `/dir3` group before the expected
  `[dir2, dir1]`.
- `cargo test -p zed -- --test-threads=1`: **79 passed, 0 failed** (full suite green).
- `cargo test -p zed --bin zed test_multi_workspace_session_restore` (isolated):
  passes.
- Introduced by upstream `ee3f40fe25` "Re-add MultiWorkspace (#48800)" (arrived
  via the 369-commit upstream merge `8823d2bcea`); NOT a plan-024 regression
  (plan commits only add the agent_board/war_room panels + a screenshot bin;
  `initialize_panels` is not exercised by these tests).

## Suspected vector (unproven)

Cross-test interference under parallel execution. Session ids are random v4
(`Session::test()`), so not session-keyed DB rows. Candidates: process-global
`GLOBAL_KEY_VALUE_STORE` LazyLock (stateless fallback db) keyed by
window/workspace ids that collide across concurrent test Apps, or a
timing/serialization-flush race in `flush_workspace_serialization`.

## Next step

Reproduce under `--test-threads=N` bisect to the interfering test pair, then
either serialize the multi-workspace restore tests with a shared static lock
(pattern: `crates/agent_board` `PANEL_TEST_LOCK`, commit `97c2dadd58`) or fix
the actual shared-state collision upstream.

## Workaround

`cargo test -p zed -- --test-threads=1`
