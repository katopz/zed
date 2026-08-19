//! Mention protocol for the war room (Plan 024 P1).
//!
//! A message **routes** to an agent iff its first token matches
//! `@<device_name>:<session_prefix4>` — e.g. `@SHIKUWA:b1c9 run cargo clippy`.
//! Mentions mid-text are display-highlighted only (no injection); this keeps
//! routing deterministic and the parse O(1). `@all` broadcast is deferred.
//!
//! Injection reuses the exact Plan-015 reply pipeline
//! (`inject_web_reply` → agent_panel drain → `AcpThread::send`), so there is
//! no new delivery path — only a new producer.
//!
//! Loop guards (agent ↔ agent ping-pong) live here too: a per-target cooldown
//! plus an hourly cap, checked at injection time. The watermark and guard are
//! process-globals because BOTH the 15s poll path and the SSE push path scan
//! for mentions and must share one high-water mark and one rate budget.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::types::BoardMessage;

/// A parsed `@device:prefix text` mention. Borrows from the source message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMention<'a> {
    pub device: &'a str,
    pub prefix: &'a str,
    pub text: &'a str,
}

/// Outcome of scanning one board message for a routing mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// No first-token mention targeting us — nothing to do.
    NotRouted,
    /// The poster is the target itself (`sender == device:prefix`) — dropped.
    SelfMention,
    /// Routed to one of our local sessions.
    Routed,
}

/// A mention that targets this device, ready for guard + injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionRoute {
    /// First 4 chars of the target session id (the Plan 015 routing key).
    pub prefix: String,
    /// Command text after the mention token.
    pub text: String,
    /// `message.sender` when set, else `message.device_name` — used both for
    /// the injected `📢 war-room [@sender]` label and self-mention detection.
    pub sender_label: String,
    /// Unix millis of the source message.
    pub ts: i64,
}

/// Parse `@device:prefix4 text` from the FIRST token of `message`.
///
/// Returns `None` when the message does not start with a well-formed mention
/// (mid-text mentions, `@all`, bad prefix lengths, or empty command text).
pub fn parse_mention(message: &str) -> Option<ParsedMention<'_>> {
    let rest = message.strip_prefix('@')?;
    let colon = rest.find(':')?;
    let device = &rest[..colon];
    if device.is_empty()
        || !device
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let after = &rest[colon + 1..];
    // The prefix is exactly 4 ASCII alphanumerics followed by a boundary
    // (end-of-string or non-alphanumeric). Counting ASCII chars keeps every
    // slice below on a char boundary.
    let prefix_len = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count()
        .min(4);
    if prefix_len != 4 {
        return None;
    }
    debug_assert!(after.is_char_boundary(4));
    let after_prefix = &after[4..];
    if after_prefix
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let text = after_prefix.trim();
    if text.is_empty() {
        // A bare mention token is a display highlight, not a command.
        return None;
    }
    Some(ParsedMention {
        device,
        prefix: &after[..4],
        text,
    })
}

/// Scan one message for a routing mention targeting `device_name`.
///
/// Pure: no globals touched. Returns the outcome plus, when routed, the
/// [`MentionRoute`] the caller should push through the guard.
pub fn scan_message_for_device(
    message: &BoardMessage,
    device_name: &str,
) -> (ScanOutcome, Option<MentionRoute>) {
    let Some(mention) = parse_mention(&message.text) else {
        return (ScanOutcome::NotRouted, None);
    };
    if mention.device != device_name {
        return (ScanOutcome::NotRouted, None);
    }
    let sender_label = sender_label(message);
    if sender_label == format!("{device_name}:{}", mention.prefix) {
        return (ScanOutcome::SelfMention, None);
    }
    (
        ScanOutcome::Routed,
        Some(MentionRoute {
            prefix: mention.prefix.to_string(),
            text: mention.text.to_string(),
            sender_label,
            ts: message.ts,
        }),
    )
}

// ---------------------------------------------------------------------------
// Loop guard — cooldown + hourly cap per target session.
// ---------------------------------------------------------------------------

/// Decision returned by [`MentionGuard::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionDecision {
    Allow,
    SuppressCooldown,
    SuppressRateCap,
}

/// Per-target-session injection budget. Pure state machine — the process
/// global below wraps it so the poll and SSE paths share one budget.
#[derive(Debug)]
pub struct MentionGuard {
    cooldown_ms: i64,
    max_per_hour: u32,
    last_injection_ms: HashMap<String, i64>,
    /// target -> (hour-start unix ms, injections in that hour)
    hour_counts: HashMap<String, (i64, u32)>,
}

const HOUR_MS: i64 = 60 * 60 * 1000;

impl MentionGuard {
    pub fn new(cooldown_secs: u64, max_per_hour: u32) -> Self {
        Self {
            cooldown_ms: cooldown_secs as i64 * 1000,
            max_per_hour,
            last_injection_ms: HashMap::new(),
            hour_counts: HashMap::new(),
        }
    }

    /// Check (and, when allowed, consume) one injection slot for `target`.
    pub fn check(&mut self, target: &str, now_ms: i64) -> MentionDecision {
        if let Some(&last) = self.last_injection_ms.get(target) {
            if now_ms.saturating_sub(last) < self.cooldown_ms {
                return MentionDecision::SuppressCooldown;
            }
        }
        let hour_start = now_ms - now_ms % HOUR_MS;
        let entry = self
            .hour_counts
            .entry(target.to_string())
            .or_insert((hour_start, 0));
        if entry.0 != hour_start {
            *entry = (hour_start, 0);
        }
        if entry.1 >= self.max_per_hour {
            return MentionDecision::SuppressRateCap;
        }
        entry.1 += 1;
        self.last_injection_ms.insert(target.to_string(), now_ms);
        MentionDecision::Allow
    }
}

// ---------------------------------------------------------------------------
// Process globals — shared by the 15s poll path and the SSE push path.
// ---------------------------------------------------------------------------

/// High-water mark over scanned message timestamps. In-memory only: after a
/// restart the first round re-scans the visible feed, re-injecting pending
/// mentions (bounded by the cooldown) — documented, acceptable per Plan 024.
static MENTION_WATERMARK_MS: AtomicI64 = AtomicI64::new(0);

static MENTION_GUARD: Mutex<Option<MentionGuard>> = Mutex::new(None);

/// Mentions injected since the war room panel was last opened. Rendered as the
/// panel icon label; cleared by the Toggle/ToggleFocus handlers.
static UNWATCHED_MENTIONS: AtomicUsize = AtomicUsize::new(0);

/// Configure the process-global guard. Called once by the runtime at start.
pub fn configure_guard(cooldown_secs: u64, max_per_hour: u32) {
    if let Ok(mut guard) = MENTION_GUARD.lock() {
        *guard = Some(MentionGuard::new(cooldown_secs, max_per_hour));
    }
}

/// Current watermark (unix millis). Messages with `ts <= watermark` were
/// already scanned and must not be re-processed.
pub fn watermark_ms() -> i64 {
    MENTION_WATERMARK_MS.load(Ordering::Relaxed)
}

/// Advance the watermark to `ts` (monotonic — never moves backwards).
pub fn advance_watermark(ts: i64) {
    MENTION_WATERMARK_MS.fetch_max(ts, Ordering::Relaxed);
}

/// Check the shared guard for one injection slot. When allowed, the caller
/// still performs the actual `inject_web_reply` + counter bump.
pub fn try_acquire_injection(target: &str, now_ms: i64) -> MentionDecision {
    match MENTION_GUARD.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(guard) => guard.check(target, now_ms),
            None => MentionDecision::Allow,
        },
        Err(_) => MentionDecision::SuppressCooldown,
    }
}

/// Count of mention-injections not yet seen by the operator.
pub fn unwatched_mention_count() -> usize {
    UNWATCHED_MENTIONS.load(Ordering::Relaxed)
}

/// Record that an injection happened (bumps the unwatched counter).
pub fn record_injection() {
    UNWATCHED_MENTIONS.fetch_add(1, Ordering::Relaxed);
}

/// Clear the unwatched counter — called when the war room panel is opened.
pub fn clear_unwatched_mentions() {
    UNWATCHED_MENTIONS.store(0, Ordering::Relaxed);
}

/// Push one routed mention through the shared guard and, when allowed, into
/// the Plan-015 reply pipeline. Shared by the poll and SSE paths.
pub fn inject_route(route: &MentionRoute) -> MentionDecision {
    let decision = try_acquire_injection(&route.prefix, route.ts);
    match decision {
        MentionDecision::Allow => {
            let text = format!("📢 war-room [@{}] {}", route.sender_label, route.text);
            auto_prompt::peer_states::inject_web_reply(route.prefix.clone(), text);
            record_injection();
        }
        // Still visible in the feed — the operator sees the storm even when
        // injection is suppressed.
        MentionDecision::SuppressCooldown | MentionDecision::SuppressRateCap => {
            log::warn!(
                "[agent_board] mention to {} suppressed ({decision:?}): {}",
                route.prefix,
                route.text
            );
        }
    }
    decision
}

/// Full per-message pipeline for push delivery (SSE): watermark check → scan
/// → self-mention drop → guard → inject. The poll path uses
/// `feeder::extract_mentions_for_device` + `inject_route` instead, sharing the
/// same watermark and guard globals, so neither path double-delivers.
pub fn handle_board_message(message: &BoardMessage, device_name: &str) -> ScanOutcome {
    if message.ts <= watermark_ms() {
        return ScanOutcome::NotRouted;
    }
    let (outcome, route) = scan_message_for_device(message, device_name);
    advance_watermark(message.ts);
    if let ScanOutcome::SelfMention = outcome {
        log::debug!(
            "[agent_board] dropping self-mention from {}: {}",
            message.sender,
            message.text
        );
    }
    if let Some(route) = route {
        inject_route(&route);
    }
    outcome
}

/// Label describing who posted a message, for feed rendering.
pub fn sender_label(message: &BoardMessage) -> String {
    if message.sender.is_empty() {
        message.device_name.clone()
    } else {
        message.sender.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str, sender: &str, device_name: &str, ts: i64) -> BoardMessage {
        BoardMessage {
            v: 1,
            device_id: String::new(),
            device_name: device_name.to_string(),
            sender: sender.to_string(),
            text: text.to_string(),
            ts,
        }
    }

    // ── parse_mention ──

    #[test]
    fn parse_valid_mention() {
        let parsed = parse_mention("@SHIKUWA:b1c9 run cargo clippy").unwrap();
        assert_eq!(parsed.device, "SHIKUWA");
        assert_eq!(parsed.prefix, "b1c9");
        assert_eq!(parsed.text, "run cargo clippy");
    }

    #[test]
    fn parse_mid_text_mention_is_not_routed() {
        // Mentions not in the first token are display-only.
        assert!(parse_mention("hey @m3:f3a2 do something").is_none());
        assert!(parse_mention("please @m3:f3a2").is_none());
    }

    #[test]
    fn parse_at_all_is_deferred() {
        assert!(parse_mention("@all stop everything").is_none());
    }

    #[test]
    fn parse_bad_prefix_length() {
        assert!(parse_mention("@m3:f3a run it").is_none()); // 3 chars
        assert!(parse_mention("@m3:f3a2x run it").is_none()); // 5 alnum, no boundary
        assert!(parse_mention("@m3:f3a- run it").is_none()); // 3 alnum + dash
    }

    #[test]
    fn parse_empty_text_is_not_a_command() {
        assert!(parse_mention("@m3:f3a2").is_none());
        assert!(parse_mention("@m3:f3a2   ").is_none());
    }

    #[test]
    fn parse_missing_or_malformed_token() {
        assert!(parse_mention("no mention here").is_none());
        assert!(parse_mention("m3:f3a2 implicit").is_none()); // no leading @
        assert!(parse_mention("@:f3a2 no device").is_none());
        assert!(parse_mention("@m3 no prefix").is_none());
        assert!(parse_mention("@m!3:f3a2 bad device chars").is_none());
    }

    #[test]
    fn parse_prefix_boundary_allows_punctuation() {
        let parsed = parse_mention("@m3:f3a2, please rebase").unwrap();
        assert_eq!(parsed.prefix, "f3a2");
        assert_eq!(parsed.text, ", please rebase");
    }

    // ── scan_message_for_device ──

    #[test]
    fn scan_routes_own_device() {
        let msg = message("@m3:f3a2 stop and commit", "web", "katopz-phone", 1000);
        let (outcome, route) = scan_message_for_device(&msg, "m3");
        assert_eq!(outcome, ScanOutcome::Routed);
        let route = route.unwrap();
        assert_eq!(route.prefix, "f3a2");
        assert_eq!(route.text, "stop and commit");
        assert_eq!(route.sender_label, "web");
    }

    #[test]
    fn scan_skips_other_devices() {
        let msg = message("@SHIKUWA:b1c9 rebase first", "web", "phone", 1000);
        let (outcome, route) = scan_message_for_device(&msg, "m3");
        assert_eq!(outcome, ScanOutcome::NotRouted);
        assert!(route.is_none());
    }

    #[test]
    fn scan_sender_label_falls_back_to_device_name() {
        let msg = message("@m3:f3a2 hi", "", "SHIKUWA", 1000);
        let (_, route) = scan_message_for_device(&msg, "m3");
        assert_eq!(route.unwrap().sender_label, "SHIKUWA");
    }

    #[test]
    fn scan_self_mention_is_dropped() {
        // An agent on m3 whose own label matches the target — dropped.
        let msg = message("@m3:f3a2 keep going", "m3:f3a2", "m3", 1000);
        let (outcome, route) = scan_message_for_device(&msg, "m3");
        assert_eq!(outcome, ScanOutcome::SelfMention);
        assert!(route.is_none());
    }

    #[test]
    fn scan_same_device_sibling_agent_is_routed() {
        // Agent m3:aaaa mentioning sibling m3:f3a2 — allowed.
        let msg = message("@m3:f3a2 mind rebasing?", "m3:aaaa", "m3", 1000);
        let (outcome, _) = scan_message_for_device(&msg, "m3");
        assert_eq!(outcome, ScanOutcome::Routed);
    }

    // ── MentionGuard ──

    #[test]
    fn guard_allows_first_and_enforces_cooldown() {
        let mut guard = MentionGuard::new(60, 20);
        assert_eq!(guard.check("f3a2", 0), MentionDecision::Allow);
        assert_eq!(
            guard.check("f3a2", 30_000),
            MentionDecision::SuppressCooldown
        );
        assert_eq!(guard.check("f3a2", 60_000), MentionDecision::Allow);
    }

    #[test]
    fn guard_cooldown_is_per_target() {
        let mut guard = MentionGuard::new(60, 20);
        assert_eq!(guard.check("f3a2", 0), MentionDecision::Allow);
        assert_eq!(guard.check("aaaa", 1_000), MentionDecision::Allow);
    }

    #[test]
    fn guard_enforces_hourly_cap() {
        let mut guard = MentionGuard::new(0, 3);
        let t = 1_000_000;
        assert_eq!(guard.check("f3a2", t), MentionDecision::Allow);
        assert_eq!(guard.check("f3a2", t + 1), MentionDecision::Allow);
        assert_eq!(guard.check("f3a2", t + 2), MentionDecision::Allow);
        assert_eq!(guard.check("f3a2", t + 3), MentionDecision::SuppressRateCap);
        // Next hour resets the bucket.
        let next_hour = (t / HOUR_MS + 1) * HOUR_MS;
        assert_eq!(guard.check("f3a2", next_hour), MentionDecision::Allow);
    }
}
