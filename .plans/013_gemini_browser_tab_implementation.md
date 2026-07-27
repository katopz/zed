# Plan 013: Internal browser tab (CDP + texture-rendered pane) + Gemini MCP proxy

Builds on the decision record in [012](./012_gemini_browser_proxy_poc.md). This
is the concrete implementation plan; 012 stays as the "what was asked / what
was found / what I won't do" record and isn't restated here except where a
design choice here changes something 012 assumed.

## Problem

Add an internal browser tab to Zed (originally scoped off obscura), used as a
PoC to talk to `gemini.google.com` under the user's own logged-in session:
Gemini reachable as a provider, GLM-callable, a manual "ask Gemini" button,
and thread continuity — plus, from this session: any link should default to
opening in this internal browser (with a setting to fall back to the OS
browser), and free-form DOM access (debug/communicate/inject at will), ideally
with the page actually rendered inline as a GPUI pane rather than a separate
window.

## Design pivot: real Chrome over CDP, not obscura

012 concluded obscura could only ever be the "without render" half (it has no
layout/paint pipeline at all — confirmed by reading its source, no
`screenshot`/`Page.captureScreenshot` equivalent anywhere), forcing a split
design: a separate webview crate for the visible/login half, obscura for
headless calls, and — since GPUI has no native child-view hosting API either
(confirmed: no `child_view`/`platform_view`/`NativeView` anywhere in
`crates/gpui*`) — the visible half would have to be its own native OS window,
never a real Zed pane.

This session's texture idea removes that constraint. **Driving a real
installed Chrome/Chromium via the Chrome DevTools Protocol** (rather than
obscura's from-scratch DOM/V8 engine) gets:

- **Actual rendered frames**: `Page.captureScreenshot` / `Page.startScreencast`
  produce real encoded frames of the real page, because it's Chrome's own
  Blink+compositor doing the rendering — something obscura structurally
  cannot do.
- **Full DOM/debug/inject access**: CDP's `DOM` domain (query/inspect/mutate
  nodes), `Runtime.evaluate`/`Runtime.callFunctionOn` (arbitrary JS), `Input`
  domain (synthetic mouse/key events) — this is literally the devtools
  protocol, so "access the DOM to debug/communicate/inject as we please" is
  the protocol's native purpose, not something bolted on.
- **A real login, no spoofing**: launching real Chrome against the user's own
  (or a dedicated secondary) profile directory means an actual logged-in
  Gemini session — no `stealth`/fingerprint-rotation code needed anywhere,
  since it's a genuine browser rather than an impersonation of one.
- **One engine for both "render" and "no render"**: the same CDP connection
  works against headed or headless (`--headless=new`, which still renders
  internally and still answers `captureScreenshot`) Chrome — no separate
  obscura-headless / webview-headed split.
- **A route around the GPUI embedding wall**: GPUI can't host a native child
  view, but it *can* composite pixel/texture content (`Canvas` —
  `crates/gpui/src/elements/canvas.rs:10,23`, custom GPU paint callback;
  `Img` — `crates/gpui/src/elements/img.rs:192`, bitmap via the image atlas).
  Feeding decoded screencast frames into one of those turns the browser tab
  into a genuinely inline GPUI pane, not a separate window.

Recommended CDP client crate: `chromiumoxide` (Apache-2.0/MIT, actively
maintained — confirmed via its GitHub: launches or connects to headed/headless
Chrome, full protocol coverage code-generated from Chrome's `protocol.json`
including `DOM`, `Page`, `Runtime`, `Input`, `Page::execute` for raw command
access). This replaces obscura as the engine dependency; obscura's `Page` API
shape (`goto`/`evaluate`/`query_selector`/`wait_for_selector`) is still worth
mirroring in our own wrapper type since it already proved to be the right
surface for the DOM inject/read work in 012.

**Open risk, not yet validated — needs a throwaway spike before committing
further**: decoding a screencast/screenshot frame and uploading it through
GPUI's image atlas every frame may or may not sustain a usable frame rate
(Gemini's UI is mostly static text with bursty streaming updates, not video,
so this is likely fine, but it hasn't been measured). If the spike shows it
can't keep up, the fallback is the previous plan's design: a separate native
window owned by the Zed window (macOS `NSWindow addChildWindow:` /
Windows owner-window), same CDP backend, same DOM/inject mechanics — only the
"where do the pixels live" answer changes.

## Components

### 1. `crates/browser_tab` — CDP session + GPUI pane

New crate (`[lib] path = "browser_tab.rs"`), single logical component:

- `browser_tab.rs` — `Browser`/`Tab` types: launch-or-connect to Chrome
  (`chromiumoxide::Browser::launch`/`connect`) with a dedicated
  `--user-data-dir` and fixed `--remote-debugging-port` (needed so the
  standalone MCP server in component 2 can attach to the same instance);
  `Tab::evaluate(js) -> Value`, `Tab::query_selector`, `Tab::wait_for_selector`,
  `Tab::content()` — mirrors obscura's `Page` surface from 012 since that
  shape already covers the inject/read use case.
- `frame_source.rs` — wraps `Page.startScreencast` (preferred: push-based,
  avoids polling) with `Page.captureScreenshot` polling as a fallback if
  screencast proves unreliable; decodes frames to a pixel buffer.
- `pane.rs` — the GPUI element rendering the current frame via `Img`/`Canvas`
  inside a normal Zed pane/tab; forwards the pane's own GPUI mouse/key events
  to `Input.dispatchMouseEvent`/`dispatchKeyEvent` (coordinate-translated to
  the CDP viewport) so the user can interact with the page normally — this is
  the part that needs the perf spike above before real investment.
- `settings.rs` — new setting `open_links_in: "internal_browser" |
  "system_browser"` (default `"internal_browser"` per this session's ask),
  in `settings_content` alongside existing project/agent settings.
- Feature-flag gated (`crates/feature_flags`), off by default — this stays a
  local, opt-in experiment per the 012 boundary, not a shipped default even
  though the *link-opening default* the user wants is "internal browser
  first" — the flag gates whether the feature exists/builds into a runnable
  path at all, not the internal-vs-external default once enabled.

Link interception: `App::open_url` (`crates/gpui/src/app.rs:1408`) is the
single choke point all ~195 `cx.open_url(...)` call sites already go through
(terminal hyperlinks, markdown links, agent chat links, editor cmd-click).
Add the setting check there: internal browser tab if enabled and the flag is
on, else fall through to the existing `self.platform.open_url(url)`. No
per-feature call-site changes needed.

### 2. `crates/gemini_mcp` — standalone MCP server binary

A separate local process, not linked into the main Zed binary, matching how
Zed already runs and connects to any external MCP server
(`crates/context_server`: stdio transport + `context_servers` setting in
`settings_content::agent`/`project` — no new Zed-side MCP-client code needed
at all).

- Connects via its own `chromiumoxide` CDP client to the **same**
  `--remote-debugging-port`/target the in-Zed `browser_tab` pane is showing.
  CDP natively supports multiple concurrent debugger clients against one
  target, so this needs no IPC/bridge to the Zed process — just pointing at
  the same port. (Needs verifying during implementation that concurrent
  `Runtime.evaluate` calls from two independent CDP clients against the same
  page don't race destructively for this use case — expected fine since
  calls are short request/response, but flag as a thing to actually test.)
- Exposes `gemini_ask(prompt, timeout_ms?) -> { response }` (inject + wait +
  read as one call), optionally split `gemini_inject`/`gemini_read_response`.
- Registered once in the user's local `context_servers` setting; after that
  GLM, the manual button, and any other MCP-capable caller all use the same
  tool — no bespoke per-caller plumbing.
- Does not depend on obscura's own `obscura-mcp` (built around obscura's
  headless `Page`, not a live CDP session) — useful only as a reference for
  MCP tool/protocol shape.

### 3. Gemini-specific glue

- Concrete selectors for Gemini's prompt input and response container aren't
  knowable in advance (not public, likely a framework-managed contenteditable
  rather than a plain `<textarea>`) — this needs live DOM inspection during
  implementation, done through the same `Tab::evaluate`/DOM access this plan
  already builds, and is expected to be the most fragile part (breaks on
  Gemini UI changes).
- Response-ready detection: poll/observe for the "stop generating"/streaming
  affordance to disappear before reading final text.
- Thread alignment stays local-only per 012 — a stored mapping of Gemini's
  own conversation URL/id (read from the page, e.g. the URL fragment after
  starting a chat) to Zed's `session_id`, not a real bidirectional sync since
  there's no external thread-list API being talked to.

## Non-goals

- No `stealth`/fingerprint spoofing anywhere — real Chrome + a real profile
  makes it unnecessary.
- No dependency on obscura for the engine (its lack of a paint pipeline is
  the reason for this whole pivot); it remains referenced only as prior art
  for API shape and MCP tool-shape.
- Not shipped as a default-enabled feature to all Zed users — feature-flag
  gated, local PoC, per 012.
- Not building full general-purpose browser chrome (address bar, history UI,
  bookmarks, tab strip beyond what's needed to show the Gemini tab) — scope
  is the PoC, not a browser product.
- Not attempting mass/bulk automation or multi-account handling — single
  profile, single visible session, human-paced.

## Files (new/touched)

- `crates/browser_tab/browser_tab.rs`, `frame_source.rs`, `pane.rs`,
  `settings.rs`, `Cargo.toml` (new crate)
- `crates/gemini_mcp/main.rs`, `Cargo.toml` (new crate, standalone binary)
- `crates/gpui/src/app.rs` — `open_url` gains the setting check
- `crates/settings_content/src/project.rs` (or wherever `open_links_in` best
  fits existing settings groupings) — new setting field
- `assets/settings/default.json`, `docs/src/reference/all-settings.md` —
  document the new setting (mechanical, mirrors any other settings addition)
- `crates/feature_flags/src/feature_flags.rs` — new flag for this feature
- `crates/agent_ui/src/conversation_view/thread_view.rs` — manual "ask
  Gemini" button, mirroring the existing manual auto-prompt sparkle button
  pattern (`thread_view.rs:7248`/`manual_auto_prompt()`), calling the MCP
  tool instead of duplicating DOM logic

## Increment 1 as built — deviations from the design above

The first increment delivers the end-to-end path the user actually asked for
(right-click selected text → Gemini answers → reply opens in Zed) and
deliberately defers the texture-rendered pane, which is the only genuinely
risky part. Real Chrome shows its own window, so nothing about the core value
depends on rendering into GPUI.

Three design choices changed once the code was written against the codebase:

1. **A hand-written CDP client instead of `chromiumoxide`.** `async-tungstenite`
   is already a workspace dependency and CDP is JSON-RPC over a plain localhost
   WebSocket, so `crates/gemini_browser/src/cdp.rs` speaks it directly over
   Zed's existing smol stack. `chromiumoxide` is tokio-based; adopting it would
   have meant hosting a tokio runtime inside a smol process for four CDP
   domains' worth of use. Only `Target`, `Runtime`, `Input`, and `Page` are
   needed, and everything (inject, submit, read) goes through `Runtime.evaluate`
   plus `Input.insertText`/`dispatchKeyEvent`.
2. **A setting, not a `feature_flags` flag.** `crates/feature_flags` is Zed's
   *server-announced / staff* rollout mechanism; a user cannot flip those
   locally, which is the opposite of what a local opt-in PoC needs. Gating is
   `gemini_browser.enabled` (default `false`) in `settings_content`, which also
   gates whether the context-menu item is even shown.
3. **`Input.insertText` rather than JS text assignment.** Gemini's composer is a
   Quill `contenteditable`; assigning `textContent` leaves its internal model
   empty and the send button disabled. Going through Chrome's real input
   pipeline avoids that class of problem entirely, and needs no JS string
   escaping for the prompt.

Also worth recording: response completion is detected by **text stability**
(unchanged for 3 consecutive 400ms polls) rather than by watching for a
"stop generating" button, so it does not depend on one more unstable class
name. The selector in use is pinned before sending when one already matches,
because re-resolving mid-wait could switch selectors and make an older message
look like the new reply.

### Files added/changed in increment 1

- `crates/gemini_browser/{Cargo.toml,src/gemini_browser.rs,src/cdp.rs,src/gemini_page.rs}`
- `crates/settings_content/src/settings_content.rs` — `GeminiBrowserSettingsContent`
- `crates/settings/src/vscode_import.rs` — new field in the struct literal
- `assets/settings/default.json` — defaults (disabled)
- `crates/zed_actions/src/lib.rs` — `gemini_browser` action namespace
- `crates/editor/src/mouse_context_menu.rs` — "Ask Gemini" entry, gated on
  `has_selections` and the setting
- `crates/zed/src/zed.rs` — three action handlers + result/toast helpers
- `crates/zed/src/main.rs` — `gemini_browser::init(cx)`

### How to use increment 1

1. Add to Zed settings:
   ```json
   "gemini_browser": { "enabled": true }
   ```
   (`chrome_path` only if Chrome is not in a standard location.)
2. Run `gemini_browser: open gemini browser` from the command palette. A Chrome
   window opens on a dedicated profile under
   `<zed-data-dir>/gemini_browser/profile`. **Sign in to Gemini there once** —
   the cookie persists for later runs.
3. Select code in the editor, right-click → **Ask Gemini**. A toast shows while
   it works; the reply opens as a read-only Markdown tab.
4. If it reports that it cannot find the prompt box, run
   `gemini_browser: diagnose gemini browser` — it prints the current URL/title
   and which candidate selectors match, which is what to update in
   `COMPOSER_SELECTORS`/`RESPONSE_SELECTORS` in `gemini_page.rs`.

### Verification status

- `cargo check -p gemini_browser`, `-p editor`, `-p zed --bin zed` — all clean
  (the whole Zed binary type-checks).
- `cargo test -p gemini_browser` — 1 test passes (the char-boundary truncation
  guard).
- `./script/clippy -p gemini_browser` — exit 0, including `cargo machete`
  (which caught two genuinely unused deps, `schemars` and `serde`, now removed).
- `cargo clippy -p editor` / `-p zed --all-targets -- --deny warnings` — exit 0,
  zero findings in the touched files.
- **Not yet run: the release-profile `./script/clippy` for `editor`/`zed`.** The
  `target/release` directory is in a corrupted state in this working copy
  (duplicate `rustix` rmeta, `can't find crate for gpui` in untouched crates
  like `release_channel`, `wasmtime-c-api-impl` failing to build). This volume
  does not support hard links, so the incremental cache is unreliable
  (`hard linking files in the incremental compilation cache failed` recurs
  throughout every build), and several builds here were killed by timeouts. The
  same release-profile invocation passes for `gemini_browser` and for untouched
  `util`, so this is environmental. A `cargo clean --release` followed by a full
  rebuild is the fix, which is worth doing before opening a PR.
- **Nothing has been run against the real Gemini site.** No end-to-end
  behavioral verification exists: the selectors are educated guesses, not
  observed. The first real run is expected to need selector tuning.

### Known limitations of increment 1

- Selectors are best-guess candidate lists; they need live tuning against the
  real signed-in DOM, which is what `gemini_browser: DiagnoseGeminiBrowser`
  exists for. This is the most likely thing to need fixing first.
- Chrome is killed when Zed exits (process-group kill via
  `util::process::Child`), which can leave the profile dirty enough for Chrome
  to offer "restore pages?" on next launch. A graceful `Browser.close` on
  shutdown is not implemented.
- No streaming: the reply appears in Zed only once complete.
- Thread mapping, the MCP server, `open_links_in` link interception, and the
  GPUI texture pane are all still unbuilt (see tasks below).

## Tasks

- [x] `crates/gemini_browser`: CDP client over `async-tungstenite`/smol —
      launch Chrome with a dedicated profile, read `DevToolsActivePort`,
      attach a flattened page session, `evaluate`/`insert_text`/`press_enter`
- [x] `crates/gemini_browser`: Gemini page automation — candidate selectors,
      focus+replace composer, submit, text-stability response wait, `diagnose`
- [x] `gemini_browser.enabled` setting (default off) + `settings_content`,
      `default.json`, `vscode_import` plumbing
- [x] `gemini_browser` action namespace in `zed_actions`
- [x] "Ask Gemini" editor context-menu entry, gated on selection + setting
- [x] Workspace handlers: ask-about-selection, open browser (sign-in),
      diagnose selectors; reply opens in a read-only Markdown buffer
- [x] Reuse an already-running Chrome on the same profile instead of stalling
      until the startup timeout
- [ ] **Live-tune the Gemini selectors** against the real signed-in DOM using
      the diagnose action (expected first fix)
- [ ] Graceful `Browser.close` on shutdown so the profile is not left dirty
- [ ] Spike: `Page.startScreencast` frames decoded into a GPUI `Img`/`Canvas`
      at an acceptable rate for a mostly-static chat UI (the deferred
      texture-pane risk; fallback is an owned native window)
- [ ] `crates/browser_tab`: generalize beyond Gemini — `wait_for_selector`/
      `content` and a reusable tab abstraction
- [ ] `crates/browser_tab`: frame source (screencast preferred, screenshot
      polling fallback) + GPUI pane element rendering it
- [ ] `crates/browser_tab`: input forwarding (GPUI pane events →
      `Input.dispatch*`)
- [ ] `crates/browser_tab`: fallback to an owned native window if the texture
      spike fails on a given platform
- [ ] `open_links_in` setting (`settings_content`, `default.json`,
      `all-settings.md`) + `App::open_url` interception
      (`crates/gpui/src/app.rs:1408`)
- [ ] `crates/gemini_mcp`: standalone MCP server binary, CDP-attaches to the
      same target, exposes `gemini_ask`/`gemini_inject`/`gemini_read_response`
- [ ] Live DOM inspection of `gemini.google.com` to find real selectors for
      prompt input + response container + streaming-done signal
- [ ] Manual "ask Gemini" button in `thread_view.rs`, calling the MCP tool
- [ ] Local thread-mapping store (Gemini conversation id/url ↔ Zed
      `session_id`)
- [ ] `./script/clippy` clean
