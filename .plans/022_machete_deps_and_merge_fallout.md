# 022 — Machete dep trim + upstream-merge test fallout repair

## Goal

Make `script/clippy` exit green end-to-end by clearing the `cargo machete`
unused-dependency findings, and repair whatever pre-existing breakage surfaces
once `language_models`' test target is actually compiled again (it had not been
built since the upstream sync `cc07356651`).

## Root causes found

- `language_models` carried 5 stale deps (`async-tungstenite`, `gpui_platform`,
  `shellexpand`, `uuid`, `workspace`) added in `d35f53a61f` for the gemini-web
  provider. The provider was removed in `965c560f5d` (captcha-blocked) without
  reverting the manifest.
- `agent_board` carried `smol` + `zed_actions` from its initial commit
  `40299c25f1`; never referenced (realtime client ships as SSE over
  `http_client`).
- Upstream merge `cc07356651` / conflict resolution `8bac6e1b6a` renamed
  `AnthropicModelMode`/`BedrockModelMode` variant `Default` → `Auto` but missed
  3 test sites, so `language_models`/`anthropic` test targets no longer
  compiled (`--all-targets` builds deps' libs, not their tests — which is why
  the earlier `-p agent_ui` gate never caught it).
- `agent_board`'s four `build_response_*` tests mutate the crate-global
  `ROOM_SNAPSHOT`/`WRITER` statics and run in parallel → intermittent
  `build_response_empty_room_has_devices_but_no_states` failure (global
  overwritten between `set_room_snapshot` and `build_response`).

## Tasks

- [x] Verify each machete finding against source + `git log -S` (no
      feature-unification impact: all removed deps declared no features)
- [x] Remove 5 stale deps from `crates/language_models/Cargo.toml`
- [x] Remove `smol` + `zed_actions` from `crates/agent_board/Cargo.toml`
      (kept `workspace` — still used)
- [x] Fix enum-rename fallout: `ModelMode::Default` → `::Auto` in
      `anthropic/src/completion.rs`,
      `language_models/src/provider/{anthropic.rs,bedrock.rs}`
- [x] Serialize global-state tests: `board_state::TEST_LOCK` held for the full
      body of each `build_response_*` test
- [x] Gate: `./script/clippy -p language_models -p agent_board -p anthropic`
      → exit 0 (clippy `--release --all-targets --all-features --deny
      warnings` clean, machete clean, typos/buf not installed → skip by design)
- [x] Tests: anthropic 18/18, agent_board 37/37 across 6 runs (flake gone),
      language_models 144/147 — 3 failures proven pre-existing on clean HEAD
      (see `.issues/014`), flagged not fixed
- [-] Defer: repair the 3 pre-existing `language_models` test failures
      (`.issues/014_language_models_preexisting_test_failures.md`)

## Notes

- Transient `tungstenite-0.27 … can't find crate for thiserror` error appeared
  once mid-session and vanished on rerun; the lock diff was exactly the 7
  removed edges (no version churn). If it recurs, suspect the shared release
  target cache, not the lock.
- `uuid`/`workspace`/`shellexpand`/etc. remain workspace deps — other crates
  still use them; only the stale edges were removed.
