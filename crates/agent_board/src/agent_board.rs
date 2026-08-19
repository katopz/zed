//! agent_board — a multi-device, multi-agent notepad board that mirrors
//! `auto_prompt::plan_registry` to/from a Cloudflare KV worker.
//!
//! See `.plans/013_agent_board.md` for the full design. The short version:
//! - `SetRoom` / `JoinRoom` (the user's "foo"/"bar") connect this device to a
//!   room name, persisted to `~/.config/zed/agent_board.json`.
//! - A background task polls the worker, injects remote claims into the local
//!   `plan_registry`, and posts this device's local claims as its status.
//! - The panel renders the room: active device statuses (what each agent is
//!   working on) + the last short messages.
//! - `auto_prompt`'s existing `format_claims_for_context()` picks up remote
//!   claims with no core changes, so agents reason about each other's work.
//!
//! Plan 024: the network surface lives in [`runtime::BoardRuntime`] (one per
//! process); this panel and [`war_room::WarRoomPanel`] are pure views over it.

pub mod board_state;
pub mod client;
mod feeder;
pub mod identity;
pub mod mcp_tools;
pub mod mentions;
pub mod realtime_client;
pub mod runtime;
pub mod types;
pub mod war_room;

use std::sync::Arc;

use anyhow::Context as _;
use gpui::{
    Action, App, AppContext, Context, Entity, EventEmitter, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, actions, div, px,
};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use ui::ActiveTheme;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

use crate::runtime::BoardRuntime;
use crate::types::BoardMessage;

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

/// Default per-target cooldown between mention injections (Plan 024 P1).
fn default_mention_cooldown_secs() -> u64 {
    60
}

/// Default per-target mention-injection cap per hour (Plan 024 P1).
fn default_mention_max_per_hour() -> u32 {
    20
}

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
    /// Whether real-time push (SSE) is enabled (Plan 015). When true, the
    /// panel maintains an SSE connection to the worker for instant reply
    /// delivery. When false, falls back to the 15s feeder poll.
    #[serde(default)]
    pub realtime_enabled: bool,
    /// Minimum seconds between mention-injections per target session
    /// (Plan 024 loop guard).
    #[serde(default = "default_mention_cooldown_secs")]
    pub mention_cooldown_secs: u64,
    /// Maximum mention-injections per target session per hour (Plan 024 loop
    /// guard).
    #[serde(default = "default_mention_max_per_hour")]
    pub mention_max_per_hour: u32,
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
            realtime_enabled: false,
            mention_cooldown_secs: default_mention_cooldown_secs(),
            mention_max_per_hour: default_mention_max_per_hour(),
        }
    }
}

impl AgentBoardConfig {
    fn config_path() -> anyhow::Result<std::path::PathBuf> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(std::path::Path::new(&home)
            .join(".config")
            .join("zed")
            .join("agent_board.json"))
    }

    pub(crate) fn load() -> Self {
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

    pub(crate) fn save(&self) -> anyhow::Result<()> {
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
// Panel — a pure view over the shared BoardRuntime (Plan 024 P0).
// ---------------------------------------------------------------------------

/// The agent board panel: a dockable view of the room. All networking,
/// polling, and MCP ownership lives in [`BoardRuntime`].
pub struct AgentBoardPanel {
    focus_handle: gpui::FocusHandle,
    runtime: Entity<BoardRuntime>,
    _subscription: Subscription,
}

impl EventEmitter<PanelEvent> for AgentBoardPanel {}

impl AgentBoardPanel {
    /// Construct a panel bound to the shared runtime. Requires
    /// [`init`] to have run (the runtime global to exist).
    pub fn new(
        _workspace: &mut Workspace,
        _window: &mut gpui::Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let runtime = BoardRuntime::global(cx);
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let _subscription = cx.observe(&runtime, |_, _, cx| cx.notify());
            Self {
                focus_handle,
                runtime,
                _subscription,
            }
        })
    }

    pub(crate) fn runtime(&self) -> &Entity<BoardRuntime> {
        &self.runtime
    }
}

impl Focusable for AgentBoardPanel {
    fn focus_handle(&self, _cx: &gpui::App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AgentBoardPanel {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted_color = colors.text_muted;
        let fg = colors.text;
        let accent = colors.text_accent;

        let runtime = self.runtime().read(cx);
        let connected = runtime.connected();
        let room = runtime.room();
        let realtime_enabled = runtime.realtime_enabled();
        let muted_set = runtime.config().muted.clone();
        let snapshot = runtime.snapshot().cloned();

        let mut children: Vec<gpui::AnyElement> = Vec::new();

        // Statuses (other agents' work — the "don't touch" signals).
        if let Some(snapshot) = &snapshot {
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
                        .text_color(muted_color)
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

        // Last messages.
        if let Some(snapshot) = &snapshot {
            if !snapshot.messages.is_empty() {
                children.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_color(muted_color)
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

        // Agent states (Phase 2): what each agent is doing right now, with a
        // per-row mute toggle. Muted rows are dimmed; clicking the toggle adds/
        // removes a MuteKey targeting this exact device+session+sub_agent.
        if let Some(snapshot) = &snapshot {
            if !snapshot.states.is_empty() {
                children.push(
                    div()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .text_color(muted_color)
                        .text_xs()
                        .child(SharedString::from("— agent states —"))
                        .into_any_element(),
                );
                for state in &snapshot.states {
                    let key = types::MuteKey {
                        device_id: Some(state.device_id.clone()),
                        session_id: Some(state.session_id.clone()),
                        sub_agent_id: state.sub_agent_id.clone(),
                    };
                    let is_muted = muted_set.contains(&key);
                    let sub_label = state.sub_agent_id.as_deref().unwrap_or("(main)");
                    let text_color = if is_muted { muted_color } else { fg };
                    let toggle_label = if is_muted { "🔇" } else { "🔊" };
                    let line = format!(
                        "{} · {}: {}",
                        state.device_name, sub_label, state.state_text
                    );
                    let mute_id = format!(
                        "mute-toggle:{}/{}/{}",
                        state.device_id,
                        state.session_id,
                        state.sub_agent_id.as_deref().unwrap_or("")
                    );
                    children.push(
                        div()
                            .px_2()
                            .py_0p5()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_1()
                            .text_color(text_color)
                            .text_xs()
                            .child(SharedString::from(line))
                            .child(
                                div()
                                    .id(mute_id)
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(accent))
                                    .child(SharedString::from(toggle_label))
                                    .on_click(cx.listener({
                                        let key = key.clone();
                                        let runtime = self.runtime().clone();
                                        move |_this, _event, _window, cx| {
                                            runtime.update(cx, |runtime, cx| {
                                                runtime.toggle_mute(key.clone(), cx)
                                            });
                                        }
                                    }))
                                    .into_any_element(),
                            )
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
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(accent)
                                    .child(SharedString::from("Agent Board".to_string())),
                            )
                            .child(
                                div()
                                    .id("agent-board-realtime-toggle")
                                    .cursor_pointer()
                                    .text_xs()
                                    .text_color(if realtime_enabled {
                                        accent
                                    } else {
                                        muted_color
                                    })
                                    .hover(|style| style.text_color(accent))
                                    .child(SharedString::from(if realtime_enabled {
                                        "📡 ON"
                                    } else {
                                        "📡"
                                    }))
                                    .on_click(cx.listener({
                                        let runtime = self.runtime().clone();
                                        move |_this, _event, _window, cx| {
                                            runtime.update(cx, |runtime, cx| {
                                                runtime.toggle_realtime(cx)
                                            });
                                        }
                                    }))
                                    .into_any_element(),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_color)
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

    fn position(&self, _window: &gpui::Window, _cx: &gpui::App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut gpui::Window,
        _cx: &mut Context<Self>,
    ) {
        // Left/right only; no persistence for simplicity.
    }

    fn default_size(&self, _window: &gpui::Window, _cx: &gpui::App) -> gpui::Pixels {
        px(320.)
    }

    fn icon(&self, _window: &gpui::Window, _cx: &gpui::App) -> Option<ui::IconName> {
        Some(ui::IconName::QueueMessage)
    }

    fn icon_tooltip(&self, _window: &gpui::Window, _cx: &gpui::App) -> Option<&'static str> {
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
// init — create the runtime singleton, register actions on every workspace.
// ---------------------------------------------------------------------------

/// Create the process-global [`BoardRuntime`] (starts the poll loop when the
/// worker is configured) and register board + war-room actions with all
/// workspaces. `http` is the app-wide HTTP client.
pub fn init(http: Arc<dyn HttpClient>, cx: &mut App) {
    BoardRuntime::init_global(http, cx);
    war_room::init(cx);

    cx.observe_new(move |workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<AgentBoardPanel>(window, cx) {
                close_or_open(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<AgentBoardPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &SetRoom, window, cx| {
            open_or_focus(workspace, window, cx, |runtime, cx| {
                let current = runtime.config().room.clone();
                if let Some(room) = prompt_string("Set agent board room name", &current) {
                    runtime.set_room(room, cx);
                }
            });
        });
        workspace.register_action(|workspace, _: &JoinRoom, window, cx| {
            open_or_focus(workspace, window, cx, |runtime, cx| {
                let current = runtime.config().room.clone();
                if let Some(room) = prompt_string("Join agent board room", &current) {
                    runtime.set_room(room, cx);
                }
            });
        });
        workspace.register_action(|workspace, _: &Refresh, window, cx| {
            open_or_focus(workspace, window, cx, |runtime, cx| {
                runtime.force_refresh(cx);
            });
        });
        workspace.register_action(|workspace, _: &PostMessage, window, cx| {
            open_or_focus(workspace, window, cx, |runtime, cx| {
                if let Some(text) = prompt_string("Post a short message to the board", "") {
                    runtime.post_message(text, cx);
                }
            });
        });
    })
    .detach();
}

fn close_or_open(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.panel::<AgentBoardPanel>(cx).is_none() {
        let panel = AgentBoardPanel::new(workspace, window, cx);
        workspace.add_panel(panel, window, cx);
    } else {
        workspace.close_panel::<AgentBoardPanel>(window, cx);
    }
}

fn open_or_focus(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut BoardRuntime, &mut Context<BoardRuntime>),
) {
    let panel = match workspace.panel::<AgentBoardPanel>(cx) {
        Some(panel) => panel,
        None => {
            let panel = AgentBoardPanel::new(workspace, window, cx);
            workspace.add_panel(panel.clone(), window, cx);
            panel
        }
    };
    let runtime = panel.read(cx).runtime().clone();
    runtime.update(cx, |runtime, cx| then(runtime, cx));
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn format_message(message: &BoardMessage) -> String {
    let secs = message.ts / 1000;
    let when = chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string());
    format!(
        "[{when}] {}: {}",
        mentions::sender_label(message),
        message.text
    )
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
