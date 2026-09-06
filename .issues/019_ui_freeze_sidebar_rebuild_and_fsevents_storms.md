# 019: UI freeze regression — sidebar rebuild cost + exFAT FSEvents rescan storms

**Status:** Fixed — `240b7cfa05` (fs watcher) + `50c090fbc5` (sidebar rebuild), pushed to `develop` 2026-09-06. Follow-ups: `AutoPromptContext::collect` serialization fixed (`a087ed63e8`); sidebar weekly view filter + env-gated metadata retention added (see below).

## Symptoms

- Whole-window freezes of ~20s to ~1min, repeated: 2026-09-06 ~10:34, ~11:01 (75s), ~11:07, ~11:10, ~11:11.
- User reported last-known-good at `14d3658121` (issue 017 docs commit); regression perceived after that window (`e6711127fd`..`0ee52c5864`).
- Freezes clustered around auto_prompt summary handoffs and new-thread forks — i.e. whenever a dispatch fires panel events.

## Evidence gathered live

- `sample(1)` during the 11:07/11:10/11:11 freezes: main thread inside
  `Sidebar::update_entries → rebuild_contents` (sidebar.rs:1600-1601, 1449) —
  `PathList::hash` (`Path::hash` parses every path component) per row via
  `HashMap<PathList, _>` lookup, plus `worktree_info_from_thread_paths` string
  building (`from_utf8_lossy`) per row.
- Store scale: `sidebar_threads` = 12,699 rows; the top project group holds
  3,630 rows sharing ONE distinct 14-path work-dir list. Per rebuild:
  ~5M component hashes + ~200k allocations on the main thread.
- Trigger frequency jumped in the regression window: `0ee52c5864` (correctly)
  un-suppressed background chains — every background stop now decides +
  dispatches instead of dying silently when the focused view had auto-prompt
  off; `77c01056b0` escalation dispatches starved forks. Each dispatch emits
  `ThreadInteracted`/`ActiveViewChanged` → `schedule_update_entries` → full
  rebuild. Frequency × per-row cost = the freeze.
- Second, independent source: `fs::fs_watcher` "filesystem watcher lost sync"
  storms — 1,845 events in the 02:31–06:14 log window. FSEvents on the exFAT
  SDXC volume drops events under IO load and answers
  `kFSEventStreamEventFlagMustScanSubDirs`; each drop schedules rescans for
  ALL 52–58 registrations, rescanning 15k-entry trees on slow media and
  feeding more change events back into the sidebar rebuild path.

## Fixes

- `240b7cfa05` `fix(fs)`: macOS `statfs` detection mirroring the Linux one —
  exfat/msdos/fat* and smbfs/afpfs/nfs/webdav/sshfs/fuse get the poll watcher
  (bounded 2s scans; `ZED_FILE_WATCHER_POLL_MS` tunable; `ZED_FILE_WATCHER_MODE`
  still overrides). Stops the storm at the source.
- `50c090fbc5` `fix(sidebar)`: `resolve_workspace` HashMap<PathList,_> → Vec +
  linear `PathList::eq` (byte memcmp, no component hashing — groups have a
  handful of workspaces); `make_thread_entry` memoizes
  `worktree_info_from_thread_paths` per distinct `WorktreePaths` (rows share
  path sets; `Arc`-shared clones keep per-entry Vecs cheap).

## Validation

- `cargo test -p sidebar` 145/145; `cargo test -p fs --lib` 16/16 (new
  `macos_poll_detection_says_no_for_apfs_tmp` included); clippy clean for both.
- Live sampling during a freeze no longer lands in `rebuild_contents`.

## Known follow-ups (not fixed here)

- ~~`AutoPromptContext::collect` (crates/auto_prompt/src/context.rs) still
  serializes the WHOLE thread via per-entry `to_markdown` on the main thread~~
  **FIXED (follow-up commit):** `collect` now gates per-message serialization
  on `thread.token_usage().is_none()`. When the provider reports usage (the
  common case), it serializes only the first user message + the trailing
  assistant run (what consumers actually read: `first_user_message`,
  `last_assistant_message`, plan fields) and skips all tool-call
  serializations and historical messages; `messages` stays empty and the
  chars/4 estimate covers only plan/doc sources (superseded by
  `actual_input_tokens` anyway). Full serialization remains as the fallback
  for providers without usage. Verified all `context_json` consumers parse
  only `session_id`/`plan_files`/`last_assistant_message`/`current_paths`
  (lightweight orchestrator, retry context, plan detectors, plan landscape,
  checkbox verification, auto-claim). Thread-backed regression tests added in
  `tests/context_helpers_test.rs` (`collect_from_thread`): fast path vs full
  path parity for `first_user_message`/`last_assistant_message`. Side benefit:
  `context_json` shrinks from ~80K+ chars to ~1-2K per stop, speeding every
  `serde_json::from_str` parse in the plan/summary machines.
- Sidebar store hygiene: 12,699 rows (3,819 with empty folder_paths) — an
  archive/cleanup pass would shrink every rebuild proportionally.

## Follow-ups landed (2026-09-06, same day)

- **Sidebar weekly view filter (default on)** — `Sidebar::weekly_filter_enabled`
  (persisted via `SerializedSidebar.weekly_filter_enabled`, default true).
  With no search query active, `rebuild_contents` gathers only threads with
  `thread_display_time` (interacted_at/updated_at) within the last
  `WEEKLY_FILTER_DAYS = 7` days; a header filter icon
  (`IconName::Filter`, `toggle_state`) toggles it — checked = last 7 days,
  unchecked = full history. Search bypasses the filter so old threads stay
  findable, and the active/retained threads of every workspace are always
  exempt (an open old thread must never vanish from the list).
  `has_stored_thread_rows` applies the same predicate so fully-stale groups
  render the "No threads yet" empty state. Test-harness sidebars start with
  the filter off (they seed old-timestamp threads); the behavior is covered
  by `test_weekly_filter_hides_old_threads_and_toggle_lists_all`.
- **Env-gated metadata retention** — `ThreadMetadataStore::enforce_retention_cap`
  runs after each store reload. Reads `ZED_AUTO_ARCHIVE_THREAD_CAP` (unset/0
  = disabled, default). When set (e.g. 500): groups unarchived non-draft
  threads by (folder_paths, remote identity), archives oldest-beyond-cap via
  `archive(id, None, cx)` — metadata-only flag flip, no worktree archive jobs,
  thread bodies untouched, threads stay reachable in the archive view.
  Pinned threads and threads updated within 7 days are never archived.
  Covered by `test_retention_*` (4 tests) in thread_metadata_store.rs.
