# 035 — Upstream sync runbook
Status: active — Cargo.lock recipe verified against the 2026-10 sync (upstream `6b985a8b26`)

How to merge `upstream/main` into `develop` with the fewest hand-resolved
conflicts. Background and measurements: `.plans/038_upstream_conflict_isolation.md`.

## Before the merge

1. `git fetch upstream && git pull --rebase` on `develop`.
2. Predict the cost: `script/fork-conflict-sim HEAD <upstream tip> <old merge-base>`
   lists conflicted files, hunks and lines inside conflict regions. Region lines
   track resolve time better than hunk counts.
3. `script/fork-surface` lists the fork lines still living in upstream files.
   If a file near the top is also hot upstream, split it first (below).

## During the merge

`git merge upstream/main` (no rebase: the fork history is long and shared).

### Cargo.lock: regenerate, never hand-merge

```sh
git checkout --theirs Cargo.lock
cargo metadata --format-version 1 >/dev/null   # adds the fork-only deps to the lock
git add Cargo.lock
```

Upstream's lock already pins every shared crate; cargo only adds the entries
the fork's `Cargo.toml` files need. Verified on the 2026-10 sync: upstream's
lock plus `cargo metadata` matched the fork's committed lock byte for byte.
Do this **after** resolving every `Cargo.toml` conflict, because cargo
resolves from the manifests.

Avoid `cargo update`: it bumps unrelated crates and turns the merge into a
dependency upgrade.

### Cargo.toml

Resolve by hand: keep upstream's lines and re-add the fork's dependency lines
(the 2026-10 replay had 5 hunks in the root `Cargo.toml`, 2 in
`crates/language_models/Cargo.toml`).

### Rust files

Fork code inside upstream files follows the seam pattern from plan 038:

- Fork-only items live in a child file (`<file>/fork.rs`, `<file>/tests/fork_tests.rs`)
  reached through `mod fork; use fork::*;` or `mod fork_tests;`. Upstream never
  touches these files, so they never conflict.
- Fork state hangs off one `fork` field (`ForkThreadViewState`,
  `ForkAgentPanelState`, `ForkAgentSettings`, ...). Upstream's new fields go next to it.
- Fork behaviour inside upstream functions is a single call. On a conflict,
  take upstream's side and re-add the one-line call.

When resolving, keep upstream's version of any line that is not a seam.
If a resolution needs more than a seam line, move that code into the fork file.

## After the merge

1. `./script/clippy`, then the tests of `agent_ui`, `agent`, `acp_thread`, `language_models`.
2. If a file conflicted heavily, split its new fork code out with
   `script/fork-split-items` and record the before and after numbers from
   `script/fork-conflict-sim` in plan 038.
