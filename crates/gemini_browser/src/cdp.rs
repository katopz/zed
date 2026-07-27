//! Minimal Chrome DevTools Protocol client.
//!
//! CDP is JSON-RPC over a plain localhost WebSocket, and the only domains this
//! needs are `Target`, `Runtime`, `Input`, and `Page`. That is small enough to
//! speak directly over Zed's existing smol/`async-tungstenite` stack, which
//! avoids taking on a full CDP crate (`chromiumoxide`) whose tokio runtime
//! would then have to be hosted inside the Zed process alongside smol.
//!
//! Calls are strictly sequential: each `call` writes a request and then reads
//! frames until the response with the matching id arrives, discarding protocol
//! events in between. Nothing here subscribes to events, so dropping them is
//! intentional rather than lossy.

use anyhow::{Context as _, Result, bail};
use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::Message;
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::BackgroundExecutor;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long to wait for Chrome to write its `DevToolsActivePort` file. A cold
/// first launch has to create the whole profile directory, so this is generous.
const CHROME_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-command ceiling. Individual CDP commands are fast; anything this slow
/// means Chrome is wedged, and failing beats hanging the caller forever.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// Locates a Chrome/Chromium binary, preferring stable Chrome.
///
/// Returns `None` rather than erroring so the caller can surface a single
/// actionable message that also mentions the override setting.
pub fn find_chrome() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    const CANDIDATES: &[&str] = &[
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];

    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

/// A Chrome process launched with remote debugging enabled.
///
/// Dropping this kills the process group (see [`util::process::Child`]), so the
/// owner must keep it alive for as long as the browser should stay open.
pub struct Chrome {
    /// Held to tie Chrome's lifetime to this value; never read directly.
    ///
    /// `None` when attaching to a Chrome that was already running, since that
    /// process belongs to whoever started it and must not be killed here.
    _child: Option<util::process::Child>,
    port: u16,
    browser_ws_path: String,
}

impl Chrome {
    pub async fn launch(
        chrome_path: &Path,
        user_data_dir: &Path,
        headless: bool,
        executor: &BackgroundExecutor,
    ) -> Result<Self> {
        std::fs::create_dir_all(user_data_dir).with_context(|| {
            format!(
                "failed to create Chrome profile directory {}",
                user_data_dir.display()
            )
        })?;

        let port_file = user_data_dir.join("DevToolsActivePort");

        // Chrome only honors `--remote-debugging-port` for the process that owns
        // the profile. If an instance is already running against this profile, a
        // second launch just forwards its arguments to that instance and exits
        // without writing a new port file, so waiting for one would stall until
        // the timeout. Reuse the live endpoint instead.
        if let Some((port, browser_ws_path)) = read_endpoint_file(&port_file)
            && endpoint_is_live(port, executor).await
        {
            log::info!("attaching to the Chrome already running on the Gemini profile");
            return Ok(Self {
                _child: None,
                port,
                browser_ws_path,
            });
        }

        // A leftover port file from a previous run would otherwise be read as
        // this run's endpoint, pointing the client at a dead port.
        if port_file.exists() {
            std::fs::remove_file(&port_file).with_context(|| {
                format!("failed to remove stale port file {}", port_file.display())
            })?;
        }

        let mut command = std::process::Command::new(chrome_path);
        command
            // Port 0 lets Chrome pick a free port and report it in the profile
            // directory, so concurrent instances never collide on a fixed port.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            // Chrome only honors --remote-debugging-port for the process that
            // owns the profile. Without a dedicated --user-data-dir, launching
            // while the user's normal Chrome is running would just hand the URL
            // to that existing instance and exit, leaving no debuggable target.
            .arg("--disable-features=Translate");
        if headless {
            command.arg("--headless=new");
        }
        // Deliberately not passing automation-concealing flags such as
        // `--disable-blink-features=AutomationControlled`, and deliberately
        // leaving Chrome's "controlled by automated test software" infobar in
        // place. This drives the user's own logged-in session; it does not need
        // to disguise itself as human traffic, and shouldn't.

        let child = util::process::Child::spawn(command, Stdio::null(), Stdio::null(), Stdio::null())
            .context("failed to launch Chrome")?;

        let (port, browser_ws_path) = read_devtools_endpoint(&port_file, executor).await?;
        Ok(Self {
            _child: Some(child),
            port,
            browser_ws_path,
        })
    }

    pub async fn connect(&self, executor: BackgroundExecutor) -> Result<Cdp> {
        let stream = smol::net::TcpStream::connect(("127.0.0.1", self.port))
            .await
            .with_context(|| format!("failed to connect to Chrome DevTools on port {}", self.port))?;
        let url = format!("ws://127.0.0.1:{}{}", self.port, self.browser_ws_path);
        let (websocket, _response) = async_tungstenite::client_async(&url, stream)
            .await
            .with_context(|| format!("failed to open a DevTools WebSocket at {url}"))?;
        Ok(Cdp {
            websocket,
            next_id: 0,
            page_session: None,
            executor,
        })
    }
}

/// Parses Chrome's `DevToolsActivePort` file.
///
/// The first line is the port and the second is the browser WebSocket path. Both
/// are required, since the file may be observed mid-write.
fn read_endpoint_file(port_file: &Path) -> Option<(u16, String)> {
    let contents = std::fs::read_to_string(port_file).ok()?;
    let mut lines = contents.lines();
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim();
    if path.is_empty() {
        return None;
    }
    Some((port, path.to_string()))
}

/// Probes whether something is still accepting connections on `port`.
///
/// Used to tell a live Chrome from a stale port file left behind by one that
/// exited without cleaning up.
async fn endpoint_is_live(port: u16, executor: &BackgroundExecutor) -> bool {
    let connect = smol::net::TcpStream::connect(("127.0.0.1", port));
    let timeout = executor.timer(Duration::from_secs(2));
    select_biased! {
        stream = connect.fuse() => stream.is_ok(),
        _ = timeout.fuse() => false,
    }
}

/// Waits for Chrome to publish its debugging endpoint.
async fn read_devtools_endpoint(
    port_file: &Path,
    executor: &BackgroundExecutor,
) -> Result<(u16, String)> {
    let poll_interval = Duration::from_millis(100);
    let attempts = (CHROME_STARTUP_TIMEOUT.as_millis() / poll_interval.as_millis()).max(1);

    for _ in 0..attempts {
        if let Some(endpoint) = read_endpoint_file(port_file) {
            return Ok(endpoint);
        }
        executor.timer(poll_interval).await;
    }

    bail!(
        "Chrome did not report a DevTools port within {}s. Check that the configured Chrome \
         binary starts successfully and that the profile directory is writable.",
        CHROME_STARTUP_TIMEOUT.as_secs()
    )
}

pub struct Cdp {
    websocket: WebSocketStream<smol::net::TcpStream>,
    next_id: u64,
    page_session: Option<String>,
    executor: BackgroundExecutor,
}

impl Cdp {
    async fn call(&mut self, method: &str, params: Value, session_id: Option<&str>) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;

        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session_id) = session_id {
            request["sessionId"] = json!(session_id);
        }
        self.websocket
            .send(Message::Text(request.to_string().into()))
            .await
            .with_context(|| format!("failed to send CDP command {method}"))?;

        // `read_response` takes the socket rather than `&mut self` so the
        // timeout task can be created from `self.executor` without a second
        // mutable borrow.
        let timeout = self.executor.timer(COMMAND_TIMEOUT);
        select_biased! {
            response = Self::read_response(&mut self.websocket, id).fuse() => {
                response.with_context(|| format!("CDP command {method} failed"))
            }
            _ = timeout.fuse() => {
                bail!("CDP command {method} timed out after {}s", COMMAND_TIMEOUT.as_secs())
            }
        }
    }

    async fn read_response(
        websocket: &mut WebSocketStream<smol::net::TcpStream>,
        id: u64,
    ) -> Result<Value> {
        loop {
            let Some(message) = websocket.next().await else {
                bail!("DevTools connection closed while awaiting a response");
            };
            let text = match message.context("DevTools WebSocket error")? {
                Message::Text(text) => text,
                Message::Close(_) => bail!("Chrome closed the DevTools connection"),
                // Ping/pong are handled by the WebSocket layer; binary frames
                // are not used by CDP.
                _ => continue,
            };

            let message: Value =
                serde_json::from_str(text.as_str()).context("malformed CDP message")?;
            // Events carry `method` and no `id`, and responses to other
            // in-flight ids belong to nobody here since calls are sequential.
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("{message}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Attaches to a page whose URL starts with `url_prefix`, creating a new tab
    /// if no such page is open.
    ///
    /// Reusing an existing tab means repeated asks land in the tab the user is
    /// already looking at, instead of piling up new ones.
    pub async fn attach_to_page(&mut self, url_prefix: &str, create_url: &str) -> Result<()> {
        let targets = self.call("Target.getTargets", json!({}), None).await?;
        let existing = targets["targetInfos"]
            .as_array()
            .and_then(|infos| {
                infos.iter().find(|info| {
                    info["type"] == "page"
                        && info["url"]
                            .as_str()
                            .is_some_and(|url| url.starts_with(url_prefix))
                })
            })
            .and_then(|info| info["targetId"].as_str().map(str::to_string));

        let target_id = match existing {
            Some(target_id) => target_id,
            None => {
                let created = self
                    .call("Target.createTarget", json!({ "url": create_url }), None)
                    .await?;
                created["targetId"]
                    .as_str()
                    .context("Target.createTarget did not return a targetId")?
                    .to_string()
            }
        };

        // `flatten` multiplexes the page session over this same socket, so no
        // second WebSocket connection is needed.
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let session_id = attached["sessionId"]
            .as_str()
            .context("Target.attachToTarget did not return a sessionId")?
            .to_string();
        self.page_session = Some(session_id);
        Ok(())
    }

    fn page_session(&self) -> Result<String> {
        self.page_session
            .clone()
            .context("no Gemini page is attached")
    }

    /// Evaluates JavaScript in the attached page and returns its value.
    ///
    /// Promises are awaited, so `expression` may be an async IIFE. This is
    /// public so callers can inspect and debug the live DOM directly.
    pub async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let session_id = self.page_session()?;
        let result = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    // Some controls refuse to act outside a user-gesture
                    // context; this makes evaluated clicks behave like real ones.
                    "userGesture": true,
                }),
                Some(&session_id),
            )
            .await?;

        if let Some(details) = result.get("exceptionDetails") {
            let description = details["exception"]["description"]
                .as_str()
                .or_else(|| details["text"].as_str())
                .unwrap_or("unknown JavaScript exception");
            bail!("{description}");
        }
        Ok(result["result"]["value"].clone())
    }

    /// Inserts text into the focused element through Chrome's input pipeline.
    ///
    /// This is used instead of assigning to the element's text because Gemini's
    /// composer is a rich-text editor whose framework only observes real input
    /// events; a direct property write would leave its internal model empty and
    /// the send button disabled.
    pub async fn insert_text(&mut self, text: &str) -> Result<()> {
        let session_id = self.page_session()?;
        self.call(
            "Input.insertText",
            json!({ "text": text }),
            Some(&session_id),
        )
        .await?;
        Ok(())
    }

    pub async fn press_enter(&mut self) -> Result<()> {
        let session_id = self.page_session()?;
        for event_type in ["keyDown", "keyUp"] {
            self.call(
                "Input.dispatchKeyEvent",
                json!({
                    "type": event_type,
                    "key": "Enter",
                    "code": "Enter",
                    "windowsVirtualKeyCode": 13,
                    "nativeVirtualKeyCode": 13,
                    "text": "\r",
                }),
                Some(&session_id),
            )
            .await?;
        }
        Ok(())
    }

    pub fn executor(&self) -> &BackgroundExecutor {
        &self.executor
    }
}
