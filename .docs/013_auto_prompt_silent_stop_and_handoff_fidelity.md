# 013: Auto-prompt silent stop + handoff fidelity (plan 031)

- **Follows**: `.docs/008` (unified overflow flow), `.docs/011` (fixed decision + start context), `.plans/027` (summary-first fast path)
- **Commit**: `124d528572` (develop)
- **Status**: implemented and closed

## Why

Two fleet failures, same root: the overflow handoff delegated everything to a
static `CONTINUE_REMAINS_DECISION` with a pre-written housekeeping fallback.

1. A summary whose remaining work is ALL armed/deferred/owner-gated (e.g. the
   T2c-a wash handoff: 32K cooled-window cell, deferred T2c-b, unchanged league
   lanes) still classified as `Steps` — the chain kept going and would spin up
   benchmark/housekeeping work on a hot machine instead of stopping.
2. Sibling-activity lines in the start-context block were hard-cut at 160
   chars mid-word ("…says 24 would-f"), hiding the operative tail and priming
   the worker toward whatever the truncated line mentioned.

## What changed

### `paused` kill switch (silent stop, live)

- `auto_prompt.json` `"paused": true` or env `ZED_AUTO_PROMPT_PAUSED`
  ("0"/"false" = off). The config cache re-validates on file mtime, so
  flipping the flag takes effect on the NEXT chain event without restart.
- Blocks, with a log line each time: orchestrator decide (native
  `decide_precheck` + Claude `decide_claude`), summary fast path (via the
  decide path), overflow Phase 1/2 (`context_overflow_outcome`), and the
  stop-time housekeeping hook.
- Manual clicks still dispatch — explicit human intent overrides the pause.

### Terminal summary detection (`SummaryContinuation::Terminal`)

- `extract_summary_next_steps` now returns `Terminal` when EVERY
  bullet/numbered item in the remaining-work section carries a deferral
  marker (`DEFERRED_ITEM_MARKERS`: armed, defer, owner-gated, gated on,
  owner go, cooldown, cooled, thermal, left to the, unchanged, awaiting,
  parked, on hold, `[-]`). An actionable `- [ ]` checkbox beats the markers;
  a prose-only section is conservatively not terminal.
- Fast path (below overflow gate): Terminal → `Stopped` — chain ends.
- Overflow Phase 2: Terminal → `Stopped` — plan tasks do NOT resurrect a
  terminal chain (deliberate override of plan-027's
  plan-tasks-beat-nothing-left, which still holds for plain NothingLeft).
- `summary_declares_terminal` (pub) is also checked in the `Stopped` arm of
  `run_auto_prompt` to SKIP the once-per-session housekeeping dispatch — a
  terminal stop spawns nothing (no doc-sync, no benchmark re-runs).

### Decision enrichment (keeps the no-orchestrator-call property)

- When the summary names executable remaining work, those steps now ride in
  the Decision slot AHEAD of the fixed directive, both on the same-thread
  fast path and in overflow Phase 2:
  `{steps}\n\n{CONTINUE_REMAINS_DECISION}`. The named task wins over
  re-derivation; the fixed directive stays as the housekeeping fallback.
- A summary with no next-steps section keeps the exact fixed directive.

### `.issues` is a task source

- `plan_source::PLAN_DIR_NAMES = [".plan", ".plans", ".issues"]` — the origin
  scan and the worktree fallback (`read_plan_dir_from_worktree`) both pick up
  issue files, so their `- [ ]` checkboxes count in
  `detect_remaining_plan_tasks` and are visible to the orchestrator. The
  fleet tracks armed levers in `.issues` only (e.g. 771).

### 512-char activity snippets, ellipsis-marked

- `ACTIVITY_SNIPPET_MAX_CHARS = 512` (was 160) and a truncated line now ends
  with `…` (`snippet_with_ellipsis`, char-boundary-safe) — the receiving
  worker can tell a cut line from a complete statement.

## Incident addendum (2026-09-01 22:39): auth-guard false positive

The fleet repo-sync `for r in …` loop tripped `is_interactive_auth_command`'s
substring matcher (`"auth "` matched inside the loop body) and
`is_interactive_tool_pending` — which scanned EVERY terminal tool call since
the last user message — silently returned `NoAction` at a thread stop
(`Auth command detected: 'for r in katgpt-rs riir-ai …', pausing` in Zed.log).
The chain died with no continuation; the owner saw "auto prompt not trigger
at all". Fix:

- Only the MOST RECENT terminal tool call (`latest_terminal_command`) can be
  auth-pending — if the worker ran further commands after an auth-shaped one,
  it was not blocked on it.
- The matcher matches concrete auth invocations at shell-segment starts
  (`INTERACTIVE_AUTH_INVOCATIONS`: `gh auth login`, `gcloud auth login`,
  `az login`, `docker login`, …) instead of substring patterns —
  `gh auth status`, `git -C riir-auth pull`, and sync for-loops never match.
- Guard log upgraded INFO → WARN with resume semantics.
- Missing an exotic auth flow is the safe direction: the chain continues and
  the worker's next turn fails visibly; a false positive killed chains
  silently.

## Tests

- `auto_prompt`: paused config serde roundtrip; fast-path Steps enrichment /
  no-steps exact directive / Terminal→Stopped; `summary_declares_terminal`
  against the real-world wash handoff shape (+ actionable and non-summary
  negatives); Phase-2 Terminal stops despite `.issues` plan tasks.
- `plan_source`: `.issues` files visible from origin refs.
- `agent_ui`: snippet ellipsis + multibyte safety.
- `cargo clippy -p auto_prompt -p agent_ui --all-targets -- --deny warnings` → clean.
