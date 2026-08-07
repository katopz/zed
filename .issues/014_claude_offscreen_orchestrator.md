# 014 — Claude auto_prompt: off-screen hidden-thread orchestrator

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
- [x] Feature flag `claude-hidden-orchestrator` (off by default) for GOAT gate.
- [x] No-tool-leak mitigation: orchestrator system prompt forbids tool use,
      demands JSON-only output; reject non-JSON replies as stop.
- [x] Unit tests: prompt contract (no-tool, JSON schema), parse roundtrip,
      tool-leak reply -> stop.
- [x] Async spawn tests (TestAppContext + Project + StubAgentConnection):
      `hidden_thread_async` module covers continue roundtrip, stop verdict,
      tool-leak -> stop, low-confidence -> stop. Verifies the hidden session
      spawns, the verdict round-trips, and outcomes map correctly — the
      GOAT items verifiable without a live Claude Code run.

## GOAT gate (verify before promoting to default)
Items verifiable via tests (DONE):
- [x] Verdict round-trips: worker output -> hidden session -> JSON verdict ->
      Continue/Stopped outcome. Covered by `test_hidden_thread_continue_verdict_roundtrips`
      and `test_hidden_thread_stop_verdict_stops`.
- [x] No tool-leak in parse path: non-JSON reply -> Stopped, never loops.
      Covered by `test_hidden_thread_tool_leak_reply_stops`.
- [x] Low-confidence continue -> Stopped. Covered by
      `test_hidden_thread_low_confidence_stops`.

Items requiring a live Zed run with the feature on (NOT done here):
- [ ] Hidden session spawns and is invisible (not in sidebar/retained_threads).
      The test uses a bare Entity<AcpThread> that is never registered in any
      panel list — but sidebar invisibility can only be confirmed visually.
- [ ] No tool-leak in production: the prompt forbids tools, but a real Claude
      Code session has tool access. The test verifies the PARSE-side guard;
      production tool-leak prevention needs a live run.
- [ ] Concurrency safe: worker + orchestrator don't deadlock/double-cancel.
      The test is single-threaded; real concurrency needs a live run.
- [ ] No Anthropic API key in Zed required (the whole point). The test uses a
      FakeLanguageModel placeholder and never calls a real API; the
      no-key-in-production guarantee needs a live run.

## Risks (acknowledged, not blockers)
- Latency: full second Claude session per decision (3-10s vs ~1s LLM call).
- Shared Claude quota: two concurrent sessions during decision window.
- Tool-leak: orchestrator is a full agent; mitigated by prompt + JSON guard.
