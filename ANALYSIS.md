# Crash Analysis: auto-prompt start-context re-entrant read panic kills Zed during continuation dispatch

Status: FIXED — regression introduced in `6606142b41`, fixed in this commit (see Reproduction).

## Crash Summary

- **Build:** Zed Dev 1.17.0 `f6746e2cce70988dc35d31c2f37aa09cfc810eb4` (crashing); `38917190c33dae9062400a20b6674400e1365489` (works fine, 12 commits earlier)
- **Error:** `cannot read agent_ui::conversation_view::ConversationView while it is already being updated` (gpui double-lease panic, `crates/gpui/src/app/entity_map.rs:164` → `double_lease_panic("read")`)
- **Crash Site:** `AgentPanel::active_thread_activity` → `view.read(cx)`, called from `start_context_block` at the top of `dispatch_action` (`crates/agent_ui/src/auto_prompt/mod.rs`), while the same `ConversationView` entity is leased by `_view.update_in(...)`

## Root Cause

`6606142b41` ("static manual auto-prompt + pre-gathered start context on continuations") added `start_context_block`, called **synchronously inside `dispatch_action`**, which itself runs inside `_view.update_in(...)` — i.e. while the dispatching `ConversationView` is **leased** (removed from the entity map; this fork's gpui uses a lease model, `EntityMap::lease` → `entities.remove`).

`start_context_block` → `workspace.read(cx).panel::<AgentPanel>().active_thread_activity(cx)` iterates `conversation_views()` = active + retained views and does `view.read(cx)` on each. When the auto-continued thread **is the displayed one** (the common case — the user watches the thread that just stopped/overflowed), the iterated active view **is the leased view**, and `EntityMap::read` panics with the double-lease message.

In release there is no panic hook in the app binary (`set_hook` only exists in collab/remote_server), so the panic message goes to stderr (lost for a GUI launch) and the unwind aborts crossing the extern-C runloop boundary — the app dies mid-dispatch.

### Log evidence (2026-08-31, `~/Library/Logs/Zed/Zed.log.old`)

- Session B (started 17:19:33, `f6746e2`): last line `17:26:20 [auto_prompt] LLM returned action - dispatching with prompt: …` (ContextOverflow Phase 2 on the thread the user was watching — the Claude session running `cat .benchmarks/.highwater …`). The next expected line, `[auto_prompt] dispatch_action: is_native_agent=…`, is **missing** — pinning the freeze/death between the two log lines, exactly across `start_context_block`.
- The 10-minute memory-heartbeat (`zed::reliability`, last log 17:25:04) never fired again → process gone by ~17:35; user relaunched at 17:43:10.
- Four earlier dispatches today (16:41:30, 16:52:52, 17:00:43, 17:11:01) all logged past `start_context_block` — those continued **background/retained** threads, not the displayed one, so no lease conflict. Matches the user report "crashed twice today" and "via Claude agent cause this" (the watched Claude thread was the trigger).
- Crash #1 (~17:12:55–17:19:33) shows a different signature: it follows Claude ACP session activity and matches the **chronic** main-thread stall captured in the Aug 24–30 `.spin` reports (`/Library/Logs/DiagnosticReports/zed_*.spin`): main thread in `AgentPanel::render_toolbar` → `ThreadView::render_sandbox_status` → `refresh_sandbox_status` → `Thread::update` during render (`crates/agent_ui/src/conversation_view/thread_view.rs:5261`), plus a same-day 137 GB/2.3 h disk-writes resource report for `zed`. That is a pre-existing issue, **not** this regression; filed here for follow-up.

## Reproduction

```
cargo test -p agent_ui test_active_thread_activity_skips_leased_dispatching_view
```

The test opens a generating sibling thread (stub connection holds the prompt open), opens a second (displayed) thread, then calls `active_thread_activity` from inside the displayed view's `update` — mimicking `dispatch_action`'s start-context gather. Verified both ways:

- Without the skip: panics with `cannot read agent_ui::conversation_view::ConversationView while it is already being updated` (the production crash).
- With the fix: passes, returning only the generating sibling's activity.

## Suggested Fix (implemented)

- `AgentPanel::active_thread_activity(&self, cx, skip_view: Option<EntityId>)` — skips the given view id before reading.
- `start_context_block` takes `dispatching_view_id: gpui::EntityId` and passes it down.
- `dispatch_action` computes `cx.entity().entity_id()` (the leased dispatching view) and passes it in.

Alternative considered: pre-gather the start context in `run_auto_prompt` before entering `update_in`. Rejected — both the manual and LLM paths funnel through `dispatch_action`, and the skip-id approach is the minimal change that keeps the gather point (and its cache-freshness comment) unchanged. Semantically the dispatching thread is the continuation *target*, not a "sibling agent", so excluding it is correct regardless.

## Follow-ups (not in this commit)

- Chronic render-path stall: `ThreadView::refresh_sandbox_status` does `Thread::update` + pending-task bookkeeping during `AgentPanel` render (`agent_panel.rs:6778` → `thread_view.rs:5261`); it is the main-thread stack in every recent `.spin`/cpu-resource report. Should be moved off the render path.
- Consider installing a panic hook in the app binary that logs to `Zed.log` before unwinding, so main-thread panics stop dying silently for GUI launches.

## Update (same day)

Both follow-ups landed:

- Render-path stall fixed: refresh is event-driven off-render (`ThreadView::refresh_sandbox_status`), the toolbar renders the cached status read-only (`ThreadView::sandbox_status_element`) — see `.issues/015_sandbox_status_render_path_update_loop.md`.
- Panic hook installed in `zed::reliability::init`: panics now log to `Zed.log` with a backtrace before unwinding.
