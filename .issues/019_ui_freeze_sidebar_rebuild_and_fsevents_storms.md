# 019: UI freeze regression — sidebar rebuild cost + exFAT FSEvents rescan storms

**Status:** Fixed — `240b7cfa05` (fs watcher) + `50c090fbc5` (sidebar rebuild), pushed to `develop` 2026-09-06.

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

- `AutoPromptContext::collect` (crates/auto_prompt/src/context.rs) still
  serializes the WHOLE thread via per-entry `to_markdown` on the main thread
  inside `decide_finish` (called from `cx.update` in `decide_async`) for every
  non-summary stop. Verified no runtime consumer reads `messages` from
  `context_json` (lightweight orchestrator, retry context, plan detectors,
  pending-question all read only `last_assistant_message`/`plan_files`/
  `current_paths`); `messages` only feeds the chars/4 `approximate_token_count`
  fallback for providers that don't report usage. Candidates: derive the
  estimate from raw source lengths without building per-message Strings, or
  gate full serialization on `actual_input_tokens.is_none()`. Sub-second cost
  today; not the multi-second freeze sampled above.
- Sidebar store hygiene: 12,699 rows (3,819 with empty folder_paths) — an
  archive/cleanup pass would shrink every rebuild proportionally.
