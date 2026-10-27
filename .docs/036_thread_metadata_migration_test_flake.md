# 036: Thread-metadata remote-connection migration test flake
Status: done

- **Commit**: `de20a0aac9` (per-App test database in `thread_metadata_store` `init_test`)
- **Resolves**: `.issues/035_flaky_thread_metadata_migrate_remote_connections.md`
  (removed after this entry; see git history)
- **Date**: 2026-10-27
- **Same class as**: `.docs/014_multi_workspace_restore_flake_isolation.md`

## What happened

`thread_metadata_store::tests::test_migrate_thread_remote_connections_backfills_from_workspace_db`
failed now and then in full `agent_ui` runs and passed alone.

## Cause

The module's `init_test` never set a per-App `db::AppDatabase`, so
`AppDatabase::global` fell back to the process-wide `TEST_APP_DATABASE`.
`KeyValueStore::global` and `WorkspaceDb::global` both resolve through it, so
every parallel test shared one KVP table. The test clears the
`thread-metadata-remote-connection-backfill` key and then runs the migration.
When another test's migration wrote the key in between, the migration
returned early and the row was never backfilled.

The thread-metadata DB itself was already per-test (`TestMetadataDbName`),
which hid the shared KVP and workspace stores.

## Fix

`init_test` now calls `cx.set_global(db::AppDatabase::test_new())` first, as
the other `agent_ui` test helpers already do.

## Verification

`agent_ui` lib tests filtered to `thread_metadata_store`, `--test-threads 32`,
100 runs each, run alternately:

| Build | Failures |
|---|---|
| before | 11 / 100 |
| after | 0 / 100 |

Full `agent_ui` suite: 665 passed. `./script/clippy -p agent_ui` clean.

## Trap for new tests

Any test helper that touches `KeyValueStore::global(cx)`, `WorkspaceDb::global(cx)`
or another `static_connection!` store needs `cx.set_global(db::AppDatabase::test_new())`.
Without it, the test shares state with every other test in the process.
