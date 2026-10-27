# 037 — Work Remote: GUI on M3, agents + compute detached on M5 (zed-agentd)
Status: in progress — P0 sync + compile fallout fixed, P1 daemon + P2 hook/menu implemented, journal bench GOAT (−92% append), P4 journal gc + native agent backend (`eval-cli --acp`) done and smoke-verified locally; P3 on-host e2e still blocked on M5 SSH access (owner)

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

```sh
cargo install --path crates/agentd         # → ~/.cargo/bin/zed-agentd (shim also probes ~/.local/bin)
zed-agentd status                          # sessions JSON (live/attached/queued)
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
- [ ] P3 verify Zed's reconnect flow calls `session/load` (not only `session/list`) for external agents on window restore
- [x] P4 native Zed agent as a backend: `zed-agentd attach -- eval-cli --acp` (`c212b13be4`, harness `767ba226e9`); verified locally incl. kill-mid-turn detach + replay. Structured diffs done; follow-ups (model switching over ACP, real-host run) in `.issues/036_native_agent_acp_stdio_server.md`
- [ ] P4 live turn-state on reattach: synthesize a "turn still running" indicator (ACP has no turn-ended notification; today replay just keeps streaming)
- [ ] P4 auto-deploy `zed-agentd` to host on Work Remote (reuse remote_server upload path) instead of manual `cargo install`
- [x] P4 `zed-agentd gc` — journal retention (`f1fd59819f`): `gc [--max-age-days N] [--max-total-mb N]` (defaults 30 d / 1 GiB) via a `Gc` handshake, plus one run with the defaults at daemon start. Only idle sessions are evicted (not live, unattached, nothing queued or in flight); `FileJournal::remove_session` drops the cached append handle first, then unlinks log before record. 3 hub tests + 1 journal test; e2e smoke on the real binary (startup gc removed a year-old session, CLI size cap removed the rest, bad args exit 1). Not periodic: a daemon that runs for months only gcs on start or on demand
- [-] Windows hosts — shim is POSIX `sh`; hook is a no-op on Windows paths (deferred, no Windows host in rig)

## Perf / benchmark gate

- Replay coalescing: N per-token chunks → 1 merged chunk per run (unit-tested).
- [x] Bench `.benchmarks/002_agentd_journal_replay.md`: 10k-chunk replay 7.5 ms; append
  249.7 → 19.7 ms (−92%) by caching per-session log handles — promoted to default.
- Hot path is line-forwarding through one std thread hub; no allocation beyond JSON parse.
- Feature gate: runtime toggle (`DetachableRemoteAgents`, off by default) — raw/stock path
  unchanged when off; promote to default only after P3 e2e is green.
