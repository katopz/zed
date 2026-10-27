//! Fork-owned `NativeAgent` API (plan 038): kept out of `agent.rs` so
//! upstream syncs don't conflict on it. Child module of the crate root, so it
//! sees the private types and fields it extends.

use super::*;

static AGENTS_MD_REL_PATH: LazyLock<Option<Arc<RelPath>>> = LazyLock::new(|| {
    RelPath::from_unix_str(claude_md_symlink::AGENTS_MD)
        .ok()
        .map(|path| path.into_arc())
});

static CLAUDE_MD_REL_PATH: LazyLock<Option<Arc<RelPath>>> = LazyLock::new(|| {
    RelPath::from_unix_str(claude_md_symlink::CLAUDE_MD)
        .ok()
        .map(|path| path.into_arc())
});

impl NativeAgent {
    /// Symlinks `CLAUDE.md` onto this worktree's `AGENTS.md`, so Claude Code
    /// reads the same project instructions the native agent does.
    ///
    /// Returns `None` — touching the filesystem not at all — unless the
    /// worktree is local, has an `AGENTS.md`, and has no `CLAUDE.md` yet. The
    /// entry lookups are in-memory, so the steady state after the link exists
    /// costs nothing on the repeated project-context refreshes.
    pub(crate) fn link_project_claude_md(
        worktree: &Entity<Worktree>,
        project: &Entity<Project>,
        cx: &App,
    ) -> Option<Task<()>> {
        if !AgentSettings::get_global(cx).fork.project_claude_md_symlink {
            return None;
        }

        let fs = project.read(cx).fs().clone();
        let worktree = worktree.read(cx);
        // Remote worktrees don't live on the filesystem `fs` addresses.
        worktree.as_local()?;

        let has_agents_md = AGENTS_MD_REL_PATH
            .as_ref()
            .and_then(|path| worktree.entry_for_path(path))
            .is_some_and(|entry| entry.is_file());
        let has_claude_md = CLAUDE_MD_REL_PATH
            .as_ref()
            .and_then(|path| worktree.entry_for_path(path))
            .is_some();
        if !has_agents_md || has_claude_md {
            return None;
        }

        let worktree_root = worktree.abs_path();
        Some(cx.background_spawn(async move {
            claude_md_symlink::link_project_claude_md(fs.as_ref(), &worktree_root)
                .await
                .log_err();
        }))
    }
}

impl NativeAgentConnection {
    /// Seeds an already-created native session with `messages`, replaying that
    /// history into the session's [`AcpThread`] as real rendered turns and tool
    /// cards.
    ///
    /// This is the native-agent branch path for "Branch New Thread": the caller
    /// creates a fresh, empty session through the normal panel wiring, then
    /// calls this to fork another thread's message history into it. Unlike
    /// flattening the conversation into a single composer message, the new
    /// thread carries the original user/assistant message boundaries so tool
    /// cards, thinking blocks, and per-message structure survive the fork.
    ///
    /// The returned task resolves once the replayed events have been drained
    /// into the AcpThread.
    pub fn seed_history(
        &self,
        session_id: &acp_v2::SessionId,
        messages: Vec<Arc<Message>>,
        cx: &mut App,
    ) -> Task<Result<acp_v2::StopReason>> {
        let Some((thread, acp_thread)) = self.0.update(cx, |agent, _cx| {
            agent
                .sessions
                .get(session_id)
                .map(|s| (s.thread.clone(), s.acp_thread.clone()))
        }) else {
            return Task::ready(Err(anyhow!("session not found for seed_history")));
        };

        let events = thread.update(cx, |thread, cx| {
            thread.set_messages(messages, cx);
            thread.replay(cx)
        });
        Self::handle_thread_events(events, acp_thread, None, cx)
    }
}

impl NativeThreadEnvironment {
    /// Creates a subagent pinned to `model_override` (the secondary reviewer),
    /// else the configured `verdict_model`, else the parent thread's model.
    /// Used by the `request_verdict` tool so the reviewer can be a different
    /// model than the worker.
    pub(crate) fn create_verdict_subagent_thread(
        &self,
        label: String,
        model_override: Option<LanguageModelSelection>,
        cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>> {
        let model =
            model_override.or_else(|| AgentSettings::get_global(cx).fork.verdict_model.clone());
        self.create_subagent_thread_inner(label, model, cx)
    }

    pub(crate) fn list_peer_threads(&self, cx: &mut App) -> Result<Vec<ThreadListing>> {
        let requester = self
            .thread
            .upgrade()
            .context("Thread no longer exists")?
            .read(cx)
            .id()
            .clone();
        self.agent
            .upgrade()
            .context("Agent no longer exists")?
            .read(cx)
            .list_threads(&requester, cx)
    }

    pub(crate) fn send_peer_message(
        &self,
        request: ThreadMessageRequest,
        cx: &mut AsyncApp,
    ) -> Task<Result<ThreadMessageOutcome>> {
        let agent = self.agent.clone();
        let sender_thread = self.thread.clone();
        cx.spawn(async move |cx| {
            let sender = sender_thread.read_with(cx, |thread, _| thread.id().clone())?;
            let wait_for_reply = request.wait_for_reply;
            let (session_id, title, posted) = agent.update(cx, |agent, cx| {
                agent.post_thread_message(&sender, request, cx)
            })??;

            let delivery = match posted {
                thread_mailbox::PostedMessage::Queued { position } => {
                    ThreadMessageDelivery::Queued { position }
                }
                thread_mailbox::PostedMessage::Delivered { delivery, thread } => {
                    let turn = cx.update(|cx| delivery.start(cx));
                    // The turn runs detached so cancelling this tool call
                    // never cancels the recipient's turn.
                    let (response_tx, response_rx) = oneshot::channel();
                    cx.spawn(async move |_| {
                        response_tx.send(turn.await).ok();
                    })
                    .detach();

                    let reply = if wait_for_reply {
                        let response = response_rx
                            .await
                            .context("The target thread was dropped")??;
                        if matches!(response, acp_v2::StopReason::Cancelled) {
                            anyhow::bail!("The target thread's turn was cancelled");
                        }
                        thread.read_with(cx, |thread, _| {
                            thread_mailbox::last_agent_message_text(thread)
                        })
                    } else {
                        None
                    };
                    ThreadMessageDelivery::Delivered { reply }
                }
            };

            Ok(ThreadMessageOutcome {
                session_id,
                title,
                delivery,
            })
        })
    }
}

impl NativeSubagentHandle {
    /// Returns the completion of a still-running native execution for the
    /// subagent session, i.e. a retry that replaced the execution whose
    /// completion already resolved. `None` means nothing replaced it, so the
    /// cancellation was genuine.
    pub(crate) fn running_successor_execution(
        acp_thread: &Entity<AcpThread>,
        session_id: &acp_v2::SessionId,
        cx: &App,
    ) -> Option<NativeCompletion> {
        let connection = acp_thread
            .read(cx)
            .connection()
            .clone()
            .into_any()
            .downcast::<NativeAgentConnection>()
            .ok()?;
        let execution = connection
            .0
            .read(cx)
            .sessions
            .get(session_id)?
            .execution
            .as_ref()?;
        execution
            .completion
            .peek()
            .is_none()
            .then(|| execution.completion.clone())
    }
}

impl NativeThreadEnvironment {
    pub(super) fn create_subagent_thread_inner(
        &self,
        label: String,
        model: Option<LanguageModelSelection>,
        cx: &mut App,
    ) -> Result<Rc<dyn SubagentHandle>> {
        let Some(parent_thread_entity) = self.thread.upgrade() else {
            anyhow::bail!("Parent thread no longer exists".to_string());
        };
        let parent_thread = parent_thread_entity.read(cx);
        let current_depth = parent_thread.depth();
        let parent_session_id = parent_thread.id().clone();

        if current_depth >= MAX_SUBAGENT_DEPTH {
            return Err(anyhow!(
                "Maximum subagent depth ({}) reached",
                MAX_SUBAGENT_DEPTH
            ));
        }

        let subagent_thread: Entity<Thread> = cx.new(|cx| {
            let mut thread = Thread::new_subagent(&parent_thread_entity, model.as_ref(), cx);
            thread.set_title(label.into(), cx);
            thread
        });

        let session_id = subagent_thread.read(cx).id().clone();

        let acp_thread = self
            .agent
            .update(cx, |agent, cx| -> Result<Entity<AcpThread>> {
                let project_id = agent
                    .sessions
                    .get(&parent_session_id)
                    .map(|s| s.project_id)
                    .context("parent session not found")?;
                Ok(agent.register_session(subagent_thread.clone(), project_id, None, cx))
            })??;
        self.acp_thread
            .update(cx, |parent, cx| {
                parent.register_subagent(acp_thread.clone(), cx)
            })
            .context("Parent thread no longer exists")?;

        let depth = current_depth + 1;

        telemetry::event!(
            "Subagent Started",
            session = parent_thread_entity.read(cx).id().to_string(),
            subagent_session = session_id.to_string(),
            depth,
            is_resumed = false,
        );

        self.prompt_subagent(session_id, subagent_thread, acp_thread)
    }
}
