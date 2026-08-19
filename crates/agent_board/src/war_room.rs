//! War Room panel (Plan 024): the conversational surface over the agent
//! board. One shared room feed where the operator (Zed panel), the operator
//! (phone, via Plan 015's web UI), and agents (via MCP) post messages and
//! `@device:sess4`-mention any agent to command it.
//!
//! Two halves:
//! - [`build_work_board`]: a PURE projection over existing streams (room
//!   snapshot + local plan claims) — no new write path, no new wire type. It
//!   is the pinned todolist: who is doing what, stale, or released, race
//!   conflicts flagged, capped to the last 5 hours.
//! - [`WarRoomPanel`]: a pure view over [`crate::runtime::BoardRuntime`]. No
//!   network ownership — all I/O lives in the runtime singleton.

use auto_prompt::plan_registry::ActivePlanClaim;
use editor::Editor;
use gpui::{
    Action, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, actions, div, px,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::ActiveTheme;
use workspace::dock::{DockPosition, Panel, PanelEvent};
use workspace::Workspace;

use crate::mentions;
use crate::runtime::BoardRuntime;
use crate::types::{AgentStateMessage, BoardMessage, RoomSnapshot};

actions!(
    war_room,
    [
        /// Toggle the war room panel visibility.
        Toggle,
        /// Toggle focus on the war room panel.
        ToggleFocus,
        /// Force a refresh of the war room now.
        Refresh,
    ]
);

pub const WAR_ROOM_KEY: &str = "WarRoomPanel";

// ---------------------------------------------------------------------------
// Work board projection (Plan 024 P7) — pure, no I/O, no globals.
// ---------------------------------------------------------------------------

/// Items older than this drop off the board. Time-boxed so the projection
/// stays lean regardless of room chattiness; KV keeps 7d for forensics.
pub const WORK_BOARD_WINDOW_MS: i64 = 5 * 60 * 60 * 1000;

/// A scope/claim without activity for this long renders `Stale`. Matches the
/// plan_registry heartbeat GC window (300s).
pub const STALE_AFTER_MS: i64 = 300 * 1000;

/// Prefix of the terminal-state broadcast emitted at the chain-stop release
/// hook (`plan_registry::release_all_for_session`). `released`, not `done`:
/// the hook fires on success AND abort, and v1 does not distinguish them.
pub const RELEASED_PREFIX: &str = "released: ";

/// Upper bound on rendered rows (bounded by the 5h window anyway).
pub const WORK_BOARD_ROW_CAP: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkState {
    Doing,
    Stale,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkItem {
    /// File name, e.g. `024_war_room_panel.md`.
    pub plan_name: String,
    /// Normalized full path — the merge key.
    pub plan_path: String,
    /// `device:sess4` labels of every owner seen in the window.
    pub owner_labels: Vec<String>,
    pub task_summary: String,
    pub state: WorkState,
    /// Two or more distinct devices `Doing` the same plan — the claim system
    /// narrows this to ~15s of feeder lag; the flag catches what it can't
    /// prevent (manual/same-file collisions).
    pub race: bool,
    /// Wall-clock unix millis (normalized at the call site: claims arrive as
    /// `claimed_ago_secs`, remote items already carry wall-clock ts).
    pub last_activity_ts: i64,
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn session_prefix4(session_id: &str) -> String {
    session_id.chars().take(4).collect()
}

/// A `released: {plan}` state broadcast; `meta` carries the plan path.
fn released_plan(state: &AgentStateMessage) -> Option<(&str, &str)> {
    let name = state.state_text.strip_prefix(RELEASED_PREFIX)?;
    Some((name, state.meta.as_str()))
}

#[derive(Default)]
struct WorkAccumulator {
    owner_devices: Vec<String>,
    owner_labels: Vec<String>,
    task_summary: String,
    last_activity_ts: i64,
    doing: bool,
    released_ts: Option<i64>,
}

impl WorkAccumulator {
    fn add_owner(&mut self, device: &str, label: String) {
        if !self.owner_devices.iter().any(|d| d == device) {
            self.owner_devices.push(device.to_string());
        }
        if !self.owner_labels.contains(&label) {
            self.owner_labels.push(label);
        }
    }
}

/// Build the pinned work board: merge remote status scopes, local plan claims,
/// and `released:` terminal states by normalized plan path. Pure — the same
/// snapshot yields the same board, so it is trivially testable and safe to
/// recompute on every runtime notify.
///
/// Clock epochs (P7): local claims use `time_monotonic_secs` (which is
/// actually UNIX-epoch seconds in plan_registry) exposed as `claimed_ago_secs`;
/// remote items carry wall-clock `updated_at`/`ts`. Both normalize to
/// `now_ms - claimed_ago_secs * 1000` here — never mix raw epochs.
pub fn build_work_board(
    snapshot: &RoomSnapshot,
    local_claims: &[ActivePlanClaim],
    local_device_name: &str,
    now_ms: i64,
) -> Vec<WorkItem> {
    let mut items: std::collections::HashMap<String, WorkAccumulator> =
        std::collections::HashMap::new();

    // Remote scopes from every device's status (including our own, which the
    // feeder re-posts every round).
    for status in &snapshot.statuses {
        for scope in &status.scopes {
            let Some(plan_file) = &scope.plan_file else {
                continue;
            };
            let acc = items.entry(normalize_path(plan_file)).or_default();
            acc.add_owner(
                &status.device_name,
                format!(
                    "{}:{}",
                    status.device_name,
                    session_prefix4(&scope.session_id)
                ),
            );
            if !scope.task_summary.is_empty() {
                acc.task_summary = scope.task_summary.clone();
            }
            acc.last_activity_ts = acc.last_activity_ts.max(status.updated_at);
            if !status.stale {
                acc.doing = true;
            }
        }
    }

    // Local claims (remote-mirrored ones are excluded — they arrive via the
    // snapshot above). `active_claims` only returns heartbeat-fresh claims,
    // so these are `Doing` by definition.
    for claim in local_claims {
        if claim.session_id.starts_with(crate::feeder::REMOTE_SESSION_PREFIX) {
            continue;
        }
        let acc = items.entry(normalize_path(&claim.plan_file)).or_default();
        acc.add_owner(
            local_device_name,
            format!(
                "{}:{}",
                local_device_name,
                session_prefix4(&claim.session_id)
            ),
        );
        if !claim.task_summary.is_empty() {
            acc.task_summary = claim.task_summary.clone();
        }
        let activity_ms = now_ms.saturating_sub(claim.claimed_ago_secs as i64 * 1000);
        acc.last_activity_ts = acc.last_activity_ts.max(activity_ms);
        acc.doing = true;
    }

    // Released terminal states — matched by meta == plan path. A fresh Doing
    // scope on the same path wins (re-claimed after release).
    for state in &snapshot.states {
        let Some((_, plan_path)) = released_plan(state) else {
            continue;
        };
        if plan_path.is_empty() {
            continue;
        }
        let acc = items.entry(normalize_path(plan_path)).or_default();
        acc.released_ts = Some(acc.released_ts.map_or(state.ts, |ts| ts.max(state.ts)));
        acc.last_activity_ts = acc.last_activity_ts.max(state.ts);
        acc.add_owner(
            &state.device_name,
            format!(
                "{}:{}",
                state.device_name,
                session_prefix4(&state.session_id)
            ),
        );
    }

    let mut board: Vec<WorkItem> = items
        .into_iter()
        .filter(|(_, acc)| now_ms.saturating_sub(acc.last_activity_ts) <= WORK_BOARD_WINDOW_MS)
        .map(|(plan_path, acc)| {
            let state = if acc.doing {
                WorkState::Doing
            } else if acc.released_ts.is_some() {
                WorkState::Released
            } else {
                WorkState::Stale
            };
            let race = acc.doing && acc.owner_devices.len() >= 2;
            WorkItem {
                plan_name: basename(&plan_path).to_string(),
                plan_path,
                owner_labels: acc.owner_labels,
                task_summary: acc.task_summary,
                state,
                race,
                last_activity_ts: acc.last_activity_ts,
            }
        })
        .collect();

    board.sort_by(|a, b| {
        race_rank(a)
            .cmp(&race_rank(b))
            .then_with(|| state_rank(a).cmp(&state_rank(b)))
            .then_with(|| b.last_activity_ts.cmp(&a.last_activity_ts))
    });
    board.truncate(WORK_BOARD_ROW_CAP);
    board
}

fn race_rank(item: &WorkItem) -> u8 {
    if item.race {
        0
    } else {
        1
    }
}

fn state_rank(item: &WorkItem) -> u8 {
    match item.state {
        WorkState::Doing => 0,
        WorkState::Stale => 1,
        WorkState::Released => 2,
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

/// The war room: roster + pinned work board + shared feed + @mention input.
/// A pure view over [`BoardRuntime`].
pub struct WarRoomPanel {
    focus_handle: FocusHandle,
    runtime: Entity<BoardRuntime>,
    input: Entity<Editor>,
    board_collapsed: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PanelEvent> for WarRoomPanel {}

impl WarRoomPanel {
    /// Construct the panel. Cheap by design (no I/O — the runtime owns the
    /// network), so eager registration in `initialize_panels` is free.
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Entity<Self> {
        let runtime = BoardRuntime::global(cx);
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let input = cx.new(|cx| Editor::single_line(window, cx));
            let subscription = cx.observe(&runtime, |_, _, cx| cx.notify());
            Self {
                focus_handle,
                runtime,
                input,
                board_collapsed: false,
                _subscriptions: vec![subscription],
            }
        })
    }

    fn send_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .input
            .update(cx, |editor, cx| editor.text(cx))
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }
        self.runtime.update(cx, |runtime, cx| {
            runtime.post_message(text, cx);
        });
        self.input.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });
    }

    fn prefill_mention(&mut self, label: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("@{label} ");
        let input = self.input.clone();
        input.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.focus_handle(cx).focus(window, cx);
        });
    }

    fn render_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        let accent = colors.text_accent;
        let runtime = self.runtime.read(cx);
        let connected = runtime.connected();
        let room = runtime.room();
        let realtime_enabled = runtime.realtime_enabled();

        let connection_line = if connected {
            format!("connected to {room}")
        } else {
            "not connected (local-only)".to_string()
        };

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
                            .child(SharedString::from("War Room".to_string())),
                    )
                    .child(
                        div()
                            .id("war-room-realtime-toggle")
                            .cursor_pointer()
                            .text_xs()
                            .text_color(if realtime_enabled { accent } else { muted })
                            .hover(|style| style.text_color(accent))
                            .child(SharedString::from(if realtime_enabled {
                                "📡 ON"
                            } else {
                                "📡"
                            }))
                            .on_click(cx.listener({
                                let runtime = self.runtime.clone();
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
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("war-room-refresh")
                            .cursor_pointer()
                            .text_xs()
                            .text_color(muted)
                            .hover(|style| style.text_color(accent))
                            .child(SharedString::from("⟳"))
                            .on_click(cx.listener({
                                let runtime = self.runtime.clone();
                                move |_this, _event, _window, cx| {
                                    runtime.update(cx, |runtime, cx| {
                                        runtime.force_refresh(cx)
                                    });
                                }
                            }))
                            .into_any_element(),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(connection_line)),
                    ),
            )
            .into_any_element()
    }

    fn render_work_board(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        let fg = colors.text;
        let accent = colors.text_accent;
        let warning = cx.theme().status().warning;

        let (board, _local_device) = {
            let runtime = self.runtime.read(cx);
            let claims = auto_prompt::plan_registry::active_claims();
            let device = crate::board_state::device_name().unwrap_or_default();
            let now_ms = now_unix_ms();
            let board = runtime
                .snapshot()
                .map(|snapshot| build_work_board(snapshot, &claims, &device, now_ms))
                .unwrap_or_default();
            (board, device)
        };

        let doing = board.iter().filter(|i| i.state == WorkState::Doing).count();
        let stale = board.iter().filter(|i| i.state == WorkState::Stale).count();
        let released = board
            .iter()
            .filter(|i| i.state == WorkState::Released)
            .count();
        let summary = format!("{doing} doing · {stale} stale · {released} released");

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if self.board_collapsed {
            rows.push(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(summary.clone()))
                    .into_any_element(),
            );
        } else {
            if board.is_empty() {
                rows.push(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child(SharedString::from("no active work in the last 5h"))
                        .into_any_element(),
                );
            }
            for (index, item) in board.iter().enumerate() {
                let color = if item.race {
                    warning
                } else {
                    match item.state {
                        WorkState::Doing => fg,
                        WorkState::Stale | WorkState::Released => muted,
                    }
                };
                let flag = if item.race { " ⚠" } else { "" };
                let owners = item.owner_labels.join(", ");
                let line = format!(
                    "{}{flag} · {owners} · {}",
                    match item.state {
                        WorkState::Doing => "doing",
                        WorkState::Stale => "stale",
                        WorkState::Released => "released",
                    },
                    if item.task_summary.is_empty() {
                        &item.plan_name
                    } else {
                        &item.task_summary
                    },
                );
                rows.push(
                    div()
                        .id(("work-item", index as u64))
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .text_color(color)
                        .hover(|style| style.text_color(accent))
                        .child(SharedString::from(line))
                        .into_any_element(),
                );
            }
        }

        div()
            .flex()
            .flex_col()
            .pb_1()
            .child(
                div()
                    .id("war-room-board-toggle")
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .hover(|style| style.text_color(accent))
                    .child(SharedString::from(format!(
                        "— work board ({summary}) —"
                    )))
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.board_collapsed = !this.board_collapsed;
                        cx.notify();
                    }))
                    .into_any_element(),
            )
            .children(rows)
            .into_any_element()
    }

    fn render_roster(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        let fg = colors.text;

        let snapshot = self.runtime.read(cx).snapshot().cloned();
        let mut entries: Vec<(String, String)> = Vec::new(); // (label, state_text)
        if let Some(snapshot) = &snapshot {
            for state in &snapshot.states {
                let label = format!(
                    "{}:{}",
                    state.device_name,
                    session_prefix4(&state.session_id)
                );
                if !entries.iter().any(|(existing, _)| *existing == label) {
                    entries.push((label, state.state_text.clone()));
                }
            }
            for status in &snapshot.statuses {
                for scope in &status.scopes {
                    let label = format!(
                        "{}:{}",
                        status.device_name,
                        session_prefix4(&scope.session_id)
                    );
                    if !entries.iter().any(|(existing, _)| *existing == label) {
                        entries.push((label, scope.task_summary.clone()));
                    }
                }
            }
        }

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (index, (label, state_text)) in entries.iter().enumerate() {
            let line = format!("{label} · {state_text}");
            rows.push(
                div()
                    .id(("roster", index as u64))
                    .cursor_pointer()
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .text_color(fg)
                    .hover(|style| style.text_color(colors.text_accent))
                    .child(SharedString::from(line))
                    .on_click(cx.listener({
                        let label = label.clone();
                        move |this, _event, window, cx| {
                            this.prefill_mention(&label, window, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_1()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from("— roster (click to @mention) —"))
                    .into_any_element(),
            )
            .children(rows)
            .into_any_element()
    }

    fn render_feed(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        let fg = colors.text;
        let accent = colors.text_accent;

        let snapshot = self.runtime.read(cx).snapshot().cloned();
        let local_device = crate::board_state::device_name().unwrap_or_default();

        // Merge messages + agent states into one chronological feed.
        enum FeedEntry {
            Message(BoardMessage),
            State(AgentStateMessage),
        }
        let mut entries: Vec<(i64, FeedEntry)> = Vec::new();
        if let Some(snapshot) = &snapshot {
            for message in &snapshot.messages {
                entries.push((message.ts, FeedEntry::Message(message.clone())));
            }
            for state in &snapshot.states {
                entries.push((state.ts, FeedEntry::State(state.clone())));
            }
        }
        entries.sort_by_key(|(ts, _)| *ts);

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (ts, entry) in entries.iter().rev().take(100).rev() {
            let line = match entry {
                FeedEntry::Message(message) => {
                    let secs = ts / 1000;
                    let when = chrono::DateTime::from_timestamp(secs, 0)
                        .map(|dt| dt.format("%H:%M:%S").to_string())
                        .unwrap_or_else(|| "?".to_string());
                    format!(
                        "[{when}] {}: {}",
                        mentions::sender_label(message),
                        message.text
                    )
                }
                FeedEntry::State(state) => {
                    let sub = state.sub_agent_id.as_deref().unwrap_or("main");
                    format!(
                        "· {} [{}:{sub}] {}",
                        state.device_name,
                        session_prefix4(&state.session_id),
                        state.state_text
                    )
                }
            };
            let is_state = matches!(entry, FeedEntry::State(_));
            let is_mention = match entry {
                FeedEntry::Message(message) => message.text.starts_with('@'),
                _ => false,
            };
            let own = match entry {
                FeedEntry::Message(message) => {
                    !local_device.is_empty() && message.device_name == local_device
                }
                _ => false,
            };
            let color = if is_state {
                muted
            } else if is_mention {
                accent
            } else if own {
                accent
            } else {
                fg
            };
            rows.push(
                div()
                    .px_2()
                    .py_0p5()
                    .text_xs()
                    .text_color(color)
                    .child(SharedString::from(line))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .id("war-room-feed")
            .overflow_y_scroll()
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_1()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from("— feed —"))
                    .into_any_element(),
            )
            .children(rows)
            .into_any_element()
    }

    fn render_input(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let muted = colors.text_muted;
        div()
            .flex()
            .items_center()
            .gap_1()
            .pt_1()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::Newline, window, cx| {
                            this.send_input(window, cx);
                        },
                    ))
                    .child(self.input.clone()),
            )
            .child(
                div()
                    .id("war-room-send")
                    .cursor_pointer()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .hover(|style| style.text_color(colors.text_accent))
                    .child(SharedString::from("Send"))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.send_input(window, cx);
                    }))
                    .into_any_element(),
            )
            .into_any_element()
    }
}

impl Focusable for WarRoomPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WarRoomPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors();
        let fg = colors.text;
        div()
            .key_context("WarRoom")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .text_color(fg)
            .child(self.render_header(cx))
            .child(self.render_work_board(cx))
            .child(self.render_roster(cx))
            .child(self.render_feed(cx))
            .child(self.render_input(cx))
            .into_any_element()
    }
}

impl Panel for WarRoomPanel {
    fn persistent_name() -> &'static str {
        WAR_ROOM_KEY
    }

    fn panel_key() -> &'static str {
        WAR_ROOM_KEY
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
        Some(ui::IconName::UserGroup)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("War Room")
    }

    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let count = mentions::unwatched_mention_count();
        if count > 0 {
            Some(count.to_string())
        } else {
            None
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Toggle.boxed_clone()
    }

    /// Directly behind CollabPanel (5). Outline was renumbered 6→7 and Debug
    /// 7→8 to keep priorities unique — `Dock::add_panel` panics on duplicates
    /// in debug builds.
    fn activation_priority(&self) -> u32 {
        6
    }
}

/// Register war-room actions with all workspaces. The runtime global must
/// already exist (created by [`crate::agent_board::init`]).
pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            mentions::clear_unwatched_mentions();
            if !workspace.toggle_panel_focus::<WarRoomPanel>(window, cx) {
                if workspace.panel::<WarRoomPanel>(cx).is_none() {
                    let panel = WarRoomPanel::new(window, cx);
                    workspace.add_panel(panel, window, cx);
                } else {
                    workspace.close_panel::<WarRoomPanel>(window, cx);
                }
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            mentions::clear_unwatched_mentions();
            workspace.toggle_panel_focus::<WarRoomPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Refresh, _window, cx| {
            if let Some(panel) = workspace.panel::<WarRoomPanel>(cx) {
                let runtime = panel.read(cx).runtime.clone();
                runtime.update(cx, |runtime, cx| runtime.force_refresh(cx));
            }
        });
    })
    .detach();
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests — the projection is pure, so every rule is unit-testable. The panel
// smoke test below additionally exercises the gpui construction path against
// an inert (local-only) runtime.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ActiveScope, DeviceStatus, ScopeKind};

    fn claim(plan_file: &str, session_id: &str, summary: &str, claimed_ago_secs: u64) -> ActivePlanClaim {
        ActivePlanClaim {
            plan_file: plan_file.to_string(),
            session_id: session_id.to_string(),
            task_summary: summary.to_string(),
            claimed_ago_secs,
        }
    }

    fn status(device_name: &str, stale: bool, updated_at: i64, scopes: Vec<ActiveScope>) -> DeviceStatus {
        DeviceStatus {
            v: 1,
            device_id: format!("id-{device_name}"),
            device_name: device_name.to_string(),
            location_hash: String::new(),
            project_path: String::new(),
            scopes,
            updated_at,
            stale,
        }
    }

    fn scope(session_id: &str, plan_file: &str, summary: &str) -> ActiveScope {
        ActiveScope {
            session_id: session_id.to_string(),
            plan_file: Some(plan_file.to_string()),
            task_summary: summary.to_string(),
            scope_kind: ScopeKind::Plan,
        }
    }

    fn released_state(device_name: &str, session_id: &str, plan_path: &str, ts: i64) -> AgentStateMessage {
        let plan_name = basename(plan_path);
        AgentStateMessage {
            v: 1,
            device_id: format!("id-{device_name}"),
            device_name: device_name.to_string(),
            session_id: session_id.to_string(),
            sub_agent_id: None,
            state_text: format!("{RELEASED_PREFIX}{plan_name}"),
            meta: plan_path.to_string(),
            ts,
        }
    }

    fn snapshot(statuses: Vec<DeviceStatus>, states: Vec<AgentStateMessage>) -> RoomSnapshot {
        RoomSnapshot {
            v: 1,
            room: "test".to_string(),
            statuses,
            messages: Vec::new(),
            states,
            replies: Vec::new(),
        }
    }

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn fresh_scope_is_doing() {
        let snap = snapshot(
            vec![status(
                "SHIKUWA",
                false,
                NOW - 30_000,
                vec![scope("b1c9ffff", "/repo/.plans/024_a.md", "war room")],
            )],
            vec![],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].state, WorkState::Doing);
        assert!(!board[0].race);
        assert_eq!(board[0].owner_labels, vec!["SHIKUWA:b1c9".to_string()]);
        assert_eq!(board[0].task_summary, "war room");
        assert_eq!(board[0].plan_name, "024_a.md");
    }

    #[test]
    fn stale_status_is_stale() {
        let snap = snapshot(
            vec![status(
                "SHIKUWA",
                true,
                NOW - 600_000,
                vec![scope("b1c9ffff", "/repo/.plans/024_a.md", "war room")],
            )],
            vec![],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board[0].state, WorkState::Stale);
    }

    #[test]
    fn released_state_pins_released_item() {
        let snap = snapshot(
            vec![],
            vec![released_state("SHIKUWA", "b1c9ffff", "/repo/.plans/023_b.md", NOW - 60_000)],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].state, WorkState::Released);
        assert_eq!(board[0].plan_name, "023_b.md");
    }

    #[test]
    fn doing_scope_wins_over_released_for_same_plan() {
        // Re-claimed after release: fresh scope + old released marker.
        let path = "/repo/.plans/024_a.md";
        let snap = snapshot(
            vec![status(
                "m3",
                false,
                NOW - 10_000,
                vec![scope("f3a2ffff", path, "redo it")],
            )],
            vec![released_state("SHIKUWA", "b1c9ffff", path, NOW - 3_600_000)],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].state, WorkState::Doing);
    }

    #[test]
    fn race_flag_on_two_devices_doing_same_plan() {
        let path = "/repo/.plans/024_a.md";
        let snap = snapshot(
            vec![
                status(
                    "m3",
                    false,
                    NOW - 10_000,
                    vec![scope("f3a2ffff", path, "mine")],
                ),
                status(
                    "SHIKUWA",
                    false,
                    NOW - 20_000,
                    vec![scope("b1c9ffff", path, "also mine")],
                ),
            ],
            vec![],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), 1);
        assert!(board[0].race, "two devices doing the same plan must flag");
        assert_eq!(board[0].owner_labels.len(), 2);
    }

    #[test]
    fn same_device_two_sessions_is_not_a_race() {
        let path = "/repo/.plans/024_a.md";
        let snap = snapshot(
            vec![status(
                "m3",
                false,
                NOW - 10_000,
                vec![
                    scope("f3a2ffff", path, "a"),
                    scope("aaaa1111", path, "b"),
                ],
            )],
            vec![],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert!(!board[0].race);
    }

    #[test]
    fn merges_local_claims_and_remote_scopes_by_path() {
        // Local claim + remote scope on the same normalized path merge into
        // one Doing item with two owners → race.
        let path = "/repo/.plans/024_a.md";
        let snap = snapshot(
            vec![status(
                "SHIKUWA",
                false,
                NOW - 10_000,
                vec![scope("b1c9ffff", path, "theirs")],
            )],
            vec![],
        );
        let claims = vec![claim(path, "f3a2ffff", "ours", 30)];
        let board = build_work_board(&snap, &claims, "m3", NOW);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].state, WorkState::Doing);
        assert!(board[0].race, "local + remote owners = race");
        assert_eq!(board[0].owner_labels.len(), 2);
    }

    #[test]
    fn remote_mirrored_local_claims_are_excluded() {
        // Claims mirrored from remote devices (remote: prefix) are already
        // represented by the snapshot's statuses — must not double-count.
        let claims = vec![claim(
            "/repo/.plans/024_a.md",
            "remote:dev-x:sess1",
            "mirrored",
            10,
        )];
        let snap = snapshot(vec![], vec![]);
        let board = build_work_board(&snap, &claims, "m3", NOW);
        assert!(board.is_empty());
    }

    #[test]
    fn five_hour_cutoff_drops_old_items() {
        let old = snapshot(
            vec![status(
                "SHIKUWA",
                true,
                NOW - WORK_BOARD_WINDOW_MS - 1,
                vec![scope("b1c9ffff", "/repo/.plans/old.md", "ancient")],
            )],
            vec![],
        );
        assert!(build_work_board(&old, &[], "m3", NOW).is_empty());

        // Exactly at the boundary: kept.
        let boundary = snapshot(
            vec![status(
                "SHIKUWA",
                true,
                NOW - WORK_BOARD_WINDOW_MS,
                vec![scope("b1c9ffff", "/repo/.plans/edge.md", "edge")],
            )],
            vec![],
        );
        let board = build_work_board(&boundary, &[], "m3", NOW);
        assert_eq!(board.len(), 1);
    }

    #[test]
    fn local_claim_converts_clock_epochs() {
        // claimed 4h ago, still within the window.
        let claims = vec![claim(
            "/repo/.plans/024_a.md",
            "f3a2ffff",
            "long run",
            4 * 60 * 60,
        )];
        let snap = snapshot(vec![], vec![]);
        let board = build_work_board(&snap, &claims, "m3", NOW);
        assert_eq!(board.len(), 1);
        assert_eq!(board[0].state, WorkState::Doing);
        assert_eq!(board[0].last_activity_ts, NOW - 4 * 60 * 60 * 1000);

        // claimed 6h ago, outside the window.
        let claims = vec![claim(
            "/repo/.plans/024_b.md",
            "f3a2ffff",
            "too old",
            6 * 60 * 60,
        )];
        assert!(build_work_board(&snap, &claims, "m3", NOW).is_empty());
    }

    #[test]
    fn board_order_races_first_then_state() {
        let path_stale = "/repo/.plans/stale.md";
        let path_doing = "/repo/.plans/doing.md";
        let path_race = "/repo/.plans/race.md";
        let snap = snapshot(
            vec![
                status(
                    "SHIKUWA",
                    true,
                    NOW - 10_000,
                    vec![scope("b1c9ffff", path_stale, "stale work")],
                ),
                status(
                    "SHIKUWA",
                    false,
                    NOW - 20_000,
                    vec![scope("b1c9ffff", path_doing, "doing work")],
                ),
                status(
                    "m3",
                    false,
                    NOW - 30_000,
                    vec![scope("f3a2ffff", path_race, "race work")],
                ),
                status(
                    "SHIKUWA",
                    false,
                    NOW - 40_000,
                    vec![scope("b1c9ffff", path_race, "race work 2")],
                ),
            ],
            vec![released_state("m3", "f3a2ffff", "/repo/.plans/done.md", NOW - 5_000)],
        );
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), 4);
        // Category-primary order: race-flagged Doing → Doing → Stale → Released.
        assert!(board[0].race, "race item sorts first");
        assert_eq!(board[0].plan_name, "race.md");
        assert_eq!(board[1].state, WorkState::Doing);
        assert_eq!(board[2].state, WorkState::Stale);
        assert_eq!(board[3].state, WorkState::Released);
    }

    #[test]
    fn row_cap_truncates() {
        let scopes: Vec<ActiveScope> = (0..30)
            .map(|i| scope(&format!("sess{i:04}"), &format!("/repo/.plans/p{i:02}.md"), "w"))
            .collect();
        let snap = snapshot(vec![status("SHIKUWA", false, NOW, scopes)], vec![]);
        let board = build_work_board(&snap, &[], "m3", NOW);
        assert_eq!(board.len(), WORK_BOARD_ROW_CAP);
    }

    // -----------------------------------------------------------------------
    // Panel smoke test (P6, previously deferred): construct the panel against
    // an inert local-only runtime, drive the roster-prefill / send / render
    // paths, and prove both panels share ONE runtime entity (the
    // single-poll-loop GOAT invariant). Hermetic by construction: default
    // config ⇒ no worker URL ⇒ no network, no MCP socket, no poll task; the
    // fake HTTP client 404s if anything ever escapes.
    // -----------------------------------------------------------------------

    /// Trivial window root so the test never forces a full Workspace frame
    /// render — the workspace entity exists for the panel constructors'
    // `Context<Workspace>`, not for drawing.
    struct SmokeRoot;

    impl Render for SmokeRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    async fn panel_smoke_local_only(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        cx.update(|cx| {
            BoardRuntime::init_global_with_config(
                http_client::FakeHttpClient::with_404_response(),
                crate::AgentBoardConfig::default(),
                cx,
            );
        });

        let fake_fs = fs::FakeFs::new(cx.executor());
        let project = project::Project::test(fake_fs, [], cx).await;
        let window = cx.add_window(|_window, _cx| SmokeRoot);
        let workspace = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| Workspace::test_new(project.clone(), window, cx))
            })
            .unwrap();
        let cx = &mut gpui::VisualTestContext::from_window(window.into(), cx);

        // Both panels construct against the ONE shared runtime entity.
        let war_room = workspace.update_in(cx, |_, window, cx| {
            WarRoomPanel::new(window, cx)
        });
        let board_panel = workspace.update_in(cx, |workspace, window, cx| {
            crate::AgentBoardPanel::new(workspace, window, cx)
        });
        let shared_runtime = cx.update(|_window, cx| BoardRuntime::global(cx).entity_id());
        assert_eq!(
            war_room.read_with(cx, |panel, _| panel.runtime.entity_id()),
            shared_runtime
        );
        assert_eq!(
            board_panel.read_with(cx, |panel, _| panel.runtime().entity_id()),
            shared_runtime,
            "the status board and the war room must share one runtime (one poll loop)"
        );

        // Local-only runtime: no rounds, not connected.
        cx.update(|_window, cx| {
            let runtime = BoardRuntime::global(cx);
            let runtime = runtime.read(cx);
            assert_eq!(runtime.poll_rounds(), 0);
            assert!(!runtime.connected());
        });

        // Roster click path: prefill fills the input with an @mention.
        war_room.update_in(cx, |panel, window, cx| {
            panel.prefill_mention("SHIKUWA:b1c9", window, cx)
        });
        let input_text = war_room.read_with(cx, |panel, cx| panel.input.read(cx).text(cx));
        assert_eq!(input_text, "@SHIKUWA:b1c9 ");

        // Render draws the full element tree (header / work board / roster /
        // feed / input) through the framework's draw cycle — layout, prepaint
        // and interaction-state registration all included — without panicking
        // against an empty snapshot.
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(700.)),
            |_, _| war_room.clone().into_any_element(),
        );

        // Feed a realistic snapshot through the runtime's post-poll hook —
        // two devices racing one plan, a released marker, a mention message,
        // a stale scope — then draw again. This exercises the render path
        // with non-empty data (work board rows incl. the race row, roster
        // entries, interleaved feed) and the poll_rounds counter once more.
        let now = now_unix_ms();
        let race_path = "/repo/.plans/024_a.md";
        let snapshot = RoomSnapshot {
            v: 1,
            room: "test-room".to_string(),
            statuses: vec![
                status(
                    "m3",
                    false,
                    now - 10_000,
                    vec![scope("f3a2ffff", race_path, "war room")],
                ),
                status(
                    "SHIKUWA",
                    false,
                    now - 20_000,
                    vec![scope("b1c9ffff", race_path, "war room too")],
                ),
                status(
                    "OLD",
                    true,
                    now - STALE_AFTER_MS - 1,
                    vec![scope("aaaa1111", "/repo/.plans/old.md", "ancient")],
                ),
            ],
            messages: vec![BoardMessage {
                v: 1,
                device_id: "id-SHIKUWA".to_string(),
                device_name: "SHIKUWA".to_string(),
                sender: "operator".to_string(),
                text: "@m3:f3a2 run cargo clippy".to_string(),
                ts: now - 5_000,
            }],
            states: vec![released_state("SHIKUWA", "b1c9ffff", "/repo/.plans/023_b.md", now - 60_000)],
            replies: Vec::new(),
        };
        cx.update(|_window, cx| {
            BoardRuntime::global(cx).update(cx, |runtime, cx| {
                runtime.on_snapshot(snapshot, cx)
            });
        });
        cx.update(|_window, cx| {
            assert_eq!(BoardRuntime::global(cx).read(cx).poll_rounds(), 1);
        });
        cx.draw(
            gpui::point(px(0.), px(0.)),
            gpui::size(px(400.), px(700.)),
            |_, _| war_room.clone().into_any_element(),
        );

        // Send with no worker: warn + clear the input, never panic.
        war_room.update_in(cx, |panel, window, cx| {
            panel.input.update(cx, |editor, cx| {
                editor.set_text("@SHIKUWA:b1c9 run clippy", &mut *window, cx)
            });
            panel.send_input(window, cx);
        });
        let after_send = war_room.read_with(cx, |panel, cx| panel.input.read(cx).text(cx));
        assert_eq!(after_send, "");

        drop(war_room);
        drop(board_panel);
        cx.run_until_parked();
    }
}
