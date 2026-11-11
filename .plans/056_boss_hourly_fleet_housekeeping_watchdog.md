# 056 — Boss: hourly fleet housekeeping + false-stop forensics (fallback when auto_prompt & the watchdog both miss)
Status: round 2 AGREE — W0 next (implementation owner-gated: file at will)

## Why (the incident that triggered this)

Thread `riir-refine idle loop housekeeping resume` stopped "cleanly" while open,
unclaimed issues remained (`/Users/katopz/git/katgpt-rs/.issues` 991–993,
`/Users/katopz/git/riir-refine/.issues` 216–224). Its final summary:

> (3) What remains: all with the m3 lane / the owner — run-7 freeze bench
> (AC-gated …), the ONE combined round-3 re-verdict …, then the send (owner go
> + gh-outbound-pr seal + the two owner rulings). Plan 360/Batch 349 claimed by
> zed-m3-b349. My lane has no open items: 218 yielded, P114 (sec-batch gated),
> P115 (owner-gated), walk #25 Thu.
> (4) … Stopping cleanly — the next unit (if the loop re-fires) is the ordinary
> queue, not 218.

## Root cause (verified in code, not guessed)

- Every `(3)` bullet carried deferral vocabulary (`AC-gated`, `owner-gated`,
  `owner go`) → `section_all_deferred`/`DEFERRED_ITEM_MARKERS`
  (`crates/auto_prompt/src/remaining_work.rs`) →
  `SummaryContinuation::AllDeferred` → `summary_declares_terminal` →
  `KeywordStop::Terminal` in `classify_keyword_stop`
  (`crates/auto_prompt/src/auto_prompt.rs`). Plan-031 policy: **terminal stop,
  no housekeeping pass** — "gated work is not picked up autonomously and
  unrelated housekeeping would run uninvited". The harness worked as designed.
- The worker scoped "what remains" to **its own lane**. The ordinary queue
  (other lanes' `.issues`) is real, actionable, ungated — and nothing re-fires
  the loop once the chain halts. The worker never saw the queue: `rig status`
  feeds fleet-cache (rigs/jobs) but repo `.issues` counts are not in
  `peer_states::unmuted_states_for_context`.
- Plan 048's watchdog (`agent-mail rig watch`,
  `crates/agent_mail/src/watch.rs`) does not see this either:
  - *Stalled agent* skips auto_prompt-owned sessions (6 h window,
    `mark_auto_prompt_owned`/`auto_prompt_owns`) and a thread that ended with a
    **finished summary is not a stall** by definition.
  - *Idle* rule compares QUIET rigs vs **queued agent-mail jobs**, not repo
    `.issues` on disk.
- Conclusion: three layers (worker judgment, auto_prompt per-thread reflex,
  048 fleet watchdog) all have a blind spot at "ordinary queue unclaimed while
  the rig is quiet". The **boss** is the missing fourth opinion — as a
  *fallback*, not a replacement (plan 050's split stays: auto_prompt = reflex,
  watchdog = fleet incidents, boss = hourly queue ownership + stop forensics).

## Boss requirement spec (owner's words, organized)

- **Cadence**: hourly — implemented as `min_ticks = 6` on the existing 10-min
  `rig watch` tick (single system, one launchd job; no second scheduler).
- **Trigger conditions (all must hold)**: no active agent on the rig,
  resources available (`rig status` QUIET + mem/disk), no `--exclusive` job
  running/queued, and **actionable** issues/mail still remain (see trigger
  precision below).
- **Unusual-stop forensics** on recent threads — check for:
  1. stop with **no summary** (no `## Summary` section at end);
  2. **waiting for mail** but stopped instead of housekeeping/scheduled wake;
  3. summary-but-**nonsense stop** (declares lane empty while actionable
     repo `.issues` unclaimed — this incident's class);
  4. other odd ends (stopped mid-plan, plan untouched for N hours while
     ungated `- [ ]` items exist).
- **Response**: create a **new thread** (mail-spawned native agent thread —
  never resurrect the dead chain; auto_prompt owns continuation semantics).
- **Housekeeping checklist** (priority-ordered, bounded per run):
  1. sync git leftovers on all repos (both boxes); clean stale/orphan
     worktrees/branches/temp dirs (`/tmp/<plan>` targets included);
  2. check all repo `.issues`/mail/ongoing — resume or resolve until gated;
  3. ask Claude verdict where required (`request_verdict` / plan 036 reviewer);
     still gated → park with the verdict recorded;
  4. run `cargo refine` fixer tiers on fresh clippy findings;
  5. compact history + doc sync (`.plans/` status lines, BOUNDARY.md drift);
  6. check staked proposals/plans/issues → do them, or file an issue with ref
     hash when the trigger matches;
  7. research: search online papers/blogs for perf/sec + corpus improvements —
     **last**, only inside the remaining budget;
  8. check mail, reorganize/balance task load across rigs;
  9. spy for conflicts/collisions (two agents claiming the same task) →
     referee per 048 duplicate rules;
  10. housekeeping on **bmo** specifically (no Zed IDE/agent lives there).
- **Quiet logging**: one line per hourly tick max ("boss tick: quiet · 2
  actionable issues · no action (cooldown)"); verbose output only when it
  actually acts (new incident → spawn/escalate).

## Design (reuse the 048 substrate — no parallel system)

- `crates/agent_mail/src/watch.rs` gains a `boss` layer reusing the
  `Incident { fingerprint, detail, min_ticks }` machinery, cooldown and
  owner escalation as-is. `min_ticks = 6` gates each rule to an effective
  hourly cadence.
- **Feature flag + keep-it-honest gate**: boss rules ship behind a flag
  (e.g. `agent_mail_boss: bool` in watch.json, default off); measure false
  stops/day with and without the flag for one week; keep the boss enabled by
  default only if the count still justifies it.
- Deterministic 0-token rules (inputs from `rig probe`/fleet-cache ONLY — no
  transcript scraping):
  - **R-unclaimed-queue**: for each repo in `rigs.json`, count only
    **actionable** issues — parse each issue doc's status line (2nd line) and
    `- [ ]` items: skip done/deferred/owner-gated/blocked statuses and
    strikethrough/`[-]` items; skip owner-excluded repos (e.g. seal-* issues
    not authored by katopz). Incident fires when actionable count > 0 AND no
    live claim on any of them AND rig QUIET + resources free + no exclusive
    job. **Fingerprint = the set of actionable issue ids** (so resolving one
    changes the fingerprint; a queue of only gated work never fires).
  - **R-false-stop**: a thread that ended this hour with no summary /
    stop-over-Waiting / terminal-stop-over-unclaimed-queue → incident.
    Requires thread end info (ended_at, ended_by, had_summary) exposed by
    `rig probe` / fleet-cache — W4 ships BEFORE W1 for this reason.
- **Claim source of truth**: the issue doc's own status/claim line plus the
  agent-mail live registry (`rig probe` sessions + jobs). No free-text
  `**Claimed:**` matching — that is the 048 duplicate rule's heuristic, not a
  claim registry.
- **Action**: on new incident, mail-spawn ONE new native thread on the owning
  rig with a **bounded** housekeeping contract: at most N units or a time
  budget per run (pick at implementation), checklist in priority order with
  research last, and the thread ends with a normal summary. Its own
  AllDeferred stop is then the correct ending, not a hole — the boss may
  re-fire next hour only if a *new* actionable queue/stop incident appears.
  Mark boss-spawned sessions like `mark_auto_prompt_owned` does so the
  watchdog's stalled rule skips them (no double-drive).
- **Spawn targets**: QUIET rigs with resources; **m3 excluded** (no jobs).
- **Collision guard**: before spawn, re-check the claim registry (048
  duplicate rule data); the boss thread claims its unit in its issue/plan
  status line; if another agent claimed meanwhile, boss yields and burns the
  cooldown.
- **Escalation**: after 3 unresolved rounds → owner mail (048 escalation
  path). Same fingerprint never re-wakes inside the cooldown.
- **bmo lane**: no Zed agent there → boss runs an SSH maintenance script via
  `agent-mail rig run bmo --shared`, **limited to**: fast-forward sync, stale
  `/tmp/<plan>` dirs, worktrees/branches that are merged AND whose path is
  not any live process's cwd. **Never auto-drop stashes** — report them in
  the tick line / escalation mail instead.
- **Do NOT loosen** `AllDeferred → Terminal` in auto_prompt: plan-031's
  "no uninvited housekeeping from a worker thread" is correct; the boss is the
  *invited* housekeeper at the fleet layer.

## Suggested follow-ups

- [ ] W0 (ships FIRST — root-cause fix, no stop semantics change): write
      per-repo actionable-`.issues` counts into `fleet-cache.json` at
      `rig status` time and surface them through
      `auto_prompt::peer_states::unmuted_states_for_context` (single choke
      point) so workers see the unclaimed queue **before** declaring "lane
      empty" — this alone should make this incident class rare.
- [ ] Deferred: export `ui`'s `dot_separator` so the sidebar id badge reuses
      it instead of drawing its own `•` label (keeps separators
      byte-identical across rows).

## Verdict log

- **Round 1 — REVISE** (Claude reviewer, `<session 5cb6c933>`): fix sidebar
  doc-comment misplacement + per-render `rigs.json` I/O + rustfmt (done,
  landed with the UI commit); boss: W0 first, W4 before W1, actionable-only
  trigger with id-set fingerprints, registry-based claims, bounded spawn,
  bmo limits, m3 excluded, feature flag + false-stop counter, min_ticks=6.
- **Round 2 — AGREE** (same reviewer session): UI fixes verified (incl.
  `rustfmt --check`), plan covers every round-1 point. Non-blocking follow-ups
  filed: status line rule (done), dot_separator deferred (done), and
  `.issues/059` for the mail-liveness test isolation bug (reads real
  `~/.agent-mail`; fix = point `AGENT_MAIL_HOME` at a temp dir).

## Tasks

- [ ] W0: actionable `.issues` counts → `fleet-cache.json` +
      `peer_states::unmuted_states_for_context` (root-cause fix; ships first)
- [ ] W4: `rig probe` / fleet-cache expose thread end (ended_at, ended_by,
      had_summary) — prerequisite for R-false-stop, ships before W1
- [ ] W1: boss rules in `watch.rs` (R-unclaimed-queue + R-false-stop) behind
      the feature flag, `min_ticks = 6`, quiet per-tick log line,
      actionable-only trigger, id-set fingerprints, registry-based claims
- [ ] W2: bounded housekeeping prompt + mail-spawn of the new thread;
      boss-spawn ownership seam (watchdog skips boss sessions); collision
      guard; m3 excluded from spawn targets
- [ ] W3: bmo lane maintenance script via `rig run bmo --shared` (merged-only
      branches, cwd-live worktree guard, stashes reported not dropped)
- [ ] W5: tests — `watch` detect unit tests + e2e for "terminal stop with
      unclaimed actionable queue → boss spawns one housekeeping thread"
      (plan-011-style harness, `advance_clock` for the hourly tick)
- [x] W6: verdict round 2 on this revised proposal before implementation
      (round 1 REVISE addressed; round 2 AGREE)

## References

- `crates/auto_prompt/src/remaining_work.rs` — `SummaryContinuation`,
  `section_all_deferred`, `DEFERRED_ITEM_MARKERS`
- `crates/auto_prompt/src/auto_prompt.rs` — `classify_keyword_stop`,
  `stop_keyword_outcome`, `summary_continuation_fast_path`
- `crates/agent_mail/src/watch.rs` — `tick`, `Incident`, cooldown/escalation
- `crates/agent_mail/src/rig.rs` — `write_fleet_cache`,
  `cached_fleet_summary`, `mark_auto_prompt_owned`
- Plans: 048 (watchdog substrate), 050 (auto_prompt ↔ orchestrator split),
  036 (verdict reviewer), 055 (tried-steps ledger), 031 (all-deferred policy)
