# 057 — Tiered job queue: est + urgent proposals, in-place retier (boss de-jam)
Status: DONE — zed f66b3e1646 on develop, deployed amo+bmo 2026-11-12 17:5x; bmo queue re-tiered live (10 jobs), submitters notified

## Why (the incident)

bmo's queue at 17:03 (2026-11-12): a 26 h CPU run (`6a300a3e`) holding the rig,
10 jobs FIFO behind it — a 30 min canary at the head blocking even the shared
sweeps, two multi-hour cells (i1190 ablation #4, 614 C_K #9) ahead of a 1 h
pre-flight probe (ABBA #10) that needed to run BEFORE them. Queue order was
submission order (UUIDv7 filename sort); the only reorder paths were
cancel+resubmit (silent to submitters: `cancel_job` just `remove_file`s, no
mail; new id breaks proposer bookkeeping) or renaming files (the id IS the
order). Neither is lossless → "can we rearrange without breaking?" was NO.

## Owner decree (2026-11-12)

- Queue order = proposal-driven, not submission-driven: tiers
  **minutes (<60 s) → hours (<24 h) → days**, shortest estimate first inside a
  tier; the boss rearranges by estimate; proposers estimate time-used on every
  `rig run`.
- Only a human-approved `[urgent]` mail-title prefix may break the rule;
  urgent jobs run FIFO among themselves (estimate ignored — the approval
  already ranked them).

## What shipped (zed f66b3e1646, crates/agent_mail + crates/rig_panel)

- `JobSpec.est_secs` + `JobSpec.urgent` (`#[serde(default)]` — mixed-version
  safe both directions).
- `sort_queue`: the effective order, applied by the scheduler AND `rig status`
  AND the panel — display equals admission. Key: urgent rank 0 (FIFO by
  submitted_at), then tier rank, then `est_secs.unwrap_or(1 h placeholder)`,
  then submitted_at, then id.
- `rig run --est 30m` (parse_duration) places the job; `--urgent` is refused
  unless `--title` starts with `[urgent]` (`ensure_urgent_title`).
- `rig set <RIG> <ID> [--est D] [--urgent|--deurgent]` → `set-local` over SSH:
  rewrites the queued spec in place (atomic staged write). No cancel, so id,
  key, notify all survive. Running jobs refuse (past admission). Nudges
  `schedule_jobs().ok()` like the executor end path — embedded hosts (rig
  panel) re-tier without starting jobs.
- `rig status` lines: `#k <id> <Lane> · [URGENT · ]<tier> · est <d> · title`.
- Panel (rig_panel): meta shows `URGENT` (warning), `tier · est`, `over est`
  (warning, running past estimate), `no est` (muted — discipline gap for the
  boss to fill). Rebuild of Zed picks it up; old Zed still sees the new info
  via JobLine.text/tooltip from new-binary probes.

## Semantics kept

- Exclusive head-of-line blocking unchanged (first exclusive in SORTED order
  blocks; shared jobs ahead of it still backfill).
- Un-estimated job = hours tier with a 1 h sort placeholder: omission neither
  starves nor outranks an honest estimate.
- Submission order (UUIDv7) remains the final tiebreak — pure-FIFO for equal
  keys.

## bmo live rearrangement (2026-11-12 ~17:50)

- New binary: amo `~/.cargo/bin/agent-mail` replaced (backup
  /tmp/agent-mail.bak-20261112); bmo via scp over the bridge (bin only —
  source never copied) + atomic mv; old 16 h kicker killed so the next
  admission tick runs new code. The running executor keeps its old inode and
  its end-of-job sweep is cooldown-blocked, so no old-order admission is
  possible.
- Re-tiered: probes 45 s ×3 (#1-3), git-sync 5 m (#4), sweeps 15 m ×2 (#5-6),
  canary 30 m (#7), ABBA 1 h (#8), i1190 6 h (#9), C_K 8 h (#10).
  Verified via `rig status bmo`.
- Submitters fyi'd: zed-9665f823 (canary), zed-c21902cf (i1190),
  zed-77d19c15 (probe + C_K), zed-abcf2935 (ABBA).

## Verification

- `cargo test -p agent_mail` 89+6 pass (new: tier mapping, SJF+tiebreak order,
  urgent FIFO, retier round-trip, set-flag parsing, urgent title gate);
  `cargo test -p rig_panel` 2 pass; clippy clean on both crates.
- Live: `rig set` round-trips over SSH, status renders tiered order.

## Post-drain verification (2026-11-12, zed-061faf21@amo)

The bmo queue emptied after the re-tier and was suspected wiped. Verified NOT
a bug: all 10 re-tiered jobs + the 26 h `6a300a3e` head have terminal records
in `~/.agent-mail/jobs/done/` on bmo (46 records, 379 logs retained; queue/
and running/ legitimately empty). Tiering behaved as designed: shared sweeps
backfilled during the exclusive head, probes/canary first after it, cells
last. Outcomes: 7 done (exit 0), 1 failed exit 101 (i1190 katgpt-core
staging conflict — logged in riir-infer 1190), 3 failed exit None
(worktree-scoped, killed by bmo's riir-infer checkout being in a broken
`HEAD (no branch)` state — repaired on-box: switched to develop, ff-pulled,
worktree-add round-trip OK). Regression found: `rig log <SHORT-ID>` no
longer resolves prefixes in the 057 binary — filed as `.issues/060`.
Fleet clock note: bmo's wall clock reads ~Nov 13 while amo reads Nov 12 —
timestamps in job records/mails are bmo-clock; the skew predates 057.

## Open (deferred, not blocking)

- [ ] Other rigs (m3, 4090) still run the old binary — upgrade opportunistically; mixed-version is serde-safe, only sorting needs the new binary
- [-] Anti-starvation aging for the days tier (a stream of minutes jobs delays days jobs indefinitely) — accepted for now per the decree; revisit if it bites
- [-] Boss (watch.rs) could auto-estimate known job classes and retier on incidents — proposal only
- [ ] Zed GUI rebuild to show the panel meta additions in-app (text/tooltip already carry them via new-binary probes)
