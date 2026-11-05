# 053 — Mail-triggered thread spawn (`[M]` threads)

Status: DONE — round 2 (owner follow-up): resume + rig watcher landed, settings on for m3, Zed Dev rebuilt; impl `eb06bedf77`

## Round 2 — the two defers, done

- [x] **Resume saved thread for `zed-*` mail** (`discover_resume_targets` + `lookup_saved_threads` + `NativeAgent::open_saved_mail_thread`): mail to a `zed-<8>` inbox whose session is not live now loads the saved thread from `ThreadsDatabase` (top-level rows only), reopens it via `open_thread_with_work_dirs` on the donor project with the thread's original `folder_paths`, pins it in `pinned_threads`, and delivers the mail through the normal mailbox path. No match (history pruned) → stays parked. Same gate (`agent_mail_spawn_threads`) and cap as spawning; `spawned_threads` renamed `pinned_threads` (spawn + resume) and is now the cap source. Test: `test_agent_mail_resumes_saved_thread_for_zed_name` (in-memory per-test DB, synthetic id, asserts resume + pin + turn prompt).
- [x] **Rig watcher for Zed-not-running** (`agent-mail rig zed-wake`, new `crates/agent_mail/src/zed_wake.rs`): one tick per invocation — wakes only when this rig has NO fresh live entry (a running Zed either claims the mail itself or has the gate off), mail is past a 2 min grace, not fyi-only, not `_any`/`_all`; 10 min cooldown via `watch/zed-wake.json`; launch command from `AGENT_MAIL_WAKE_COMMAND` (space-separated argv), default `open -ga "Zed Dev"` on macOS; `--install` writes a launchd plist (`dev.agent-mail.zed-wake`, 60 s interval). Tests: wake-past-grace + cooldown, never-when-live-entry, fyi-never. Deploy per rig: rebuild agent-mail, `agent-mail rig zed-wake --install`, `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.agent-mail.zed-wake.plist` (Windows: Task Scheduler on `rig zed-wake --once`).
- [x] Owner settings: `{ "agent": { "fork": { "agent_mail_spawn_threads": true } } }` added to `~/.config/zed/settings.json` (m3).
- [x] Zed Dev rebuilt via `./script/bundle-mac -i` and agent-mail binary reinstalled.

## Round 3 — per-rig deployment (owner follow-up)

- [x] **m3** (this rig): watcher verified loaded (`launchctl list` → `dev.agent-mail.zed-wake`, exit 0).
- [x] **amo** (has Zed IDE): repo pulled to `eac5b5e0fa` (clean tree, ff-only); `cargo install --path crates/agent_mail --force`; `rig zed-wake --install` + launchctl bootstrapped (loaded, exit 0); settings key added (JSON5 validated); **Zed Dev rebuilt + installed** via direct `./script/bundle-mac -i` run (binary mtime Nov 6 00:02; rig-job runner exit 255 — see below). Takes effect on amo's next Zed restart.
- [x] **4090** (Windows, Zed at `C:\Users\katop\OneDrive\Desktop\zed.exe`): gitsync had already pulled `eac5b5e0fa`; `cargo install --path crates\agent_mail --force` → new `agent-mail.exe`; watcher via Task Scheduler `\zed-wake` (every minute, `/it`, wrapper `E:\git\zed-wake.cmd` sets `AGENT_MAIL_WAKE_COMMAND` to the Desktop zed.exe since Windows has no default launch command; log `E:\git\zed-wake.log`); settings key added via `E:\git\zed-wake-settings.ps1` (since removed); first tick ran, task Ready with next run scheduled.
- [x] **bmo**: skipped — no Zed, no zed repo ("first free agent" mail there is Claude-waiter-only; a Zed wake is meaningless and the wire format is compat-by-design). Revisit only if bmo gains a Zed.
- [x] **Known issue (rig-job runner)**: `agent-mail rig run amo` job `01a1ac98` failed exit 255 in ~1s with the runner's SSH answering `Host key verification failed` (amo's known_hosts entry predates its Tailscale IP; the log path `rig log` gives resolves differently from amo's side — retrieval via `HostKeyAlias=amo` scp worked). Direct ssh run used instead; runner auth on amo needs a look (filed below).
- [ ] **Deferred**: 4090 Zed rebuild (spawn/resume support in its Desktop zed.exe) — the box is BUSY with 3 GPU contenders and a Windows release build would contend for CPU; do it in a quiet window via `script/dev-rebuild-windows.ps1` on the box. Watcher + settings are in place, so mail-triggered wakes already work there once its Zed build ships the feature.
- [ ] **Deferred (refinement)**: pinned mail-driven threads hold their `AcpThread` until Zed restarts; unpin when the user deletes the thread from history (needs a delete-notification hook into `NativeAgent`).

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

- Zed running with zero live sessions: no donor project → mail parks.
- Closing a mail-driven thread's panel view does not release the session while
  `pinned_threads` pins it; the name respawns/resumes only after the session
  leaves the agent (Zed restart). Refinement candidate: observe thread deletion.
- The rig watcher wakes at most one editor per 10 min and only when no fresh
  live entry exists; it cannot distinguish "Zed running with the gate off"
  from "Zed absent" in that case — both leave mail parked, so the watcher stays
  quiet while a Zed poller is alive.
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
- [x] Round 2: resume saved thread for `zed-*` mail (see above)
- [x] Round 2: rig watcher `agent-mail rig zed-wake` (see above)
- [x] Plan updated, commits `feat:` on develop, push
