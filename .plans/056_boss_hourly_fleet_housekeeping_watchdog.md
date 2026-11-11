# 056 — Boss: hourly fleet housekeeping + false-stop forensics (fallback when auto_prompt & the watchdog both miss)
Status: hardening landed per round 3 (REVISE addressed in `b6bfd21598`) — round 4 verdict next

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
  the loop once the chain halts. The worker never saw the queue: fleet-cache
  carried rigs/jobs only, not repo `.issues` counts.
- Plan 048's watchdog (`agent-mail rig watch`,
  `crates/agent_mail/src/watch.rs`) does not see this either:
  - *Stalled agent* skips auto_prompt-owned sessions (6 h window,
    `mark_auto_prompt_owned`/`auto_prompt_owns`) and a thread that ended with a
    **finished summary is not a stall** by definition.
  - *Idle* rule compares QUIET rigs vs **queued agent-mail jobs**, not repo
    `.issues` on disk.
- Conclusion: three layers (worker judgment, auto_prompt per-thread reflex,
  048 fleet watchdog) all have a blind spot at "ordinary queue unclaimed while
  the rig is quiet". The **boss** is the missing fourth opinion — a fallback,
  not a replacement (plan 050's split stays: auto_prompt = reflex,
  watchdog = fleet incidents, boss = hourly queue ownership + stop forensics
  + stale-mail pickup).

## Owner direction (2026-11-11 — supersedes earlier gating)

1. **Claimed-but-stopped counts**: a claim whose lease expired (holder stopped
   working) makes the item actionable again — `open_item`'s lease-expiry
   semantics already do this; the boss rule adds the complementary live
   check (a live agent/process actually inside the repo means it is worked).
2. **Benchmark routine + boundary check-up** are explicit housekeeping
   checklist items (were missing from the plan's checklist).
3. **Compaction scope**: history .md, AGENTS.md, AND code comments — stale
   references to long-gone issues cost tokens on every read; keep only the
   fact and the critical note, never the full issue narrative.
4. **Implement the remains — no more parking work as owner-gated.** The only
   owner gate is a credential/secret touch to deploy. Everything else that
   would have been "owner-gated" is resolved by asking Claude for the verdict
   (recorded) and self-logging. Codified in `~/.config/zed/AGENTS.md`
   (synced to m3/4090).
5. **Unread mail**: pending mail with no live agent to open it (the m3→amo
   case) is the boss's to open.

## Design (as implemented)

- `crates/agent_mail/src/boss.rs` — the lane, on the watchdog substrate
  (`Incident { fingerprint, detail, min_ticks }`, cooldown, owner escalation
  as-is). Config: `boss: bool` (default **on**; `boss: false` in watch.json
  disables) + `boss_exclude_repos` (default seal-game-editor, sealm-toolkit).
  Cadence: `min_ticks = 6` on the 10-min tick (~hourly); mail rule 3 ticks.
- Rules (deterministic, 0-token, pure over the fleet snapshot):
  - **R-unclaimed-queue** (`boss:queue:<repo>`): repo holds actionable
    `.plans`/`.issues` items (`open_item`: status not done + claim lease
    expired — claimed-but-stopped counts) AND no live agent or busy contender
    inside `/git/<repo>` on any rig AND a QUIET accepts-jobs rig exists
    (m3 excluded via `accepts_jobs`). Fingerprint = repo (stable; the
    cooldown/escalation machinery bounds repetition).
  - **R-false-stop** (`boss:false-stop:<session>`): auto_prompt recorded the
    chain's stop via `boss::note_stop` (`~/.agent-mail/watch/thread_ends/`),
    kind = `stop-keyword-terminal` (the incident class), within 1 h, and that
    session's repo still holds actionable items.
  - **R-unread-mail** (`boss:unread-mail`): aggregate incident for pending
    mail past a 30-min grace in inboxes whose addressee is not live on any
    rig — excluding system inboxes (`_`-prefixed, `jobs`, `watch-*` waiter
    inboxes, owner inbox, local OS user's inbox). Only when a live Zed
    exists on this rig (with none, plan 053's `zed-wake` launchd job owns
    the wake).
- **Action**: boss incidents skip the LLM orchestrator — `deliver_boss` mails
  `*@<target>` (first free agent; local rig for mail incidents, lowest-name
  QUIET rig otherwise) with the bounded housekeeping contract
  (`housekeeping_prompt`): at most 5 checklist units, claim with
  `**Claimed:**` leases, checklist = git hygiene (bmo via `rig run bmo
  --shared`) → boundary check-up → repo queues → mail drain → benchmark
  routine → compaction (history .md + AGENTS.md + code comments, facts and
  critical notes only) → `cargo refine` tiers → research last. The woken
  thread ends with a normal summary; its own all-deferred stop is then the
  correct ending, and the boss may re-fire on new incidents only.
- **W0** (`fleet-cache.json.issues` + `rig::cached_fleet_issues` +
  `peer_states::fleet_context`): every full `rig status` now writes per-repo
  actionable counts; auto_prompt's deciders see "Unclaimed actionable issues:
  katgpt-rs 2 · …" so a "lane empty" summary cannot silently fold the queue
  into "gated". Root-cause fix, ships first.
- **Markers**: `auto_prompt::stop_keyword_outcome` records
  `note_chain_stop` for the three Stopped arms (terminal / repeated / final)
  with cwd + had_summary. Guarded by `cfg!(test)` so tests never write into
  the real `~/.agent-mail`.
- **Deviations from the round-2-agreed design** (owner direction supersedes):
  flag defaults ON (not off+one-week measurement); thread-end markers via
  mailroom files instead of `rig probe` fields (same determinism, less
  plumbing); `*@rig` wake instead of literal new-thread spawn (plan 053's
  spawn/resume machinery takes over once the agent is woken); `rig probe`
  end-reason fields dropped in favor of the markers.

## Do NOT loosen

`AllDeferred → Terminal` in auto_prompt stays (plan-031's "no uninvited
housekeeping from a worker thread" is correct); the boss is the *invited*
housekeeper at the fleet layer.

## Verdict log

- **Round 1 — REVISE** (Claude reviewer, `<session 5cb6c933>`): fix sidebar
  doc-comment misplacement + per-render `rigs.json` I/O + rustfmt (done,
  landed with the UI commit); boss: W0 first, actionable-only trigger with
  id-set fingerprints, registry-based claims, bounded spawn, bmo limits, m3
  excluded, feature flag + false-stop counter, min_ticks=6.
- **Round 2 — AGREE** (same reviewer session): UI fixes verified, plan
  covers every round-1 point. Non-blocking follow-ups: status line rule
  (done), `dot_separator` export (deferred), `.issues/059` for the
  mail-liveness test isolation bug (filed).
- **Round 3 — REVISE** (`<session 2f16b6d4>`): implementation review found
  six defect classes — (a) boss incidents hit the 3-round owner escalation
  (storm + gating-law violation) and the 30-min cooldown is shorter than a
  housekeeping run; (b) unread-mail misclassifications (`orchestrator`/
  `boss` reply sinks, placeholder inboxes, remote human inboxes,
  `deliver_at` aging); (c) false-stop needed 6 ticks but its marker
  survives ~1 h (would never fire) + `cfg!(test)` guard scope + doc-comment
  misplacement; (d) default-on accepted under the gating law; (e)
  `deliver_boss` double-wake (one mail per incident, same resolved agent);
  (f) idle agents in a repo suppressed the queue rule forever. All fixed in
  `b6bfd21598`: never-escalate + 60-min cooldown + work-set `reset_key`
  restarts; per-target combined mail + false-stop subsumes the queue rule;
  config `boss_exclude_inboxes` + `deliver_at` aging + `BOSS_STOP_TICKS=2`;
  deps-dir test guard + restored doc; only generating or <60-min-idle
  agents count as working.
- **Round 4 — pending**: re-verdict of the hardening.

## Tasks

- [x] W0: actionable `.issues` counts → `fleet-cache.json` +
      `peer_states::fleet_context` (root-cause fix; shipped first)
- [x] W1: boss rules in `crates/agent_mail/src/boss.rs`
      (R-unclaimed-queue + R-false-stop + R-unread-mail) behind the `boss`
      flag, `min_ticks = 6`/`3`, quiet per-tick accounting, lease-based
      claims with expired-lease = actionable
- [x] W2: bounded housekeeping prompt + `deliver_boss` (`*@<target>` wake,
      m3 excluded, local-rig targeting for mail incidents)
- [x] W3: bmo lane folded into the checklist (item 1: `agent-mail rig run
      bmo --shared -- <command>`; merged-only branches, cwd-live worktree
      guard, stashes reported not dropped)
- [x] W4: thread-end markers (`boss::note_stop` / `recent_thread_ends`) +
      auto_prompt `note_chain_stop` (deviation from probe-fields design,
      recorded above)
- [x] W5: tests — boss rules (queue/false-stop/unread-mail/lease semantics),
      scan + counts, marker roundtrip, prompt completeness, WatchConfig
      serde defaults, backlog regression (agent_mail 60 + auto_prompt 486
      pass)
- [ ] W6: verdict round 4 on the hardened implementation (round 3 =
      REVISE, addressed in `b6bfd21598`)
- [x] Owner follow-ups folded in: claimed-but-stopped accounting (lease
      expiry + live-in-repo check), benchmark routine + boundary check-up as
      checklist items, compaction scope incl. code comments, un-gating law

## References

- `crates/agent_mail/src/boss.rs` — the lane
- `crates/agent_mail/src/watch.rs` — tick wiring, `deliver_boss`,
  `WatchConfig.boss`, `backlog` (now built on `scan_open_work`)
- `crates/agent_mail/src/rig.rs` — `FleetCache.issues`,
  `cached_fleet_issues`
- `crates/auto_prompt/src/peer_states.rs` — unclaimed-queue context line
- `crates/auto_prompt/src/auto_prompt.rs` — `note_chain_stop`
- `crates/auto_prompt/src/remaining_work.rs` — `SummaryContinuation`,
  `section_all_deferred`, `DEFERRED_ITEM_MARKERS`
- `crates/agent_mail/src/zed_wake.rs` — the no-Zed wake (plan 053), which
  the unread-mail rule defers to
- Plans: 048 (watchdog substrate), 050 (auto_prompt ↔ orchestrator split),
  036 (verdict reviewer), 053 (mail-spawned threads), 055 (tried-steps
  ledger), 031 (all-deferred policy)
