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
use anyhow::{Context as _, bail};
use gpui::{App, Entity, Task, WeakEntity};
use project::{AgentId, Project};
use util::path_list::PathList;

use crate::Agent;
use crate::agent_connection_store::AgentConnectionStore;

/// Reviewer backend backed by the panel's Claude Code connection.
pub struct ClaudeCodeReviewer {
    connection_store: WeakEntity<AgentConnectionStore>,
}

impl ClaudeCodeReviewer {
    /// Builds the backend. Register it with
    /// `acp_thread::verdict::set_reviewer(Some(...))` when the panel creates
    /// its connection store, and clear it when the panel drops.
    pub fn new(connection_store: WeakEntity<AgentConnectionStore>) -> Arc<Self> {
        Arc::new(Self { connection_store })
    }

    fn claude_key() -> Agent {
        Agent::Custom {
            id: AgentId(CLAUDE_AGENT_ID.into()),
        }
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
        let store = self.connection_store.clone();
        cx.spawn(async move |cx| {
            let Some(store) = store.upgrade() else {
                bail!("agent panel connection store is gone");
            };

            // Connected entry → shared connect task (also covers the
            // still-connecting case; this awaits it like any panel consumer).
            let connect_task = cx.update(|cx| {
                store
                    .read(cx)
                    .entry(&Self::claude_key())
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
