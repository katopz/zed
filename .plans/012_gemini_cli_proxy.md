# Plan 012: Gemini web-session ACP proxy (`gemini-web-acp`)

## Problem (revised — billing, not auth)

**Prior framing was wrong.** The original assumption was "no API key available,
so use the web session". The real driver is **billing isolation**:

- The `gemini.google.com` web UI runs against the user's existing flat-fee
  subscription (Google AI Pro/Ultra, or Workspace Gemini add-on).
- `gemini-cli`'s "Sign in with Google" path for **Workspace** accounts
  (`katopz@maxion.game`) routes through a GCP project + Vertex/Gemini for Cloud
  API — **metered billing, separate from and on top of** the web subscription.
- Personal-Gmail OAuth (`katopz@gmail.com`) is free-tier (60 req/min,
  1000 req/day) but doesn't use the paid subscription either.
- Result: any non-browser path means paying twice — once for the web
  subscription the user already has, once for API/Vertex usage.

**The web session is the only path that uses the subscription the user already
pays for.** That's why the proxy exists. Not a workaround for missing auth,
a workaround for double-billing.

## Architecture (CLI, no Zed UI)

Per the user's "stick with cli no ui as possible" — this is a **standalone
binary that speaks ACP over stdio**, registered in Zed as a custom agent.
Zero changes to Zed's codebase. The user installs the binary, adds one entry
to `agent_servers` in settings, and a "Gemini (web)" thread shows up in the
agent panel next to Gemini CLI / Claude / Codex.

```
Zed agent panel
    │ ACP (JSON-RPC over stdio)
    ▼
gemini-web-acp binary  ◄── new crate, standalone
    │ CDP (WebSocket on localhost)
    ▼
Chrome with dedicated profile (logged in once)
    │ HTTPS
    ▼
gemini.google.com  ◄── uses existing web subscription
```

Why this shape and not the `gemini_browser` branch's shape:

- **`gemini_browser` failed because it was UI-first**: right-click "Ask Gemini"
  in 5 surfaces (editor, markdown preview, terminal, project panel, agent
  threads), custom Zed actions, custom settings, modifications across 12+
  Zed crates. ~2100 lines, never validated end-to-end against real Gemini.
  Heavy surface area, fragile integration, no actual proof it answers.
- **This plan is binary-first**: one new crate outside Zed's tree (or in
  `crates/` but not wired into the Zed binary), one settings entry, zero
  editor integration. Prove the proxy works as a CLI first; UI is a later
  increment only if the CLI path lands.

## Scope

### In scope

- New Rust binary `gemini-web-acp` (name bikesheddable).
- Minimal ACP server loop over stdio: handle `initialize`, `newSession`,
  `prompt`, `cancel`, `sessionHistory`. Stream `session/update` events back.
- CDP client to drive Chrome — port `crates/gemini_browser/src/cdp.rs`
  from the `gemini_browser` branch (already proven to type-check and lint
  clean, deliberately hand-written over `async-tungstenite`/smol to avoid
  the tokio-in-smol problem `chromiumoxide` would introduce).
- Gemini page automation — port `crates/gemini_browser/src/gemini_page.rs`,
  but **the selectors must be tuned against the real signed-in DOM** before
  this plan is considered done (this is the part `gemini_browser` never did).
- One-time login flow: visible Chrome window on a dedicated profile dir, user
  logs into `gemini.google.com` once with `katopz@maxion.game`, cookies
  persist in the profile, all subsequent runs are headless.
- CLI mode: `gemini-web-acp prompt "explain this code"` for non-ACP use
  (smoke testing, scripting). Same binary, different entry point.

### Out of scope (explicitly)

- No Zed crate modifications. No `crates/editor`, `crates/agent_ui`,
  `crates/zed` changes. No context menus, no actions, no settings additions
  beyond the user's own `agent_servers` entry.
- No obscura. No stealth/anti-fingerprinting. No `chromiumoxide`.
- No thread-history import from Gemini (Gemini web has no thread-list API;
  each Zed thread maps 1:1 to a fresh Gemini conversation).
- No `open_links_in` link interception.
- No GPUI texture-rendered browser pane.

## Hard constraints (unchanged from prior 012)

- Real Chrome + real profile only. No stealth, no spoofing.
- Single session, human-paced, no bulk automation, no multi-account.
- Default-off — user has to install the binary and add the settings entry.
  Never shipped as a default Zed feature.
- If this ever needs to ship to other users, the answer is the real Gemini
  API path, not this proxy.

## Components

### 1. `crates/gemini_web_acp/` — the binary

Single crate, single binary, library root `gemini_web_acp.rs`. Roughly:

- `main.rs` — arg parse, dispatch to `acp_server::run()` or `cli::run(prompt)`.
- `acp_server.rs` — stdio JSON-RPC loop, ACP method handlers, drives
  `GeminiPage` per `prompt` request. **Depend on the published
  `agent-client-protocol = "=1.3.0"` crate** (same version Zed uses —
  verified in root `Cargo.toml`) to get all request/response/event types
  for free. Only the stdio framing + dispatch loop is hand-rolled.
- `cli.rs` — one-shot prompt → print response to stdout. For smoke testing.
- `cdp.rs` — ported from `gemini_browser/src/cdp.rs` (412 lines,
  `Target`/`Runtime`/`Input`/`Page` only, `async-tungstenite`/smol).
- `gemini_page.rs` — ported + **live-tuned** from `gemini_browser/src/gemini_page.rs`.
  Candidate selectors + `diagnose()` action reused; the actual winning
  selectors get pinned only after live DOM inspection.
- `chrome.rs` — launch/reuse Chrome with `--user-data-dir=<profile>` +
  `--remote-debugging-port=<port>`, read `DevToolsActivePort`.

Estimated size: ~1000 lines total (vs `gemini_browser`'s ~2100 with UI).

### 2. Settings entry (user's `settings.json`, not Zed defaults)

```jsonc
"agent_servers": {
  // ...existing claude-acp entry...
  "gemini-web": {
    "type": "custom",
    "command": "/path/to/gemini-web-acp",
    "args": ["--acp"],
    "env": {
      "GEMINI_WEB_PROFILE": "~/.local/share/gemini-web-acp/profile",
      "GEMINI_WEB_HEADLESS": "1"
    }
  }
}
```

Nothing in `assets/settings/default.json` or `crates/settings_content`. The
user opts in by adding this entry themselves.

### 3. One-time login

First run with `GEMINI_WEB_HEADLESS=0` (or a `--login` flag):

```bash
gemini-web-acp --login
```

Opens Chrome visibly on the dedicated profile, navigates to
`https://gemini.google.com`, user logs in once with `katopz@maxion.game`,
closes the window. Profile persists cookies. All later `--acp` runs are
headless and reuse the session.

## Decision: branch from `gemini_browser` or fresh?

Three options, ranked:

1. **Cherry-pick just `cdp.rs` + `gemini_page.rs` from `gemini_browser` (tip
   `fefaa0ac`) onto a fresh `gemini_cli_proxy` branch off `de146c3`.** Keeps
   the proven CDP code, drops all the UI integration (~1700 lines of it).
   Recommended.
2. **New branch, rewrite CDP client from scratch.** No — the existing one
   already type-checks and lint-passes; rewriting is pure waste.
3. **Continue on `gemini_browser` branch.** No — too much UI baggage in the
   commit history and the branch name lies about what it does.

Going with option 1 unless user objects.

## Tasks

### Branch setup

- [ ] `git checkout de146c3528c8ad00b023609d08cbc2a032620e41`
- [ ] `git checkout -b gemini_cli_proxy`
- [ ] Extract `crates/gemini_browser/src/cdp.rs` and
      `crates/gemini_browser/src/gemini_page.rs` from `fefaa0ac` into a new
      `crates/gemini_web_acp/src/` (drop the `gemini_browser` crate shell,
      Zed integration, settings, actions — keep only the two page-driving
      files).
- [ ] Decide crate location: in-tree under `crates/` (but NOT in the Zed
      workspace `Cargo.toml` so it doesn't get built with Zed), or out-of-tree
      in a separate repo. In-tree-not-in-workspace is simpler for now.

### Core implementation

- [ ] `main.rs`: arg parse, `--acp` / `--login` / `prompt <text>` modes.
- [ ] `chrome.rs`: launch Chrome with dedicated profile + debug port, reuse
      existing instance if already running on that profile (port from
      `gemini_browser`).
- [ ] `acp_server.rs`: minimal JSON-RPC stdio loop. Methods: `initialize`,
      `newSession`, `prompt`, `cancel`. Events: `session/update` with
      `MessageChunk` for streamed text.
- [ ] `cli.rs`: one-shot mode, prints final response to stdout. For smoke
      tests and scripting.
- [ ] `cargo clippy` clean, `cargo test` for the char-boundary truncation
      guard (port the existing test).

### Live DOM tuning (the part `gemini_browser` skipped)

- [ ] `gemini-web-acp --login` → log in as `katopz@maxion.game`.
- [ ] Run a `diagnose` against the signed-in page (port the action from
      `gemini_browser/src/gemini_page.rs::diagnose`) → capture which
      candidate selectors actually match.
- [ ] Pin the winning selectors in `gemini_page.rs::COMPOSER_SELECTORS`
      and `RESPONSE_SELECTORS`. Remove candidates that don't match.
- [ ] End-to-end smoke test: `gemini-web-acp prompt "what is 2+2"` returns
      a sensible answer.

### Zed integration (zero code changes — config only)

- [ ] Build the binary, note its path.
- [ ] Add the `agent_servers.gemini-web` entry to user's settings.json.
- [ ] Restart Zed, start a "Gemini (web)" thread from the agent panel,
      send a prompt, confirm the response streams back.
- [ ] Confirm billing: it uses the existing web subscription, not
      Vertex/API metered billing. (Implicit — traffic goes to
      `gemini.google.com`, not `aiplatform.googleapis.com`.)

### Polish (deferred)

- [ ] Graceful `Browser.close` on shutdown so the Chrome profile isn't
      left dirty.
- [ ] Streaming response (currently waits for text-stability before
      returning the whole response).
- [ ] Prompt-size cap with explicit notice when truncated.
- [ ] `--headless` mode that doesn't need the login flow because the
      profile already has cookies.

## Open questions

1. Crates workspace membership: in `crates/gemini_web_acp/` but excluded
   from the root `Cargo.toml` workspace (so Zed doesn't build it), or
   completely out-of-tree? In-tree-but-excluded is easier to develop.
2. ~~ACP protocol version to target — needs checking what Zed currently
   speaks.~~ **Resolved**: depend on `agent-client-protocol = "=1.3.0"`
   (Zed's own pinned version, root `Cargo.toml`) — schema types come free.
   Custom agents and registry agents go through the same `AgentConnection`
   path in `crates/agent_servers/src/custom.rs`, so as long as we speak the
   same JSON-RPC over stdio, Zed can't tell us apart from real gemini-cli.
3. Does the user want streaming responses in the Zed thread, or is
   "wait for full response, then post once" acceptable for v1? Streaming
   is more work (MutationObserver bridge from page → CDP → ACP events).

## Summary

The real ask is "use my existing Gemini web subscription from Zed without
paying for API/Vertex on top". The `gemini_browser` branch had the right
engine (hand-written CDP over smol) but the wrong shape (UI integration
across 5 Zed surfaces, never validated). This plan keeps the engine, drops
the UI, ships it as a standalone ACP server binary registered as a custom
agent — the same way `gemini-cli` itself is shipped.
