//! Serves Zed's native agent as an ACP agent over stdio, without any of the
//! editor UI crates.
//!
//! Two binaries share this crate:
//! - `eval-cli --acp` (local, `.issues/036`)
//! - `remote_server agent-acp` (the host side of Work Remote, plan 042 —
//!   same shape as the embedded `agentd` subcommand)
//!
//! The dependency budget is deliberate: [`boot_app`] initializes exactly the
//! state the serve path reads (settings, language models + providers, the
//! extension/LSP hosts, a live `SettingsStore`) and none of the UI tree —
//! no `agent_ui`, no `terminal_view`, no `workspace`. The agent's terminal
//! tool runs on `terminal`-crate entities through `AcpThread::create_terminal`
//! (`HeadlessTerminal(true)` makes it a plain piped subprocess), so
//! `terminal_view::init` contributes nothing here. See plan 042 task 1 for
//! the full map.
pub mod acp_server;
pub mod model_config;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser as _;
use client::{Client, ProxySettings, RefreshLlmTokenListener, UserStore};
use db::AppDatabase;
use extension::ExtensionHostProxy;
use fs::RealFs;
use gpui::http_client::read_proxy_from_env;
use gpui::{App, AppContext as _, AsyncApp};
use gpui_tokio::Tokio;
use language::LanguageRegistry;
use language_extension::LspAccess;
use language_model::LanguageModelRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::project_settings::ProjectSettings;
use release_channel::{AppCommitSha, AppVersion};
use reqwest_client::ReqwestClient;
use settings::{Settings, SettingsStore};
use util::ResultExt as _;

/// The state slice of [`boot_app`] that the serve path needs. Same shape the
/// eval CLI's headless boot produces; `project::Project::local` consumes it.
pub struct NativeAgentAppState {
    pub languages: Arc<LanguageRegistry>,
    pub client: Arc<Client>,
    pub user_store: gpui::Entity<UserStore>,
    pub fs: Arc<dyn fs::Fs>,
    pub node_runtime: NodeRuntime,
}

/// Initializes the minimal headless app state the native-agent ACP server
/// reads at runtime. Deliberately does NOT run `agent_ui::init`,
/// `terminal_view::init`, the thread/prompt metadata stores, or the
/// skills-updated hook (its only reader is `try_global`-guarded, and the
/// shipped GUI closure would panic headless — it dereferences
/// `workspace::AppState`).
pub fn boot_app(cx: &mut App) -> Arc<NativeAgentAppState> {
    let app_commit_sha = option_env!("ZED_COMMIT_SHA").map(|s| AppCommitSha::new(s.to_owned()));

    let app_version = AppVersion::load(
        env!("ZED_PKG_VERSION"),
        option_env!("ZED_BUILD_ID"),
        app_commit_sha,
    );

    release_channel::init(app_version.clone(), cx);
    gpui_tokio::init(cx);

    let settings_store = SettingsStore::new(cx, &settings::default_settings());
    cx.set_global(settings_store);
    theme_settings::init(theme::LoadThemes::JustBase, cx);

    let user_agent = format!(
        "Zed Agent ACP Server/{} ({}; {})",
        app_version,
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let proxy_str = ProxySettings::get_global(cx).proxy.to_owned();
    let proxy_url = proxy_str
        .as_ref()
        .and_then(|input| input.parse().ok())
        .or_else(read_proxy_from_env);
    let http = {
        let _guard = Tokio::handle(cx).enter();
        ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)
            .expect("could not start HTTP client")
    };
    cx.set_http_client(Arc::new(http));

    let client = Client::production(cx);
    cx.set_http_client(client.http_client());

    let app_db = AppDatabase::new();
    cx.set_global(app_db);

    let git_binary_path = None;
    let fs = RealFs::new(git_binary_path, cx.background_executor().clone());
    <dyn fs::Fs>::set_global(fs.clone(), cx);

    let mut languages = LanguageRegistry::new(cx.background_executor().clone());
    languages.set_language_server_download_dir(paths::languages_dir().clone());
    let languages = Arc::new(languages);

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));

    extension::init(cx);

    let (mut node_options_tx, node_options_rx) = watch::channel(None);
    cx.observe_global::<SettingsStore>(move |cx| {
        let settings = &ProjectSettings::get_global(cx).node;
        let options = NodeBinaryOptions {
            allow_path_lookup: !settings.ignore_system_version,
            allow_binary_download: true,
            use_paths: settings.path.as_ref().map(|node_path| {
                let node_path = PathBuf::from(shellexpand::tilde(node_path).as_ref());
                let npm_path = settings
                    .npm_path
                    .as_ref()
                    .map(|path| PathBuf::from(shellexpand::tilde(&path).as_ref()));
                (
                    node_path.clone(),
                    npm_path.unwrap_or_else(|| {
                        let base_path = PathBuf::new();
                        node_path.parent().unwrap_or(&base_path).join("npm")
                    }),
                )
            }),
        };
        node_options_tx.send(Some(options)).log_err();
    })
    .detach();
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);

    let extension_host_proxy = ExtensionHostProxy::global(cx);
    debug_adapter_extension::init(extension_host_proxy.clone(), cx);
    language_extension::init(LspAccess::Noop, extension_host_proxy, languages.clone());
    language_model::init(cx);
    RefreshLlmTokenListener::register(client.clone(), user_store.clone(), cx);
    language_models::init(user_store.clone(), client.clone(), cx);
    languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

    // The user-global `~/.config/zed/AGENTS.md` rides prompts through this
    // global (`agent/src/thread.rs`); read errors log instead of notifying —
    // there is no UI here to notify with.
    agent_settings::init_user_agents_md(fs.clone(), cx, |state, _cx| {
        if let agent_settings::UserAgentsMdState::Error(message) = state {
            log::error!("Failed to load user AGENTS.md: {message}");
        }
    });

    // The server runs with no controlling TTY, so PTY allocation fails with
    // `ENOTTY`. Tell the agent to run its terminal commands without a PTY
    // (and non-interactively) instead.
    cx.set_global(acp_thread::HeadlessTerminal(true));

    Arc::new(NativeAgentAppState {
        languages,
        client,
        user_store,
        fs,
        node_runtime,
    })
}

/// `remote_server agent-acp` — the subcommand name. Dispatched before clap in
/// `remote_server/src/main.rs`, the same way as the embedded `agentd`.
pub const SUBCOMMAND: &str = "agent-acp";

/// Arguments of the [`SUBCOMMAND`] entry point.
#[derive(clap::Parser, Debug)]
#[command(name = SUBCOMMAND, about = "Serve Zed's native agent over ACP stdio")]
pub struct ServerArgs {
    /// Language model to serve, in `provider/model` format.
    #[arg(long, default_value = "anthropic/claude-sonnet-4-6-latest")]
    pub model: String,

    /// Enable or disable extended thinking. Defaults to model auto-detection
    /// if omitted.
    #[arg(long)]
    pub thinking: Option<bool>,

    /// Reasoning effort level for models that support thinking (low, medium,
    /// high). Defaults to "high" for thinking-capable models.
    #[arg(long)]
    pub reasoning_effort: Option<String>,

    /// Disable staff mode (staff mode is enabled by default).
    #[arg(long)]
    pub no_staff: bool,
}

/// Entry point for the [`SUBCOMMAND`] binary mode: boots headless gpui and
/// serves until the client closes stdin. Exits the process with 0 on a clean
/// client hangup, 1 on an error.
pub fn main_from_args(mut args: std::env::Args) -> anyhow::Result<()> {
    // argv = [<binary path>, SUBCOMMAND, ...flags]; clap wants [<program>, ...flags].
    let program = args.next().unwrap_or_else(|| SUBCOMMAND.to_string());
    let _subcommand = args.next();
    let args = ServerArgs::parse_from(std::iter::once(program).chain(args));
    env_logger::try_init().ok();

    let http_client = Arc::new(reqwest_client::ReqwestClient::new());
    let app = gpui_platform::headless().with_http_client(http_client);
    let model_name = args.model.clone();
    let thinking_override = args.thinking;
    let reasoning_effort = args.reasoning_effort.clone();
    let no_staff = args.no_staff;

    app.run(move |cx| {
        let app_state = boot_app(cx);
        cx.spawn(async move |cx| {
            let exit_code = match run_server(
                &app_state,
                &model_name,
                thinking_override,
                reasoning_effort.as_deref(),
                !no_staff,
                cx,
            )
            .await
            {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("Error: {e:#}");
                    1
                }
            };
            cx.update(|cx| cx.quit());
            std::process::exit(exit_code);
        })
        .detach();
    });
    Ok(())
}

/// Applies the provider env overrides, authenticates every provider against
/// the environment, then serves. The server-side twin of eval-cli's `--acp`
/// branch; provider API keys arrive by environment variable, never argv.
pub async fn run_server(
    app_state: &Arc<NativeAgentAppState>,
    model_name: &str,
    thinking_override: Option<bool>,
    reasoning_effort: Option<&str>,
    staff: bool,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    use feature_flags::FeatureFlagAppExt as _;
    // Materializes the FeatureFlagStore so `on_flags_ready` observers can't
    // panic on their first activation. (`AsyncApp::update` returns the
    // closure's value directly in this fork — no `?` on it.)
    cx.update(|cx| cx.set_staff(staff));

    // Each settings change below runs in its own `cx.update` so GPUI flushes
    // the resulting `NotifyGlobalObservers` effect before providers
    // authenticate (same reason as eval-cli's main loop).
    if let Some(providers_json) = model_config::openai_compatible_providers_override() {
        cx.update(|cx| model_config::apply_openai_compatible_providers(&providers_json, cx))?;
    }
    if let Some(models_json) = model_config::anthropic_available_models_override() {
        cx.update(|cx| model_config::apply_anthropic_available_models(&models_json, cx))?;
    }

    let auth_tasks = cx.update(|cx| {
        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry
                .providers()
                .iter()
                .map(|p| p.authenticate(cx))
                .collect::<Vec<_>>()
        })
    });
    futures::future::join_all(auth_tasks).await;

    let openai_compatible_providers_json = model_config::openai_compatible_providers_override();
    let anthropic_available_models_json = model_config::anthropic_available_models_override();
    let model = acp_server::ModelOptions {
        model_name,
        thinking_override,
        reasoning_effort,
        openai_compatible_providers_json: openai_compatible_providers_json.as_deref(),
        anthropic_available_models_json: anthropic_available_models_json.as_deref(),
    };
    acp_server::serve(app_state, model, cx).await
}
