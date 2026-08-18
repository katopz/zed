# 020 — Weekly/Opus limit error phrasing (plan 019 follow-up)

## Context

Plan 019 shipped API-first retry scheduling for subscription rate limits but
left one follow-up open: verify the real weekly-limit error phrasing once
captured. Instead of waiting for a live weekly-limit hit, the exact formats
were confirmed from public documentation of Claude Code's messages
(ssdnodes.com/learn/claude-usage-limits-explained, 2026-07):

- `You've hit your session limit · resets 3:45pm` (5-hour window)
- `You've hit your weekly limit · resets Mon 12:00am` (7-day window — note
  the **weekday prefix** before the clock)
- `You've hit your Opus limit · resets 3:45pm` (7-day Opus-only window =
  `seven_day_opus`)

## Gaps found vs. plan 019's implementation

1. **Opus limit unrecognized** — neither `is_rate_limit_error` nor
   `error_mentions_weekly` matched "opus limit"; the synthetic-message form
   (no `errorKind` envelope) would not schedule at all.
2. **Weekly text fallback unparseable** — `build_session_limit` guarded on
   `"session limit"` only, and resolved dates as today/tomorrow; the weekly
   form (`resets Mon 12:00am`, up to 7 days out) fell back to None → API
   error → death spiral when no usage hint was recorded.
3. **Splash filter missed weekly/opus** — `looks_like_session_limit` only
   matched session phrasing, so weekly/opus synthetic messages were treated
   as real assistant replies (immediate re-dispatch against exhausted
   quota).

## Tasks

- [x] Shared `LIMIT_PHRASES` const; `is_rate_limit_error` + splash guard
      consume it (DRY)
- [x] Rename `looks_like_session_limit` → `looks_like_usage_limit` covering
      session/weekly/weekly-chat/opus phrasing; update call sites
      (`auto_prompt.rs`, `claude_agent.rs`)
- [x] `error_mentions_weekly` matches `opus limit` (routes to
      `seven_day`/`seven_day_opus` windows)
- [x] `build_session_limit`: guard accepts any limit phrase; parse optional
      weekday token (`Mon`/`Monday`/…); resolve next occurrence of that
      weekday (`resolve_reset_at`)
- [x] Unified `format_display` (weekday prefix when the retry lands on
      another day + optional tz label) for both API and text paths
- [x] `session_limit_from_thread`: assistant-message branch goes API-first
      too, but only when the message has the splash shape (limit phrase +
      `resets`) so prose quoting a limit phrase never schedules
- [x] Tests: weekly weekday form, roll-to-next-week, same-day-stay, opus
      form, guard/phrasing tests, matrix scenarios 11 (opus phrasing →
      seven_day_opus reset) and 12 (no hint → weekday text fallback)
- [x] clippy clean (auto_prompt, agent_ui); auto_prompt test suite passes

## Notes

- Notification wording stays two-branch ("Weekly limit reached" covers the
  Opus window — it is the same 7-day family; the scheduled time is exact
  from the usage API either way).
- Ops note: `target/debug` (240G) on the SD card was deleted to unblock the
  disk (was 100% full); tests ran with `CARGO_TARGET_DIR=/tmp/...`.

## Outcome

Commit: see `git log` (feat(020)).
