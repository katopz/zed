# 034 — Zed Dev crash fixes bundle & swap

## Status: COMPLETE — swap verified 2026-10-26 (idle-loop records pass)

## Date: 2026-10-24 (swap executed 16:32 local)

## Context
Two crashes fixed on `develop`:
1. `ab0610e12b` — `list_threads` tool double-lease panic (`crates/agent/src/tools/list_threads_tool.rs`)
2. `3d7e9a4c72` — board notification skipped its dense slot → index OOB (`crates/agent_ui/src/entry_view_state.rs:434`)

## Bundle
- `script/bundle-mac` → `target/aarch64-apple-darwin/release/Zed-aarch64.dmg`
- Binary verified: `1.17.0+dev.3d7e9a4c72b55d79f05ce989b2341dc7df91d5ec`
- Ad-hoc signed (`dev.zed.Zed-Dev`), no notarization (no cert env)
- DMG mounted & verified, then detached; app swapped into /Applications by detached script

## Swap sequence (detached script, ~45s after launch)
1. `osascript -e 'quit app "Zed Dev"'` (graceful; saves session DB)
2. Wait for process exit
3. `rm -rf "/Applications/Zed Dev.app"`; `cp -R` new app from dmg staging
4. `open "/Applications/Zed Dev.app"`

## Post-swap verification (2026-10-26)
- App bundle mtimes = Oct 24 16:32:02 (the `cp -R` moment — swap fired as scheduled)
- `/tmp/zed-dev-swap.sh` gone (self-deleted per design)
- Running binary embeds the post-swap hash: `grep -c 3d7e9a4c72` on
  `/Applications/Zed Dev.app/Contents/MacOS/zed` → 1 (both fixes aboard)
- Live agents (this idle loop included) run inside the swapped app — do NOT
  re-run the swap while any agent session is live in it.

## Cleanup note
Staging app at `target/aarch64-apple-darwin/release/dmg/Zed Dev.app` can be
removed with the next target-dir clean (regenerable build artifact).

## Numbering note
Filed as 034 — the on-disk draft had reused 007, already held by
`007_exfat_appledouble_hygiene.md` (dual-allocation class, caught and
renumbered before any commit landed).
