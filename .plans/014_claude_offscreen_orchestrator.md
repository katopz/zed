# 014 — Claude auto_prompt: off-screen hidden-thread orchestrator

Moved from `.issues/014_claude_offscreen_orchestrator.md` — this is a feature,
not a bug.

## Problem
`auto_prompt::claude_agent::decide_claude` hard-requires Zed's default model
provider to be Anthropic (`claude_agent.rs:115-122`). Anyone without an
Anthropic API key configured in Zed — including the operator who uses Claude
Code as a *fallback for when GLM hits its rate limit* — gets `NoAction` on
every stop. The Claude auto-prompt chain is dead at the gate.

Why the guard exists: the current orchestrator is a `LanguageModelRegistry`
streaming completion, which needs a real API key. Claude Code's own auth
(browser/subscription) is invisible to that registry.

## Why not the obvious alternatives (rejected)
- **GLM as orchestrator** — bogus for this operator: Claude Code is used
  *because* GLM is rate-limited. A GLM orchestrator dies exactly when it's
  needed most.
- **Subagent** — bogus on control-flow grounds: both subagent mechanisms
  (`spawn_agent` tool in `crates/agent/`, Claude Code's internal Task tool)
  require an *active turn*. Auto-prompt fires *after* the worker stops,
  out-of-band. No turn to spawn within.

## Decision: off-screen hidden Claude thread (Option A)
Reuse Claude Code's own auth by spawning a **second, invisible Claude Code
session** on the same connection, sending it the worker's last 2-3 paragraphs
plus the continue/stop question, reading its reply, and parsing it as a
verdict. No API key required. Independent decider preserved.

## Architecture
```
worker (claude-acp) stops
  -> decide_claude [sync]:
       clone connection + project from worker thread
       stash in LlmCallData
  -> decide_claude_with_hidden_thread [async]:
       connection.new_session(project, work_dirs) -> hidden Entity<AcpThread>
         (NEVER registered in AgentPanel.retained_threads -> invisible)
       hidden.send(orchestrator_prompt) -> await stop
       hidden.last_assistant_message_text() -> verdict JSON
       parse_claude_response -> Continue/Stopped
       drop hidden (cancelled/cleaned up)
  -> dispatch verdict back to ORIGINAL worker thread
```

## Implementation tasks
- [x] Extend `LlmCallData` with `connection: Option<Rc<dyn AgentConnection>>`
      and `project: Option<Entity<Project>>` (None on native path).
- [x] `decide_claude`: capture `connection.clone()` + `project.clone()` from
      the worker thread; remove/relax the Anthropic-provider guard.
- [x] New `decide_claude_with_hidden_thread(data, cx)`:
      - `new_session` -> hidden thread (never registered in panel)
      - build orchestrator prompt (system + worker's last 2-3 paragraphs)
      - `send` + await stop with timeout
      - read reply, `parse_claude_response`, map to Continue/Stopped
      - drop hidden thread
- [x] Route in `agent_ui/src/auto_prompt/mod.rs`: Claude path calls
      `decide_claude_async` (dispatcher) which picks hidden vs LLM by cfg.
- [x] Feature flag `claude-hidden-orchestrator` — **now default-on** (no flag
      needed, see Cargo.toml `default = ["claude-hidden-orchestrator"]`).
- [x] No-tool-leak mitigation: orchestrator system prompt forbids tool use,
      demands JSON-only output; reject non-JSON replies as stop.
- [x] **Send HIDDEN_ORCHESTRATOR_PROMPT in the message** (fix in `2c2b78f58a`).
- [x] Unit tests: prompt contract (no-tool, JSON schema), parse roundtrip,
      tool-leak reply -> stop.
- [x] Async spawn tests: `hidden_thread_async` module — 8+ tests covering
      continue roundtrip, stop verdict, tool-leak -> stop, low-confidence,
      missing-next_prompt fallback, missing-confidence, close-session,
      skip-close-when-not-supported.
- [x] **Close hidden session after judgment** (session leak fix).
- [x] **String-aware JSON extraction** (`extract_json_object` rewrite).
- [x] **Programmatic tool-leak guard** (layer 2 of 3): inspect hidden session's
      entry history for ToolCall entries since last user message; stop if found.

## GOAT gate
Items verifiable via tests (DONE):
- [x] Verdict round-trips: worker output -> hidden session -> JSON verdict ->
      Continue/Stopped outcome.
- [x] No tool-leak in parse path: non-JSON reply -> Stopped, never loops.
- [x] Programmatic tool-leak guard: ToolCall in entry history -> Stopped even
      with valid JSON.
- [x] Low-confidence continue -> Stopped.
- [x] Session cleanup: `close_session` called on every exit path.
- [x] JSON extraction robustness: trailing prose, braces/quotes in strings.
- [x] No Anthropic API key required: `test_hidden_thread_never_calls_language_model`
      asserts `FakeLanguageModel::completion_count() == 0` after a full Continue
      verdict roundtrip. The hidden path communicates only via the ACP connection
      (Claude Code's own auth), never through `LanguageModelRegistry`.
- [x] Sidebar invisibility: statically verified by code structure. The hidden
      session is spawned via `AcpConnection::new_session` (the Claude Code path),
      which creates an `Entity<AcpThread>` + inserts into the connection's internal
      `sessions` ref-count map — but does NOT set up `save_thread` observers or any
      `ThreadMetadataStore`/`ThreadsDatabase` interaction. The sidebar is driven by
      `ThreadMetadataStore.entries()`, which is only populated via
      `NativeAgent::register_session` → `save_thread` → `ThreadsDatabase` →
      `ThreadStore.reload` → migration. Since `AcpConnection::new_session` bypasses
      this entire chain, the hidden session is structurally invisible. (Note:
      `NativeAgentConnection::new_session` DOES set up `save_thread` observers, so
      the hidden orchestrator would leak if used with native agents — but it's only
      used for Claude Code/ACP threads, where `claude_decision_hidden` captures the
      worker's `AcpConnection`.)

Items with static + live-run verification:
- [x] Concurrency isolation (static half verified): `test_hidden_thread_concurrent_decisions_isolate`
      spawns two concurrent `decide_claude_with_hidden_thread` calls, verifies
      each gets its own verdict (no cross-contamination), both sessions are
      closed exactly once (no leak), and neither deadlocks. This proves the
      orchestration layer has no shared mutable state between concurrent
      invocations. Live run still needed to confirm no deadlock under real ACP
      protocol multiplexing (two sessions on one OS process/connection).
- [ ] Real Claude Code prompt compliance (no tool-leak in production).
      Static defense: 3-layer guard (prompt forbids tools + programmatic entry
      history check + parse-side JSON rejection). Live run confirms a real Claude
      Code session respects the no-tools constraint.

## Risks (acknowledged, not blockers)
- Latency: full second Claude session per decision (3-10s vs ~1s LLM call).
- Shared Claude quota: two concurrent sessions during decision window.
- Tool-leak: orchestrator is a full agent; mitigated by prompt + programmatic
  guard + JSON guard (3 layers).

## Perf/sec considerations
- **Latency budget**: the hidden session adds 3-10s per auto_prompt decision.
  This is acceptable because auto_prompt fires on STOP (not every tick) — the
  worker is already idle. The operator would otherwise manually decide
  continue/stop, which is slower.
- **Quota cost**: each decision burns a Claude Code turn. At 15s poll intervals
  with ~5 decisions per plan, that's ~25 extra turns per plan. Monitor usage.
- **No background polling**: the orchestrator is one-shot per stop event. It
  does NOT run a persistent loop — it's spawned on demand and dropped after.
- **Session lifecycle**: `new_session` → `send` → `close_session` is the hot
  path. Each is an RPC to the Claude Code process. The 180s timeout bounds
  worst-case latency.

## Live-run verification checklist
Run these after building Zed with the feature (default-on, no flag needed):

### Build
```bash
cargo build --release  # or: cargo run --release
```

### Setup
1. Configure a Claude Code agent server in Zed settings (no Anthropic API key
   required — Claude Code uses its own browser/subscription auth).
2. Set `ZED_LOG_DIR=/tmp/zed-logs` (or check `~/Library/Logs/Zed/` on macOS).
3. Open a project with some work to do (e.g. a TODO in a source file).
4. Start a Claude Code agent thread and give it a multi-step task.
5. Let auto_prompt fire (worker stops → orchestrator decides continue/stop).

### Item 1: Sidebar invisibility (structurally verified, confirm live)
- [ ] During the orchestrator turn (3-10s after worker stop), check the agent
      sidebar: no new thread should appear.
- [ ] After the orchestrator completes, check the sidebar again: still no new
      thread.
- [ ] Grep logs for `spawned hidden orchestrator session` — the session ID
      should NOT match any sidebar entry.

### Item 2: No tool-leak in production
- [ ] Grep logs for `tool-leak` — if the programmatic guard fires, it means
      the real Claude Code session ignored the no-tools prompt and ran a tool.
      The guard stops the chain regardless, but this indicates prompt
      compliance is insufficient and may need strengthening.
- [ ] Check the orchestrator's response: should be pure JSON
      (`{"continue": ...}`). If it contains tool output or prose, the
      parse-side guard (layer 3) will reject it and stop.
- [ ] If the orchestrator returns valid JSON AND used no tools → pass.

### Item 3: Concurrency under real ACP multiplexing
      (static half verified by `test_hidden_thread_concurrent_decisions_isolate`)
- [ ] Run a long worker task (20+ auto_prompt decisions). Monitor for:
      - Deadlock: worker or orchestrator hangs indefinitely (180s timeout
        should fire if so).
      - Double-cancel: `cancel` called on an already-cancelled session.
      - Session confusion: worker receives orchestrator's messages or vice
        versa (different session IDs on the same connection).
- [ ] Grep logs for `timed out after 180s` — indicates potential deadlock.
- [ ] Check the worker thread still responds normally after each orchestrator
      decision (no stuck state).

### Item 4: No Anthropic API key required (structurally verified, confirm live)
- [ ] Ensure NO Anthropic API key is configured in Zed settings.
- [ ] Start a Claude Code thread — auto_prompt should still work (orchestrator
      uses Claude Code's own auth, not the API key).
- [ ] If auto_prompt stops with `No language model configured`, the
      `claude_decision_hidden` guard fired — this is expected when no model is
      configured at all (the `LlmCallData.model` slot needs *some* model, even
      though it's never used for the API call). Configure any dummy model to
      satisfy the struct shape.
