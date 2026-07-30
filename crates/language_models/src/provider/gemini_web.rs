//! Native LLM provider that drives `gemini.google.com` under the user's
//! existing web subscription (Google AI Pro/Ultra, Workspace Gemini add-on)
//! via a real Chrome instance controlled over the Chrome DevTools Protocol.
//!
//! Why this exists: every other access path (`gemini-cli` OAuth, API key,
//! Vertex AI) bills through GCP/Vertex metered pricing, separate from and on
//! top of the flat-fee web subscription. The web session is the only path
//! that uses the subscription the user already pays for.
//!
//! Auth model: the user clicks "Sign in" once in the provider settings; a
//! visible Chrome window opens on a dedicated profile directory; the user
//! logs into `gemini.google.com` normally; cookies persist in the profile;
//! every subsequent `stream_completion_text` reuses the session headlessly.
//!
//! See `.plans/012_gemini_cli_proxy.md` for the design record.

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::{BoxStream, self}};
use gpui::{App, AsyncApp, Entity, FontWeight, Task};
use language_model::{
    InlineDescription, InlineProviderSettings, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelId, LanguageModelName, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelProviderName, LanguageModelProviderState,
    LanguageModelRequest, LanguageModelToolChoice, LanguageModelToolSchemaFormat,
    MessageContent, ProviderSettingsView, Role, StopReason,
};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use settings::Settings;
use smol::{fs, net::TcpStream};
use std::{
    future::Future,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use ui::{IconName, prelude::*};
use util::truncate_to_byte_limit;

use crate::AllLanguageModelSettings;

/// Provider id used in `LanguageModelProviderId`. Picked to distinguish from
/// the existing Google API provider (`google`).
pub const GEMINI_WEB_PROVIDER_ID: LanguageModelProviderId =
    LanguageModelProviderId::new("gemini-web");
pub const GEMINI_WEB_PROVIDER_NAME: LanguageModelProviderName =
    LanguageModelProviderName::new("Gemini Web");

const GEMINI_ORIGIN: &str = "https://gemini.google.com";
const GEMINI_APP_URL: &str = "https://gemini.google.com/app";

/// Default Chrome flags: `--remote-debugging-port=0` lets Chrome pick a free
/// port; we read it back from `DevToolsActivePort` in the profile dir.
const CHROME_FLAGS: &[&str] = &[
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-features=Translate",
];

/// Per-request prompt size cap. Gemini's web composer has its own limit; this
/// keeps us from sending the entire repo by accident.
const MAX_PROMPT_BYTES: usize = 32_000;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct GeminiWebSettings {
    /// Whether the provider appears in the model dropdown at all. Default
    /// off — this is a local-only PoC, never shipped as a default Zed feature.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the Chrome/Chromium binary. `None` means autodetect the
    /// standard install locations for the current platform.
    #[serde(default)]
    pub chrome_path: Option<String>,

    /// Profile directory used by the dedicated Chrome instance. `None` means
    /// `<zed-data-dir>/gemini-web/profile`. Cookies persist here so the
    /// one-time login survives restarts.
    #[serde(default)]
    pub profile_dir: Option<String>,

    /// Run Chrome without a visible window. Leave off until the profile has
    /// a valid signed-in session; the initial login needs a visible window.
    #[serde(default)]
    pub headless: bool,

    /// How long to wait for a Gemini response before giving up, in seconds.
    #[serde(default = "default_response_timeout_seconds")]
    pub response_timeout_seconds: u64,
}

fn default_response_timeout_seconds() -> u64 {
    120
}

/// Long-lived state shared by the provider and every model it produces.
/// Owns the Chrome child process, the CDP connection, and the cached
/// authentication state.
pub struct State {
    /// Set once Chrome has been launched at least once and we know the
    /// DevTools port. Cleared if Chrome dies.
    browser: Mutex<Option<BrowserHandle>>,
    /// Cached websocket URL of the Gemini page target, refreshed by
    /// `authenticate` and after target-list changes. Reading this
    /// synchronously (no await) in `stream_completion` is how we avoid
    /// holding `AsyncApp` across the `.boxed()` boundary.
    gemini_target_ws: Mutex<Option<String>>,
    /// True iff `gemini.google.com` shows a logged-in session (composer
    /// selector matches and no sign-in prompt). Refreshed by `authenticate`
    /// and after each request when the response selector misses.
    authenticated: bool,
    /// True while `authenticate()` is running (Chrome launched, polling
    /// for login). Drives the "Signing in…" button state in the settings UI.
    auth_in_progress: bool,
    /// Single-flight serialization for prompts. Gemini's web composer is
    /// single-threaded, so concurrent Zed agent requests must take this
    /// semaphore one at a time.
    request_lock: Arc<smol::lock::Semaphore>,
}

impl State {
    fn new() -> Self {
        Self {
            browser: Mutex::new(None),
            gemini_target_ws: Mutex::new(None),
            authenticated: false,
            auth_in_progress: false,
            request_lock: Arc::new(smol::lock::Semaphore::new(1)),
        }
    }

    fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

/// Chrome child handle + the DevTools port Chrome actually picked.
struct BrowserHandle {
    _child: util::process::Child,
    #[allow(dead_code)]
    debug_port: u16,
}

pub struct GeminiWebLanguageModelProvider {
    state: Entity<State>,
}

impl GeminiWebLanguageModelProvider {
    pub fn new(cx: &mut App) -> Self {
        let state = cx.new(|_| State::new());
        Self { state }
    }

    fn settings(cx: &App) -> GeminiWebSettings {
        AllLanguageModelSettings::get_global(cx)
            .gemini_web
            .clone()
    }

    fn profile_dir(cx: &App) -> Result<PathBuf> {
        if let Some(dir) = &Self::settings(cx).profile_dir {
            let expanded = shellexpand::tilde(dir).to_string();
            Ok(PathBuf::from(expanded))
        } else {
            Ok(paths::config_dir().join("gemini-web").join("profile"))
        }
    }

    fn chrome_binary(cx: &App) -> Result<String> {
        if let Some(p) = &Self::settings(cx).chrome_path {
            return Ok(p.clone());
        }
        detect_chrome()
    }
}

impl LanguageModelProviderState for GeminiWebLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for GeminiWebLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        GEMINI_WEB_PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        GEMINI_WEB_PROVIDER_NAME
    }

    fn icon(&self) -> language_model::IconOrSvg {
        language_model::IconOrSvg::Icon(IconName::AiGoogle)
    }

    fn default_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(Arc::new(GeminiWebModel {
            id: LanguageModelId::from("gemini-web-3".to_string()),
            name: LanguageModelName::from("Gemini Web 3".to_string()),
            state: self.state.clone(),
        }))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        // Single model for v1 — fast path uses the same one.
        self.default_model(_cx)
    }

    fn provided_models(&self, _cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        vec![Arc::new(GeminiWebModel {
            id: LanguageModelId::from("gemini-web-3".to_string()),
            name: LanguageModelName::from("Gemini Web 3".to_string()),
            state: self.state.clone(),
        })]
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read_with(cx, |s, _| s.is_authenticated())
    }

    fn authenticate(
        &self,
        cx: &mut App,
    ) -> Task<Result<(), language_model::AuthenticateError>> {
        let state = self.state.clone();
        let profile_dir = Self::profile_dir(cx).ok();
        let chrome = Self::chrome_binary(cx).ok();
        // Flip auth_in_progress up front so the settings UI shows
        // "Signing in…" immediately, before the spawn even starts.
        state.update(cx, |s, _| s.auth_in_progress = true);
        cx.spawn(async move |cx| {
            // Always clear auth_in_progress on exit (success or failure).
            let result = do_authenticate(state.clone(), profile_dir, chrome, cx).await;
            state.update(cx, |s, _| s.auth_in_progress = false);
            result
        })
    }

    fn authentication_error_message(&self) -> gpui::SharedString {
        "Gemini Web isn't signed in. Open Settings → AI → Gemini Web and click \
         Sign in to log into gemini.google.com once."
            .into()
    }

    fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView> {
        // Use Inline (not SubPage) so the Sign-in button appears directly on
        // the LLM Providers list page — same pattern as Copilot Chat. With
        // SubPage the user had to click "Configure" first to even see the
        // button, which is what caused the "no sign-in button" confusion.
        let is_authenticated = self.state.read_with(cx, |s, _| s.is_authenticated());
        let title = if is_authenticated { None } else { Some("Configure Gemini Web".into()) };
        let description = if is_authenticated {
            None
        } else {
            Some(InlineDescription::Text(
                "Drive gemini.google.com under your existing web subscription \
                 (Google AI Pro/Ultra, Workspace add-on) via a logged-in \
                 Chrome profile. No API key, no Vertex, no extra billing."
                    .into(),
            ))
        };
        let state = self.state.clone();
        Some(ProviderSettingsView::Inline(InlineProviderSettings {
            title,
            description,
            create_view: Arc::new(move |_window, cx| {
                cx.new(|cx| {
                    // Re-render whenever State changes so the button label
                    // flips immediately when auth_in_progress / authenticated flip.
                    let state_for_observe = state.clone();
                    cx.observe(&state_for_observe, |_, _, cx| cx.notify())
                        .detach();
                    ConfigurationView { state: state.clone() }
                })
                .into()
            }),
        }))
    }
}

pub struct GeminiWebModel {
    id: LanguageModelId,
    name: LanguageModelName,
    state: Entity<State>,
}

impl LanguageModel for GeminiWebModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        self.name.clone()
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        GEMINI_WEB_PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        GEMINI_WEB_PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        false
    }

    fn supports_images(&self) -> bool {
        false
    }

    fn supports_thinking(&self) -> bool {
        false
    }

    fn supports_tool_choice(&self, _choice: LanguageModelToolChoice) -> bool {
        false
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchema
    }

    fn telemetry_id(&self) -> String {
        "gemini-web/gemini-web-3".into()
    }

    fn max_token_count(&self) -> u64 {
        // Gemini 3 claims 1M context. The web composer is the bottleneck —
        // it'll reject far below this. Conservative placeholder.
        1_000_000
    }

    fn max_output_tokens(&self) -> Option<u64> {
        None
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        // Extract everything Send up front — `AsyncApp` is `!Send` and
        // cannot cross the `.boxed()` boundary. After this point we work
        // with Send values only.
        let prompt = build_prompt(&request);
        let prompt = truncate_to_byte_limit(&prompt, MAX_PROMPT_BYTES).to_string();
        let executor = cx.background_executor().clone();
        let ws_url = cx.update(|cx| {
            self.state.read_with(cx, |s, _| s.gemini_target_ws.lock().clone())
        });
        let request_lock = cx.update(|cx| self.state.read_with(cx, |s, _| s.request_lock.clone()));
        let timeout_seconds = cx.update(|cx| {
            AllLanguageModelSettings::get_global(cx)
                .gemini_web
                .response_timeout_seconds
        });

        async move {
            let ws_url = ws_url.ok_or_else(|| {
                LanguageModelCompletionError::Other(anyhow!(
                    "no Gemini target cached — click Sign in first to launch \
                     Chrome and navigate to gemini.google.com"
                ))
            })?;

            // Serialize against other concurrent prompts to the same
            // single-threaded Gemini composer.
            let _permit = request_lock.acquire().await;

            let cdp = Cdp::connect(&ws_url, executor.clone()).await?;
            let mut page = GeminiPage::open(&cdp).await?;
            if !page.is_logged_in().await.unwrap_or(false) {
                return Err(LanguageModelCompletionError::Other(anyhow!(
                    "Gemini session is not signed in. Open Settings → AI → \
                     Gemini Web and click Sign in."
                )));
            }
            let response = page
                .ask(&prompt, Duration::from_secs(timeout_seconds))
                .await
                .map_err(LanguageModelCompletionError::Other)?;

            // v1: emit the full response as a single Text event followed by
            // Stop. Streaming (MutationObserver → CDP binding → chunked
            // events) is a later increment.
            let stream = stream::iter(vec![
                Ok(LanguageModelCompletionEvent::StartMessage {
                    message_id: uuid::Uuid::now_v7().to_string(),
                }),
                Ok(LanguageModelCompletionEvent::Text(response)),
                Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)),
            ])
            .boxed();

            Ok(stream)
        }
        .boxed()
    }
}

/// Serialize a future onto the per-state request queue. Each queued task
/// waits for the previous one to complete before starting.
#[allow(dead_code)]
fn enqueue_request(
    _state: &Entity<State>,
    _cx: &AsyncApp,
    _task: impl Future<Output = ()> + Send + 'static,
) {
    // Replaced by the per-state `request_lock` semaphore acquired directly
    // in `stream_completion`. Kept briefly to avoid touching the trait
    // surface; will be removed in a follow-up.
}

/// Extracted body of `LanguageModelProvider::authenticate`. Wrapped by the
/// trait impl so the trait impl can flip `auth_in_progress` on entry/exit
/// without scattering that bookkeeping through the launch/poll loop.
///
/// `profile_dir` and `chrome` are pre-resolved `Option`s so the error path
/// for missing Chrome / unwritable profile dir reads cleanly inside the
/// spawned task rather than panicking in the trait impl.
async fn do_authenticate(
    state: Entity<State>,
    profile_dir: Option<PathBuf>,
    chrome: Option<String>,
    cx: &mut AsyncApp,
) -> Result<(), language_model::AuthenticateError> {
    let chrome = chrome.ok_or_else(|| {
        language_model::AuthenticateError::Other(anyhow!(
            "could not locate Chrome binary; set gemini_web.chrome_path"
        ))
    })?;
    let profile_dir = profile_dir.ok_or_else(|| {
        language_model::AuthenticateError::Other(anyhow!(
            "could not resolve profile dir"
        ))
    })?;

    fs::create_dir_all(&profile_dir).await.map_err(|e| {
        language_model::AuthenticateError::Other(anyhow!(
            "creating profile dir: {e}"
        ))
    })?;

    // Kill any existing instance first — same profile, two Chromes
    // is a hard error for Chrome.
    state.update(cx, |s, _| {
        if let Some(mut h) = s.browser.lock().take() {
            let _ = h._child.kill();
        }
    });

    let mut cmd = std::process::Command::new(&chrome);
    cmd.args(CHROME_FLAGS)
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(GEMINI_APP_URL);
    // Login flow always uses a visible window regardless of the
    // headless setting — the user needs to actually see the page.
    let child = util::process::Child::spawn(
        cmd,
        Stdio::null(),
        Stdio::piped(),
        Stdio::piped(),
    )
    .map_err(|e| {
        language_model::AuthenticateError::Other(anyhow!("spawn chrome: {e}"))
    })?;

    let port = wait_for_devtools_port(&profile_dir, Duration::from_secs(10))
        .await
        .map_err(|e| {
            language_model::AuthenticateError::Other(anyhow!(
                "DevToolsActivePort: {e}"
            ))
        })?;

    // Find the Gemini target that Chrome opened on launch and cache
    // its websocket URL so `stream_completion` can read it
    // synchronously without crossing the Send boundary.
    let ws_url = resolve_gemini_target_ws(port).await.map_err(|e| {
        language_model::AuthenticateError::Other(anyhow!(
            "resolving Gemini target: {e}"
        ))
    })?;
    state.update(cx, |s, _| {
        *s.browser.lock() = Some(BrowserHandle {
            _child: child,
            debug_port: port,
        });
        *s.gemini_target_ws.lock() = Some(ws_url);
    });

    // Poll the Gemini page for login completion. Typical time: 5-60s
    // depending on whether the user has 2FA, etc.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            state.update(cx, |s, _| s.authenticated = false);
            return Err(language_model::AuthenticateError::Other(anyhow!(
                "login timed out after 5 minutes"
            )));
        }
        gpui_platform::background_executor().timer(Duration::from_millis(1500)).await;
        let logged_in = check_login_via_http(port).await.unwrap_or_default();
        if logged_in {
            state.update(cx, |s, _| s.authenticated = true);
            return Ok(());
        }
    }
}

// ============================================================================
// chrome: launch, devtools port discovery, target listing
// ============================================================================

#[derive(Debug, Deserialize)]
struct TargetListing {
    #[serde(rename = "type")]
    kind: String,
    #[allow(dead_code)]
    id: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct BrowserVersion {
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

/// Discover Chrome's DevTools port by reading `DevToolsActivePort` in the
/// profile dir. Chrome writes it as `<port>\n<ws-path>\n` once it's listening.
async fn wait_for_devtools_port(profile_dir: &PathBuf, timeout: Duration) -> Result<u16> {
    let path = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(bytes) = fs::read(&path).await {
            let text = String::from_utf8_lossy(&bytes);
            if let Some(first_line) = text.lines().next() {
                if let Ok(port) = first_line.trim().parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        if Instant::now() > deadline {
            bail!("DevToolsActivePort never appeared in {:?}", profile_dir);
        }
        gpui_platform::background_executor().timer(Duration::from_millis(100)).await;
    }
}

/// Plain HTTP GET that parses the response body as JSON. smol + hand-rolled
/// request — we only hit localhost DevTools endpoints.
async fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let parsed: http_client::Url = url
        .parse()
        .with_context(|| format!("parsing {url}"))?;
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed.port().unwrap_or(80);
    let mut stream = TcpStream::connect(format!("{host}:{port}"))
        .await
        .with_context(|| format!("connecting {url}"))?;
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    use futures::AsyncWriteExt;
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    smol::io::AsyncReadExt::read_to_end(&mut stream, &mut buf).await?;
    // Split headers / body at the first blank line.
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("no header/body separator in response"))?;
    let body = &buf[split + 4..];
    serde_json::from_slice(body).with_context(|| "parsing JSON response")
}

/// Reach the page-level `Runtime.evaluate` via a transient CDP connection.
/// Used by the login-poll loop so we don't hold a long-lived connection
/// while the user is still typing their password.
async fn check_login_via_http(port: u16) -> Result<bool> {
    let targets: Vec<TargetListing> =
        http_get_json(&format!("http://127.0.0.1:{port}/json")).await?;
    let Some(target) = targets
        .into_iter()
        .find(|t| t.kind == "page" && t.url.starts_with(GEMINI_ORIGIN))
    else {
        return Ok(false);
    };
    let executor = gpui_platform::background_executor();
    let cdp = Cdp::connect(&target.web_socket_debugger_url, executor).await?;
    let value = cdp
        .evaluate(
            "(function() {
                for (const sel of [
                    'div.ql-editor[contenteditable=\"true\"]',
                    'rich-textarea div[contenteditable=\"true\"]',
                    'div[contenteditable=\"true\"][role=\"textbox\"]'
                ]) {
                    if (document.querySelector(sel)) return true;
                }
                return false;
            })()",
        )
        .await?;
    Ok(value
        .get("result")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

/// Find the websocket URL of an existing Gemini page target via HTTP
/// `/json`. Called once after Chrome launch (from `authenticate`) so
/// `stream_completion` can read the cached URL synchronously without
/// holding `AsyncApp` across an await.
async fn resolve_gemini_target_ws(port: u16) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let targets: Vec<TargetListing> =
            http_get_json(&format!("http://127.0.0.1:{port}/json")).await?;
        if let Some(t) = targets
            .into_iter()
            .find(|t| t.kind == "page" && t.url.starts_with(GEMINI_ORIGIN))
        {
            return Ok(t.web_socket_debugger_url);
        }
        if Instant::now() > deadline {
            bail!(
                "Chrome launched but no gemini.google.com page target \
                 appeared after 10s"
            );
        }
        gpui_platform::background_executor().timer(Duration::from_millis(200)).await;
    }
}

/// Locate the Chrome binary in platform-standard install locations.
fn detect_chrome() -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        for candidate in [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ] {
            if std::path::Path::new(candidate).exists() {
                return Ok(candidate.to_string());
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        for candidate in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ] {
            if std::path::Path::new(candidate).exists() {
                return Ok(candidate.to_string());
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
            let p = format!(
                "{}\\Google\\Chrome\\Application\\chrome.exe",
                local_appdata
            );
            if std::path::Path::new(&p).exists() {
                return Ok(p);
            }
        }
        if let Ok(program_files) = std::env::var("PROGRAMFILES") {
            let p = format!("{}\\Google\\Chrome\\Application\\chrome.exe", program_files);
            if std::path::Path::new(&p).exists() {
                return Ok(p);
            }
        }
    }
    bail!(
        "no Chrome/Chromium binary found. Set `gemini_web.chrome_path` in \
         your settings to point at one."
    );
}

// ============================================================================
// cdp: minimal Chrome DevTools Protocol client over smol + async-tungstenite
// ============================================================================

/// Minimal CDP client: speaks JSON-RPC over a single WebSocket. Covers the
/// four domains we need: `Target` (discover/create), `Runtime` (evaluate),
/// `Input` (insertText, dispatchKey), `Page` (navigate, lifecycle).
pub struct Cdp {
    writer: futures::lock::Mutex<WsSplit>,
    pending: Arc<Mutex<std::collections::HashMap<u64, futures::channel::oneshot::Sender<Value>>>>,
    next_id: Arc<Mutex<u64>>,
    reader_task: Option<Task<()>>,
}

struct WsSplit {
    ws: async_tungstenite::WebSocketSender<TcpStream>,
}

impl Cdp {
    /// Connect to a page- or browser-level CDP websocket URL.
    pub async fn connect(ws_url: &str, executor: gpui::BackgroundExecutor) -> Result<Self> {
        let stream = TcpStream::connect(ws_url_to_host_port(ws_url)?)
            .await
            .with_context(|| format!("connecting {ws_url}"))?;
        let (ws, _response) = async_tungstenite::client_async(ws_url, stream)
            .await
            .context("websocket handshake")?;
        let (writer, mut reader) = ws.split();
        let pending: Arc<
            Mutex<std::collections::HashMap<u64, futures::channel::oneshot::Sender<Value>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pending_for_reader = pending.clone();

        let reader_task = Some(executor.spawn(async move {
            use async_tungstenite::tungstenite::Message;
            while let Some(msg) = reader.next().await {
                let text = match msg {
                    Ok(Message::Text(t)) => t.to_string(),
                    Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => continue,
                };
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) {
                    if let Some(tx) = pending_for_reader.lock().remove(&id) {
                        let _ = tx.send(parsed);
                    }
                    continue;
                }
                // Otherwise: event. We don't subscribe to any in v1 — drop.
            }
            pending_for_reader.lock().clear();
        }));

        Ok(Self {
            writer: futures::lock::Mutex::new(WsSplit { ws: writer }),
            pending,
            next_id: Arc::new(Mutex::new(0)),
            reader_task,
        })
    }

    /// Send a CDP method and await its response. Errors if the reader task
    /// has died (Chrome closed the ws) or Chrome returned an `error` field.
    pub async fn call_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = {
            let mut next = self.next_id.lock();
            *next += 1;
            *next
        };
        let message = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = futures::channel::oneshot::channel();
        self.pending.lock().insert(id, tx);

        {
            let mut writer = self.writer.lock().await;
            writer
                .ws
                .send(async_tungstenite::tungstenite::Message::Text(
                    message.to_string().into(),
                ))
                .await
                .context("sending CDP message")?;
        }

        let response = rx
            .await
            .map_err(|_| anyhow!("CDP reader dropped response for id {id}"))?;
        if let Some(err) = response.get("error") {
            bail!("CDP {method} failed: {err}");
        }
        Ok(response)
    }

    /// Convenience: `Runtime.evaluate`. Returns the full response object;
    /// callers usually want `result.value`.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let response = self
            .call_method(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(exception) = response
            .get("result")
            .and_then(|r| r.get("exceptionDetails"))
        {
            bail!("JS exception: {exception}");
        }
        Ok(response)
    }

    /// `Input.insertText` — the reliable way to type into a contenteditable
    /// like Gemini's Quill composer. JS text assignment leaves the model
    /// empty; this goes through Chrome's real input pipeline.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        self.call_method("Input.insertText", json!({ "text": text }))
            .await?;
        Ok(())
    }

    /// `Input.dispatchKeyEvent` for a named key. We synthesize rawKeyDown +
    /// keyUp; Chrome's input pipeline fills in the rest.
    pub async fn press_key(&self, key: &str) -> Result<()> {
        for kind in ["rawKeyDown", "keyUp"] {
            self.call_method(
                "Input.dispatchKeyEvent",
                json!({
                    "type": kind,
                    "key": key,
                    "code": key_code_for(key),
                    "windowsVirtualKeyCode": vk_for(key),
                }),
            )
            .await?;
        }
        Ok(())
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        // Dropping the reader task cancels it (GPUI Task cancels on drop),
        // which closes the ws cleanly.
        if let Some(task) = self.reader_task.take() {
            drop(task);
        }
    }
}

fn key_code_for(key: &str) -> &'static str {
    match key {
        "Enter" => "Enter",
        "Backspace" => "Backspace",
        "Tab" => "Tab",
        "Escape" => "Escape",
        _ => "",
    }
}

fn vk_for(key: &str) -> u32 {
    match key {
        "Enter" => 0x0D,
        "Backspace" => 0x08,
        "Tab" => 0x09,
        "Escape" => 0x1B,
        _ => 0,
    }
}

/// Parse `ws://host:port/devtools/page/<id>` into the `host:port` smol's
/// `TcpStream::connect` wants.
fn ws_url_to_host_port(ws_url: &str) -> Result<String> {
    let parsed: http_client::Url = ws_url.parse()?;
    let host = parsed.host_str().context("ws url has no host")?;
    let port = parsed.port().context("ws url has no port")?;
    Ok(format!("{host}:{port}"))
}

// ============================================================================
// gemini_page: high-level "send a prompt, get a response" against the DOM
// ============================================================================

/// Drives the Gemini web app via a CDP session.
///
/// Gemini's markup is not a public API and class names change, so every
/// element is located through a list of candidate selectors rather than one
/// hardcoded string. `diagnose()` reports which candidates currently match
/// so a break can be identified and fixed quickly.
pub struct GeminiPage<'a> {
    cdp: &'a Cdp,
}

/// The prompt composer. Gemini renders a Quill rich-text editor, so the
/// `contenteditable` variants are the ones that normally hit. Tuned against
/// the real DOM in increment 2 (this file ships increment 1).
const COMPOSER_SELECTORS: &[&str] = &[
    "div.ql-editor[contenteditable=\"true\"]",
    "rich-textarea div[contenteditable=\"true\"]",
    "div[contenteditable=\"true\"][role=\"textbox\"]",
    "div[contenteditable=\"true\"]",
    "textarea[aria-label]",
];

/// Individual model response blocks, newest last.
const RESPONSE_SELECTORS: &[&str] = &[
    "model-response message-content",
    "message-content.model-response-text",
    "message-content",
    ".model-response-text",
    "div.markdown",
];

/// Poll cadence while a response streams in.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// Consecutive unchanged polls before a response is considered complete.
const STABLE_POLLS: usize = 3;

impl<'a> GeminiPage<'a> {
    pub async fn open(cdp: &'a Cdp) -> Result<Self> {
        // Ensure we're on the Gemini app. The URL is loaded at Chrome launch
        // for the login flow; for headless use we re-check here.
        let url = cdp
            .evaluate("location.href")
            .await?
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !url.starts_with(GEMINI_ORIGIN) {
            cdp.call_method("Page.navigate", json!({ "url": GEMINI_APP_URL }))
                .await?;
            gpui_platform::background_executor().timer(Duration::from_secs(2)).await;
        }
        Ok(Self { cdp })
    }

    /// `true` iff the page shows a composer (i.e. user is logged in).
    pub async fn is_logged_in(&mut self) -> Result<bool> {
        let value = self
            .cdp
            .evaluate(&format!(
                "(function() {{ for (const s of {selectors}) {{
                    if (document.querySelector(s)) return true;
                }} return false; }})()",
                selectors = js_array(COMPOSER_SELECTORS)
            ))
            .await?;
        Ok(value
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Send a prompt through the composer, wait for the response to reach
    /// text-stability, return the final text.
    pub async fn ask(&mut self, prompt: &str, timeout: Duration) -> Result<String> {
        // Focus the composer.
        let focus_js = format!(
            "(function() {{
                for (const s of {selectors}) {{
                    const el = document.querySelector(s);
                    if (el) {{ el.focus(); el.click(); return true; }}
                }}
                return false;
            }})()",
            selectors = js_array(COMPOSER_SELECTORS)
        );
        let focused = self
            .cdp
            .evaluate(&focus_js)
            .await?
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !focused {
            bail!(
                "could not find Gemini composer. The DOM selectors in \
                 gemini_web.rs::COMPOSER_SELECTORS need to be tuned against \
                 the real signed-in page."
            );
        }

        // Clear any leftover text via Ctrl/Cmd-A + Backspace, then type the
        // prompt via the real input pipeline.
        #[cfg(target_os = "macos")]
        let modifier_bit: u32 = 4; // Meta
        #[cfg(not(target_os = "macos"))]
        let modifier_bit: u32 = 2; // Control
        self.cdp
            .call_method(
                "Input.dispatchKeyEvent",
                json!({
                    "type": "rawKeyDown",
                    "key": "a",
                    "code": "KeyA",
                    "modifiers": modifier_bit,
                    "windowsVirtualKeyCode": 0x41,
                }),
            )
            .await?;
        self.cdp
            .call_method(
                "Input.dispatchKeyEvent",
                json!({ "type": "keyUp", "key": "a", "code": "KeyA" }),
            )
            .await?;
        self.cdp.press_key("Backspace").await?;

        self.cdp.insert_text(prompt).await?;
        // Small delay so the composer's framework notices the input and
        // enables the send button.
        gpui_platform::background_executor().timer(Duration::from_millis(300)).await;

        // Count response blocks BEFORE submit, so we know which one is the
        // new reply.
        let prior_count = self.response_block_count().await?;
        self.cdp.press_key("Enter").await?;

        // Poll for a new block, then wait for its text to stabilize.
        let deadline = Instant::now() + timeout;
        // First, wait for a new response block to appear.
        let new_block_idx;
        loop {
            if Instant::now() > deadline {
                bail!("timed out waiting for Gemini to start responding");
            }
            let count = self.response_block_count().await?;
            if count > prior_count {
                new_block_idx = count - 1;
                break;
            }
            gpui_platform::background_executor().timer(POLL_INTERVAL).await;
        }

        // Then wait for text stability on that block.
        let mut last_text = String::new();
        let mut stable = 0;
        loop {
            if Instant::now() > deadline {
                bail!("timed out waiting for Gemini response to stabilize");
            }
            gpui_platform::background_executor().timer(POLL_INTERVAL).await;
            let text = self.response_block_text(new_block_idx).await?;
            if text == last_text {
                stable += 1;
                if stable >= STABLE_POLLS {
                    return Ok(text);
                }
            } else {
                stable = 0;
                last_text = text;
            }
        }
    }

    async fn response_block_count(&mut self) -> Result<usize> {
        let value = self
            .cdp
            .evaluate(&format!(
                "(function() {{
                    for (const s of {selectors}) {{
                        const n = document.querySelectorAll(s).length;
                        if (n > 0) return n;
                    }}
                    return 0;
                }})()",
                selectors = js_array(RESPONSE_SELECTORS)
            ))
            .await?;
        Ok(value
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0))
    }

    async fn response_block_text(&mut self, idx: usize) -> Result<String> {
        let value = self
            .cdp
            .evaluate(&format!(
                "(function() {{
                    for (const s of {selectors}) {{
                        const els = document.querySelectorAll(s);
                        if (els.length > {idx}) {{
                            return els[{idx}].textContent || '';
                        }}
                    }}
                    return '';
                }})()",
                selectors = js_array(RESPONSE_SELECTORS),
                idx = idx,
            ))
            .await?;
        Ok(value
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

/// Render a Rust `&[&str]` as a JavaScript array literal of string literals.
/// Used to inline the candidate selector lists into the `evaluate` JS.
fn js_array(items: &[&str]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

// ============================================================================
// Prompt building
// ============================================================================

/// Flatten a `LanguageModelRequest` into a single text prompt suitable for
/// pasting into Gemini's composer. v1 ignores tools, images, and structured
/// system prompts — everything becomes text.
fn build_prompt(request: &LanguageModelRequest) -> String {
    let mut out = String::new();
    for msg in &request.messages {
        let role_label = match msg.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        let text = message_content_to_text(&msg.content);
        if !text.is_empty() {
            out.push_str(&format!("**{role_label}:**\n{text}\n\n"));
        }
    }
    // If there's no explicit framing, just send the last user message verbatim.
    if out.trim().is_empty() {
        for msg in request.messages.iter().rev() {
            if msg.role == Role::User {
                let text = message_content_to_text(&msg.content);
                if !text.is_empty() {
                    return text;
                }
            }
        }
    }
    out
}

/// Concatenate the text parts of a `Vec<MessageContent>`. Tool calls, tool
/// responses, and images are skipped in v1 — the web composer is text-only.
fn message_content_to_text(content: &[MessageContent]) -> String {
    let mut out = String::new();
    for part in content {
        if let MessageContent::Text(t) = part {
            out.push_str(t);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

// ============================================================================
// ConfigurationView: the provider's settings sub-page
// ============================================================================

struct ConfigurationView {
    state: Entity<State>,
}

impl ConfigurationView {
    fn trigger_sign_in(&mut self, cx: &mut Context<Self>) {
        // Reconstruct the provider from the shared state entity and call
        // its `authenticate` method. The trait impl flips `auth_in_progress`
        // up front and clears it on exit; the `observe` registered in
        // `settings_view` re-renders us when either flips.
        let provider = GeminiWebLanguageModelProvider { state: self.state.clone() };
        provider.authenticate(cx).detach_and_log_err(cx);
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let authenticated = state.is_authenticated();
        let signing_in = state.auth_in_progress;
        let button_label = if signing_in {
            "Signing in…"
        } else if authenticated {
            "Re-sign in"
        } else {
            "Sign in"
        };

        v_flex()
            .gap_2()
            .child(
                Label::new("Gemini Web")
                    .size(LabelSize::Large)
                    .weight(FontWeight::BOLD),
            )
            .child(
                Label::new(if authenticated {
                    "Signed in. Pick 'Gemini Web 3' from the model dropdown to use it."
                } else if signing_in {
                    "A Chrome window opened. Log into gemini.google.com there — \
                     this page updates automatically when login completes."
                } else {
                    "Not signed in. Click Sign in to launch Chrome and log into \
                     gemini.google.com once. Cookies persist in the profile so \
                     future sessions reuse the login."
                })
                .color(if authenticated {
                    Color::Success
                } else if signing_in {
                    Color::Accent
                } else {
                    Color::Warning
                }),
            )
            .child(
                Button::new("gemini-web-sign-in", button_label)
                    .full_width()
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .disabled(signing_in)
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.trigger_sign_in(cx);
                    })),
            )
            .child(
                Label::new(
                    "This provider drives a real Chrome window on a dedicated \
                     profile, so it uses your existing Gemini web subscription \
                     (Google AI Pro/Ultra, Workspace add-on). No API key, no \
                     Vertex, no extra billing.",
                )
                .color(Color::Muted)
                .size(LabelSize::Small),
            )
    }
}

// ============================================================================
// Small helpers shared across the file
// ============================================================================
