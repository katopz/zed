//! Fork-only tests for this module, kept out of the upstream file so syncs
//! do not conflict on them (plan 038).

use super::*;

#[test]
fn test_usage_reset_remaining_shows_precise_units() {
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();

    let one_day_three_hours = now + chrono::Duration::hours(27);
    assert_eq!(usage_reset_remaining(one_day_three_hours, now), "1d 3h");

    let three_hours_ten_minutes = now + chrono::Duration::minutes(190);
    assert_eq!(
        usage_reset_remaining(three_hours_ten_minutes, now),
        "3h 10m"
    );

    let ten_minutes_forty_seconds = now + chrono::Duration::seconds(640);
    assert_eq!(
        usage_reset_remaining(ten_minutes_forty_seconds, now),
        "10m 40s"
    );
}

#[test]
fn test_usage_reset_remaining_clamps_past_resets() {
    let now = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
    let already_passed = now - chrono::Duration::hours(2);
    assert_eq!(usage_reset_remaining(already_passed, now), "0s");
}

#[test]
fn test_weekly_daily_split_shares_remaining_budget_across_days() {
    // 50% of the weekly allowance used: half of each day's 14.3% share
    // pool remains, worth 3.5 of the 7 equal day shares.
    let window = UsageWindow {
        used: 0.5,
        resets_at: None,
    };
    assert_eq!(
        format_weekly_daily_split(window),
        "14.3%/day \u{2022} 50% left \u{2248} 3.5 of 7 days"
    );
}

#[test]
fn test_weekly_daily_split_clamps_exhausted_window() {
    let window = UsageWindow {
        used: 1.2,
        resets_at: None,
    };
    assert_eq!(
        format_weekly_daily_split(window),
        "14.3%/day \u{2022} 0% left \u{2248} 0.0 of 7 days"
    );
}

#[test]
fn test_extract_summary_section() {
    // Runs to the next heading of the same or higher level.
    assert_eq!(
        extract_summary_section(
            "Some preamble.\n\n## Summary\n\n- did a thing\n- did another\n\n## Next steps\n\nmore"
        )
        .as_deref(),
        Some("## Summary\n\n- did a thing\n- did another")
    );

    // Deeper headings stay inside the section; the trailing section runs to
    // the end of the document.
    assert_eq!(
        extract_summary_section("### 📋 Summary of changes\n\n#### Files\n\n- `a.rs`\n").as_deref(),
        Some("### 📋 Summary of changes\n\n#### Files\n\n- `a.rs`")
    );

    // Bold-only lines act as headings, and are closed by the next one.
    assert_eq!(
        extract_summary_section("**Summary:**\n\nall good\n\n**Caveats**\n\nnone").as_deref(),
        Some("**Summary:**\n\nall good")
    );

    // The last summary heading wins.
    assert_eq!(
        extract_summary_section("## Summary\n\nfirst\n\n## Detail\n\nx\n\n## Summary\n\nsecond")
            .as_deref(),
        Some("## Summary\n\nsecond")
    );

    // Headings inside fenced code blocks are not summaries or terminators.
    assert_eq!(
        extract_summary_section("## Summary\n\n```md\n## Not a heading\n```\n\ndone").as_deref(),
        Some("## Summary\n\n```md\n## Not a heading\n```\n\ndone")
    );

    assert_eq!(extract_summary_section("just prose, no headings"), None);
    assert_eq!(extract_summary_section("## Results\n\nnothing here"), None);
    // A summary heading with an empty body isn't worth copying.
    assert_eq!(extract_summary_section("blah\n\n## Summary\n\n"), None);
}

#[test]
fn test_parse_markdown_heading() {
    assert_eq!(parse_markdown_heading("## Summary"), Some((2, "Summary")));
    assert_eq!(
        parse_markdown_heading("  ### Summary ###  "),
        Some((3, "Summary"))
    );
    assert_eq!(parse_markdown_heading("**Summary**"), Some((7, "Summary")));
    // Needs whitespace after the hashes, and at most six of them.
    assert_eq!(parse_markdown_heading("#hashtag"), None);
    assert_eq!(parse_markdown_heading("####### too deep"), None);
    // Partially bold lines are prose, not headings.
    assert_eq!(parse_markdown_heading("**bold** and more"), None);
    assert_eq!(parse_markdown_heading("plain text"), None);
}
