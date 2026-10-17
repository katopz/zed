# 035 — Native agent `send_message` / `list_threads` (Claude SendMessage parity)
Status: DONE — impl 4765075d04; settings_ui gate + release `./script/clippy` (scoped) green

## Goal
Native-agent threads (GLM or any model) can message other live native threads the
way Claude Code's `SendMessage`/`ListAgents` do. Before this, `create_thread`
siblings were fire-and-forget and subagents were reachable only by their parent
via `spawn_agent(session_id)`.

## Design
- Substrate reused: `NativeAgent.sessions` (every live native thread, incl.
  subagents and `create_thread` siblings), `AcpThread::send`, `AcpThreadEvent::Stopped`.
- `NativeAgent.mailboxes: HashMap<SessionId, Vec<MailboxMessage>>` (`thread_mailbox.rs`).
  - Target idle → deliver immediately as one user turn.
  - Target generating → queue; flush on `Stopped` (not `Cancelled`), deferred one
    tick so a UI-queued user message goes first. `AcpThread::send` cancels a running
    turn, so mail is never sent while `Generating`.
  - A burst of mail is batched into one `<agent_message from=… session_id=…>` prompt.
  - Mailbox dropped (logged) when the session closes.
- `wait_for_reply` only applies to immediate delivery → no A↔B deadlock
  (a busy target always returns `queued`). The recipient's turn runs detached so
  cancelling the sender's tool call never cancels the recipient.
- Scope: same project only; native sessions only (external ACP agents not addressable).

## Tasks
- [x] `ThreadListing` / `ThreadMessageRequest` / `ThreadMessageOutcome` types
- [x] `ThreadEnvironment::{list_threads, send_message_to_thread}` + native impl
- [x] `list_threads` and `send_message` tools, registered at every depth
- [x] Enabled in `write` + `ask` profiles; excluded from permission UI list
- [x] DRY: `last_agent_message_text` shared with `NativeSubagentHandle`
- [x] Tests: busy queue → delivered after turn end; tool wait_for_reply; self-send rejected
- [x] clippy + tests green (`cargo clippy -p agent -p agent_ui --all-targets` clean; 3 thread_mailbox_tests + 2 thread_mailbox unit tests pass)
- [x] settings_ui gate `test_all_tools_are_in_tool_info_or_excluded` passes; `./script/clippy -p agent -p settings_ui -p agent_ui -p gpui_platform` (release, all-features, `--deny warnings`, + cargo machete) clean. Full-workspace run was blocked by E0463 missing-artifact corruption in the exFAT `target/release`, so it ran in `CARGO_TARGET_DIR=/tmp`; `-p gpui_platform` enables `runtime_shaders` because this M3 lacks the offline Metal toolchain
- [-] auto_prompt "continue" may still interrupt a just-delivered mail turn (the mail
  stays in history, so content isn't lost); guard auto_prompt send on `Generating` if seen in practice
- [-] Address external ACP agent threads (needs `SiblingThreadHost` extension in agent_ui)
