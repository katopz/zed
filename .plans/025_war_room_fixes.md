# 025 — War Room UX fixes (8 operator reports)

## Goal
Fix the 8 issues reported against the war room panel + browser dashboard:
1. `0 doing · 0 stale · 0 released` while 2 agents are actively running.
2. Feed should render as markdown blocks with agent name on top of each box
   and a copy icon (like the agent thread).
3. "click to mention" roster click does nothing.
4. Human can't reply — no input box visible.
5. Feed can't scroll.
6. Browser version: no button to click (GH auth disabled dead-end).
7. No visible last-sync time — can't tell if it's updating.
8. War room icon duplicates CollabPanel's `UserGroup` — want a robot.

## Root causes
| # | Root cause |
|---|---|
| 1 | Work board is plan-scope-only: agents without a claimed `.plans` file broadcast `states` but never appear. Add ad-hoc presence rows derived from fresh states (pure projection over existing stream, no new wire type). |
| 2 | Feed renders `SharedString` one-liners. Rebuild as message cards: sender header + `MarkdownElement` body (cached `Entity<Markdown>` per message) + `ui::CopyButton`. |
| 3 | `prefill_mention` writes into the input editor, but the input is pushed off-screen (see #4) so nothing visibly happens. Fix layout → click visibly fills the mention. |
| 4 | Root flex column overflows when board/roster rows exceed panel height: children with content-height min sizes push the last children (feed, input) out of the visible area. Pin input at the bottom (`flex_shrink_0`), cap board/roster sections with internal scroll. |
| 5 | Same layout bug — feed collapses to 0 height (nothing to scroll). Bounded `flex_1 min_h_0` + `overflow_y_scroll` + autoscroll on new messages (`ScrollHandle::scroll_to_bottom`). |
| 6 | Worker dashboard dead-ends when `GITHUB_CLIENT_ID` unset: sign-in button hidden, no boot path → no data, no buttons. Boot read-only (GET room + SSE, both unauthenticated), render messages feed + statuses, disable reply bar with an explanatory note. |
| 7 | Runtime tracks no sync metadata; SSE events never nudge the panel (only the 15s poll does). Track `last_synced_at`/`last_sync_error`, render in header; SSE msg/state events trigger `force_refresh`. |
| 8 | `IconName::UserGroup` == CollabPanel. Add `assets/icons/robot.svg` + `IconName::Robot`. |

## Tasks
- [x] `assets/icons/robot.svg` + `IconName::Robot` in `crates/icons` (icons tests enforce pairing). Also made `test_no_dangling_icons` skip macOS `._*.svg` AppleDouble sidecars (they appear on non-Apple filesystem mounts and broke the disk-level test).
- [x] `war_room.rs`: layout rework — header/board/roster `flex_shrink_0` with internal scroll caps, feed `flex_1 min_h_0` scroll + autoscroll, input pinned.
- [x] `war_room.rs`: feed message cards (sender header + timestamp + `CopyButton` + cached markdown body via `MarkdownCache`, blake3-keyed, evicted per feed).
- [x] `war_room.rs`: ad-hoc presence rows in `build_work_board` from fresh states without plan scopes (`(no plan)` Doing rows, latest-state-wins, STALE_AFTER_MS cutoff).
- [x] `war_room.rs`: header sync line (last sync time / error / local-only / waiting) + `Robot` icon.
- [x] `runtime.rs`: `last_synced_at` + `last_sync_error` tracking; `realtime_nudge` (2s-throttled `force_refresh`) for SSE feed events.
- [x] `realtime_client.rs`: foreground-owned task with runtime weak handle; msg/state/status SSE events nudge refresh; identical mention/reply injection.
- [x] `agent_board/Cargo.toml`: add `markdown` dep.
- [x] Worker `index.js`: read-only boot when GH disabled (SSE + 15s poll); war-room feed panel (messages from snapshot + SSE); status scopes ingested as pseudo-states; disabled reply bar with note.
- [x] Tests: 4 new projection tests (ad-hoc rows: fresh/covered/stale/latest-wins), runtime sync-metadata assertions; panel smoke draws the new card feed. 84 passed.
- [x] `./script/clippy` + `cargo test -p agent_board -p icons` green; `node --check` worker; screenshot example re-rendered through Metal (artifact refreshed at `.plans/024_war_room_panel.png`).

## Validation
- `./script/clippy -p agent_board -p icons`
- `cargo test -p agent_board` (war_room + runtime suites)
- `node --check agent-board-worker/src/index.js`
- Manual: reopen war room, verify input visible, feed scrolls, cards render md, roster click fills mention, header shows sync time.
