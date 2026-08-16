//! Claude subscription usage for the rolling 5-hour and weekly rate-limit windows.
//!
//! Zed drives Claude through the external `claude-acp` agent server, so the
//! `anthropic-ratelimit-unified-*` response headers that carry subscription
//! usage never reach this process. The numbers are polled instead from the
//! endpoint backing Claude Code's `/usage` command, authenticated with the
//! OAuth token Claude Code stores locally.

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::{AsyncBody, HttpClient, Request, StatusCode};
use parking_lot::Mutex;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_secs(300);

/// How much of one rate-limit window has been consumed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindow {
    /// Fraction of the window's allowance consumed, in `0.0..=1.0`.
    pub used: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeUsage {
    pub five_hour: Option<UsageWindow>,
    pub seven_day: Option<UsageWindow>,
    pub seven_day_opus: Option<UsageWindow>,
}

impl ClaudeUsage {
    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none() && self.seven_day_opus.is_none()
    }
}

pub struct ClaudeUsageStore {
    usage: Option<ClaudeUsage>,
    failing: bool,
    _poll: Task<()>,
}

struct GlobalClaudeUsageStore(Entity<ClaudeUsageStore>);

impl Global for GlobalClaudeUsageStore {}

impl ClaudeUsageStore {
    /// Polling starts on the first call, so installs that never open a Claude
    /// thread never read the keychain.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<GlobalClaudeUsageStore>() {
            return store.0.clone();
        }

        let store = cx.new(|cx| Self {
            usage: None,
            failing: false,
            // Polling reaches the keychain and the network, neither of which a
            // test should touch just by opening a Claude thread.
            _poll: if cfg!(test) {
                Task::ready(())
            } else {
                Self::poll(cx)
            },
        });
        cx.set_global(GlobalClaudeUsageStore(store.clone()));
        store
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalClaudeUsageStore>()
            .map(|store| store.0.clone())
    }

    pub fn usage(&self) -> Option<&ClaudeUsage> {
        self.usage.as_ref()
    }

    fn poll(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let token_cache = Arc::new(Mutex::new(None));
            loop {
                let http_client = cx.update(|cx| cx.http_client());
                let token_cache = token_cache.clone();

                let delay = match cx
                    .background_spawn(fetch_usage(http_client, token_cache))
                    .await
                {
                    Ok(usage) => {
                        let applied = this.update(cx, |this, cx| {
                            this.failing = false;
                            if this.usage.as_ref() != Some(&usage) {
                                this.usage = Some(usage);
                                cx.notify();
                            }
                        });
                        if applied.is_err() {
                            return;
                        }
                        POLL_INTERVAL
                    }
                    Err(error) => {
                        // The endpoint only answers for subscription logins, so
                        // a steady stream of failures is the normal state for
                        // API-key users. Report the first one, then stay quiet.
                        match this.update(cx, |this, _| std::mem::replace(&mut this.failing, true))
                        {
                            Ok(false) => log::warn!("could not read Claude usage: {error:#}"),
                            Ok(true) => log::debug!("could not read Claude usage: {error:#}"),
                            Err(_) => return,
                        }
                        RETRY_INTERVAL
                    }
                };

                cx.background_executor().timer(delay).await;
            }
        })
    }
}

/// Claude Code rotates its OAuth token periodically, so the token is cached
/// across polls and only re-read from the keychain once the server rejects it.
async fn fetch_usage(
    http_client: Arc<dyn HttpClient>,
    token_cache: Arc<Mutex<Option<String>>>,
) -> Result<ClaudeUsage> {
    let cached_token = token_cache.lock().clone();
    let was_cached = cached_token.is_some();
    let mut access_token = match cached_token {
        Some(token) => token,
        None => read_access_token().await?,
    };

    let (mut status, mut body) = request_usage(&http_client, &access_token).await?;
    if was_cached && matches!(status.as_u16(), 401 | 403) {
        access_token = read_access_token().await?;
        (status, body) = request_usage(&http_client, &access_token).await?;
    }

    // The body echoes account details, so keep it out of the error.
    if !status.is_success() {
        token_cache.lock().take();
        bail!("Claude usage request failed with status {status}");
    }

    let payload: UsagePayload =
        serde_json::from_slice(&body).context("parsing Claude usage response")?;
    let usage = ClaudeUsage::from(payload);

    // Rings would silently stay hidden if the response shape ever drifts, so
    // surface it as an error the log can show instead.
    if usage.is_empty() {
        bail!("Claude usage response contained no known rate-limit windows");
    }

    *token_cache.lock() = Some(access_token);
    Ok(usage)
}

async fn request_usage(
    http_client: &Arc<dyn HttpClient>,
    access_token: &str,
) -> Result<(StatusCode, Vec<u8>)> {
    let request = Request::get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA_HEADER)
        .header("Accept", "application/json")
        .body(AsyncBody::default())?;

    let mut response = http_client
        .send(request)
        .await
        .context("sending Claude usage request")?;
    let status = response.status();

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("reading Claude usage response")?;

    Ok((status, body))
}

async fn read_access_token() -> Result<String> {
    if let Some(token) = read_token_from_keychain().await? {
        return Ok(token);
    }

    let path = util::paths::home_dir()
        .join(".claude")
        .join(".credentials.json");
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("reading Claude credentials from {}", path.display()))?;
    parse_access_token(&contents)
}

#[cfg(target_os = "macos")]
async fn read_token_from_keychain() -> Result<Option<String>> {
    // Claude Code stores its credentials as a generic password, which gpui's
    // `read_credentials` (internet passwords, keyed by server) cannot reach.
    let mut command = util::command::new_command("/usr/bin/security");
    command.args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"]);
    let output = command
        .output()
        .await
        .context("running security(1) for Claude credentials")?;

    // A missing item and a denied access prompt both land here; fall through to
    // the credentials file rather than failing outright.
    if !output.status.success() {
        return Ok(None);
    }

    let contents = String::from_utf8(output.stdout).context("keychain item was not UTF-8")?;
    parse_access_token(&contents).map(Some)
}

#[cfg(not(target_os = "macos"))]
async fn read_token_from_keychain() -> Result<Option<String>> {
    Ok(None)
}

fn parse_access_token(contents: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct StoredCredentials {
        #[serde(rename = "claudeAiOauth")]
        claude_ai_oauth: Option<StoredOAuth>,
    }

    #[derive(Deserialize)]
    struct StoredOAuth {
        #[serde(rename = "accessToken")]
        access_token: String,
    }

    serde_json::from_str::<StoredCredentials>(contents.trim())
        .context("parsing Claude credentials")?
        .claude_ai_oauth
        .map(|oauth| oauth.access_token)
        .context("Claude credentials had no OAuth access token")
}

#[derive(Deserialize)]
struct UsagePayload {
    five_hour: Option<WindowPayload>,
    seven_day: Option<WindowPayload>,
    seven_day_opus: Option<WindowPayload>,
}

#[derive(Deserialize)]
struct WindowPayload {
    utilization: Option<f32>,
    resets_at: Option<ResetsAt>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResetsAt {
    UnixSeconds(i64),
    Rfc3339(String),
}

impl From<UsagePayload> for ClaudeUsage {
    fn from(payload: UsagePayload) -> Self {
        Self {
            five_hour: payload.five_hour.and_then(WindowPayload::into_window),
            seven_day: payload.seven_day.and_then(WindowPayload::into_window),
            seven_day_opus: payload.seven_day_opus.and_then(WindowPayload::into_window),
        }
    }
}

impl WindowPayload {
    fn into_window(self) -> Option<UsageWindow> {
        Some(UsageWindow {
            used: normalize_utilization(self.utilization?),
            resets_at: self.resets_at.and_then(ResetsAt::into_datetime),
        })
    }
}

impl ResetsAt {
    fn into_datetime(self) -> Option<DateTime<Utc>> {
        match self {
            ResetsAt::UnixSeconds(seconds) => DateTime::from_timestamp(seconds, 0),
            ResetsAt::Rfc3339(text) => DateTime::parse_from_rfc3339(&text)
                .ok()
                .map(|parsed| parsed.with_timezone(&Utc)),
        }
    }
}

/// The endpoint is undocumented and currently reports whole percents (`45` is
/// 45%). Values below 1 are read as an already-normalized fraction, so a change
/// to that shape degrades to the right number instead of a silent 0%.
fn normalize_utilization(raw: f32) -> f32 {
    let fraction = if raw >= 1.0 { raw / 100.0 } else { raw };
    fraction.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_utilization() {
        assert_eq!(normalize_utilization(0.0), 0.0);
        assert_eq!(normalize_utilization(1.0), 0.01);
        assert_eq!(normalize_utilization(45.0), 0.45);
        assert_eq!(normalize_utilization(100.0), 1.0);
        assert_eq!(normalize_utilization(120.0), 1.0);
        assert_eq!(normalize_utilization(0.45), 0.45);
        assert_eq!(normalize_utilization(-5.0), 0.0);
    }

    #[test]
    fn test_parse_usage_payload() {
        let usage: ClaudeUsage = serde_json::from_str::<UsagePayload>(
            r#"{
                "five_hour": {"utilization": 45, "resets_at": "2026-08-16T22:00:00Z"},
                "seven_day": {"utilization": 12, "resets_at": 1787000000},
                "seven_day_opus": {"utilization": 30},
                "unknown_window": {"utilization": 99}
            }"#,
        )
        .unwrap()
        .into();

        let five_hour = usage.five_hour.unwrap();
        assert_eq!(five_hour.used, 0.45);
        assert_eq!(
            five_hour.resets_at,
            Some("2026-08-16T22:00:00Z".parse::<DateTime<Utc>>().unwrap())
        );

        let seven_day = usage.seven_day.unwrap();
        assert_eq!(seven_day.used, 0.12);
        assert_eq!(seven_day.resets_at, DateTime::from_timestamp(1787000000, 0));

        let seven_day_opus = usage.seven_day_opus.unwrap();
        assert_eq!(seven_day_opus.used, 0.3);
        assert_eq!(seven_day_opus.resets_at, None);
    }

    #[test]
    fn test_parse_partial_usage_payload() {
        let usage: ClaudeUsage = serde_json::from_str::<UsagePayload>(r#"{"five_hour": {}}"#)
            .unwrap()
            .into();

        assert!(usage.is_empty());
    }

    #[test]
    fn test_parse_access_token() {
        let token = parse_access_token(
            r#"{"claudeAiOauth": {"accessToken": "sk-test", "expiresAt": 123}}"#,
        )
        .unwrap();
        assert_eq!(token, "sk-test");

        assert!(parse_access_token(r#"{"other": {}}"#).is_err());
        assert!(parse_access_token("not json").is_err());
    }
}
