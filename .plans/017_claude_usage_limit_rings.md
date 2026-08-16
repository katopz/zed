# Claude subscription usage rings (5-hour + weekly) beside the context ring

## Goal
Two extra circular-progress rings in the agent panel toolbar, immediately left
of the existing context-usage ring, showing how much of the Claude
subscription's rolling **5-hour** and **weekly** rate-limit windows has been
consumed. Hovering them opens a tooltip — the same `hoverable_tooltip` pattern
the context ring already uses — listing each window's percentage and when it
resets, including the Opus-specific weekly window as a text row.

## Why the data has to be polled
Zed drives Claude through the external `claude-acp` agent server, so the
`anthropic-ratelimit-unified-5h-*` / `-7d-*` response headers that carry
subscription usage are consumed by that child process and never reach Zed.
Three non-starters were ruled out first:

- **Zed's own Anthropic provider headers** — `crates/language_models/src/provider/anthropic.rs`
  is API-key-only (no subscription OAuth), and API-key accounts get per-minute
  request/token limits, not the 5h/weekly subscription windows. Wrong numbers
  even if they were reachable.
- **ACP** — `agent-client-protocol` 1.3.0 has a `UsageUpdate` for context
  tokens only. No rate-limit channel, and the agent server is not ours to change.
- **`claude` CLI** — `/usage` is interactive-only; there is no non-interactive
  subcommand to shell out to.

That leaves the endpoint backing `/usage`:
`GET https://api.anthropic.com/api/oauth/usage`, authenticated with the OAuth
token Claude Code already stores locally.

## Design

`crates/agent_ui/src/claude_usage.rs` — new module:

- `UsageWindow { used: f32 /* 0.0..=1.0 */, resets_at: Option<DateTime<Utc>> }`
- `ClaudeUsage { five_hour, seven_day, seven_day_opus }`
- `ClaudeUsageStore` — gpui global entity holding the latest `ClaudeUsage` plus
  the polling `Task`.

**Lazy start.** `ClaudeUsageStore::global()` creates the store (and starts
polling) on first call; `ThreadView::new` only calls it when
`agent_id == agent_servers::CLAUDE_AGENT_ID`. Installs that never open a Claude
thread never touch the keychain. `render_claude_usage` uses `try_global` so
rendering never starts a poll.

**Token caching.** The OAuth token is read once and cached in the poll task, not
re-read every tick — otherwise a `/usr/bin/security` subprocess would spawn
1440×/day. Claude Code rotates the token periodically, so a `401`/`403` on a
cached token clears the cache and re-reads once before giving up. Any other
non-success status also drops the cache.

**Credential lookup.** Claude Code stores a *generic* password
(`Claude Code-credentials`), which gpui's `read_credentials` cannot reach — that
API queries *internet* passwords keyed by server. macOS therefore shells out to
`/usr/bin/security find-generic-password`; every platform falls back to
`~/.claude/.credentials.json`. A denied keychain prompt and a missing item both
fall through to the file rather than failing outright.

**Failing loud enough to debug, quiet enough to ignore.** The endpoint only
answers for subscription logins, so API-key users would otherwise get a warning
every 5 minutes. First failure logs at `warn`, repeats at `debug`, and a success
resets the latch. A response that parses but yields no known windows is treated
as an error rather than silently hiding the rings — that is the failure mode if
the undocumented response shape ever drifts.

**Lenient parsing.** Unknown windows are ignored (serde default), `resets_at`
accepts RFC-3339 or unix seconds, and `utilization` is read as whole percents
(`45` → 0.45) with values below 1 treated as an already-normalized fraction, so
a shape change degrades to the right number instead of a silent 0%.

**Colors** match the context ring exactly: `text_muted`, switching to
`status().warning` at ≥85%. Rings are labelled `5h` / `7d` rather than icons —
there is no calendar glyph in `IconName`, and two near-identical clock icons
would read worse than two-character labels.

## Tasks
- [x] Rule out header/ACP/CLI data sources; confirm `claude-acp` is the active agent
- [x] `claude_usage.rs`: types, global store, poll loop, token cache + 401 retry
- [x] Keychain (macOS) + `~/.claude/.credentials.json` credential lookup
- [x] Lenient payload parsing with unit tests
- [x] `render_claude_usage` + `ClaudeUsageTooltip` in `thread_view.rs`
- [x] Wire rings into the toolbar row left of `render_token_usage`
- [x] `cargo clippy -p agent_ui --all-targets` clean; unit tests pass
- [ ] Verify the live response shape (`five_hour` / `seven_day` / `seven_day_opus`,
      `utilization`, `resets_at`) against a real account — blocked on reading the
      OAuth token, which needs the user's go-ahead
- [ ] Confirm whether `anthropic-beta: oauth-2025-04-20` is required or harmful
      on this endpoint (sent today; drop it if it provokes a 400)

## Known trade-offs
- Polling runs for the process lifetime once started, at 60s (300s after a
  failure). It does not stop when the last Claude thread closes — the store is
  global and has no refcount. One HTTPS GET per minute; tune via `POLL_INTERVAL`.
- The endpoint is undocumented. Everything above fails closed: rings hide, one
  warning lands in the log.
- macOS will prompt once for keychain access ("Zed wants to access key
  'Claude Code-credentials'"). Choosing *Always Allow* makes it silent; denying
  it falls through to the credentials file and then hides the rings.
