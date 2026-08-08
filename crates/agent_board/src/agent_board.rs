//! agent_board — a multi-device, multi-agent notepad board that mirrors
//! `auto_prompt::plan_registry` to/from a Cloudflare KV worker.
//!
//! See `.plans/013_agent_board.md` for the full design. The short version:
//! - `SetRoom` / `JoinRoom` (the user's "foo"/"bar") connect this device to a
//!   room name, persisted to `~/.config/zed/agent_board.json`.
//! - A background task polls the worker, injects remote claims into the local
//!   `plan_registry`, and posts this device's local claims as its status.
//! - The panel renders the room: active device statuses (what each agent is
//!   working on) + the last 10 short messages.
//! - `auto_prompt`'s existing `format_claims_for_context()` picks up remote
//!   claims with no core changes, so agents reason about each other's work.

pub mod board_state;
pub mod client;
mod feeder;
pub mod identity;
pub mod types;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use gpui::{
    Action, App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Subscription,
    Task, Window, actions, div, px,
};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use ui::ActiveTheme;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

use crate::client::BoardClient;
use crate::identity::DeviceIdentity;
use crate::types::{BoardMessage, RoomSnapshot};

actions!(
    agent_board,
    [
        /// Toggle the agent board panel visibility.
        Toggle,
        /// Toggle focus on the agent board panel.
        ToggleFocus,
        /// Set the room name this device posts to (the user's "foo").
        SetRoom,
        /// Join an existing room by name (the user's "bar").
        JoinRoom,
        /// Force a refresh of the board now.
        Refresh,
        /// Post a short message to the room feed.
        PostMessage,
    ]
);

pub const AGENT_BOARD_KEY: &str = "AgentBoard";

// ---------------------------------------------------------------------------
// Config — `~/.config/zed/agent_board.json` (mirrors auto_prompt's pattern).
// ---------------------------------------------------------------------------

/// Settings for the agent board. Single-user tool, so no secrets here; the SSH
/// private key authorizes writes to the worker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentBoardConfig {
    /// Absolute or `~/`-prefixed path to an ed25519 OpenSSH private key.
    #[serde(default = "default_ssh_key_path")]
    pub ssh_key_path: String,
    /// Base URL of the deployed worker, e.g. `https://agent-board.<acct>.workers.dev`.
    #[serde(default)]
    pub worker_url: String,
    /// Room name both devices agree on. When empty (default), the room is
    /// derived from the SSH key as `blake3(raw_pubkey)` hex — so two devices
    /// with the same key auto-join the same room (Phase 2 point 1-2).
    #[serde(default)]
    pub room: String,
    /// Poll cadence in seconds. KV is eventually consistent; 15s is a sane floor.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Mute targets: agent states matching any [`MuteKey`] here are suppressed
    /// from chat injection and auto_prompt context (Phase 2 point 5-6).
    #[serde(default)]
    pub muted: Vec<crate::types::MuteKey>,
}

fn default_ssh_key_path() -> String {
    "~/.ssh/id_ed25519".to_string()
}
fn default_poll_interval_secs() -> u64 {
    15
}

impl Default for AgentBoardConfig {
    fn default() -> Self {
        Self {
            ssh_key_path: default_ssh_key_path(),
            worker_url: String::new(),
            room: String::new(),
            poll_interval_secs: default_poll_interval_secs(),
            muted: Vec::new(),
        }
    }
}

impl AgentBoardConfig {
    fn config_path() -> Result<std::path::PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(std::path::Path::new(&home)
            .join(".config")
            .join("zed")
            .join("agent_board.json"))
    }

    fn load() -> Self {
        let Ok(path) = Self::config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
                log::warn!(
                    "[agent_board] failed to parse {}: {error}; using defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, contents)
            .with_context(|| format!("writing agent_board config to {}", path.display()))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

/// The agent board panel: a dockable view of the room.
pub struct AgentBoardPanel {
    focus_handle: FocusHandle,
    config: AgentBoardConfig,
    /// App-wide HTTP handle, retained so the client can be rebuilt when the
    /// room or identity changes without reaching back into globals.
    http: Arc<dyn HttpClient>,
    /// Lazily-built client once an identity + worker URL are available.
    client: Option<Arc<BoardClient>>,
    /// Room name resolved from config (explicit) or identity (derived).
    /// None until `try_start` succeeds.
    resolved_room: Option<String>,
    /// Latest snapshot rendered into the panel.
    snapshot: Option<RoomSnapshot>,
    /// Background poll task; dropped when the panel is dropped.
    poll_task: Option<Task<()>>,
    /// Local session id we attribute remote claims relative to. In practice the
    /// board is per-device, so an empty string means "no local thread yet"; the
    /// plan_registry still reflects remote claims regardless.
    local_session_id: String,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for AgentBoardPanel {}

impl AgentBoardPanel {
    /// Construct a panel and attach it to a workspace. Also kicks off the poll
    /// loop if the worker URL + identity are configured.
    pub fn new(
        http: Arc<dyn HttpClient>,
        _workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let config = AgentBoardConfig::load();

        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let mut panel = Self {
                focus_handle,
                config: config.clone(),
                http,
                client: None,
                resolved_room: None,
                snapshot: None,
                poll_task: None,
                local_session_id: String::new(),
                _subscriptions: Vec::new(),
            };
            panel.try_start(cx);
            panel
        })
    }

    /// Build the client from the config + a device identity. No-op (logs) when
    /// the worker URL or SSH key is missing — the board is strictly additive,
    /// so an unconfigured device simply falls back to local-only plan_registry.
    fn try_start(&mut self, cx: &mut Context<Self>) {
        if self.config.worker_url.trim().is_empty() {
            log::info!(
                "[agent_board] worker_url not set in config; running local-only (no remote sync)"
            );
            return;
        }
        let key_path = match identity::expand_ssh_path(&self.config.ssh_key_path) {
            Ok(path) => path,
            Err(error) => {
                log::warn!("[agent_board] ssh key not found: {error:#}; not starting sync");
                return;
            }
        };
        let device_name = hostname();
        let location_hash = identity::location_hash();
        let identity = match DeviceIdentity::load(&key_path, device_name, location_hash) {
            Ok(identity) => identity,
            Err(error) => {
                log::warn!("[agent_board] failed to load device identity: {error:#}");
                return;
            }
        };
        let identity = Arc::new(identity);
        // Phase 2 point 1-2: room = hash(ssh-key). When config.room is empty
        // (default), derive from the identity so two devices sharing a key
        // auto-join the same room. An explicit config.room still overrides.
        let room = if self.config.room.trim().is_empty() {
            identity.room_id().to_string()
        } else {
            self.config.room.clone()
        };
        let client = Arc::new(BoardClient::new(
            self.http.clone(),
            self.config.worker_url.clone(),
            identity,
        ));
        // Phase 2: register the muted set + writer handle so auto_prompt can
        // both read peer states (via peer_states::unmuted_states_for_context)
        // and post agent states (via board_state::post_state) without holding a
        // GPUI entity handle. The muted set translates the board's MuteKey into
        // auto_prompt's dependency-free PeerStateMute at the boundary.
        let muted: Vec<auto_prompt::peer_states::PeerStateMute> = self
            .config
            .muted
            .iter()
            .map(|m| auto_prompt::peer_states::PeerStateMute {
                device_id: m.device_id.clone(),
                session_id: m.session_id.clone(),
                sub_agent_id: m.sub_agent_id.clone(),
            })
            .collect();
        auto_prompt::peer_states::set_muted(muted);
        board_state::register_writer(
            Some(client.clone()),
            Some(room.clone()),
            cx.background_executor().clone(),
        );
        self.resolved_room = Some(room);
        self.client = Some(client);
        self.start_poll(cx);
    }

    fn start_poll(&mut self, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let identity = client.identity().clone();
        let Some(room) = self.resolved_room.clone() else {
            return;
        };
        let project_path = String::new();
        let local_session_id = self.local_session_id.clone();
        let interval = Duration::from_secs(self.config.poll_interval_secs.max(5));

        // One immediate round, then periodic.
        let task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                let client = client.clone();
                let identity = identity.clone();
                let room = room.clone();
                let project_path = project_path.clone();
                let local_session_id = local_session_id.clone();
                let result = cx
                    .background_spawn(async move {
                        feeder::sync_round(&client, &identity, &room, &project_path, &local_session_id)
                            .await
                    })
                    .await;
                match result {
                    Ok(snapshot) => {
                        this.update(cx, |this, cx| {
                            this.snapshot = Some(snapshot);
                            cx.notify();
                        })
                        .ok();
                    }
                    Err(error) => {
                        log::debug!("[agent_board] sync round failed: {error:#}");
                    }
                }
                cx.background_executor().timer(interval).await;
            }
        });
        self.poll_task = Some(task);
    }

    fn force_refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // Cheap path: just nudge the poll loop by restarting it, which fires an
        // immediate round. Avoids a second concurrent fetch.
        self.start_poll(cx);
    }

    fn set_room(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(room) = prompt_string("Set agent board room name", &self.config.room) else {
            return;
        };
        self.config.room = room.trim().to_string();
        if let Err(error) = self.config.save() {
            log::warn!("[agent_board] could not persist config: {error:#}");
        }
        // Restart the poll loop bound to the new room.
        self.client = None;
        self.resolved_room = None;
        self.try_start(cx);
        cx.notify();
    }

    fn join_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // JoinRoom and SetRoom are functionally identical on KV (no join
        // semantics) — keep both actions for UX intent.
        self.set_room(window, cx);
    }

    fn post_message(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = prompt_string("Post a short message to the board", "") else {
            return;
        };
        let Some(client) = self.client.clone() else {
            log::warn!("[agent_board] not connected to a worker; cannot post message");
            return;
        };
        let room = self
            .resolved_room
            .clone()
            .unwrap_or_else(|| self.config.room.clone());
        let device_name = client.identity().device_name().to_string();
        let body = types::PostMessageBody {
            device_name,
            text,
        };
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = cx
                .background_spawn(async move { client.post_message(&room, body).await })
                .await;
            if let Err(error) = result {
                log::warn!("[agent_board] post_message failed: {error:#}");
            } else {
                let _ = this.update(cx, |this, cx| this.force_refresh_no_window(cx));
            }
        })
        .detach();
    }

    fn force_refresh_no_window(&mut self, cx: &mut Context<Self>) {
        self.start_poll(cx);
    }
}

impl Focusable for AgentBoardPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentBoardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        let fg = colors.text;
        let accent = colors.text_accent;

        let connected = self.client.is_some();
        let room = self
            .resolved_room
            .clone()
            .unwrap_or_else(|| self.config.room.clone());

        let mut children: Vec<gpui::AnyElement> = Vec::new();

        // Statuses (other agents' work — the "don't touch" signals).
        if let Some(snapshot) = &self.snapshot {
            for status in &snapshot.statuses {
                let label = format!(
                    "{} · {} scope(s){}",
                    status.device_name,
                    status.scopes.len(),
                    if status.stale { " · STALE" } else { "" }
                );
                children.push(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(muted)
                        .text_xs()
                        .child(SharedString::from(label))
                        .into_any_element(),
                );
                for scope in &status.scopes {
                    let kind = match scope.scope_kind {
                        types::ScopeKind::Plan => "plan",
                        types::ScopeKind::Issue => "issue",
                        types::ScopeKind::Proposal => "proposal",
                    };
                    let file = scope
                        .plan_file
                        .as_deref()
                        .and_then(|path| path.rsplit('/').next());
                    let file = file.unwrap_or("(no file)");
                    let line = format!("  [{kind}] {file} — {}", scope.task_summary);
                    children.push(
                        div()
                            .pl_4()
                            .pr_2()
                            .py_0p5()
                            .text_color(fg)
                            .text_xs()
                            .child(SharedString::from(line))
                            .into_any_element(),
                    );
                }
            }
        }

        // Last 10 messages.
        if let Some(snapshot) = &self.snapshot {
            if !snapshot.messages.is_empty() {
                children.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_color(muted)
                        .text_xs()
                        .child(SharedString::from("— messages —"))
                        .into_any_element(),
                );
                for message in &snapshot.messages {
                    let line = format_message(message);
                    children.push(
                        div()
                            .px_2()
                            .py_0p5()
                            .text_color(fg)
                            .text_xs()
                            .child(SharedString::from(line))
                            .into_any_element(),
                    );
                }
            }
        }

        let connection_line = if connected {
            format!("connected to {room}")
        } else {
            "not connected (local-only)".to_string()
        };

        div()
            .key_context("AgentBoard")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .text_color(fg)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pb_1()
                    .child(
                        div()
                            .text_sm()
                            .text_color(accent)
                            .child(SharedString::from("Agent Board".to_string())),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(connection_line)),
                    ),
            )
            .children(children)
            .into_any_element()
    }
}

impl Panel for AgentBoardPanel {
    fn persistent_name() -> &'static str {
        AGENT_BOARD_KEY
    }

    fn panel_key() -> &'static str {
        AGENT_BOARD_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // Left/right only; no persistence for simplicity.
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> gpui::Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::QueueMessage)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Agent Board")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Toggle.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

// ---------------------------------------------------------------------------
// init — register actions on every workspace, and add the panel when toggled.
// ---------------------------------------------------------------------------

/// Register agent-board actions with all workspaces. `http` is the app-wide
/// HTTP client so the panel can talk to the worker without depending on the
/// `client` crate.
pub fn init(http: Arc<dyn HttpClient>, cx: &mut App) {
    cx.observe_new(move |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
        workspace.register_action({
            let http = http.clone();
            move |workspace, _: &Toggle, window, cx| {
                if !workspace.toggle_panel_focus::<AgentBoardPanel>(window, cx) {
                    close_or_open(workspace, http.clone(), window, cx);
                }
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<AgentBoardPanel>(window, cx);
        });
        workspace.register_action({
            let http = http.clone();
            move |workspace, _: &SetRoom, window, cx| {
                open_or_focus(workspace, http.clone(), window, cx, |panel, window, cx| {
                    panel.set_room(window, cx)
                });
            }
        });
        workspace.register_action({
            let http = http.clone();
            move |workspace, _: &JoinRoom, window, cx| {
                open_or_focus(workspace, http.clone(), window, cx, |panel, window, cx| {
                    panel.join_room(window, cx)
                });
            }
        });
        workspace.register_action({
            let http = http.clone();
            move |workspace, _: &Refresh, window, cx| {
                open_or_focus(workspace, http.clone(), window, cx, |panel, window, cx| {
                    panel.force_refresh(window, cx)
                });
            }
        });
        workspace.register_action({
            let http = http.clone();
            move |workspace, _: &PostMessage, window, cx| {
                open_or_focus(workspace, http.clone(), window, cx, |panel, window, cx| {
                    panel.post_message(window, cx)
                });
            }
        });
    })
    .detach();
}

fn close_or_open(
    workspace: &mut Workspace,
    http: Arc<dyn HttpClient>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.panel::<AgentBoardPanel>(cx).is_none() {
        let panel = AgentBoardPanel::new(http, workspace, window, cx);
        workspace.add_panel(panel, window, cx);
    } else {
        workspace.close_panel::<AgentBoardPanel>(window, cx);
    }
}

fn open_or_focus(
    workspace: &mut Workspace,
    http: Arc<dyn HttpClient>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut AgentBoardPanel, &mut Window, &mut Context<AgentBoardPanel>),
) {
    let panel = match workspace.panel::<AgentBoardPanel>(cx) {
        Some(panel) => panel,
        None => {
            let panel = AgentBoardPanel::new(http, workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            panel
        }
    };
    panel.update(cx, |panel, cx| then(panel, window, cx));
}

// `client_http` removed: every action handler carries the captured `http` from
// `init`, so no global lookup is needed.

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn hostname() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "unknown".to_string())
}

fn format_message(message: &BoardMessage) -> String {
    let secs = message.ts / 1000;
    let when = chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string());
    format!("[{when}] {}: {}", message.device_name, message.text)
}

/// Minimal prompt for a string via the OS. For a single-user tool this is a
/// blocking std input read; in a GUI this would be a popover. Kept simple to
/// avoid pulling in editor/picker machinery.
fn prompt_string(_prompt: &str, default: &str) -> Option<String> {
    // TODO(replace with GPUI popover): for now we return the default so the
    // action is a no-op until a real input UI is wired. The room can be set
    // directly in `~/.config/zed/agent_board.json`.
    if default.is_empty() {
        None
    } else {
        Some(default.to_string())
    }
}
