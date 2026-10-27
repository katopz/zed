# 035 — Flaky `test_migrate_thread_remote_connections_backfills_from_workspace_db`
Status: open — seen once 2026-10-27 post upstream sync (6b985a8b26); passes 3/3 in isolation

## Symptom

`cargo test -p agent_ui --lib`: 664 passed / 1 failed —
`thread_metadata_store::tests::test_migrate_thread_remote_connections_backfills_from_workspace_db`.
Re-running it alone passes every time; a sibling full run in the same tree passed 665/665.

## Suspicion

Parallel-test interference on shared state (workspace DB / KVP global, or a
process-global env/config read), the same class as the watchdog env-var race
fixed by `set_watchdog_override`. `thread_metadata_store.rs` carries 730 fork
lines (pinning), so check the fork migration path first.

## Tasks

- [ ] Reproduce under load: `cargo test -p agent_ui --lib -- --test-threads 16` in a loop, capture the assertion
- [ ] Find the shared state and scope it per-app (pattern: `WatchdogConfigOverride`)
