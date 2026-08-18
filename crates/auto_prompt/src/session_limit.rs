//! Retry scheduling for subscription rate-limit windows (5-hour session and
//! weekly), from two sources in order of trust:
//!
//! 1. **Usage API** — agent_ui's poller (the endpoint backing the usage
//!    rings) pushes exact absolute `resets_at` timestamps per window into
//!    [`record_usage_hint`]; when the turn error indicates a limit and a
//!    window reads exhausted, we schedule at its reset (handles the weekly
//!    window without knowing its error-text format). See
//!    `.plans/019_weekly_limit_api_scheduled_retry.md`.
//! 2. **Error-text parsing** — Claude Code reports window exhaustion as
//!    `You've hit your session limit · resets 1:20am (Asia/Bangkok)` (or
//!    `weekly limit` / `Opus limit`, with a weekday prefix on weekly resets,
//!    e.g. `resets Mon 12:00am`), delivered either as a turn-level error
//!    (captured by `AcpThread::last_api_error`, usually wrapped as
//!    `Internal error: …: {"errorKind": "rate_limit"}`) or as a synthetic
//!    message in the transcript (visible as the last assistant message).
//!    The reset time is rendered in the *machine's* local timezone
//!    (that is what the "(Asia/Bangkok)" label is), so the parsed wall-clock
//!    time is interpreted in `Local`.
//!
//! The continuation is scheduled at reset + margin (default 60s).
//! See `.plans/018_session_limit_scheduled_retry.md`.

use chrono::{
    DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Utc, Weekday,
};
use gpui::App;
use std::sync::Mutex;

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
    /// Human-readable retry time, e.g. `1:21am (Asia/Bangkok)` or `Thu 3:05am`.
    pub retry_display: String,
}

/// One rate-limit window as reported by the usage endpoint backing the
/// usage rings (`claude_usage.rs` in agent_ui).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageWindowHint {
    /// Fraction of the window's allowance consumed, in `0.0..=1.0`.
    pub used: f32,
    /// Exact absolute reset timestamp.
    pub resets_at: Option<DateTime<Utc>>,
}

/// Latest known usage for the subscription windows. Pushed by agent_ui's
/// usage poller via [`record_usage_hint`] — the auto_prompt decision
/// functions cannot depend on agent_ui, so the data crosses as a plain
/// static instead of a parameter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageHint {
    pub five_hour: Option<UsageWindowHint>,
    pub seven_day: Option<UsageWindowHint>,
    pub seven_day_opus: Option<UsageWindowHint>,
}

static USAGE_HINT: Mutex<Option<UsageHint>> = Mutex::new(None);

/// Called by the agent_ui usage poller after each successful poll.
pub fn record_usage_hint(hint: UsageHint) {
    *USAGE_HINT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hint);
}

/// A window counts as exhausted only at ~100% — the endpoint reports whole
/// percents, so 0.99 absorbs rounding while anything lower means a
/// transient error rather than window exhaustion.
const EXHAUSTED_THRESHOLD: f32 = 0.99;

/// Subscription-limit phrases Claude Code prints in its error and splash
/// text, one per window family: `session limit` (5-hour), `weekly limit` /
/// `weekly chat limit` (7-day), `opus limit` (7-day, Opus-only).
const LIMIT_PHRASES: [&str; 4] = [
    "session limit",
    "weekly limit",
    "weekly chat limit",
    "opus limit",
];

/// Quick guard used to keep limit-synthetic messages out of context payloads
/// (e.g. `last_assistant_message` on a scheduled continuation).
pub fn looks_like_usage_limit(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    LIMIT_PHRASES.iter().any(|phrase| lowered.contains(phrase))
}

/// The error text indicates a subscription rate-limit error (any window).
fn is_rate_limit_error(text: &str) -> bool {
    text.to_ascii_lowercase().contains("rate_limit") || looks_like_usage_limit(text)
}

/// The error text names a weekly (7-day) window — the all-model `seven_day`
/// or the Opus-only `seven_day_opus` behind an "Opus limit" message.
pub fn error_mentions_weekly(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.contains("opus limit")
        || lowered.contains("weekly")
        || lowered.contains("7-day")
        || lowered.contains("7 day")
        || lowered.contains("seven day")
        || lowered.contains("seven-day")
}

/// Parse a session-limit reset schedule from `text`, interpreting the
/// wall-clock time in the local timezone at `Local::now()`.
pub fn parse_session_limit(text: &str, margin_secs: u64) -> Option<SessionLimitReset> {
    build_session_limit(text, Local::now(), margin_secs)
}

/// Resolve a retry schedule from a turn error text: the recorded usage hint
/// first (exact absolute reset timestamps from the usage endpoint), then the
/// plan-018 text parser as fallback.
pub fn session_limit_from_error_text(text: &str, margin_secs: u64) -> Option<SessionLimitReset> {
    usage_hint_limit(text, margin_secs, Local::now())
        .or_else(|| parse_session_limit(text, margin_secs))
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
        .and_then(|text| session_limit_from_error_text(text, margin_secs))
        .or_else(|| {
            thread
                .last_assistant_message_text(cx)
                .as_deref()
                .and_then(|text| {
                    // Assistant messages are arbitrary text, so require the
                    // splash shape (limit phrase + `resets`) before trusting
                    // the usage hint — prose that merely quotes a limit
                    // phrase must never schedule a retry.
                    let splash_shaped = looks_like_usage_limit(text)
                        && text.to_ascii_lowercase().contains("resets");
                    if !splash_shaped {
                        return None;
                    }
                    session_limit_from_error_text(text, margin_secs)
                })
        })
}

/// Resolve the retry schedule from the recorded usage hint.
///
/// Window selection keys off the error phrasing so we never wait on an
/// inferred window: a session-phrased error trusts `five_hour` even when a
/// weekly window is also exhausted (if stacked, the retry fails with a
/// weekly error and reschedules once — self-correcting); a weekly-phrased
/// error trusts the weekly windows; a bare `rate_limit` kind with a healthy
/// 5h window must be the weekly constraint.
fn usage_hint_limit(
    error_text: &str,
    margin_secs: u64,
    now: DateTime<Local>,
) -> Option<SessionLimitReset> {
    if !is_rate_limit_error(error_text) {
        return None;
    }
    let hint = USAGE_HINT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()?;
    let weekly_reset = max_exhausted_reset(&[hint.seven_day, hint.seven_day_opus]);
    if error_mentions_weekly(error_text) {
        return weekly_reset.map(|reset| build_from_absolute(reset, now, margin_secs));
    }
    if let Some(five_hour_reset) = exhausted_reset(hint.five_hour) {
        return Some(build_from_absolute(five_hour_reset, now, margin_secs));
    }
    if !looks_like_usage_limit(error_text) {
        return weekly_reset.map(|reset| build_from_absolute(reset, now, margin_secs));
    }
    None
}

fn exhausted_reset(window: Option<UsageWindowHint>) -> Option<DateTime<Utc>> {
    let window = window?;
    (window.used >= EXHAUSTED_THRESHOLD)
        .then_some(window.resets_at)
        .flatten()
}

fn max_exhausted_reset(windows: &[Option<UsageWindowHint>]) -> Option<DateTime<Utc>> {
    windows.iter().filter_map(|window| exhausted_reset(*window)).max()
}

/// Build a schedule from an exact absolute reset timestamp (usage API).
fn build_from_absolute(
    reset_at_utc: DateTime<Utc>,
    now: DateTime<Local>,
    margin_secs: u64,
) -> SessionLimitReset {
    let reset_at = reset_at_utc.with_timezone(&Local);
    let retry_at = reset_at + chrono::Duration::seconds(margin_secs as i64);
    SessionLimitReset {
        retry_delay_ms: (retry_at - now).num_milliseconds().max(0) as u64,
        retry_display: format_display(retry_at, now, None),
        reset_at,
        retry_at,
    }
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
    let limit_ix = LIMIT_PHRASES
        .iter()
        .filter_map(|phrase| lowered.find(phrase))
        .min()?;
    let resets_ix = lowered[limit_ix..].find("resets")? + limit_ix;
    let after = &text[resets_ix + "resets".len()..];

    let (weekday, after) = match parse_weekday(after) {
        Some((weekday, consumed)) => (Some(weekday), &after[consumed..]),
        None => (None, after),
    };
    let (hour, minute, pm, consumed) = parse_clock(after)?;
    let timezone_label = parse_paren_zone(&after[consumed..]);

    let hour_24 = match (hour, pm) {
        (12, false) => 0,
        (hour, true) if hour != 12 => hour + 12,
        (hour, _) => hour,
    };
    let time = NaiveTime::from_hms_opt(hour_24, minute, 0)?;
    let reset_at = resolve_reset_at(now, weekday, time)?;
    let retry_at = reset_at + chrono::Duration::seconds(margin_secs as i64);
    let retry_delay_ms = (retry_at - now).num_milliseconds().max(0) as u64;
    Some(SessionLimitReset {
        reset_at,
        retry_at,
        retry_delay_ms,
        retry_display: format_display(retry_at, now, timezone_label.as_deref()),
    })
}

/// Next occurrence of `time`: on the parsed weekday when one is present
/// (weekly resets can be days out), else today or tomorrow (a session
/// window never exceeds five hours, so the clock time alone pins the date).
fn resolve_reset_at(
    now: DateTime<Local>,
    weekday: Option<Weekday>,
    time: NaiveTime,
) -> Option<DateTime<Local>> {
    let today = now.date_naive();
    match weekday {
        None => {
            let mut reset_at = local_from_naive(today, time)?;
            if reset_at <= now {
                reset_at = local_from_naive(today.succ_opt()?, time)?;
            }
            Some(reset_at)
        }
        Some(weekday) => {
            for day_offset in 0..=7 {
                let date = today + chrono::Duration::days(day_offset);
                if date.weekday() != weekday {
                    continue;
                }
                if let Some(reset_at) = local_from_naive(date, time) {
                    if reset_at > now {
                        return Some(reset_at);
                    }
                }
            }
            None
        }
    }
}

/// Parse an optional leading weekday token (`Mon`, `Tuesday`, …) as printed
/// by weekly-limit messages (`resets Mon 12:00am`). Returns the weekday and
/// the bytes consumed.
fn parse_weekday(s: &str) -> Option<(Weekday, usize)> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|b| b.is_ascii_alphabetic())?;
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
        end += 1;
    }
    let word = s[start..end].to_ascii_lowercase();
    let weekday = match word.as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    };
    Some((weekday, end))
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

/// `H:MMam/pm`, prefixed with the weekday when the retry lands on another
/// day (weekly resets can be days out), e.g. `Thu 3:05am` or
/// `Thu 3:05am (Asia/Bangkok)`.
fn format_display(
    retry_at: DateTime<Local>,
    now: DateTime<Local>,
    timezone_label: Option<&str>,
) -> String {
    let clock = format_clock(retry_at);
    let mut display = clock;
    if retry_at.date_naive() != now.date_naive() {
        display = format!("{} {display}", retry_at.format("%a"));
    }
    if let Some(label) = timezone_label {
        display = format!("{display} ({label})");
    }
    display
}

fn format_clock(retry_at: DateTime<Local>) -> String {
    let hour = retry_at.hour();
    let (hour_12, meridiem) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    format!("{hour_12}:{:02}{meridiem}", retry_at.minute())
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
        assert_eq!(limit.retry_display, "Wed 1:21am (Asia/Bangkok)");
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
        assert_eq!(limit.retry_display, "Wed 12:06am (Asia/Bangkok)");

        let text = "You've hit your session limit · resets 11:59pm (Asia/Bangkok)";
        let now = local(2026, 8, 18, 10, 0);
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 19, 0, 0));
        assert_eq!(limit.retry_display, "Wed 12:00am (Asia/Bangkok)");
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
    fn parses_weekly_weekday_form() {
        let text = "You've hit your weekly limit · resets Mon 9:30am";
        let now = local(2026, 8, 18, 12, 0); // Tuesday
        let limit = build_session_limit(text, now, 60).unwrap();
        // Next Monday after Tue 2026-08-18 is 2026-08-24.
        assert_eq!(limit.reset_at, local(2026, 8, 24, 9, 30));
        assert_eq!(limit.retry_at, local(2026, 8, 24, 9, 31));
        assert_eq!(limit.retry_display, "Mon 9:31am");
    }

    #[test]
    fn weekday_time_passed_rolls_to_next_week() {
        let text = "You've hit your weekly limit · resets Monday 9:30am";
        let now = local(2026, 8, 24, 10, 0); // Monday, past 9:30am
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.reset_at, local(2026, 8, 31, 9, 30));
    }

    #[test]
    fn weekday_time_ahead_stays_today() {
        let text = "You've hit your weekly limit · resets Mon 9:30am";
        let now = local(2026, 8, 24, 8, 0); // Monday, before 9:30am
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.reset_at, local(2026, 8, 24, 9, 30));
        // Same-day retry carries no weekday prefix (matches `format_display`).
        assert_eq!(limit.retry_display, "9:31am");
    }

    #[test]
    fn parses_opus_limit_form() {
        let text = "You've hit your Opus limit · resets 3:45pm";
        let now = local(2026, 8, 18, 14, 0);
        let limit = build_session_limit(text, now, 60).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 15, 46));
        assert_eq!(limit.retry_display, "3:46pm");
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
        assert!(looks_like_usage_limit(SAMPLE_ERROR));
        assert!(looks_like_usage_limit("SESSION LIMIT reached"));
        assert!(looks_like_usage_limit(
            "You've hit your weekly limit · resets Mon 12:00am"
        ));
        assert!(looks_like_usage_limit("You've hit your Opus limit · resets 3:45pm"));
        assert!(!looks_like_usage_limit("connection refused"));
    }

    #[test]
    fn weekly_and_opus_phrasing_routes_to_weekly_windows() {
        assert!(error_mentions_weekly(
            "You've hit your weekly limit · resets Mon 12:00am"
        ));
        assert!(error_mentions_weekly("You've hit your Opus limit · resets 3:45pm"));
        assert!(!error_mentions_weekly(SAMPLE_ERROR));
    }

    #[test]
    fn usage_hint_resolution_matrix() {
        use super::{EXHAUSTED_THRESHOLD, USAGE_HINT, usage_hint_limit};

        fn window(used: f32, resets_at: DateTime<Local>) -> super::UsageWindowHint {
            super::UsageWindowHint {
                used,
                resets_at: Some(resets_at.with_timezone(&Utc)),
            }
        }

        let now = local(2026, 8, 18, 12, 0); // Tuesday
        let five_reset = local(2026, 8, 18, 15, 0); // today 3pm
        let seven_day_reset = local(2026, 8, 21, 9, 0); // Friday
        let opus_reset = local(2026, 8, 22, 10, 0); // Saturday

        let bare_rate_limit = "Internal error: {\"errorKind\": \"rate_limit\"}";
        let weekly_text = "You've reached your weekly chat limit. Try again later.";

        // Shared static: reset, then run every scenario sequentially.
        *USAGE_HINT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        // 1. No hint recorded → nothing to resolve.
        assert!(usage_hint_limit(bare_rate_limit, 60, now).is_none());

        // 2. five_hour exhausted + bare errorKind → exact five_hour reset.
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(1.0, five_reset)),
            seven_day: Some(window(0.5, seven_day_reset)),
            seven_day_opus: None,
        });
        let limit = usage_hint_limit(bare_rate_limit, 60, now).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 15, 1));
        assert_eq!(limit.retry_display, "3:01pm");

        // 3. Weekly phrasing wins over an exhausted five_hour; max of the
        //    weekly windows (seven_day Friday < opus Saturday).
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(1.0, five_reset)),
            seven_day: Some(window(1.0, seven_day_reset)),
            seven_day_opus: Some(window(1.0, opus_reset)),
        });
        let limit = usage_hint_limit(weekly_text, 60, now).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 22, 10, 1));
        assert_eq!(limit.retry_display, "Sat 10:01am");

        // 4. Session phrasing trusts five_hour even when weekly is stacked.
        let limit = usage_hint_limit(SAMPLE_ERROR, 60, now).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 18, 15, 1));

        // 5. Bare errorKind + healthy five_hour + exhausted weekly → weekly.
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(0.95, five_reset)),
            seven_day: Some(window(1.0, seven_day_reset)),
            seven_day_opus: None,
        });
        let limit = usage_hint_limit(bare_rate_limit, 60, now).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 21, 9, 1));
        assert_eq!(limit.retry_display, "Fri 9:01am");

        // 6. Session phrasing + healthy five_hour → defer to text parsing.
        assert!(usage_hint_limit(SAMPLE_ERROR, 60, now).is_none());

        // 7. Non-rate-limit text never consults the hint.
        assert!(usage_hint_limit("connection refused", 60, now).is_none());

        // 8. Below the exhaustion threshold → window counts as healthy.
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(EXHAUSTED_THRESHOLD - 0.01, five_reset)),
            seven_day: Some(window(0.98, seven_day_reset)),
            seven_day_opus: None,
        });
        assert!(usage_hint_limit(bare_rate_limit, 60, now).is_none());

        // 9. `session_limit_from_error_text` falls back to text parsing when
        //    the hint declines (session phrasing, healthy five_hour) —
        //    scenario 6's data is still recorded.
        let limit = session_limit_from_error_text(SAMPLE_ERROR, 60).unwrap();
        assert!(limit.retry_display.ends_with("(Asia/Bangkok)"));

        // 10. A passed `resets_at` (stale hint) clamps to an immediate retry.
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(1.0, local(2026, 8, 18, 11, 0))),
            seven_day: None,
            seven_day_opus: None,
        });
        let limit = usage_hint_limit(bare_rate_limit, 60, now).unwrap();
        assert_eq!(limit.retry_delay_ms, 0);
        assert_eq!(limit.reset_at, local(2026, 8, 18, 11, 0));

        // 11. Opus phrasing routes to the weekly windows even when only
        //     seven_day_opus is exhausted.
        record_usage_hint(super::UsageHint {
            five_hour: Some(window(0.5, five_reset)),
            seven_day: Some(window(0.5, seven_day_reset)),
            seven_day_opus: Some(window(1.0, opus_reset)),
        });
        let limit =
            usage_hint_limit("You've hit your Opus limit · resets 3:45pm", 60, now).unwrap();
        assert_eq!(limit.retry_at, local(2026, 8, 22, 10, 1));
        assert_eq!(limit.retry_display, "Sat 10:01am");

        // 12. Weekly text with no hint → text fallback parses the weekday.
        //     (Real `Local::now()` — only weekday-invariant facts asserted.)
        *USAGE_HINT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let limit = session_limit_from_error_text(
            "You've hit your weekly limit · resets Mon 9:30am",
            60,
        )
        .unwrap();
        assert_eq!(limit.reset_at.weekday(), Weekday::Mon);
        assert!(limit.retry_display.ends_with("9:31am"));

        *USAGE_HINT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}
