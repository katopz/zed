//! Detection of turns that ended because the provider API was unreachable
//! (offline, DNS failure, connection refused), from two sources:
//!
//! 1. **Turn-level error** — `AcpThread::last_api_error`, set when the
//!    completion request itself failed (native agents such as GLM surface the
//!    HTTP client's `error sending request … dns error: failed to lookup
//!    address information`).
//! 2. **Synthetic message** — Claude Code reports exhausted connectivity
//!    retries as an assistant message ending the turn normally, e.g.
//!    `API Error: Can't reach the API server — check your internet or DNS
//!    (ENOTFOUND)`, so `had_api_error` stays false.
//!
//! Every downstream path (orchestrator call, overflow Phase 1 summarize
//! request, rules-based summary handoff) needs the same unreachable API, and a
//! rules-based fallback has no fresh information to act on. The only useful
//! move is to wait and retry the same thread, with exponential backoff that
//! grows across consecutive unreachable turns.

use std::sync::atomic::{AtomicU32, Ordering};

use acp_thread::{AcpThread, AgentThreadEntry};
use gpui::App;

/// Lowercased connectivity markers: Node errno codes (Claude Code), Claude
/// Code's own phrasing, and the Rust HTTP client's resolver/connect errors.
const NETWORK_MARKERS: [&str; 15] = [
    "enotfound",
    "eai_again",
    "econnrefused",
    "econnreset",
    "etimedout",
    "enetunreach",
    "ehostunreach",
    "can't reach the api server",
    "check your internet",
    "connection error",
    "fetch failed",
    "dns error",
    "failed to lookup address",
    "network is unreachable",
    "error sending request",
];

/// Claude Code prefixes its synthetic failure messages with this.
const SYNTHETIC_ERROR_PREFIX: &str = "API Error";

static UNREACHABLE_STREAK: AtomicU32 = AtomicU32::new(0);

/// Whether an error text names a connectivity failure.
pub fn is_network_error(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    NETWORK_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Whether an assistant message is Claude Code's synthetic connectivity
/// failure. Only the last non-empty line counts, and it must carry the
/// `API Error` prefix — prose that merely discusses DNS or ENOTFOUND must
/// never be mistaken for a dead connection.
pub fn is_unreachable_message(text: &str) -> bool {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .is_some_and(|line| line.starts_with(SYNTHETIC_ERROR_PREFIX) && is_network_error(line))
}

/// The connectivity error that ended the thread's latest turn, if any.
pub fn unreachable_error_from_thread(thread: &AcpThread, cx: &App) -> Option<String> {
    if thread.had_api_error()
        && let Some(error) = thread.last_api_error()
        && is_network_error(error)
    {
        return Some(error.to_string());
    }
    // The synthetic message must answer the latest user message; an older
    // one followed by a newer prompt is stale.
    let latest_is_assistant = thread
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            AgentThreadEntry::UserMessage(_) => Some(false),
            AgentThreadEntry::AssistantMessage(_) => Some(true),
            _ => None,
        })
        .unwrap_or(false);
    if !latest_is_assistant {
        return None;
    }
    thread
        .last_assistant_message_text(cx)
        .filter(|message| is_unreachable_message(message))
}

/// Record one more consecutive unreachable turn; returns the streak length,
/// used as the backoff exponent.
pub fn record_unreachable() -> u32 {
    UNREACHABLE_STREAK
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1)
}

/// Clear the streak once a turn ends without a connectivity failure.
pub fn reset_unreachable_streak() {
    UNREACHABLE_STREAK.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_claude_code_synthetic_message() {
        assert!(is_unreachable_message(
            "API Error: Can't reach the API server — check your internet or DNS (ENOTFOUND)"
        ));
        assert!(is_unreachable_message(
            "Working on the plan now.\n\nAPI Error: Connection error.\n"
        ));
    }

    #[test]
    fn ignores_prose_and_non_network_api_errors() {
        assert!(!is_unreachable_message(
            "Fixed the ENOTFOUND handling in the DNS resolver.\n\n## Summary\ndone"
        ));
        assert!(!is_unreachable_message(
            "API Error: ENOTFOUND was mentioned earlier\n\nNow continuing the task."
        ));
        assert!(!is_unreachable_message(
            "API Error: 400 {\"type\":\"invalid_request_error\"}"
        ));
    }

    #[test]
    fn detects_native_http_client_errors() {
        assert!(is_network_error(
            "error sending request for url (https://api.z.ai/api/paas/v4/chat/completions)"
        ));
        assert!(is_network_error(
            "dns error: failed to lookup address information: nodename nor servname provided"
        ));
        assert!(!is_network_error("429 Too Many Requests"));
    }
}
