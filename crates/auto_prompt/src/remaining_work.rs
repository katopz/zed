//! Remaining-work detection in assistant summaries and the pre-stop plan verification prompts.

use crate::lightweight_context::{count_actionable_tasks, is_actionable_checkbox};
use crate::{build_plan_landscape, context, is_perf_related, peer_states, plan_registry};
use agent_client_protocol::schema::v2 as acp;
use std::path::PathBuf;

/// Code-level remaining work detection: scans the last assistant message
/// for patterns that explicitly indicate incomplete work. This is a safety
/// net that catches cases where the LLM ignores its own Rule 10.
pub(crate) fn extract_remaining_section(text: &str) -> Option<String> {
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    if paragraphs.is_empty() {
        return None;
    }

    let trigger_words = [
        "remaining work",
        "remaining:",
        "still need",
        "still needs",
        "next step",
        "next steps",
        "todo:",
        "action items",
        "left to do",
    ];

    let scan_count = 3.min(paragraphs.len());
    let scan_start = paragraphs.len() - scan_count;

    for i in (scan_start..paragraphs.len()).rev() {
        let lower = paragraphs[i].to_lowercase();
        for trigger in &trigger_words {
            if lower.contains(trigger) {
                return Some(paragraphs[i..].join("\n\n"));
            }
        }
    }

    for i in (scan_start..paragraphs.len()).rev() {
        let has_actionable_checkbox = paragraphs[i].lines().any(is_actionable_checkbox);
        if has_actionable_checkbox {
            let start = if i > 0 {
                let prev_lower = paragraphs[i - 1].to_lowercase();
                let prev_is_header = paragraphs[i - 1].starts_with('#')
                    || (paragraphs[i - 1].len() < 80 && paragraphs[i - 1].ends_with(':'));
                if trigger_words.iter().any(|t| prev_lower.contains(t)) || prev_is_header {
                    i - 1
                } else {
                    i
                }
            } else {
                i
            };
            return Some(paragraphs[start..].join("\n\n"));
        }
    }

    let fallback_start = paragraphs.len().saturating_sub(2);
    Some(paragraphs[fallback_start..].join("\n\n"))
}

/// Phase 2 continuation derived from the summary's own next-steps guidance.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SummaryContinuation {
    /// The summary lists actionable next steps — continue with them.
    Steps(String),
    /// The summary explicitly declares no remaining work — the continuation
    /// should fall through to plan files / housekeeping instead of ordering
    /// the agent to "start working immediately" on nothing.
    NothingLeft,
    /// Every remaining item is non-executable now: armed-but-not-built
    /// levers, deferred items, owner-gated arms, thermal/cooldown-gated
    /// benchmark cells. The chain must STOP — gated work is not picked up
    /// autonomously and unrelated housekeeping would run uninvited (plan 031
    /// silent stop). Beats remaining plan tasks by design.
    Terminal,
}

/// Nothing-left cues: phrases a summary uses to declare that no work remains.
pub(crate) const NOTHING_LEFT_CUES: &[&str] = &[
    "nothing left",
    "nothing to do",
    "nothing remains",
    "nothing remaining",
    "nothing outstanding",
    "nothing actionable",
    "no remaining work",
    "no remaining tasks",
    "no remaining items",
    "no outstanding",
    "no open items",
    "no pending",
    "no further work",
    "no further action",
    "all tasks complete",
    "all complete",
    "all done",
    "fully complete",
    "none left",
];

/// Text of a numbered list item (`"1. foo"` / `"1) foo"`), or None.
pub(crate) fn numbered_item_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let digits_len = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    // ASCII digits are 1-byte, so this index is a char boundary.
    let rest = &trimmed[digits_len..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

/// True when a next-steps section declares there is no work left.
///
/// Requires an explicit nothing-left cue, and lets real work override the
/// negation: an actionable unchecked checkbox or a numbered step that is not
/// itself a nothing-left statement means tasks still remain (mixed sections
/// like "Nothing critical, but T2 still needs verification" keep working).
pub(crate) fn section_indicates_no_remaining_work(section: &str) -> bool {
    let lower = section.to_lowercase();
    if !NOTHING_LEFT_CUES.iter().any(|cue| lower.contains(cue)) {
        return false;
    }
    !section.lines().any(|line| {
        if is_actionable_checkbox(line) {
            return true;
        }
        match numbered_item_text(line) {
            Some(text) => {
                let text_lower = text.to_lowercase();
                !NOTHING_LEFT_CUES.iter().any(|cue| text_lower.contains(cue))
            }
            None => false,
        }
    })
}

/// Markers that make a remaining-work bullet non-executable now: armed
/// levers, deferrals, owner gates, thermal/cooldown gates, work handed to
/// another active session, unchanged backlog lanes.
pub(crate) const DEFERRED_ITEM_MARKERS: &[&str] = &[
    "armed",
    "defer",
    "owner-gated",
    "gated on",
    "owner go",
    "cooldown",
    "cooled",
    "thermal",
    "left to the",
    "unchanged",
    "awaiting",
    "parked",
    "on hold",
    "[-]",
    "blocked on",
    "blocked until",
    "blocked by",
    "still blocked",
    "waiting on",
    "waiting for",
    "needs you",
    "needs the owner",
];

/// Substring match that must start at a word boundary, so a negated form
/// ("unblocked by", "disarmed") never reads as the marker itself.
pub(crate) fn contains_marker(lower_line: &str, marker: &str) -> bool {
    lower_line.match_indices(marker).any(|(index, _)| {
        lower_line[..index]
            .chars()
            .next_back()
            .is_none_or(|previous| !previous.is_alphanumeric())
    })
}

/// True when every bullet/numbered item in the section carries a deferral
/// marker (an actionable unchecked checkbox immediately disqualifies). A
/// prose-only section is conservatively NOT terminal — no bullets, no
/// verdict.
pub(crate) fn section_all_deferred(section: &str) -> bool {
    let items: Vec<&str> = section
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || numbered_item_text(line).is_some()
        })
        .collect();
    if items.is_empty() {
        return false;
    }
    items.iter().all(|line| {
        if is_actionable_checkbox(line) {
            return false;
        }
        let lower = line.to_lowercase();
        DEFERRED_ITEM_MARKERS
            .iter()
            .any(|marker| contains_marker(&lower, marker))
    })
}

/// Continuation for summaries that declare no remaining work: switch to
/// housekeeping instead of ordering work on an empty queue.
pub(crate) fn housekeeping_continuation() -> String {
    "The previous session's summary reports no remaining work. Do NOT summarize again — \
     switch to housekeeping, in this order:\n\
     1. Run the boundary-guard and doc-sync skills for this repo.\n\
     2. Mine the next riir-clippy batch; search online to unblock any blocked task.\n\
     3. Re-run benchmarks if the working tree is dirty.\n\
     4. If no issues, plans, or proposals remain in this repo, look for unclaimed work \
     in the other workspace repos — only where no other agent is active.\n\
     5. If nothing actionable remains, stop cleanly; do not invent work."
        .to_string()
}

/// Extract the "what to do next" section from a ContextOverflow Phase 1 summary.
///
/// Unlike `detect_remaining_work`, which deliberately skips auto_prompt
/// summary responses to avoid re-summarization loops in the safety-net path,
/// this function is called from the Phase 2 continuation builder where the
/// summary IS the authoritative source of what should happen next.
///
/// Looks for the first section whose heading contains a next-steps indicator:
///   "recommended next steps", "next steps", "what remains", "remaining",
///   "what's left", "todo", "action items", "follow-up", "pending".
/// Returns the heading + body (everything until the next same-or-higher level
/// heading or end of message), so the continuation prompt keeps full context.
/// Falls back to the last 3 paragraphs (matching `extract_remaining_section` semantics)
/// when no explicit section is found, then to None when nothing actionable remains.
///
/// A section that explicitly declares no remaining work ("Nothing left",
/// "all tasks complete") returns `NothingLeft` instead of a continuation that
/// would order the agent to start working on an empty queue.
pub(crate) fn extract_summary_next_steps(summary: &str) -> Option<SummaryContinuation> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }

    let trigger_phrases = [
        "recommended next steps",
        "next steps",
        "what remains",
        "what is left",
        "what's left",
        "remaining",
        "to do",
        "todo",
        "action items",
        "follow-up",
        "follow up",
        "pending",
        "when resuming",
        "need to",
    ];

    // Collect every heading that mentions a trigger phrase, paired with its
    // section body. We scan ALL of them (not just the first) because a summary
    // may have a "## What Remains" section that just says "some tasks left"
    // (no actionable markers) followed by a "## Recommended Next Steps" section
    // that actually lists what to do.
    let all_lines: Vec<&str> = trimmed.lines().collect();
    let mut candidate_sections: Vec<String> = Vec::new();
    for (idx, line) in all_lines.iter().enumerate() {
        let lower = line.to_lowercase();
        if !lower.starts_with('#') {
            continue;
        }
        if !trigger_phrases.iter().any(|t| lower.contains(t)) {
            continue;
        }
        let heading_level = line.chars().take_while(|c| *c == '#').count();
        let mut end_idx = all_lines.len();
        for (j, l) in all_lines.iter().enumerate().skip(idx + 1) {
            let lvl = l.chars().take_while(|c| *c == '#').count();
            if lvl > 0 && lvl <= heading_level {
                end_idx = j;
                break;
            }
        }
        candidate_sections.push(all_lines[idx..end_idx].join("\n").trim().to_string());
    }

    // Prefer the first trigger heading whose body has actionable markers.
    // If none qualify, fall back to prose scan of the last 3 paragraphs.
    let section: Option<String> = candidate_sections.into_iter().find(|sec| {
        let lower = sec.to_lowercase();
        sec.contains("- ")
            || sec.contains("* ")
            || sec.contains("1.")
            || sec.contains("2.")
            || lower.contains("must")
            || lower.contains("need to")
            || lower.contains("should")
            || lower.contains("want me to")
            || lower.contains("decide")
    });

    let section = match section {
        Some(s) => s,
        None => {
            // No actionable trigger heading. Fall back to scanning the last
            // 3 paragraphs for trigger words (same heuristic as extract_remaining_section)
            // so a summary that embeds next steps in prose is still picked up.
            let paragraphs: Vec<&str> = trimmed
                .split("\n\n")
                .map(|p| p.trim())
                .filter(|p| !p.is_empty())
                .collect();
            let scan = 3.min(paragraphs.len());
            let scan_start = paragraphs.len() - scan;
            let mut hit: Option<usize> = None;
            for i in (scan_start..paragraphs.len()).rev() {
                let lower = paragraphs[i].to_lowercase();
                if trigger_phrases.iter().any(|t| lower.contains(t)) {
                    hit = Some(i);
                    break;
                }
            }
            match hit {
                Some(i) => paragraphs[i..].join("\n\n").trim().to_string(),
                None => return None,
            }
        }
    };

    if section.trim().is_empty() {
        return None;
    }

    if section_indicates_no_remaining_work(&section) {
        log::info!("[auto_prompt::extract_summary_next_steps] Summary declares no remaining work");
        return Some(SummaryContinuation::NothingLeft);
    }

    if section_all_deferred(&section) {
        log::info!(
            "[auto_prompt::extract_summary_next_steps] Summary remaining items are all armed/deferred/owner-gated — terminal"
        );
        return Some(SummaryContinuation::Terminal);
    }

    Some(SummaryContinuation::Steps(format!(
        "Continuing from the previous session's summary. Recommended next steps:\n\n\
         {section}\n\n\
         Pick up from here. Do NOT summarize again — start working immediately: \
         execute the highest-priority item above; where a decision is needed, \
         make the call yourself and proceed — unless the item is explicitly \
         owner-gated ('owner go', 'gated on'), in which case skip it, say why, \
         and take the next ungated item."
    )))
}

pub(crate) fn detect_remaining_work(last_assistant_message: Option<&str>) -> Option<String> {
    let msg = last_assistant_message?.trim();
    if msg.is_empty() {
        return None;
    }

    // Skip auto_prompt's own ContextOverflow Phase 1 summary responses.
    // The Phase 1 prompt asks the worker to summarize with sections:
    //   ### 1. Original Task / ### 2. What Was Accomplished / ### 3. What Remains / ### 4. Active Plan State
    // The summary naturally contains "What Remains" with unchecked items — those
    // were already evaluated by the worker as deferred/blocked, not actionable.
    // Firing the safety net on these creates a re-summarization loop.
    if is_auto_prompt_summary_response(msg) {
        log::info!(
            "[auto_prompt::detect_remaining_work] Skipping — message is an auto_prompt summary response"
        );
        return None;
    }

    let section = extract_remaining_section(msg);

    // Tightened pattern set (issue 006): only phrases that are authoritative
    // task-list markers. Generic phrases like "remaining work", "remaining:",
    // "still need", "next step(s)" were dropped because they matched inside
    // ordinary prose — e.g. "No remaining work" — and forced an extra LLM
    // second-opinion call every time a worker summarized a clean stop.
    //
    // We also require the phrase to appear in a *list/heading context*: the
    // line must start with a markdown list/heading marker. This prevents
    // free-form mentions like "consider the action items above" from firing.
    const PATTERNS: &[&str] = &["todo:", "action items", "left to do"];

    // Negation cues: if any appear within ~40 chars before the matched phrase
    // on the same line, treat the mention as referring to work that does NOT
    // exist and skip the override. Covers "no remaining work", "nothing left
    // to do", "no action items", etc.
    const NEGATIONS: &[&str] = &[
        "no ",
        "none ",
        "nothing ",
        "no further ",
        "nothing left ",
        "already done",
        "all done",
        "complete",
        "finished",
        "shipped",
        "landed",
        "resolved",
    ];

    for (line_idx, line) in msg.lines().enumerate() {
        let trimmed = line.trim_start();
        if !is_list_or_heading_line(trimmed) {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let mut matched_pattern: Option<&str> = None;
        for pattern in PATTERNS {
            if let Some(pos) = lower.find(pattern) {
                // Negation guard: scan up to 40 chars before the match for a
                // negation cue on the same line.
                let window_start = pos.saturating_sub(40);
                let prefix = &lower[window_start..pos];
                if NEGATIONS.iter().any(|neg| prefix.contains(neg)) {
                    log::info!(
                        "[auto_prompt::detect_remaining_work] Pattern '{pattern}' found on line {line_idx} but negated by preceding text — skipping override"
                    );
                    continue;
                }
                matched_pattern = Some(pattern);
                break;
            }
        }
        let Some(pattern) = matched_pattern else {
            continue;
        };
        log::warn!(
            "[auto_prompt::detect_remaining_work] Pattern found: {pattern} in a list/heading line of last_assistant_message — overriding stop"
        );
        let section_text = section.as_deref().unwrap_or(msg);
        // We already know the matched line is a list/heading line, so the
        // section is actionable by construction; surface it to the second
        // opinion and let the LLM decide whether to continue.
        return Some(format!(
            "Previous assistant mentioned remaining work. Extracted section:\n\n\
             {section_text}\n\n\
             If this describes specific actionable remaining work, continue with it. \
             If the work is already done or this is a false positive, stop."
        ));
    }

    for line in msg.lines() {
        let trimmed = line.trim_start();
        if is_actionable_checkbox(trimmed) {
            log::warn!(
                "[auto_prompt::detect_remaining_work] Pattern found: unchecked checkbox in last_assistant_message — overriding stop"
            );
            let section_text = section.as_deref().unwrap_or(msg);
            return Some(format!(
                "Previous assistant left unchecked items. Extracted section:\n\n\
                 {section_text}\n\n\
                 If these are real remaining tasks, continue with them. \
                 If already done or this is a false positive, stop."
            ));
        }
    }

    None
}

/// Whether a line starts with a markdown list or heading marker. Used by
/// [`detect_remaining_work`] to require task-list context for the substring
/// patterns (issue 006): a phrase like "action items" buried mid-paragraph in
/// prose does not count, but `## Action items` or `- Action items:` does.
pub(crate) fn is_list_or_heading_line(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('-')
        || trimmed.starts_with('*')
        || trimmed.starts_with('+')
        || trimmed.starts_with('#')
    {
        return true;
    }
    // Ordered list: digit(s) followed by `.` or `)`.
    let bytes = trimmed.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0 && idx + 1 < bytes.len() {
        let sep = bytes[idx];
        let next = bytes[idx + 1];
        return (sep == b'.' || sep == b')') && (next == b' ' || next == b'\t');
    }
    false
}

/// Detect whether an assistant message is ALREADY a summary suitable for a
/// context-overflow thread handoff, so Phase 1's "Stop and summarize" request
/// can be skipped.
///
/// Broader than `is_auto_prompt_summary_response` (which only matches responses
/// to Phase 1's own prompt wording). This also catches VOLUNTARY summaries that
/// agents produce following instructions like "Always end with TL;DR" — the
/// common case when context overflows on an agent that already self-summarized.
///
/// A markdown heading (levels 1-3) containing "summary" or "tl;dr" is treated
/// as a deliberate handoff signal. The Phase 1 response pattern (≥3 of 4
/// markers: original task / accomplished / what remains / active plan) is also
/// accepted via `is_auto_prompt_summary_response`.
pub(crate) fn looks_like_voluntary_summary(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Phase 1 response pattern — catch auto_prompt's own echo.
    if is_auto_prompt_summary_response(trimmed) {
        return true;
    }

    // A markdown heading at level 1-3 containing "summary" / "tl;dr" / "tldr"
    // is a deliberate handoff marker from the agent. Line-anchored and requires
    // a space after the `#` run (strict ATX heading) so prose mentions like
    // "see the Summary section" don't match.
    for raw_line in trimmed.lines() {
        let line = raw_line.trim_start();
        let bytes = line.as_bytes();
        let hash_count = line.chars().take_while(|c| *c == '#').count();
        if !(1..=3).contains(&hash_count) {
            continue;
        }
        // Require a space after the '#' run (e.g. "## Summary").
        if bytes.len() <= hash_count || bytes[hash_count] != b' ' {
            continue;
        }
        let lower = line.to_lowercase();
        if lower.contains("summary") || lower.contains("tl;dr") || lower.contains("tldr") {
            return true;
        }
    }

    false
}

/// Detect auto_prompt's own ContextOverflow Phase 1 summary responses.
///
/// When context overflows, auto_prompt sends:
///   "Stop what you are doing and provide a concise summary of your progress.
///    Include: (1) what was the original task, (2) what was accomplished,
///    (3) what remains to be done, (4) the current state of any active plans."
///
/// The worker responds with a structured summary that naturally has "What Remains"
/// sections with unchecked items. Both `detect_remaining_work` and
/// `llm_acknowledged_all_tasks_blocked` must skip these to avoid false positives.
pub(crate) fn is_auto_prompt_summary_response(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Phase 1 prompt asks for exactly these 4 sections
    let has_original_task = lower.contains("original task");
    let has_accomplished =
        lower.contains("what was accomplished") || lower.contains("accomplished");
    let has_remains = lower.contains("what remains") || lower.contains("what remain");
    let has_active_plan = lower.contains("active plan") || lower.contains("plan state");
    let section_matches = [
        has_original_task,
        has_accomplished,
        has_remains,
        has_active_plan,
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    // Need at least 3 of 4 to avoid false positives on random text
    section_matches >= 3
}

/// Check if the LLM explicitly declares that ALL remaining tasks are blocked,
/// not just that some tasks happen to mention blocking keywords.
///
/// This is a stricter version of the old `llm_acknowledged_blocked_tasks` — it requires
/// the LLM to state that the work as a whole cannot proceed (e.g. "nothing actionable",
/// "no further action"). Summary responses from auto_prompt's own Phase 1 are excluded
/// because they naturally mention "blocked" items as part of their structured output.
pub(crate) fn llm_acknowledged_all_tasks_blocked(last_assistant_message: Option<&str>) -> bool {
    let msg = match last_assistant_message {
        Some(m) if !m.trim().is_empty() => m.trim(),
        _ => return false,
    };

    // Never match on auto_prompt's own summary responses — they always mention
    // "blocked" items as part of the Phase 1 summary structure.
    if is_auto_prompt_summary_response(msg) {
        log::info!(
            "[auto_prompt::llm_acknowledged_all_tasks_blocked] Skipping — message is an auto_prompt summary response"
        );
        return false;
    }

    let lower = msg.to_lowercase();

    // The LLM must explicitly say everything is blocked / nothing can proceed.
    // Requires BOTH a qualifier (nothing/no further/can't) AND "blocked"/"deferred" together.
    // Bare "all remaining" + "blocked" is excluded because summary sections naturally
    // contain both words without meaning the worker is declaring a hard stop.
    let all_blocked = lower.contains("nothing actionable")
        || lower.contains("nothing left to do")
        || lower.contains("nothing left to implement")
        || lower.contains("no further action")
        || lower.contains("no further work")
        || lower.contains("cannot proceed further")
        || lower.contains("can't proceed further")
        || lower.contains("all remaining tasks are blocked")
        || lower.contains("all remaining work is blocked");

    all_blocked
}

pub(crate) fn extract_plan_paths_from_context(context_json: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    serde_json::from_str::<Ctx>(context_json)
        .ok()
        .map(|ctx| ctx.plan_files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default()
}

pub(crate) fn auto_claim_plan(
    next_prompt: &str,
    context_json: &str,
    session_id: &acp::SessionId,
    title: Option<&str>,
) {
    let plan_paths = extract_plan_paths_from_context(context_json);
    let session_id_str = session_id.to_string();
    if let Some(claimed) = plan_registry::auto_claim_from_prompt(
        next_prompt,
        &plan_paths,
        &session_id_str,
        title.unwrap_or("auto_prompt continuation"),
    ) {
        log::info!("[auto_prompt] Auto-claimed plan {claimed} for session {session_id_str}");
        // Phase 2 (P2.2 plan-start hook): announce to the agent board that this
        // agent is starting work on a plan, so peer agents know what we're about
        // to do. The meta field carries the plan filename for display.
        let plan_name = claimed.rsplit('/').next().unwrap_or(&claimed);
        peer_states::broadcast_state(
            &session_id_str,
            None,
            &format!("starting: {plan_name}"),
            &claimed,
        );
    }
}

/// Scope filter for `detect_remaining_plan_tasks`.
///
/// When context overflows and auto_prompt must build a continuation prompt
/// without an LLM round-trip, plan files are consulted in priority order:
/// current-repo unclaimed plans first, then other-repo unclaimed plans.
/// This prevents a noisy workspace from hijacking the continuation away from
/// the session's actual target project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlanRepoFilter {
    /// All unclaimed plans regardless of which repo they live in.
    All,
    /// Only plans whose path is under one of `work_dirs` (the session's own repos).
    CurrentRepo,
    /// Only plans whose path is NOT under any of `work_dirs`.
    OtherRepos,
}

pub(crate) fn plan_belongs_to_current_repo(plan_path: &str, work_dirs: &[PathBuf]) -> bool {
    work_dirs
        .iter()
        .any(|dir| plan_path.starts_with(dir.to_string_lossy().as_ref()))
}

pub(crate) fn detect_remaining_plan_tasks(
    context_json: &str,
    filter: PlanRepoFilter,
    work_dirs: Option<&[PathBuf]>,
) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    let ctx = serde_json::from_str::<Ctx>(context_json).ok()?;
    let session_id = ctx.session_id.as_deref().unwrap_or("");

    // For CurrentRepo / OtherRepos filters, use the session's actual work_dirs
    // (passed in from the caller) to classify each plan. When work_dirs is None
    // or empty, fall back to All-filter behaviour — we can't classify without
    // knowing which repos the session is targeting.
    let dirs: Vec<PathBuf> = match (filter, work_dirs) {
        (PlanRepoFilter::All, _) | (_, None | Some(&[])) => Vec::new(),
        (PlanRepoFilter::CurrentRepo | PlanRepoFilter::OtherRepos, Some(dirs)) => dirs.to_vec(),
    };
    let can_classify = !dirs.is_empty();

    let mut remaining = Vec::new();
    for plan in &ctx.plan_files {
        let count = count_actionable_tasks(&plan.content);
        if count == 0 {
            continue;
        }
        if plan_registry::is_claimed_by_other(&plan.path, session_id) {
            log::info!(
                "[auto_prompt::detect_remaining_plan_tasks] Skipping plan claimed by another agent: {}",
                plan.path
            );
            continue;
        }
        match filter {
            PlanRepoFilter::All => {}
            PlanRepoFilter::CurrentRepo => {
                if can_classify && !plan_belongs_to_current_repo(&plan.path, &dirs) {
                    continue;
                }
            }
            PlanRepoFilter::OtherRepos => {
                if can_classify && plan_belongs_to_current_repo(&plan.path, &dirs) {
                    continue;
                }
            }
        }
        let filename = plan.path.rsplit('/').next().unwrap_or("?");
        remaining.push(format!("- {filename}: {count} unchecked task(s)"));
    }
    if remaining.is_empty() {
        return None;
    }
    let scope_label = match filter {
        PlanRepoFilter::All => "",
        PlanRepoFilter::CurrentRepo => " (current repo)",
        PlanRepoFilter::OtherRepos => " (other repos)",
    };
    log::warn!(
        "[auto_prompt::detect_remaining_plan_tasks{scope_label}] Found {} unclaimed plan file(s) with unchecked tasks:\n{}",
        remaining.len(),
        remaining.join("\n")
    );
    let claims_note = plan_registry::format_claims_for_context(session_id)
        .map(|claims| format!("\n\n{claims}"))
        .unwrap_or_default();
    Some(format!(
        "Plan files have remaining unchecked tasks{scope_label}:\n\n{}\n\n\
         Continue with the next unchecked task. Mark completed steps as [x].{claims_note}",
        remaining.join("\n")
    ))
}

pub(crate) fn build_pre_stop_verification_prompt(
    context_json: &str,
    work_dirs: &Option<Vec<PathBuf>>,
) -> Option<String> {
    let landscape = build_plan_landscape(context_json);

    log::info!(
        "[auto_prompt::pre_stop_verification] work_dirs={:?}, has_landscape={}",
        work_dirs.as_ref().map(|dirs| dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()),
        landscape.is_some()
    );

    #[derive(serde::Deserialize)]
    struct Ctx {
        #[serde(default)]
        plan_files: Vec<context::PlanFileContent>,
    }
    if let Ok(ctx) = serde_json::from_str::<Ctx>(context_json) {
        let summary: Vec<String> = ctx
            .plan_files
            .iter()
            .map(|f| {
                let tasks = count_actionable_tasks(&f.content);
                let project = std::path::Path::new(&f.path)
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let filename = f.path.rsplit('/').next().unwrap_or("?");
                format!("{project}/{filename} (tasks={tasks})")
            })
            .collect();
        log::info!(
            "[auto_prompt::pre_stop_verification] plan_files=[{}]",
            summary.join(", ")
        );
    }

    let is_perf = is_perf_related(context_json, work_dirs.as_deref());

    let mut checks = vec![
        "1. **Last message**: Re-read your last message. Any remaining work or unchecked items?"
            .to_string(),
        "2. **Diagnostics**: Are there any errors or warnings from `cargo check` / `cargo clippy`?"
            .to_string(),
        "3. **Git**: Any uncommitted changes?".to_string(),
    ];

    if is_perf {
        checks.push("4. **Benchmarks**: Did you run relevant benchmarks?".to_string());
    }

    let mut sections = vec![checks.join("\n")];

    if let Some(landscape) = landscape {
        sections.push(format!(
            "## Remaining Plans\n\n\
             {landscape}\n\n\
             If any plan is in the SAME repo as the current work and has unchecked tasks, \
             declare `continuing`. Otherwise declare `stopping`."
        ));
    }

    sections.push(
        "## Declare\n\n\
         State one of:\n\
         - `continuing: <what remains>`\n\
         - `stopping: <reason>` — when nothing remains in this repo"
            .to_string(),
    );

    Some(format!(
        "PRE-STOP VERIFICATION — check state, then decide.\n\n{}\n\n\
         IMPORTANT: This is a read-only check. Do NOT run commands, do NOT fix anything, do NOT commit. \
         Just review your state and declare continuing or stopping.",
        sections.join("\n\n")
    ))
}

pub(crate) fn build_checkbox_verification_prompt(context_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct ContextPlanFiles {
        plan_files: Vec<context::PlanFileContent>,
    }

    let ctx: ContextPlanFiles = serde_json::from_str(context_json)
        .inspect_err(|e| {
            log::warn!(
                "[auto_prompt::build_checkbox_verification_prompt] Failed to parse context JSON: {e}"
            );
        })
        .ok()?;

    for file in &ctx.plan_files {
        let mut in_code_block = false;
        for line in file.content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }
            if in_code_block {
                continue;
            }
            if is_actionable_checkbox(trimmed) {
                let filename = file.path.rsplit('/').next().unwrap_or(&file.path);
                let plan_dir = file.path.rsplit('/').nth(1).unwrap_or(".plans");
                log::info!(
                    "[auto_prompt::build_checkbox_verification_prompt] Found unchecked items in {plan_dir}/{filename}"
                );
                return Some(format!(
                    "MANDATORY CHECKPOINT: Verify plan checkboxes before documentation.\n\n\
                     Re-read all {plan_dir}/ files and verify every '- [ ]' step against the actual code changes:\n\
                     1. Read each plan file in {plan_dir}/\n\
                     2. For each '- [ ]' item, check if the code already implements it\n\
                     3. Mark completed items as '- [x]' — do NOT re-execute completed work\n\
                     4. If any item is truly incomplete, continue working on it\n\
                     5. Only after ALL items in ALL plan files are '- [x]', create documentation at .docs/\n\n\
                     Unchecked items found in: {plan_dir}/{filename}"
                ));
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::should_defer_after_retry_exhaustion;

    #[test]
    fn deferral_markers_match_only_at_word_starts() {
        assert!(section_all_deferred("- Phase 3: still blocked on SSH."));
        assert!(!section_all_deferred(
            "- Phase 3 is now unblocked by the new key: run it."
        ));
        assert!(!section_all_deferred("- Re-enable the disarmed lever."));
        assert!(section_all_deferred("- Lever re-armed for the next run."));
    }

    // --- extract_remaining_section tests ---

    #[test]
    fn test_extract_remaining_section_single_paragraph_no_trigger() {
        let text = "This is a single paragraph.";
        assert_eq!(
            extract_remaining_section(text),
            Some("This is a single paragraph.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trigger_includes_header_and_list() {
        let text = "First paragraph.\n\nSecond paragraph.\n\n### Remaining work:\n\n- Do thing A\n- Do thing B";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining work:\n\n- Do thing A\n- Do thing B".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_real_world_remaining_work() {
        let text = "What was accomplished:\n\n1. Fixed bug\n2. Added tests\n\n### Remaining work for next session:\n\n- Rebuild with Metal backend\n- Retest training\n- If NaN resolved: Run full benchmark";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining work for next session:\n\n- Rebuild with Metal backend\n- Retest training\n- If NaN resolved: Run full benchmark".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trailing_whitespace_fallback_two() {
        let text = "First.\n\nLast.  \n\n";
        assert_eq!(
            extract_remaining_section(text),
            Some("First.\n\nLast.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_empty() {
        assert_eq!(extract_remaining_section(""), None);
    }

    #[test]
    fn test_extract_remaining_section_only_whitespace() {
        assert_eq!(extract_remaining_section("   \n\n  \n\n  "), None);
    }

    #[test]
    fn test_extract_remaining_section_two_paragraphs_no_trigger() {
        let text = "First paragraph here.\n\nSecond paragraph here.";
        assert_eq!(
            extract_remaining_section(text),
            Some("First paragraph here.\n\nSecond paragraph here.".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_single_line() {
        let text = "Just one line no double breaks";
        assert_eq!(
            extract_remaining_section(text),
            Some("Just one line no double breaks".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_checkbox_with_header() {
        let text =
            "Summary of work done.\n\n### Remaining:\n\n- [ ] Task 1\n- [ ] Task 2\n- [ ] Task 3";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Remaining:\n\n- [ ] Task 1\n- [ ] Task 2\n- [ ] Task 3".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_checkbox_includes_preceding_header_like() {
        let text = "Summary.\n\nPending items:\n\n- [ ] Task 1\n- [ ] Task 2";
        assert_eq!(
            extract_remaining_section(text),
            Some("Pending items:\n\n- [ ] Task 1\n- [ ] Task 2".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_many_paragraphs_scans_last_3() {
        let text = "P1.\n\nP2.\n\nP3.\n\nP4.\n\n### Next steps:\n\n- Do X";
        assert_eq!(
            extract_remaining_section(text),
            Some("### Next steps:\n\n- Do X".to_string())
        );
    }

    #[test]
    fn test_extract_remaining_section_trigger_in_middle_of_last_3() {
        let text = "Accomplished A.\n\nAccomplished B.\n\n### TODO:\n\n- Fix bug\n- Add tests\n\nAlso mentioned in passing.";
        assert_eq!(
            extract_remaining_section(text),
            Some("### TODO:\n\n- Fix bug\n- Add tests\n\nAlso mentioned in passing.".to_string())
        );
    }

    #[test]
    fn test_detect_remaining_work_strikethrough_checkbox_not_actionable() {
        let msg = "Summary of work done.\n\n- [ ] ~~T5: SIMD stuff~~ Skipped — YAGNI";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "struck-through checkbox should not trigger remaining work override, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_ignores_no_remaining_work_prose() {
        // Regression for issue 006: worker says "No remaining work" and the
        // old naive substring match on "remaining work" forced a second-opinion
        // LLM call. Tightened patterns + list-context requirement must skip it.
        let msg = "All 7 commits landed and pushed successfully.\n\nNo remaining work.";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "'No remaining work' in prose must NOT trigger override, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_ignores_pattern_in_prose_without_list_context() {
        // "action items" buried mid-paragraph must not fire.
        let msg = "I reviewed the action items above and they are all complete. Stopping now.";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "pattern in prose without list/heading context must NOT trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_fires_on_action_items_heading_with_list() {
        let msg = "Done with phase 1.\n\n## Action items\n\n- Fix bug A\n- Add tests for B";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "'## Action items' heading followed by list items SHOULD trigger"
        );
    }

    #[test]
    fn test_detect_remaining_work_fires_on_todo_list_item() {
        let msg = "Phase 1 complete.\n\n- TODO: wire up the new config flag";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "'- TODO:' list item SHOULD trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_detect_remaining_work_negation_guard_skips_override() {
        // Even in a list/heading line, a negation cue before the pattern must
        // suppress the override.
        let msg = "Summary.\n\n- No action items left to do — all complete";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_none(),
            "negated 'No action items left to do' must NOT trigger, got: {result:?}"
        );
    }

    #[test]
    fn test_is_list_or_heading_line_markers() {
        assert!(is_list_or_heading_line("- item"));
        assert!(is_list_or_heading_line("* item"));
        assert!(is_list_or_heading_line("+ item"));
        assert!(is_list_or_heading_line("# Heading"));
        assert!(is_list_or_heading_line("## Subheading"));
        assert!(is_list_or_heading_line("1. first"));
        assert!(is_list_or_heading_line("12) twelfth"));
        assert!(!is_list_or_heading_line("Plain prose."));
        assert!(!is_list_or_heading_line("1.NoSpace"));
        assert!(!is_list_or_heading_line(""));
    }

    #[test]
    fn test_detect_remaining_work_mixed_checkboxes_only_actionable_triggers() {
        let msg = "Work done.\n\n- [ ] ~~Skipped~~ Skipped\n\n- [ ] Real remaining task";
        let result = detect_remaining_work(Some(msg));
        assert!(
            result.is_some(),
            "real checkbox among skipped should trigger"
        );
        assert!(result.unwrap().contains("Real remaining task"));
    }

    #[test]
    fn test_extract_remaining_section_only_skipped_checkboxes() {
        let text =
            "Summary.\n\n### Remaining:\n\n- [ ] ~~Task A~~ Skipped\n- [ ] ~~Task B~~ — deferred";
        let result = extract_remaining_section(text);
        // extract_remaining_section returns the trailing paragraphs regardless,
        // but detect_remaining_work will filter — test the pipeline end-to-end instead
        // Here we just verify no actionable checkbox is detected in those paragraphs
        if let Some(section) = &result {
            let has_actionable = section.lines().any(is_actionable_checkbox);
            assert!(!has_actionable, "should have no actionable checkboxes");
        }
    }

    #[test]
    fn test_detect_remaining_plan_tasks_no_plan_files() {
        let context_json = r#"{"messages":[]}"#;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_none(), "no plan_files field => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_all_checked() {
        let context_json = r##"{"plan_files":[{"path":".plans/001_test.md","content":"# Plan\n\n- [x] Done task\n- [x] Another done task"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_none(), "all tasks checked => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_has_unchecked() {
        let context_json = r##"{"plan_files":[{"path":".plans/001_test.md","content":"# Plan\n\n- [x] Done\n- [ ] Remaining task\n"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_some(), "unchecked task should return Some");
        let prompt = result.unwrap();
        assert!(
            prompt.contains("001_test.md"),
            "should mention plan filename"
        );
        assert!(
            prompt.contains("1 unchecked"),
            "should report 1 unchecked task"
        );
        assert!(
            prompt.contains("Continue with the next unchecked task"),
            "should instruct to continue"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_multiple_plans() {
        let context_json = r##"{"plan_files":[
            {"path":".plans/001_alpha.md","content":"- [x] Done\n- [ ] T2: Fix bug"},
            {"path":".plans/002_beta.md","content":"- [ ] T1: New feature"},
            {"path":".plans/003_done.md","content":"- [x] All done"}
        ]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(result.is_some(), "plans with unchecked tasks => Some");
        let prompt = result.unwrap();
        assert!(prompt.contains("001_alpha.md"), "should list alpha");
        assert!(prompt.contains("002_beta.md"), "should list beta");
        assert!(
            !prompt.contains("003_done.md"),
            "should NOT list fully-done plan"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_invalid_json() {
        let result = detect_remaining_plan_tasks("not json at all", PlanRepoFilter::All, None);
        assert!(result.is_none(), "invalid json => None");
    }

    #[test]
    fn test_detect_remaining_plan_tasks_skipped_only_is_not_actionable() {
        let context_json = r##"{"plan_files":[{"path":".plans/004_skip.md","content":"- [ ] ~~Task A~~ Skipped\n- [ ] ~~Task B~~ — deferred"}]}"##;
        let result = detect_remaining_plan_tasks(context_json, PlanRepoFilter::All, None);
        assert!(
            result.is_none(),
            "only skipped/strikethrough checkboxes should not be actionable"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_current_repo_filters_other_repos() {
        // Two repos, each with an unclaimed plan. CurrentRepo filter should
        // pick up only the plan whose path is under the session's work_dirs.
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-a/.plans/001_current.md","content":"- [ ] T1"},
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        let current = detect_remaining_plan_tasks(
            context_json,
            PlanRepoFilter::CurrentRepo,
            Some(&work_dirs),
        )
        .expect("current-repo plan should be found");
        assert!(
            current.contains("001_current.md"),
            "current-repo plan should be listed"
        );
        assert!(
            !current.contains("002_other.md"),
            "other-repo plan should be filtered out"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_other_repos_excludes_current() {
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-a/.plans/001_current.md","content":"- [ ] T1"},
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        let other =
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::OtherRepos, Some(&work_dirs))
                .expect("other-repo plan should be found");
        assert!(
            other.contains("002_other.md"),
            "other-repo plan should be listed"
        );
        assert!(
            !other.contains("001_current.md"),
            "current-repo plan should be excluded"
        );
    }

    #[test]
    fn test_detect_remaining_plan_tasks_current_repo_none_falls_through() {
        // Only other-repo plans exist — CurrentRepo returns None.
        let context_json = r##"{"plan_files":[
            {"path":"/Users/me/proj-b/.plans/002_other.md","content":"- [ ] T2"}
        ]}"##;
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        assert!(
            detect_remaining_plan_tasks(
                context_json,
                PlanRepoFilter::CurrentRepo,
                Some(&work_dirs)
            )
            .is_none(),
            "no current-repo plans => None"
        );
        assert!(
            detect_remaining_plan_tasks(context_json, PlanRepoFilter::OtherRepos, Some(&work_dirs))
                .is_some(),
            "other-repo plan should be found"
        );
    }

    #[test]
    fn test_plan_belongs_to_current_repo_uses_work_dirs() {
        let work_dirs = vec![PathBuf::from("/Users/me/proj-a")];
        assert!(
            plan_belongs_to_current_repo("/Users/me/proj-a/.plans/001.md", &work_dirs),
            "plan under a work_dir is current-repo"
        );
        assert!(
            !plan_belongs_to_current_repo("/Users/me/proj-b/.plans/002.md", &work_dirs),
            "plan outside all work_dirs is NOT current-repo"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_recommended_heading() {
        let summary = "# Session Summary\n\n## What Was Accomplished\n\nDid X.\n\n\
                       ## Recommended Next Steps (When Resuming)\n\n\
                       1. Fix the riir-gpu build break\n\
                       2. Finish the Cargo.toml cleanup\n\
                       3. Commit on develop\n\n\
                       ## Active Plan Files\n\n\n- .plans/302_*";
        let result =
            extract_summary_next_steps(summary).expect("recommended next steps should be found");
        let SummaryContinuation::Steps(result) = result else {
            panic!("actionable next steps must produce Steps, got {result:?}");
        };
        assert!(
            result.contains("Fix the riir-gpu build break"),
            "should contain the first recommended step"
        );
        assert!(
            result.contains("Commit on develop"),
            "should contain the last recommended step"
        );
        assert!(
            !result.contains("Active Plan Files"),
            "should NOT bleed into the next section"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_what_remains_heading() {
        let summary = "# Summary\n\n## Original Task\n\nRefactor X.\n\n## What Remains\n\n\
                       - [ ] T2: verify build\n\
                       - [ ] T3: commit\n";
        let result =
            extract_summary_next_steps(summary).expect("What Remains section should be found");
        let SummaryContinuation::Steps(result) = result else {
            panic!("unchecked checkboxes must produce Steps, got {result:?}");
        };
        assert!(
            result.contains("T2: verify build"),
            "should contain the unchecked task"
        );
        assert!(
            result.contains("make the call yourself"),
            "tail should tell the agent to resolve decisions itself"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_ignores_heading_without_actionable_body() {
        let summary =
            "# Summary\n\n## Next Steps\n\nSee the plan file for details.\n\n## Done\n\nAll good.";
        let result = extract_summary_next_steps(summary);
        assert!(
            result.is_none(),
            "heading with no actionable markers should be ignored"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_no_heading_falls_back_to_prose() {
        // Summary embeds next steps in the last paragraph without a dedicated heading.
        let summary = "# Summary\n\nDid A.\n\nDid B.\n\nStill need to run the benchmark and commit the results.";
        let result =
            extract_summary_next_steps(summary).expect("prose trigger 'need to' should match");
        let SummaryContinuation::Steps(result) = result else {
            panic!("prose next steps must produce Steps, got {result:?}");
        };
        assert!(
            result.contains("run the benchmark"),
            "fallback should pick up the prose next-step"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_empty_returns_none() {
        assert!(extract_summary_next_steps("").is_none());
        assert!(extract_summary_next_steps("   ").is_none());
    }

    #[test]
    fn test_extract_summary_next_steps_user_real_world_summary() {
        // Reproduces the exact structure from the user's reported bug:
        // the summary had "Recommended Next Steps (When Resuming)" with 3 numbered
        // items. Before the fix, auto_prompt's Phase 2 ignored this entirely and
        // continued with "plan files have remaining unchecked tasks" instead.
        let summary = "# Session Summary\n\n## Original Task\n\nRefactor.\n\n\
                       ## What Was Accomplished\n\nSplit done.\n\n\
                       ## What Remains\n\nSome tasks left.\n\n\
                       ## Recommended Next Steps (When Resuming)\n\n\
                       1. Decide on the riir-gpu build break first\n\
                       2. Skip verification and finish cleanup\n\
                       3. Revert the in-flight work\n\n\
                       ## Active Plan / Issue Files\n\n- .plans/302_* complete";
        let result = extract_summary_next_steps(summary)
            .expect("recommended next steps must be extracted for Phase 2");
        let SummaryContinuation::Steps(result) = result else {
            panic!("numbered steps must produce Steps, got {result:?}");
        };
        assert!(
            result.contains("Decide on the riir-gpu build break"),
            "must contain step 1"
        );
        assert!(
            result.contains("Revert the in-flight work"),
            "must contain step 3"
        );
        assert!(
            result.contains("Pick up from here"),
            "continuation framing should be present"
        );
        assert!(
            result.contains("where a decision is needed"),
            "tail should instruct the agent to resolve the decision itself"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_gated_remains_stay_steps() {
        // Reproduces the user-reported confusion: a boundary-session summary
        // whose remains are ALL owner-gated or deliberately deferred. The
        // remains must still be extracted as Steps (NOT skipped, NOT nothing-
        // left) — a different session's plan-file fallback must never silently
        // replace them — and the tail must tell the agent to respect the gates
        // instead of deciding on the owner's behalf.
        let summary = "## Summary\n\n\
                       **(1) Original task** — Fix and remove `.issues/724` and `.issues/737` in riir-ai.\n\n\
                       **(2) Accomplished**\n\
                       - Workspace gate script landed, 15 repos clean\n\n\
                       **(3) What remains**\n\
                       - `.issues/739` — engram trait seam relocation (CODE, owner go; T1–T5 with GOAT gate)\n\
                       - T-AGENTS prose extraction from the 5 long AGENTS.md files — deliberately not done\n\
                       - riir-chain shows 2 transient gate findings until the pin branch merges\n\n\
                       **(4) Active plan state**\n\
                       - `.issues/739` — OPEN, gated on owner go\n\
                       - `.proposals/025` — DEFER by decision record, not a task";
        let result = extract_summary_next_steps(summary)
            .expect("gated remains must still be extracted as Steps");
        let SummaryContinuation::Steps(result) = result else {
            panic!("gated remains are real remains, got {result:?}");
        };
        assert!(
            result.contains(".issues/739"),
            "the remains section must be embedded so the next thread sees it: {result}"
        );
        assert!(
            result.contains("owner-gated"),
            "tail must instruct the agent to respect explicit owner gates: {result}"
        );
        // All-gated remains are NOT nothing-left: there is still work the
        // owner can ungat, so the housekeeping path must not fire.
        assert!(!result.contains("boundary-guard"));
    }

    #[test]
    fn test_extract_summary_next_steps_nothing_left_returns_housekeeping_path() {
        // The reported bug: a "What Remains" section that declares nothing
        // left still counted as actionable ("- " bullet marker) and produced
        // "start working immediately" on an empty queue.
        let summary = "# Session Summary\n\n## Original Task\n\nRefactor.\n\n\
                       ## What Was Accomplished\n\nAll done.\n\n\
                       ## What Remains\n\n- Nothing — all tasks complete.";
        let result = extract_summary_next_steps(summary)
            .expect("nothing-left section must still be detected");
        assert_eq!(
            result,
            SummaryContinuation::NothingLeft,
            "a remains section declaring nothing left must NOT produce Steps"
        );
    }

    #[test]
    fn test_extract_summary_next_steps_nothing_left_prose() {
        // No heading — the prose fallback picks up "Nothing left to do" and
        // must classify it as NothingLeft rather than actionable steps.
        let summary = "# Session Summary\n\nDid A.\n\nDid B.\n\nNothing left to do.";
        let result =
            extract_summary_next_steps(summary).expect("prose nothing-left must be detected");
        assert_eq!(result, SummaryContinuation::NothingLeft);
    }

    #[test]
    fn test_extract_summary_next_steps_mixed_negation_with_real_work_stays_steps() {
        // Negation cue + real unchecked work → Steps, not NothingLeft.
        let summary = "# Summary\n\n## What Remains\n\n\
                       T1 is all done.\n\
                       - [ ] T2: verify build\n";
        let result = extract_summary_next_steps(summary).expect("section should be found");
        let SummaryContinuation::Steps(result) = result else {
            panic!("an actionable checkbox must override the negation cue, got {result:?}");
        };
        assert!(result.contains("T2: verify build"));
    }

    #[test]
    fn test_extract_summary_next_steps_negated_numbered_item_is_nothing_left() {
        // A numbered item whose text is itself a nothing-left statement must
        // not count as real work.
        let summary = "# Summary\n\n## Next Steps\n\n1. Nothing left to do — all complete.";
        let result =
            extract_summary_next_steps(summary).expect("numbered negation must be detected");
        assert_eq!(result, SummaryContinuation::NothingLeft);
    }

    #[test]
    fn test_housekeeping_continuation_lists_skills_and_stop() {
        let prompt = housekeeping_continuation();
        assert!(prompt.contains("boundary-guard"));
        assert!(prompt.contains("doc-sync"));
        assert!(prompt.contains("riir-clippy"));
        assert!(prompt.contains("Re-run benchmarks"));
        assert!(prompt.contains("no other agent is active"));
        assert!(
            prompt.contains("stop cleanly"),
            "must end with a stop instruction so the loop terminates"
        );
        assert!(prompt.contains("Do NOT summarize again"));
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_gpu() {
        // Old pattern: "remaining + gpu" was a false positive — the worker might have
        // 5 remaining tasks where only 1 needs GPU and 4 are actionable.
        // New function requires explicit "all remaining ... blocked" language.
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("5 remaining tasks require GPU hardware")),
            "remaining + gpu should NOT suppress fallback — some tasks may still be actionable"
        );
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "All remaining tasks are blocked by GPU hardware requirement"
            )),
            "all remaining + blocked should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_nothing_actionable() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "Nothing actionable to implement without hardware access"
            )),
            "nothing actionable should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_nothing_left() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "Nothing left to do, all tasks complete or blocked"
            )),
            "nothing left to do should suppress fallback"
        );
    }

    // ── looks_like_voluntary_summary ─────────────────────────────────────

    #[test]
    fn looks_like_voluntary_summary_user_real_world_transcript() {
        // The exact message from the user's reported bug: an agent following
        // an "Always end with TL;DR" instruction self-summarized with a
        // `## Summary` heading. auto_prompt's Phase 1 then asked for ANOTHER
        // summary, wasting a full response. This message MUST match so Phase 1
        // is skipped and Phase 2 reuses it directly.
        let msg = "Both commits are clean. The remaining uncommitted files \
                   in riir-game-sdk (.docs/, Cargo.lock, README.md) and \
                   poc-maxman (.docs/, Cargo.toml, Cargo.lock, README.md) are \
                   from another agent's WIP — I left them untouched per the rule.

## Summary

Three commits landed across two repos:

| Repo | Commit | What |
|---|---|---|
| `riir-game-sdk` | `8bad2ea` | `riir-games-cluster` crate skeleton |
| `poc-maxman` | `31adb56` | AGENTS.md references to Issue 025 |

### What was created

**`riir-game-sdk/crates/riir-games-cluster/`** — skeleton crate, compiles clean.

### What's NOT done (intentionally)

- T2–T5 — actual code extraction, needs a plan.

### Next step

Ready to execute Issue 024 (Hero → Avatar rename)?";
        assert!(
            looks_like_voluntary_summary(msg),
            "voluntary `## Summary` heading must trigger skip-Phase-1"
        );
    }

    #[test]
    fn looks_like_voluntary_summary_tldr_heading() {
        assert!(looks_like_voluntary_summary(
            "## TL;DR\n\nDid X, Y, Z. Next: do W."
        ));
        assert!(looks_like_voluntary_summary("### TL;DR\n\nDid the thing."));
    }

    #[test]
    fn looks_like_voluntary_summary_summary_at_h1_h2_h3() {
        assert!(looks_like_voluntary_summary("# Summary\n\nDone."));
        assert!(looks_like_voluntary_summary("## Summary\n\nDone."));
        assert!(looks_like_voluntary_summary("### Summary\n\nDone."));
    }

    #[test]
    fn looks_like_voluntary_summary_phase1_response_still_matches() {
        // The existing is_auto_prompt_summary_response pattern must still be
        // accepted (≥3 of 4 markers) so Phase 2 handles it after Phase 1 fires.
        let phase1_response = "## Original Task\n\nRefactor.\n\n## What Was \
            Accomplished\n\nSplit done.\n\n## What Remains\n\nSome left.\n\n
            ## Active Plan State\n\n- .plans/302 complete";
        assert!(looks_like_voluntary_summary(phase1_response));
    }

    #[test]
    fn looks_like_voluntary_summary_no_heading_no_match() {
        // A normal working message with no summary heading must NOT match —
        // otherwise Phase 1 would never fire and context would overflow with
        // no handoff summary.
        assert!(!looks_like_voluntary_summary(
            "I committed the fix on develop. All tests pass."
        ));
        assert!(!looks_like_voluntary_summary(
            "Working on the refactor now, 3 files done, 2 to go."
        ));
    }

    #[test]
    fn looks_like_voluntary_summary_prose_mention_no_match() {
        // Prose that mentions the word "summary" but isn't a heading must NOT
        // match — avoids false positives when the agent references a summary
        // elsewhere (e.g. the agent ui's own summary, or a prior thread).
        assert!(!looks_like_voluntary_summary(
            "I added a summary field to the struct. Next I'll wire up the UI."
        ));
        assert!(!looks_like_voluntary_summary(
            "See the summary section of the PR description for details."
        ));
    }

    #[test]
    fn looks_like_voluntary_summary_heading_no_space_no_match() {
        // `#Summary` (no space) is not a valid ATX heading — reject it to keep
        // the detector strict and avoid matching version strings like `#Summary`.
        assert!(!looks_like_voluntary_summary("#Summary of changes"));
    }

    #[test]
    fn looks_like_voluntary_summary_heading_too_deep_no_match() {
        // `#### Summary` (level 4+) is too deep to be a deliberate handoff
        // marker — reject to reduce false positives.
        assert!(!looks_like_voluntary_summary("#### Summary\n\nnested note"));
    }

    #[test]
    fn looks_like_voluntary_summary_empty_no_match() {
        assert!(!looks_like_voluntary_summary(""));
        assert!(!looks_like_voluntary_summary("   \n\n  "));
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_no_further() {
        assert!(
            llm_acknowledged_all_tasks_blocked(Some(
                "No further action possible without external API"
            )),
            "no further action should suppress fallback"
        );
        assert!(
            llm_acknowledged_all_tasks_blocked(Some("No further work can be done at this time")),
            "no further work should suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_remaining_work_heading() {
        // This is the key test case from the user's bug report:
        // The summary contains "Remaining Work" as a heading AND mentions "blocked",
        // but NOT all tasks are blocked — some are just not started yet.
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(
                "## Remaining Work (blocked or needs real .mlmodelc)\n\
                 - Refactor main.rs: Not started\n\
                 - Stateful KV cache: Blocked on macOS 15+\n\
                 - FP16 tensor: Blocked on conversion pipeline"
            )),
            "summary with Remaining Work heading + some blocked tasks should NOT suppress fallback"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_just_remaining() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("5 remaining tasks to implement")),
            "remaining without all-blocked language should NOT be detected as blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_just_blocked() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(
                "The build is blocked by a missing dependency"
            )),
            "blocked without all-tasks-are-blocked language should NOT be detected as blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_none() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(None),
            "None should not be blocked"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_empty() {
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some("")),
            "empty string should not be blocked"
        );
    }

    // --- Summary response detection tests ---

    #[test]
    fn test_is_auto_prompt_summary_response_real_summary() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             User asked to implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 complete with 38 tests.\n\
             \n### 3. What Remains\n\
             Phase 5-7 deferred.\n\
             \n### 4. Active Plan State\n\
             Plan 264 is in progress.";
        assert!(
            is_auto_prompt_summary_response(summary),
            "real Phase 1 summary should be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_real_summary_from_bug() {
        // Actual text from bug.md L7968-8059
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             \nUser asked to continue where previous session left off.\n\
             \n### 2. What Was Accomplished\n\
             \n6 commits on develop.\n\
             \n### 3. What Remains\n\
             \nPhase 5-7 need GPU.\n\
             \n### 4. Active Plan State\n\
             \nPlan 264 complete through Phase 4.";
        assert!(
            is_auto_prompt_summary_response(summary),
            "bug.md summary should be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_not_summary() {
        assert!(
            !is_auto_prompt_summary_response("I implemented the feature and committed."),
            "normal assistant response should NOT be detected as summary"
        );
        assert!(
            !is_auto_prompt_summary_response(""),
            "empty string should NOT be detected"
        );
    }

    #[test]
    fn test_is_auto_prompt_summary_response_partial_not_enough() {
        // Only 2 of 4 markers
        let text = "### Original Task\nSome task\n\n### What Remains\nTodo items";
        assert!(
            !is_auto_prompt_summary_response(text),
            "only 2 of 4 markers should NOT be detected (need >= 3)"
        );
    }

    #[test]
    fn test_detect_remaining_work_skips_summary_response() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             Implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 done.\n\
             \n### 3. What Remains\n\
             - [ ] Phase 5: GPU training\n\
             - [ ] Phase 6: Benchmarks\n\
             \n### 4. Active Plan State\n\
             Plan 264 in progress.";
        assert_eq!(
            detect_remaining_work(Some(summary)),
            None,
            "should NOT detect remaining work in auto_prompt Phase 1 summary"
        );
    }

    #[test]
    fn test_llm_acknowledged_all_tasks_blocked_skips_summary_response() {
        let summary = "## Session Summary\n\
             \n### 1. Original Task\n\
             Implement Plan 264.\n\
             \n### 2. What Was Accomplished\n\
             Phases 1-4 done.\n\
             \n### 3. What Remains\n\
             All remaining tasks are blocked by GPU.\n\
             \n### 4. Active Plan State\n\
             Plan 264 in progress.";
        assert!(
            !llm_acknowledged_all_tasks_blocked(Some(summary)),
            "should NOT match 'all tasks blocked' inside auto_prompt Phase 1 summary"
        );
    }

    // --- Verification prompt content tests ---
    // These catch the bug where the prompt told the AI to DO work
    // (cargo check, fix errors, commit) instead of just asking about state.

    #[test]
    fn test_verification_prompt_is_read_only() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        // Must contain the read-only disclaimer
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("do not run commands"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
        assert!(
            lower.contains("do not fix anything"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
        assert!(
            lower.contains("do not commit"),
            "verification prompt must be read-only, got:\n{prompt}"
        );
    }

    #[test]
    fn test_verification_prompt_no_action_imperatives() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        let lower = prompt.to_lowercase();

        // Check items should be questions, not commands
        assert!(
            !lower.contains("fix errors"),
            "prompt should not command 'fix errors', got:\n{prompt}"
        );
        assert!(
            !lower.contains("fix warnings"),
            "prompt should not command 'fix warnings', got:\n{prompt}"
        );
        assert!(
            !lower.contains("commit with"),
            "prompt should not command 'commit with', got:\n{prompt}"
        );
        assert!(
            !lower.contains("run relevant benchmarks"),
            "prompt should not command 'run relevant benchmarks', got:\n{prompt}"
        );
    }

    #[test]
    fn test_verification_prompt_uses_questions_not_commands() {
        let context_json = r#"{"plan_files":[{"path":"plans/001.md","content":"- [ ] task 1"}]}"#;
        let prompt = build_pre_stop_verification_prompt(context_json, &None)
            .expect("should return Some with plan files");

        // Check items should be questions
        assert!(
            prompt.contains("Are there any errors"),
            "diagnostics check should be a question, got:\n{prompt}"
        );
        assert!(
            prompt.contains("Any uncommitted changes"),
            "git check should be a question, got:\n{prompt}"
        );
    }

    /// Bugfix contract (2026-09-11 WAF-block freeze): when every orchestration
    /// retry has failed AND the source thread's turn itself failed at the API
    /// level (`had_api_error`), the retry-exhaustion path must defer
    /// (`RetryAfterBackoff`) rather than fall through to the no-LLM safety
    /// nets (`detect_remaining_work` / `detect_remaining_plan_tasks`), which
    /// would "continue" from a stale assistant message into the same blocked
    /// API — the infinite send → 405 → decide → send loop. Mirrors the
    /// issue-007 contract style: the rule lives in
    /// `should_defer_after_retry_exhaustion`, which the match guard on the
    /// exhaustion arm calls, so flipping the rule breaks this test.
    #[test]
    fn test_retry_exhaustion_with_api_error_defers_instead_of_safety_net() {
        assert!(
            should_defer_after_retry_exhaustion(true),
            "had_api_error + all retries failed must defer, not safety-net continue"
        );
        assert!(
            !should_defer_after_retry_exhaustion(false),
            "no API error: safety nets stay reachable for ordinary retry exhaustion"
        );
    }

    #[test]
    fn test_verification_prompt_no_plan_files_still_returns_prompt() {
        // Even without plan files, the function returns Some with just
        // the checklist (no Remaining Plans section).
        let context_json = r#"{"plan_files":[]}"#;
        let result = build_pre_stop_verification_prompt(context_json, &None);
        assert!(
            result.is_some(),
            "should return Some even with empty plan files"
        );
        let prompt = result.unwrap();
        assert!(
            !prompt.contains("## Remaining Plans"),
            "should not have Remaining Plans section when no plan files"
        );
    }
}
