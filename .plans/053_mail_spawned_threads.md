# 053 — Mail-triggered thread spawn (`[M]` threads)

Status: DONE — all gates green (agent_mail 49 + agent 838 tests, clippy clean on agent/agent_mail/agent_settings/settings_content/agent_ui); impl `f9f692ab4b`

## Why

Cross-rig mail can only reach threads that already exist: the poller claims
inboxes for live sessions only, `*@rig` resolves via the liveness registry of
existing agents, and the Claude Stop waiter wakes existing sessions. Mail to
`task-foo@rig` with no such thread parks forever. Owner asked for mail to be
able to *trigger* a new thread on the target rig, titled with a `[M] ` prefix
so its mail origin is visible.

## Design

- `Mailroom::pending_names()` — inbox dirs holding ≥1 pending (due) envelope;
  `_all` (broadcast, cursor-based) excluded.
- Poller (fg snapshot): prunes spawn state alongside `live_since`, builds the
  live inbox-name set override-aware, and computes a spawn budget
  (`enabled && sessions non-empty && under cap`).
- `discover_spawn_targets(mailroom, live_names, budget)`: pending names − live
  names − `zed-*` (machine addresses of dead sessions park instead) − `_any`
  (shared `*@rig` fallback) − fyi-only batches (fyi never wakes, so never
  spawns). Discovery precedes the claim, so a failed spawn retries next tick
  instead of dropping claimed mail.
- `NativeAgent::spawn_mail_thread(name)`: fresh top-level thread on a live
  donor session's project (donor's `work_dirs`), titled `[M] <name>`,
  registered in `spawned_names` so the session answers as `<name>@rig` —
  `agent_mail_address`, the send path `from`, claims, and liveness entries all
  use the override. Mail is posted through the normal mailbox path: the body
  becomes the first user message.
- **Strong handles**: `AgentMailState::spawned_threads` pins the spawned
  `Entity<AcpThread>` — no panel view owns mail-spawned threads, and without
  the pin the release would clean the session up the moment the spawn returns
  (found by the spawn test).
- Cap: `MAX_MAIL_SPAWNED_THREADS = 8` live mail-spawned threads.
- Gate: `agent.fork.agent_mail_spawn_threads`, default **false** (remote
  work trigger; enable per rig — workers yes, owner laptop no):

  ```json
  { "agent": { "fork": { "agent_mail_spawn_threads": true } } }
  ```

## Non-goals / known limits

- No saved-thread resume for `zed-*` addresses of stopped sessions (parked;
  follow-up candidate).
- Zed running with zero live sessions: no donor project → mail parks.
- If Zed itself is not running on the rig, no poller exists — needs an
  agent-mail-side rig watcher, out of scope here.
- Closing a spawned thread's panel view does not release the session while
  `spawned_threads` pins it; the name respawns only after the session leaves
  the agent (Zed restart). Refinement candidate: observe thread deletion.

## Tasks

- [x] `Mailroom::pending_names()` + unit test
- [x] `agent_mail_spawn_threads` in `settings_content` + `agent_settings` (+ 2 test fixtures)
- [x] Bridge: `spawned_names` map, `mail_name_for_session`, `agent_mail_address` + send-path `from` override
- [x] Bridge: poller discovery + claim + `spawn_mail_thread` (`[M]` title, strong AcpThread pin)
- [x] Tests: spawn + follow-up threads to same session, fyi never spawns, off by default, `zed-*` never spawns
- [x] `./script/clippy` clean (agent, agent_mail, agent_settings, settings_content, agent_ui)
- [-] Defer: resume saved thread for `zed-*` mail instead of parking
- [-] Defer: rig-level watcher for spawning when Zed is not running
- [x] Plan updated, commit `feat:` on develop, push
