# 008: Unified context-overflow flow (native + Claude agents)

- **Plan**: `.plans/023_unified_context_overflow_flow.md`
- **Status**: implemented; see commit referenced in the plan header after close-out
- **Date**: 2026-08-19

## What changed

### A. Claude parity above 320k (req 1, 3)
- `config.rs`: `max_context_tokens` default 80k → **256k**; new
  `claude_context_overflow_tokens` (**320k**, env
  `ZED_AUTO_PROMPT_CLAUDE_CONTEXT_OVERFLOW_TOKENS`).
- `auto_prompt.rs`: Phase 1/2 state machine extracted into
  `context_overflow_outcome(&LlmCallData)` (A2) — no behavior change for native.
- `claude_agent.rs`: `claude_context_overflow_decision` gates on the thread's
  API-reported `token_usage().input_tokens` (> 320k → `NeedsLlmCall` with
  `context_exceeds_limit = true`). No usage data → stay same-thread (no
  guessing). `agent_ui` routes those calls through `decide_with_llm`
  (`use_native_flow`), so Claude reuses the battle-tested Phase 1
  (same-thread summarize) → Phase 2 (new thread) machinery, including
  pending-question fast path and the issue-007 RetryAfterBackoff guard.
- The "never `ContextOverflow` / never `force_new_thread`" contract in the
  module doc now documents the >320k exception.
- Drive-by fix: `claude_decision_needs_llm` (non-default-feature build) used
  `configured.model` and `HIDDEN_ORCHESTRATOR_PROMPT` — identifiers that don't
  exist in that cfg; now uses its `model` param + `CLAUDE_SYSTEM_PROMPT`.

### B. New-thread payload (req 1, 2)
- **B1 `## 4. Addition request`**: new `auto_prompt::append_addition_request`.
  The draft is stashed at Phase-1/clarify time (UI `DRAFT_STASH` keyed by
  session id) because Phase 1's `set_message` + `send()` destroy it; the
  new-thread dispatch takes the stash (or live-reads the editor for the
  voluntary-summary path) and appends `## 4. Addition request`, then clears
  the source editor.
- **B2 boundary cut**: `extract_decision_prompt` stops at `## 4. Addition
  request` so the carried draft never pollutes decision extraction.
- **B4 no more "blinking"**: new threads are created via
  `AgentInitialContent::ContentBlock` with the full
  `with_first_prompt_context` payload (summary inlined as `## 1`). The old
  `ThreadSummary` path inserted an `@thread` mention that made the new thread
  re-summarize the old one with a full LLM call — removed from both
  `dispatch_action` and the `AutoPromptNewThread` handler.
  `set_continued_from` is preserved (sidebar link). Manual user-clicked
  thread-summary inserts are unchanged. `build_auto_prompt_follow_up` removed
  (orphaned).

### C. Same-thread below 256k (req 4)
- `dispatch_action`: `use_new_thread = force_new_thread || (native && tokens >
  max_context_tokens)`. `same_thread_token_threshold` auto mode now resolves
  to `max_context_tokens` (256k) instead of 50%-of-model-max capped at 100k;
  explicit positive overrides still honored. ACP guard only stops *ordinary*
  same-thread continuations whose active thread vanished — `force_new_thread`
  (Claude Phase 2) reaches the new-thread branch.

### D. Low confidence → pros/cons clarify (req 5)
- New `AutoPromptOutcome::ClarificationRequest` + `CLARIFY_REGISTRY`
  (once per chain, sticky by design).
- Fires in the WantsStop path when the worker's last message presents
  options/decision points (`pending_question::mentions_decision_point`) and
  has no pros/cons layout yet (`has_pros_cons_layout`); user-deferral /
  genuine user-input cases are excluded upstream by
  `is_waiting_for_user_decision`. Sent same-thread, with the draft stashed.

### E. Housekeeping on stop (req 6)
- `config.rs`: `housekeeping_command` (default `Some("housekeeping")`, env
  `ZED_AUTO_PROMPT_HOUSEKEEPING_COMMAND`; empty string or `null` disables).
- In `run_auto_prompt`'s Stopped arm (automatic path only): resolves the
  command against the thread's session capabilities (slash commands +
  skills), sends `/{command}` same-thread once per session
  (`HOUSEKEEPING_REGISTRY`), then the next stop truly stops. Unresolvable
  command logs and stops normally — never fails the chain.

## Config

| Key | Default | Env |
|---|---|---|
| `max_context_tokens` | 256000 (was 80000) | `ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS` |
| `claude_context_overflow_tokens` | 320000 | `ZED_AUTO_PROMPT_CLAUDE_CONTEXT_OVERFLOW_TOKENS` |
| `housekeeping_command` | `"housekeeping"` | `ZED_AUTO_PROMPT_HOUSEKEEPING_COMMAND` |

`~/.config/zed/auto_prompt.json` pinned the old 80000 — updated to 256000
(stale override would have defeated the new gate).

## Validation

- `cargo clippy -p auto_prompt -p agent_ui --release --all-targets
  --all-features -- --deny warnings` — clean.
- `cargo test -p auto_prompt` — 365 + 40 tests pass (21 new: config defaults,
  overflow phases incl. voluntary-summary skip, addition-request append +
  decision boundary, clarify once-guard + detectors, Claude gate).
- `cargo test -p agent_ui auto_prompt::tests` — 3 new stash/registry tests pass.
- Manual e2e with a real >320k Claude session still recommended (plan risk).
