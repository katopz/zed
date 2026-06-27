# Plan 010: auto_prompt should answer the agent's pending question, not stop+summarize

## Problem

When an agent ends its turn with a direct question to the user (e.g. "Which do
you want? Option A or Option B?", "Want me to do that?"), auto_prompt currently
falls through to the default `decide_with_llm` flow:

1. orchestration LLM gets the lightweight context (last msg + plan summary),
2. it cannot "continue work" because there is no work — there is a question,
3. it returns `should_continue=false` with no prompt,
4. auto_prompt hits the pre-stop verification path,
5. eventually emits a **"Stop what you are doing and provide a concise summary…"**
   (the ContextOverflow Phase 1 prompt, OR the pre-stop verification prompt).

That summary request is exactly the wrong move: it drains tokens, throws away
the agent's actual question, and forces a multi-cycle summary dance when a
single short answer would unblock the chain.

The user's complaint (verbatim, trimmed):

> clearly auto_prompt not give a shit about the question at all, it should
> call ai api to reason about last2-3 paragraph and answer that instead
> stupid stop everytime which drain more token instead of just answer,
> anyhow if reasoning fail to fetch or not confidence it should [then]
> allow to stop and ask for summary

## Design

Add a **fast path** at the very top of `decide_with_llm`, BEFORE the
`context_exceeds_limit` branch. Reasoning about a 2-3 paragraph question is
cheap and avoids the expensive summary path even when context is near the
limit.

```
decide_with_llm(data, cx)
  ├─ detect_pending_question(last_assistant_message)?
  │    ├─ None  → fall through to existing flow (overflow / lightweight / decide)
  │    └─ Some(question)
  │         ├─ build_answer_prompt(question, last_assistant_message)
  │         ├─ call_language_model(targeted prompt)
  │         │    ├─ Err  → log, fall through (user: "reasoning fail to fetch")
  │         │    └─ Ok(parsed)
  │         │         ├─ confidence < ANSWER_CONFIDENCE_THRESHOLD
  │         │         │   → log, fall through (user: "not confidence")
  │         │         └─ confidence >= threshold + has answer
  │         │             → Continue(answer) wrapped via with_first_prompt_context
  │         └─ (fall-through continues normal decide_with_llm body)
```

The fast path is **purely additive**: every branch either returns a `Continue`
(with a real answer) or falls through. No existing behavior is removed.

### Detection heuristics (`detect_pending_question`)

Scan the **last 3 paragraphs** of `last_assistant_message` (same scope as the
existing `extract_remaining_section` helper) for question-to-user patterns:

- Explicit option-request: "which do you want", "which one", "option a", "option b",
  "option 1", "option 2", "a or b", "1 or 2"
- Permission request: "want me to", "should i", "shall i", "do you want",
  "do you prefer", "would you like", "may i", "can i proceed", "ok to"
- Direct question marker: ends a paragraph with `?` AND contains a
  second-person pronoun ("you", "your")

Skip when the message is itself an auto_prompt summary response (reuse
`is_auto_prompt_summary_response` guard — same protection as
`detect_remaining_work`).

Return the **extracted question text** (the paragraph(s) containing the
question, plus the preceding option-list paragraph if present, so the answerer
has full context). This is the "last 2-3 paragraph" window the user asked for.

### Answerer LLM call

Reuse the existing `call_language_model` plumbing. A dedicated system prompt
asks the model to:

1. Read the agent's question + the preceding 2-3 paragraphs,
2. Pick the **most defensible** option (or answer the yes/no),
3. Return JSON `{ "answer": "<imperative instruction>", "confidence": 0.0-1.0 }`.

Confidence gate (`ANSWER_CONFIDENCE_THRESHOLD = 0.6`): below it, fall through
to the normal flow rather than guessing. The user explicitly authorised this:
"if reasoning … not confidence it should [then] allow to stop and ask for
summary".

The dispatched `next_prompt` is the answer, wrapped via `with_first_prompt_context`
so the chain keeps its summary/title context — same pattern as every other
`Continue` in this file.

## Files

- `crates/auto_prompt/src/pending_question.rs` — NEW. Pure detection +
  targeted-prompt builder + async answerer. Self-contained, unit-testable
  detection without GPUI.
- `crates/auto_prompt/src/auto_prompt.rs` — wire `pending_question` module
  into `decide_with_llm` (one early-return block, ~25 lines).
- `crates/auto_prompt/README.md` — document the new fast path in the
  architecture diagram + add a section.

## Non-goals

- Not changing the existing `evaluate_response` / safety-net / verification
  logic. The fast path runs *before* all of them and falls through on any
  failure.
- Not adding a new `DecisionSource` variant — when the fast path fires, the
  source is conceptually `LlmResponse` (the orchestration LLM answered).
- Not touching `decide()` (sync pre-check). Question detection needs the LLM
  to answer, so it belongs in `decide_with_llm`.

## Tasks

- [x] Create `pending_question.rs` with `detect_pending_question`, `build_answer_prompt`, `answer_pending_question`
- [x] Add unit tests for detection (option request, permission request, yes/no question, summary-response skip, no-question fallthrough)
- [x] Wire fast path into `decide_with_llm` before the `context_exceeds_limit` branch
- [x] Update README architecture diagram + add "Pending question fast path" section
- [x] `./script/clippy` on the crate
- [x] Commit on `develop` with `feat:` prefix
