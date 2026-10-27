# decide_with_llm: decompose the 590-line `Ok` arm
Status: open — refactor; not started (filed 2026-10-27)

## Problem
`crates/auto_prompt/src/auto_prompt.rs` is 3,033 lines (under the 3,200 cap,
over the 2,048 soft target). The bulk is one function: `decide_with_llm`
(~704 lines). Its head is already a clean early-return pipeline (context-full
cutoff → summary fast path → pending-question fast path → overflow), but the
`match result { Ok((_raw, mut response)) => { … } Err(err) => { … } }` body
holds ~590 lines in the `Ok` arm alone.

Issue 039 split the file by pure moves; this one is not a pure move — it
changes control flow, so it needs care, not a mechanical extraction.

## Plan
- [ ] Map each early `return` in the `Ok` arm to the `AutoPromptOutcome` it
      produces; group them into phases (response post-processing, pre-stop
      verification, same-thread vs new-thread dispatch).
- [ ] Extract each phase to a `fn` returning `Option<AutoPromptOutcome>`
      (or `ControlFlow`) so `decide_with_llm` reads as the phase sequence.
- [ ] Move the extracted phases to a sibling module if it is cohesive
      (candidate: `verdict_dispatch.rs`).
- [ ] Gate: all `auto_prompt` tests pass unchanged (441 at filing), clippy clean,
      no test edited except for import paths.

## Not in scope
- `claude_agent.rs`: tests moved out in 62591e1a52 (2,353 → 1,260 lines).
- Upstream Zed files over 2,048 lines: splitting them would conflict with
  every upstream sync.
