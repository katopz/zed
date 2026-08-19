//! Real-time push client (Plan 015): connects to the worker's SSE endpoint
//! (`GET /v1/rooms/{room}/events?device={device_name}`) to receive pushed
//! replies without polling. When a reply targeting this device arrives via
//! the SSE stream, it's injected into `auto_prompt::peer_states` for the
//! agent_panel notification timer to pick up.
//!
//! The SSE client is optional (toggled by the 📡 icon on the panel). When
//! disabled, the system falls back to the existing 15s feeder poll.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::{AsyncBufReadExt, StreamExt};
use gpui::{BackgroundExecutor, Task};
use http_client::{AsyncBody, HttpClient};

use crate::identity::DeviceIdentity;

/// SSE connection state for the agent board. Held alive by the panel when
/// the 📡 toggle is on. Dropping this struct cancels the background task.
pub struct RealtimeClient {
    _task: Task<()>,
}

impl RealtimeClient {
    /// Start a background SSE connection to the worker. The task runs for
    /// the lifetime of the returned `RealtimeClient`. Auto-reconnects with
    /// exponential backoff on disconnection.
    pub fn start(
        http: Arc<dyn HttpClient>,
        base_url: String,
        room: String,
        identity: Arc<DeviceIdentity>,
        executor: BackgroundExecutor,
    ) -> Self {
        let device_name = identity.device_name().to_string();
        let timer_executor = executor.clone();
        let task = executor.spawn(async move {
            let mut backoff = Duration::from_secs(1);
            let max_backoff = Duration::from_secs(30);
            loop {
                match connect_and_drain(
                    &http,
                    &base_url,
                    &room,
                    &device_name,
                )
                .await
                {
                    Ok(()) => {
                        log::info!(
                            "[agent_board] SSE stream closed cleanly, reconnecting immediately"
                        );
                        backoff = Duration::from_secs(1);
                    }
                    Err(error) => {
                        log::warn!(
                            "[agent_board] SSE stream error: {error:#}; reconnecting in {:?}",
                            backoff
                        );
                        timer_executor.timer(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                    }
                }
            }
        });
        Self { _task: task }
    }
}

/// Connect to the SSE endpoint and drain events until the connection closes.
/// Each `data: {json}` line is parsed; if it's a reply targeting this device,
/// it's injected into `auto_prompt::peer_states`.
async fn connect_and_drain(
    http: &Arc<dyn HttpClient>,
    base_url: &str,
    room: &str,
    device_name: &str,
) -> Result<()> {
    let uri = format!(
        "{}/v1/rooms/{}/events?device={}",
        base_url,
        urlencoding(room),
        urlencoding(device_name),
    );

    let response = http
        .get(&uri, AsyncBody::empty(), true)
        .await
        .context("agent_board SSE connect")?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("SSE endpoint returned HTTP {status}");
    }

    let body = response.into_body();
    let reader = futures::io::BufReader::new(body);
    let mut lines = reader.lines();

    while let Some(line_result) = lines.next().await {
        let line = line_result.context("reading SSE line")?;

        // SSE events arrive as `data: {json}\n\n`. Lines starting with `:`
        // are keepalive comments — skip them. Empty lines are event
        // separators — skip them.
        if line.is_empty() || line.starts_with(':') {
            continue;
        }

        // Strip the `data: ` prefix.
        let payload = if let Some(stripped) = line.strip_prefix("data: ") {
            stripped
        } else if let Some(stripped) = line.strip_prefix("data:") {
            stripped
        } else {
            // Not an SSE data line — could be an event type or id. Skip.
            continue;
        };

        // Try to parse the event as a board object. Two shapes matter here:
        // (a) a reply push targeting this device (worker broadcasts the raw
        //     reply JSON), and (b) a feed message (Plan 024) — scanned for
        // `@device:sess4` mentions so delivery is instant when 📡 is on,
        // instead of waiting out the 15s poll.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
            if value.get("target_device").is_some() {
                // Reply push: inject only when it targets this device.
                if value.get("target_device").and_then(|v| v.as_str()) == Some(device_name) {
                    let prefix = value
                        .get("target_session_prefix")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = value
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    auto_prompt::peer_states::inject_web_reply(prefix, text);
                }
            } else if let Some(message) = parse_board_message(&value) {
                crate::mentions::handle_board_message(&message, device_name);
            }
        }
    }

    Ok(())
}

/// Best-effort `BoardMessage` extraction from an SSE payload: has `text` +
/// `ts` + `device_name`, and is not a status (no `scopes`) or state (no
/// `state_text`) broadcast.
fn parse_board_message(value: &serde_json::Value) -> Option<crate::types::BoardMessage> {
    if value.get("scopes").is_some() || value.get("state_text").is_some() {
        return None;
    }
    Some(crate::types::BoardMessage {
        v: 1,
        device_id: value
            .get("device_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        device_name: value
            .get("device_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        sender: value
            .get("sender")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        text: value.get("text")?.as_str()?.to_string(),
        ts: value.get("ts")?.as_i64()?,
    })
}

/// Minimal percent-encoding for URL path/query segments. Reuses the same
/// logic as `client::urlencoding`.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{byte:02X}"));
            }
        }
    }
    out
}
