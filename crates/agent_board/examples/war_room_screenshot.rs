//! War-room panel screenshot generator (Plan 024 GOAT artifact).
//!
//! Renders [`WarRoomPanel`] against an inert local-only runtime with a
//! realistic room snapshot (two devices racing one plan, a released marker, a
//! stale scope, a live mention, roster prefill) and captures the frame
//! through the real compositor via an offscreen window — the PNG is
//! pixel-faithful to what the dock renders. Mirrors the pattern of
//! `crates/zed/src/visual_test_runner.rs`.
//!
//! Usage:
//!
//! ```text
//! cargo run -p agent_board --example war_room_screenshot [-- <output-dir>]
//! ```
//!
//! Writes `war_room_panel.png` into the output dir (default
//! `target/agent_board`) and prints the path. macOS-only (needs the Metal
//! headless renderer); exits non-zero if capture is unavailable.

use std::sync::Arc;
use std::time::Duration;

use agent_board::runtime::BoardRuntime;
use agent_board::types::{ActiveScope, AgentStateMessage, BoardMessage, DeviceStatus, RoomSnapshot, ScopeKind};
use agent_board::war_room::WarRoomPanel;
use agent_board::AgentBoardConfig;
use gpui::{AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div, px, size};
use ui::ActiveTheme;

/// Window root that hosts the panel the way the dock does: the dock supplies
/// the panel background, so a standalone render must too (the window clear
/// color is otherwise black and a dark-on-dark render would be unreadable).
struct ScreenshotRoot {
    panel: Entity<WarRoomPanel>,
}

impl Render for ScreenshotRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().colors().panel_background;
        div().size_full().bg(background).child(self.panel.clone())
    }
}

fn main() {
    // Mirror crates/zed/src/visual_test_runner.rs: stateless mode keeps
    // settings::init away from the real user config directories.
    // SAFETY: single-threaded example main, before any threads spawn.
    unsafe { std::env::set_var("ZED_STATELESS", "1") };

    let output_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/agent_board".to_string());

    let mut cx = gpui::VisualTestAppContext::with_asset_source(
        gpui_platform::current_platform(false),
        Arc::new(assets::Assets),
    );
    cx.update(|cx| {
        assets::Assets.load_fonts(cx).expect("failed to load bundled fonts");
        settings::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        // Force One Dark regardless of system appearance so the artifact is
        // deterministic (theme_settings resolves the default by system
        // appearance, which is Light on some machines).
        let registry = theme::ThemeRegistry::global(cx);
        let dark = registry
            .get(theme::DEFAULT_DARK_THEME)
            .expect("One Dark in base themes");
        theme::GlobalTheme::update_theme(cx, dark);
        editor::init(cx);
        BoardRuntime::init_global_with_config(
            http_client::FakeHttpClient::with_404_response(),
            AgentBoardConfig::default(),
            cx,
        );
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let race_path = "/repo/.plans/024_war_room_panel.md";
    let scope = |session_id: &str, plan: &str, summary: &str| ActiveScope {
        session_id: session_id.to_string(),
        plan_file: Some(plan.to_string()),
        task_summary: summary.to_string(),
        scope_kind: ScopeKind::Plan,
    };
    let status = |device: &str, updated_at: i64, stale: bool, scopes: Vec<ActiveScope>| DeviceStatus {
        v: 1,
        device_id: format!("id-{device}"),
        device_name: device.to_string(),
        location_hash: String::new(),
        project_path: "/repo".to_string(),
        scopes,
        updated_at,
        stale,
    };
    let snapshot = RoomSnapshot {
        v: 1,
        room: "test-room".to_string(),
        statuses: vec![
            status("m3", now - 10_000, false, vec![scope("f3a2ffff", race_path, "GOAT gate: mention pipeline")]),
            status("SHIKUWA", now - 20_000, false, vec![scope("b1c9ffff", race_path, "same plan — race ⚠")]),
            status("OLD", now - 6 * 60 * 60 * 1000, true, vec![scope("aaaa1111", "/repo/.plans/old.md", "ancient scope")]),
        ],
        messages: vec![
            BoardMessage {
                v: 1,
                device_id: "id-phone".to_string(),
                device_name: "katopz-phone".to_string(),
                sender: "web".to_string(),
                text: "@m3:f3a2 run cargo clippy before the merge".to_string(),
                ts: now - 5_000,
            },
            BoardMessage {
                v: 1,
                device_id: "id-SHIKUWA".to_string(),
                device_name: "SHIKUWA".to_string(),
                sender: "operator".to_string(),
                text: "released: 023_context_overflow.md".to_string(),
                ts: now - 60_000,
            },
        ],
        states: vec![AgentStateMessage {
            v: 1,
            device_id: "id-SHIKUWA".to_string(),
            device_name: "SHIKUWA".to_string(),
            session_id: "b1c9ffff".to_string(),
            sub_agent_id: None,
            state_text: "released: 023_context_overflow.md".to_string(),
            meta: "/repo/.plans/023_context_overflow.md".to_string(),
            ts: now - 60_000,
        }],
        replies: Vec::new(),
    };
    cx.update(|cx| {
        BoardRuntime::global(cx).update(cx, |runtime, cx| {
            runtime.on_snapshot(snapshot, cx)
        })
    });

    let window = cx
        .open_offscreen_window(size(px(460.), px(780.)), |window, cx| {
            let panel = WarRoomPanel::new(window, cx);
            panel.update(cx, |panel, cx| {
                panel.prefill_mention("SHIKUWA:b1c9", window, cx)
            });
            cx.new(|_| ScreenshotRoot { panel })
        })
        .expect("failed to open offscreen window");

    // Let initialization and layout settle, then force a redraw cycle the
    // same way crates/zed/src/visual_test_runner.rs does: refresh marks the
    // window dirty and requests a frame; run_until_parked drives the frame
    // callbacks so `rendered_frame` holds the actual panel scene.
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, _cx| {
        window.refresh();
    })
    .expect("failed to refresh window");
    cx.run_until_parked();
    cx.advance_clock(Duration::from_millis(200));
    cx.run_until_parked();

    match cx.capture_screenshot(window.into()) {
        Ok(image) => {
            assert!(image.width() > 0 && image.height() > 0);
            std::fs::create_dir_all(&output_dir).expect("failed to create output dir");
            let path = std::path::Path::new(&output_dir).join("war_room_panel.png");
            image.save(&path).expect("failed to save screenshot");
            println!(
                "war room panel screenshot: {} ({}x{})",
                path.display(),
                image.width(),
                image.height()
            );
        }
        Err(error) => {
            eprintln!("screenshot capture failed: {error:#}");
            std::process::exit(1);
        }
    }
}
