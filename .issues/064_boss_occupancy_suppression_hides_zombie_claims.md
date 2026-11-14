# Issue 064 — boss queue suppression is repo-granular + housekeeping prompts allowed subset sweeps: zombie claims persist while lanes stop "cleanly"

**Status:** OPEN — filed 2026-11-14 (zed-82c2af33@m3, false-stop diagnosis lane, user-directed). Prompt law LANDED `5184c8b09a` (verdict round 1 REVISE applied). All three verdict-approved boss-side fixes LANDED: claim-scoped occupancy `1e71dc232f`, open_item LANDED+grace `adca307669`, bounded re-fire `fb4e95551b` (agent_mail 90/90, clippy clean). Remaining: close-out debt, highwater (bumped to 064), wake-reachability audit, OPEN-without-checkbox parsing.

## The incident

Two m3 chains stopped 2026-11-14 morning while the fleet held claimable, box-actionable work:
- `e951a582` ("Sprint 031 housekeeping boundary doc sync"): valid `<stop>` 03:39:56 → housekeeping pass → second `<stop>` 05:20:01, halt final.
- `14c3ed6a` ("Sprint 039 housekeeping unclaimed issue pickup"): valid `<stop>` 04:45:25 → housekeeping pass → second `<stop>` 04:50:35, halt final.

Both summaries declared emptiness per-lane and labeled the rest "gated / sibling-owned / claimed by live sessions". A four-scanner read-only sweep of all 16 tracked repos' `.issues/` at ~01:31Z found **~22 unclaimed, unblocked, m3-actionable issues** at that moment, including riir-ai 1222 ("Claimed: — none yet"), riir-ai 1220 (plain PoC code), riir-ai 1017 (readout overdue since attempt 4c's ~11-13 deadline), riir-ai 1095 (lever-5 A/B pending since lease closed 10-31), riir-train 623 (explicit m3 lane, follower cap expired), 14 items across seal-remake + seal-sprints, katgpt-rs 1210 (C2–C4/C7 staged-unclaimed). Not a false claim in every repo — riir-infer's zero was genuine — but "no box-appropriate work" was false fleet-wide.

## Why "claimed and silent" persists (mechanism, verified in code)

1. **Worker-side subset sweep** (`crates/auto_prompt/src/remaining_work.rs` `housekeeping_continuation`, old step 4: "unclaimed work in other workspace repos — only where no other agent is active"): one vague line. Workers verified a subset of repos, wrote all-deferred summaries, and the stop was honored — twice in one morning. FIXED this lane: step 4 now mandates the fleet sweep (`agent-mail rig status` / `fleet-cache.json` → per-repo `**Claimed:**` grep → expired lease = claimable → liveness check → claim-and-do, rig work via `agent-mail rig run --est`), and step 5 makes an unproven stop a stated violation.
2. **Boss-side wake contract had the same hole** (`crates/agent_mail/src/boss.rs` `housekeeping_prompt` step 3: "work `.issues`/`.plans` items — resume or resolve until done or gated"): a boss-woken agent could subset-sweep and stop again, wasting the wake. FIXED this lane: step 3 carries the same sweep + claim-liveness law.
3. **R-unclaimed-queue suppression is repo-granular** (`boss.rs` `detect` → `repo_is_worked`): ONE live agent (generating, or idle <60 min) with cwd inside `/git/<repo>` on ANY rig silences the queue fire for the WHOLE repo. Example: the amo session holding live leases on riir-ai 1165/1177 suppressed `boss:queue:riir-ai` while 1197 (lease expired 2026-11-13T16:39Z), 932 (lapsed 2026-11-14T01:50Z), 1222, 1220, 1017, 1095 sat claimable. Occupied-by-anyone = sibling zombie files invisible.
4. **One wake per work-set**: queue incidents carry `reset_key = work_set_key(works)`; the plan-056 contract says a woken thread's own all-deferred stop is "the correct ending" and the boss "may re-fire on new incidents only". A subset-sweep wake that changed no file leaves the set unchanged → no re-fire → the zombie queue persists indefinitely.
5. **`open_item` blind spots** (`watch.rs`): (a) `LANDED` is not in the finished list, so landed-but-open files (riir-ai 1201, 1207 — both LANDED 11-13, files still on disk) count as actionable and inflate the counts workers see; (b) an OPEN file with no `- [ ]` checkbox is invisible (false negatives); (c) `Lease closed:` anywhere suppresses live-claim detection per file — fine — but the fleet convention "remove the issue file when fixed" is not enforced, so closed debt lingers (riir-train 689, riir-ai 1180/1201/1207, riir-game-sdk 059).
6. **Cross-rig wake fragility**: queue fires wake `*@<lowest-name QUIET rig>`; m3 (where the interactive repos live) is `accepts_jobs: false`, and prior routes died unfulfilled ("the 4090 wake did not pick this up — suspected non-draining inbox", riir-reflex 117; related open issue 061 notify-fallback-for-dead-inboxes).

## Tasks

- [x] `housekeeping_continuation` step 4/5: fleet sweep + claim-liveness law + proof-before-stop (this lane, see the fix commit).
- [x] boss `housekeeping_prompt` step 3: same sweep + claim law (same commit).
- [x] Claim-scoped occupancy for R-unclaimed-queue (VERDICT-APPROVED, LANDED `1e71dc232f`): a live agent or busy contender suppresses only files under a LIVE lease — those are already excluded by `open_item`, so occupancy suppresses nothing; `repo_is_worked` + `WORKED_IDLE_MAX_MINUTES` removed. The push-first claim law covers double-booking.
- [x] Bounded re-fire when the work set is unchanged after a wake (VERDICT-APPROVED with cap, LANDED `fb4e95551b`): `BOSS_REWAKE_CAP = 3`, cooldown grows with the round (1×/2×/3× the base hour), past the cap one report escalates to the owner, then quiet until the work set changes.
- [ ] Close-out debt pass: remove landed issue files per fleet convention (riir-train 689; riir-ai 1180/1201/1207; riir-game-sdk 059) or fold their landing notes and delete — owner of each = the landing lane; boss housekeeping contract already carries compaction.
- [x] `open_item` hygiene (VERDICT-APPROVED, LANDED `adca307669`): LANDED joins the finished list position-gated (first status token only — the "OPEN — R1 landed" prose case cannot finish a file); status tokenization drops markdown-marker empty runs; claim grace of 60 min past expiry. The OPEN-without-checkbox parse stays open below.
- [ ] Highwater drift: `.issues/.highwater` reads 059 while 060–064 exist (this file allocated 064 after the upstream 062/063 collision; same drift class as riir-ai issue 1205) — bumped to 064 with this issue's landing; keep it monotonic on every future allocation.
- [ ] Audit wake-target reachability: log-and-verify boss wakes actually drain (ties into issue 061 + the 056 drain round, `02e767737d`); a wake that no live agent picks up should escalate, not cooldown.

## Evidence

- Halt log lines: `~/Library/Logs/Zed/Zed.log` 2026-11-14 03:39:56 / 04:45:25 (`valid <stop> — dispatching one housekeeping pass`) and 04:50:35 / 05:20:01 (`worker emitted the <stop> stop keyword — stopping chain`).
- Fleet cache at incident time: `~/.agent-mail/fleet-cache.json` 2026-11-14T01:27:53Z — 231 unclaimed fleet-wide, 4090 QUIET, m3 QUIET.
- Scan tallies (read-only, 01:31Z): riir-train 2 M3-OK + 25 routable-heavy (4090 idle); riir-ai 5 M3-OK; riir-infer 0 (genuine); mid repos 0 (genuine); katgpt-rs 1210 + riir-dapps 150; seal-remake 7 + seal-sprints 7 M3-OK.
- Suppression rule: `crates/agent_mail/src/boss.rs` `detect` (`repo_is_worked`, L400-421) + `reset_key`/work-set semantics (L476) + plan 056 "Design (as implemented)".
- Plan 056 documented the 2026-11-13 ancestor incident (refine idle loop false-stop) and built the boss as the fourth opinion; this issue records the second-generation leak through the boss's own wake path.
