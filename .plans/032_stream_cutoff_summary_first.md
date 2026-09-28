# 032 — Stream cutoff (MaxTokens): summary-first fork instead of context-losing bare fork

Status: done (2026-09-28)

## Problem

When a worker's response stream is cut off mid-generation because the model's
context window filled (provider reports `StopReason::MaxTokens`), auto_prompt
dispatches a brand-new thread immediately with a bare
"Context limit reached. Continue from where we left off." prompt
(`decide_with_context`, MaxTokens branch).

`build_prompt_summary` ignores the last assistant message
(`_last_assistant_message` is unused), so the new thread's only context is the
thread title + "context limit reached (MaxTokens)". The entire conversation
context is lost — no summary is ever produced.

Secondary: `mod.rs` logs "will apply backoff retry" for MaxTokens, but the
MaxTokens branch returns `DispatchNow` before any backoff/retry logic — no
retry ever happens and the log is misleading.

## Root cause chain

1. `acp_thread::run_turn` L4004: `stop_reason == MaxTokens` → `had_error = true`,
   turn ends with `MaxOutputTokensError`.
2. `on_thread_stopped` → `run_auto_prompt` → logs "will apply backoff retry"
   (never happens).
3. `decide_with_context` L1451: MaxTokens → `DispatchNow` bare continuation,
   bypassing the Phase 1/2 summarize machine entirely.
4. `dispatch_action`: tokens over threshold → new thread with title-only summary.

## Fix (matches desired UX: over context → summarize first, then fork; under → resume)

- Classify MaxTokens stops via `token_usage()` + `model.max_token_count()`:
  - **Window full** (`input + output + reserve >= model window`): a same-thread
    Phase 1 summarize request would be rejected by the API (input alone fills
    the window). Produce the 4-part handoff summary with a bounded
    **orchestrator LLM call** (title + original task + char-boundary-safe tail
    of the truncated output), then fork with the real summary
    (`force_new_thread = true`). Falls back to the legacy bare fork if the
    orchestrator call fails, so behavior never regresses.
  - **Output-cap cutoff** (window has room): the context is intact — resume
    **same-thread** with a plain "resume where you stopped" prompt. No fork.
  - **Voluntary summary already present**: precheck light path routes into the
    existing Phase 1/2 machine, whose `skip_phase_1` forks straight to Phase 2
    with the existing summary (unchanged).
- Fix the stale "will apply backoff retry" log.

## Tasks

- [x] Add `max_tokens_context_window_full` pure classifier + resume prompt const
- [x] Add `tail_at_char_boundary` helper (multibyte-safe tail extraction)
- [x] Add `cutoff_context_full` flag to `LlmCallData` (+ all construction sites)
- [x] Rework `decide_with_context` MaxTokens branch (classify, no bare fork)
- [x] Route voluntary-summary light path into Phase 1/2 machine on window-full
- [x] Add `cutoff_context_full_outcome` (bounded orchestrator summary → fork, legacy fallback)
- [x] Hook the cutoff branch at the top of `decide_with_llm`
- [x] Fix misleading MaxTokens log in `agent_ui/auto_prompt/mod.rs`
- [x] Unit tests: classifier table, tail helper multibyte, fork success/fallback
- [x] `cargo test -p auto_prompt` + scoped clippy + fmt
- [x] Commit + push, close plan with results

## Notes

- Orchestrator summary call reuses `call_language_model` + `AutoPromptResponse.thread_summary`
  (JSON contract) — no new parser.
- Fork construction mirrors Phase 2 (`with_first_prompt_context`,
  `extract_summary_next_steps` terminal/nothing-left handling, slash-command
  preservation, token-count reset, `auto_claim_plan`).
- Usage-less providers (no token usage reported) keep legacy behavior
  (classifier returns false → bare fork path), documented in tests.

## Result

- `cargo test -p auto_prompt`: 417 lib + 43 context tests pass (6 new:
  classifier table, tail multibyte, fork success / fallback / terminal).
- `cargo clippy -p auto_prompt --tests`: clean.
- `cargo check -p agent_ui`: clean.
- fmt note: repo files are not clean under local stable rustfmt (style
  edition drift); only the new/edited code was formatted and two unrelated
  fmt-churn hunks were hand-reverted. Do NOT run `cargo fmt -p auto_prompt`
  with a stable toolchain here — it reformats unrelated files.
- Fix commit: 8a327152cf (develop).
