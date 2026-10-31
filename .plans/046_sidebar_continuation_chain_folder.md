# 046 — Sidebar: fold continuation chains into one group
Status: DONE — impl `71d9413ade`; release DMG built via `script/bundle-mac`

## Goal
Plan 007 linked summary-handoff threads with `from`/`to` chips, but the list
stayed flat, so a long auto_prompt chain was scattered between unrelated
threads. Show each chain as one folder-like group instead.

## Design
- Chain = threads linked by `continued_from_session_id` within one project
  group (roots found by walking parents; bounded walk guards corrupt cycles).
- The newest thread heads the group; the group sorts where it does (so newest
  on top still holds). Older threads follow it, indented, newest first.
- Head shows a clickable `▸ N earlier` chip (replaces `from`/`to`).
- Folded by default; unfolded when the active thread is an older member;
  always unfolded in search results. User toggles persist per chain root
  (the oldest thread, stable as continuations are added) for the session.
- A folded head shows its own title but the most urgent status of its hidden
  threads (waiting > running > error), plus their notification dot, so a fold
  never hides work.
- Hidden children still count in `current_thread_ids`/`current_session_ids`,
  so their notification and last-accessed state survive the fold.
- Fork-owned logic in `crates/sidebar/src/continuation_chains.rs`; upstream
  touch is the `push_entries_by_display_time` hook + `render_thread` wiring.

## Tasks
- [x] grouping (`group_continuation_chains`) + `ContinuationRow` roles
- [x] `ThreadItem::continuation_folder` / `continuation_child`; drop `ThreadContinuationDirection` + `activate_thread_by_id`
- [x] folded head status/notification rollup (`chain_status`)
- [x] tests: `test_continuation_chain_folds_under_newest_thread`, `test_folded_chain_surfaces_the_most_urgent_status`; all 163 sidebar tests pass; clippy `-p sidebar -p ui -p agent_ui` clean
- [x] release DMG (`script/bundle-mac`); Metal Toolchain component installed on m3 (`xcodebuild -downloadComponent MetalToolchain`) so release shaders precompile
- [-] persist fold state across restarts (session-only today; add if wanted)
