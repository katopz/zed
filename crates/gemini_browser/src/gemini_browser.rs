//! Asks Gemini by driving the Gemini web app in a Chrome instance Zed launches.
//!
//! Chrome runs against a dedicated profile directory under Zed's data dir, so
//! the user signs in to Gemini once in that window and the session cookie is
//! reused afterwards. Chrome only honors `--remote-debugging-port` for the
//! process that owns its profile, which is why a dedicated profile is required
//! rather than the user's normal one: launching against an already-running
//! profile would just hand the URL to that instance and exit.
//!
//! Disabled by default; set `gemini_browser.enabled` to turn it on.

mod cdp;
mod gemini_page;

pub use gemini_page::{GEMINI_APP_URL, truncate_at_char_boundary};

use anyhow::{Context as _, Result, anyhow};
use futures::lock::Mutex;
use gpui::{App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Task};
use settings::{RegisterSetting, Settings, SettingsContent};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::cdp::{Chrome, find_chrome};
use crate::gemini_page::GeminiPage;

#[derive(Debug, Clone, RegisterSetting)]
pub struct GeminiBrowserSettings {
    pub enabled: bool,
    pub chrome_path: Option<PathBuf>,
    pub headless: bool,
    pub response_timeout: Duration,
}

impl Settings for GeminiBrowserSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        // Unwrapping matches the `Settings` contract: these keys are present in
        // default.json, and a missing default should fail loudly rather than
        // silently substituting a different value.
        let content = content.gemini_browser.as_ref().unwrap();
        Self {
            enabled: content.enabled.unwrap(),
            chrome_path: content.chrome_path.as_ref().map(PathBuf::from),
            headless: content.headless.unwrap(),
            response_timeout: Duration::from_secs(content.response_timeout_seconds.unwrap()),
        }
    }
}

struct GlobalGeminiBrowser(Entity<GeminiBrowser>);

impl Global for GlobalGeminiBrowser {}

pub fn init(cx: &mut App) {
    let browser = cx.new(|_| GeminiBrowser::default());
    cx.set_global(GlobalGeminiBrowser(browser));
}

/// A single Chrome instance and the Gemini tab attached to it.
struct Session {
    // Chrome must outlive the page: dropping this kills the browser process.
    _chrome: Chrome,
    page: GeminiPage,
}

impl Session {
    async fn launch(
        settings: &GeminiBrowserSettings,
        executor: &BackgroundExecutor,
    ) -> Result<Self> {
        let chrome_path = settings
            .chrome_path
            .clone()
            .or_else(find_chrome)
            .context(
                "Could not find a Chrome or Chromium binary. Set `gemini_browser.chrome_path` \
                 to the browser executable.",
            )?;

        let user_data_dir = paths::data_dir().join("gemini_browser").join("profile");
        let chrome =
            Chrome::launch(&chrome_path, &user_data_dir, settings.headless, executor).await?;
        let cdp = chrome.connect(executor.clone()).await?;
        let page = GeminiPage::open(cdp).await?;
        Ok(Self {
            _chrome: chrome,
            page,
        })
    }
}

#[derive(Default)]
pub struct GeminiBrowser {
    // An async mutex, because the guard is held across the awaits that drive
    // Chrome. Serializing asks also matches the single shared Gemini tab: two
    // concurrent prompts would interleave in the same composer.
    session: Arc<Mutex<Option<Session>>>,
}

impl GeminiBrowser {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalGeminiBrowser>()
            .map(|global| global.0.clone())
    }

    /// Sends `prompt` to Gemini and resolves with the response text.
    ///
    /// Launches Chrome on first use and reuses it afterwards.
    pub fn ask(&self, prompt: String, cx: &mut Context<Self>) -> Task<Result<String>> {
        self.run(Operation::Ask(prompt), cx)
    }

    /// Reports which of Gemini's candidate selectors currently match, for
    /// diagnosing a broken prompt box or response reader.
    pub fn diagnose(&self, cx: &mut Context<Self>) -> Task<Result<String>> {
        self.run(Operation::Diagnose, cx)
    }

    /// Evaluates arbitrary JavaScript in the Gemini tab and returns its value.
    pub fn evaluate(&self, expression: String, cx: &mut Context<Self>) -> Task<Result<String>> {
        self.run(Operation::Evaluate(expression), cx)
    }

    /// Runs `operation` against a live Gemini page, launching or replacing the
    /// Chrome session as needed.
    fn run(&self, operation: Operation, cx: &mut Context<Self>) -> Task<Result<String>> {
        let settings = GeminiBrowserSettings::get_global(cx).clone();
        if !settings.enabled {
            return Task::ready(Err(anyhow!(
                "The browser-driven Gemini integration is off. Set `gemini_browser.enabled` to \
                 true in your settings to enable it."
            )));
        }

        let session = self.session.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut session = session.lock().await;

            // A session whose Chrome the user has since closed would fail every
            // future call, so probe it and relaunch rather than wedging.
            if let Some(existing) = session.as_mut()
                && let Err(error) = existing.page.evaluate("1").await
            {
                log::info!("Gemini browser session is no longer responsive ({error:#}); relaunching");
                *session = None;
            }

            if session.is_none() {
                *session = Some(Session::launch(&settings, &executor).await?);
            }
            let active = session
                .as_mut()
                .context("Gemini browser session unavailable")?;
            let page = &mut active.page;
            match operation {
                Operation::Ask(prompt) => page.ask(&prompt, settings.response_timeout).await,
                Operation::Diagnose => page.diagnose().await,
                Operation::Evaluate(expression) => {
                    let value = page.evaluate(&expression).await?;
                    Ok(match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    })
                }
            }
        })
    }
}

enum Operation {
    Ask(String),
    Diagnose,
    Evaluate(String),
}
