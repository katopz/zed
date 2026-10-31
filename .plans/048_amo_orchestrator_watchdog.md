# 048 — amo as rig orchestrator: zero-token watchdog + exception-driven LLM
Status: PROPOSED — 2026-10-31; awaiting owner picks (orchestrator runtime, nudge scope); builds on plan 047

## Goal
amo (always on, builder) becomes the orchestrator instead of m3/the owner:
every 5 minutes it checks all rigs so a duplicate, drift, idle rig or stalled
agent never lasts more than ~5 minutes, and it mails the agents involved to
resolve it or take other work. Token cost must stay small.

## Verdict
**Split the check from the judgement.**

| Layer | Runs | Cost | Does |
|---|---|---|---|
| **Watchdog** (`agent-mail rig watch --once`, plain program) | every 5 min via launchd on amo | **0 tokens**, ~0.5 s | `rig status --json` for all rigs, diff vs last tick, fixed rules → incidents |
| **Orchestrator** (LLM) | **only when a new incident appears** | ~1–2k tokens in per incident | reads a tiny digest, mails the agents involved (advisory), exits |

Rejected: an LLM orchestrator polling every 5 min (288 wakes/day, growing
context, mostly "nothing changed").

## Watchdog rules (deterministic)
- **Duplicate**: same job key on two rigs (P2 keys); same command + same repo
  running on two rigs; two live agents with the same `**Claimed:**` item.
- **Drift**: a tracked repo at different commits across rigs, or dirty where
  it should be clean; `agent-mail` version or `rigs.json` mismatch.
- **Idle**: a QUIET rig while another rig has queued work it could take
  (resource fit, role; m3 excluded).
- **Stalled agent**: idle > 5 min, no scheduled wake pending (`later/` empty),
  last message is not a question to the owner and not a finished summary.
- **Poisoned / stuck**: P1 poison labels, dead leases, outbox backlog growing,
  rig offline.

Each incident gets a **fingerprint**; only *new* or *changed* fingerprints
wake the orchestrator. The same incident never re-wakes it within a cooldown
(proposed 30 min), and after 3 unresolved rounds it escalates to the owner
instead of looping.

## Digest (the only thing the LLM sees)
Per incident, only the agents and rigs involved:
- rig line: role, QUIET/BUSY, top heavy command (truncated);
- agent line: name, rig, repo, status, idle minutes, **last paragraph of its
  last assistant message, capped ~300 chars** (from the Claude transcript
  `~/.claude/projects/<proj>/<session>.jsonl`, read by the watchdog, no LLM);
- the rule that fired + the watchdog's suggested action.
Target ≤ 1.5k tokens per incident; the whole fleet is never dumped.

## Authority (keeps it safe)
- Advisory only: the orchestrator **mails** agents (they decide, matching the
  047 poison policy); it never kills processes or edits repos.
- Respects the owner: never assigns work to m3; never nudges an agent whose
  last message asks the owner a question; rate limit 1 nudge / agent / 30 min.
- Assigning "other work" needs a backlog: unclaimed `- [ ]` items in
  `.plans`/`.issues` across repos (reuse the claim lease: the orchestrator
  suggests, the agent claims).

## Runtime choices (owner)
- [ ] Orchestrator runtime:
  - **A. fresh `claude -p` per incident on amo** (recommended): bounded context
    every time; needs the Claude CLI + `claude setup-token` on amo (amo has
    only Zed's bundled agent today; SSH sessions cannot read the keychain).
  - B. one long-lived Claude session on amo woken by mail: zero setup, but its
    context grows per incident (relies on auto-compaction).
  - C. a pinned Zed native thread on amo with a cheap model (e.g. GLM): woken by
    the plan-043 native poller; needs a stable mail alias for that thread.
- [ ] Nudge scope: all agents on all rigs, or only agents on autonomous lanes?
- [ ] Which repos count for drift (proposed: zed, katgpt-rs, riir-ai, riir-train, riir-infer)?

## Lessons applied
- launchd runs with a bare environment (the `cargo: command not found` swap
  failure): the plist pins absolute binary paths and `PATH`.
- Watchdog stale → `rig status` on any rig shows "orchestrator stale" (amo
  down = no orchestration, never silent).

## Tasks
- [ ] W1 `rig watch --once`: tick, diff vs `~/.agent-mail/watch/last.json`, rules, fingerprints, cooldown
- [ ] W1 transcript tail: last assistant paragraph per Claude session (bounded read from file end)
- [ ] W1 launchd plist on amo (StartInterval 300, absolute paths); "orchestrator stale" in `rig status`
- [ ] W2 digest + wake the orchestrator (runtime per owner pick); escalation after 3 rounds
- [ ] W2 backlog scan for "assign other work" (unclaimed items, claim-lease aware)
- [ ] bench: watchdog tick wall time + tokens per incident; target 0 tokens on quiet ticks, ≤ 1.5k per incident
