# Plan 03: Auto Prompt State Machine Refactor

## Status

- [x] Task 1: Extract `evaluate_response()` pure function
- [x] Task 2: Remove LLM bypass paths in `decide()`
- [x] Task 3: Refactor `decide_with_llm()` to use `evaluate_response()` + verification gate
- [x] Task 4: Write state machine tests for `evaluate_response()`
- [x] Task 5: Write verification gate tests
- [x] Task 6: Write `decide()` gate tests
- [x] Task 7: Run tests, fix diagnostics, commit

---

## Problem

Auto_prompt stops prematurely because multiple code paths bypass the LLM or bypass pre-stop verification:

1. **`decide()` bypasses** — `exceeds_token_limit`, `StopReason::MaxTokens`, `had_error/Refusal` return `DispatchNow`/`DispatchAfterDelay` without calling the LLM
2. **`decide_with_llm()` immediate stops** — `all_done`, `confidence < 0.5`, `no next_prompt`, `empty next_prompt` return `Stopped` without verification
3. **No testable state machine** — logic is scattered across 300 lines of async code with atomic globals

## Design Rule

**Only the pre-stop verification gate can stop auto_prompt.** Every stop must:

1. Call the LLM (last assistant message always sent)
2. LLM says stop → enter pre-stop verification (never stop immediately)
3. Verification dispatches another iteration
4. Only after `verification_count >= 1` and LLM still says stop → actually stop

---

## State Machine

```
┌─────────────────────────────────────────────────────┐
│  decide() — Synchronous Gate                         │
│                                                      │
│  NoAction exits (cannot call LLM):                   │
│  • Cancelled by user                                 │
│  • No model configured                               │
│  • Max iterations reached                            │
│  • Interactive auth pending                          │
│  • Config load failure                               │
│  • Context serialization failure                     │
│                                                      │
│  ALL other cases → NeedsLlmCall(data)                │
│  (token limit, MaxTokens, error, Refusal — all       │
│   now go to LLM instead of bypass)                   │
└──────────────────┬──────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────┐
│  decide_with_llm() — Call LLM                        │
│                                                      │
│  LLM Error → retry or propagate                      │
│  LLM Response → evaluate_response()                  │
└──────────────────┬──────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────┐
│  evaluate_response() — Pure Function                  │
│                                                      │
│  Input: AutoPromptResponse + EvaluationInput          │
│  Output: EvaluationResult enum                       │
│                                                      │
│  Continue cases (order matters):                     │
│  1. all_done + next_plan → Continue(next_plan)       │
│  2. all_done + should_continue → Continue(gitflow)   │
│  3. detect_remaining_work → Continue(remaining)      │
│  4. should_continue + valid_prompt → Continue(prompt)│
│                                                      │
│  All other cases → WantsStop(reason)                 │
│  (confidence < 0.5, !should_continue, no prompt,     │
│   empty prompt, etc.)                                │
└──────────────────┬──────────────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────────────┐
│  Verification Gate — THE ONLY WAY TO STOP            │
│                                                      │
│  match evaluate_response() {                         │
│    Continue { prompt } → dispatch new thread         │
│    WantsStop { reason } → {                          │
│      verification_count == 0:                        │
│        → increment count                             │
│        → build verification prompt                   │
│        → dispatch verification thread (Continue)     │
│      verification_count >= 1:                        │
│        → Stopped { reason } ← THE ONLY STOP EXIT     │
│    }                                                 │
│  }                                                   │
└─────────────────────────────────────────────────────┘
```

---

## Files to Modify

| File | Change |
|------|--------|
| `crates/auto_prompt/src/auto_prompt.rs` | Extract `evaluate_response()`, remove bypasses, refactor `decide_with_llm()` |
| `crates/auto_prompt/src/auto_prompt.rs` (tests) | Add state machine tests |

No new files needed.

---

## Task Details

### Task 1: Extract `evaluate_response()` Pure Function

Add new types and function at module level (before `decide()`):

```rust
/// Input for the pure evaluation function.
pub struct EvaluationInput {
    pub should_continue: bool,
    pub confidence: Option<f64>,
    pub next_prompt: Option<String>,
    pub reason: Option<String>,
    pub all_plan_done: bool,
    pub next_plan_prompt: Option<String>,
    pub last_assistant_message: Option<String>,
    pub context_json: String,
    pub work_dirs: Option<Vec<PathBuf>>,
}

/// Result of evaluating an LLM response.
#[derive(Debug, PartialEq)]
pub enum EvaluationResult {
    /// Continue the chain with this prompt.
    Continue {
        prompt: String,
        reason: String,
    },
    /// LLM wants to stop — must go through verification gate.
    WantsStop {
        reason: String,
    },
}

/// Pure function — no side effects, no atomics, fully testable.
pub fn evaluate_response(input: &EvaluationInput) -> EvaluationResult {
    // 1. all_done transitions
    // 2. detect_remaining_work override
    // 3. should_continue + valid prompt → Continue
    // 4. Everything else → WantsStop
}
```

Logic order in `evaluate_response()`:

1. **all_done + next plan** → `Continue(next_plan_prompt)` — transition to next plan
2. **all_done + should_continue** → `Continue(gitflow)` — dispatch final commit
3. **detect_remaining_work** → `Continue(remaining_work)` — override stop
4. **should_continue + non-empty prompt** → `Continue(prompt)` — normal continuation
5. **Everything else** → `WantsStop(reason)` — includes:
   - all_done + !should_continue (no next plan)
   - confidence < 0.5
   - !should_continue + no prompt
   - should_continue + empty/missing prompt

### Task 2: Remove LLM Bypass Paths in `decide()`

Remove these three blocks that return `DispatchNow`/`DispatchAfterDelay` without calling LLM:

| Lines | Condition | Current Return | New Behavior |
|-------|-----------|---------------|--------------|
| ~534-543 | `exceeds_token_limit` | `DispatchNow(make_continue_prompt())` | Remove, fall through to `NeedsLlmCall` |
| ~549-558 | `StopReason::MaxTokens` | `DispatchNow(make_continue_prompt())` | Remove, fall through to `NeedsLlmCall` |
| ~562-577 | `had_error \|\| Refusal` | `DispatchAfterDelay(make_continue_prompt())` | Remove, fall through to `NeedsLlmCall` |

Keep these `NoAction` exits (cannot physically call LLM):
- Config load failure
- `StopReason::Cancelled`
- Interactive auth pending
- Max iterations reached
- No model configured
- Context serialization failure

### Task 3: Refactor `decide_with_llm()` to Use State Machine

Replace the 300-line match block with:

```rust
match result {
    Ok((raw_response, response)) => {
        write_decision_log(...);

        let input = EvaluationInput {
            should_continue: response.should_continue,
            confidence: response.confidence,
            next_prompt: response.next_prompt.clone(),
            reason: response.reason.clone(),
            all_plan_done: /* detect #ALL_PLAN_DONE */,
            next_plan_prompt: find_next_plan_prompt(...),
            last_assistant_message: data.last_assistant_message.clone(),
            context_json: data.context_json.clone(),
            work_dirs: data.work_dirs.clone(),
        };

        match evaluate_response(&input) {
            EvaluationResult::Continue { prompt, reason } => {
                log::info!("[auto_prompt] Continue: {reason}");
                let next_prompt = with_first_prompt_context(
                    prompt,
                    prompt_summary.as_deref(),
                    data.title.as_deref(),
                    data.last_assistant_message.as_deref(),
                );
                Ok(AutoPromptOutcome::Continue(AutoPromptAction { ... }))
            }
            EvaluationResult::WantsStop { reason } => {
                // === VERIFICATION GATE — THE ONLY WAY TO STOP ===
                let verification_count = VERIFICATION_COUNT.load(Ordering::Relaxed);
                if verification_count == 0 {
                    VERIFICATION_COUNT.fetch_add(1, Ordering::Relaxed);
                    let verification_prompt = build_pre_stop_verification_prompt(...)
                        .unwrap_or_else(|| generic_verification_prompt());
                    log::info!("[auto_prompt] Pre-stop verification dispatched");
                    let next_prompt = with_first_prompt_context(
                        verification_prompt,
                        prompt_summary.as_deref(),
                        data.title.as_deref(),
                        data.last_assistant_message.as_deref(),
                    );
                    Ok(AutoPromptOutcome::Continue(AutoPromptAction { ... }))
                } else {
                    log::info!("[auto_prompt] Verification passed, stopping: {reason}");
                    write_stop_log(...);
                    reset_iteration();
                    Ok(AutoPromptOutcome::Stopped { reason })
                }
            }
        }
    }
    Err(err) => { ... }
}
```

Key changes:
- `build_pre_stop_verification_prompt()` returns `None` → use **generic verification prompt** (never immediate stop)
- No more `verification_count < max` vs `>= max` — once verification_count >= 1, stop is accepted
- Remove `max_verification_attempts` field (no longer needed — one verification is sufficient)

### Task 4: State Machine Tests for `evaluate_response()`

Test file: `crates/auto_prompt/src/auto_prompt.rs` in `mod tests`

Helper:
```rust
fn make_input() -> EvaluationInput { /* sensible defaults */ }
fn make_response() -> AutoPromptResponse { /* sensible defaults */ }
```

Test cases:

| # | Scenario | Input | Expected |
|---|----------|-------|----------|
| 1 | all_done + next plan | `all_plan_done=true, next_plan_prompt=Some("do next")` | `Continue` with next plan prompt |
| 2 | all_done + should_continue + no next plan | `all_plan_done=true, should_continue=true` | `Continue` with gitflow prompt |
| 3 | all_done + !should_continue + no next plan | `all_plan_done=true, should_continue=false` | `WantsStop` |
| 4 | detect_remaining_work "remaining work" | `should_continue=false, last_assistant_message="## Remaining Work\n- fix tests"` | `Continue` override |
| 5 | detect_remaining_work "- [ ]" | `should_continue=false, last_assistant_message="- [ ] do thing"` | `Continue` override |
| 6 | detect_remaining_work "TODO:" | `should_continue=false, last_assistant_message="TODO: fix this"` | `Continue` override |
| 7 | detect_remaining_work no match | `should_continue=false, last_assistant_message="all done"` | `WantsStop` |
| 8 | should_continue + valid prompt | `should_continue=true, next_prompt=Some("commit changes")` | `Continue` with prompt |
| 9 | should_continue + empty prompt | `should_continue=true, next_prompt=Some("")` | `WantsStop` |
| 10 | should_continue + whitespace prompt | `should_continue=true, next_prompt=Some("   ")` | `WantsStop` |
| 11 | should_continue + no prompt | `should_continue=true, next_prompt=None` | `WantsStop` |
| 12 | !should_continue + has prompt | `should_continue=false, next_prompt=Some("review")` | `WantsStop` (LLM says stop, prompt ignored) |
| 13 | confidence 0.3 + should_continue | `should_continue=true, confidence=0.3, next_prompt=Some("go")` | `Continue` (confidence < 0.5 only matters when stopping) |
| 14 | confidence 0.3 + !should_continue | `should_continue=false, confidence=0.3` | `WantsStop` with low confidence reason |
| 15 | confidence 0.8 + !should_continue | `should_continue=false, confidence=0.8` | `WantsStop` |
| 16 | all_done + next plan + should_continue=false | `all_plan_done=true, next_plan_prompt=Some("next"), should_continue=false` | `Continue` (next plan takes priority) |
| 17 | all_done + remaining work + no next plan | `all_plan_done=true, should_continue=false, last_assistant_message="remaining: fix test"` | `Continue` (remaining work overrides all_done stop) |
| 18 | #ALL_PLAN_DONE in prompt | `next_prompt=Some("done #ALL_PLAN_DONE"), should_continue=true` | `Continue` with prompt stripped of `#ALL_PLAN_DONE` |
| 19 | last_assistant_message None | `should_continue=false, last_assistant_message=None` | `WantsStop` (no remaining work to detect) |
| 20 | last_assistant_message empty | `should_continue=false, last_assistant_message=Some("")` | `WantsStop` |

### Task 5: Verification Gate Tests

Test that `decide_with_llm()` verification gate works correctly. These test the integration between `evaluate_response()` and the verification counter.

Since `decide_with_llm()` is async and calls an external LLM, mock the evaluation by:
- Setting `VERIFICATION_COUNT` before test, resetting after
- Testing the gate logic separately as a pure function `handle_wants_stop()`

```rust
fn handle_wants_stop(
    reason: String,
    verification_count: u32,
    context_json: &str,
    work_dirs: Option<&[PathBuf]>,
) -> VerificationGateResult {
    if verification_count == 0 {
        VerificationGateResult::DispatchVerification { prompt: ... }
    } else {
        VerificationGateResult::Stop { reason }
    }
}
```

Test cases:

| # | Scenario | verification_count | Expected |
|---|----------|-------------------|----------|
| 1 | First stop request | 0 | `DispatchVerification` |
| 2 | First stop + plan files | 0 | `DispatchVerification` with plan-based prompt |
| 3 | First stop + no plan files | 0 | `DispatchVerification` with generic prompt |
| 4 | After verification | 1 | `Stop` |
| 5 | After multiple verifications | 3 | `Stop` |

### Task 6: `decide()` Gate Tests

Test that `decide()` correctly routes to `NoAction` vs `NeedsLlmCall`. These require mocking `App` context — mark as integration tests if unit testing is impractical.

Document the expected behavior in test comments:

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `StopReason::Cancelled` | `NoAction` |
| 2 | No model configured | `NoAction` |
| 3 | Max iterations | `NoAction` |
| 4 | Interactive auth pending | `NoAction` |
| 5 | Config load failure | `NoAction` |
| 6 | `exceeds_token_limit` | `NeedsLlmCall` (was `DispatchNow`) |
| 7 | `StopReason::MaxTokens` | `NeedsLlmCall` (was `DispatchNow`) |
| 8 | `had_error=true` | `NeedsLlmCall` (was `DispatchAfterDelay`) |
| 9 | `StopReason::Refusal` | `NeedsLlmCall` (was `DispatchAfterDelay`) |
| 10 | Normal `EndTurn` | `NeedsLlmCall` |

### Task 7: Run Tests, Fix Diagnostics, Commit

```bash
cargo test -p auto_prompt --quiet
./script/clippy
```

Commit message: `refactor(auto_prompt): state machine for stop flow — only verification gate can stop`

---

## Dependencies

None — pure refactor within `crates/auto_prompt`.

## Estimated Effort

- Task 1-3: Refactoring (~1 hour)
- Task 4-6: Tests (~1 hour)
- Task 7: Verify and commit (~15 min)