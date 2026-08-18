//! Parsing of provider session-limit messages that embed a reset time.
//!
//! Claude Code reports subscription-window exhaustion as
//! `Internal error: You've hit your session limit · resets 1:20am
//! (Asia/Bangkok): {"errorKind": "rate_limit"}` — delivered either as a
//! turn-level error (captured by `AcpThread::last_api_error`) or as a
//! synthetic message in the transcript (visible as the last assistant
//! message). The reset time is rendered in the *machine's* local timezone
//! (that is what the "(Asia/Bangkok)" label is), so the parsed wall-clock
//! time is interpreted in `Local` and the continuation is scheduled at
//! reset + margin (default 60s). See `.plans/018_session_limit_scheduled_retry.md`.

use chrono::{DateTime, Local, NaiveDate, NaiveTime, TimeZone, Timelike};
use gpui::App;

/// Default margin added to the parsed reset time before retrying, in seconds.
pub const DEFAULT_SESSION_LIMIT_MARGIN_SECS: u64 = 60;

/// A parsed session-limit reset schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionLimitReset {
    /// Wall-clock reset time (next occurrence of the parsed time, local tz).
    pub reset_at: DateTime<Local>,
    /// When auto-prompt should dispatch the continuation (reset + margin).
    pub retry_at: DateTime<Local>,
    /// Milliseconds from parse time until `retry_at`.
    pub retry_delay_ms: u64,
    /// Human-readable retry time, e.g. `1:21am (Asia/Bangkok)`.
    pub retry_display: String,
}

/// Quick guard used to keep limit-synthetic messages out of context payloads
/// (e.g. `last_assistant_message` on a scheduled continuation).
pub fn looks_like_session_limit(text: &str) -> bool {
    text.to_ascii_lowercase().contains("session limit")
}

/// Parse a session-limit reset schedule from `text`, interpreting the
/// wall-clock time in the local timezone at `Local::now()`.
pub fn parse_session_limit(text: &str, margin_secs: u64) -> Option<SessionLimitReset> {
    build_session_limit(text, Local::now(), margin_secs)
}

/// Check a thread's last turn error, then its last assistant message, for a
/// session-limit reset schedule.
pub fn session_limit_from_thread(
    thread: &acp_thread::AcpThread,
    cx: &App,
    margin_secs: u64,
) -> Option<SessionLimitReset> {
    thread
        .last_api_error()
        .and_then(|text| parse_session_limit(text, margin_secs))
        .or_else(|| {
            thread
                .last_assistant_message_text(cx)
                .as_deref()
                .and_then(|text| parse_session_limit(text, margin_secs))
        })
}

/// Deterministic core of [`parse_session_limit`].
pub(crate) fn build_session_limit(
    text: &str,
    now: DateTime<Local>,
    margin_secs: u64,
) -> Option<SessionLimitReset> {
    // `to_ascii_lowercase` preserves byte lengths, so indices found in the
    // lowered copy are valid slices of the original text.
    let lowered = text.to_ascii_lowercase();
    let session_limit_ix = lowered.find("session limit")?;
    let resets_ix = lowered[session_limit_ix..].find("resets")? + session_limit_ix;
    let after = &text[resets_ix + "resets".len()..];

    let (hour, minute, pm, consumed) = parse_clock(after)?;
    let timezone_label = parse_paren_zone(&after[consumed..]);

    let hour_24 = match (hour, pm) {
        (12, false) => 0,
        (hour, true) if hour != 12 => hour + 12,
        (hour, _) => hour,
    };
    let time = NaiveTime::from_hms_opt(hour_24, minute, 0)?;
    let today = now.date_naive();
    let mut reset_at = local_from_naive(today, time)?;
    if reset_at <= now {
        let tomorrow = today.succ_opt()?;
        reset_at = local_from_naive(tomorrow, time)?;
    }
    let retry_at = reset_at + chrono::Duration::seconds(margin_secs as i64);
    let retry_delay_ms = (retry_at - now).num_milliseconds().max(0) as u64;
    Some(SessionLimitReset {
        reset_at,
        retry_at,
        retry_delay_ms,
        retry_display: format_retry_display(retry_at, timezone_label.as_deref()),
    })
}

/// Parse `H:MMam` / `H:MM pm` starting at the first ASCII digit in `s`.
/// Returns `(hour, minute, is_pm, bytes_consumed)`.
fn parse_clock(s: &str) -> Option<(u32, u32, bool, usize)> {
    let bytes = s.as_bytes();
    let mut ix = bytes.iter().position(|b| b.is_ascii_digit())?;
    let mut hour = 0u32;
    let mut digits = 0;
    while ix < bytes.len() && bytes[ix].is_ascii_digit() && digits < 2 {
        hour = hour * 10 + (bytes[ix] - b'0') as u32;
        ix += 1;
        digits += 1;
    }
    if digits == 0 || !(1..=12).contains(&hour) {
        return None;
    }
    if ix >= bytes.len() || bytes[ix] != b':' {
        return None;
    }
    ix += 1;
    if ix + 2 > bytes.len() || !bytes[ix].is_ascii_digit() || !bytes[ix + 1].is_ascii_digit() {
        return None;
    }
    let minute = (bytes[ix] - b'0') as u32 * 10 + (bytes[ix + 1] - b'0') as u32;
    if minute > 59 {
        return None;
    }
    ix += 2;
    if ix < bytes.len() && bytes[ix] == b' ' {
        ix += 1;
    }
    let meridiem = s.get(ix..ix + 2)?.to_ascii_lowercase();
    let pm = match meridiem.as_str() {
        "am" => false,
        "pm" => true,
        _ => return None,
    };
    ix += 2;
    Some((hour, minute, pm, ix))
}

/// Extract a `(...)` timezone label following the parsed clock, e.g.
/// `(Asia/Bangkok)`. Purely cosmetic — the time itself is interpreted in
/// the local timezone (see the module docs).
fn parse_paren_zone(s: &str) -> Option<String> {
    let open = s.find('(')?;
    let rest = &s[open + 1..];
    let close = rest.find(')')?;
    Some(rest[..close].trim().to_string())
}

fn local_from_naive(date: NaiveDate, time: NaiveTime) -> Option<DateTime<Local>> {
    let naive = date.and_time(time);
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(datetime) => Some(datetime),
        // DST fold: the earlier instant is the safe choice (retry lands
        // right at window reset rather than an hour past it).
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest),
        chrono::LocalResult::None => None,
    }
}

fn format_retry_display(retry_at: DateTime<Local>, timezone_label: Option<&str>) -> String {
    let hour = retry_at.hour();
    let (hour_12, meridiem) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    match timezone_label {
        Some(label) => format!("{hour_12}:{:02}{meridiem} ({label})", retry_at.minute()),
        None => format!("{hour_12}:{:02}{meridiem}", retry_at.minute()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(year, month, day)
                    .unwrap()
                    .and_hms_opt(hour, minute, 0)
                    .unwrap(),
            )
            .single()
            .unwrap()
    }

    const SAMPLE_ERROR: &str = "Internal error: You've hit your session limit \u{00b7} resets 1:20am (Asia/Bangkok): {\n  \"errorKind\": \"rate_limit\"\n}";
    const SAMPLE_MESSAGE: &str =
        "You've hit your session limit \u{00b7} resets 1:20am (Asia/Bangkok)";

    #[test]
    fn parses_full_claude_error_payload() {
        let now = local(2026, 8, 18, 23, 0);
        let limit = build_session_limit(SAMPLE_ERROR, now, 60).unwrap();
        let expected_retry = local(2026, 8, 19, 1, 21);
        assert_eq!(limit.reset_at, local(2026, 8, 19, 1, 20));
        assert_eq!(limit.retry_at, expected_retry);
        assert_eq!(
            limit.retry_delay_ms,
            (expected_retry - now).num_milliseconds() as u64
        );
        assert_eq!(limit.retry_display, "1:21am (Asia/Bangkok)");
    }

    #[test]
    fn parses_synthetic_message_form() {
        let now = local(2026, 8, 18, 0, 30);
        let limit = build_session_limit(SAMPLE_MESSAGE, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 1, 21));
        assert_eq!(limit.retry_display, "1:21am (Asia/Bangkok)");
    }

    #[test]
    fn parses_pm_time() {
        let text = "You've hit your session limit \u{00b7} resets 2:45pm (America/New_York)";
        let now = local(2026, 8, 18, 14, 0);
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 14, 46));
        assert_eq!(limit.retry_display, "2:46pm (America/New_York)");
    }

    #[test]
    fn midnight_edges_roll_display() {
        let text = "resets 12:05am (Asia/Bangkok) — session limit";
        // Guard phrase must precede `resets` for a match; craft a realistic one.
        let text = format!("You've hit your session limit · {text}");
        let now = local(2026, 8, 18, 23, 0);
        let limit = build_session_limit(&text, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 19, 0, 6));
        assert_eq!(limit.retry_display, "12:06am (Asia/Bangkok)");

        let text = "You've hit your session limit · resets 11:59pm (Asia/Bangkok)";
        let now = local(2026, 8, 18, 10, 0);
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 19, 0, 0));
        assert_eq!(limit.retry_display, "12:00am (Asia/Bangkok)");
    }

    #[test]
    fn missing_timezone_label_still_parses() {
        let text = "You've hit your session limit · resets 5:00am";
        let now = local(2026, 8, 18, 4, 0);
        let limit = build_session_limit(text, now, 0).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 5, 0));
        assert_eq!(limit.retry_display, "5:00am");
    }

    #[test]
    fn margin_is_applied() {
        let now = local(2026, 8, 18, 1, 0);
        let limit = build_session_limit(SAMPLE_MESSAGE, now, 300).unwrap();
        // 1:20am is still ahead of now (01:00) — same-day retry at reset + 300s.
        assert_eq!(limit.retry_at, local(2026, 8, 18, 1, 25));
        assert_eq!(limit.retry_display, "1:25am (Asia/Bangkok)");
        assert_eq!(
            limit.retry_delay_ms,
            (limit.retry_at - now).num_milliseconds() as u64
        );
    }

    #[test]
    fn rejects_non_limit_texts() {
        let now = local(2026, 8, 18, 12, 0);
        assert!(build_session_limit("Error: connection refused", now, 60).is_none());
        assert!(build_session_limit("resets 1:20am but no guard phrase", now, 60).is_none());
        assert!(build_session_limit(
            "You've hit your session limit · resets 99:00am",
            now,
            60
        )
        .is_none());
        assert!(build_session_limit(
            "You've hit your session limit · resets 9:60am",
            now,
            60
        )
        .is_none());
        assert!(build_session_limit(
            "You've hit your session limit · resets 10:00",
            now,
            60
        )
        .is_none());
    }

    #[test]
    fn guard_phrase_must_precede_resets() {
        let now = local(2026, 8, 18, 12, 0);
        // "resets" appearing before "session limit" must not match.
        assert!(build_session_limit(
            "resets 1:20am (UTC) … unrelated, then: session limit",
            now,
            60
        )
        .is_none());
    }

    #[test]
    fn looks_like_guard() {
        assert!(looks_like_session_limit(SAMPLE_ERROR));
        assert!(looks_like_session_limit("SESSION LIMIT reached"));
        assert!(!looks_like_session_limit("connection refused"));
    }
}
