# Plan 025: Auto-allow permission prompts after a countdown

## Problem

Even with `agent.tool_permissions.default: "allow"` + `sandbox_permissions.allow_*`,
some prompts are **forced** by design (`authorize_always_prompt`, ignores settings):

- edits to sensitive paths (`~/.config/zed/`, `.zed/`, `~/.agents/skills/`)
- symlink escapes
- non-builtin skill loading (first use per thread)

During unattended auto_prompt runs these dialogs park the thread forever ("ui
stuck and wait forever prevent continuous work").

## Design

Opt-in setting: permission prompts (`AuthorizationKind::PermissionGrant`)
auto-answer with the **least-privileged allow** option after a countdown.

- New setting `agent.auto_allow_permissions_after_seconds: Option<u64>`
  (default `None` = disabled; user sets `30`).
  Top-level under `agent` (not `tool_permissions`) because it also covers
  external ACP agents (claude-acp), and `tool_permissions` is documented
  native-only.
- Timer lives in `Conversation` (agent_ui) — the layer where both native and
  ACP prompts converge and where the pending-request tracking already lives.
  Uses `cx.background_executor().timer` (fake-clock friendly in tests, per
  gpui-test skill). Countdown = whole seconds decremented per 1s tick — no
  `Instant::now()` deadlines (real-clock vs test-clock mismatch).
- One detached tick task per pending request; exits when its entry is answered
  (removed on `ToolAuthorizationReceived`), fired, or the entity drops.
- Option pick: `resolve_outcome_from_selection(options, None, true)` —
  Flat → first `AllowOnce` (sandbox escalation = "Allow once", sensitive =
  "Only this time"); Dropdown → last choice ("Only this time"). Never deny.
- Carve-outs (leave pending): `AuthorizationKind::ActionChoice` (windows fs
  warning, decisions), and the sandbox-fallback prompt (detected via
  `SANDBOX_FALLBACK_RETRY_OPTION_ID`) because auto-choosing
  "retry vs unsandboxed" after a sandbox failure deserves human eyes.
- UI: countdown label ("Auto-allow in Ns") on the permission card
  (`render_permission_buttons` covers flat+dropdown) and in the
  "Awaiting Confirmation" banner. Repaint via `cx.observe(&conversation)` in
  `ThreadView::new` + `cx.notify()` per tick.

## Files

- `crates/settings_content/src/agent.rs` — new content field + docs
- `crates/agent_settings/src/agent_settings.rs` — struct field, `from_settings`, parse test
- `crates/agent_ui/src/conversation_view.rs` — countdown state, tick task, fire logic, accessor, tests
- `crates/agent_ui/src/conversation_view/thread_view.rs` — observe + countdown labels
- `~/.config/zed/settings.json` — `"auto_allow_permissions_after_seconds": 30`

## Non-goals

- No change to forced-prompt policy itself (still forced, just auto-answered).
- No ActionChoice / elicitation auto-answer.
- No settings-UI page row (defer).

## Tasks

- [x] Add setting to settings_content + agent_settings (struct/from_settings/parse test)
- [x] Conversation: countdown map, tick task, fire_auto_allow, accessor
- [x] ThreadView: observe conversation, countdown label on card + banner
- [x] Tests: auto-allow fires after timeout; no auto-allow by default
- [x] Update user settings.json with 30s
- [x] Clippy + targeted tests green
- [x] Commit `feat:` on develop (3ffef69ab4, pushed)
- [-] Settings-UI page row (deferred; JSON-only is fine for now)
