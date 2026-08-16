# Plan 016 — Unity MCP for Zed native agents (GLM) — record & runbook

Status: **DONE** (Aug 16, 2026) · Fix commit: `riir-unity@develop 37116f2` · Verified live end-to-end

## Original task

Native Zed agents (GLM via z.ai, OpenAI-compatible provider) must use the Unity MCP
tools exactly like the Claude agent does — in Zed, with no proxy/adapter. Symptom
observed: "unity mcp is exposed for Claude Agent but native agent GLM can't see it".

## Verdict

No proxy, no adapter, **no Zed code changes needed**. Zed's native MCP client stack
serves all native agents; the outage was a Unity-side missing package. "Claude works"
was an illusion (stale cached tool lists + handshake-only health check).

## Architecture (verified in zed source + live probes)

```
GLM native agent ── Zed context_servers (settings.json)
                          │ stdio spawn
                          ▼
                relay_mac_arm64 --mcp        (MCP server mode)
                          │ localhost
                          ▼
                relay_mac_arm64 --relay      (WebSocket :9001 + REST :9002,
                          │                   spawned by Unity Editor via
                          │                   com.unity.ai.assistant package)
                          ▼
                Unity Editor (Project Settings → AI → Unity MCP)
                ~/.unity/mcp/connections/bridge-*.json = registration
```

External agents (claude-agent-sdk) get the SAME servers: Zed injects
`--mcp-config` built from `context_servers` (observed in live `ps` output), so
Claude and GLM consume the identical `unity-mcp` relay entry.

### Zed source receipts (grep evidence)

- `crates/context_server/src/transport.rs` — first-party MCP client, stdio + HTTP(SSE) transports
- `crates/settings_content/src/project.rs` `ContextServerCommand` — keys: `command`/`args`/`env`/`timeout` (untagged enum: `command`→Stdio, `url`→Http)
- `crates/agent/src/thread.rs:4143` — context-server tools gated by `profile.is_context_server_tool_enabled()`
- `assets/settings/default.json:1182` — default `write` profile ships `enable_all_context_servers: true` → profile gating is NOT a blocker for the default setup
- `crates/agent/src/agent.rs` — `NativeAgent` subscribes to project `ContextServerStore` (all native models share it)

### Unity-side receipts

- Relay path is official first-party: `~/.unity/relay/relay_mac_arm64.app/Contents/MacOS/relay_mac_arm64` (Unity blog "unity-ai-mcp-how-to-get-started" + `com.unity.ai.assistant@2.11` docs name this exact path)
- `relay --help`: `--mcp` (client side) vs `--relay` (server side, `--editor-pid`, ports 9001/9002)
- Package provides Project Settings → AI → Unity MCP page, Unity Bridge, Pending Connections approval
- 3rd-party decoys identified and rejected: `https://ai-game.dev/mcp` (IvanMurzak "AI Game Developer" — user-confirmed deprecated), `com.unity.ai.assistant` is NOT 3rd-party despite suspicion

## Timeline & git forensics (riir-unity)

- **Aug 13 night** (editor PID 36488): package installed locally (uncommitted manifest edit) → `Editor-prev.log` shows relay server up (`editorPid:36488`, ports 9001/9002, "Unity client connected"). Claude configured via Unity's *Configure Claude Code* (wrote `unity-mcp` into `~/.claude.json`) and approved. **This was "Claude works".**
- **Aug 14 14:51** (PID 40120): session loaded the package via stale `packages-lock.json`, then lock rewritten WITHOUT it (manifest never listed it in git).
- **Aug 14 15:04**: commit `a284874` "chore(unity): commit Unity-generated packages-lock.json" — committed the pruned lock, cementing the wipe. Manifest edit itself was lost to git cleanup during heavy sibling-agent activity (13:53–15:24).
- **Aug 14–16**: bridge dead for EVERYONE. Proof it was dead for Claude too: all live relay processes (4 Claude-owned, 2 Zed-owned) had **zero TCP connections**; fresh `relay --mcp` handshake returned `{"tools":[]}`; `claude mcp list` still showed `✓ Connected` because that check is handshake-only.
- **Aug 16**: root-caused → reinstalled → bridge up (`bridge-0e2a693f-53937.json`) → `tools/list` = **54 tools** → live `Unity_ManageEditor GetState` round-trip from a GLM native session succeeded → commit `37116f2` (manifest + lock) pushed.

## Root cause

The Unity MCP bridge ships ONLY with `com.unity.ai.assistant`. Its install was a
local, **uncommitted** manifest edit; git operations wiped it; a sibling commit
locked the pruned state. Missing package → no relay server → `tools/list` = `[]`
for every client. Native agents surfaced this honestly (live tool query per turn);
Claude sessions masked it (cached tool lists + handshake-only health).

## Runbook — what it takes (reproduce/repair in ~2 min)

- [x] Add `"com.unity.ai.assistant": "2.17.0-pre.1"` to `mmorpg-template/Packages/manifest.json` deps
- [x] Get Unity to resolve (focus editor; UPM watcher unreliable for external edits — Cmd+R or restart if inert; `upm.log` confirms activity)
- [x] Confirm bridge: `lsof -iTCP:9001 -iTCP:9002 -sTCP:LISTEN` and `ls ~/.unity/mcp/connections/`
- [x] One-time client approval: Project Settings → AI → Unity MCP → Pending Connections → Allow (per client, remembered; Unity security design, not bypassable client-side)
- [x] Zed side: `context_servers.unity-mcp` = `{ "command": "<relay path>", "args": ["--mcp"] }` — was already correct; restart Zed only if tools don't appear after bridge start
- [x] Verify: `tools/list` returns 54 tools; call `Unity_ManageEditor Action=GetState` from a native agent
- [x] **Commit manifest + packages-lock.json immediately** (uncommitted install = the outage)
- [-] Cleanup dead 3rd-party entries: `deprecated-unity` (Zed settings), `ai-game-developer` (`~/.claude.json`) — deferred, harmless
- [-] Multi-editor pinning: `"args": ["--mcp", "--project-path", "<abs path>"]` — deferred until >1 editor is routine
- [-] macOS single-session guard for relay (forum workaround is PowerShell) — deferred; only matters with many concurrent agent windows

## Gotchas

1. Relay tolerates **one client session** (non-Pro) — extra Zed agent windows can drop connections; first session wins.
2. `claude mcp list` `✓ Connected` ≠ tools present (handshake-only check). Always verify with a fresh `tools/list`.
3. UPM may ignore external `manifest.json` edits until editor focus/refresh/restart.
4. The package's paid part (AI chat subscription) is never touched; the MCP bridge is local & free (open beta).
5. Zed tool names are sanitized for providers (`.`→`_`); collisions get `<server_snake>_<tool>` prefix (e.g. `mcp_server_zai_zread_read_file`).
