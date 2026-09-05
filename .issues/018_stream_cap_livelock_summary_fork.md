# Issue 018: Stream-cap queue livelocked summary forks — "auto_prompt not triggered at all"

Status: FIXED — bounded escalation + configurable cap landed (`77c01056b0`). GOAT live-verify pending: next saturated-farm session should show `deferral N/240` and either a slot win or the 20-min escalation, never an unbounded silent queue.

## Symptom

2026-09-05, ~17:58 local. A thread ended with the mandated 4-part `## Summary`
handoff (riir-ai Issue 097 E3 report). auto_prompt did not continue the chain;
the user had to click manually. Reported as "not trigger auto_prompt at all".

## Evidence (`~/Library/Logs/Zed/Zed.log(.old)`)

- `17:58:02 WARN [auto_prompt::context_overflow] Last message is already a
  voluntary summary — skipping Phase 1, going straight to Phase 2
  (session=7667e5fd-…)` — detection worked perfectly.
- The very same second: `dispatch_action: 5 threads already generating
  (cap 2), queueing new-thread dispatch for retry in 5000ms`.
- The retry then looped every 5s from 17:58 to past 19:00 — over an hour —
  while FOUR distinct chains (prompts 4622/4739/4885/5762 chars) queued.
- Count hovered 4–5 the whole time: the user's agent farm legitimately keeps
  ≥2 threads in `Generating` (`running_turn.is_some()` — long terminal turns,
  background continuations), so the cap never freed for long.

## Root cause

Issue 006 P2's anti-repaint-storm cap (`MAX_CONCURRENT_STREAMING_THREADS = 2`)
deferred background new-thread forks through `spawn_stream_cap_retry`, which:

1. was **unbounded** — no give-up, no escalation;
2. was **silent** — info-level logs only, no user-visible signal;
3. assumed slots free quickly — false for a multi-agent farm.

Manual clicks bypass the cap (`focus_new_thread`), which is why only manual
continuation worked. The summary fast path itself was never at fault.

## Fixes landed (`77c01056b0`)

- [x] `spawn_stream_cap_retry` owns a deferral budget: each requeue carries an
      incremented counter; at `STREAM_CAP_MAX_DEFERRALS` (240 × 5s = ~20 min)
      `stream_cap_decision` returns `Escalate` and the chain dispatches
      despite the cap — one extra stream, once, per starved chain, no focus
      steal. Bounded starvation replaces the livelock.
- [x] First deferral logs a `warn` (was info-only); every retry line carries
      `deferral N/240`; escalation warns loudly with elapsed minutes.
- [x] Cap is configurable: `max_concurrent_streams` in `auto_prompt.json` or
      `ZED_AUTO_PROMPT_MAX_CONCURRENT_STREAMS` (default 2; `0` = unlimited).
      Multi-agent workloads raise it so forks flow without queueing.
- [x] Tests: `stream_cap_decision` dispatch/defer/escalate matrix
      (agent_ui) + `max_concurrent_streams_defaults_and_overrides`
      (auto_prompt config serde). Full `auto_prompt` lib suite 404/404.

## Known race left open (pre-existing)

A queued loop is not cancelled when the user manually continues the same
chain — a late slot win (or, now, the 20-min escalation) can fork a second
continuation thread alongside the manual one. Predates this fix (the old
unbounded queue had the same race with worse timing). Candidate follow-up:
suppress the queue when the source session already has a `continued_from`
successor in `ThreadMetadataStore`.

Related: `.issues/006_auto_prompt_cpu_drain_analysis.md` (P2 cap origin),
`.issues/017_fd_exhaustion_terminal_tool.md` (same session family).
