use gpui::{App, Entity, actions};
use multi_buffer::MultiBuffer;
use workspace::{Pane, SplitDirection, Workspace};

pub mod svg_preview_view;

pub use zed_actions::preview::svg::{OpenPreview, OpenPreviewToTheSide};

actions!(
    svg,
    [
        /// Opens a following SVG preview that syncs with the editor.
        OpenFollowingPreview
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        crate::svg_preview_view::SvgPreviewView::register(workspace, window, cx);

        let workspace_entity = cx.entity();
        cx.subscribe_in(
            &workspace_entity,
            window,
            |workspace, _workspace_entity, event: &workspace::Event, window, cx| {
                let workspace::Event::ItemAdded { item } = event else {
                    return;
                };
                let Some(buffer) = item.act_as::<MultiBuffer>(cx) else {
                    return;
                };
                auto_open_svg_preview(workspace, buffer, window, cx);
            },
        )
        .detach();
    })
    .detach();
}

fn auto_open_svg_preview(
    workspace: &mut Workspace,
    buffer: Entity<MultiBuffer>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    if !svg_preview_view::SvgPreviewView::is_svg_file(&buffer, cx) {
        return;
    }

    // If a Follow-mode preview already exists, it updates via its own
    // workspace subscription — no need to create another one.
    let has_following_preview = workspace.panes().iter().any(|pane| {
        pane.read(cx)
            .items_of_type::<svg_preview_view::SvgPreviewView>()
            .any(|view| view.read(cx).is_following())
    });
    if has_following_preview {
        return;
    }

    let preview = svg_preview_view::SvgPreviewView::create_svg_view(
        svg_preview_view::SvgPreviewMode::Follow,
        workspace,
        buffer,
        window,
        cx,
    );

    // Try to find an existing right pane, or split the active pane
    let right_pane = workspace.find_pane_in_direction(SplitDirection::Right, cx);
    let right_pane = match right_pane {
        Some(pane) => pane,
        None => {
            let active_pane = workspace.active_pane().clone();
            workspace.split_pane(active_pane, SplitDirection::Right, window, cx)
        }
    };

    right_pane.update(cx, |pane: &mut Pane, cx| {
        pane.add_item(Box::new(preview), false, false, None, window, cx);
    });

    cx.notify();
}
