# 049 — See the rigs: Zed "Rigs" panel + `agent-mail top` terminal UI
Status: DONE (code) — T1-T4 `a548ca70fd` + `f3288b32b5`; panel click-through waits on the owner's Zed Dev rebuild (bundle building)

## Owner notes (2026-11-01)
- The war room (plan 024) was retired for a messy UI and request amplification
  (`.docs/012_war_room_stack_retirement.md`: the feeder's poll loop turned every
  status view into KV reads, 15.2M/day). No Cloudflare here, but the same rule
  holds for SSH: keep request frequency low.
- bmo has no Zed: the terminal views must stand alone in a native terminal.

## Problem
Jobs queued with `agent-mail rig run` (plan 047), heavy processes, agents per
rig and mail are only visible through `agent-mail rig status` output and
lifecycle mail. The owner wants to *see* them: in Zed when the IDE is open,
in a terminal on headless rigs, with logs.

## Verdict
**One data source, two thin views.** Both read the same JSON agent-mail
already produces (`rig status --json`, `rig log`, inboxes); no new service,
no web/Cloudflare surface (the retired war room, plan 024, died of that).

| View | Where | Notes |
|---|---|---|
| `agent-mail top` (ratatui) | any terminal, all 4 rigs incl. the 4090 over SSH | build first: smaller, settles the data model |
| Zed **Rigs** panel | dock panel beside thread history | jobs are not threads, so a separate panel; jobs link to their submitter's thread |

## Design
### Shared model (`agent_mail` lib)
- `FleetSnapshot` = `rig status --json` rows + this rig's mailbox summary
  (owner escalations, broadcasts, per-agent pending counts, outbox).
- `job_log(rig, id, tail)` local or over SSH (exists as `rig log`).

### `agent-mail top`
- Layout: rigs list (QUIET/BUSY, load, mem, GPU) | jobs + heavy processes of
  the selected rig | live log tail of the selected job; Mail tab.
- Keys: ↑↓ select, Tab switch pane, `l` full log, `c` cancel job, `m` mail,
  `r` refresh now, `q` quit. Refresh every 10 s; the log pane tails every 2 s.
- ratatui + crossterm behind a default-on `tui` feature (binary size).

### Zed Rigs panel (fork-owned crate)
- Tree: rig → state, running/queued jobs, heavy processes, agents, outbox;
  Mailbox section: owner escalations, broadcasts, local agents' mail.
- Click a job → read-only buffer tailing its log; click an agent → its thread
  if local; click a message → full text. Job row actions: cancel, re-run.
- Refresh only while the panel is visible (10–15 s), off otherwise.
- Status-bar badge: count of BUSY rigs / running jobs / waiting escalations.

## Tasks
- [x] T1 shared model in the lib: `rig::cached_probe` (a probe younger than 10 s is reused, so any number of viewers costs ≤ 1 real probe per rig per 10 s: the war-room lesson), `Mailroom::mailbox_summary` (read-only: viewers never claim agents' mail), `Mailroom::job_on_rig` (log/cancel locally or over SSH)
- [x] T2 `agent-mail top [SECS]` (`crates/agent_mail/src/tui.rs`, ratatui 0.30 crossterm-only behind the default `tui` feature): Rigs tab (rig list QUIET/BUSY/OFFLINE, state + heavy work + agents + escalations, jobs list, live log of the selected running job) and Mail tab (inbox counts, pending + broadcasts, full message); keys q, 1/2, Tab, ↑↓/jk, Enter, c (confirm y), r; fetches on a background thread so SSH never freezes the screen; fleet every 15 s (min 5), log every 3 s only while shown. Plus `agent-mail rig status --watch [SECS]`, a plain redraw loop with a mail digest for minimal terminals. Tests: 2 `TestBackend` render tests (incl. a 12×5 terminal and CJK/em-dash text). Real-terminal check: pty run on m3 and on bmo over `ssh -tt` both rendered every rig + panes, `q` exited clean. 4090: builds + tests pass; install waits for its running job (the executor holds `agent-mail.exe`)
- [x] T3 `crates/rig_panel` (`a548ca70fd`): dock panel (right, `Server` icon) registered only where `~/.agent-mail/rigs.json` exists; rig tree with Disclosure (state, facts, not-quiet reasons, jobs, heavy work grouped, agents, outbox, escalations); click a job → live log tail (40 lines, buffer font) with a confirmed cancel; read-only Mail section (inbox counts, pending + broadcasts, click to expand); polls only while active via `Panel::set_active` (fleet 15 s, log 3 s). Agent→thread jump not done: the probe has Claude session names, not Zed thread ids (follow-up if wanted). `cargo clippy -p zed -p rig_panel` clean
- [x] T4 `RigBadge` status item: "N busy · N offline · N jobs · N for you", refreshes once a minute, click opens the panel. Test: `badge_counts_busy_offline_jobs_and_escalations`
- [x] bench: `agent-mail top` 0.35% of one core over 60 s (incl. 4 fleet refreshes) < 1%; panel polling stops in `set_active(false)` (task dropped)
- [-] e2e in the GUI panel: owner click-through after the Zed Dev rebuild. Terminal view e2e done (pty on m3 + bmo over ssh -tt); live adoption seen: another agent's `rig run --exclusive` job `21dee646` on bmo shows in `rig status` / the fleet cache
