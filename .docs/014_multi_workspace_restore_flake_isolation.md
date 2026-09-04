# 014: Multi-workspace session-restore test flake (parallel test threads)

- **Commits**: `bbfefaa8ac` (isolate zed.rs test App database from the
  process-global `TEST_APP_DATABASE`), this entry (verification + teardown)
- **Resolves**: `.issues/014_multi_workspace_session_restore_parallel_flake.md`
  (removed after this entry; see git history)
- **Status**: done
- **Date**: 2026-09-04

## What happened

`zed::tests::test_multi_workspace_session_restore` failed deterministically
under default parallel test threads (passed with `--test-threads=1` and in
isolation) since the upstream MultiWorkspace merge: restored window A's
`project_group_keys()` contained an extra `/dir3` group — rows leaked from a
concurrent test's session-serialization flush.

## Root cause

Every test `App` without an explicit `db::AppDatabase` global fell back to
the process-global `TEST_APP_DATABASE`, where window/workspace id sequences
collide across concurrently running tests; a slow serialization flush from
one test leaked workspace rows into another test's session restore.

## Fix

`init_test_with_state` now sets `cx.set_global(db::AppDatabase::test_new())`
(`bbfefaa8ac`) so each test App owns an isolated database — the same
isolation pattern `agent_ui`'s `init_test` already used.

## Verification

`cargo test -p zed --bin zed` (default parallel threads) run 3×:
79 passed / 0 failed each (~14.5s per run). The failure was deterministic
pre-fix, so three consecutive green passes confirm resolution.
