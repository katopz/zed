//! Claude Code reviewer backend for the verdict ping-pong
//! (`.plans/029_claude_code_verdict_reviewer.md`, proposal 001 phase 6).
//!
//! Implements `acp_thread::verdict::VerdictReviewer` on top of the panel's
//! `AgentConnectionStore`: when `agent.verdict_reviewer = "claude_code"`, the
//! `request_verdict` tool spawns an off-screen Claude Code session via the
//! same connection the panel already maintains, so the reviewer runs on
//! Claude Code's own subscription auth — no Anthropic API key required.
//!
//! Visibility matches the hidden orchestrator (`.plans/014`): the session is
//! never registered in any panel list, so it's invisible to the user.

use std::sync::Arc;

use acp_thread::AcpThread;
use acp_thread::verdict::VerdictReviewer;
use agent_servers::CLAUDE_AGENT_ID;
use anyhow::{Context as _, Result, bail};
use gpui::{App, Entity, Task, WeakEntity};
use project::{AgentId, Project};
use util::path_list::PathList;

use crate::Agent;
use crate::agent_connection_store::AgentConnectionStore;
use crate::agent_panel::AgentPanel;
use crate::thread_worktree_archive::all_open_workspaces;

/// Reviewer backend backed by the panel's Claude Code connection.
pub struct ClaudeCodeReviewer {
    connection_store: WeakEntity<AgentConnectionStore>,
}

/// Preference order for candidate connection stores: a Claude entry beats a
/// bare store, the requesting project's store beats a foreign one, and the
/// store this reviewer was registered with is only a tiebreaker.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StoreRank {
    has_claude_entry: bool,
    is_same_project: bool,
    is_registered: bool,
}

impl ClaudeCodeReviewer {
    /// Builds the backend. Register it with
    /// `acp_thread::verdict::set_reviewer(Some(...))` when the panel creates
    /// its connection store.
    pub fn new(connection_store: WeakEntity<AgentConnectionStore>) -> Arc<Self> {
        Arc::new(Self { connection_store })
    }

    fn claude_key() -> Agent {
        Agent::Custom {
            id: AgentId(CLAUDE_AGENT_ID.into()),
        }
    }
}

/// Picks the best live connection store. The handle captured at registration
/// goes stale whenever the panel that registered last drops (window closed,
/// workspace swapped) while other panels keep running, so the registered
/// handle is only a hint — live panels are scanned as the source of truth.
fn resolve_connection_store(
    registered: &WeakEntity<AgentConnectionStore>,
    project: &Entity<Project>,
    cx: &App,
) -> Result<Entity<AgentConnectionStore>> {
    let mut candidates = Vec::new();
    if let Some(store) = registered.upgrade() {
        candidates.push((rank_store(&store, project, true, cx), store));
    }
    for workspace in all_open_workspaces(cx) {
        let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) else {
            continue;
        };
        let store = panel.read(cx).connection_store().clone();
        candidates.push((rank_store(&store, project, false, cx), store));
    }

    candidates
        .into_iter()
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, store)| store)
        .context("no open agent panel with a connection store — open the agent panel to connect Claude Code")
}

fn rank_store(
    store: &Entity<AgentConnectionStore>,
    project: &Entity<Project>,
    is_registered: bool,
    cx: &App,
) -> StoreRank {
    let store = store.read(cx);
    StoreRank {
        has_claude_entry: store.entry(&ClaudeCodeReviewer::claude_key()).is_some(),
        is_same_project: store.project().entity_id() == project.entity_id(),
        is_registered,
    }
}

impl VerdictReviewer for ClaudeCodeReviewer {
    fn label(&self) -> &'static str {
        "claude_code"
    }

    fn spawn_session(
        &self,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<AcpThread>>> {
        let registered_store = self.connection_store.clone();
        cx.spawn(async move |cx| {
            let store =
                cx.update(|cx| resolve_connection_store(&registered_store, &project, cx))?;

            // Connected entry → shared connect task (also covers the
            // still-connecting case; this awaits it like any panel consumer).
            let connect_task = cx.update(|cx| {
                store
                    .read(cx)
                    .entry(&ClaudeCodeReviewer::claude_key())
                    .map(|entry| entry.read(cx).wait_for_connection())
            });
            let Some(connect_task) = connect_task else {
                bail!("Claude Code is not connected — open the agent panel to connect it");
            };

            let connected = connect_task
                .await
                .context("Claude Code connection failed")?;
            let connection = connected.connection;

            // Defensive: the store entry could have been reused for another
            // agent; never spawn a reviewer on the wrong connection.
            anyhow::ensure!(
                connection.agent_id().as_ref() == CLAUDE_AGENT_ID,
                "connected agent is not Claude Code"
            );

            let thread = cx
                .update(|cx| connection.clone().new_session(project, work_dirs, cx))
                .await?;
            Ok(thread)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_view::tests::{StubAgentServer, init_test};
    use acp_thread::StubAgentConnection;
    use fs::FakeFs;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};
    use serde_json::json;
    use std::path::Path;
    use std::rc::Rc;
    use workspace::MultiWorkspace;

    /// Window + panel with a connected Claude Code entry in its store.
    async fn bootstrap_panel(cx: &mut TestAppContext) -> (Entity<Project>, Entity<AgentPanel>) {
        init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            language_model::LanguageModelRegistry::test(cx);
        });
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let multi_workspace =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _cx| multi_workspace.workspace().clone())
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        let connection = StubAgentConnection::new().with_agent_id(AgentId(CLAUDE_AGENT_ID.into()));
        panel.update_in(cx, |panel, _window, cx| {
            panel
                .connection_store()
                .update(cx, |store, cx| {
                    store.request_connection(
                        ClaudeCodeReviewer::claude_key(),
                        Rc::new(StubAgentServer::new(connection)),
                        cx,
                    );
                });
        });
        cx.run_until_parked();
        (project, panel)
    }

    #[gpui::test]
    async fn spawn_session_falls_back_to_live_panel_after_registered_store_drops(
        cx: &mut TestAppContext,
    ) {
        let (project, _panel) = bootstrap_panel(cx).await;

        // A later registration whose store has since been dropped, as happens
        // when a second panel/window closes: the global slot kept pointing at
        // the dead store while the live panel above stayed fully connected.
        let dropped_store =
            cx.update(|cx| cx.new(|cx| AgentConnectionStore::new(project.clone(), cx)));
        let stale_handle = dropped_store.downgrade();
        drop(dropped_store);
        cx.run_until_parked();
        assert!(stale_handle.upgrade().is_none(), "store should be gone");

        let reviewer = ClaudeCodeReviewer::new(stale_handle);
        let _thread = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect("reviewer should resolve the live panel's connection store");
    }

    #[gpui::test]
    async fn spawn_session_uses_registered_store_while_alive(cx: &mut TestAppContext) {
        let (project, panel) = bootstrap_panel(cx).await;
        let store = panel.read_with(cx, |panel, _cx| panel.connection_store().downgrade());

        let reviewer = ClaudeCodeReviewer::new(store);
        let _thread = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect("reviewer should use the registered, still-live store");
    }

    #[gpui::test]
    async fn spawn_session_errors_when_no_panel_remains(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree("/project", json!({ "file.txt": "" })).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;

        let dropped_store = cx.new(|cx| AgentConnectionStore::new(project.clone(), cx));
        let stale_handle = dropped_store.downgrade();
        drop(dropped_store);
        cx.run_until_parked();

        let reviewer = ClaudeCodeReviewer::new(stale_handle);
        let error = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect_err("spawn_session should fail without any live panel");
        assert!(
            error.to_string().contains("no open agent panel"),
            "unexpected error: {error:#}"
        );
    }
}
