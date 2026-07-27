# Plan 012: Gemini-via-headless-browser proxy (PoC) — request summary & decision record

## What was asked

Referencing https://github.com/h4ckf0r0day/obscura, add a "browser tab" feature
to Zed and use it to proxy https://gemini.google.com/ (the Gemini *web app*,
not the Gemini API) through that embedded browser. Specifically:

1. **Proxy Gemini as a new AI provider** — route provider calls through the
   embedded browser tab instead of Google's official API.
2. **GLM (the current agent) can call the proxy Gemini and get a result**,
   acting as an impersonated logged-in user — described as "like we did in
   `auto_prompt`."
3. **A manual button** ("ask proxy Gemini"), mirroring the existing manual
   auto-prompt button, that returns 2-3 paragraphs of Gemini's reasoning as
   context.
4. **Thread alignment** — Gemini's web UI has its own thread
   create/list/search; capture/mirror that so a Gemini web thread lines up
   with the corresponding Zed thread.

Expectation was that this needs new crate(s) under `crates/` plus a feature
flag.

## What was found

- **obscura is not a Gemini-specific tool.** It's a standalone Rust headless-
  browser engine (embedded V8 + Chrome DevTools Protocol, positioned as a
  drop-in Puppeteer/Playwright-compatible replacement for headless Chrome),
  with an optional "stealth mode" (anti-fingerprinting, tracker blocking) and
  an MCP server for AI-agent integration. It has no built-in concept of
  Gemini, chat threads, or session impersonation — using it for this task
  means driving it ourselves against `gemini.google.com`.
- **`crates/auto_prompt` is not a precedent for browser impersonation.** It is
  pure decision logic: on `AcpThreadEvent::Stopped` it calls an LLM through
  Zed's own `LanguageModelRegistry` (a real, configured API key) to decide
  whether to auto-dispatch a "keep working" follow-up into the *same Zed
  agent thread*. It never opens a browser, never touches a logged-in web
  session, and has no dependency on any webview/CDP/headless-browser crate
  (confirmed via dependency and repo-wide grep). Its "thread" concept is
  purely Zed's internal ACP `session_id` — there is no external/remote thread
  list anywhere in it.
  - Manual-trigger UI precedent does exist and is reusable as a UI pattern:
    the sparkle "kick it now" button at
    `crates/agent_ui/src/conversation_view/thread_view.rs:7248` calling
    `manual_auto_prompt()` (`thread_view.rs:8221`).
  - Feature-flag precedent exists at `crates/feature_flags/` (`FeatureFlag`
    trait, `FeatureFlagAppExt::has_flag`/`when_flag_enabled`), though
    `auto_prompt` itself doesn't use it — it gates via its own per-thread
    toggle + `~/.config/zed/auto_prompt.json`.
- **Zed already has an official, legitimate Gemini provider.** `crates/google_ai/`
  (Google Generative Language API client) and
  `crates/language_models/src/provider/google.rs` (wired into
  `LanguageModelRegistry`, with API-key auth and settings UI) already let any
  Zed agent — including GLM — call real Gemini models today. No browser
  needed for provider access, manual "ask Gemini" buttons, or per-thread model
  calls; those are all buildable directly on this existing path.

## What I don't want to do

I pushed back on building the browser-impersonation path as originally
scoped, for one specific reason: as a **shipped, mergeable Zed feature**, an
embedded stealth headless browser driving a user's own logged-in
`gemini.google.com` session to extract chat responses as a stand-in API is
automated, anti-detection access to a consumer product specifically to
bypass its official API/ToS — and merging it into Zed would hand that
capability to every Zed user by default, not just enable one personal
experiment.

The user clarified this is a **local-only PoC**, on their own machine, using
their own paid Gemini subscription and their own paid Claude access, not a
feature intended to ship to other Zed users. That materially changes the
picture — automating one's own already-authenticated session for a personal,
non-distributed experiment is a different risk class than shipping it as
default product behavior. Proceeding on that basis, with these boundaries
kept regardless:

- **Not implementing obscura's "stealth mode" / anti-fingerprinting.**
  Building in deliberate anti-bot-detection evasion is a step further than
  "automate my own logged-in session" — it's specifically about defeating
  Google's automated-abuse defenses. The PoC will use plain CDP-level
  automation only.
- **Gated behind a feature flag, off by default, not wired into any release
  build path** — this stays an explicit opt-in local experiment
  (`crates/feature_flags` mechanism), not something enabled for the general
  Zed user base.
- **No volume/scale automation** — single-session, human-paced interaction
  matching normal manual usage, not a bulk-query or resale mechanism.
- **No claim of it becoming an official/merged provider** — if the PoC
  proves out and there's a desire to ship something to other users, that
  should be the *real* Gemini API path (`google_ai`/`language_models::provider::google`),
  not the browser-scraping path.

## Open items for the implementation plan

- Confirm which piece of obscura to vendor/depend on (the engine crate
  itself vs. driving a system Chrome via plain CDP) and whether a new
  `crates/gemini_browser_proxy` (or similar, single new crate housing tab
  management + CDP client + Gemini DOM/session glue) is the right shape vs.
  folding into an existing crate per the "avoid many small files" guideline.
- Design the feature-flag name/gate and where the manual-ask button and
  provider registration hook in (`agent_ui`, `language_models`).
- Decide how "thread alignment" is represented locally, since there is no
  real external thread-list API to sync against — likely a locally-stored
  mapping (Gemini web conversation ID/URL captured from the DOM ↔ Zed
  `session_id`) rather than a genuine bidirectional sync.

## Browser tab — confirmed obscura API & concrete design

Pulled obscura's actual workspace/source (`h4ckf0r0day/obscura`, not just its
README) to answer directly: **yes, we can add a browser tab using obscura**,
with one hard constraint that changes the shape of "with/without render":

- Workspace = `obscura-dom` (html5ever/selectors DOM tree), `obscura-net`
  (fetch/cookie jar via reqwest, no cookie-crate dep — obscura owns its own
  `CookieJar`), `obscura-browser` (`Page`/`BrowserContext`/profiles),
  `obscura-cdp` (CDP protocol server, for external Puppeteer/Playwright
  clients), `obscura-js` (V8 bindings/ops), `obscura-cli`, `obscura-mcp`, and
  the embeddable top-level `obscura` crate (`Browser`/`Page`/`Cookie`/`Error`).
  Not on crates.io — path/git dependency only.
- **No layout or paint pipeline exists anywhere in the workspace.** `Page` has
  `goto`, `evaluate` (raw JS), `content()` (`document.documentElement.outerHTML`
  string), `query_selector`/`wait_for_selector` (via injected `_nid` id +
  `evaluate`), `Element::{text, attribute, click}`, `on_request`/`on_response`
  interception, `enable_interception`. There is no `screenshot()`/CDP
  `Page.captureScreenshot` equivalent — it cannot produce pixels, only DOM/JS
  results. So obscura is inherently the **"without render"** half only.
- `BrowserConfig` supports `stealth: bool` (fingerprint/profile spoofing,
  explicitly built to evade bot-detection heuristics — the maintainers' own
  comment notes identity-rotation is itself a bot signal) and `storage_dir`
  (persistent `CookieJar` on disk, so a session logged in once can be reused
  headlessly across runs). Per the earlier boundary, `stealth` stays `false`.

**Design: two tracks, one shared cookie jar.**

1. **"With render" — the actual visible browser tab.** obscura can't paint,
   so this needs a real OS webview (e.g. `wry`/`tao`, or a native
   WKWebView binding on macOS), which is a new dependency Zed doesn't
   currently have (confirmed: no `wry`/`tao`/CEF/servo-renderer anywhere in
   `Cargo.lock` or any crate). This is the panel the user actually looks at
   and logs into `gemini.google.com` through normally — a real, undetectable
   browser because it *is* a real browser, no spoofing needed. On successful
   login its cookie jar is exported to the same `storage_dir` obscura reads.
2. **"Without render" — obscura, headless.** Once `storage_dir` has a valid
   Gemini session cookie, `obscura::Browser::builder().storage_dir(dir).build()`
   drives `gemini.google.com` in the background (no window) for the actual
   provider calls: `goto`, `wait_for_selector` on the prompt textarea,
   `evaluate`/`click` to submit, `wait_for_selector`/`evaluate` on the
   response container to read text back out. This is the path the manual-ask
   button and GLM-initiated calls actually use at request time.

So "with render" and "without render" are two different engines sharing one
cookie store, not one engine with a flag — render is only for the one-time
human login step; every subsequent automated call is headless obscura.

## Prompt injection + MCP wrapper (convenience layer)

Follow-up ask: since the user still watches/uses the real Gemini GUI
themselves, can we skip manual copy/paste by injecting the prompt into the
input box and hooking the response programmatically, and wrap that as an
MCP tool? **Yes — this is straightforward and lower-risk than the headless
path**, because it operates on the same visible tab the user is already
looking at (no hidden background scraping, no need for obscura or stealth at
all for this piece).

### Mechanics

This targets the **visible webview tab** (the "with render" track above),
not headless obscura — obscura has no paint output so it can't be the
surface the user watches. Whatever webview crate backs the visible tab
(`wry`/`tao` or a native binding) needs only one primitive: an
`evaluate_script(js) -> result` call, the same shape obscura's own
`Page::evaluate` already has. With that:

- **Inject**: find Gemini's prompt textarea (`document.querySelector` on its
  input element — exact selector to be captured by hand during the PoC,
  since Gemini's DOM/class names aren't public and will need live
  inspection), set its content, dispatch a real `input`/`InputEvent` (Gemini's
  editor is almost certainly a rich-text/contenteditable box, not a plain
  `<textarea>`, so a simple `.value =` assignment likely won't trigger its
  framework's state — needs `execCommand('insertText', ...)` or a dispatched
  `InputEvent` with `data`, discoverable during PoC), then click/dispatch the
  send button.
- **Hook the response**: poll (or `MutationObserver` bridged back through
  `evaluate_script`) the response container until Gemini's own
  "stop generating" affordance disappears (stream finished), then read the
  finished response element's text out.
- This is the exact same shape as the existing manual auto-prompt button
  (`thread_view.rs:7248` → `manual_auto_prompt()`) — a one-shot
  "send input, await output" round trip — just retargeted at a live web
  page's DOM instead of an ACP thread.

### MCP wrapper

Zed already has a full MCP **client** (`crates/context_server`: transport,
protocol, stdio-based server processes registered via `context_servers` in
`settings_content::agent`/`project`). So no new MCP-client work is needed in
Zed itself — the missing piece is a small local MCP **server** process that
owns the live webview tab and exposes it as tool(s), e.g.:

- `gemini_ask(prompt: string, timeout_ms?: number) -> { response: string }`
  — inject + wait + read, as one call.
- Optionally split into `gemini_inject(prompt)` / `gemini_read_response()` if
  a caller wants to watch it stream rather than block for the final text.

Once that server is registered locally (same mechanism as any other MCP
server in Zed's settings), it's usable identically by: GLM calling it as a
tool mid-conversation (original ask #2), the manual button just invoking the
same tool instead of duplicating the DOM logic (#3), and any other
MCP-capable agent — no Zed-specific UI plumbing required beyond the button
itself.

obscura ships its own `obscura-mcp` crate, but its tool set is built around
obscura's own headless `Page`, not a live webview session, so it isn't a
drop-in server for this — it's a useful reference for MCP tool/protocol
shape, not something to depend on directly here.

### Still local-only

This changes nothing about the earlier boundaries: single visible session
the user is actively watching, human-paced (bounded by how fast the user
would manually copy/paste anyway), no stealth, not wired into any shipped
default — the MCP server is a local process the user runs and registers
themselves for their own PoC.

## Default link handling + confirmed window architecture

Follow-up requirement: any link should open in the internal browser by
default from now on, with a setting to fall back to the OS default browser;
if the internal browser "can't render in tab" (GUI not capable), opening it
as a separate modal/window instead is acceptable — as long as DOM
grep/inject/submit/response-readback still works.

Researched both parts concretely against GPUI rather than assuming:

### Link interception — one choke point, not scattered

`App::open_url` (`crates/gpui/src/app.rs:1408`) is the single place every
"open a link" feature in Zed calls through — terminal hyperlinks
(`terminal_view.rs:1215`), markdown links (`markdown.rs:1450,2021,2925`),
agent panel chat links (`conversation_view.rs:2618`), editor cmd/ctrl-click
(`navigation.rs:1117`), ~195 call sites total, all `cx.open_url(url)`. It
currently delegates straight to `self.platform.open_url(url)`
(`crates/gpui/src/platform.rs:175`, implemented per-OS). Adding a setting
(e.g. `open_links_in: "internal_browser" | "system_browser"`, default
internal) and checking it inside `App::open_url` before deciding whether to
route to the internal browser tab vs. fall through to
`self.platform.open_url(url)` covers every existing call site for free —
no per-feature changes needed.

### The "browser tab" is necessarily a separate native window, not a GPUI pane

This isn't a fallback-only case — it's the actual architecture. Confirmed by
reading GPUI's rendering internals: GPUI has **no child-view/native-view
hosting primitive at all** (no `child_view`/`platform_view`/`NativeView`
anywhere in `crates/gpui`, `gpui_macos`, `gpui_linux`, `gpui_windows`,
`gpui_web`; zero `wry`/`WebView` references in the whole repo). GPUI owns and
paints its entire window surface itself (one `NSView` per window on macOS,
`crates/gpui_macos/src/window.rs:882-984`); its only compositing primitives
for foreign content are pixel/texture-based (`Canvas`, `Img`), not a live
independently-driven surface like a webview needs. So a real webview can
never be an inline element inside a Zed tab/pane — it can only be its own
native OS window.

Design: the internal browser is a **separate native window** (via whatever
webview crate is chosen — `wry`/`tao` or a native WKWebView binding),
created as a *child/owned* window of the main Zed window where the platform
supports it (e.g. macOS `NSWindow addChildWindow:`, Windows owner-window
relationship) so it tracks/stays-with the Zed window and reads as "attached"
to the user even though it's not literally docked — rather than a true GPUI
pane. "Modal window" framing from the ask is directionally this same thing;
concretely it should be a normal-but-owned window rather than a blocking
modal, since the user needs to keep interacting with both Zed and the
browser window at once.

Whether it's this owned window (the real design) or, on a platform where
even child-window ownership isn't available, a fully independent top-level
window (degraded fallback) — the invariant stays the same either way: the
webview's own script-evaluation API (`evaluate_script`/equivalent) is what
DOM query/inject/submit/read-response is built on, and that doesn't change
based on window ownership. Window architecture and the inject/hook mechanics
from the prior section are orthogonal — this section only changes *where
the pixels live*, not how automation talks to the page.
