# 050 — auto_prompt ↔ orchestrator: who continues a thread
Status: DONE — `09d10e5e05` (2026-11-01, owner lgtm)

## Verdict (owner question: sunset auto_prompt and judge on bmo?)
Keep auto_prompt. It is the per-thread reflex inside Zed: it sees context
overflow, pending questions, API/auth failures, session limits and stalls,
and reacts in under a second with no SSH. The amo orchestrator (plan 048) is
the fleet layer: duplicates, drift, idle rigs, queues. auto_prompt is also
the natural fallback when amo or SSH is unreachable. A judge on bmo would
add processes to the quiet bench rig and an SSH hop to every decision.

Roles: amo orchestrator + builder · m3 proposer (owner) + auto_prompt host,
never a job target · bmo quiet runner (`--exclusive`) · 4090 CUDA/heavy runner.

## Seams built
- [x] Ownership: `chain_auto_prompt` (agent_ui) calls
  `agent_mail::watch::mark_auto_prompt_owned(session)` for every thread with
  auto_prompt on; `rig probe` reports `auto_prompt: true` for that Claude
  session (6 h window); the watchdog's stalled rule skips it, so the two
  layers never nudge the same thread. Test: the `chained` agent in
  `detects_duplicates_drift_stalls_and_no_job_rigs`.
- [x] Fleet context: every full `rig status` writes `~/.agent-mail/fleet-cache.json`
  (one line per rig: role, QUIET/BUSY, heavy work, jobs); auto_prompt's
  `peer_states::unmuted_states_for_context()` (the single choke point all
  deciders use) appends it when ≤ 5 min old. No extra requests per decision
  (war-room lesson). Tests: `fleet_summary_is_one_line_per_rig_and_cached`;
  auto_prompt 462 lib tests pass (fleet read is off under `cfg(test)`).

## Follow-ups
- [-] Retire the agent_board peer-state feed entirely once the fleet context
  proves sufficient (the board is already off by default since `845972dfc2`).
