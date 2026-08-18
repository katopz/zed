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
- [x] **Context-parity fix**: the hidden orchestrator now receives the same
      `plan_summary` (unchecked task counts), `stop_phase`, `current_paths`,
      and `had_error` context as the native-agent GLM path. Before this fix,
      the hidden path's `context_json` had only `session_id` + `iteration_count`
      + `last_assistant_message` — no plan state — so a worker that emitted a
      completion summary with `[ ]` items still in the plan looked "done" and
      the orchestrator stopped. Now `claude_decision_hidden` calls
      `read_plan_files` (same as the native path) and `judge_with_hidden_session`
      calls `build_lightweight_orchestration_context` to build the same
      lightweight context the GLM path uses.
- [x] **Prompt upgrade**: `HIDDEN_ORCHESTRATOR_PROMPT` ported with the
      task-awareness rules from `default_auto_prompt_system_prompt.txt`:
      plan_summary-aware continuation (unchecked tasks → continue),
      "NEVER declare done/blocked when unchecked tasks remain", GPU/benchmark
      tasks are continue-not-stop, permission-seeking auto-answer, stop_phase
      thresholds, lowest-numbered-plan priority. The no-tools + JSON-only hard
      constraints are preserved.

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
- [x] Real Claude Code prompt compliance (no tool-leak in production).
      Static defense: 3-layer guard (prompt forbids tools + programmatic entry
      history check + parse-side JSON rejection). Live run confirms a real
      Claude Code session respects the no-tools constraint.
      STATUS (2026-08-14): live harness built + validated —
      `script/verify-hidden-orchestrator-compliance.sh`. It extracts the exact
      `HIDDEN_ORCHESTRATOR_PROMPT` from `claude_agent.rs` (no drift), composes
      the byte-exact message `judge_with_hidden_session` sends, runs real
      headless `claude` in a bait workspace (unchecked plan file on disk that
      plan_summary references — a leaking judge would Read it, and Read is
      permitted headlessly so a leak is genuinely observable), then asserts:
      zero tool_use/tool_result/toolUseResult events, num_turns == 1, verdict
      parses per `parse_claude_response` (string-aware first-{...}), and rule-1
      compliance (unchecked plan → continue, confidence ≥ 0.5).
      First live attempt was quota-blocked: the operator's Claude subscription
      hit its seven-day limit (resets 2026-08-14 22:00 Asia/Bangkok; the
      harness detects this and exits 2 = BLOCKED with the reset time — not a
      compliance failure). Re-run after reset:
      `script/verify-hidden-orchestrator-compliance.sh` (exit 0 = compliant).
      Static suite re-validated same day: `cargo test -p auto_prompt` →
      329 + 40 passed, 0 failed (incl. all 17 hidden-orchestrator tests).
      STATUS (2026-08-18): **PASS** — `script/verify-hidden-orchestrator-compliance.sh`
      exit 0 (quota window reset): num_turns=1, tool_events=0, 13s, $0.0856.
      Verdict `{"continue": true, "confidence": 0.9, ...}` — rule-1 compliant
      (unchecked bait plan → continue). Corroborated by production sessions
      same day (274d2767/2791a1fd/0ca1c2c1/9315916b): all 0 tool_use, valid
      JSON verdicts, confidence 0.85–0.95, real plan refs (340 T2.1 → T2.3,
      339 T4.1).

## Risks (acknowledged, not blockers)
- Latency: full second Claude session per decision (3-10s vs ~1s LLM call).
- Shared Claude quota: two concurrent sessions during decision window.
- Tool-leak: orchestrator is a full agent; mitigated by prompt + programmatic
  guard + JSON guard (3 layers).

### Operational note: interrupt-noise byproduct (2026-08-18 diagnostics)
Session-jsonl audits show `[Request interrupted by user]` barrages in the
sdk-ts-driven riir/katgpt sessions. Root causes (in order): quota exhaustion
(41× `You've hit your session limit` synthetic messages — parallel agents
share one subscription, and in-flight requests die as interrupts),
orchestrator-initiated aborts (40× `No response requested.` — send-then-cancel
races in idle-loop drivers), and DNS drops (5× ENOTFOUND). The hidden
orchestrator's own abort-after-verdict lifecycle adds benign interrupts too.
None are human ESC presses. Mitigation for the quota layer: stagger parallel
agents or back off when limit-reset synthetic messages appear.

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

> Headless shortcut for Item 2 (no GUI needed):
> `script/verify-hidden-orchestrator-compliance.sh` drives a real Claude Code
> turn through the byte-exact orchestrator message and asserts no-tool-leak +
> JSON compliance. Exit 0 = pass, 1 = violation, 2 = quota-blocked (rerun
> after the printed reset time).

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
- [x] During the orchestrator turn (3-10s after worker stop), check the agent
      sidebar: no new thread should appear.
      VERIFIED (2026-08-18, headless): sidebar reads threads.db; hidden IDs
      absent → no entry can render during the turn.
- [x] After the orchestrator completes, check the sidebar again: still no new
      thread.
- [x] Grep logs for `spawned hidden orchestrator session` — the session ID
      should NOT match any sidebar entry.
      VERIFIED (2026-08-18): 23 spawns in Zed.log (22:09–22:54 window); all 5
      spot-checked IDs (274d2767, 2791a1fd, 0ca1c2c1, 9315916b, 7269d5cc)
      → 0 rows in threads.db `threads` table (authoritative sidebar store,
      queried read-only).

### Item 2: No tool-leak in production
- [x] Grep logs for `tool-leak` — if the programmatic guard fires, it means
      the real Claude Code session ignored the no-tools prompt and ran a tool.
      VERIFIED (2026-08-18): 0 `tool-leak` hits in Zed.log — guard never fired
      in production.
- [x] Check the orchestrator's response: should be pure JSON
      (`{"continue": ...}`). If it contains tool output or prose, the
      parse-side guard (layer 3) will reject it and stop.
      VERIFIED (2026-08-18): 4 production sessions + compliance probe all
      pure JSON.
- [x] If the orchestrator returns valid JSON AND used no tools → pass.
      VERIFIED (2026-08-18): PASS (see probe + production evidence above).

### Item 3: Concurrency under real ACP multiplexing
      (static half verified by `test_hidden_thread_concurrent_decisions_isolate`)
- [x] Run a long worker task (20+ auto_prompt decisions). Monitor for:
      - Deadlock: worker or orchestrator hangs indefinitely (180s timeout
        should fire if so).
      - Double-cancel: `cancel` called on an already-cancelled session.
      - Session confusion: worker receives orchestrator's messages or vice
        versa (different session IDs on the same connection).
      VERIFIED (2026-08-18): 23 decisions in Zed.log (≥20); 0 error/cancel/
      fail lines from `auto_prompt::claude` — no deadlock, double-cancel, or
      session confusion observed.
- [x] Grep logs for `timed out after 180s` — indicates potential deadlock.
      VERIFIED (2026-08-18): 0 hits in Zed.log.
- [x] Check the worker thread still responds normally after each orchestrator
      decision (no stuck state).
      VERIFIED (2026-08-18): verdict chain shows progressive work across
      decisions (plan 340 T2.1 → 340 T2.3 → plan 339 T4.1, 22:31→22:48) —
      worker kept accepting and executing next_prompts.

### Item 4: No Anthropic API key required (structurally verified, confirm live)
- [x] Ensure NO Anthropic API key is configured in Zed settings.
      VERIFIED (2026-08-18): no `language_models`/anthropic block in user
      settings; no `anthropic`/`api_key` in project `.zed/settings.json`.
- [x] Start a Claude Code thread — auto_prompt should still work (orchestrator
      uses Claude Code's own auth, not the API key).
      VERIFIED (2026-08-18): 23 working decisions today with no API key.
- [x] If auto_prompt stops with `No language model configured`, the
      `claude_decision_hidden` guard fired — this is expected when no model is
      configured at all (the `LlmCallData.model` slot needs *some* model, even
      though it's never used for the API call). Configure any dummy model to
      satisfy the struct shape.
      N/A (2026-08-18): guard never fired — decisions succeeded without key.
