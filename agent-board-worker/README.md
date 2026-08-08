# agent-board-worker

Cloudflare Worker backing Zed's `agent_board` panel. A single-user,
KV-backed notepad that mirrors `auto_prompt::plan_registry` claims across
devices so two agents (e.g. the M3 laptop and the 4090 box) don't clobber each
other's work. See `../.plans/001_agent_board.md` for the full design.

## Endpoints (v1)

### Zed device → worker (existing)

| Method | Path                       | Auth     | Purpose                                  |
|--------|----------------------------|----------|------------------------------------------|
| GET    | `/v1/rooms/{room}`         | none     | Latest device statuses + last 10 msgs + last 10 states + last 10 replies |
| POST   | `/v1/rooms/{room}/status`  | ed25519  | Latest-wins device status (heartbeat). Now also relays to WebSocket/SSE clients. |
| POST   | `/v1/rooms/{room}/msg`     | ed25519  | Append a short message (TTL 1 week). Relays to WS/SSE. |
| POST   | `/v1/rooms/{room}/state`   | ed25519  | Append an agent state broadcast (≤256 chars, last 10 retained). Relays to WS/SSE. |
| GET    | `/healthz`                 | none     | Liveness                                 |

### Browser → worker (Plan 015)

| Method | Path                            | Auth                | Purpose                              |
|--------|---------------------------------|---------------------|--------------------------------------|
| GET    | `/`                             | none                | Single-page HTML dashboard (~15KB).  |
| GET    | `/ws?room={room}`               | Google token (1st WS message) | WebSocket real-time push.     |
| GET    | `/v1/rooms/{room}/events`       | none (read-only)    | SSE stream of room events.           |
| POST   | `/v1/rooms/{room}/reply`        | Google token OR ed25519 | Store operator reply, relay to WS/SSE. |

Writes require headers `X-Device-Id`, `X-Timestamp`, `X-Sig`, `X-Pubkey`. The
signature is `ed25519_sign(request_body_text + "|" + timestamp)` over the raw
bytes (no pre-hash). The first device self-registers; subsequent devices must be
added to the allowlist (KV key `device:{device_id}` = pubkey base64).

### Browser auth (Google OAuth)

The dashboard uses Google Identity Services (GIS) ID-token flow. The browser
sends the JWT as the first WebSocket message or as `Authorization: Bearer
<jwt>` on `POST /reply`. The worker verifies the signature against Google's
JWKS (cached 1h), checks `iss`, `aud`, `exp`, `email_verified`, and asserts
`email == ALLOWED_EMAIL` (env var, default `katopz@gmail.com`). No client
secret is needed for the ID-token flow; the `GOOGLE_CLIENT_ID` env var is
public and embedded in the dashboard HTML.

### Durable Object

`RoomCoordinator` (one per room) holds WebSocket + SSE connections, validates
auth on the first WS message, and relays every write (HTTP POST or WS message)
to all other connections in the same room. Bound via `ROOM_COORDINATOR` in
`wrangler.toml`.

## Deploy

```bash
cd agent-board-worker
npm install            # installs @noble/ed25519 + wrangler
npx wrangler login     # one-time, if not already logged in

# 1. Create the KV namespace:
npx wrangler kv:namespace create AGENT_BOARD
# 2. Paste the printed `id` into wrangler.toml under [[kv_namespaces]].
# 3. Deploy:
npx wrangler deploy
```

Note the deployed URL (e.g. `https://agent-board.<account>.workers.dev`) and set
it in `~/.config/zed/agent_board.json`:

```json
{
  "ssh_key_path": "~/.ssh/id_ed25519",
  "worker_url": "https://agent-board.<account>.workers.dev",
  "room": "zed-agent-board",
  "poll_interval_secs": 15
}
```

Both devices use the **same** room name and point at their **own** ed25519 SSH
key. The first device to write bootstraps the allowlist; the second device must
be registered manually once (copy its `X-Pubkey` into a KV `device:` key, or
briefly clear the namespace so it self-registers, then re-add the first).

## Local smoke test

```bash
npx wrangler dev   # serves on http://localhost:8787
curl http://localhost:8787/healthz
# {"ok":true}
```

Full signed round-trips are exercised from the Rust client's integration path;
the worker alone only needs the healthz + GET shape check.

## KV key shapes

- `device:{device_id}` → base64 raw 32-byte ed25519 pubkey (allowlist)
- `room:{room}:device:{device_id}` → latest-wins status JSON
- `room:{room}:msg:{sortable_key}` → append-only message JSON
- `room:{room}:state:{sortable_key}` → agent state broadcast JSON (Phase 2)
- `room:{room}:reply:{sortable_key}` → operator reply JSON (Plan 015 W6)

All keys expire after 1 week (`DEFAULT_TTL_SECONDS`, configurable in
`wrangler.toml`). Agent states (`state:` prefix) and replies (`reply:` prefix)
are ring-buffered to the last 10 per room; `state_text`, `meta`, and reply
`text` are capped at 256/1024 bytes server-side (defense-in-depth — the Rust
client pre-truncates too).

## Durable Object (Plan 015)

The first `npx wrangler deploy` after adding `RoomCoordinator` runs the
`[[migrations]] tag = "v1"` block and creates the DO namespace. Subsequent
deploys are zero-downtime. If the DO is unavailable (e.g. region issue), HTTP
POSTs still succeed — the KV write happens first, the relay is best-effort.
