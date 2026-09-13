Status: DONE — fix live-verified 2026-09-13; issue 017 removed (full
narrative in git history; this file is the durable closeout record)

# Terminal PTY fd leak — "The terminal is fd-exhausted" (issue 017 closeout)

Fixed 2026-09-04; live-verified 2026-09-13.

## Root cause (short)

Agent terminal-tool PTYs were never released after the command exited:
the alacritty event loop drains until EOF on the PTY master, which only
arrives when EVERY process holding the slave exits — commands that leave
a background process behind (`npm run dev &`, watch modes) pin the slave
forever, and tool-call history kept the `Entity<Terminal>` alive for the
thread lifetime. On 2026-09-04 this reached EMFILE (soft limit 2560):
4,762 `Too many open files` errors — ripgrep/git/LSP/MCP spawns were
victims, not causes.

## Fixes (2026-09-04)

- `85f8ca173a` — `Terminal::shutdown_backend()` (idempotent Drop teardown);
  `acp_thread::Terminal::kill` releases the PTY backend on every kill path
  (tool-call handle drop, user stop, timeout/cancel, rewind).
- `2eee6352ac` — reliability heartbeat logs `open fds N` +
  `zombie children N` on the memory line, `warn` past 1024.
- `5169e192e8` — `raise_open_file_limit()` at startup (soft raised toward
  65536 when the hard limit allows).

## Live-verify (2026-09-13 — the issue's GOAT box, closed)

App: `Zed Dev.app` built 2026-09-11 23:36 (contains all three fixes),
running since 2026-09-12 07:01 — a 36h+ multi-agent farm session,
resident ~9 GB, terminal-tool-heavy throughout.

- Heartbeat `open fds`: band 271-324 across all 45 retained heartbeats
  (2026-09-13 11:33 -> 18:43); earliest 324, latest 299, no growth trend.
  `lsof -p` spot check: 374.
- `zombie children 0` on every heartbeat; zero `Too many open files`
  lines in retained logs.
- Child shells inherit the raised limit (`ulimit -Sn` = 67840) — the
  startup raise is active end-to-end.
- Per-subsystem fd attribution (the issue's conditional follow-up): NOT
  NEEDED — the creep it would diagnose is absent (flat band, ~200x
  headroom below the 65536 soft limit).
