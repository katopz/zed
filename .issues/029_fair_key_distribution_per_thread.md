# Issue 029: Fair key distribution across concurrent agent threads

status: fixed

## Symptom
6 concurrent agents all pick K4 when K1 is unavailable. Expected: each new
thread rolls a key and the population distributes fairly across the healthy
spares (K2/K3/K4).

## Root cause
`KeyHealthTracker.last_used_slot` is process-global (one shared tracker per
provider behind `Arc<ParkingMutex<..>>`), so the plan-027 "session-sticky"
pick degenerates to "process-sticky":

1. K1 healthy → everyone on K1 (by design).
2. K1 backs off → the next request rolls a random spare (say K4) and
   `record_attempt` sets `last_used_slot = Quaternary`.
3. Every subsequent request from ALL agents hits the sticky branch → K4.
4. `reset_key_session` clears the pick on new-thread dispatch, but the very
   next request re-sets it → pile-on returns immediately.

The provider layer never had thread identity, so stickiness was implemented
on shared state. `LanguageModelRequest.thread_id` (already populated by the
agent layer) fixes that.

## Fix
- Per-thread sticky picks: `thread_picks: HashMap<thread_id, (slot, last_used)>`
  in the tracker (ephemeral, TTL-pruned). Same thread → same key while healthy
  (prompt cache stays hot per thread, plan-027 intent preserved).
- Fair rotation for fresh picks: round-robin cursor over the healthy spares
  with a random per-process start. Consecutive fresh picks (new threads)
  spread evenly: 6 agents over 3 spares → exactly 2 each.
- K1-priority unchanged (K1 always wins while healthy).
- `reset_session`/`record_attempt` removed — new threads get fresh picks via
  their new thread id; `reset_key_session` keeps its probe-all-keys behavior
  (clears stale backoffs).
- Manual `PartialEq` on the tracker comparing only the persisted slot health,
  so ephemeral selection mutations don't trigger a backoff-file persist on
  every request.

## Tasks
- [x] health.rs: thread map + rotation cursor + selection rewrite
- [x] health.rs: tests (fair distribution, per-thread stickiness, TTL prune)
- [x] open_ai_compatible.rs: pass `request.thread_id` through to `retry_stream`
- [x] open_ai_compatible.rs: drop `reset_session` call from `reset_key_session`
- [x] agent_ui hook comment update
- [x] clippy + tests green on language_models (151 passed, incl. 13 selection + 6 retry_stream tests) + agent_ui check clean
