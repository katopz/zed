# 051 — Fleet web: thread history + rig queues on the web (Fastly edge, GitHub login)
Status: DEPLOYED, waiting on the owner — `84b6d9bcf2` (origin) + `39a402bbe0` (edge); live at https://locally-discrete-pony.edgecompute.app and fails closed until gate (a) the OAuth App and (b) Funnel on amo are done

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
- [x] T1 `sqlez::Connection::open_read_only` (`84b6d9bcf2`)
- [x] T2 `crates/fleet_web` thread export: view model, redaction (key/token patterns + `*SECRET*=`/`api_key:` assignments), 16 KiB block truncation on char boundaries; 5 tests
- [x] T3 `fleet-web publish [--every SECS]`: incremental by `updated_at`, deletes vanished threads, ships changed files as one tar stream over ssh, keeps `pending.txt` for retries
- [x] T4 `fleet-web serve`: tiny_http origin, 4 workers, blake3 signature (60 s skew, path-bound), fleet served stale while a refresher re-probes every 15 s, and only while someone asked within 2 min (`39a402bbe0`)
- [x] T5 `edge/`: Fastly JS Compute, Preact + signals SSR, GitHub OAuth, allowlist, signed session, CacheOverride TTLs, safe markdown, CSP; 16 node tests; blake3 vector `86c1282c…6cc2d5` matches Rust
- [x] T6 local e2e against the REAL origin + real m3 DB under Viceroy: dev login → index (556 threads, 4 rig cards) 47–50 ms warm → thread page 1.08 MB in 0.29 s; unsigned origin call 401
- [x] T7 deployed service `4mM9Fp0M7wsQonO0mXJvNE` v3: backends `origin` (amo.tail360d46.ts.net:443), `github`, `github_api`; config store `fleet_web_config` (allowed_logins=katopz, origin_host); secret store `fleet_web_secrets` (session_key, origin_key = m3's `~/.agent-mail/web/origin.key`). Prod check: `/` 302 → /login, `/api/fleet` 401, POST 405, CSP/XFO/Referrer headers present, `/login` shows "edge is misconfigured" until the client id exists
- [x] T8 bench (m3, 556 threads, release): first full publish 3.7 s, 206 MB of view JSON, peak RSS 897 MB (one 11.9 MB-zstd thread parsed as a single `Value`); no-change re-publish ≈ 0 s; no `.env` key from zed/seal-remake appears in any exported file
- [ ] Owner gate (a): create a GitHub OAuth App — Homepage `https://locally-discrete-pony.edgecompute.app`, callback `https://locally-discrete-pony.edgecompute.app/auth/callback` — then: `fastly config-store-entry create --store-id=DpahiNurZX266W5VQKZu16 --key=github_client_id --value=<id>` and `printf %s '<secret>' | fastly secret-store-entry create --store-id=EDiS0ObzSIH3MTJ2X0s3Sq --name=github_client_secret --stdin` (or hand them to an agent)
- [ ] Owner gate (b): tailnet admin → enable HTTPS certificates + Funnel (`CertDomains` is empty today)
- [ ] amo origin install (amo + bmo were offline 2026-11-01): `cargo install --path crates/fleet_web`, copy m3's `~/.agent-mail/web/origin.key` to amo (it is the key the edge holds), `fleet-web serve` as a launchd job, `tailscale funnel --bg 8787`
- [ ] Publishers: `fleet-web publish --every 120` as a launchd job on m3 (and amo; the 4090 if it runs Zed threads)
- [-] Stream-parse very large threads to cut the 897 MB first-export peak (only the initial export and edits of that one thread hit it)
- [-] Later: Claude Code transcripts as threads too; Cloudflare backend adapter
