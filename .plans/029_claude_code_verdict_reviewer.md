# Claude Code Verdict Reviewer (phase 6 of proposal 001)

Status: IMPLEMENTED — `agent.verdict_reviewer = "claude_code"` behind the ping-pong gate; teardown limitation fixed via `.issues/016` part 2 (drain pattern)

## Goal

Let the `request_verdict` ping-pong (`.proposals/001_claude_sub_agent_verdict.md`)
use a **Claude Code (ACP) session as the reviewer**, so the second opinion runs
on Claude Code's own subscription auth (browser login) instead of requiring an
Anthropic API key in Zed. This is the path the user actually has available when
the native worker runs on GLM.

## Design (substrate-first)

Reuses, in order of preference:

1. `AgentConnection::new_session` / `close_session` — the exact machinery the
   hidden orchestrator (`auto_prompt/claude_agent.rs`, plan 014) uses to run
   off-screen Claude Code sessions with subscription auth.
2. `AgentConnectionStore` (agent_ui) — the live connection registry keyed by
   `Agent`; the panel already connects Claude Code there.
3. The verdict session registry (`acp_thread::verdict`, phase 1) — gains an
   optional `reviewer_thread: Option<Entity<AcpThread>>` so follow-up rounds
   resume the SAME session (ping-pong rule) and `close_reviewer_session` can
   free the ACP process.

Cross-crate wiring follows the fork's established break-the-circle pattern
(`auto_prompt::peer_states::register_broadcaster` ← `agent_board::register_writer`):
a provider trait + global in `acp_thread`, implemented and registered by
`agent_ui` (which owns the connection store). gpui `Entity` handles are
`Send + Sync` (fields are `Weak<RwLock<..>>` + ids), so the provider can be a
`Send + Sync` trait object even though `AgentConnection` itself is `Rc`.

## Changes

- `crates/acp_thread/src/verdict.rs`
  - `pub trait VerdictReviewer: Send + Sync` — `label()` +
    `spawn_session(project, work_dirs, cx) -> Task<Result<Entity<AcpThread>>>`
  - `set_reviewer` / `reviewer` global provider slot
  - `register_reviewer_session(id, thread) -> usize` (stores the thread)
  - `pub const REVIEWER_TURN_TIMEOUT` (180s, mirrors hidden orchestrator)
  - `pub async fn reviewer_turn(thread, message, timeout, cx) -> Result<String>`
    — bounded `AcpThread::send` + collect last assistant message
    (single-sourced turn runner; the hidden orchestrator keeps its own copy for
    now — dedup tracked in the follow-up)
  - `close_reviewer_session(id, cx)` / `complete(id, cx)` — close via
    `thread.connection().close_session` and drop the registry entry
- `crates/agent_ui/src/verdict_reviewer.rs` (new) — `ClaudeCodeReviewer`
  holding `WeakEntity<AgentConnectionStore>`; resolves the `claude-acp`
  connection (`entry.wait_for_connection`), spawns the session. Registered in
  `AgentPanel::new` next to the connection store.
- `crates/agent/src/tools/request_verdict_tool.rs`
  - routing: `agent.verdict_reviewer` = `native` (default) | `claude_code`
  - pure `resolve_route()` (unit-tested); claude_code without a registered
    provider → clear error, never silent fallback
  - `final_round` input flag → closes the reviewer session on agreement
  - budget-exceeded → close + complete (negotiation cannot continue)
- `ThreadEnvironment` gains `project()` / `work_dirs()` accessors (default
  `None`, native impls provided) so the tool can spawn the reviewer session in
  the worker's project.
- Settings: `agent.verdict_reviewer` (`settings_content` + `agent_settings`).

## Known limitation (fixed via `.issues/016` part 2)

TTL-expired reviewer sessions (parent abandoned mid-negotiation without
`final_round`) used to idle until app exit because `verdict::prune` has no
`cx`. FIXED: prune defers the thread to a pending-close list and the panel's
10s drain loop pumps `drain_pending_closes(cx)` to close the ACP session.

## Validation

- `cargo test -p acp_thread verdict` — provider registry + reviewer-session
  storage/close tests (StubAgentConnection)
- `cargo test -p agent request_verdict` — routing + serde tests
- Full `agent` + `agent_ui conversation_view` suites for regressions
- Targeted clippy on all touched crates

## Tasks

- [x] acp_thread::verdict — provider trait (`VerdictReviewer`), global slot
      (`set_reviewer`/`reviewer`), reviewer session storage
      (`register_reviewer_session`/`reviewer_thread`), bounded turn runner
      (`reviewer_turn`, 180s), `complete_reviewer` close path; 8 tests green
- [x] settings — `agent.verdict_reviewer` (`native` | `claude_code`, default
      native) through `settings_content` + `agent_settings` + test literals
- [x] ThreadEnvironment — `project`/`work_dirs` accessors (default `None`,
      native impls read thread + parent ACP wrapper)
- [x] tool — `resolve_route` (pure, never silently falls back), `final_round`
      closes the reviewer session, budget-exhausted tears down; 4 tests green
- [x] agent_ui — `ClaudeCodeReviewer` (WeakEntity<AgentConnectionStore>,
      resolves the `claude-acp` entry, awaits connect task, spawns off-screen
      session) registered at panel creation
- [x] thread_view prompt — `final_round` instruction on the closing round
- [x] `.issues/016` — GOAT benchmark plan + TTL-session teardown limitation
- [x] proposal 001 — phase 6 checkbox + status

## Validation

- `cargo test -p acp_thread verdict` — 8/8
- `cargo test -p agent request_verdict` — 4/4 (routing + serde)
- full `cargo test -p agent` — 737 passed; `agent_ui conversation_view` — 116
  passed (caught + fixed a discarded-`Deferred` bug in the first registration)
- targeted clippy clean on acp_thread / agent / agent_ui
