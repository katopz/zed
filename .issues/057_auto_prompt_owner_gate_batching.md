# 057 — auto_prompt: batch the owner gate at stop + chain KPI counters

Status: open — deferred from the openrsi-harness lesson session (zed/auto_prompt follow-up)
**Claimed:** amo-idle-lane, 1794389000, expires 2026-11-13T00:00:00+00:00

OpenRSI's bottleneck after automation: review bandwidth (median 3.5 days, up to 23 commits). Our analog is the owner gate — questions currently surface one at a time instead of one consolidated list at halt.

## Tasks

- [ ] At a valid `<stop>`, collect owner-gated items across the session's claimed plans into ONE consolidated `#OwnerGate` list in the final summary context (the verdict tool already supports owner-gated sections)
- [ ] Chain KPI counters logged per chain: time-to-first-verified-gain per plan, verdict turnaround — their "first gain at 4.2–7.4 h" and review-median metrics
- [ ] Optional: frozen-baseline block in `plan_registry` claims (commit + baseline numbers + noise floor) so the orchestrator can reject a "done" citing an absolute number against a stale baseline

## Refs

- Blog: https://index.openrsi.foundation/blog/2026/openrsi-sglang-omni/ (review bandwidth; evaluator is the product)
- Landed first half of the lesson in plan 055: tried-steps ledger + plan scoreboard (crates/auto_prompt)
