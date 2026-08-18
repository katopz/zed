# 015 — Upstream sync prep: 369 commits behind upstream/main

Created: 2026-08-19
Status: OPEN

## Situation

- `develop` = `e07b4105f0` — **359 ahead** of merge-base
- `upstream/main` = `00c0e96e76` (Make opening large files use less peak memory #62748) — **369 ahead** of us
- merge-base: `aba12fc8a0fe` ("Bump Zed to v1.14.0 (#61488)") — clean release boundary
- 26 of 36 fork-touched crates also changed upstream since merge-base

## High-risk collision areas (upstream commits since merge-base)

- `crates/language_models` (17 upstream commits)
  - `9f164a0d2e` #62652 "Share compatible Chat Completions infrastructure" — rewrites
    `provider/open_ai_compatible.rs`, exactly where Issue-007 key-health persistence hooks
    the `State` ctor (`Fs::global` load at construction, ~line 550 pre-merge).
    Fork-only dir `provider/open_ai_compatible/` (health.rs) has no upstream counterpart;
    the conflict is at integration points in `open_ai_compatible.rs` (ctor, error paths, module wiring).
  - `e4ac280d48` #61787 "Move OpenAI subscription code to separate crate" — file moves around
    the provider registry (`register_compatible_providers` area).
  - `c28cf645f9` #61370 explicit OpenAI compaction — touched `open_ai_compatible.rs`.
  - Structure note: upstream `open_ai_compatible` is a single file; fork adds the subdir —
    no upstream deletion risk for `health.rs` itself.
- `crates/project` (46 commits, highest churn), `agent` (15), `agent_ui` (24), `zed` (24), `ui` (10)
  - Re-verify fork's project/zed changes from plans 013/015/021 (board integration, resume toast).
- Workspace `Cargo.toml`: fork added members (`auto_prompt`, `agent_board`) — expect trivial
  members-list conflict.
- No upstream equivalent for fork features — keep carrying:
  usage-limit retry scheduling (plans 018-021), `auto_prompt` crate, `agent_board`,
  GitHub device flow (015). Grep of upstream log for "usage limit"/"ModelMode" = zero hits.

## Pre-flight checklist

- [ ] `git fetch upstream` — re-check drift vs this doc
- [ ] `GIT_EDITOR=true git merge upstream/main` on `develop` (non-interactive)
- [ ] Resolve conflicts, priority: language_models → agent → agent_ui → project → zed → rest
- [ ] Re-verify Issue-007 health-persistence integration after #62652 refactor
      (State ctor Fs load, backoff save/load, health.rs module wiring)
- [ ] Verify ModelMode naming consistency (previous fallout: `e7b82018a6`)
- [ ] `CARGO_TARGET_DIR=/tmp/sync_target cargo test -p language_models --lib` (expect 147/147)
- [ ] `CARGO_TARGET_DIR=/tmp/sync_target cargo test -p agent_board -p auto_prompt`
- [ ] `./script/clippy` gate exit 0
- [ ] Clean `/tmp` target when done
- [ ] Live GOAT (user-side, carried from plans 020/021/022): one real session-limit cycle +
      one weekly/opus cycle end-to-end

## Notes

- Repo lives on exFAT — see `.docs/007_exfat_appledouble_hygiene.md`; run the sidecar cleanup
  before the merge so git output stays readable:
  `find . \( -name '._*' -o -name '.DS_Store' \) -delete`
