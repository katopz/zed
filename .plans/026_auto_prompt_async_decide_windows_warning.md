# 026: Async auto-prompt decide + Windows NTFS warning once-per-thread

Operator reports (4):
1. Manual auto-prompt button click freezes UI before "Processing..." shows.
2. War room has no robot icon.
3. Windows: "This command can write to a file on a Windows drive" fires for EVERY git command, even `git status` (regression).
4. War room can't copy text.

## Root causes

- [x] (1) `run_auto_prompt` calls `auto_prompt::decide()` synchronously inside the
      click dispatch. `decide` reads `.plans/` + `.docs/` (sync fs, up to 10×100KB
      files) and builds the full context (every thread entry → markdown) on the
      MAIN thread BEFORE `AutoPromptState::Processing` is ever set (state is only
      set inside the spawned `NeedsLlmCall`/`DispatchAfterDelay` tasks).
      On Windows (Defender scans every open) this is a multi-100ms main-thread stall.
- [x] (2) Robot icon + robot.svg landed in 3316fabc86 (Aug 21). Renders via
      `render_alpha_mask` (svg colors are irrelevant — monochrome tint), verified
      `assets/icons/robot.svg` exists + `icons` crate test covers it. Operator's
      Windows build predates the fix → stale build.
- [x] (3) The sandbox ALWAYS includes worktree abs paths as writable
      (`sandbox_worktree_writable_paths` in terminal_tool.rs). On native Windows
      the worktree is always on `C:\` (DrvFs via WSL) → `contains_windows_fs` is
      true for every sandboxed command → warning prompt per command, by design
      "recurs until disabled in settings". Prompt storm.
- [x] (4) Per-message CopyButton landed in 3316fabc86. Same stale-build
      explanation. (Mouse text selection is not supported for markdown outside
      editors anywhere in Zed — copy button is the established pattern, same as
      the agent panel.)

## Fixes

- [x] (1a) `auto_prompt` crate: split `decide` into `decide_precheck` (cheap,
      main) → `read_plan_files`/`read_doc_files` (pure fs, backgroundable) →
      `decide_finish` (main). New `decide_async` runs fs reads on a background
      executor. Sync `decide` kept as a thin wrapper (same behavior).
- [x] (1b) `agent_ui` `run_auto_prompt`: manual click sets
      `AutoPromptState::Processing` synchronously (instant feedback, paints
      right after the handler returns), then the whole decide+dispatch flow runs
      in one spawned task returned to the caller (decide phase is now
      cancellable via `_auto_prompt_task`). NoAction/DispatchNow arms reset
      state + clear the stored task at the end so no stale handle remains.
- [x] (3) `ThreadSandboxGrants.windows_fs_warning_ack`: recorded when the user
      picks "Continue" on the DrvFs warning; gate skips acked threads. Persisted
      in `DbSandboxGrants` (`#[serde(default)]`, backward compatible). Warns
      once per thread instead of per command; settings toggle still available.

## Verification

- [x] `cargo test -p auto_prompt` — 384 passed
- [x] `cargo test -p agent sandboxing::` — 28 passed (new ack round-trip test)
- [x] `cargo test -p icons` — 2 passed (robot.svg exists + no dangling)
- [x] `cargo test -p agent_board` — 84 passed
- [x] `cargo test -p agent_ui auto_prompt::` — 15 passed
- [x] `cargo clippy -p auto_prompt -p agent -p agent_ui --all-targets` clean

## Operator follow-up (issues 2 & 4)

The robot icon (`assets/icons/robot.svg`, dock `IconName::Robot`) and the
per-message copy button both landed in 3316fabc86. The reporting Windows
build predates it — pull + rebuild on the Windows box and both appear.
Mouse text selection is not supported for markdown outside editors anywhere
in Zed (same as the agent panel); the copy button is the pattern.
