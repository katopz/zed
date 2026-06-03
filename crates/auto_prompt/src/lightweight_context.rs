use serde::Serialize;

/// Item-level markers that indicate a non-actionable checkbox despite being unchecked.
const SKIP_ITEM_MARKERS: &[&str] = &[
    "DEFERRED",
    "\u{23f8}\u{fe0f}",
    "\u{2014} deferred",
    "- deferred",
    "~~",
    "Skipped",
    "skipped",
    "Cancelled",
    "cancelled",
    "N/A",
    "Won't fix",
    "wontfix",
    "NOT PLANNED",
    "out of scope",
];

/// Section header keywords (lowercase) that indicate non-actionable items.
const SKIP_SECTION_KEYWORDS: &[&str] = &["out of scope", "future", "backlog", "wishlist"];

#[derive(serde::Deserialize)]
struct Context {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    #[serde(default)]
    plan_files: Vec<crate::context::PlanFileContent>,
}

#[derive(Serialize)]
struct PlanSummaryEntry {
    path: String,
    unchecked: usize,
}

#[derive(Serialize)]
struct LightweightContext {
    stop_phase: String,
    iteration_count: u32,
    had_error: bool,
    last_assistant_message: Option<String>,
    plan_summary: Vec<PlanSummaryEntry>,
}

/// Build a lightweight orchestration context containing only the last 2 assistant messages
/// and plan file summaries (task counts). This replaces the full AutoPromptContext
/// serialization to reduce token usage from ~80K to ~500 tokens.
pub fn build_lightweight_orchestration_context(
    context_json: &str,
    stop_phase: &crate::context::StopPhase,
    iteration_count: u32,
    had_error: bool,
) -> String {
    let context: Context = match serde_json::from_str(context_json) {
        Ok(context) => context,
        Err(error) => {
            log::warn!(
                "[lightweight_context] Failed to parse context_json: {error}, using empty defaults"
            );
            Context {
                session_id: None,
                last_assistant_message: None,
                plan_files: Vec::new(),
            }
        }
    };

    let session_id = context.session_id.as_deref().unwrap_or("");

    let plan_summary: Vec<PlanSummaryEntry> = context
        .plan_files
        .iter()
        .filter(|file| !crate::plan_registry::is_claimed_by_other(&file.path, session_id))
        .filter_map(|file| {
            let unchecked = count_actionable_tasks(&file.content);
            if unchecked > 0 {
                Some(PlanSummaryEntry {
                    path: file.path.clone(),
                    unchecked,
                })
            } else {
                None
            }
        })
        .collect();

    let lightweight = LightweightContext {
        stop_phase: format!("{stop_phase:?}"),
        iteration_count,
        had_error,
        last_assistant_message: context.last_assistant_message,
        plan_summary,
    };

    serde_json::to_string(&lightweight).unwrap_or_else(|error| {
        log::error!("[lightweight_context] Failed to serialize lightweight context: {error}");
        format!(r#"{{"error":"serialization failed: {error}"}}"#)
    })
}

fn is_actionable_checkbox(line: &str) -> bool {
    let trimmed = line.trim_start();
    if !trimmed.starts_with("- [ ] ") && !trimmed.starts_with("* [ ] ") {
        return false;
    }
    let line_lower = trimmed.to_lowercase();
    !SKIP_ITEM_MARKERS
        .iter()
        .any(|marker| line_lower.contains(&marker.to_lowercase()))
}

fn count_actionable_tasks(content: &str) -> usize {
    let mut count = 0;
    let mut in_code_block = false;
    let mut skip_section = false;

    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        if trimmed.starts_with("## ") {
            let section_lower = trimmed.to_lowercase();
            skip_section = SKIP_SECTION_KEYWORDS
                .iter()
                .any(|keyword| section_lower.contains(keyword));
            continue;
        }
        if trimmed.starts_with("# ") {
            skip_section = false;
            continue;
        }
        if skip_section {
            continue;
        }
        if is_actionable_checkbox(trimmed) {
            count += 1;
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_actionable_checkbox_normal_task() {
        assert!(is_actionable_checkbox("- [ ] Implement the thing"));
    }

    #[test]
    fn test_actionable_checkbox_star_variant() {
        assert!(is_actionable_checkbox("* [ ] Another task"));
    }

    #[test]
    fn test_actionable_checkbox_strikethrough_skipped() {
        assert!(!is_actionable_checkbox(
            "- [ ] ~~T5: SIMD-accelerate stuff~~ Skipped — YAGNI"
        ));
    }

    #[test]
    fn test_actionable_checkbox_deferred() {
        assert!(!is_actionable_checkbox(
            "- [ ] ~~**Task 4.4:** Q4S training benchmark~~ — deferred to Phase 5"
        ));
    }

    #[test]
    fn test_count_actionable_tasks_ignores_strikethrough() {
        let plan = "\
# Plan

- [x] Done task
- [ ] Real task
- [ ] ~~Skipped task~~ Skipped — YAGNI
- [ ] ~~Deferred task~~ — deferred
";
        assert_eq!(count_actionable_tasks(plan), 1);
    }

    #[test]
    fn test_build_lightweight_context_basic() {
        let context_json = serde_json::json!({
            "last_assistant_message": "I completed task 1.",
            "plan_files": [
                {
                    "path": ".plans/001_plan.md",
                    "content": "- [ ] Task 1\n- [x] Task 2\n- [ ] Task 3"
                },
                {
                    "path": ".plans/002_done.md",
                    "content": "- [x] Task A\n- [x] Task B"
                }
            ]
        })
        .to_string();

        let result = build_lightweight_orchestration_context(
            &context_json,
            &crate::context::StopPhase::Working,
            3,
            false,
        );

        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed["stop_phase"], "Working");
        assert_eq!(parsed["iteration_count"], 3);
        assert_eq!(parsed["had_error"], false);
        assert_eq!(parsed["last_assistant_message"], "I completed task 1.");
        assert_eq!(parsed["plan_summary"].as_array().expect("array").len(), 1);
        assert_eq!(parsed["plan_summary"][0]["path"], ".plans/001_plan.md");
        assert_eq!(parsed["plan_summary"][0]["unchecked"], 2);
    }

    #[test]
    fn test_build_lightweight_context_invalid_json() {
        let result = build_lightweight_orchestration_context(
            "not valid json",
            &crate::context::StopPhase::PreStop,
            1,
            true,
        );

        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed["stop_phase"], "PreStop");
        assert_eq!(parsed["had_error"], true);
        assert_eq!(parsed["plan_summary"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn test_build_lightweight_context_skips_zero_unchecked() {
        let context_json = serde_json::json!({
            "plan_files": [
                {
                    "path": ".plans/done.md",
                    "content": "- [x] Everything done"
                }
            ]
        })
        .to_string();

        let result = build_lightweight_orchestration_context(
            &context_json,
            &crate::context::StopPhase::Working,
            5,
            false,
        );

        let parsed: serde_json::Value = serde_json::from_str(&result).expect("valid json");
        assert_eq!(parsed["plan_summary"].as_array().expect("array").len(), 0);
    }
}
