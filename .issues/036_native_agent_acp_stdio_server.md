# 036 — Serve Zed's native agent over ACP stdio (`eval-cli --acp`) for zed-agentd
Status: open — design scoped 2026-10-27, not started; plan 037 P4

## Goal

`zed-agentd attach -- eval-cli --acp` so the fork's native agent runs detached
on the remote host like Claude/Codex do (`.plans/037_work_remote_agentd.md` P4).
Today the native agent loop, LLM calls and threads.db all run in the M3 GUI.

## Substrate already in the tree

- `crates/eval_cli/src/main.rs` + `headless.rs`: boots headless gpui, project,
  `LanguageModelRegistry` (env API keys, openai-compatible / anthropic overrides),
  `NativeAgent` + `NativeAgentConnection`, creates an `AcpThread` per session
  and drives `send_and_wait_for_completion`. Everything except the stdio server.
- `agent-client-protocol = 3.5.0`: `Agent.builder()` + `on_receive_request`
  handles the JSON-RPC side (initialize, session/new, session/prompt,
  session/cancel, session/load, session/list).
- `agent::ThreadEvent` (`crates/agent/src/thread.rs`) is nearly ACP-shaped
  (`AgentText`, `AgentThinking`, `ToolCallUpdate`, `ToolCallAuthorization`, `Stop`).
- zed-agentd already journals/replays `session/update`s and rewrites
  `loadSession`, so the server does not need its own replay for detach.

## Decision to make first: which layer to translate

| Option | Pros | Cons |
|---|---|---|
| A. Tap `AcpThread` events (`NewEntry` / `EntryUpdated`) and diff entries back into `session/update` | Terminals, diffs, retries, compaction all work as in the GUI | Entries hold markdown entities, so it needs a per-entry "already sent" cursor to emit chunk deltas; tool-call content (diff/terminal entities) has to be re-serialised |
| B. Consume `ThreadEvent`s directly, bypassing `AcpThread` | Near 1:1 mapping, no diffing | `NativeAgentConnection::handle_thread_events` is where terminals are created (`create_terminal`, agent.rs ~3714) and authorizations resolved; bypassing it means re-implementing both |

Recommendation: **A**. The GUI path stays the single source of truth, and the
cursor-diff code is local to the new module. B duplicates
`handle_thread_events` logic that upstream churns often (plan 038 risk).

## Tasks

- [ ] `crates/eval_cli/src/acp_server.rs` behind `--acp`: stdio transport, initialize advertising `loadSession: false` (zed-agentd adds it), session/new → `connection.new_session(project_for(cwd), ...)`
- [ ] session/prompt → `send_and_wait_for_completion`; `AcpThread` events → `session/update` via per-entry sent cursor (assistant text/thought chunks, tool_call / tool_call_update incl. status, diff content as `ToolCallContent::Diff`, terminal output as text)
- [ ] `ToolCallAuthorization` → `session/request_permission` round-trip to the client, answer → `authorize_tool_call`
- [ ] session/cancel → `AcpThread::cancel`
- [ ] model selection: `--model` default + ACP session config option if the client sends one
- [ ] test: in-process client over a duplex pipe with `FakeLanguageModel` (text + one tool call + one permission) asserting the update sequence
- [ ] e2e: `zed-agentd attach -- eval-cli --acp` locally, kill the attach mid-turn, reattach, replay matches (same harness as the P1 fake-agent smoke)
- [-] Native-agent `session/load` from threads.db — zed-agentd journal covers reattach; only needed without the daemon
