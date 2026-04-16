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
        iteration_count: 1,
        stop_phase: StopPhase::Working,
        verification_count: 0,
        was_truncated: false,
        plan_has_checkboxes: false,
        first_plan_filename: String::new(),
        plan_number: String::new(),
        first_user_message: None,
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
