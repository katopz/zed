//! "Work Remote": open an SSH project whose agents run under `zed-agentd` on
//! the host, so closing this window never stops an agent turn.

use agent_servers::DetachableRemoteAgents;
use db::kvp::GlobalKeyValueStore;
use gpui::{App, AppContext as _, TaskExt as _, actions};
use workspace::Workspace;

actions!(
    work_remote,
    [
        /// Opens a remote project whose agents keep running on the host after this window closes.
        WorkRemote,
        /// Starts remote agents directly again instead of through zed-agentd.
        DisableDetachableAgents,
    ]
);

const ENABLED_KEY: &str = "work_remote_detachable_agents";

pub fn init(cx: &mut App) {
    let enabled = match GlobalKeyValueStore::global().read_kvp(ENABLED_KEY) {
        Ok(value) => value.as_deref() == Some("true"),
        Err(error) => {
            log::error!("reading {ENABLED_KEY}: {error:#}");
            false
        }
    };
    DetachableRemoteAgents::set_enabled(enabled, cx);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace
            .register_action(|_workspace, _: &WorkRemote, window, cx| {
                set_enabled(true, cx);
                window.dispatch_action(
                    Box::new(zed_actions::OpenRemote {
                        from_existing_connection: false,
                        create_new_window: Some(true),
                    }),
                    cx,
                );
            })
            .register_action(|_workspace, _: &DisableDetachableAgents, _window, cx| {
                set_enabled(false, cx);
            });
    })
    .detach();
}

fn set_enabled(enabled: bool, cx: &mut App) {
    DetachableRemoteAgents::set_enabled(enabled, cx);
    cx.background_spawn(async move {
        GlobalKeyValueStore::global()
            .write_kvp(ENABLED_KEY.to_string(), enabled.to_string())
            .await
    })
    .detach_and_log_err(cx);
}
