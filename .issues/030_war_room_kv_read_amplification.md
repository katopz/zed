# 030 — War room KV read amplification (15.2M reads/day) → stack retired

Status: FIXED + RETIRED (2026-08-28). Code kill-switch: commit `845972dfc2`.
Cloudflare teardown: KV namespace `d2cdb46dee30430b96dbf5b439ed318b` DELETED
(~3k keys), worker `agent-board-worker` DELETED (API code 10007; live URL now
serves Cloudflare error 1042). Teardown recorded in this file's introduction
commit (`git log -- .issues/030_war_room_kv_read_amplification.md`).

## Symptom (2026-08-25, Cloudflare dashboard)

- KV List Operations: 1,290,000/day (~15/sec sustained)
- KV Read Operations: 15,200,000/day (~176/sec sustained)
- Free tier = 1,000 lists/day, 100,000 reads/day → 1,290× and 152× over.
  Sustained on Workers Paid ≈ $400/month (reads $0.50/M + lists $5.00/M).

## Attribution (code-verified)

Namespace `AGENT_BOARD` (`agent-board-worker`, plans 013/015/024/026) was the
only KV consumer of this stack; reads:lists ratio 11.8:1 matches its access
pattern exactly.

Amplifiers, in `agent-board-worker/src/index.js`:

1. `handleGetRoom` = 4 KV lists + 1 KV get **per key** — devices (≤64) +
   msgs (≤105) + states (≤55) + replies (≤55). A busy room ≈ 200 reads per
   GET. Read amplification is structural: ring buffers stored key-per-item.
2. `verifySignature` = 1 KV get + 1 KV list **per signed POST** (no auth
   cache), plus 1 put to refresh the pubkey.
3. Poll fan-out: every Zed window polls every 15s AND `realtime_nudge`
   (`crates/agent_board/src/runtime.rs`) restarts the round on every SSE
   event, throttled to only 1 round / 2s per window (7.5× base rate in a
   chatty room; `realtime_enabled: true` on m3).
4. Dashboard read-only tab runs `fetchRoom(); fetchThreads()` every 15s
   unconditionally — even while SSE is connected ("safety net").
5. Plan 026 (deployed 2026-08-24, the day before the spike) added streaming
   `POST /thread` per session update: each = auth get+list + doc get + put.

Model check: ~6 windows (nudged) + one read-only dashboard tab + a few dozen
agents broadcasting states/threads ≈ 50K+ GETs/day × ~200 reads + POST auth
ops ≈ 10–15M reads/day. ✔ matches observed 15.2M.

## Fix (root cause: read amplification × poller fan-out)

The stack was retired rather than optimized (single-user tool, cost ≫ value):

- `AgentBoardConfig.enabled` master kill switch, **default false**. When
  false, `BoardRuntime::try_start` returns before building a
  `BoardClient` — no poll loop, no SSE, no status/thread POSTs, no MCP
  socket; panels stay local-only. Test:
  `runtime::tests::disabled_board_with_worker_url_stays_fully_inert`
  (worker_url set + realtime_enabled: true + 404 fake HTTP → zero client,
  zero poll task, zero requests).
- User config `~/.config/zed/agent_board.json`: `enabled: false`,
  `worker_url: ""`, `realtime_enabled: false`.
- Cloudflare: KV namespace deleted (all ~3k keys gone), worker deleted,
  DO namespace `RoomCoordinator` removed with the script.
- Panels/README/plans marked OBSOLETE.

## What would fix it if ever revived (do NOT redeploy without these)

1. Serve `GET /room` + `/threads` from RoomCoordinator DO memory (writes
   already relay through it); refresh from KV ≤1×/15s. Reads scale with
   rooms, not pollers. ~99% reduction.
2. Auth cache in `verifySignature` (isolate Map, 5-min TTL).
3. Dashboard: 60s+ poll backoff while SSE connected.
4. Zed: nudge throttle ≥ poll interval (15s), not 2s.

## Notes

- Other machines (e.g. the 4090 box) that had `agent_board.json` configured
  will get harmless errors until their config sets `enabled: false` — the
  dead worker URL can no longer receive anything regardless.
- Old binaries without the `enabled` key ignore it (serde default), and the
  empty `worker_url` keeps them local-only too.
