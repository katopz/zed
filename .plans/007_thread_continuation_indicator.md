# Thread Continuation Indicator

## Problem
When `auto_prompt` creates a new thread from a summary (ContextOverflow Phase 2),
neither the old nor the new thread has any indicator of the relationship:

- The OLD thread (summarized) doesn't show that the conversation continued elsewhere.
- The NEW thread (created from summary) doesn't show where it came from.

Users lose track of the conversation flow across threads.

## Solution
Add a small clickable chip on the second metadata line of `ThreadItem`:

- **New thread**: `from` — links back to the source thread.
- **Old thread**: `to` — links forward to the continuation thread.

Just the words `from` / `to`, no title text. Keep it minimal.

## Design: Single-Column Bidirectional Derivation

Store ONE column (`continued_from_session_id`) on the new thread. Derive both
directions at render time:

- **New thread**: has `continued_from_session_id` → show `from` chip
- **Old thread**: reverse-scan `ThreadMetadataStore` for any thread whose
  `continued_from_session_id == this.session_id` → show `to` chip

The reverse scan is O(n) but thread lists are small (typically <100 entries)
and already fully loaded in memory.

## Tasks

- [x] **DB migration**: Add `continued_from_session_id TEXT` column to `sidebar_threads`
- [x] **ThreadMetadata struct**: Add `continued_from_session_id: Option<acp::SessionId>` field
- [x] **DB save/load**: Persist and load the new field in `ThreadMetadataDb::save` and `Column for ThreadMetadata`
- [x] **ThreadMetadataStore**: Add `set_continued_from()` method + `find_continuation_of()` reverse-lookup
- [x] **Auto prompt wiring**: In `dispatch_action`, when creating `ThreadSummary`, set `continued_from_session_id` on the new thread's metadata after creation
- [x] **ThreadItem UI**: Add `continuation()` builder that renders `from` / `to` as a clickable chip
- [x] **Sidebar rendering**: In `render_thread`, compute continuation info (both directions) and pass to ThreadItem
- [x] **Click navigation**: Clicking the indicator activates the referenced thread (reuse existing `activate_thread` path)
- [x] **Tests**: Verify the continuation is set, persisted, and rendered

## Files to Modify

| File | Change |
|------|--------|
| `crates/agent_ui/src/thread_metadata_store.rs` | DB migration, struct field, save/load, store methods |
| `crates/agent_ui/src/auto_prompt/mod.rs` | Set `continued_from_session_id` in `dispatch_action` |
| `crates/ui/src/components/ai/thread_item.rs` | Add continuation indicator rendering |
| `crates/sidebar/src/sidebar.rs` | Compute + pass continuation info, click handler |

## Non-Goals
- Subagent parent threads (already handled by `SubagentContext`)
- Chain visualization (showing N-deep continuation chains)
- Editing/breaking continuation links
