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
| GET    | `/ws?room={room}`               | GitHub token (1st WS message) | WebSocket real-time push.     |
| GET    | `/v1/rooms/{room}/events`       | none (read-only)    | SSE stream of room events.           |
| POST   | `/v1/rooms/{room}/reply`        | GitHub token OR ed25519 | Store operator reply, relay to WS/SSE. |
| POST   | `/auth/github/device`           | none                | Start device-flow sign-in (user_code). |
| POST   | `/auth/github/poll`             | none                | Poll for the device-flow token.      |

Writes require headers `X-Device-Id`, `X-Timestamp`, `X-Sig`, `X-Pubkey`. The
signature is `ed25519_sign(request_body_text + "|" + timestamp)` over the raw
bytes (no pre-hash). The first device self-registers; subsequent devices must be
added to the allowlist (KV key `device:{device_id}` = pubkey base64).

### Browser auth (GitHub, device flow — replaces Google OAuth)

The dashboard signs in with GitHub via the OAuth **device flow** — the same
identity model Zed itself uses (GitHub sign-in). Why device flow: it needs
only a public `GITHUB_CLIENT_ID`, so the worker keeps its "no secrets"
property (no client secret, ever). Flow: the dashboard fetches a `user_code`
from `POST /auth/github/device`, the operator authorizes at
`github.com/login/device`, the dashboard polls `POST /auth/github/poll` until
GitHub issues the token, then sends it as the first WebSocket message
(`{type:"auth", github_token}`) or as `Authorization: Bearer <token>` on
`POST /reply`. Tokens are opaque, so verification asks `api.github.com/user`
whose token it is (cached 10 min by sha256(token)) and asserts
`login == ALLOWED_LOGIN` (env var, default `katopz`).

**Setup** (one-time): GitHub → Settings → Developer settings → OAuth Apps →
New OAuth App (callback URL can be the worker URL; it is unused by device
flow) → enable **Device Flow** in the app's advanced settings → paste the
client id into `GITHUB_CLIENT_ID` in `wrangler.toml` → `npx wrangler deploy`.

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

## Live deployment (2026-08-18)

- URL: `https://agent-board-worker.foxfox.workers.dev`
- KV namespace `AGENT_BOARD`: `d2cdb46dee30430b96dbf5b439ed318b`
- First device registered (bootstrap): the operator's real SSH-key identity
  (`device_id` = blake3 of `~/.ssh/id_ed25519` pubkey) — same identity Zed's
  `DeviceIdentity` derives, so the panel needs no extra registration step.
- `GITHUB_CLIENT_ID` is still empty — browser GitHub Sign-In disabled until
  an OAuth App client id is set (device flow enabled, see setup above) and
  redeployed. ed25519 paths (Zed device) are fully live.
- Zed-side config lives at `~/.config/zed/agent_board.json`
  (`worker_url` + `realtime_enabled: true`).

### GOAT verification suite

`test/goat.mjs` (run from `agent-board-worker/test/`):

```bash
npm install
node goat.mjs https://agent-board-worker.foxfox.workers.dev goat-test
```

Signs with the operator's real SSH key (byte-identical to Zed's requests) and
asserts 16 checks: dashboard HTML markers, room snapshot, SSE relay latency
(<1s warm), status/state/msg/reply POSTs, typed reply relay + KV persistence,
WS ed25519 auth + fan-out, bad-token close 4001, unknown-device 403, and
anti-replay 401. Exit 0 = all pass.

Known KV caveat surfaced by T9: the device-allowlist bootstrap gate reads
`KV list`, which is eventually consistent (≤60s). A second device registering
within that window of the *first-ever* registration can self-register before
the list converges. Steady state rejects unknown devices correctly. Single-
user tool: accepted tradeoff; a strongly consistent DO registry is the fix if
this ever matters.
