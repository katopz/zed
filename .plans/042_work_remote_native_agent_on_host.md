# 042 — Work Remote: run the built-in Zed Agent on the host
Status: IN EXECUTION — option A landed server-side 2026-10-29 (task 1 map + the `agent_acp_server` crate + `remote_server agent-acp`; both detach-smoke postures PASS; tasks 2–3 GUI-side next; task 4 needs a human at the GUI)
**Claimed:** zed-m3-idle1029-wr042, 1793246151, expires 2026-10-31T04:30:00+00:00

## Goal

Under Work Remote, closing Zed on the M3 must not stop the **built-in Zed Agent**,
not only external ACP agents. Hosts: amo (`katopz@100.111.186.76`) and bmo
(`katopz@100.100.117.44`), both macOS arm64.

Today (plan 037) only external agents (Claude Code, Codex, Gemini, custom) run
on the host under `zed-agentd`. The Zed Agent's loop, LLM calls and threads.db
stay in the M3 GUI, so they stop when the M3 closes. The host-side server
exists (`eval-cli --acp`, `.issues/036`) but nothing launches it from the GUI.

## Evidence so far

- [x] amo real-host e2e (2026-10-27): M3 → amo over Tailscale SSH,
      `SMOKE_VIA_REMOTE_SERVER=1 SMOKE_SSH="ssh katopz@100.111.186.76"
      detach_smoke.py` with the fork's `remote_server` (zed-agentd embedded) and
      `eval-cli --acp`, both native `aarch64-apple-darwin` debug builds with
      embedded assets, stripped. Client killed 1 s into a 4 s turn → turn
      finished on amo → mid-turn reattach `running=true` → answer →
      `running=false` → later client replays answer + `completed` tool. PASS,
      no leftover processes.
- [x] bmo real-host e2e, same command and binaries (copied from the M3): PASS, no leftover processes.

## Host prerequisites found

- SSH user is `katopz` on both (not `kat`).
- Claude Code over SSH on amo starts logged out: its login is in the macOS
  login keychain, which SSH sessions cannot read (`User interaction is not
  allowed`). Fix without touching the keychain: `claude setup-token`, then
  `agent_servers.claude-acp.env.CLAUDE_CODE_OAUTH_TOKEN` in the M3's settings
  (env reaches the host by name through the shim's `--pass-env`).
- bmo has no node, no brew, no cmake, no Claude login. Building Zed on bmo
  fails (`wasmtime-c-api-impl` needs cmake); build on the M3 and copy instead.
- A debug `remote_server` is ~410 MB stripped; the dev GUI uploads it
  uncompressed by default (`ZED_BUILD_REMOTE_SERVER=nocompress`), minutes over
  Tailscale per rebuild.

## Design options

| Option | Deploy | Cost |
|---|---|---|
| A. `remote_server agent-acp` subcommand serving the native agent (like the embedded `agentd`) | zero extra upload, version-matched | pulls `agent` + `language_models` into remote_server; eval-cli's ACP server must not depend on `agent_ui`/`terminal_view` |
| B. Upload `eval-cli` next to remote_server | second ~650 MB debug binary | duplicates gpui/project in two binaries |

Leaning A, same reasoning as agentd embedding (P4 of plan 037).

## Tasks

- [x] Map `eval-cli --acp` deps: what of `agent_ui`/`terminal_view` it really needs — **DONE 2026-10-29: NONE of the 15 `agent_ui::init` side effects is read by the serve path, and `terminal_view::init` contributes nothing** (the agent's terminal tool runs on `terminal`-crate entities via `AcpThread::create_terminal`; `HeadlessTerminal(true)` makes it a piped subprocess). Runtime globals the serve path reads: `SettingsStore`, `GlobalLanguageModelRegistry`, the lazily-self-initializing `GlobalThreadsDatabase` (`paths::data_dir()/threads/threads.db`; `ZED_STATELESS=1` for in-memory), the NativeAgent-owned `SkillIndex`, and `HeadlessTerminal`. Explicitly NOT read: `PromptStore::global`, `ThreadStore::global`, both metadata stores, `InlineAssistant`, `CommandPaletteFilter` (guarded no-op), `workspace::AppState`. The `SkillsUpdatedHook` is an opt-in notification whose only reader is `try_global`-guarded (settings_ui); the shipped GUI closure would panic headless (dereferences `workspace::AppState::global`) — simply not set here. Landmines honored: `language_model::init` before any model touch (registry panics otherwise); `set_staff` materializes `FeatureFlagStore` so `on_flags_ready` observers can't panic; threads DB needs the paths dir. Consequence: the extract —
      **new crate `agent_acp_server`** (acp_server.rs + model_config.rs moved verbatim from eval_cli via `git mv`/cut, `boot_app` = the minimal-init recipe, `run_server` = env overrides → authenticate-all → serve, `main_from_args` = the subcommand entry; eval_cli's `--acp` branch now calls the same `run_server` + `boot_app` — one server path, two binaries; tests moved with the code, 5 pass; `cargo clippy -p agent_acp_server -p eval_cli -p remote_server` clean). **`remote_server agent-acp --model <provider/model> [--thinking] [--reasoning-effort] [--no-staff]`** dispatched before clap like the embedded `agentd` (cross-platform — no agentd dep). Measured: both `detach_smoke.py` postures PASS (plain `zed-agentd attach -- eval-cli --acp` AND `SMOKE_VIA_REMOTE_SERVER=1` through `remote_server exec` + the embedded-agentd shim) on the refactored path; a direct `remote_server agent-acp` cycle (initialize → session/new → prompt → tool call → "pineapple" → end_turn) PASSes against the fake model with keys by env only. Scope decisions recorded: `languages` "load-grammars" NOT enabled for remote_server (matches its existing editor posture; built-in grammar compilation would bloat the ~410 MB server further) — language-aware tool quality follows the host's existing language support; `prompt_store::init` + the rules→skills migration skipped (serve path never reads the prompt library; the migration is idempotent + KVP-guarded for a later GUI launch); `agent_settings::init_user_agents_md` IS installed (host `~/.config/zed/AGENTS.md` reaches prompts, errors log). Pre-existing failures found + worked around, NOT introduced: `cargo test -p remote_server` fails 2 unix tests on clean develop too (verified by stash); the detach smoke died on ANY change because macOS `/var/folders` mktemp paths exceed the agentd socket's `SUN_LEN` — the harness now mktemps under `/tmp` (one-line fix in `detach_smoke.py`); `cargo test -p agent_acp_server`'s dev-dep graph exposed the latent remote/workspace test-support cfg pair (enum gains `Mock` while workspace's match arm is cfg'd out) — the crate carries `workspace(test-support)` + `remote_connection(test-support)` dev-deps (the eval_cli parity set) which keeps both sides consistent.
- [ ] Provider keys: the GUI holds them (keychain on M3); pass the selected
      provider's key to the host agent by env name, never argv — **server half DONE: the host agent authenticates from the environment (`run_server`'s authenticate-all pass reads each provider's env key; `ZED_OPENAI_COMPATIBLE_PROVIDERS`/`ZED_ANTHROPIC_AVAILABLE_MODELS` overrides work identically to eval-cli). Remaining: the GUI-side half — read the selected provider's key in the Work Remote launch path and inject it into the host env (the `--pass-env`/descriptor env mechanism plan 037 built).**
- [ ] Agent picker: under Work Remote on a remote project, the Zed Agent entry
      starts the host agent through the shim instead of the in-GUI loop
      (launch shape proven by the smoke descriptor: `sh -c 'exec "$ZED_AGENTD_REMOTE_SERVER" agentd attach "$@"' shim -- remote_server agent-acp --model <selected>`; the GUI passes its selected model + provider-key env).
- [ ] GUI e2e on amo, then bmo: close M3 mid-turn, reopen, history + live stream
