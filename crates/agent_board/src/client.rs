//! HTTP client for the agent-board Cloudflare Worker.
//!
//! All write requests are ed25519-signed (see [`crate::identity`]). The body
//! bytes that get signed are the *exact* bytes placed on the wire, so callers
//! must serialize once and reuse.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, http::request::Builder};
use serde::de::DeserializeOwned;

use crate::identity::DeviceIdentity;
use crate::types::{BoardMessage, PostMessageBody, PostStatusBody, RoomSnapshot, SCHEMA_VERSION};

const X_DEVICE_ID: &str = "X-Device-Id";
const X_TIMESTAMP: &str = "X-Timestamp";
const X_SIG: &str = "X-Sig";
const X_PUBKEY: &str = "X-Pubkey";

/// A signed handle to a remote agent-board worker.
pub struct BoardClient {
    http: Arc<dyn HttpClient>,
    base_url: String,
    identity: Arc<DeviceIdentity>,
}

impl BoardClient {
    pub fn new(http: Arc<dyn HttpClient>, mut base_url: String, identity: Arc<DeviceIdentity>) -> Self {
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self { http, base_url, identity }
    }

    #[allow(dead_code)]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[allow(dead_code)]
    pub fn device_id(&self) -> &str {
        self.identity.device_id()
    }

    /// Borrow the device identity (for the feeder, which takes it separately so
    /// it can be reused without holding the client).
    pub fn identity(&self) -> &Arc<DeviceIdentity> {
        &self.identity
    }

    /// `GET /v1/rooms/{room}` — read the current snapshot. Not authenticated.
    pub async fn get_room(&self, room: &str) -> Result<RoomSnapshot> {
        let uri = format!("{}/v1/rooms/{}", self.base_url, urlencoding(room));
        let response = self
            .http
            .get(&uri, AsyncBody::empty(), true)
            .await
            .context("agent_board GET room")?;
        let snapshot = read_json(response).await.context("decoding room snapshot")?;
        Ok(snapshot)
    }

    /// `POST /v1/rooms/{room}/status` — latest-wins device status.
    pub async fn post_status(&self, room: &str, body: PostStatusBody) -> Result<()> {
        let body_text = serde_json::to_string(&body).context("serializing status body")?;
        let uri = format!("{}/v1/rooms/{}/status", self.base_url, urlencoding(room));
        self.send_signed(&uri, body_text.into_bytes()).await?;
        Ok(())
    }

    /// `POST /v1/rooms/{room}/msg` — append a short notepad message.
    pub async fn post_message(&self, room: &str, body: PostMessageBody) -> Result<BoardMessage> {
        let body_text = serde_json::to_string(&body).context("serializing message body")?;
        let uri = format!("{}/v1/rooms/{}/msg", self.base_url, urlencoding(room));
        let response = self.send_signed(&uri, body_text.into_bytes()).await?;
        let message = read_json(response).await.context("decoding posted message")?;
        Ok(message)
    }

    /// `POST /v1/rooms/{room}/state` — append an agent state broadcast
    /// (Phase 2). The worker keeps only the last [`MAX_ROOM_STATES`] per room
    /// and truncates `state_text`/`meta` to [`MAX_STATE_TEXT_BYTES`].
    pub async fn post_state(
        &self,
        room: &str,
        body: crate::types::PostStateBody,
    ) -> Result<crate::types::AgentStateMessage> {
        let body_text = serde_json::to_string(&body).context("serializing state body")?;
        let uri = format!("{}/v1/rooms/{}/state", self.base_url, urlencoding(room));
        let response = self.send_signed(&uri, body_text.into_bytes()).await?;
        let state = read_json(response).await.context("decoding posted state")?;
        Ok(state)
    }

    /// `POST /v1/rooms/{room}/reply` — post a steering reply from the web UI
    /// (Plan 015). Signed with ed25519 so Zed-originated replies also work.
    /// The web UI uses the Google OAuth path instead.
    pub async fn post_reply(
        &self,
        room: &str,
        body: crate::types::WebReply,
    ) -> Result<()> {
        let body_text = serde_json::to_string(&body).context("serializing reply body")?;
        let uri = format!("{}/v1/rooms/{}/reply", self.base_url, urlencoding(room));
        self.send_signed(&uri, body_text.into_bytes()).await?;
        Ok(())
    }

    async fn send_signed(
        &self,
        uri: &str,
        body_bytes: Vec<u8>,
    ) -> Result<http_client::Response<AsyncBody>> {
        // The signature is over the request body text + "|" + timestamp. We must
        // send the same bytes we sign, so reconstruct the exact body text here.
        let timestamp = unix_secs();
        let body_text = String::from_utf8(body_bytes)
            .context("request body must be valid utf-8 to be signed")?;
        let signature = self.identity.sign(&body_text, timestamp)?;
        // Sanity-check the signing path once per process is overkill; the worker
        // will reject bad sigs. Skip here for latency.

        let request = Builder::new()
            .uri(uri)
            .method("POST")
            .header("Content-Type", "application/json")
            .header(X_DEVICE_ID, self.identity.device_id())
            .header(X_TIMESTAMP, timestamp.to_string())
            .header(X_SIG, signature)
            .header(X_PUBKEY, self.identity.public_key_b64())
            .body(AsyncBody::from(body_text.into_bytes()))?;
        self.http
            .send(request)
            .await
            .and_then(|response| {
                let status = response.status();
                if status.is_success() {
                    Ok(response)
                } else {
                    Err(anyhow!("agent_board request to {uri} failed: HTTP {status}"))
                }
            })
            .with_context(|| format!("agent_board POST {uri}"))
    }
}

/// Minimal percent-encoding for the room path segment. Only reserved/space
/// chars that would break the path are escaped; alphanumerics pass through.
fn urlencoding(room: &str) -> String {
    let mut out = String::with_capacity(room.len());
    for byte in room.bytes() {
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

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Collect a response body fully and parse it as JSON.
async fn read_json<T: DeserializeOwned>(response: http_client::Response<AsyncBody>) -> Result<T> {
    let mut bytes = Vec::new();
    response
        .into_body()
        .read_to_end(&mut bytes)
        .await
        .context("reading agent_board response body")?;
    serde_json::from_slice(&bytes).context("parsing agent_board json response")
}

// Re-export so callers can stamp schema versions if needed.
#[allow(dead_code)]
pub fn schema_version() -> u32 {
    SCHEMA_VERSION
}
