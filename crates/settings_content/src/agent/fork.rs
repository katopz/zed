//! Fork-owned agent settings content (plan 038), kept out of `agent.rs` so
//! upstream syncs don't conflict on it. Flattened into `AgentSettingsContent`.

use crate::LanguageModelSelection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Clone, PartialEq, Serialize, Deserialize, JsonSchema, MergeFrom, Debug, Default)]
pub struct ForkAgentSettingsContent {
    /// Whether the `request_verdict` tool is registered for native agent
    /// threads (verdict ping-pong, `.proposals/001_claude_sub_agent_verdict.md`).
    /// Purely user-invoked (button / right-click menu), so on by default; the
    /// GOAT benchmark (`.issues/016`) decides whether it stays.
    ///
    /// Default: true
    pub verdict_ping_pong: Option<bool>,

    /// The model for the reviewer subagent spawned by the `request_verdict`
    /// tool. Defaults to the parent agent's model when not specified.
    pub verdict_model: Option<LanguageModelSelection>,

    /// Maximum rounds per `request_verdict` negotiation before the tool
    /// refuses further calls.
    ///
    /// Default: 3
    pub verdict_max_rounds: Option<usize>,

    /// Which backend provides the reviewer thread for the `request_verdict`
    /// tool. `native` spawns a subagent pinned to `verdict_model` (requires an
    /// Anthropic API key for a Claude reviewer); `claude_code` spawns an
    /// off-screen Claude Code session on its existing connection (subscription
    /// auth, no API key).
    ///
    /// Default: native
    pub verdict_reviewer: Option<VerdictReviewerSetting>,

    /// A second reviewer model offered next to the primary verdict entry
    /// ("Verdict with <provider>", e.g. GLM). When set, `request_verdict`
    /// accepts `reviewer: "secondary"` and runs the review as a native
    /// subagent pinned to this model, so multi-key providers rotate keys as
    /// usual. Unset hides the secondary entry.
    ///
    /// Default: null
    pub verdict_secondary_model: Option<LanguageModelSelection>,

    /// Whether to symlink `~/.claude/CLAUDE.md` to your user-global `AGENTS.md`,
    /// so Claude Code reads the same personal instructions Zed's agent does.
    ///
    /// Only applies when Claude Code is installed (`~/.claude` exists) and an
    /// `AGENTS.md` exists. An existing `CLAUDE.md` is never replaced.
    ///
    /// Default: true
    pub claude_md_symlink: Option<bool>,

    /// Whether to symlink `CLAUDE.md` to `AGENTS.md` in the root of each project
    /// you open, so Claude Code reads the same project instructions Zed's agent
    /// does.
    ///
    /// Off by default because it adds an untracked file to the repository. An
    /// existing `CLAUDE.md` is never replaced.
    ///
    /// Default: false
    pub project_claude_md_symlink: Option<bool>,

    /// Whether auto_prompt should steal keyboard focus when it creates a
    /// new background continuation thread.
    ///
    /// When false (the default) the new thread is still created and becomes
    /// the panel's base view, but keyboard focus stays wherever the user
    /// left it — typing in another thread, an editor, or a terminal is not
    /// interrupted. Set to true to have the new thread's composer focused
    /// automatically.
    ///
    /// Default: false
    pub auto_focus_new_thread: Option<bool>,

    /// When set, tool permission prompts are automatically answered with the
    /// least-privileged "allow" option after this many seconds, so unattended
    /// agent runs don't stall waiting for confirmation. The prompt shows a
    /// countdown while it is pending. Answering manually cancels the countdown.
    /// Applies to the native agent and to external agents (ACP). Prompts that
    /// are decisions rather than allow/deny grants (e.g. the sandbox-fallback
    /// retry choice) are never auto-answered.
    ///
    /// Default: null (prompts wait indefinitely)
    pub auto_allow_permissions_after_seconds: Option<u64>,

    /// Maximum time to wait for a streamed completion event from the language
    /// model before treating the stream as dead and retrying the request.
    /// Guards against providers that accept the HTTP connection but then
    /// never deliver SSE events, which would otherwise wedge the thread in
    /// `Generating` until the auto-prompt watchdog intervenes. The timeout
    /// only fires when no tool results are pending; while tools run, a silent
    /// stream is legitimate. Set to 0 to disable.
    ///
    /// Default: 120
    pub stream_idle_timeout_secs: Option<u64>,

    /// Whether external agents (ACP) are offered the subagents capability, so
    /// subagents they launch, including Claude Code's background agents,
    /// appear in the thread as cards with a live running / done status.
    ///
    /// Negotiated when an agent connection starts, so a change applies to
    /// agents started afterwards.
    ///
    /// Default: true
    pub external_subagents: Option<bool>,
    /// Minutes between orchestrator watchdog ticks on the `agent-mail`
    /// orchestrator rig (plan 048). Applied to that rig whenever it changes;
    /// unset leaves the rig's own setting (default 10) alone.
    ///
    /// Default: null
    pub agent_mail_watch_interval_minutes: Option<u64>,
}

/// Which backend provides the reviewer thread for the `request_verdict` tool.
#[derive(Clone, Default, Debug, Serialize, Deserialize, JsonSchema, MergeFrom, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VerdictReviewerSetting {
    /// Subagent pinned to `verdict_model` (inherits parent model when unset).
    /// Opt in via `agent.verdict_reviewer = "native"`.
    Native,
    /// Off-screen Claude Code session on its existing connection — uses
    /// Claude Code's own subscription auth, no Anthropic API key required.
    /// Default. Requires Claude Code connected in the agent panel; errors
    /// honestly when absent — never falls back to the worker's own model.
    #[default]
    ClaudeCode,
}
