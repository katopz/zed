use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the auto-prompt hook.
///
/// Loaded from `~/.config/zed/auto_prompt.json` or environment variables.
/// The LLM used is whatever Zed has configured as the default model.
///
/// Enable/disable is controlled by the UI toggle in the agent panel toolbar,
/// not by this config file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoPromptConfig {
    /// Optional system prompt to use when calling the LLM.
    /// Defaults to a built-in prompt that instructs the model to return JSON.
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// Maximum number of auto-prompt iterations before hard-stopping the loop.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,

    /// Token count threshold (approximate) at which the summarize→new-thread
    /// flow takes over (plan 023). Below it the chain always answers in the
    /// same thread (req 4); above it Phase 1 asks for a summary on the same
    /// thread and Phase 2 forks a continuation thread.
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,

    /// Claude (ACP) threads above this many input tokens join the native
    /// Phase 1/2 summarize→fork flow instead of relying on Claude Code's
    /// internal compaction alone (plan 023 A3, req 1). Below it, Claude
    /// always continues in the same thread.
    #[serde(default = "default_claude_context_overflow_tokens")]
    pub claude_context_overflow_tokens: usize,

    /// Base delay in milliseconds for exponential backoff on errors.
    /// Actual delay = backoff_base_ms * 2^retry_count (capped at 60s).
    #[serde(default = "default_backoff_base_ms")]
    pub backoff_base_ms: u64,

    /// Maximum number of pre-stop verification attempts before forcing a stop.
    /// When the LLM says stop, we verify (plans done, diagnostics clean, git committed).
    /// If verification fails, we retry up to this many times before forcing stop.
    #[serde(default = "default_max_verification_attempts")]
    pub max_verification_attempts: u32,

    /// Maximum number of automatic retry attempts for LLM orchestration call failures.
    /// When the auto-prompt's own LLM call fails (network/timeout/parse), it will
    /// retry with exponential backoff up to this many times before showing "Retry" button.
    #[serde(default = "default_max_llm_retries")]
    pub max_llm_retries: u32,

    /// Token count threshold below which auto-prompt continues in the same thread
    /// instead of creating a new thread with summary. Only applies to native Zed agent.
    /// When the actual input token count exceeds this value, a new thread is created.
    ///
    /// `0` (the default) = "auto": since plan 023 the auto value resolves to
    /// `max_context_tokens` (256k default) — below the overflow gate the chain
    /// always continues same-thread, and the Phase 1/2 machinery owns forking
    /// above it. Any positive value overrides this with a fixed threshold.
    #[serde(default = "default_same_thread_token_threshold")]
    pub same_thread_token_threshold: usize,

    /// Extra margin (seconds) added to a parsed session-limit reset time
    /// before auto-prompt dispatches the continuation. Claude reports e.g.
    /// "resets 1:20am (Asia/Bangkok)"; we schedule at reset + this margin
    /// (default 60s) to avoid racing the enforcement window.
    #[serde(default = "default_session_limit_margin_secs")]
    pub session_limit_margin_secs: u64,

    /// Watchdog: seconds the worker thread may stay in `Generating` without
    /// stopping before a reasoning LLM is asked whether to keep waiting or
    /// halt. The watchdog is the only mechanism that recovers from a worker
    /// LLM stream hang — `on_thread_stopped` never fires in that case, so all
    /// other auto-prompt timeouts are unreachable.
    ///
    /// On each expiry a headless LLM sees the last tool call (input + output),
    /// the last assistant message, the cumulative elapsed time, and which
    /// timeout number this is. `continue` reschedules for another window;
    /// `halt` cancels the worker and injects a timeout notice into the same
    /// thread so the worker can recover (retry / change approach / stop).
    #[serde(default = "default_watchdog_timeout_secs")]
    pub watchdog_timeout_secs: u64,

    /// Whether the stuck-thread watchdog is active. Disable to revert to the
    /// pre-watchdog behaviour (a hung worker stream stalls forever).
    #[serde(default = "default_watchdog_enabled")]
    pub watchdog_enabled: bool,

    /// Whether Phase 2 (context-overflow continuation) prompts are authored
    /// by an LLM reasoning pass over the summary (default) instead of the
    /// deterministic rule-based chain. The rule-based chain always remains
    /// as the fallback when the reasoning call fails, times out, or is
    /// disabled here — overflow must never get more stuck than without it.
    #[serde(default = "default_reasoned_phase2_enabled")]
    pub reasoned_phase2_enabled: bool,

    /// Whether decision-form elicitations (`ask_user` / ACP session
    /// elicitation) are auto-answered on threads whose auto-prompt is
    /// enabled: an LLM reasoning pass picks the option (with a one-line
    /// rationale in the free-text field) and a countdown backstop selects
    /// the first option when reasoning fails or is slow. Without this, a
    /// worker blocked on a decision form stalls the whole chain until a
    /// human answers.
    #[serde(default = "default_elicitation_auto_answer_enabled")]
    pub elicitation_auto_answer_enabled: bool,

    /// Countdown before the first-option backstop fires on an unanswered
    /// decision form. Reasoned answers arriving earlier win.
    #[serde(default = "default_elicitation_countdown_secs")]
    pub elicitation_countdown_secs: u64,

    /// Slash command / skill dispatched once when an automatic chain stops
    /// with no remaining tasks (plan 023 E, req 6) — e.g. a housekeeping
    /// skill that syncs docs. Availability-checked against the thread's
    /// agent commands/skills before sending; an unresolvable command logs
    /// and stops normally, never failing the chain. `None` (or an empty
    /// string in config/env) disables the hook.
    #[serde(default = "default_housekeeping_command")]
    pub housekeeping_command: Option<String>,
}

fn default_max_iterations() -> u32 {
    20
}

pub fn default_max_context_tokens() -> usize {
    // Plan 023 A1 gate, retuned 2026-08-23: below it same-thread (req 4),
    // above it the summarize→fork dance (req 3). The 256k value let worker
    // threads balloon to 343–413k actual input tokens before the gate could
    // fire (it only runs at turn end), and GLM requests at those sizes are
    // exactly the ones that stall (see .docs/009). 200k forks earlier and
    // keeps decide calls small; override with ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS.
    200_000
}

fn default_claude_context_overflow_tokens() -> usize {
    320_000
}

fn default_backoff_base_ms() -> u64 {
    2_000
}

fn default_max_verification_attempts() -> u32 {
    2
}

fn default_max_llm_retries() -> u32 {
    3
}

fn default_same_thread_token_threshold() -> usize {
    // 0 means "auto" (50% of model max input tokens, capped at 100k); see dispatch_action.
    0
}

fn default_session_limit_margin_secs() -> u64 {
    crate::session_limit::DEFAULT_SESSION_LIMIT_MARGIN_SECS
}

fn default_watchdog_timeout_secs() -> u64 {
    1800
}

fn default_watchdog_enabled() -> bool {
    true
}

fn default_housekeeping_command() -> Option<String> {
    Some("housekeeping".to_string())
}

fn default_reasoned_phase2_enabled() -> bool {
    true
}

fn default_elicitation_auto_answer_enabled() -> bool {
    true
}

fn default_elicitation_countdown_secs() -> u64 {
    60
}

impl Default for AutoPromptConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: default_max_iterations(),
            max_context_tokens: default_max_context_tokens(),
            backoff_base_ms: default_backoff_base_ms(),
            max_verification_attempts: default_max_verification_attempts(),
            max_llm_retries: default_max_llm_retries(),
            same_thread_token_threshold: default_same_thread_token_threshold(),
            session_limit_margin_secs: default_session_limit_margin_secs(),
            watchdog_timeout_secs: default_watchdog_timeout_secs(),
            watchdog_enabled: default_watchdog_enabled(),
            claude_context_overflow_tokens: default_claude_context_overflow_tokens(),
            housekeeping_command: default_housekeeping_command(),
            reasoned_phase2_enabled: default_reasoned_phase2_enabled(),
            elicitation_auto_answer_enabled: default_elicitation_auto_answer_enabled(),
            elicitation_countdown_secs: default_elicitation_countdown_secs(),
        }
    }
}

impl AutoPromptConfig {
    /// Returns the path to the config file: `~/.config/zed/auto_prompt.json`
    pub fn config_path() -> Result<PathBuf> {
        let config_dir = paths::config_dir();
        Ok(config_dir.join("auto_prompt.json"))
    }

    /// Load config from file, falling back to environment variables.
    pub fn load() -> Result<Self> {
        log::info!("[auto_prompt::config] Loading config...");
        let path = Self::config_path()?;
        log::info!("[auto_prompt::config] Config path: {:?}", path);

        if path.exists() {
            log::info!("[auto_prompt::config] Config file exists, loading from file");
            let content = std::fs::read_to_string(&path)?;
            let config: Self = serde_json::from_str(&content)?;
            log::info!(
                "[auto_prompt::config] Loaded from file: max_iterations={}",
                config.max_iterations
            );
            return Ok(config);
        }

        log::info!(
            "[auto_prompt::config] Config file not found, loading from environment variables"
        );
        let config = Self::from_env();
        log::info!(
            "[auto_prompt::config] Loaded from env: max_iterations={}",
            config.max_iterations
        );
        Ok(config)
    }

    /// Build config from environment variables.
    fn from_env() -> Self {
        let system_prompt = std::env::var("ZED_AUTO_PROMPT_SYSTEM_PROMPT").ok();

        let max_iterations = std::env::var("ZED_AUTO_PROMPT_MAX_ITERATIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_max_iterations);

        let max_context_tokens = std::env::var("ZED_AUTO_PROMPT_MAX_CONTEXT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_max_context_tokens);

        let claude_context_overflow_tokens =
            std::env::var("ZED_AUTO_PROMPT_CLAUDE_CONTEXT_OVERFLOW_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_claude_context_overflow_tokens);

        let backoff_base_ms = std::env::var("ZED_AUTO_PROMPT_BACKOFF_BASE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_backoff_base_ms);

        let max_verification_attempts = std::env::var("ZED_AUTO_PROMPT_MAX_VERIFICATION_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_max_verification_attempts);

        let max_llm_retries = std::env::var("ZED_AUTO_PROMPT_MAX_LLM_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_max_llm_retries);

        let same_thread_token_threshold =
            std::env::var("ZED_AUTO_PROMPT_SAME_THREAD_TOKEN_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_same_thread_token_threshold);

        let session_limit_margin_secs = std::env::var("ZED_AUTO_PROMPT_SESSION_LIMIT_MARGIN_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_session_limit_margin_secs);

        let watchdog_timeout_secs = std::env::var("ZED_AUTO_PROMPT_WATCHDOG_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(default_watchdog_timeout_secs);

        let watchdog_enabled = std::env::var("ZED_AUTO_PROMPT_WATCHDOG_ENABLED")
            .ok()
            .map(|v| !matches!(v.as_str(), "0" | "false"))
            .unwrap_or_else(default_watchdog_enabled);

        // Set + non-empty → that command; set + empty → explicitly disabled;
        // unset → default ("housekeeping").
        let housekeeping_command = match std::env::var("ZED_AUTO_PROMPT_HOUSEKEEPING_COMMAND") {
            Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
            Ok(_) => None,
            Err(_) => default_housekeeping_command(),
        };

        let reasoned_phase2_enabled = std::env::var("ZED_AUTO_PROMPT_REASONED_PHASE2_ENABLED")
            .ok()
            .map(|v| !matches!(v.as_str(), "0" | "false"))
            .unwrap_or_else(default_reasoned_phase2_enabled);

        let elicitation_auto_answer_enabled =
            std::env::var("ZED_AUTO_PROMPT_ELICITATION_AUTO_ANSWER_ENABLED")
                .ok()
                .map(|v| !matches!(v.as_str(), "0" | "false"))
                .unwrap_or_else(default_elicitation_auto_answer_enabled);

        let elicitation_countdown_secs =
            std::env::var("ZED_AUTO_PROMPT_ELICITATION_COUNTDOWN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_elicitation_countdown_secs);

        Self {
            system_prompt,
            max_iterations,
            max_context_tokens,
            backoff_base_ms,
            max_verification_attempts,
            max_llm_retries,
            same_thread_token_threshold,
            session_limit_margin_secs,
            watchdog_timeout_secs,
            watchdog_enabled,
            claude_context_overflow_tokens,
            housekeeping_command,
            reasoned_phase2_enabled,
            elicitation_auto_answer_enabled,
            elicitation_countdown_secs,
        }
    }

    /// Calculate backoff delay for a given retry count.
    /// Capped at 60 seconds.
    pub fn backoff_delay_ms(&self, retry_count: u32) -> u64 {
        let capped_retry = retry_count.min(5);
        let delay = self.backoff_base_ms * 2u64.pow(capped_retry);
        delay.min(60_000)
    }

    /// Write current config to the config file.
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;

        // Invalidate cache so next load picks up the new config
        crate::invalidate_config_cache();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_context_tokens_defaults_to_200k() {
        // Plan 023 A1 (req 3/4): the gate — below it same-thread, above it
        // the summarize→fork flow. 200k per .docs/009 (256k let threads
        // balloon to 343–413k before forking).
        assert_eq!(default_max_context_tokens(), 200_000);
        assert_eq!(AutoPromptConfig::default().max_context_tokens, 200_000);
    }

    #[test]
    fn claude_context_overflow_tokens_defaults_to_320k() {
        // Plan 023 A3 (req 1): Claude joins the native overflow flow above 320k.
        assert_eq!(default_claude_context_overflow_tokens(), 320_000);
        assert_eq!(
            AutoPromptConfig::default().claude_context_overflow_tokens,
            320_000
        );
    }

    #[test]
    fn reasoned_phase2_defaults_to_enabled() {
        // The LLM reasoning pass is the default Phase 2 author; the
        // rule-based chain is the fallback (and the flag-off mode).
        assert!(default_reasoned_phase2_enabled());
        assert!(AutoPromptConfig::default().reasoned_phase2_enabled);
    }

    #[test]
    fn elicitation_auto_answer_defaults() {
        assert!(default_elicitation_auto_answer_enabled());
        assert_eq!(default_elicitation_countdown_secs(), 60);
        assert!(AutoPromptConfig::default().elicitation_auto_answer_enabled);
        assert_eq!(
            AutoPromptConfig::default().elicitation_countdown_secs,
            60
        );
    }

    #[test]
    fn housekeeping_command_defaults_to_housekeeping() {
        // Plan 023 E (req 6): default command name, overridable/disablable.
        assert_eq!(
            default_housekeeping_command(),
            Some("housekeeping".to_string())
        );
        assert_eq!(
            AutoPromptConfig::default().housekeeping_command,
            Some("housekeeping".to_string())
        );
    }

    #[test]
    fn serde_missing_fields_fall_back_to_defaults() {
        let config: AutoPromptConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.max_context_tokens, 200_000);
        assert_eq!(config.claude_context_overflow_tokens, 320_000);
        assert_eq!(
            config.housekeeping_command,
            Some("housekeeping".to_string())
        );
    }

    #[test]
    fn serde_null_housekeeping_command_disables() {
        let config: AutoPromptConfig =
            serde_json::from_str(r#"{"housekeeping_command": null}"#).unwrap();
        assert_eq!(config.housekeeping_command, None);
    }
}
