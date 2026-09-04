# Issue 017: FD exhaustion kills agent session — "The terminal is fd-exhausted"

Status: root cause identified, leak fix + diagnostics + headroom landed (see Commits); GOAT live-verify pending next long session.

## Symptom

During a long auto_prompt session (2026-09-04, ~17:46–18:02 local) every spawn
started failing with `Too many open files (os error 24)`: 4,762 EMFILE errors
from `project_search` (ripgrep), `git_store` (git subprocesses), MCP server
spawns, and LSP server spawns. The agent's own summary recorded: "The terminal
is fd-exhausted for the rest of this session — no further commands can run".
Zed was restarted at 18:04 to recover; the fresh process sits at ~372 fds.

## Evidence

- `~/Library/Logs/Zed/Zed.log`: 4,762 `Too many open files` errors
  17:46:59–18:02 (+07:00); log file begins already mid-exhaustion, so the leak
  predates the first retained line.
- Memory heartbeat shows the exhausted process at 8.6 GiB → 13.2 GiB (+4.5 GiB
  in 30 s) while EMFILE errors flooded.
- Interleaved `+00:00`/`+07:00` timestamps at the same instants = two Zed
  processes writing one log; the fd-exhausted one is the dead predecessor.
- Live process after restart: `lsof -p <pid> | wc -l` = 372; 16 healthy
  children, 0 zombies.
- Shell/GUI soft fd limit on this machine: **2560** (`ulimit -Sn`), hard:
  unlimited. 2560 is reachable by a leaking multi-hour agent session.

## Root cause (the leak, not the victims)

ripgrep/git/LSP/MCP spawn failures are victims. The leak: **agent terminal-tool
PTYs are never released after the command exits.**

- Every `terminal` tool call creates a PTY-backed `terminal::Terminal` whose
  alacritty event loop drains until EOF on the PTY master (`drain_on_exit`).
- EOF only arrives when *every* process holding the PTY slave exits. Commands
  that leave a background process behind (`npm run dev &`, watch modes,
  detached daemons) pin the slave forever, so the master fd + the event-loop
  thread live on.
- The `Entity<terminal::Terminal>` is kept alive by the thread's tool-call
  history (UI) for the whole thread lifetime — and auto_prompt retains up to 8
  threads. `Terminal::drop` (which closes everything) therefore never runs.
- `AcpThread::release_terminal` → `Terminal::kill` only called
  `kill_active_task`, a no-op once the task already completed — so the normal
  happy path never released anything either.

## Why not "kill zombies on thread start"?

Zombies hold PID slots, **not fds** — a reap sweep cannot fix EMFILE. The
documented-safe reaping already exists (`util::process::Child` and
`util::command::darwin::Child` drop-reap, issue 006 P1). A raw
`waitpid(-1, WNOHANG)` sweep is unsafe here: smol tracks spawned pids via its
own SIGCHLD driver and an external reap races it, breaking every pending
`status()` call (see `crates/util/src/process.rs` comment). Instead of
sweeping, the reliability log now reports zombie children every heartbeat so a
reap regression becomes visible.

## Fixes landed

- [x] `terminal::Terminal::shutdown_backend()` — Drop teardown extracted into an
      idempotent public method (stop event loop, terminate + kill child,
      shutdown headless subprocess).
- [x] `acp_thread::Terminal::kill` calls it after `kill_active_task` — covers
      the normal release path (tool-call handle drop), user stop, timeout/cancel
      kill, and `rewind` entry removal. PTY master fd + io thread are freed at
      end of every tool call instead of leaking for the thread's lifetime.
- [x] Reliability heartbeat now logs `open fds N` (macOS `/dev/fd`, Linux
      `/proc/self/fd`) and `zombie children N` (sysinfo `ProcessStatus::Zombie`,
      confirmed supported on macOS in sysinfo 0.37) on the existing
      `memory usage:` line, with a `log::warn!` past 1024 fds.
- [x] `raise_open_file_limit()` at startup (unix): soft limit raised toward
      65536 when the hard limit allows (this box: 2560 soft / unlimited hard).
      Headroom so a future leak degrades slowly and gets logged instead of
      instantly killing every spawn.
- [ ] GOAT verify: run a long auto_prompt session, confirm `open fds` stays
      flat across terminal-tool-heavy work and that killed/leaving-background
      commands no longer accumulate fds.
- [ ] Revisit if fds still creep: instrument per-subsystem fd attribution
      (LSP/MCP/PTY buckets) in the same heartbeat.

## Commits

- fix(terminal): release agent tool-call PTY backends on kill — `85f8ca173a`
- feat(reliability): log open fds and zombie children on memory heartbeat — `2eee6352ac`
- feat(zed): raise open file limit at startup — `5169e192e8`

## Files

| File | Change |
|------|--------|
| `crates/terminal/src/terminal.rs` | `shutdown_backend()` extracted from `Drop` |
| `crates/acp_thread/src/terminal.rs` | `kill()` now releases the PTY backend |
| `crates/zed/src/reliability.rs` | fd + zombie counts on the heartbeat log |
| `crates/zed/src/main.rs` | `raise_open_file_limit()` |
| `crates/zed/Cargo.toml` | `libc.workspace = true` |

Related: `.issues/006_auto_prompt_cpu_drain_analysis.md` (zombie reaping P1,
retained threads), upstream zed-industries/zed#63418 (terminal tool degradation
in long agent sessions).
