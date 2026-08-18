# 019 — Weekly limit: schedule auto_prompt retry from the usage API

## Problem

Plan 018 schedules retries from the *parsed error text* ("resets 1:20am"). Two
gaps:

1. The **weekly** limit error text is unknown (not yet captured) — text
   parsing can't cover it until someone pastes the real message.
2. Text parsing guesses the next occurrence of a wall-clock time in local tz.
   When **both** the 5-hour and weekly windows are exhausted (parallel agents
   share one subscription), the text's "1:20am" points at the 5h reset — we
   wake up, hit the weekly limit, and burn a cycle per 5h window.

## Insight

The usage endpoint backing the rings (commit `97f50e6ddc`,
`claude_usage.rs`) already reports per-window `utilization` (whole percent)
and an **exact absolute `resets_at` UTC timestamp** for `five_hour`,
`seven_day`, and `seven_day_opus`. No text format needed.

## Design

- `auto_prompt::session_limit` grows a pushed hint: agent_ui's existing poll
  loop calls `record_usage_hint(UsageHint)` after each successful poll
  (single fetch point, DRY — no new HTTP/keychain code).
- Resolution when a turn error arrives (`session_limit_from_error_text`):
  1. Gate: text must indicate a limit error (`rate_limit` errorKind,
     `session limit`, `weekly limit`, `weekly chat limit`).
  2. Window selection by phrasing — never overshoot on inference:
     - weekly phrasing → max `resets_at` among exhausted weekly windows
       (`seven_day` / `seven_day_opus`, `used >= 0.99`);
     - session phrasing → `five_hour` if exhausted (even if weekly is too —
       if stacked, the retry fails with a weekly error and we reschedule
       once, self-correcting);
     - bare `rate_limit` kind → `five_hour` if exhausted, else weekly.
  3. Fall back to plan 018's text parser (synthetic-message form, API miss).
- Schedule = chosen `resets_at` + `session_limit_margin_secs`. Display
  includes the weekday when >24h out ("Thu 3:05am").
- Notification wording splits: "Weekly limit reached — …" vs "Session limit
  reached — …".

When the real weekly error text is captured later, only the phrase list may
need tightening.

## Tasks

- [x] `.plans/019_weekly_limit_api_scheduled_retry.md` (this file)
- [x] `session_limit`: `UsageHint` static + `record_usage_hint` + resolution matrix
- [x] `claude_usage.rs`: record hint in poll loop
- [x] `conversation_view`: use `session_limit_from_error_text` + weekly wording
- [x] `cargo clippy` clean (auto_prompt, agent_ui)
- [x] `cargo test -p auto_prompt --lib` pass

## Verification

```
$ CARGO_TARGET_DIR=/tmp/zed-019 cargo clippy -p auto_prompt --lib --tests  # clean
$ CARGO_TARGET_DIR=/tmp/zed-019 cargo clippy -p agent_ui --lib --tests    # clean
$ CARGO_TARGET_DIR=/tmp/zed-019 cargo test -p auto_prompt --lib
   test result: ok. 339 passed; 0 failed  (10 session_limit tests incl.
   usage_hint_resolution_matrix: 10 scenarios)
$ CARGO_TARGET_DIR=/tmp/zed-019 cargo test -p agent_ui --lib claude_usage  # 4 passed
$ CARGO_TARGET_DIR=/tmp/zed-019 cargo test -p agent_ui --lib auto_prompt   # 4 passed
```

## Follow-up when the real weekly error text arrives

Paste it and tighten `is_rate_limit_error`/`error_mentions_weekly` phrases
(and extend the text parser only if the API hint somehow misses).

## GOAT

Control-flow only; the hint write is one mutex store per 60s poll. No gate.
