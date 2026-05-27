use gpui::{App, actions};
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
                let workspace::Event::ActiveItemChanged = event else {
                    return;
                };
                auto_open_svg_preview(workspace, window, cx);
            },
        )
        .detach();
    })
    .detach();
}

fn auto_open_svg_preview(
    workspace: &mut Workspace,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let active_item = workspace.active_item(cx);
    let Some(active_item) = active_item else {
        return;
    };
    let Some(buffer) = active_item.act_as::<MultiBuffer>(cx) else {
        return;
    };
    if !svg_preview_view::SvgPreviewView::is_svg_file(&buffer, cx) {
        return;
    }

    // Check if there's already a preview for this buffer in any pane
    let singleton = buffer.read(cx).as_singleton();
    if let Some(ref singleton_buffer) = singleton {
        let entity_id = singleton_buffer.entity_id();
        let already_has_preview = workspace.panes().iter().any(|pane| {
            pane.read(cx)
                .items_of_type::<svg_preview_view::SvgPreviewView>()
                .any(|view| {
                    view.read(cx)
                        .buffer_entity_id(cx)
                        .is_some_and(|id| id == entity_id)
                })
        });
        if already_has_preview {
            return;
        }
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
