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

    /// Token count threshold (approximate) at which context is considered too large
    /// and the system forces a "continue" prompt instead of asking the LLM.
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,

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
}

fn default_max_iterations() -> u32 {
    20
}

fn default_max_context_tokens() -> usize {
    80_000
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

impl Default for AutoPromptConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: default_max_iterations(),
            max_context_tokens: default_max_context_tokens(),
            backoff_base_ms: default_backoff_base_ms(),
            max_verification_attempts: default_max_verification_attempts(),
            max_llm_retries: default_max_llm_retries(),
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

        Self {
            system_prompt,
            max_iterations,
            max_context_tokens,
            backoff_base_ms,
            max_verification_attempts,
            max_llm_retries,
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
