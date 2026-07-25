# Issue 007: auto_prompt creates failed new threads when all APIs are rate-limited during context overflow

## Status
- [x] Symptom identified (death spiral: failed thread → new thread → fails again)
- [x] Root cause identified — TWO compounding bugs:
  1. `decide_with_llm` Phase 2 fires unconditionally on `context_exceeds_limit`, ignoring `had_error`
  2. **Schema migration regression (commit 9b063ddf)**: v1 `PersistedKeyHealthFile` (pre-Quaternary) rejected on parse with "missing field `quaternary`" → entire backoff state wiped → system thinks ALL slots are healthy → never rotates to the working slot 3 → every turn hits rate-limited slot 1 → `had_error=true`
- [x] Fix #1 implemented (new `RetryAfterBackoff` outcome + guard in `decide_with_llm`) — commit `e2c73b2972`
- [x] Fix #2 implemented (`#[serde(default)]` on `quaternary` + forward-compat migration in `reload_persisted_health`)
- [x] Fix #1 committed (`e2c73b2972` on `develop`)
- [ ] Fix #2 committed
- [ ] GOAT verified (live behavior under real rate-limit conditions)

## Symptom

When the source thread stops with `had_error=true` (e.g. all configured API keys
hit rate limits, network error, server 5xx) AND its context has overflowed,
auto_prompt still creates a fresh continuation thread in Phase 2. The new thread
immediately hits the same rate-limited API, fails on its first turn, and stops
with `had_error=true`. auto_prompt fires again, the new thread's context is not
yet over the limit (so it does not take the Phase 2 path), but it can still
overflow shortly after, restarting the cycle.

The observable result: a cascade of short-lived failed threads, each burning a
"first turn" against an already-exhausted API quota, plus repeated summary
generation that wastes tokens.

## Root cause

**TWO compounding bugs.** Fix #1 alone is insufficient — fix #2 is the actual
root cause of the user's reported symptom (slot 3 never tried).

### Bug #1: `decide_with_llm` ignores `had_error` on context overflow

In `crates/auto_prompt/src/auto_prompt.rs::decide_with_llm`, the
`context_exceeds_limit` branch unconditionally enters the Phase 1 / Phase 2
state machine. The only guard is `summary_state` (per-session). There is no
guard on `data.had_error`, so Phase 2 happily creates a new thread that is
doomed to fail when the upstream API is the actual cause of the stop.

The existing LLM-orchestration retry loop in
`crates/agent_ui/src/auto_prompt/mod.rs::on_thread_stopped` (lines ~803-836)
only retries the *orchestration* LLM call (auto_prompt's own model call). It
does not cover the case where `decide_with_llm` *succeeds* in producing a
`Continue` action whose dispatch (the user-facing model call) then fails.

### Bug #2: Schema migration regression wipes backoff state (THE REAL CAUSE)

Commit `9b063ddf` (Quaternary key slot) added a new required field
`quaternary` to `PersistedKeyHealthFile` and bumped `schema_version` 1→2.
But the deserialization was **not forward-compatible**: serde rejected v1
files with `"missing field `quaternary`"` BEFORE the `schema_version` check
could fire. The schema_version check was effectively dead code for the actual
migration case.

The failure path returns `KeyHealthTracker::default()` — all slots healthy,
zero failures, no backoff. This means:
1. On boot, the system forgets that slots 1+2 are rate-limited.
2. `select_from_candidates` picks slot 1 (or any) — it looks healthy.
3. Slot 1 immediately returns 429 rate-limit error.
4. `had_error=true` is set on the thread.
5. auto_prompt fires, context has overflowed, my issue 007 guard triggers
   `RetryAfterBackoff` — but the real problem is slot 3 was never tried.

**Evidence** from `/Users/katopz/Library/Logs/Zed/Zed.log` (2026-07-25):
```
15:17:18 WARN failed to parse persisted key health at .../nanbeige4.1-3b.json: missing field `quaternary` at line 1 column 268
15:17:18 WARN failed to parse persisted key health at .../GLM.json: missing field `quaternary` at line 1 column 290
... (later, my new code fires because had_error=true)
15:28:40 WARN RetryAfterBackoff (attempt 1/3) — deferring decision: context overflow with source thread error (likely rate limit)
```

The user's on-disk `nanbeige4.1-3b.json` still has the v1 shape with primary
showing 87 consecutive failures and 14454s backoff remaining — all of which
was being silently discarded on every boot.

**Fix:** Add `#[serde(default)]` to the `quaternary` field (with
`#[derive(Default)]` on `PersistedKeyHealth` — healthy default), and rewrite
the schema_version check in `reload_persisted_health` to be forward-compatible:
accept anything `<= CURRENT`, migrate up, reject only `> CURRENT`.

## Fix

Two-part change:

### (a) New `RetryAfterBackoff` outcome variant

`AutoPromptOutcome::RetryAfterBackoff { delay_ms, reason }` — tells the caller
to wait `delay_ms` and then re-run `decide_with_llm`. Used when we cannot make
a safe forward decision right now (context full + source thread had an error).

### (b) Guard in `decide_with_llm`

At the top of the `context_exceeds_limit` branch, before the Phase 1/Phase 2
state machine: if `data.had_error`, return `RetryAfterBackoff` with a delay
computed from the current failure count via `config.backoff_delay_ms(...)`.

Rationale for using `had_error` (not a more specific signal):
- `MaxTokens` and `Refusal` are already handled at `decide()` (lines 945, 994)
  and bypass `decide_with_llm` entirely, so they cannot reach this guard.
- The remaining `had_error=true` cases that reach `decide_with_llm` are stream
  errors (rate limit, network, 5xx) and failed tool calls. For tool-call
  failures the guard is slightly over-aggressive (it'll defer Phase 2 even
  though a new thread would succeed), but the cost is one backoff delay — the
  retry will then take the normal Phase 2 path. Safe over-aggression beats the
  death spiral.

### (c) Handler in `on_thread_stopped`

Restructure the existing retry loop so it handles both `Err(...)` (existing)
and `Ok(RetryAfterBackoff { delay_ms, .. })` (new) uniformly: increment
`llm_failure_count`, sleep, re-run `decide_with_llm`. When
`llm_failure_count > max_llm_retries`, convert to `Stopped` with the reason.

The manual retry path in `thread_view.rs::render_auto_prompt_toggle` also
pattern-matches `AutoPromptOutcome`; for that path we treat `RetryAfterBackoff`
as a soft stop (user can click retry again).

## Files modified

| File | Change |
|------|--------|
| `crates/auto_prompt/src/auto_prompt.rs` | Add `RetryAfterBackoff` variant; guard in `decide_with_llm`; tests (fix #1) |
| `crates/agent_ui/src/auto_prompt/mod.rs` | Handle `RetryAfterBackoff` in retry loop (fix #1) |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | Handle `RetryAfterBackoff` in manual retry path (fix #1) |
| `crates/language_models/src/provider/open_ai_compatible/health.rs` | `#[serde(default)]` on `quaternary`; `#[derive(Default)]` on `PersistedKeyHealth`; forward-compat migration in `reload_persisted_health`; v1→v2 migration tests (fix #2) |

## Severity

**MEDIUM-HIGH.** Burns API quota on doomed threads; under sustained rate limits
the user sees a flurry of failed thread toasts and no forward progress. Does
not corrupt state, but wastes tokens and confuses the user.
