# 015 — Sandbox-status chip updated entities from render: chronic main-thread stall / CPU burn

Status: FIXED — commit `8ed38b22f9` (render path made read-only; refresh moved to events).

## Evidence

- `/Library/Logs/DiagnosticReports/zed_*.spin` on Aug 24 (×5), Aug 26, Aug 27, Aug 28, Aug 30:
  main thread pegged in one repeating stack —
  `AgentPanel::render_toolbar` (agent_panel.rs:6814) →
  `Entity<ThreadView>::update` → `render_sandbox_status` → `refresh_sandbox_status` →
  `Entity<Thread>::update` → `refresh_verified_sandbox_status` (agent/thread.rs:1865).
- `/Library/Logs/DiagnosticReports/zed_*.cpu_resource.diag` daily Aug 25–30 + a
  137.44 GB dirty-memory / 8257 s disk-writes report (Aug 30) — sustained burn, not a one-off.
- Aug 31 17:12–17:19 freeze (user-reported "crash twice today", crash #1) matches the
  same signature window; crash #2 (17:26) was the separate auto-prompt double-lease
  regression fixed in `4c8bae3ddc` — see `ANALYSIS.md`.

## Root cause

`AgentPanel::render_toolbar` called `thread_view.update(...)` during the panel's own
render. In this fork's lease-based gpui, a render that reads *and* writes an entity in
the same pass re-marks it changed on every frame — a self-sustaining redraw loop.
Each pass also rebuilt the whole `SandboxStatusKey` (settings + all worktree paths +
git dirs, cloned and sorted twice) and mutated ThreadView mid-render (status store +
possible `cx.spawn`), so the loop carried real per-frame work on the main thread.

## Fix

- `ThreadView::refresh_sandbox_status` is now event-driven and runs in a spawned task
  (never during render): triggers are the native-thread observe (grant changes notify
  the thread via `persist_thread_grants`), project worktree events, `SettingsStore`
  global changes, and initial construction. In-flight refreshes coalesce.
- `ThreadView::sandbox_status_element(&self, cx: &App)` renders the cached status
  read-only; `AgentPanel::render_toolbar` reads it instead of updating the view.
- Panic hook installed in `zed::reliability::init` so future panics land in `Zed.log`
  (with backtrace) instead of dying silently at the runloop FFI boundary.
