//! Fork-owned agent settings (plan 038), kept out of `agent_settings.rs` so
//! upstream syncs don't conflict on them.

use settings::{ForkAgentSettingsContent, LanguageModelSelection, VerdictReviewerSetting};

/// Default for `AgentSettings::stream_idle_timeout_secs`.
pub const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

/// Default for `AgentSettings::verdict_max_rounds` (proposal 001).
pub const DEFAULT_VERDICT_MAX_ROUNDS: usize = 3;

#[derive(Clone, Debug)]
pub struct ForkAgentSettings {
    /// Whether the `request_verdict` tool is registered for native agent
    /// threads (verdict ping-pong, proposal 001). User-invoked only; set
    /// false to opt out while the GOAT benchmark runs.
    pub verdict_ping_pong: bool,
    /// Model for the `request_verdict` reviewer subagent. `None` inherits the
    /// parent thread's model.
    pub verdict_model: Option<LanguageModelSelection>,
    /// Hard cap on `request_verdict` rounds per negotiation.
    pub verdict_max_rounds: usize,
    /// Which backend provides the reviewer thread for `request_verdict`.
    pub verdict_reviewer: VerdictReviewerSetting,
    /// Model for the secondary verdict entry (`reviewer: "secondary"`).
    /// `None` hides that entry.
    pub verdict_secondary_model: Option<LanguageModelSelection>,
    pub claude_md_symlink: bool,
    pub project_claude_md_symlink: bool,
    /// Whether auto_prompt should steal keyboard focus when it creates a new
    /// background continuation thread. Default: false (don't steal focus).
    pub auto_focus_new_thread: bool,
    /// When set, tool permission prompts are automatically answered with the
    /// least-privileged "allow" option after this many seconds. Default:
    /// None (prompts wait indefinitely).
    pub auto_allow_permissions_after_seconds: Option<u64>,
    /// Maximum time to wait for a streamed completion event before treating
    /// the stream as dead and retrying. 0 disables the timeout. Default: 120
    /// seconds — long enough for time-to-first-token on very large contexts,
    /// short enough that a wedged stream recovers without the watchdog.
    pub stream_idle_timeout_secs: u64,
    /// Whether external agents are offered the ACP subagents capability.
    pub external_subagents: bool,
    /// Watchdog interval pushed to the agent-mail orchestrator rig (plan 048).
    pub agent_mail_watch_interval_minutes: Option<u64>,
}

impl ForkAgentSettings {
    pub(crate) fn from_content(content: ForkAgentSettingsContent) -> Self {
        Self {
            verdict_ping_pong: content.verdict_ping_pong.unwrap_or(true),
            verdict_model: content.verdict_model,
            verdict_max_rounds: content
                .verdict_max_rounds
                .unwrap_or(DEFAULT_VERDICT_MAX_ROUNDS),
            verdict_reviewer: content.verdict_reviewer.unwrap_or_default(),
            verdict_secondary_model: content.verdict_secondary_model,
            claude_md_symlink: content.claude_md_symlink.unwrap_or(true),
            project_claude_md_symlink: content.project_claude_md_symlink.unwrap_or(false),
            auto_focus_new_thread: content.auto_focus_new_thread.unwrap_or(false),
            auto_allow_permissions_after_seconds: content.auto_allow_permissions_after_seconds,
            stream_idle_timeout_secs: content
                .stream_idle_timeout_secs
                .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_SECS),
            external_subagents: content.external_subagents.unwrap_or(true),
            agent_mail_watch_interval_minutes: content.agent_mail_watch_interval_minutes,
        }
    }
}
