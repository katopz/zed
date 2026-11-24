# Issue 082 — mail-wake coverage holes: cold start has no donor, `[M]` thread names are not persisted

**Status:** OPEN 2026-11-24 (zed agent on amo) — split out of issue 080's landing; the owner's click-only-wake report is fixed for the running-Zed cases, these two remain.

## Symptom

Two cases where parked agent-mail still waits for the owner's click:

1. **Cold start.** After a Zed restart with no thread opened, `agent.sessions` is empty. The issue-080 donor keep-alive can only pin an existing session, so there is no project donor; `mail_budget` is `None` and neither `open_saved_mail_thread` nor `spawn_mail_thread` can run. Mail parked in any `zed-*` inbox sits there until the user clicks a thread open. `Project::local` construction needs client/node_runtime/user_store/languages, which `NativeAgent` does not hold — the fix belongs in the agent panel layer (it already builds projects), or requires plumbing those deps into `NativeAgent`.
2. **`[M]` threads lose their inbox names.** `AgentMailState::spawned_names` (inbox-name overrides for mail-spawned `[M] <name>` threads) is in-memory only. When a spawned thread's session dies (Zed restart, or the pin map pruned), mail addressed to `<name>@rig` finds an orphan non-`zed-*` inbox: `discover_resume_targets` skips it (only `zed-*` names map to history rows via `native_thread_mail_name(&thread.id)`), and `discover_spawn_targets` spawns a FRESH empty `[M] <name>` thread if `agent_mail_spawn_threads` is on — duplicate leftovers piling up in the sidebar, none of them carrying the dead thread's context. Persistence options: a column/table in `ThreadsDatabase`, or encoding the override into the saved thread row.

## Evidence

- Budget gate: `agent_mail_bridge.rs` snapshot (`mail_budget` requires `!agent.sessions.is_empty()`); donor fallbacks `self.sessions.values().next()?` in `open_saved_mail_thread` / `spawn_mail_thread`.
- Name mapping is lookup-only for `zed-*`: `lookup_saved_threads` matches `native_thread_mail_name(&thread.id) == inbox`; `spawned_names` has no disk representation.
- Issue-080 fix (this session) added `donor_pin` + the reconcile arm leg; the donor pin covers "views closed while Zed keeps running", not process restarts.

## Tasks

- [ ] Cold start: donor-free wake. Preferred: panel-layer resume that builds the project from the saved thread's own `folder_paths` (`Project::local` + worktree for each dir) when `sessions` is empty; gated the same way as resume (non-fyi waking mail only).
- [ ] `[M]` name persistence: persist `spawned_names` (inbox name → session id) in `ThreadsDatabase`; `lookup_saved_threads` matches overrides too; on resume, re-insert the override into `spawned_names`. Kill the duplicate-spawn path for names that resolve to a saved thread.
- [ ] Unresolvable `zed-*` inboxes need a dead-letter or backoff: mail addressed to a `zed-*` inbox with no saved row (an empty thread — `thread_save_payload` never saves message-less threads — or a deleted one) stays in `new/` forever, and the poller re-runs `lookup_saved_threads` for it every tick (~500 ms, a DB query per stuck inbox). Parked-mail discovery should stop re-probing names that resolve to nothing (park-with-backoff, or dead-letter after N misses).
- [ ] Tests: restart-shaped test (no live session, parked mail, panel-level open → wake); spawned-thread persistence test (spawn, release, mail the custom name → resumes the original, no duplicate `[M]`).

## Known costs (verdict round 2, accepted for the issue-080 fix)

- The donor keep-alive holds its session's `Project` — language servers, worktrees — until Zed exits, even after the owner closes that project's window; mail resumed later lands in a project with no window. Cheap follow-up if it bites: prefer a donor whose project still has a workspace open.
- The reconcile arm leg re-reads `scheduled()` per live inbox every ~5 s; `set_waiting_for_mail` no-ops on an unchanged flag (no notify), so re-arming is free for already-waiting threads.
- Threads with zero messages are never saved, so they can't be resumed by name — acceptable: an empty thread has no context worth resuming, and a waiting thread always ran a turn (hence has content). The stuck-mail side of this is the dead-letter task above.
