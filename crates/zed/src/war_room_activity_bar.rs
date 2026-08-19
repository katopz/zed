// Synchronous artifact generator, like visual_test_runner.
#![allow(clippy::disallowed_methods)]

//! War-room activity-bar screenshot generator (Plan 024 GOAT artifact).
//!
//! Opens production-shaped offscreen windows — `MultiWorkspace` root with
//! the agent `Sidebar` registered, one `Workspace`, and the real panels
//! loaded via their production `::load` constructors (ProjectPanel 1,
//! GitPanel 3, CollabPanel 5, WarRoomPanel 6, OutlinePanel 7) — then
//! captures the frames through the real Metal compositor. Two layouts are
//! captured, because the shipped default is the agentic layout:
//!
//! - DEFAULT (agentic): Collab/Outline/Project/Git dock right while WarRoom
//!   docks left, so the strips read [WarRoom] (left) and [Project, Git,
//!   Collab, Outline] (right).
//! - CLASSIC (`PanelLayout::EDITOR` docks, applied via user settings): every
//!   panel docks left, so the strip reads [Project, Git, Collab, WarRoom,
//!   Outline] — WarRoom directly behind Collab, the adjacency the plan's
//!   GOAT gate describes.
//!
//! The dock's sorted panel entries are also asserted programmatically
//! (`panel_index_for_type`), so the adjacency claim is checked as data, not
//! just pixels. The duplicate-activation-priority debug guard itself is
//! exercised by `agent_board`'s `#[should_panic]` test; this binary runs in
//! debug builds by default, so any priority collision would panic here as
//! well.
//!
//! Mirrors the harness in `crates/zed/src/visual_test_runner.rs` (init +
//! `open_sidebar_test_window` + panel-load + capture patterns).
//!
//! Usage:
//!
//! ```text
//! cargo run -p zed --bin zed_war_room_activity_bar --features visual-tests [-- <output-dir>]
//! ```
//!
//! For each layout writes `<name>.png` (full window) plus `<name>_left.png`
//! / `<name>_right.png` (status-bar corner crops where the dock PanelButtons
//! render) into the output dir (default `target/zed`) and prints the paths.
//! macOS-only (needs the Metal headless renderer); exits non-zero if capture
//! is unavailable.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("war room activity bar generator is only supported on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
use {
    agent_board::{AgentBoardConfig, runtime::BoardRuntime, war_room::WarRoomPanel},
    anyhow::{Context as _, Result},
    assets::Assets,
    collab_ui::collab_panel::CollabPanel,
    fs::Fs,
    git_ui::git_panel::GitPanel,
    gpui::{
        App, AppContext as _, Bounds, Entity, UpdateGlobal as _, VisualTestAppContext, WindowBounds,
        WindowHandle, WindowOptions, point, px, size,
    },
    http_client::FakeHttpClient,
    node_runtime::NodeRuntime,
    outline_panel::OutlinePanel,
    project_panel::ProjectPanel,
    session::Session,
    sidebar::Sidebar,
    std::{path::Path, sync::Arc, time::Duration},
    workspace::{AppState, MultiWorkspace, Workspace},
};

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    // Mirror crates/zed/src/visual_test_runner.rs: stateless mode keeps every
    // subsystem (settings, key value store, databases) away from the real
    // user directories; open_db falls back to an in-memory store.
    // SAFETY: single-threaded example main, before any threads spawn.
    unsafe { std::env::set_var("ZED_STATELESS", "1") };

    let output_dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/zed".to_string());

    let mut cx = VisualTestAppContext::with_asset_source(
        gpui_platform::current_platform(false),
        Arc::new(Assets),
    );

    cx.update(|cx| {
        Assets.load_fonts(cx).expect("failed to load bundled fonts");
        settings::init(cx);
    });

    let app_state = cx.update(init_app_state);

    // Subsystem init, mirroring the visual test runner's sequence plus the
    // collab stack (channel / notifications / collab_ui) that the real
    // CollabPanel needs and the war-room runtime global.
    cx.update(|cx| {
        gpui_tokio::init(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        // Force One Dark regardless of system appearance so the artifact is
        // deterministic (theme_settings resolves the default by system
        // appearance, which is Light on some machines).
        let registry = theme::ThemeRegistry::global(cx);
        let dark = registry
            .get(theme::DEFAULT_DARK_THEME)
            .expect("One Dark in base themes");
        theme::GlobalTheme::update_theme(cx, dark);
        client::init(&app_state.client, cx);
        audio::init(cx);
        workspace::init(app_state.clone(), cx);
        release_channel::init(semver::Version::new(0, 0, 0), cx);
        command_palette::init(cx);
        editor::init(cx);
        call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        title_bar::init(cx);
        project_panel::init(cx);
        outline_panel::init(cx);
        channel::init(&app_state.client.clone(), app_state.user_store.clone(), cx);
        notifications::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        collab_ui::init(&app_state, cx);
        git_ui::init(cx);
        search::init(cx);
        lsp_locations::init(cx);
        prompt_store::init(cx);
        let prompt_builder = prompt_store::PromptBuilder::load(app_state.fs.clone(), false, cx);
        language_model::init(cx);
        client::RefreshLlmTokenListener::register(
            app_state.client.clone(),
            app_state.user_store.clone(),
            cx,
        );
        language_models::init(app_state.user_store.clone(), app_state.client.clone(), cx);
        project::AgentRegistryStore::init_global(
            cx,
            app_state.fs.clone(),
            app_state.client.http_client(),
        );
        agent_ui::init(
            app_state.fs.clone(),
            prompt_builder,
            app_state.languages.clone(),
            true,
            false,
            cx,
        );
        settings_ui::init(cx);

        // War room: inert local-only runtime (default config => no worker, no
        // network, no poll task) so the screenshot needs no live backend.
        BoardRuntime::init_global_with_config(
            FakeHttpClient::with_404_response(),
            AgentBoardConfig::default(),
            cx,
        );
    });

    cx.run_until_parked();

    // Small on-disk project so the worktree, editor and status bar all take
    // their production code paths.
    let temp_dir = tempfile::tempdir().expect("failed to create temp directory");
    let project_path = temp_dir
        .path()
        .canonicalize()
        .expect("failed to canonicalize temp directory");
    std::fs::write(
        project_path.join("main.rs"),
        "fn main() {\n    println!(\"war room activity bar\");\n}\n",
    )
    .expect("failed to write main.rs");

    std::fs::create_dir_all(&output_dir).expect("failed to create output dir");

    // ------------------------------------------------------------------
    // Window 1: DEFAULT settings (agentic layout). Project/Git/Collab/
    // Outline dock right; WarRoom docks left — so the honest default shot
    // shows the WarRoom icon in the LEFT strip.
    // ------------------------------------------------------------------
    let first_project =
        create_project_with_worktree(&project_path, &app_state, &mut cx)
            .expect("failed to create first project");
    let default_window = open_window_with_panels(&mut cx, &app_state, &first_project)
        .expect("failed to open default-layout window");

    // The adjacency claim as data: dock entries are sorted by activation
    // priority (Project 1, Git 3, Collab 5, WarRoom 6, Outline 7).
    let default_docks = read_dock_indices(&mut cx, &default_window.workspace);
    println!("default (agentic) layout docks: {default_docks:#?}");
    anyhow::ensure!(
        default_docks.left_war_room == Some(0) && default_docks.left_len == 1,
        "default layout: expected WarRoom alone in the left dock, got {default_docks:#?}"
    );
    anyhow::ensure!(
        default_docks.right_collab.is_some() && default_docks.right_len == 4,
        "default layout: expected Project/Git/Collab/Outline in the right dock, got {default_docks:#?}"
    );
    anyhow::ensure!(
        default_docks.right_war_room.is_none()
            && default_docks.right_outline
                == default_docks.right_collab.map(|index| index + 1),
        "default layout: right dock order should be ...Collab, Outline with no WarRoom, got {default_docks:#?}"
    );

    capture_and_save(
        &mut cx,
        default_window.window,
        &output_dir,
        "war_room_activity_bar_default",
    )?;

    // ------------------------------------------------------------------
    // Window 2: CLASSIC layout (the `PanelLayout::EDITOR` preset — every
    // panel involved here docks left), which is the layout the plan's
    // adjacency claim describes: Collab 5, WarRoom 6, Outline 7 all in the
    // LEFT strip, WarRoom directly behind Collab.
    // ------------------------------------------------------------------
    cx.update(|cx| {
        settings::SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project_panel.get_or_insert_default().dock =
                    Some(settings::DockSide::Left);
                settings.outline_panel.get_or_insert_default().dock =
                    Some(settings::DockSide::Left);
                settings.collaboration_panel.get_or_insert_default().dock =
                    Some(settings::DockPosition::Left);
                settings.git_panel.get_or_insert_default().dock =
                    Some(settings::DockPosition::Left);
            });
        });
    });
    cx.run_until_parked();

    let second_project =
        create_project_with_worktree(&project_path, &app_state, &mut cx)
            .expect("failed to create second project");
    let classic_window = open_window_with_panels(&mut cx, &app_state, &second_project)
        .expect("failed to open classic-layout window");

    let classic_docks = read_dock_indices(&mut cx, &classic_window.workspace);
    println!("classic layout docks: {classic_docks:#?}");
    anyhow::ensure!(
        classic_docks.left_len == 5,
        "classic layout: expected 5 left-dock panels, got {classic_docks:#?}"
    );
    anyhow::ensure!(
        classic_docks.left_war_room
            == classic_docks.left_collab.map(|index| index + 1),
        "classic layout: WarRoom ({:?}) must sit directly behind Collab ({:?})",
        classic_docks.left_war_room,
        classic_docks.left_collab,
    );
    anyhow::ensure!(
        classic_docks.left_outline == classic_docks.left_war_room.map(|index| index + 1),
        "classic layout: Outline ({:?}) must sit directly behind WarRoom ({:?})",
        classic_docks.left_outline,
        classic_docks.left_war_room,
    );
    anyhow::ensure!(
        classic_docks.right_len == 0
            && classic_docks.right_collab.is_none()
            && classic_docks.right_war_room.is_none()
            && classic_docks.right_outline.is_none(),
        "classic layout: right dock should be empty for this panel set, got {classic_docks:#?}"
    );
    println!(
        "left dock adjacency verified (classic layout): Collab at {:?}, WarRoom at {:?}, Outline at {:?}",
        classic_docks.left_collab, classic_docks.left_war_room, classic_docks.left_outline
    );

    capture_and_save(
        &mut cx,
        classic_window.window,
        &output_dir,
        "war_room_activity_bar_classic",
    )?;

    Ok(())
}

/// A window opened the production way: `MultiWorkspace` root, agent
/// `Sidebar` registered and open, one `Workspace`, and the real left/right
/// dock panels added (positions resolve from the CURRENT settings at
/// add_panel time, so the same helper serves both layouts).
#[cfg(target_os = "macos")]
struct OpenedWindow {
    window: WindowHandle<MultiWorkspace>,
    workspace: Entity<Workspace>,
}

#[cfg(target_os = "macos")]
fn open_window_with_panels(
    cx: &mut VisualTestAppContext,
    app_state: &Arc<AppState>,
    project: &Entity<project::Project>,
) -> Result<OpenedWindow> {
    let window: WindowHandle<MultiWorkspace> = cx
        .update(|cx| {
            let bounds = Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(1280.), px(800.)),
            };
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                |window, cx| {
                    let workspace = cx.new(|cx| {
                        Workspace::new(None, project.clone(), app_state.clone(), window, cx)
                    });
                    cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
                },
            )
        })
        .context("failed to open window")?;

    cx.run_until_parked();

    // Register the agent sidebar the way `zed::initialize_workspace` does —
    // outside the MultiWorkspace update to avoid a re-entrant read.
    cx.update_window(window.into(), |root_view, window, cx| {
        let multi_workspace: Entity<MultiWorkspace> = root_view
            .downcast()
            .map_err(|_| anyhow::anyhow!("window root is not the MultiWorkspace"))
            .expect("window root is the MultiWorkspace");
        let sidebar = cx.new(|cx| Sidebar::new(multi_workspace.clone(), window, cx));
        multi_workspace.update(cx, |multi_workspace, cx| {
            multi_workspace.register_sidebar(sidebar, cx);
        });
    })
    .context("failed to register sidebar")?;

    window
        .update(cx, |multi_workspace, _window, cx| {
            multi_workspace.open_sidebar(cx);
        })
        .context("failed to open sidebar")?;

    cx.run_until_parked();

    let workspace = window
        .update(cx, |multi_workspace, _window, _cx| {
            multi_workspace.workspaces().next().cloned()
        })
        .context("failed to get workspace")?
        .context("one workspace per window")?;

    let (weak_workspace, async_window_cx) = window
        .update(cx, |_multi_workspace, window, cx| {
            (workspace.downgrade(), window.to_async(cx))
        })
        .context("failed to get workspace handle")?;

    cx.background_executor.allow_parking();
    let project_panel = cx
        .foreground_executor
        .block_test(ProjectPanel::load(
            weak_workspace.clone(),
            async_window_cx.clone(),
        ))
        .context("failed to load project panel")?;
    let git_panel = cx
        .foreground_executor
        .block_test(GitPanel::load(
            weak_workspace.clone(),
            async_window_cx.clone(),
        ))
        .context("failed to load git panel")?;
    let collab_panel = cx
        .foreground_executor
        .block_test(CollabPanel::load(
            weak_workspace.clone(),
            async_window_cx.clone(),
        ))
        .context("failed to load collab panel")?;
    let outline_panel = cx
        .foreground_executor
        .block_test(OutlinePanel::load(
            weak_workspace,
            async_window_cx,
        ))
        .context("failed to load outline panel")?;
    cx.background_executor.forbid_parking();

    window
        .update(cx, |_multi_workspace, window, cx| {
            let war_room_panel = WarRoomPanel::new(window, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.add_panel(project_panel, window, cx);
                workspace.add_panel(git_panel, window, cx);
                workspace.add_panel(collab_panel, window, cx);
                workspace.add_panel(war_room_panel, window, cx);
                workspace.add_panel(outline_panel, window, cx);
            });
        })
        .context("failed to add panels")?;

    cx.run_until_parked();
    Ok(OpenedWindow { window, workspace })
}

#[derive(Debug)]
#[cfg(target_os = "macos")]
struct DockIndices {
    left_len: usize,
    left_collab: Option<usize>,
    left_war_room: Option<usize>,
    left_outline: Option<usize>,
    right_len: usize,
    right_collab: Option<usize>,
    right_war_room: Option<usize>,
    right_outline: Option<usize>,
}

#[cfg(target_os = "macos")]
fn read_dock_indices(cx: &mut VisualTestAppContext, workspace: &Entity<Workspace>) -> DockIndices {
    workspace
        .update(cx, |workspace, cx| {
            let read = |dock: &workspace::dock::Dock| {
                (
                    dock.panels_len(),
                    dock.panel_index_for_type::<CollabPanel>(),
                    dock.panel_index_for_type::<WarRoomPanel>(),
                    dock.panel_index_for_type::<OutlinePanel>(),
                )
            };
            let (left_len, left_collab, left_war_room, left_outline) =
                read(workspace.left_dock().read(cx));
            let (right_len, right_collab, right_war_room, right_outline) =
                read(workspace.right_dock().read(cx));
            DockIndices {
                left_len,
                left_collab,
                left_war_room,
                left_outline,
                right_len,
                right_collab,
                right_war_room,
                right_outline,
            }
        })
}

/// Settle, refresh, capture, and write the full window plus crops of the
/// status bar corners where the left/right dock PanelButtons render.
#[cfg(target_os = "macos")]
fn capture_and_save(
    cx: &mut VisualTestAppContext,
    window: WindowHandle<MultiWorkspace>,
    output_dir: &str,
    name: &str,
) -> Result<()> {
    for _ in 0..10 {
        cx.advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
    }
    cx.update_window(window.into(), |_, window, _cx| window.refresh())
        .context("failed to refresh window")?;
    cx.run_until_parked();

    let screenshot = cx
        .capture_screenshot(window.into())
        .context("failed to capture screenshot")?;
    // The compositor captures at the window's scale factor (2x on retina:
    // 2560x1600 for the 1280x800 window), so derive the scale instead of
    // asserting logical pixels.
    let scale = screenshot.width() / 1280;
    anyhow::ensure!(
        scale >= 1 && screenshot.width() == 1280 * scale && screenshot.height() == 800 * scale,
        "unexpected capture size {}x{}",
        screenshot.width(),
        screenshot.height()
    );

    let full_path = Path::new(output_dir).join(format!("{name}.png"));
    screenshot.save(&full_path).context("failed to save screenshot")?;

    let strip_width = (480 * scale).min(screenshot.width());
    let strip_height = (48 * scale).min(screenshot.height());
    let left_strip = image::imageops::crop_imm(
        &screenshot,
        0,
        screenshot.height() - strip_height,
        strip_width,
        strip_height,
    )
    .to_image();
    let left_strip_path = Path::new(output_dir).join(format!("{name}_left.png"));
    left_strip
        .save(&left_strip_path)
        .context("failed to save left strip")?;

    let right_strip = image::imageops::crop_imm(
        &screenshot,
        screenshot.width() - strip_width,
        screenshot.height() - strip_height,
        strip_width,
        strip_height,
    )
    .to_image();
    let right_strip_path = Path::new(output_dir).join(format!("{name}_right.png"));
    right_strip
        .save(&right_strip_path)
        .context("failed to save right strip")?;

    println!(
        "captured {name}: {} ({}x{}), strips {} | {}",
        full_path.display(),
        screenshot.width(),
        screenshot.height(),
        left_strip_path.display(),
        right_strip_path.display(),
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn init_app_state(cx: &mut App) -> Arc<AppState> {
    if !cx.has_global::<settings::SettingsStore>() {
        let settings_store = settings::SettingsStore::test(cx);
        cx.set_global(settings_store);
    }

    // Real filesystem: the temp project lives on disk.
    let fs: Arc<dyn Fs> = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);

    let languages = Arc::new(language::LanguageRegistry::test(
        cx.background_executor().clone(),
    ));
    let clock = Arc::new(clock::FakeSystemClock::new());
    let http_client = FakeHttpClient::with_404_response();
    let client = client::Client::new(clock, http_client, cx);
    let session = cx.new(|cx| session::AppSession::new(Session::test(), cx));
    let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| workspace::WorkspaceStore::new(client.clone(), cx));

    theme_settings::init(theme::LoadThemes::JustBase, cx);
    client::init(&client, cx);

    let app_state = Arc::new(AppState {
        client,
        fs,
        languages,
        user_store,
        workspace_store,
        node_runtime: NodeRuntime::unavailable(),
        build_window_options: |_, _| Default::default(),
        session,
    });
    AppState::set_global(app_state.clone(), cx);
    app_state
}

#[cfg(target_os = "macos")]
fn create_project_with_worktree(
    worktree_dir: &Path,
    app_state: &Arc<AppState>,
    cx: &mut VisualTestAppContext,
) -> Result<Entity<project::Project>> {
    let project = cx.update(|cx| {
        project::Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            None,
            project::LocalProjectFlags {
                init_worktree_trust: false,
                ..Default::default()
            },
            cx,
        )
    });

    let add_task = cx.update(|cx| {
        project.update(cx, |project, cx| {
            project.find_or_create_worktree(worktree_dir, true, cx)
        })
    });

    cx.background_executor.allow_parking();
    cx.foreground_executor
        .block_test(add_task)
        .context("failed to add worktree")?;
    cx.background_executor.forbid_parking();

    cx.run_until_parked();
    Ok(project)
}
