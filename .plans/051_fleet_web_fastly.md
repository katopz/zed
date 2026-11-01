# 051 — Fleet web: thread history + rig queues on the web (Fastly edge, GitHub login)
Status: IN PROGRESS — code in `crates/fleet_web` (origin) + `crates/fleet_web/edge` (Fastly Compute); owner gates listed below

## Owner ask (2026-11-01)
See every thread's history the way Zed shows it, plus each rig's queue and
status, from a website behind GitHub login. Fastly backend (Cloudflare maybe
later, not now). Preact + signals SSR. Build inside `zed/crates`, no new repo yet.

## Verdict
- **Fastly is the edge, not the store.** The account is on the free
  `developer` plan: the KV Store and Object Storage APIs return 403, and KV
  needs a sales purchase. Owner picked: **amo serves the data, Fastly does
  login + SSR + a short edge cache.** Thread data never leaves the rigs. The
  cost is that the site is down while the origin rig is offline.
- **The request budget is the war-room lesson** (`.docs/012`): the edge caches
  fleet 15 s, thread index 30 s, one thread 30 s, so origin load is bounded
  per POP no matter how many tabs are open. The origin reuses
  `rig::status`, which reuses `cached_probe` (≤ 1 probe per rig per 10 s).
  Its own memo holds the fleet for 15 s.
- **Threads are pushed, not pulled.** Each Zed rig runs `fleet-web publish`,
  which reads its own `threads.db` read-only, turns changed threads into
  compact view JSON (secrets redacted, huge tool output truncated) and ships
  only the changed files to the origin rig over the SSH agent-mail already uses.

## Contract (origin ⇄ edge)
Origin: `fleet-web serve [--bind 127.0.0.1:8787]`, exposed with Tailscale Funnel.
Every request must carry
`x-fleet-ts: <unix seconds>` and `x-fleet-sig: <hex blake3 keyed hash>`.
The key is the 32-byte `~/.agent-mail/web/origin.key` (hex in the file).
The signed message is `"{ts}\n{METHOD}\n{path_and_query}"`. Skew must be ≤ 60 s,
and a mismatch gets 401.

| GET | Body |
|---|---|
| `/api/fleet` | `{ at, self_rig, rigs: [{ name, role, ok, error?, latency_ms, probe?: RigProbe }] }`. `RigProbe` is agent_mail's: quiet, load, mem, gpus, agents, contenders, running/queued jobs, owner_mail |
| `/api/threads` | `{ at, threads: [ThreadMeta] }`, newest first, across rigs |
| `/api/threads/{rig}/{id}` | `ThreadView` |

`ThreadMeta = { rig, id, title, updated_at, created_at?, parent_id?, folders: [str], model?, messages, live }`.
`live` means updated within 2 min.
`ThreadView = ThreadMeta + { tokens?: {input, output, cache_read, cache_write}, entries: [Entry] }`.
`Entry = { role: "user"|"agent"|"resume", blocks: [Block] }`.
`Block = { kind: "text"|"thinking"|"tool_use"|"tool_result"|"image"|"mention", text, name?, status? }`.

Edge (Fastly Compute, JS): `/login` → GitHub OAuth (state cookie) → `/auth/callback`
→ allowlist check (`allowed_logins`) → signed session cookie →
`/` fleet + thread list, `/t/{rig}/{id}` thread. A hydrated signal island
refreshes the fleet every 15 s only while the tab is visible.

## Tasks
- [ ] T1 `sqlez::Connection::open_read_only` (never write Zed's live DB from another process)
- [ ] T2 `crates/fleet_web`: thread export (view model, redaction, truncation) + tests
- [ ] T3 `fleet-web publish`: incremental export, ship changed files to the origin rig over ssh (tar stream)
- [ ] T4 `fleet-web serve`: tiny_http origin, blake3 signature check, fleet memo, routes + tests
- [ ] T5 `crates/fleet_web/edge`: Fastly Compute JS app, Preact + signals SSR, GitHub OAuth, edge cache, island
- [ ] T6 local e2e: `fastly compute serve` (Viceroy) → local origin, fake OAuth bypass only in local mode
- [ ] T7 deploy the Fastly service (`*.edgecompute.app`) and the config/secret stores
- [ ] T8 bench: origin request cost, publish cost on the 556-thread m3 DB
- [-] Owner gates: (a) create the GitHub OAuth App (callback `https://<service>.edgecompute.app/auth/callback`) and hand over the client id/secret; (b) enable Tailscale HTTPS + Funnel for the tailnet (CertDomains is empty today); (c) amo + bmo were unreachable on 2026-11-01, so the origin install waits for amo
- [-] Later: Claude Code transcripts as threads too; Cloudflare backend adapter
