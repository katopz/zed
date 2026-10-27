# 039 — Split `auto_prompt.rs` (9,477 lines) into cohesive modules
Status: in progress — refactor only, no behaviour change.

## Why

`crates/auto_prompt/src/auto_prompt.rs` is 9,477 lines: about 5,300 of code
and a 4,200-line inline test module. The repo limit is 3,200 lines per file
(2,048 preferred for `.rs`). The crate is fork-only, so upstream syncs never
touch it and a split costs no merge conflicts.

## Plan

- [ ] Move the inline `mod tests` to `auto_prompt_tests.rs`
      (`#[cfg(test)] mod auto_prompt_tests;`, the `editor_tests.rs` pattern).
- [ ] Extract remaining-work and summary-continuation detection
      (`extract_remaining_section` … `build_checkbox_verification_prompt`).
- [ ] Extract response parsing and plan-landscape helpers
      (`parse_response` … `build_plan_correction_prompt`).
- [ ] Extract plan and doc file reading for context
      (`ContextFileInputs` … `read_doc_files`).
- [ ] `auto_prompt.rs` under 3,200 lines; all `auto_prompt` tests pass with
      the same count; clippy clean.
