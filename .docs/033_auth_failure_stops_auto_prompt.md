# 033 — Auth failure stops auto-prompt instead of `Processing…`
Status: done (5b8fd4b275)

## Symptom
A turn ending with `Failed to authenticate: OAuth session expired and could not
be refreshed` still kicked off the automatic chain: the button showed
`Processing…` and the decide pipeline ran, although every downstream path
(continuation, orchestrator, overflow summarize) needs the same expired login.

## Fix
- `crates/auto_prompt/src/auth_failure.rs` — `is_auth_error` (turn-level error
  text) and `is_auth_failure_message` (last non-empty line must start like an
  error: `API Error`, `Failed to authenticate`, `Invalid API key`, `OAuth`,
  `Not logged in`), combined by `auth_failure_from_thread`.
- `api_unreachable::latest_turn_failure` — shared two-source scan (turn error,
  then the synthetic assistant message answering the latest user message).
- `agent_ui::auto_prompt::run_auto_prompt` returns before setting
  `Processing` on an auth failure (automatic runs only; manual clicks pass, the
  user may have just re-logged in) and resets the session's iteration count.
- Error notification reads `Agent stopped: authentication failed — sign in again`.

## Tests
`cargo test -p auto_prompt --lib auth_failure` — synthetic messages, prose
false-positive guards, turn-level errors.
