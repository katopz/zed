# 036 — Serve Zed's native agent over ACP stdio (`eval-cli --acp`) for zed-agentd
Status: MVP done (`c212b13be4`, harness `767ba226e9`) — `eval-cli --acp` serves the native agent, verified directly, through zed-agentd with reattach replay, and with the GUI killed mid-turn; open: structured diffs, model selection over ACP, real-host run (plan 037 P3)

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

- [x] `crates/eval_cli/src/acp_server.rs` behind `--acp`: stdio transport, initialize advertising `loadSession: false` (zed-agentd adds it), session/new → `connection.new_session(project_for(cwd), ...)`
- [x] session/prompt → `send_and_wait_for_completion`; `AcpThread` events → `session/update` via per-entry sent cursor (assistant text/thought chunks from `MessageContent::source_blocks`, tool_call / tool_call_update incl. status); all tool content is sent as markdown text for now
- [x] `ToolCallAuthorization` → `session/request_permission` round-trip to the client, answer → `authorize_tool_call`. Only once-options are offered: an "always" answer persists a rule to the host's real settings file and the reload drops the server's in-memory model/provider settings (found by the smoke: it wrote to `~/.config/zed/settings.json`; reverted, and the harness now uses a throwaway HOME)
- [x] session/cancel → `AcpThread::cancel`
- [-] model selection: `--model` works; switching models from the client over ACP config options is not implemented (the GUI's model picker shows nothing for this agent)
- [x] test: `cursor_sends_each_thread_change_exactly_once` (stub `AcpThread` fed v1 updates; the cursor re-emits each change once). The permission and transport paths are covered by the smoke below rather than an in-process test
- [x] e2e (`script/agentd-smoke/`, fake OpenAI-compatible model): direct run (read_file turn, terminal turn with permission, file written); through zed-agentd (`loadSession`/`list` advertised, permission routed, second client lists + replays both turns); GUI killed 1 s into a 4 s turn → turn finished detached, replay shows the answer and `completed` tool. All PASS
- [-] Native-agent `session/load` from threads.db — zed-agentd journal covers reattach; only needed without the daemon
- [ ] Structured diffs: send edit results as v1 `ToolCallContent::Diff` instead of markdown; needs a new-text accessor on `acp_thread::Diff` (a fork seam in `acp_thread`)
- [ ] Run on the real host once plan 037 P3 has SSH: `zed-agentd attach -- eval-cli --acp --model <provider/model>` from the M3 GUI
