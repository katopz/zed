# 018 — Session limit: schedule auto_prompt retry at provider reset time

**Commit:** `0db4af571a` (develop)

## Problem

When the Claude agent hits its subscription session limit, the turn fails with:

```
Internal error: You've hit your session limit · resets 1:20am (Asia/Bangkok): {
  "errorKind": "rate_limit"
}
```

Today auto_prompt treats this like any other API error: the hidden orchestrator
(same subscription → hits the same limit) is asked to decide, fails, retries
with short exponential backoff, and the chain dies with an API-error
notification. The reset time — already embedded in the error — is wasted.

## Goal

- Parse the reset wall-clock time + tz label from the error text (or the
  synthetic message form appearing as the last assistant message).
- Schedule the continuation prompt at **reset + margin** (default 60s, e.g.
  `1:20am` → dispatch at `1:21am`) via the existing
  `AutoPromptDecision::DispatchAfterDelay` path (same-thread continuation).
- Show the schedule info ("Session limit reached — auto-continue scheduled at
  1:21am (Asia/Bangkok)") instead of the generic "Agent stopped due to an
  error" notification.

## Design

- `AcpThread.last_api_error: Option<String>` — captures `run_turn`'s `Err`
  text (cleared at turn start next to `had_api_error`). The session-limit
  error is a turn-level completion failure, so it flows through this branch.
- `auto_prompt::session_limit` — pure parser: `session limit` guard →
  `resets H:MM(am|pm)` → optional `(TZ)` label. Time interpreted in `Local`
  (Claude Code renders the reset time in the machine's local timezone; the
  tz name is cosmetic). Next-occurrence semantics: if today's slot already
  passed, use tomorrow. Deterministic core `build_session_limit(text, now,
  margin)` for tests.
- `decide()` (native) + `decide_claude()` (ACP Claude): rule-based early
  return `DispatchAfterDelay { delay_ms }` before any LLM/orchestrator call.
  The Claude path no longer requires a configured model for this case (the
  orchestrator is not consulted at all).
- Same-thread continuation (`force_new_thread: false`); ACP agents never
  spawn new threads in `dispatch_action` anyway.
- Config: `session_limit_margin_secs` (default 60) via
  `auto_prompt.json` / `ZED_AUTO_PROMPT_SESSION_LIMIT_MARGIN_SECS`.
- `conversation_view` Error arm: when the error text parses and auto_prompt
  is enabled → Info notification with the schedule; still Warning (but
  session-limit-specific text) when disabled.

### Known limitations (accepted)

- The scheduled task tracks `active_thread()` for cancellation, identical to
  the existing Refusal backoff path — switching threads mid-wait can misroute
  the dispatch. Pre-existing behavior, unchanged here.
- A manual "continue" click during an active session limit also schedules
  instead of dispatching immediately — hammering a rate-limited window
  succeeds no faster.

## Tasks

- [x] `.plans/018_session_limit_scheduled_retry.md` (this file)
- [x] `acp_thread`: capture `last_api_error` in `run_turn` Err branch + accessor
- [x] `auto_prompt::session_limit` parser + deterministic tests
- [x] Config knob `session_limit_margin_secs` (default 60s)
- [x] `decide()` session-limit rule → `DispatchAfterDelay`
- [x] `decide_claude()` session-limit rule → `DispatchAfterDelay` (model-independent)
- [x] `conversation_view` Error-arm schedule notification
- [x] `cargo clippy` clean (auto_prompt, acp_thread, agent_ui)
- [x] `cargo test -p auto_prompt --lib` pass

## Verification

- Parser unit tests: full Claude error payload, bare synthetic message,
  am/pm + midnight edges, past-time → tomorrow, non-matching texts, margin.

```
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo clippy -p auto_prompt --lib --tests   # clean
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo clippy -p acp_thread --lib           # clean
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo clippy -p agent_ui --lib --tests     # clean
   (also dropped a pre-existing unused `LanguageModelRegistry` import in
    conversation_view.rs left by ba1cc07d45)
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo test -p auto_prompt --lib
   test result: ok. 338 passed; 0 failed   (9 new session_limit tests)
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo test -p acp_thread --lib
   test result: ok. 122 passed; 0 failed
$ CARGO_TARGET_DIR=/tmp/zed-018 cargo test -p agent_ui --lib auto_prompt|watchdog|retained
   4 + 3 + 7 passed; 0 failed
```

## GOAT

Not a perf change (control-flow only, zero hot loops) — no feature gate.
