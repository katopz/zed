# 042 — Work Remote: run the built-in Zed Agent on the host
Status: proposed — amo + bmo protocol e2e PASS (2026-10-27, below); option A chosen, not started

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

- [ ] Map `eval-cli --acp` deps: what of `agent_ui`/`terminal_view` it really needs
- [ ] Provider keys: the GUI holds them (keychain on M3); pass the selected
      provider's key to the host agent by env name, never argv
- [ ] Agent picker: under Work Remote on a remote project, the Zed Agent entry
      starts the host agent through the shim instead of the in-GUI loop
- [ ] GUI e2e on amo, then bmo: close M3 mid-turn, reopen, history + live stream
