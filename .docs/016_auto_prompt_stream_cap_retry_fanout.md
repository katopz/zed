Status: DONE (fixed, tests green, commit ref below)

# Stream-cap retry fan-out froze Zed + probe-stormed GLM (22:16 freeze)

Date: 2026-09-11 22:16 (second freeze of the day; the 07:09 freeze was the
API-outage continue loop, see `.docs/015`)

## Symptom

Zed Dev froze ~22:16; force-quit at 22:17. Pre-freeze log (`Zed.log.old`)
contains 1417 `stream-cap queue: retrying deferred dispatch` lines ALL
timestamped 22:17:08 — the 1MB log rotation boundary; the loop was spinning
~1MB of logs per second at the end. Distinct deferral counters interleaved:
898× `deferral 20/240`, 276× `21/240`, 243× `19/240` — hundreds of concurrent
retry loops.

Interleaved: `reset_key_session probe: slot=Primary/Secondary/Tertiary
result=Err("error sending HTTP request to GLM")` — the GLM endpoint was
network-unreachable/blocked, and every dispatch attempt fired a full key-slot
probe burst into it.

## Root cause

Two compounding bugs in `dispatch_action_with_attempts` /
`spawn_stream_cap_retry` (agent_ui/src/auto_prompt/mod.rs):

1. **Exponential retry-loop fan-out.** The Defer branch called
   `spawn_stream_cap_retry` on EVERY deferral — including when the call came
   from inside an already-running retry loop (which also continues itself).
   Every wake: 1 loop spawns 1 new loop + continues → population doubles every
   `STREAM_CAP_RETRY_DELAY_MS` (5s). 20 generations ≈ 100s from first deferral
   to a main thread drowning in `update_in` closures (each running
   `start_context_block`, config loads, panel scans) → UI freeze. The
   issue-018 escalation bound (240 deferrals ≈ 20 min) is per-loop and never
   gets a chance to fire — the population explodes first.

2. **Provider probe storm (`reset_key_session`) before the cap gate.** Each
   deferred attempt probed every configured GLM key slot over HTTP before
   discovering it wasn't going to dispatch anyway. At peak fan-out that is
   thousands of HTTP requests per second into the provider — this is what
   "GLM is blocking us" was about: the block was self-inflicted spam.

Trigger chain: 6 threads stuck `Generating` against the unreachable GLM API →
stream cap (6) never freed → a summary fork deferred → fan-out → freeze +
probe storm.

## Fix

- Defer branch only enqueues a retry loop on the INITIAL deferral
  (`stream_cap_attempts == 0`); loop-driven re-deferrals rely on the existing
  loop's next wake. One loop per queued action; the 240-deferral escalation
  bound is now actually reachable.
- `reset_key_session` moved AFTER the stream-cap gate — keys are probed only
  when a thread is actually about to be created.

## After-fix behavior under a dead provider

Queued chains log one deferral line per 5s, zero HTTP probes, and either win a
slot or escalate once after ~20 min (one probe burst, one thread). Stuck
`Generating` threads erroring against the dead API are handled by the
`.docs/015` guards (no decide-level continue loop).

## Tests

Existing `stream_cap_decision_*` unit tests still pass (decision table
unchanged); the fan-out was structural (spawn site), covered by the gating
comment + this doc. Full agent_ui clippy + auto_prompt test suite green.
