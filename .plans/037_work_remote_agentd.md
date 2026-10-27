# 037 — Work Remote: GUI on M3, agents + compute detached on M5 (zed-agentd)
Status: in progress — P0 sync + compile fallout fixed, P1 daemon + P2 hook/menu implemented, journal bench GOAT (−92% append), P4 journal gc + native agent backend (`eval-cli --acp`) + live turn-state on reattach + auto-deploy (agentd embedded in remote_server) done and smoke-verified locally; P3 on-host e2e still blocked on M5 SSH access (owner)

## Goal

M3 runs the Zed GUI only. M5 (over Tailscale SSH) owns files, terminals, LSP
**and the agent loop**. M3 may close at any time; the agent turn keeps running
on M5; reopening M3 reattaches and shows the live/finished thread.

## Verdict: does stock "Open Remote" cover this? No.

Research against upstream `6b985a8b26`:

| Concern | Stock Open Remote |
|---|---|
| files / LSP / git / tasks | on M5 ✅ |
| terminals, external ACP agent processes | on M5, but children of M3's `ssh` → die with M3 ❌ |
| native Zed agent loop + LLM calls + threads.db | **on M3** (client) ❌ |
| remote_server after GUI quit | `ShutdownRemoteServer` on quit; cold reopen kills old server; 10 min idle ❌ |
| reattach to a turn in flight | impossible — no thread/turn proto ❌ |

Gap = a host-side process that owns the agent and outlives the SSH session.

## Design (GOAT: smallest upstream footprint)

```
M3 Zed GUI ──ssh──▶ remote_server exec ─▶ sh shim ─▶ zed-agentd attach ──unix sock──▶ zed-agentd serve (setsid, persistent)
                                                  (dies with M3, harmless)                │  journal ~/.local/share/zed-agentd
                                                                                           └─▶ ACP agent (claude/codex/gemini/...)
```

- `crates/agentd` (fork-owned, std-only, no gpui): ACP JSON-RPC **mux**.
  - Pure state machine `hub.rs` (`HubEvent` → `HubOutput`), IO in `daemon.rs`.
  - Journals every `session/update` + user prompt per session (JSONL, blake3-named).
  - Answers `session/list` + `session/load` itself (replay, text chunks coalesced);
    hides backend `resume/fork/close` so all reattach goes through the journal.
  - Tells the backend the client has **no fs/terminal** capability → agent uses its
    own tools on M5 → a detached turn never blocks on the GUI.
  - Permission requests when detached: `wait` (queue until reattach, default) |
    `allow` | `deny` (`--detached-permission` or `ZED_AGENTD_DETACHED_PERMISSION`).
  - Backend restart → journaled sessions restored silently via backend `session/load`.
- Zed hook (fork-owned file `agent_servers/src/acp/detachable.rs`): when the
  `DetachableRemoteAgents` global is on and the host is POSIX, wrap the remote
  agent command in a `sh` shim that execs `zed-agentd attach -- <agent>` (falls back
  to the agent directly if agentd is not installed). Env passed by **name**
  (`--pass-env`) so secrets never hit the host process list.
- `crates/work_remote` (fork-owned): actions `work_remote::WorkRemote` (enable +
  open the SSH picker) and `work_remote::DisableDetachableAgents`; persisted in KVP.

Upstream-file touches (the whole conflict surface of this feature):
`Cargo.toml` (3 lines), `crates/zed/Cargo.toml` (1), `crates/zed/src/main.rs` (1),
`crates/zed/src/zed/app_menus.rs` (1), `agent_servers/src/acp.rs` (1),
`agent_servers/src/agent_servers.rs` (1), `agent_servers/src/acp/transport.rs` (1 call).

## Setup on M5

Nothing to install when Zed uploads its own remote server (dev builds,
`build-remote-server-binary`, `ZED_COPY_REMOTE_SERVER`): agentd is embedded in
it. A downloaded stock server lacks it; then install it once:

```sh
cargo install --path crates/agentd         # → ~/.cargo/bin/zed-agentd (shim also probes ~/.local/bin)
zed-agentd status                          # sessions JSON (live/attached/queued)
~/.zed_server/zed-remote-server-* agentd status   # same, via the embedded copy
```

On M3: File → **Work Remote…** → pick the M5 host (Tailscale IP) → open project →
start any external agent. Close M3 freely; reopen and pick the thread from history.

## Tasks

- [x] P0 sync `upstream/main` (1038 commits) into `develop`; resolve 54 conflicted files — merge `f4e142ba98`, docs `9399fffcb6`
- [x] P0 fix merge compile fallout (auto_prompt, thread_mailbox, multi-key health, verdict tools); post-merge test fixes: compaction idle-timeout assert reads the error chain, obsolete checkpoint test dropped (upstream `eee7501970` removed checkpoints), watchdog HALT keeps its own task alive across upstream's new cancel flow (`_watchdog_recovery_task`), `RealFs::new` double-Arc. Flake → `.issues/035`
- [x] P1 `crates/agentd`: hub state machine + journal + daemon + attach + CLI
- [x] P1 unit tests (9): detach mid-turn → journal → replay; permission wait/requeue/allow; fs/terminal reject; backend restart restore; stale generation
- [x] P1 e2e smoke (fake ACP agent): GUI killed after `step0`, turn finished `step5` detached, 2nd GUI got list + full replay
- [x] P2 `detachable.rs` hook + shim (fallback-tested) + `work_remote` crate + menu item
- [ ] P3 real e2e: M3 Zed → M5 over Tailscale with Claude/Codex ACP; close M3 mid-turn; reopen; verify history + live stream
  - BLOCKED (2026-10-27, access): tailnet macOS hosts `amo` (100.111.186.76) refuses ssh (Remote Login off) and `bmo` (100.100.117.44) rejects M3's key. Needs owner: enable Remote Login on the M5 + add M3's `~/.ssh/id_ed25519.pub` to its `authorized_keys`
- [x] P3 verify Zed's reconnect flow calls `session/load` (not only `session/list`) for external agents on window restore — by code: `conversation_view.rs` (~L1420) calls `load_session` whenever the connection reports `supports_load_session()` (= the `loadSession` capability, `agent_servers/src/acp/v1.rs`), which zed-agentd always advertises; `resume_session` (no history) is only the fallback for agents without it. The live confirmation rides on the P3 e2e item
- [x] P4 native Zed agent as a backend: `zed-agentd attach -- eval-cli --acp` (`c212b13be4`, harness `767ba226e9`); verified locally incl. kill-mid-turn detach + replay. Structured diffs + model switching over ACP done; follow-up (real-host run) in `.issues/036_native_agent_acp_stdio_server.md`
- [x] P4 live turn-state on reattach: zed-agentd sends the extension notification `_zed-agentd/turn_state {sessionId, running}` — `true` right after a journal `session/load` while that session's prompt is still in flight, `false` to exactly those clients when the prompt answers or the backend exits (`turn_watchers`). The fork GUI takes it through a typed handler (`agent_servers/src/acp/detachable.rs`; not a catch-all, so the SDK's dynamic session/MCP handlers still see every other notification) and `AcpThread::set_detached_turn_running` (`acp_thread/src/fork.rs`) adopts it as a running turn: the thread shows Generating, Stop sends `session/cancel`, and a follow-up prompt replaces it. Other ACP clients ignore `_` methods. Tests: 2 hub tests, `detached_turn_runs_until_ended_cancelled_or_replaced`, `agentd_turn_state_marks_reattached_thread_running` (real v1 transport), and `detach_smoke.py` now reattaches mid-turn (running=true → live answer → running=false): PASS
- [x] P4 auto-deploy `zed-agentd` to host on Work Remote — by embedding, not a second upload: `remote_server` runs agentd as its `agentd` subcommand (dispatched in `main.rs` before clap; `agentd::cli` now holds the CLI, `SelfLauncher` makes the daemon re-run `<server> agentd serve`), and `remote_server exec` exports its own path as `ZED_AGENTD_REMOTE_SERVER` to what it starts. The shim prefers that embedded agentd (version-matched with the GUI, zero extra bytes over SSH), then an installed `zed-agentd`, then the bare agent. Upstream-file touches: `remote_server/src/main.rs` (+8), `server.rs` (+4), two `Cargo.toml` lines. Caveats: only when Zed uploads its own server (dev/`build-remote-server-binary`/`ZED_COPY_REMOTE_SERVER`; a zed.dev-downloaded stock server has no agentd → installed one or bare agent); a daemon started by an older server keeps serving newer bridges until `agentd stop` (handshake is unchanged JSON). Tests: `shim_prefers_the_agentd_embedded_in_the_remote_server`, fallback test extended to a stale env path; `SMOKE_VIA_REMOTE_SERVER=1 detach_smoke.py` (clients go through real `remote_server exec` → shim branch → embedded attach → daemon spawned as `remote_server agentd serve`): PASS; standalone smoke still PASS
- [x] P4 `zed-agentd gc` — journal retention (`f1fd59819f`): `gc [--max-age-days N] [--max-total-mb N]` (defaults 30 d / 1 GiB) via a `Gc` handshake, plus one run with the defaults at daemon start. Only idle sessions are evicted (not live, unattached, nothing queued or in flight); `FileJournal::remove_session` drops the cached append handle first, then unlinks log before record. 3 hub tests + 1 journal test; e2e smoke on the real binary (startup gc removed a year-old session, CLI size cap removed the rest, bad args exit 1). Not periodic: a daemon that runs for months only gcs on start or on demand
- [-] Windows hosts — shim is POSIX `sh`; hook is a no-op on Windows paths. Deferred until P3 is green, not for lack of a host: the 4090 (`shikuwa`, Windows, ssh `katop@100.85.179.44`) is one. Port scope: `UnixListener`/`UnixStream` in `daemon.rs`/`attach.rs` → named pipes, `setsid` → `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, the `sh` shim in `detachable.rs` → a PowerShell/`cmd` equivalent. Porting before the Unix path passes its real e2e risks porting a design that changes
  - Alternative P3 host (2026-10-27): the 4090 has WSL Ubuntu (WSL2, stopped). Usable as a Linux Work Remote target, but needs `sshd` in WSL plus a portproxy/firewall rule on a shared box, which is more invasive than enabling Remote Login on `amo`. Not set up

## Perf / benchmark gate

- Replay coalescing: N per-token chunks → 1 merged chunk per run (unit-tested).
- [x] Bench `.benchmarks/002_agentd_journal_replay.md`: 10k-chunk replay 7.5 ms; append
  249.7 → 19.7 ms (−92%) by caching per-session log handles — promoted to default.
- Hot path is line-forwarding through one std thread hub; no allocation beyond JSON parse.
- Feature gate: runtime toggle (`DetachableRemoteAgents`, off by default) — raw/stock path
  unchanged when off; promote to default only after P3 e2e is green.
