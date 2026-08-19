# Plan 023: Unified context-overflow flow for native + Claude agents

## Problem (user request, verbatim)

> for claude agent, it should work the same flow as native agent when context reach more than 320k
>
> for both agent
> 1. it should trigger new thread with summary and any text in input text box as `#4 Addition request` if has
> 2. no need to process summary as blinking on new thread anymore because mostly it already summary in last paragraph rn, consider redundant
> 3. when context over 256k, if recent paragraph is not summary do ask agent to summary and trigger new thread if need
> 4. if context not over 256k do answer in same thread
> 5. reason recent paragraph, if low confident to answer, ask agent to clarify pros/cons before make any decision
> 6. if no tasks remain do check house keeping's skill

## Current flow (verified in code)

- `crates/auto_prompt/src/auto_prompt.rs` `decide_with_llm` (L1232):
  - pending-question fast path (`pending_question.rs`) runs first;
  - `context_exceeds_limit` = effective tokens > `max_context_tokens` (**default 80_000**, `config.rs` L86);
  - Phase 1 (`summary_state==0`): return `ContextOverflow` → UI sends "summarize" to SAME thread
    (`agent_ui/src/auto_prompt/mod.rs` L1007, overwrites the editor via `set_message`);
  - voluntary-summary skip: `looks_like_voluntary_summary(last_msg)` → straight to Phase 2 (already
    implements "if recent paragraph IS summary, don't re-ask");
  - Phase 2: `force_new_thread=true` action, first prompt built by `with_first_prompt_context`
    (`## 1. Thread Summary` / `## 2` / `## 3. Decision`).
- `crates/auto_prompt/src/claude_agent.rs`: doc'd "Never return `ContextOverflow` … never
  `force_new_thread`" — Claude Code manages its own compaction, always same-thread.
- `agent_ui/src/auto_prompt/mod.rs` `dispatch_action` (L280):
  - `use_new_thread = force_new_thread || (is_native_agent && effective >= same_thread_threshold)`
    (auto threshold = 50% of model max input, capped 100k);
  - new thread uses `AgentInitialContent::ThreadSummary` → `insert_thread_summary` → `@thread`
    mention → LLM re-summarizes old thread with a loading indicator (the "blinking") even though
    `last_assistant_message` already IS the summary — redundant token burn + delay;
  - "ACP agents must NEVER create new threads" guard (L381) blocks Claude new threads entirely.
- Phase 1 `set_message` **overwrites the user's draft** in the input box; after `send()` the editor
  is cleared, so the draft is lost before Phase 2 can carry it anywhere.
- Terminal `Stopped` just shows a toast; no hygiene/housekeeping hook exists.

Substrate-first check: done — the overflow state machine (`SUMMARY_REGISTRY`, Phase 1/2,
voluntary-summary skip), slash-command preservation (`original_user_message` starts with `/`),
and pending-question fast path all exist; this plan extends them rather than building a parallel
system. No matches for "housekeeping" anywhere — B.6 is new but reuses the slash-command dispatch
substrate.

## Design

### A. Claude parity above 320k (req 1 + 3)

- New config `claude_context_overflow_tokens` (default `320_000`, env
  `ZED_AUTO_PROMPT_CLAUDE_CONTEXT_OVERFLOW_TOKENS`) in `auto_prompt/src/config.rs`.
- Bump `max_context_tokens` default `80_000` → `256_000` (req 3: the "over 256k" gate for the
  summarize→new-thread dance; native agents).
- Extract Phase 1/2 logic from `decide_with_llm` into `pub(crate) fn context_overflow_outcome(..)`
  (DRY) consumed by both paths.
- `claude_agent.rs`: in `decide_claude`/`claude_decision_needs_llm`, compute effective tokens from
  `thread.token_usage()` (actual) with chars/4 fallback; if > `claude_context_overflow_tokens` →
  return `NeedsLlmCall` routed to the shared overflow flow (Phase 1 same-thread summarize →
  Phase 2 new thread). Below 320k: unchanged same-thread behavior. Update the module doc
  ("never ContextOverflow" no longer holds above the threshold).

### B. New-thread payload (req 1 + 2, both agents)

- **B.1 `## 4. Addition request`**: stash-and-carry the input-box draft.
  - UI-side `DRAFT_STASH: RwLock<HashMap<String /*session_id*/, String>>` in
    `agent_ui/src/auto_prompt/mod.rs` (crate-local; no auto_prompt crate API change).
  - Phase 1 handler (ContextOverflow arm, L1007): read `message_editor` text BEFORE
    `set_message`; if non-empty → stash by session id.
  - `dispatch_action` new-thread branch: `draft = stash.take(session).or(live editor text)`;
    if non-empty → append `## 4. Addition request\n\n{draft}` to the first prompt; clear the
    source editor. Voluntary-summary skip path (no Phase 1) still works because we live-read.
  - `with_first_prompt_context` gains an `addition_request: Option<&str>` section.
  - **Update `extract_decision_prompt`/`strip_first_prompt_wrapper`** to cut `## 3. Decision`
    extraction at the `## 4. Addition request` boundary (they currently take everything after
    `## 3.`).
- **B.2 No ThreadSummary re-summarization**: in `dispatch_action` and the
  `AutoPromptNewThread` handler (`agent_panel.rs` L488), replace
  `AgentInitialContent::ThreadSummary` with `AgentInitialContent::ContentBlock` whose text is the
  full `with_first_prompt_context` payload (summary already inlined as `## 1`). No `@thread`
  mention → no LLM re-summarization → no blinking loading indicator. Keep
  `set_continued_from` metadata (sidebar link) — `external_thread*` already returns the id.
  Manual `initial_content_for_thread_summary` (user-clicked) keeps current behavior.

### C. Same-thread below 256k (req 4)

- `dispatch_action`: `use_new_thread = action.force_new_thread
  || (is_native_agent && effective_tokens > max_context_tokens)`.
  - `same_thread_token_threshold` becomes redundant; keep the field for compat, auto mode now
    resolves to `max_context_tokens` (below that = always same thread). Doc update in `config.rs`.
- Relax the "ACP agents must NEVER create new threads" guard: allow new thread when
  `action.force_new_thread` (Claude Phase 2 at >320k); keep the stop guard for ordinary
  same-thread continuations whose active thread vanished.

### D. Low-confidence → clarify pros/cons (req 5)

- New outcome `AutoPromptOutcome::ClarificationRequest(AutoPromptAction)` in `auto_prompt.rs`.
- In `decide_with_llm` response evaluation: when confidence < phase threshold AND the recent
  paragraphs present options/decision points (extend `pending_question::match_question_pattern`
  with an options detector — "option a/b", "approach 1/2", "should i x or y", "pros/cons") AND the
  worker hasn't already produced a pros/cons layout AND it's not a genuine user-input request
  (credentials/keys — reuse pending_question's user-input patterns) → return
  `ClarificationRequest` with a same-thread prompt: "Before deciding, list the pros/cons of each
  option you mentioned, then recommend one."
- UI arm in `run_auto_prompt` (same-thread `set_message` + send, like Phase 1).
- Loop guard: `CLARIFY_REGISTRY` (session → u32, like `SUMMARY_REGISTRY`), fires at most once per
  chain; cleared when the chain restarts.

### E. No tasks remain → housekeeping skill (req 6)

- New config `housekeeping_command: Option<String>` (default `Some("housekeeping")`, env
  `ZED_AUTO_PROMPT_HOUSEKEEPING_COMMAND`; empty string = disabled).
- In `run_auto_prompt` `Stopped` arm (automatic path only, not manual): before the final toast,
  if command configured AND resolvable as an available skill/slash-command (PromptStore/skills
  lookup) AND not already run for this session (`HOUSEKEEPING_REGISTRY`, once per chain) →
  send same-thread prompt `{command}` (skill activates like the preserved `/optimize` slash
  command) and return; the next stop re-evaluates and, with the registry set, truly stops.
  Unresolvable skill → log + normal stop (never hard-fail the chain).

## Files touched

| File | Change |
|---|---|
| `crates/auto_prompt/src/config.rs` | defaults 256k, `claude_context_overflow_tokens` 320k, `housekeeping_command`, docs |
| `crates/auto_prompt/src/auto_prompt.rs` | extract `context_overflow_outcome`, `ClarificationRequest` outcome + registry, `with_first_prompt_context` #4 section, `extract_decision_prompt` boundary |
| `crates/auto_prompt/src/claude_agent.rs` | token gate >320k → shared overflow flow; doc updates |
| `crates/auto_prompt/src/pending_question.rs` | options/pros-cons detector reuse for D |
| `crates/agent_ui/src/auto_prompt/mod.rs` | draft stash, dispatch_action thresholds + ContentBlock switch + Claude guard, ClarificationRequest arm, housekeeping hook |
| `crates/agent_ui/src/agent_panel.rs` | `AutoPromptNewThread` handler: ContentBlock switch (+draft) |
| `crates/auto_prompt/tests/context_helpers_test.rs` (+ inline `mod tests`) | coverage below |

## Tasks

- [x] A1. `config.rs`: `max_context_tokens` default 256_000; add `claude_context_overflow_tokens` (320_000) + env var; serde/default/tests
- [x] A2. `auto_prompt.rs`: extract Phase 1/2 into `context_overflow_outcome`; native path calls it (no behavior change)
- [x] A3. `claude_agent.rs`: effective-token gate → route >320k through `context_overflow_outcome`; below unchanged; module doc rewrite
- [x] B1. `with_first_prompt_context`: addition carried as `## 4. Addition request` — implemented as `auto_prompt::append_addition_request` applied at dispatch time (avoids touching all 13 `with_first_prompt_context` call sites; same payload)
- [x] B2. `extract_decision_prompt` cuts at `## 4. Addition request` boundary (`strip_first_prompt_wrapper` untouched — same-thread prompts never carry `## 4`)
- [x] B3. `agent_ui/auto_prompt/mod.rs`: `DRAFT_STASH`; stash in ContextOverflow + ClarificationRequest arms pre-`set_message`; take + append `## 4.` + clear editor in `dispatch_action` new-thread branch (live-read fallback covers the voluntary-summary path)
- [x] B4. `dispatch_action` + `agent_panel.rs` `AutoPromptNewThread`: `ThreadSummary` → `ContentBlock` (summary inlined, keep `set_continued_from` via `action.from_session_id`, keep focus behavior; orphaned `build_auto_prompt_follow_up` removed)
- [x] C1. `dispatch_action`: `use_new_thread = force_new_thread || (native && tokens > max_context_tokens)`; `same_thread_token_threshold` auto = overflow threshold (explicit overrides honored); ACP guard allows `force_new_thread`
- [x] D1. `AutoPromptOutcome::ClarificationRequest` + `CLARIFY_REGISTRY` (once per chain, sticky — deliberately NOT cleared on stop so stop→clarify→stop cannot loop)
- [x] D2. `pending_question`: `mentions_decision_point` + `has_pros_cons_layout`; low-confidence WantsStop branch returns it (after `is_waiting_for_user_decision`, before plan fallback; also added arm to thread_view.rs manual-retry match)
- [x] D3. UI arm: same-thread clarify dispatch in `run_auto_prompt` (with draft stash)
- [x] E1. `config.rs`: `housekeeping_command` (default `housekeeping`, empty/null = off)
- [x] E2. `run_auto_prompt` Stopped arm: session-capabilities resolve check (commands + skills) + `HOUSEKEEPING_REGISTRY` once-guard + same-thread dispatch
- [x] F1. Tests: 21 new in auto_prompt + 3 in agent_ui — config defaults/serde-null, overflow Phase 1/Phase 2/voluntary-skip, #4 append + decision-boundary cut, clarify once/skip detectors, Claude gate pure check, draft stash roundtrip, housekeeping once-guard
- [x] F2. `./script/clippy`-equivalent clean on touched crates (`cargo clippy -p auto_prompt -p agent_ui --release --all-targets --all-features -- --deny warnings` → exit 0)
- [x] F3. Updated `~/.config/zed/auto_prompt.json` `max_context_tokens` 80000 → 256000 (stale override would defeat A1)
- [x] F4. `.docs/008_unified_context_overflow_flow.md` + this close-out

## Gate / perf notes

- Orchestration-path only (fires on thread stop); no hot-loop changes → config values are the
  gate (env vars already exist for all three knobs). No cargo feature flag needed.
- Token savings (user's GOAT concern): B.2 removes a full LLM re-summarization per overflow
  handoff; B.1 stops silent draft loss; D avoids wasted stop→summary cycles on ambiguous turns.

## Risks

- `extract_decision_prompt` consumers (follow_up builder) must tolerate the new `## 4.` tail —
  covered by boundary cut (B2) + tests.
- Claude new-thread continuation depends on the ACP session supporting fresh sessions with the
  same agent — `external_thread` already spawns per-agent threads; verify with a manual
  >320k Claude session before closing.
- Housekeeping default name `housekeeping` may not exist as a skill in some machines →
  availability-checked, silently skips (E2).
