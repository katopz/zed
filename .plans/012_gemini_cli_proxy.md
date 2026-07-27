# Plan 012: `gemini-web` native LLM provider (web-session proxy)

## Problem

`gemini.google.com` runs against the user's existing flat-fee subscription
(Google AI Pro/Ultra, or Workspace Gemini add-on). Every other access path
bills separately, on top of that subscription:

| Path | Uses existing subscription? | Billing |
|---|---|---|
| Web UI (`gemini.google.com`) | ✅ Yes | $0 marginal |
| `gemini-cli` OAuth + Workspace (`katopz@maxion.game`) | ❌ No | GCP/Vertex metered, on top |
| `gemini-cli` OAuth + personal Gmail | ❌ No | Free tier, then hard stop |
| `gemini-cli` API key | ❌ No | Metered |
| **This plan: native provider driving the web UI** | ✅ Yes | $0 marginal |

The web session is the only path that uses the subscription the user already
pays for. Goal: expose that web session as a native Zed LLM provider so the
native Zed agent (and any custom agent that picks a model) can use it from
the model dropdown, exactly like Claude / OpenAI / the existing Google API
provider.

## Architecture: native LLM provider, not ACP, not UI features

Earlier iterations of this plan (and the abandoned `gemini_browser` branch)
went ACP-external-agent and UI-everywhere respectively. Both wrong:

- **ACP external agent** (like `claude-acp`, `gemini-cli`) means a separate
  process that owns its own conversation, tools, history. Wrong shape — the
  user wants to use Zed's native agent, which uses Zed's LLM providers, not
  to start a separate thread type.
- **UI-everywhere** (`gemini_browser` branch: right-click Ask Gemini in 5
  surfaces, ~1700 lines across 12 Zed crates) was never validated against
  real Gemini and made the surface area unmanageable. User explicitly said
  "fuck it", drop all of it.

**Native LLM provider** is the right shape:

- Implement `LanguageModelProvider` for `GeminiWebProvider` and
  `LanguageModel` for `GeminiWebModel` in
  `crates/language_models/src/provider/gemini_web.rs`.
- Register in `register_language_model_providers` (one line, no enum surgery
  — `LanguageModelProviderId("gemini-web".into())`).
- User picks "Gemini Web" from the model dropdown in agent settings, native
  Zed agent uses it as `provider: "gemini-web"`, works with all Zed agent
  features because it's a regular LLM provider.
- Only implement `stream_completion_text` for v1 (no tool calling, no image
  input). Tools/image deferred — those need extra CDP plumbing.

```
Native Zed agent (or any custom agent)
    │ uses normal LanguageModelRequest
    ▼
GeminiWebModel::stream_completion_text   ◄── new code
    │ CDP (WebSocket on localhost)
    ▼
Chrome with dedicated profile (logged in once)
    │ HTTPS
    ▼
gemini.google.com  ◄── uses existing web subscription
```

## Scope

### In scope (v1)

- New file `crates/language_models/src/provider/gemini_web.rs`:
  - `GeminiWebProvider` impl — `id`, `name`, `provided_models`,
    `is_authenticated` (checks profile for valid session cookies),
    `authenticate` (opens visible Chrome for one-time login),
    `authentication_error_message` override (no API key to check),
    `settings_view` (profile path + Chrome path + Sign-in button).
  - `GeminiWebModel` impl — `id`, `name`, `provider_id`, `provider_name`,
    `stream_completion_text`.
  - Internal CDP client module — hand-written JSON-RPC over WebSocket using
    `async-tungstenite` (already a workspace dep, smol stack — no tokio
    infection). Only `Target` + `Runtime` + `Input` + `Page` domains. Drops
    the `chromiumoxide` and `obscura` ideas from earlier iterations.
  - Internal Chrome process management module — launch/reuse Chrome with
    `--user-data-dir=<profile>` + `--remote-debugging-port=<port>`, read
    `DevToolsActivePort`, process-group kill on provider drop.
- Wire into `crates/language_models/src/language_models.rs`:
  `register_language_model_providers` gets one new line.
- New setting in `crates/language_models/src/settings.rs`:
  `GeminiWebSettings { enabled, chrome_path, profile_dir, headless,
  response_timeout_seconds }` (mirror `gemini_browser`'s setting shape —
  that part was fine).
- Default-off in `assets/settings/default.json`.

### Deferred (separate increments, not this plan)

- Right-click "Ask Gemini" context menu entries. User explicitly deferred
  this to a later plan once the provider lands.
- Tool calling. Requires extra CDP plumbing to interact with Gemini's own
  tool UI (search grounding, etc).
- Image input. Requires uploading via the composer's file picker through CDP.
- Streaming responses (v1 waits for text-stability, posts once). Streaming
  needs a `MutationObserver` bridge from the page → CDP `Runtime.bindingEvent`
  → provider → `LanguageModelCompletionEvent`.
- Session-per-thread: v1 reuses one Gemini conversation for all Zed prompts
  (each prompt is sent as a follow-up). 1:1 Zed-thread ↔ Gemini-conversation
  mapping deferred.

### Explicitly out

- No ACP server binary. Wrong shape.
- No `obscura`, no `chromiumoxide`. Real Chrome via hand-written CDP.
- No `stealth`/anti-fingerprinting. Real Chrome + real profile.
- No new UI surfaces in `crates/editor`, `crates/agent_ui`, `crates/zed`,
  etc. The only UI is the standard provider settings view (Sign-in button +
  profile path field) that every other provider already has.

## Hard constraints (unchanged from prior revisions)

- Real Chrome + real profile only. No stealth.
- Single session, human-paced, no bulk automation.
- Default-off. Never shipped as a default Zed provider. The user opts in by
  enabling the setting and clicking Sign-in once.
- If this ever needs to ship to other users, the answer remains the real
  Gemini API path (`crates/language_models/src/provider/google.rs`), not
  this proxy.

## Components

### 1. `crates/language_models/src/provider/gemini_web.rs`

Single new file (keep file count low per house style). Internal structure:

- `GeminiWebProvider` — public struct implementing `LanguageModelProvider`.
  Owns a `GeminiWebBrowserState: Entity<...>` (GPUI entity for change
  notification when auth state flips).
- `GeminiWebModel` — public struct implementing `LanguageModel`. Holds the
  browser state ref + model id/name.
- `GeminiWebBrowserState` — the long-lived state: Chrome process handle,
  CDP connection, Gemini tab target id, last-known login state.
- `cdp` — private module. `Cdp` struct speaking JSON-RPC over smol
  `async-tungstenite`. Methods: `attach_to_page(origin, default_url)`,
  `evaluate(js) -> Value`, `insert_text(text)`, `press_key(key)`,
  `wait_for_selector(selector, timeout)`.
- `chrome` — private module. `launch(profile_dir, headless) -> Child`,
  `find_devtools_port(profile_dir) -> u16`, `reuse_existing(port) -> bool`.
- `gemini_page` — private module. `GeminiPage` driving the page:
  `open(cdp) -> Self`, `ask(prompt) -> String`, `diagnose() -> String`,
  `is_logged_in() -> bool`. Candidate selectors for composer + response
  container — **must be live-tuned against the real signed-in DOM** before
  this plan is considered done.

Estimated size: ~800 lines (one file, internal modules). Roughly the engine
size of the abandoned `gemini_browser` branch minus all the UI integration.

### 2. Registration (one line)

`crates/language_models/src/language_models.rs`,
in `register_language_model_providers`:

```rust
registry.register_provider(
    Arc::new(GeminiWebLanguageModelProvider::new(cx)),
    cx,
);
```

`LanguageModelProviderId("gemini-web".into())` — dynamic, no enum surgery.

### 3. Settings

`crates/language_models/src/settings.rs`:

```rust
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GeminiWebSettings {
    pub enabled: bool,                       // default false
    pub chrome_path: Option<String>,         // null = autodetect
    pub profile_dir: Option<PathBuf>,        // null = <zed-data>/gemini-web/profile
    pub headless: bool,                      // default false (sign-in needs visible)
    pub response_timeout_seconds: u64,       // default 120
}
```

Default-off in `assets/settings/default.json`. Standard settings UI shows
profile path + Sign-in button (every provider already has a settings_view
shape — mirror Google's).

## One-time login flow

1. User enables `gemini_web.enabled` in settings.
2. User opens the provider settings view, clicks **Sign in with Google**.
3. `GeminiWebProvider::authenticate()` launches Chrome visibly on the
   dedicated profile dir, navigates to `https://gemini.google.com`.
4. User logs in once with `katopz@maxion.game` in that Chrome window.
5. `GeminiWebBrowserState` polls for login completion (e.g. composer
   selector appears, or URL changes from `/` to `/app`), flips
   `is_authenticated` to true, `cx.notify()` triggers UI update.
6. Cookies persist in profile dir. All subsequent requests are headless
   and reuse the session.
7. Future logins (cookie expiry) repeat the flow.

## Per-request flow

Each `stream_completion_text(request, cx)` call:

1. Build prompt string from `LanguageModelRequest` (messages → flat text,
   same as `gemini-cli` proxy would). v1 ignores tools, images, system
   prompts — prepends them as text.
2. Acquire the shared `GeminiWebBrowserState` (one CDP connection, serialized
   requests via a smol channel — Gemini's web composer is single-threaded,
   can't handle concurrent prompts).
3. `GeminiPage::ask(prompt)`:
   - Ensure attached to `/app` page.
   - `Input.insertText(prompt)` + submit (NOT JS text assignment — Gemini's
     composer is Quill `contenteditable`, JS assignment leaves its internal
     model empty and send stays disabled).
   - Poll response text every 400ms via `Runtime.evaluate` until 3
     consecutive unchanged polls (text-stability completion detection —
     avoids depending on a "stop generating" class name that changes).
   - Return final text.
4. Emit `LanguageModelCompletionEvent::Text(text)` once with the full
   response. Streaming deferred to a later increment.

## Tasks

### Branch setup

- [x] `git checkout de146c3528c8ad00b023609d08cbc2a032620e41`
- [x] `git checkout -b gemini_cli_proxy`
- [x] Do NOT cherry-pick anything from `gemini_browser`. User said drop it.
      Wrote fresh — the engine code isn't worth saving vs. clean room.

### Core implementation

- [x] `crates/language_models/src/provider/gemini_web.rs` skeleton:
      provider + model structs, traits impl'd with stub methods.
- [x] Wire into `register_language_model_providers` (one line).
- [x] `GeminiWebSettings` in settings.rs, default-off in `default.json`.
- [x] `cdp` module: `Cdp` struct, `attach_to_page`, `evaluate`,
      `insert_text`, `press_key`. Connect via smol TcpStream +
      async-tungstenite client_async, no runtime feature needed.
- [x] `chrome` module: spawn via util::process::Child (process-group
      kill on drop), wait for DevToolsActivePort, find Gemini target
      via HTTP /json.
- [x] `State`: GPUI entity owning Chrome child + cached ws URL + auth
      state. Serialize concurrent requests via smol Semaphore(1).
- [x] `GeminiWebLanguageModelProvider::authenticate`: spawn via
      cx.spawn (not background_spawn — AsyncApp is !Send), launch
      visible Chrome on profile, navigate to gemini.google.com,
      poll for login completion via per-poll CDP connect, flip auth
      state, cache ws_url.
- [x] `GeminiWebModel::stream_completion`: extract everything Send
      from cx up front (cx.update is sync), then build Send-only future
      that connects to cached ws_url, drives GeminiPage::ask, emits
      StartMessage/Text/Stop events.
- [x] `cargo clippy -p language_models --lib --no-deps -- --deny warnings`
      passes clean. `cargo build -p language_models --lib` passes.

### Live DOM tuning (the do-or-die step)

- [ ] Build Zed, enable provider, click Sign-in, log in as
      `katopz@maxion.game`.
- [ ] Verify the candidate selectors in `COMPOSER_SELECTORS` and
      `RESPONSE_SELECTORS` actually match the signed-in DOM. If not,
      update them based on what `GeminiPage::diagnose` (currently dead
      code, can be re-wired as a `gemini-web: diagnose` action later)
      reports.
- [ ] End-to-end smoke: native Zed agent with `provider: "gemini-web"`,
      model `gemini-web-3`, send a prompt, get a sensible reply.

### Polish (deferred increments)

- [ ] Streaming responses via `MutationObserver` → CDP binding → completion
      events.
- [ ] 1:1 Zed-thread ↔ Gemini-conversation mapping (each Zed thread opens a
      fresh Gemini conversation URL).
- [ ] Tool calling (probably out of scope forever — Gemini web tools aren't
      a public surface).
- [ ] Graceful `Browser.close` on provider drop instead of process-group
      kill (avoids "restore pages?" prompt on next Chrome launch).
- [ ] Right-click "Ask Gemini" context-menu entries — separate increment
      once the provider is solid.

### Validation

- [ ] `cargo clippy -p language_models -- --deny warnings` clean.
- [ ] `./script/clippy` clean on the touched files.
- [ ] `cargo test -p language_models` — at minimum a test that
      `truncate_at_char_boundary` is used when capping prompt size.
- [ ] End-to-end: native Zed agent using the provider returns a real reply
      from `gemini.google.com`.

## Open questions

1. **Model list.** Gemini web doesn't expose model selection as a stable
   API. v1 ships one model `gemini-web-3` (or whatever the default is).
   Multi-model (Pro vs Flash vs 2.5 vs 3) deferred until we know how to
   switch via the web UI reliably.
2. **Concurrent requests.** Single CDP connection + serialized requests
   means concurrent Zed agent calls queue. Acceptable for v1 (one user, one
   conversation at a time). Flag for v2.
3. **Cookie expiry handling.** When Gemini session expires mid-use, the
   provider should flip `is_authenticated` to false and surface a re-auth
   prompt. v1 just returns an error; v2 wires the polling.

## Summary

Native Zed LLM provider in one new file
(`crates/language_models/src/provider/gemini_web.rs`), registered in one
line, default-off, that drives real Chrome via hand-written CDP to send
prompts to `gemini.google.com` under the user's existing web subscription.
The user picks it from the model dropdown like any other provider; the
native Zed agent uses it like any other model. Right-click UI and streaming
are deferred.
