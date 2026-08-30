# 012: War room stack retirement (agent board KV + Cloudflare worker)

- **Commits**: `845972dfc2` (gate agent_board network behind enabled flag,
  default off), `38917190c3` (retire stack, record issue 030 teardown)
- **Follows**: `.issues/030_war_room_kv_read_amplification.md` (removed after
  this entry; see git history for the full teardown record)
- **Status**: done
- **Date**: 2026-08-30

## What happened

Issue 030 measured the `AGENT_BOARD` KV namespace at 15.2M reads/day and
1.29M list operations/day (~176 and ~15 per second sustained) — 152× and
1290× over the Cloudflare free tier, ≈ $400/month on Workers Paid. The only
consumer was the agent-board/war-room stack (plans 013/015/024/026): the
feeder's poll loop amplified every status view into KV reads/lists.

## Decision

The stack's operator value did not justify the cost, so it was retired
rather than optimized:

- Code: `AgentBoardConfig.enabled` defaults to **false**; all network calls
  behind the flag (`845972dfc2`).
- Infra: KV namespace (≈3k keys) and the `agent-board-worker` Cloudflare
  worker **deleted**; the live URL now serves error 1042 (`38917190c3`).
- Docs: plan 024 marked obsolete at teardown time; plan 015 (the WebUI this
  stack served) marked obsolete 2026-08-30; issue 030 removed per the
  fixed-issue convention, with this entry as the durable record.

## Follow-ups

- None. If the operator board is ever needed again, rebuild it against a
  cheaper transport (local WebSocket relay, no per-view KV reads) — do not
  resurrect the deleted KV namespace pattern.
