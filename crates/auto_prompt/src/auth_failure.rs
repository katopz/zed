//! Detection of turns that ended because the agent could not authenticate
//! with its provider (expired/revoked OAuth session, invalid API key), e.g.
//! Claude Code's `Failed to authenticate: OAuth session expired and could not
//! be refreshed`. Read from the same two sources as
//! [`crate::api_unreachable`]: the turn-level API error and the synthetic
//! assistant message that ends the turn.
//!
//! Unlike an unreachable API, waiting does not help: every continuation, the
//! orchestrator call and the overflow summarize request all need the same
//! credentials, and only the user can re-login. The chain stops immediately —
//! without ever entering `Processing` — and the user is told to sign in.

use acp_thread::AcpThread;
use gpui::App;

/// Lowercased credential-failure markers (Claude Code phrasing and the
/// Anthropic API `authentication_error` type).
const AUTH_MARKERS: [&str; 10] = [
    "failed to authenticate",
    "authentication_error",
    "oauth session expired",
    "oauth token has expired",
    "oauth token revoked",
    "could not be refreshed",
    "invalid api key",
    "invalid x-api-key",
    "please run /login",
    "not logged in",
];

/// Lowercased line prefixes Claude Code uses for synthetic auth failures.
const SYNTHETIC_AUTH_PREFIXES: [&str; 5] = [
    "api error",
    "failed to authenticate",
    "invalid api key",
    "oauth",
    "not logged in",
];

/// Whether an error text names a credential failure.
pub fn is_auth_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    AUTH_MARKERS.iter().any(|marker| lowered.contains(marker))
}

/// Whether an assistant message is a synthetic credential failure. Only the
/// last non-empty line counts, and it must start like an error — prose that
/// merely discusses OAuth or `/login` must never stop the chain.
pub fn is_auth_failure_message(text: &str) -> bool {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .is_some_and(|line| {
            let lowered = line.to_ascii_lowercase();
            SYNTHETIC_AUTH_PREFIXES
                .iter()
                .any(|prefix| lowered.starts_with(prefix))
                && is_auth_error(line)
        })
}

/// The credential failure that ended the thread's latest turn, if any.
pub fn auth_failure_from_thread(thread: &AcpThread, cx: &App) -> Option<String> {
    crate::api_unreachable::latest_turn_failure(thread, cx, is_auth_error, is_auth_failure_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_claude_code_auth_messages() {
        assert!(is_auth_failure_message(
            "Failed to authenticate: OAuth session expired and could not be refreshed"
        ));
        assert!(is_auth_failure_message(
            "Working on it.\n\nFailed to authenticate. API Error: 401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"OAuth token has expired.\"}}\n"
        ));
        assert!(is_auth_failure_message(
            "Invalid API key · Please run /login"
        ));
    }

    #[test]
    fn ignores_prose_and_non_auth_errors() {
        assert!(!is_auth_failure_message(
            "Fixed the OAuth session expired handling.\n\n## Summary\ndone"
        ));
        assert!(!is_auth_failure_message(
            "Failed to authenticate: OAuth session expired\n\nRetried and continuing the task."
        ));
        assert!(!is_auth_failure_message(
            "API Error: Can't reach the API server (ENOTFOUND)"
        ));
    }

    #[test]
    fn detects_turn_level_auth_errors() {
        assert!(is_auth_error(
            "Failed to authenticate: OAuth session expired and could not be refreshed"
        ));
        assert!(is_auth_error(
            "401 {\"type\":\"error\",\"error\":{\"type\":\"authentication_error\"}}"
        ));
        assert!(!is_auth_error("429 Too Many Requests"));
    }
}
