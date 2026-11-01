# 049 — See the rigs: Zed "Rigs" panel + `agent-mail top` terminal UI
Status: PROPOSED — 2026-11-01; awaiting owner go-ahead (build order: TUI first, then panel)

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
- [ ] T1 `FleetSnapshot` + mailbox summary in the lib; `rig status --json` gains it
- [ ] T2 `agent-mail top` (ratatui): panes, keys, log tail, cancel, mail tab
- [ ] T3 Zed Rigs panel: tree, job log buffer, agent/thread links, mailbox, actions
- [ ] T4 status-bar badge
- [ ] bench: `top` idle CPU < 1%; panel refresh off when hidden
- [ ] e2e: run a job on the 4090 and bmo, watch it in both views, cancel from each
