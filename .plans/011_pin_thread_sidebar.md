# Plan 011: Pin agent threads in the sidebar

## Problem

Users have no way to mark important agent threads as pinned. Threads they
want to keep visible float down the list as newer threads land, forcing them
to scroll/filter to find them. There is no UI affordance (hover button,
context-menu entry) to mark a thread as sticky, and no persistence layer
for the state.

## Design

Add a per-thread `pinned: bool` flag (DB-backed, mirrors the existing
`archived` column) that, when set, floats the thread to the top of its
sidebar group above all unpinned threads. Sorting among pinned threads
preserves the existing recency order (desc by `interacted_at`/`updated_at`).

Two UI affordances toggle the flag, both mirroring the established
`Rename Thread` / `Archive Thread` patterns so the change is surgical:

1. **Hover action button** beside the existing `Rename Thread` pencil — a
   `Pin`/`Unpin` icon button rendered only on hover (same `is_hovered`
   gate as the rename/archive buttons).
2. **Right-click context menu entry** — `Pin Thread` / `Unpin Thread`
   inserted directly above the existing `Rename Title` entry, matching
   the menu's `ContextMenu::build(...)` shape already in `render_thread`.

Pinned threads also render a small `Pin` indicator in the icon slot area
(or alongside the agent icon) so users can see pinned state at a glance
even when not hovering. The pin indicator is suppressed on drafts (drafts
are ephemeral; pinning them is meaningless until first message send).

### Data flow

```
ThreadMetadataStore (DB + in-memory cache)
   ▲
   │ set_pinned(thread_id, pinned, cx)
   │   └─ save_internal → DbOperation::Upsert → SQLite
   │   └─ cx.emit(ThreadMetadataStoreEvent::ThreadPinned(id, pinned))
   │
Sidebar (observer of ThreadMetadataStore via existing global)
   ▲
   │ pin_thread(thread_id, cx) / unpin_thread(...)
   │
   ├─ hover pin button (render_thread action slot)
   ├─ context-menu "Pin Thread" entry (render_thread right_click_menu)
   └─ PinSelectedThread action (registered in render, mirrors ArchiveSelectedThread)
```

### Sort change

`push_entries_by_display_time` and the in-group `threads.sort_by` block in
`rebuild_contents` get a two-tier comparator:
1. `pinned` desc (true sorts first),
2. existing recency desc (display_time).

Pinned state is read off the `ThreadMetadata` carried by each `ThreadEntry`,
so no extra plumbing into the sort comparator signatures is needed — both
call sites already have the metadata in scope.

## Files

- `crates/agent_ui/src/thread_metadata_store.rs` — add `pinned: bool` to
  `ThreadMetadata`; DB migration `ALTER TABLE sidebar_threads ADD COLUMN
  pinned INTEGER DEFAULT 0`; update `LIST_QUERY`, `Column for
  ThreadMetadata`, `ThreadMetadataDb::save`; add `set_pinned()` + emit
  `ThreadPinned` event.
- `crates/agent_ui/src/agent_ui.rs` — add `PinSelectedThread` action to the
  existing `actions!(agent, [...])` block.
- `crates/sidebar/src/sidebar.rs` —
  - `pin_thread`/`unpin_thread`/`pin_selected_thread` methods
  - hover pin button + pinned indicator in `render_thread`
  - "Pin Thread"/"Unpin Thread" context menu entry
  - register `PinSelectedThread` action in `render`
  - two-tier sort in `rebuild_contents` and `push_entries_by_display_time`
- `crates/sidebar/src/sidebar_tests.rs` — add tests mirroring
  `test_rename_selected_thread_action_renames_selected_thread` and
  `test_archive_thread_*`.

## Non-goals

- Not introducing a dedicated "Pinned" section above the project groups —
  pinned threads stay inside their project group, just at the top. A
  cross-group Pinned section is a UX call the user has not asked for.
- Not pinning terminals — the user's request is specifically about thread
  history.
- Not adding keyboard shortcuts beyond the already-dispatchable
  `PinSelectedThread` action (no default key binding; users can bind it
  themselves via keymap).

## Tasks

- [x] `thread_metadata_store.rs`: add `pinned` field + DB migration + Column/save plumbing + `set_pinned` + `ThreadPinned` event
- [x] `agent_ui.rs`: add `PinSelectedThread` action
- [x] `sidebar.rs`: `pin_thread` / `unpin_thread` / `pin_selected_thread` methods + register action in `render`
- [x] `sidebar.rs`: two-tier pinned-then-recency sort in `rebuild_contents` + `push_entries_by_display_time`
- [x] `sidebar.rs`: hover pin button beside `Rename Thread` in `render_thread`
- [x] `sidebar.rs`: `Pin Thread` / `Unpin Thread` context-menu entry in `render_thread`
- [x] `sidebar.rs`: pinned-state indicator on the thread item
- [x] `sidebar_tests.rs`: tests for pin action, sort, context-menu entry
- [x] `./script/clippy` clean
- [x] commit on `develop` with `feat:` prefix
