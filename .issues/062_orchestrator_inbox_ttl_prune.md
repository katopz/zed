# 062 — TTL-prune the orchestrator sink inbox
Status: open (2026-11-13) — filed from the plan-056 drain-round verdict (round 2, `<session f6ff9772>`, reviewer note); unclaimed

The orchestrator inbox is the designated dead-letter office: boss wakes are sent from `orchestrator@<rig>` (plan 056 drain round) so lane replies die there, and the sink has no reader by design. Disk-only growth, unbounded.

- [ ] Add retention pruning for the `orchestrator` inbox (default ~7d) alongside `THREAD_END_RETENTION` pruning in `crates/agent_mail/src/boss.rs`, so archived wake replies age out instead of accumulating forever.
- [ ] Test: envelopes older than retention are deleted from `inbox/orchestrator/{new,cur}`, fresh ones kept; other inboxes untouched.
- [ ] Log the pruned count one line per watchdog tick (same style as thread-end pruning).
