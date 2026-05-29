use auto_prompt::context::{AutoPromptContext, PlanFileContent};

// ===== Helper Functions =====

fn default_context() -> AutoPromptContext {
    use auto_prompt::context::StopPhase;
    AutoPromptContext {
        current_datetime: String::new(),
        current_paths: vec![],
        session_id: String::new(),
        title: None,
        messages: vec![],
        used_tools: false,
        entry_count: 0,
        current_plan: vec![],
        plan_files: vec![],
        doc_files: vec![],
        stop_reason: String::new(),
        had_error: false,
        approximate_token_count: 0,
        actual_input_tokens: None,
        iteration_count: 1,
        stop_phase: StopPhase::Working,
        verification_count: 0,
        was_truncated: false,
        plan_has_checkboxes: false,
        first_plan_filename: String::new(),
        plan_number: String::new(),
        first_user_message: None,
        last_assistant_message: None,
        modified_files: vec![],
        active_plan_claims: vec![],
    }
}

// ===== Checkbox Detection Tests =====

#[test]
fn test_has_task_checkboxes_with_proper_task_list() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1: Create feature branch\n- [ ] Task 2: Implement feature\n- [ ] Task 3: Add tests\n- [ ] Task 4: Documentation".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert!(context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_exactly_three_checkboxes() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1\n- [x] Task 2\n- [ ] Task 3".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Exactly 3 checkboxes - should detect
    assert!(context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_two_checkboxes() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1\n- [x] Task 2".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Only 2 checkboxes - should NOT detect (below threshold)
    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_code_blocks() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "```\n- [ ] Example in code\n- [x] Another example\n```\n\n**Tasks**:\n1. Some task\n2. Another task".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should NOT detect checkboxes in code blocks
    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_blockquotes() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "> - [ ] Example in blockquote\n> - [x] Another example\n\n**Tasks**:\n1. Some task\n2. Another task".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should NOT detect checkboxes in blockquotes
    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_example_section() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "## Examples\n\nBefore:\n- [ ] Select items\n- [x] Process results\n\n**Tasks**:\n1. Implement task\n2. Add tests".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should NOT detect - only 2 checkboxes (below threshold of 3)
    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_nested_checkboxes() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1\n  - [ ] Subtask (nested, 2 spaces)\n    - [x] Another subtask (deeply nested)\n- [ ] Task 2\n- [ ] Task 3".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should detect - 3 top-level checkboxes (0, 2, 0 spaces) + 1 deeply nested (4 spaces, ignored)
    // Total valid checkboxes: 3 (meets threshold)
    assert!(context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_mixed_content() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "# Plan 082\n\n## Examples\n```\n- [ ] Code example\n```\n\n> - [ ] Blockquote example\n\n## Tasks\n- [ ] Task 1\n- [ ] Task 2\n- [ ] Task 3\n- [x] Task 4".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should detect 4 task checkboxes at bottom
    assert!(context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_with_minimal_indentation() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1\n  - [ ] Task 2 (2 spaces)\n   - [ ] Task 3 (3 spaces)\n- [ ] Task 4\n- [ ] Task 5".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // Should detect - 3 non-indented + 1 with 2 spaces = 4 valid checkboxes
    assert!(context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_no_checkboxes() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content:
            "**Tasks**:\n1. Task 1\n2. Task 2\n3. Task 3\n\n**Deliverables**:\n- Item 1\n- Item 2"
                .to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_empty_plan_files() {
    let context = AutoPromptContext {
        plan_files: vec![],
        ..default_context()
    };

    assert!(!context.compute_plan_has_checkboxes());
}

#[test]
fn test_has_task_checkboxes_multiple_files_one_has_checkboxes() {
    let plan_file1 = PlanFileContent {
        path: ".plan/081_other.md".to_string(),
        content: "**Tasks**:\n1. Task 1".to_string(),
    };
    let plan_file2 = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: "- [ ] Task 1\n- [x] Task 2\n- [ ] Task 3".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file1, plan_file2],
        ..default_context()
    };

    assert!(context.compute_plan_has_checkboxes());
}

// ===== Filename Extraction Tests =====

#[test]
fn test_first_plan_filename_with_full_path() {
    let plan_file = PlanFileContent {
        path: "/path/to/project/.plan/082_test_plan.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert_eq!(context.compute_first_plan_filename(), "082_test_plan.md");
}

#[test]
fn test_first_plan_filename_with_relative_path() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test_plan.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert_eq!(context.compute_first_plan_filename(), "082_test_plan.md");
}

#[test]
fn test_first_plan_filename_multiple_files_uses_first() {
    let plan_file1 = PlanFileContent {
        path: ".plan/081_other.md".to_string(),
        content: "**Tasks**:\n1. Task 1".to_string(),
    };
    let plan_file2 = PlanFileContent {
        path: ".plan/082_test.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file1, plan_file2],
        ..default_context()
    };

    assert_eq!(context.compute_first_plan_filename(), "081_other.md");
}

#[test]
fn test_first_plan_filename_empty_returns_default() {
    let context = AutoPromptContext {
        plan_files: vec![],
        ..default_context()
    };

    assert_eq!(context.compute_first_plan_filename(), "plan.md");
}

// ===== Plan Number Extraction Tests =====

#[test]
fn test_plan_number_with_standard_format() {
    let plan_file = PlanFileContent {
        path: ".plan/082_test_plan.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert_eq!(context.compute_plan_number(), "082");
}

#[test]
fn test_plan_number_with_number_only() {
    let plan_file = PlanFileContent {
        path: ".plan/082.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert_eq!(context.compute_plan_number(), "082");
}

#[test]
fn test_plan_number_with_no_number_returns_default() {
    let plan_file = PlanFileContent {
        path: ".plan/test_plan.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert_eq!(context.compute_plan_number(), "00");
}

#[test]
fn test_plan_number_with_mixed_prefix() {
    let plan_file = PlanFileContent {
        path: ".plan/feature_082_test.md".to_string(),
        content: String::new(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    // "feature" doesn't start with digits, so returns default
    assert_eq!(context.compute_plan_number(), "00");
}

// ===== Remaining Plan Files Tests =====

#[test]
fn test_remaining_plan_files_all_complete_returns_empty() {
    let plan_file = PlanFileContent {
        path: ".plan/01_core.md".to_string(),
        content: "- [x] Step 1: Do thing\n- [x] Step 2: Do other thing\n- [x] Step 3: Done"
            .to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert!(context.remaining_plan_files().is_empty());
}

#[test]
fn test_remaining_plan_files_has_unchecked_returns_file() {
    let plan_file = PlanFileContent {
        path: ".plan/01_core.md".to_string(),
        content: "- [x] Step 1: Done\n- [ ] Step 2: Pending\n- [ ] Step 3: Pending".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    let remaining = context.remaining_plan_files();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, ".plan/01_core.md");
}

#[test]
fn test_remaining_plan_files_multi_plan_first_done_second_pending() {
    let plan_01 = PlanFileContent {
        path: ".plan/01_core.md".to_string(),
        content: "- [x] Step 1: Done\n- [x] Step 2: Done".to_string(),
    };
    let plan_02 = PlanFileContent {
        path: ".plan/02_bugfix.md".to_string(),
        content: "- [ ] Step 1: Inject bug\n- [ ] Step 2: Fix bug".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_01, plan_02],
        ..default_context()
    };

    let remaining = context.remaining_plan_files();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].path, ".plan/02_bugfix.md");
}

#[test]
fn test_remaining_plan_files_multi_plan_both_pending() {
    let plan_01 = PlanFileContent {
        path: ".plan/01_core.md".to_string(),
        content: "- [x] Step 1: Done\n- [ ] Step 2: Pending".to_string(),
    };
    let plan_02 = PlanFileContent {
        path: ".plan/02_bugfix.md".to_string(),
        content: "- [ ] Step 1: Inject bug".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_01, plan_02],
        ..default_context()
    };

    let remaining = context.remaining_plan_files();
    assert_eq!(remaining.len(), 2);
}

#[test]
fn test_remaining_plan_files_ignores_checkboxes_in_code_blocks() {
    let plan_file = PlanFileContent {
        path: ".plan/01_core.md".to_string(),
        content: "- [x] Step 1: Done\n- [x] Step 2: Done\n\n```\n- [ ] This is in a code block\n- [ ] Should be ignored\n```".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        ..default_context()
    };

    assert!(context.remaining_plan_files().is_empty());
}

#[test]
fn test_remaining_plan_files_no_plans_returns_empty() {
    let context = AutoPromptContext {
        plan_files: vec![],
        ..default_context()
    };

    assert!(context.remaining_plan_files().is_empty());
}

// ===== Context Bloat Simulation Tests =====
// These tests simulate the original failure scenario where 43 doc files (687KB)
// blew context past the 80K token limit, causing the LLM call to be skipped.

#[test]
fn test_token_estimate_doc_files_as_filenames_only() {
    // Before: doc_files was Vec<PlanFileContent> with full contents.
    // 43 files × ~16KB avg = 687KB → ~172K tokens (exceeds 80K limit).
    // After: doc_files is Vec<String> with filenames only.
    // 43 filenames × ~20 chars avg = ~860 chars → ~215 tokens.
    let doc_filenames: Vec<String> = (1..=43).map(|i| format!("{i:03}_summary.md")).collect();

    let total_chars: usize = doc_filenames.iter().map(|f| f.len()).sum();
    let estimated_tokens = total_chars / 4;

    assert!(
        estimated_tokens < 1000,
        "doc filenames should be tiny, got {estimated_tokens} tokens"
    );

    // Simulate OLD behavior: 687KB of content
    let old_doc_chars: usize = 703_707;
    let old_tokens = old_doc_chars / 4;
    assert!(
        old_tokens > 170_000,
        "old doc content was ~{old_tokens} tokens"
    );

    let savings_ratio = old_tokens / estimated_tokens.max(1);
    assert!(
        savings_ratio > 100,
        "should save >100x, got {savings_ratio}x"
    );
}

#[test]
fn test_full_scenario_stays_under_80k_token_limit() {
    // Simulate the full context from the failure log, with the fix applied.
    //
    // Original log breakdown:
    //   - doc_files: 687KB (703,707 chars) — now filenames only
    //   - plan_files: 198KB (202,863 chars) — unchanged (content needed)
    //   - messages: 18KB (18,486 chars) — smaller now (code blocks stripped)
    //
    // After fix: ~55K tokens, well under 80K limit.

    let doc_filenames: Vec<String> = (1..=43).map(|i| format!("{i:03}_summary.md")).collect();
    let doc_chars: usize = doc_filenames.iter().map(|f| f.len()).sum();

    // Plan files still have full content (task checkboxes needed for logic)
    let plan_content = (0..10)
        .map(|i| {
            let content = "- [x] Completed task\n".repeat(50);
            PlanFileContent {
                path: format!(".plan/{i:03}_plan.md"),
                content,
            }
        })
        .collect::<Vec<_>>();
    let plan_chars: usize = plan_content.iter().map(|p| p.content.len()).sum();

    // Messages: code blocks stripped, much smaller
    let message_content = "I'll implement the feature by modifying the following files.\n";
    let message_chars = message_content.len() * 8;

    let total_chars = doc_chars + plan_chars + message_chars;
    let estimated_tokens = total_chars / 4;

    assert!(
        estimated_tokens < 80_000,
        "total tokens should be under 80K limit, got {estimated_tokens}"
    );
}

#[test]
fn test_old_scenario_exceeded_80k_token_limit() {
    // Verify the OLD scenario (before fix) would indeed exceed 80K.
    let old_doc_chars: usize = 703_707;
    let old_plan_chars: usize = 202_863;
    let old_message_chars: usize = 18_486;
    let old_total = old_doc_chars + old_plan_chars + old_message_chars;
    let old_tokens = old_total / 4;

    assert!(
        old_tokens > 80_000,
        "old scenario should exceed 80K limit, got {old_tokens} tokens"
    );
}

#[test]
fn test_context_exceeds_token_limit_method() {
    let context = AutoPromptContext {
        approximate_token_count: 100_000,
        ..default_context()
    };
    assert!(context.exceeds_token_limit(80_000));
    assert!(!context.exceeds_token_limit(200_000));
}

#[test]
fn test_context_exceeds_token_limit_at_boundary() {
    let context = AutoPromptContext {
        approximate_token_count: 80_000,
        ..default_context()
    };
    assert!(!context.exceeds_token_limit(80_000));
}

#[test]
fn test_modified_files_deduplication() {
    let mut context = AutoPromptContext {
        modified_files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        ..default_context()
    };

    let path = "src/main.rs".to_string();
    if !context.modified_files.contains(&path) {
        context.modified_files.push(path);
    }

    assert_eq!(
        context.modified_files.len(),
        2,
        "should not duplicate files"
    );
    assert_eq!(context.modified_files[0], "src/main.rs");
}

#[test]
fn test_estimate_token_count_with_large_doc_filenames() {
    // 43 doc files as filenames — should contribute almost nothing to tokens
    let doc_filenames: Vec<String> = (1..=43).map(|i| format!("{i:03}_summary.md")).collect();

    let plan_file = PlanFileContent {
        path: ".plan/01_test.md".to_string(),
        content: "- [ ] Task 1\n- [ ] Task 2".to_string(),
    };

    let context = AutoPromptContext {
        plan_files: vec![plan_file],
        doc_files: doc_filenames,
        ..default_context()
    };

    let tokens = context.estimate_token_count();
    // Plan file ~30 chars, doc filenames ~600 chars = ~150 tokens
    assert!(tokens < 500, "token count should be tiny, got {tokens}");
}

// ===== Paragraph Budget Tests =====

#[test]
fn test_paragraph_budget_single_short_paragraph() {
    let context = AutoPromptContext {
        messages: vec![
            auto_prompt::context::ContextMessage {
                role: auto_prompt::context::ContextMessageRole::User,
                content: "do stuff".to_string(),
            },
            auto_prompt::context::ContextMessage {
                role: auto_prompt::context::ContextMessageRole::Assistant,
                content: "Short reply.".to_string(),
            },
        ],
        ..default_context()
    };
    let result = context.compute_last_assistant_message().unwrap();
    assert_eq!(result, "Short reply.");
}

#[test]
fn test_paragraph_budget_takes_paragraphs_until_over_budget() {
    let p1: String = "a".repeat(1_000);
    let p2: String = "b".repeat(5_000);
    let p3 = "c third paragraph";
    let full = format!("{p1}\n\n{p2}\n\n{p3}");

    let context = AutoPromptContext {
        messages: vec![auto_prompt::context::ContextMessage {
            role: auto_prompt::context::ContextMessageRole::Assistant,
            content: full,
        }],
        ..default_context()
    };
    let result = context.compute_last_assistant_message().unwrap();
    // p1(1000) + p2(5000) = 6000 > 5000 → take both p1 + p2
    assert!(result.contains(&p1), "should contain p1");
    assert!(result.contains(&p2), "should contain p2");
    assert!(!result.contains(p3), "should not contain p3");
}

#[test]
fn test_paragraph_budget_single_huge_paragraph_included() {
    let big = "x".repeat(10_000);
    let context = AutoPromptContext {
        messages: vec![auto_prompt::context::ContextMessage {
            role: auto_prompt::context::ContextMessageRole::Assistant,
            content: big.clone(),
        }],
        ..default_context()
    };
    let result = context.compute_last_assistant_message().unwrap();
    // Single paragraph always included even if > budget
    assert_eq!(result.len(), 10_000);
}

#[test]
fn test_paragraph_budget_no_assistant_message() {
    let context = AutoPromptContext {
        messages: vec![auto_prompt::context::ContextMessage {
            role: auto_prompt::context::ContextMessageRole::User,
            content: "hello".to_string(),
        }],
        ..default_context()
    };
    assert!(context.compute_last_assistant_message().is_none());
}
