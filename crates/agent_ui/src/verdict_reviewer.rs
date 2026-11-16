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
use agent_client_protocol::schema::v2 as acp;
use agent_servers::CLAUDE_AGENT_ID;
use agent_settings::AgentSettings;
use anyhow::{Context as _, Result};
use gpui::{App, Entity, Task, WeakEntity};
use parking_lot::Mutex;
use project::{AgentId, Project};
use serde_json::json;
use settings::{Settings as _, VerdictAdvisorSetting};
use util::path_list::PathList;

use crate::Agent;
use crate::agent_connection_store::AgentConnectionStore;
use crate::agent_panel::AgentPanel;
use crate::thread_worktree_archive::all_open_workspaces;

/// Label of a reviewer session running the advisor route (`.issues/067`),
/// kept distinct so the verdict scorer can split it from the plain session.
const ADVISOR_ROUTE_LABEL: &str = "claude_code+advisor";

/// The executor decides on its own when to consult the advisor; a verdict is
/// only worth the advisor route if the advisor actually weighed in.
const ADVISOR_SYSTEM_PROMPT: &str = "You are reviewing another agent's work and will reply with a verdict. \
     Consult your advisor once before you write each verdict, then decide.";

/// Reviewer backend backed by the panel's Claude Code connection.
pub struct ClaudeCodeReviewer {
    connection_store: WeakEntity<AgentConnectionStore>,
    advisor_sessions: Arc<Mutex<Vec<(acp::SessionId, WeakEntity<AcpThread>)>>>,
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
        Arc::new(Self {
            connection_store,
            advisor_sessions: Arc::default(),
        })
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

/// Session `_meta` that makes `claude-agent-acp` start the reviewer on the
/// executor model with the advisor tool pointed at the advisor model.
fn advisor_session_meta(advisor: &VerdictAdvisorSetting) -> acp::Meta {
    acp::Meta::from_iter([
        (
            "claudeCode".to_string(),
            json!({
                "options": {
                    "model": advisor.executor_model,
                    "settings": { "advisorModel": advisor.advisor_model },
                }
            }),
        ),
        (
            "systemPrompt".to_string(),
            json!({ "append": ADVISOR_SYSTEM_PROMPT }),
        ),
    ])
}

impl VerdictReviewer for ClaudeCodeReviewer {
    fn label(&self) -> &'static str {
        "claude_code"
    }

    fn session_label(&self, session_id: &acp::SessionId) -> &'static str {
        let is_advisor_session = self
            .advisor_sessions
            .lock()
            .iter()
            .any(|(advisor_session_id, _)| advisor_session_id == session_id);
        match is_advisor_session {
            true => ADVISOR_ROUTE_LABEL,
            false => self.label(),
        }
    }

    fn spawn_session(
        &self,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<AcpThread>>> {
        let registered_store = self.connection_store.clone();
        let advisor = AgentSettings::get_global(cx).fork.verdict_advisor.clone();
        let advisor_sessions = self.advisor_sessions.clone();
        cx.spawn(async move |cx| {
            let store =
                cx.update(|cx| resolve_connection_store(&registered_store, &project, cx))?;

            let connect_task = cx.update(|cx| {
                let claude_key = ClaudeCodeReviewer::claude_key();
                if let Some(entry) = store.read(cx).entry(&claude_key) {
                    return entry.read(cx).wait_for_connection();
                }
                // Entries disappear on agent-server updates and
                // version-available events and only reappear when a panel
                // flow re-requests the connection. Request it here, exactly
                // like the panel's own ensure path, so the reviewer works
                // whenever Claude Code is configured — not only when some
                // other flow happened to connect it first.
                let fs = store.read(cx).project().read(cx).fs().clone();
                let thread_store = agent::ThreadStore::global(cx);
                let server = claude_key.server(fs, thread_store);
                let entry = store.update(cx, |store, cx| {
                    store.request_connection(claude_key, server, cx)
                });
                entry.read(cx).wait_for_connection()
            });

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

            if let Some(advisor) = advisor {
                let meta = advisor_session_meta(&advisor);
                let advisor_spawn = cx
                    .update(|cx| {
                        connection.clone().new_session_with_meta(
                            project.clone(),
                            work_dirs.clone(),
                            meta,
                            cx,
                        )
                    })
                    .await;
                match advisor_spawn {
                    Ok(thread) => {
                        let session_id = cx.update(|cx| thread.read(cx).session_id().clone());
                        let mut advisor_sessions = advisor_sessions.lock();
                        advisor_sessions.retain(|(_, thread)| thread.upgrade().is_some());
                        advisor_sessions.push((session_id, thread.downgrade()));
                        return Ok(thread);
                    }
                    Err(error) => {
                        log::warn!(
                            "verdict advisor route ({} advised by {}) failed, \
                             falling back to the plain claude_code reviewer: {error:#}",
                            advisor.executor_model,
                            advisor.advisor_model
                        );
                    }
                }
            }

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

    /// Window + panel; `with_claude_entry` controls whether a connected
    /// Claude Code entry is pre-inserted into the store.
    async fn bootstrap_panel(
        cx: &mut TestAppContext,
        with_claude_entry: bool,
    ) -> (Entity<Project>, Entity<AgentPanel>) {
        let connection = with_claude_entry.then(StubAgentConnection::new);
        bootstrap_panel_with_connection(cx, connection).await
    }

    /// Window + panel; `claude_connection` is pre-inserted into the store as
    /// the connected Claude Code entry when present.
    async fn bootstrap_panel_with_connection(
        cx: &mut TestAppContext,
        claude_connection: Option<StubAgentConnection>,
    ) -> (Entity<Project>, Entity<AgentPanel>) {
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
            .read_with(cx, |multi_workspace, _cx| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        workspace.update(cx, |workspace, _cx| workspace.set_random_database_id());
        let cx = &mut VisualTestContext::from_window(multi_workspace.into(), cx);
        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });

        if let Some(connection) = claude_connection {
            let connection = connection.with_agent_id(AgentId(CLAUDE_AGENT_ID.into()));
            panel.update_in(cx, |panel, _window, cx| {
                panel.connection_store().update(cx, |store, cx| {
                    store.request_connection(
                        ClaudeCodeReviewer::claude_key(),
                        Rc::new(StubAgentServer::new(connection)),
                        cx,
                    );
                });
            });
        }
        cx.run_until_parked();
        (project, panel)
    }

    #[gpui::test]
    async fn spawn_session_falls_back_to_live_panel_after_registered_store_drops(
        cx: &mut TestAppContext,
    ) {
        let (project, _panel) = bootstrap_panel(cx, true).await;

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
        let (project, panel) = bootstrap_panel(cx, true).await;
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

    #[gpui::test]
    async fn spawn_session_requests_the_connection_when_the_entry_is_absent(
        cx: &mut TestAppContext,
    ) {
        let (project, panel) = bootstrap_panel(cx, false).await;
        let store = panel.read_with(cx, |panel, _cx| panel.connection_store().downgrade());

        // No Claude entry exists (pruned by an agent-server update or never
        // requested in this panel instance). The reviewer must REQUEST the
        // connection like the panel's own ensure path — not bail with
        // "not connected". In this test the real CustomAgentServer fails to
        // connect (the agent is not registered), which is the honest
        // connection error rather than the old not-connected bail.
        let reviewer = ClaudeCodeReviewer::new(store);
        let error = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect_err("unregistered agent should fail at connect");
        let error = error.to_string();
        assert!(
            !error.contains("not connected"),
            "reviewer should request the connection instead of bailing: {error}"
        );
        assert!(
            error.contains("connection failed"),
            "unexpected error: {error}"
        );
    }

    fn enable_advisor_route(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut settings = AgentSettings::get_global(cx).clone();
            settings.fork.verdict_advisor = Some(VerdictAdvisorSetting {
                executor_model: "sonnet".to_string(),
                advisor_model: "fable".to_string(),
            });
            AgentSettings::override_global(settings, cx);
        });
    }

    #[gpui::test]
    async fn advisor_route_pins_executor_and_advisor_on_the_session(cx: &mut TestAppContext) {
        let (connection, session_metas) = StubAgentConnection::new().with_session_meta_log();
        let (project, panel) = bootstrap_panel_with_connection(cx, Some(connection)).await;
        let store = panel.read_with(cx, |panel, _cx| panel.connection_store().downgrade());
        enable_advisor_route(cx);

        let reviewer = ClaudeCodeReviewer::new(store);
        let thread = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect("advisor session should spawn");

        let session_metas = session_metas.lock();
        assert_eq!(session_metas.len(), 1, "one advisor session was created");
        let options = &session_metas[0]["claudeCode"]["options"];
        assert_eq!(options["model"], "sonnet");
        assert_eq!(options["settings"]["advisorModel"], "fable");

        let session_id = thread.read_with(cx, |thread, _cx| thread.session_id().clone());
        assert_eq!(reviewer.session_label(&session_id), ADVISOR_ROUTE_LABEL);
    }

    #[gpui::test]
    async fn advisor_route_falls_back_to_the_plain_session_when_refused(cx: &mut TestAppContext) {
        // A stub without a meta log refuses `new_session_with_meta`, like a
        // connection that cannot carry session creation options.
        let (project, panel) = bootstrap_panel(cx, true).await;
        let store = panel.read_with(cx, |panel, _cx| panel.connection_store().downgrade());
        enable_advisor_route(cx);

        let reviewer = ClaudeCodeReviewer::new(store);
        let thread = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect("reviewer should fall back to the plain session");

        let session_id = thread.read_with(cx, |thread, _cx| thread.session_id().clone());
        assert_eq!(reviewer.session_label(&session_id), "claude_code");
    }

    #[gpui::test]
    async fn plain_session_is_used_while_the_advisor_setting_is_unset(cx: &mut TestAppContext) {
        let (connection, session_metas) = StubAgentConnection::new().with_session_meta_log();
        let (project, panel) = bootstrap_panel_with_connection(cx, Some(connection)).await;
        let store = panel.read_with(cx, |panel, _cx| panel.connection_store().downgrade());

        let reviewer = ClaudeCodeReviewer::new(store);
        let thread = cx
            .update(|cx| reviewer.spawn_session(project, PathList::default(), cx))
            .await
            .expect("plain session should spawn");

        assert!(session_metas.lock().is_empty(), "no session options pinned");
        let session_id = thread.read_with(cx, |thread, _cx| thread.session_id().clone());
        assert_eq!(reviewer.session_label(&session_id), "claude_code");
    }
}
