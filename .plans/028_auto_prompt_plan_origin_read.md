# Plan 028: Read auto-prompt plan files from origin refs, not the working tree

Status: done

## Problem

`auto_prompt::read_plan_files` lists `.plan`/`.plans` from the working tree of
whatever checkout the dispatcher happens to run in. When that checkout is a
dirty sibling-branch copy (or carries untracked WIP), the dispatcher ranks and
decides against stale plan contents. This exact trap forced a manual re-read in
a prior thread, and the skill-layer workaround (`riir-clippy/scripts/plan_read_origin.sh`)
only covers agents that follow the skill — Zed's built-in dispatcher still reads
the working tree.

The originally proposed fix ("read against `origin/main`") is itself wrong for
this workspace: the active integration branch is `origin/develop`, and in this
fork `origin/main` is upstream Zed, which has no `.plans/` at all. Hardcoding
either single branch loses: main-only → zero plans; branch-tip-newest-only →
wrong per-file contents when a hotfix lands only on the older-tip branch.

## Requirements

1. Plan selection must never read the working tree when the repository has
   remote-tracking refs that carry `.plan`/`.plans` entries.
2. Resolution is **per file, not per branch**: each plan file is read from the
   candidate ref where it was last touched (committer date). Candidate refs in
   preference order: `origin/develop`, `origin/main`, `origin/master`, plus the
   remote's default branch via `origin/HEAD` when resolvable.
3. File present on exactly one candidate ref → that ref wins (presence beats
   preference).
4. Identical blob on multiple refs → preference order (no extra git spawns).
5. Best-effort `git fetch origin` before resolution on the background path
   only (`decide_async`), gated to at most one attempt per repo per 60s,
   non-interactive, hard 10s timeout; failure degrades to local remote-tracking
   refs. Sync/main-thread callers never fetch.
6. Per-decision snapshot cache (30s TTL) so repeated stops don't re-spawn git.
7. Provenance logged: per-repo ref tips at info, per-file ref + blob (and
   commit when date resolution ran) at debug.
8. Working-tree read remains only as fallback when no candidate ref carries
   plan entries (non-git dir, no remote, or plans not yet pushed) — preserving
   current behavior for ordinary repos and fresh checkouts.
9. Claim keys (`plan_registry`) keep the absolute-worktree-path string format,
   so cross-agent claims still match for origin-read plans.
10. Guard test: on a dirty non-main checkout with stale local edits, untracked
    files, and per-file divergence across refs, selection yields exactly the
    per-file newest origin contents and ignores the working tree.

## Tasks

- [x] Add `crates/auto_prompt/src/plan_source.rs` (git-backed plan source).
- [x] Rework `read_plan_files` to source from origin refs with worktree
      fallback; async + cached variants; update `decide`/`decide_async`/
      Claude parity + agent_ui prewarm call sites.
- [x] Add guard + fallback + oversize + binary + subdir tests; `tempfile`
      and `smol` deps.
- [x] `script/clippy -p auto_prompt(-p agent_ui)` clean; `cargo test -p
      auto_prompt` green (393+40+6).

## Notes

- Workspace clippy disallows blocking `std::process` methods, so all git
  access uses `smol::process` (async). The fetch timeout uses the gpui
  background-executor timer (`smol::Timer` is disallowed workspace-wide).
- The Claude parity paths run synchronously on the main thread;
  `agent_ui::run_auto_prompt` now awaits a snapshot prewarm before calling
  `decide_claude`, so the synchronous read hits the TTL-fresh origin cache
  instead of falling back to the worktree.
- Real-repo smoke: in this fork `origin/main` (upstream Zed) carries no
  `.plans/` — hardcoding it would have returned zero plans. Per-file newest
  resolution across `origin/develop` + `origin/main` is the correct semantic.
