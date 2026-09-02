# Claude Sub-Agent Verdict (ping-pong second opinion)

Status: IMPLEMENTED (phases 1-6) — GOAT benchmark pending (`.issues/016`) before promoting to default

Commit: e5b496bf46 (phases 1-5), phase 6 commit (see plan 029)

## Goal

Add a native-agent button that throws the thread's `#Summary` to a **new Claude agent
thread** with a `#Verdict...` instruction, then runs a bounded ping-pong — native agent
reasons about the verdict and replies **in the same Claude thread** — until both agree.
During the whole negotiation auto_prompt must be fully suppressed for the verdict
thread (and must not derail the parent), so the whole thing runs as a sub-agent task.

## Verdict: feasible — ~80% of the substrate already exists in this fork

| Need | Existing substrate | Location |
|---|---|---|
| Verdict struct + parser | `ClaudeVerdict {continue_work, next_prompt, reason, confidence}`, `parse_claude_response`, `extract_json_object` | `crates/auto_prompt/src/claude_agent.rs` |
| Summary detection | `SUMMARY_MARKERS`, `contains_summary`, `is_auto_prompt_summary_response`, `truncate_last_paragraphs` | `crates/auto_prompt/src/claude_agent.rs`, `auto_prompt.rs` |
| Subagent create + SAME-thread follow-up | `SpawnAgentTool`: `create_subagent(label)` → `send()` returns final output; `resume_subagent(session_id)` = ping-pong in same thread | `crates/agent/src/tools/spawn_agent_tool.rs` |
| Model override precedent | `AgentSettings::subagent_model` overrides inherited model in `Thread::new_subagent` | `crates/agent/src/thread.rs` L1339-1367 |
| Claude via subscription (no API key) | hidden Claude Code session on same ACP connection (`claude-hidden-orchestrator`) | `crates/auto_prompt/src/claude_agent.rs` `judge_with_hidden_session` |
| Suppression kill-switch precedent | `auto_prompt::paused()` checked in `decide_precheck` + `decide_claude` | `crates/auto_prompt/src/auto_prompt.rs` L203 |
| Button anchor | message-editor toolbar already renders `render_add_context_button` + `render_follow_toggle` + `render_auto_prompt_toggle` | `crates/agent_ui/src/conversation_view/thread_view.rs` L4781-4787 |
| Test seams | `FakeThreadEnvironment` with fake `subagent_handle`; `test_hidden_thread_*` suite | `crates/agent/src/tests/mod.rs`, `claude_agent.rs` |

## What you miss (race-condition analysis — the important part)

1. **auto_prompt only fires from UI-level hooks.** `ConversationView::handle_thread_event`
   → `on_thread_stopped` (`agent_ui/src/auto_prompt/mod.rs` L802). A thread whose
   ThreadView is never mounted never triggers it. That is exactly why the existing
   hidden-orchestrator sessions don't race. So "do it in a sub-agent" is correct and is
   already ~90% of the fix.
2. **Remaining leak:** if the user *opens* the verdict subagent's ThreadView mid-loop,
   its `Stopped` events route through the same handler and can fire auto_prompt at the
   verdict thread, injecting a competing continue prompt mid-negotiation. Global
   `paused()` is too blunt (it would silence sibling agents too). Need a **session-scoped
   suppression registry** checked in `run_auto_prompt` entry, `decide_claude`, and the
   watchdog HALT path (`start_watchdog`).
3. **Model pinning:** `spawn_agent` subagents inherit the parent model, then the global
   `subagent_model` may override. A Claude verdict agent needs its own `verdict_model`
   setting — do not hijack `subagent_model`.
4. **Two Claude auth paths:** Anthropic API key → native `Thread` subagent works. Claude
   Code subscription auth is invisible to `LanguageModelRegistry` → verdict session must
   ride the ACP connection (hidden-session pattern). Phase the second path separately.
5. **Bounds:** every round = 2 LLM calls. Without a cap this is a death spiral
   (cf. `.issues/007_auto_prompt_api_exhaustion_death_spiral.md`). Hard max-rounds +
   escalate-to-user on exhaustion.
6. **Parent termination is already safe-ish:** during the ping-pong the parent turn is
   inside an in-progress tool call, so `has_in_progress_tool_calls` blocks
   `on_thread_stopped`. When the parent finally emits its agreed `#Summary`, the existing
   summary fast path (`summary_declares_terminal`) stops the chain — but check the
   post-stop housekeeping hook (`agent_ui/src/auto_prompt/mod.rs` L1447) doesn't spawn
   new work off that stop.
7. **Depth limit:** `MAX_SUBAGENT_DEPTH = 1` — verdict tool must be invoked from the root
   thread only; button hidden in subagent views (`is_subagent()`).
8. **Teardown:** registry entries must clear on thread close / cancel, mirroring
   `clear_summary_for_session`.

## Protocol

- Verdict thread message 1: `#Summary` + `#Verdict` instruction: reply MUST start with
  `#Verdict: AGREE` or `#Verdict: REVISE` followed by bullet reasons.
- Native agent parses verdict (pure function, unit-testable), fixes what it can, replies
  in the SAME session (`resume_subagent`) with evidence or its own `#Verdict: AGREE`.
- Agreement = both sides AGREE on consecutive messages, **and** the final summary text is
  identical on both sides (avoid vague agreement).
- Cap: `max_rounds` (default 3). On exhaustion: return last disagreement to the user.

## Plan

- [x] Phase 1: `acp_thread::verdict` module — suppression registry
      (`register/complete/is_active`/`rounds`, TTL 30min), `parse_verdict` +
      `VerdictKind` protocol functions, 6 unit tests
- [x] Phase 2: suppression hooks in `run_auto_prompt` entry (manual clicks
      still bypass) + `decide_precheck` + `decide_claude` + watchdog HALT;
      TTL expiry is the teardown (no leak path)
- [x] Phase 3: `request_verdict` tool (`crates/agent/src/tools/request_verdict_tool.rs`)
      via new `ThreadEnvironment::create_verdict_subagent` (model pinned to
      `agent.verdict_model`, falls back to parent model); rounds budget
      enforced pre-resume; output carries `round`/`max_rounds`
- [x] Phase 4: button in message-editor toolbar (root views only; disabled
      without a summary per `auto_prompt::message_looks_like_summary`); click
      sends the ping-pong instruction; settings `agent.verdict_ping_pong`,
      `agent.verdict_model`, `agent.verdict_max_rounds` (default 3)
- [x] Phase 5 (GOAT gate): feature flag `verdict_ping_pong` default false —
      tool unregistered and button hidden when off; benchmark
      (threads-with-verdict vs without: post-hoc fix rate, rounds, token cost)
      still pending before promoting to default
- [x] Phase 6: Claude Code subscription path via ACP-connection verdict session
      (`.plans/029_claude_code_verdict_reviewer.md`; `agent.verdict_reviewer =
      "claude_code"`; teardown limitation in `.issues/016`)

## Notes

- Naming: `#Verdict` (user wrote "#Vedict", treated as typo).
- Ping-pong stays visible in the parent thread as tool calls — user can watch and cancel.
- Implemented as option A (prompt-driven, tool-per-round): visible, cancellable, and each
  parent reasoning step is a real LLM turn.
- Deviation from the original plan: the registry lives in `acp_thread::verdict`, not
  `auto_prompt::verdict_loop` — `auto_prompt` does not depend on `agent`, and `acp_thread`
  is the one crate all consumers (agent tool, auto_prompt deciders, agent_ui watchdog)
  already depend on.
- Validation: `cargo test -p acp_thread verdict` (6), `cargo test -p agent request_verdict`
  (2), full `cargo test -p agent` (735 passed), `cargo test -p agent_ui conversation_view::tests`
  (116, incl. watchdog), `cargo test -p auto_prompt` (40), targeted clippy clean.
