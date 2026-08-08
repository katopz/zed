# 015 — Agent Board Web UI (browser dashboard + WebSocket steering)

## Goal
A minimal single-page HTML dashboard served by the Cloudflare Worker so the
operator can see all agent threads across devices in a browser, click any
agent to expand its streaming timeline (accordion), and post **steering
replies** that get injected into the target agent's thread on the target
device — in real-time via WebSocket, no polling.

**Example reply format** (visible in the web UI input bar):
```
REPLY:[m3:f3a2] stop and commit, tests are green
REPLY:[SHIKUWA:b1c9] switch to the develop branch first
```
- `m3` / `SHIKUWA` = device names (from `DeviceIdentity::device_name()`)
- `f3a2` / `b1c9` = first 4 chars of the agent's `session_id` (deterministic,
  stable across page refreshes, resolvable by prefix-match on the device)

**Security**: Google Sign-In (Google Identity Services). Only
`katopz@gmail.com` is allowed. No other accounts can view or post.

## Real-time model: WebSocket, not polling

The board is **push-based** when WebSocket is active. No continuous polling.

### Two trigger paths (no polling needed)

**Path 1 — Zed → Browser (state updates)**:
Zed posts state via HTTP (the existing feeder, unchanged). The Worker checks
if any browser WebSocket is connected. If yes → auto-pushes the update to
the browser instantly. The browser gets real-time without Zed needing a
WebSocket. "Auto-accept" = the browser WebSocket just needs to be connected;
no explicit broadcast request is needed.

**Path 2 — Browser → Zed (steering replies)**:
Browser sends reply via WebSocket → Durable Object relays → Zed receives
instantly **if Zed's WebSocket toggle is ON**. If toggle is OFF → reply sits
in KV, Zed picks up on next feeder poll (fallback, ~15s).

### WebSocket lifecycle
- **Browser**: connects WebSocket on page load, disconnects on page close.
  Only open when the operator is actively looking at the dashboard.
- **Zed**: has a manual WebSocket toggle (broadcast icon on the agent board
  panel). When ON, Zed maintains a WebSocket connection to the worker. When
  OFF, Zed falls back to the existing KV poll model.

### Fallback: KV poll when no WebSocket
When no WebSocket is connected (browser closed + Zed toggle off), the system
falls back to the existing 15s KV poll. Replies are persisted in KV with 7d
TTL so they survive disconnections.

## Substrate-first note (CRITICAL)

### Existing steering mechanism (native agent)
`crates/agent/src/thread.rs` ALREADY implements steering:
- `Thread::set_end_turn_at_next_boundary(bool)` — when true, the current turn
  ends at the next message boundary instead of running to completion.
- The UI sets this via `ThreadView::sync_queue_flag_to_native_thread`.

**This plan EXTENDS the existing steering mechanism — it does not build a parallel one.**
The web reply is just another source of queued messages that sets the steer flag.

### Existing ACP thread message injection
`crates/acp_thread/src/acp_thread.rs`:
- `AcpThread::send(Vec<acp::ContentBlock>)` — sends a user message.
- `AcpThread::push_agent_board_notification(text)` — display-only (P2.3).

**The reply injection uses `AcpThread::send` for both native and Claude agents.**
For native agents, the reply is also flagged as steering via
`set_end_turn_at_next_boundary`. For Claude agents, it's sent as a regular
user message (per operator spec: "maybe just send as usual").

### Existing board notification drain
`crates/agent_ui/src/agent_panel.rs`:
- `AgentPanel::start_notification_drain()` — 10s foreground timer that polls
  `auto_prompt::peer_states::drain_unseen_notifications()` and pushes to the
  active `AcpThread`.

**The reply drain extends this same timer** as the fallback path. When
WebSocket is active, replies arrive via the WebSocket push path (faster).

### Existing worker endpoints
`agent-board-worker/src/index.js`:
- `GET /v1/rooms/{room}` — room snapshot (statuses, messages, states).
- `POST /v1/rooms/{room}/status` — device heartbeat.
- `POST /v1/rooms/{room}/msg` — append a chat message.
- `POST /v1/rooms/{room}/state` — append an agent state broadcast.

**New endpoints added alongside these.** WebSocket upgrade at `GET /ws`.
HTTP POST `/reply` for the fallback path. All existing ed25519-signed writes
also trigger WebSocket relay to connected browsers (auto-accept).

### Wire types
`crates/agent_board/src/types.rs` already has:
- `AgentStateMessage { device_id, device_name, session_id, sub_agent_id, state_text, meta, ts }`
- `RoomSnapshot { statuses, messages, states }`
- `truncate_to_byte_budget`, `MAX_STATE_TEXT_BYTES`, `MAX_ROOM_STATES`

## Substrate inventory
| Concept | Exists as | Location |
|---|---|---|
| Device identity + signing | `DeviceIdentity` | `agent_board/src/identity.rs` |
| Signed KV client | `BoardClient` | `agent_board/src/client.rs` |
| Room snapshot | `RoomSnapshot` | `agent_board/src/types.rs` |
| Agent state broadcasts | `AgentStateMessage` | `agent_board/src/types.rs` |
| Feeder poll loop | `feeder::sync_round` | `agent_board/src/feeder.rs` |
| Notification drain timer | `start_notification_drain` | `agent_ui/src/agent_panel.rs` |
| Native steering | `set_end_turn_at_next_boundary` | `agent/src/thread.rs` |
| ACP message send | `AcpThread::send` | `acp_thread/src/acp_thread.rs` |
| Thread view queue | `ThreadView::add_to_queue` | `agent_ui/src/conversation_view/thread_view.rs` |
| Panel active thread | `active_agent_thread` | `agent_ui/src/agent_panel.rs` |

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  Browser (operator, any device)                                     │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  GET /  →  single-page HTML dashboard                         │  │
│  │  - Google Sign-In button (katopz@gmail.com only)              │  │
│  │  - Agent list (accordion: click to expand thread timeline)    │  │
│  │  - REPLY input bar: REPLY:[device:sess4] text                 │  │
│  │  - WebSocket connection (auto on page load)                   │  │
│  └─────────────────────────┬─────────────────────────────────────┘  │
└────────────────────────────┼────────────────────────────────────────┘
                             │ WebSocket (bidirectional, real-time)
                             │ Authorization: Bearer <google-id-token>
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Cloudflare Worker + Durable Object                                 │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  GET /              → HTML page (static, no auth)              │  │
│  │  GET /v1/rooms/:id  → room snapshot (read, no auth)            │  │
│  │  GET /ws?room=...   → WebSocket upgrade (Google token auth)    │  │
│  │  POST /v1/rooms/:id/reply → store reply (Google or ed25519)    │  │
│  │  POST /v1/rooms/:id/state  → existing, NOW also relay to WS    │  │
│  │  POST /v1/rooms/:id/status → existing, NOW also relay to WS    │  │
│  └─────────────────────────┬─────────────────────────────────────┘  │
│                             │                                       │
│  ┌─────────────────────────▼─────────────────────────────────────┐  │
│  │  Durable Object: RoomCoordinator                               │  │
│  │  - Holds WebSocket connections per room                        │  │
│  │  - Receives writes from HTTP POSTs → relays to all WS clients  │  │
│  │  - Receives WS messages → persists to KV + relays to others    │  │
│  └─────────────────────────┬─────────────────────────────────────┘  │
│  KV: room:{room}:reply:{key} → reply JSON (7d TTL, fallback path)   │
└────────────────────────────┼────────────────────────────────────────┘
                             │ WebSocket (if Zed toggle ON)
                             │ OR HTTP poll (if Zed toggle OFF, fallback)
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Zed (m3 laptop)                                                    │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  Agent Board Panel                                            │  │
│  │  - 📡 WebSocket toggle icon (click to enable real-time mode)   │  │
│  │  - When ON: maintains WebSocket → receives replies instantly   │  │
│  │  - When OFF: feeder poll loop picks up replies (~15s)          │  │
│  │  - Either way: HTTP POSTs to worker (feeder) auto-relay to     │  │
│  │    connected browsers via Durable Object                        │  │
│  └───────────────────────────────────────────────────────────────┘  │
│  Reply injection:                                                   │
│  - Resolve sess4 prefix → active session_id                         │
│  - Find AcpThread by session_id                                     │
│  - Native: send() + set_end_turn_at_next_boundary(true)             │
│  - Claude: send() as regular user message                           │
│  Agent Panel chat: "🌐 Web Reply" badge for pending replies         │
└─────────────────────────────────────────────────────────────────────┘
```

## Security model

### Google OAuth (web UI → worker)
1. Browser loads `GET /` (static HTML, no secrets — just the GIS script tag).
2. User clicks "Sign in with Google" → Google Identity Services popup.
3. GIS returns a **Google ID token** (JWT, ~1h expiry).
4. Browser sends token on WebSocket connection as the first message, and as
   `Authorization: Bearer <google-id-token>` on HTTP POST /reply.
5. Worker verifies the JWT:
   - Fetch Google JWKS from `https://www.googleapis.com/oauth2/v3/certs`
   (cache in Worker global, refresh every 1h).
   - Verify signature + `iss` + `aud` + `exp`.
   - Check `email == ALLOWED_EMAIL` (env var, default `katopz@gmail.com`)
   and `email_verified: true`.
6. If valid → accept WebSocket / process write. If not → 401 / close WS.

### Existing ed25519 gate (Zed devices → worker)
Unchanged. Zed devices still sign HTTP writes with their SSH key. The
worker accepts either a valid Google ID token OR ed25519 signature.

### WebSocket authentication
- **Browser**: sends Google ID token as the first WebSocket message after
  upgrade. Durable Object validates before accepting further messages.
- **Zed**: sends ed25519-signed challenge response as the first WS message.

### No secrets in the worker
- Google Client ID is public (embedded in HTML + worker config).
- No client secret needed for GIS ID token flow.
- The allowlist email is in worker config (env var).

## Session routing: 4-char prefix

The `session_id` is typically a UUID or opaque string. The REPLY format uses
the **first 4 characters** as a human-readable routing token:

```
REPLY:[m3:f3a2] stop and commit
            ^^^^
            first 4 chars of session_id
```

**Resolution on the device**:
- The device iterates its active `AcpThread` entities.
- For each, it reads the session_id and checks `session_id.starts_with(reply_target_prefix)`.
- If exactly one match → inject. If zero matches → log + skip (agent gone).
- If multiple matches (collision) → inject into the first match + log a warning.

**Collision risk**: with 4 hex chars, there are 65,536 possible prefixes.
For 2-5 concurrent agents, the collision probability is negligible. The log
warning makes any collision visible. If collisions become a real problem,
the prefix length can be increased to 6 or 8 chars.

## Tasks

### W1 — Worker HTML page
- [x] `GET /` → returns single-page HTML dashboard (inline CSS + JS, ~15KB).
      Static — no SSR, no framework. Data fetched via `GET /v1/rooms/{room}`
      + real-time push via WebSocket.
- [x] HTML includes:
      - Google Sign-In button (GIS script from accounts.google.com).
      - Room dashboard: device sections, each with expandable agent items.
      - REPLY input bar (shows `REPLY:[device:sess4] ____` when clicked).
      - WebSocket auto-connect on page load + Google auth.
      - Connection status indicator (🟢 connected / 🔴 disconnected).

### W2 — Accordion agent timeline view
- [x] Each device is a collapsible section. Clicking a device expands its
      agents. Clicking an agent expands its state timeline (last 10 states,
      newest first).
- [x] The timeline updates in real-time via WebSocket push (no manual refresh).
      When a new state arrives for the expanded agent, it prepends to the list.
- [x] Clicking an agent item populates the REPLY input:
      `REPLY:[{device_name}:{sess4}] ` and focuses the input.
      `sess4` = `session_id.slice(0, 4)`.
- [x] Sending a reply: POST via WebSocket message `{ type: "reply", target_device, target_session_prefix, text }`.

### W3 — Durable Object: RoomCoordinator
- [x] `agent-board-worker/src/index.js` — Durable Object class `RoomCoordinator`
      (kept in the same file per the task spec, not a separate
      `room_coordinator.js`, to match the task's explicit instruction).
      - `fetch(handler)` → WebSocket upgrade, stores connection in `this.sessions`.
      - Validates auth on first WS message (Google token or ed25519 challenge).
      - `onWebSocketMessage(ws, msg)` → parse JSON, persist to KV if needed,
        relay to all OTHER connected WS clients in the same room. (Standard
        WebSocket API with `addEventListener`; the DO won't hibernate but stays
        alive while connections are open — fine for single-user low-volume.)
      - close/error listeners remove from `this.sessions`.
      - HTTP POST handlers call the worker-level `relayToRoom(env, room, msg)`
        helper, which fetches `/relay` on the per-room DO stub.
- [x] `wrangler.toml`: add `[[durable_objects.bindings]]` + `[[migrations]]`.
- [x] Room name → Durable Object ID: `env.ROOM_COORDINATOR.idFromName(room)`.

### W4 — Worker: WebSocket upgrade + relay integration
- [x] `GET /ws?room={room}` → WebSocket upgrade route.
      Creates/gets the RoomCoordinator DO for the room, forwards the upgrade.
- [x] Existing HTTP POST handlers (`/status`, `/msg`, `/state`) now also call
      `relayToRoom(env, room, JSON.stringify(payload))` after KV write. This is
      the "auto-accept" path: any HTTP write from Zed auto-relays to connected
      browsers via the DO.
- [x] `POST /v1/rooms/:room/reply` → stores reply in KV + relays via DO.
      Auth: Google token (web) or ed25519 (Zed).
- [x] `GET /v1/rooms/:room/events?device={device_name}` → SSE stream
      (read-only push, 15s keepalive, TransformStream-based).

### W5 — Google OAuth verification in worker
- [x] Worker verifies Google ID token for WebSocket auth + POST /reply:
      - Fetch + cache Google JWKS (1h TTL in module-level `googleJwksCache`).
      - Verify JWT signature (Web Crypto RSASSA-PKCS1-v1_5 / SHA-256), `iss`,
        `aud`, `exp`, `email_verified`.
      - Assert `email == ALLOWED_EMAIL` (env var, default `katopz@gmail.com`).
- [x] The verification function `verifyGoogleToken(token, jwks, clientId, allowedEmail)`
      is pure (takes token + JWKS, returns email or null) so it's unit-testable
      with mock JWKS. Validated end-to-end with a self-signed RSA JWT in
      `/tmp/test_verify.mjs` (5 cases: valid, wrong aud, wrong email, tampered
      sig, no kid match).

### W6 — Reply wire type + client
- [ ] `agent_board/src/types.rs`: add `WebReply` struct:
      `{ v, target_device, target_session_prefix, text, author_email, ts }`.
      `target_session_prefix` is the 4-char prefix (NOT the full session_id —
      the web UI doesn't need the full id, and the prefix is the routing key).
- [ ] `RoomSnapshot` gains `#[serde(default)] replies: Vec<WebReply>`.
- [ ] `BoardClient::post_reply()` — POST to `/v1/rooms/{room}/reply`
      with ed25519 signature (for Zed-originated replies, future use).

### W7 — Zed WebSocket client + toggle
- [ ] `agent_board/src/websocket_client.rs` — new module.
      `BoardWebSocket` struct: maintains a WebSocket connection to the worker.
      - Connects to `wss://{worker_url}/ws?room={room}` with ed25519 auth.
      - On message: parse JSON, if it's a reply targeting this device →
        call `auto_prompt::peer_states::inject_web_reply(session_prefix, text)`.
      - Auto-reconnect with exponential backoff (1s, 2s, 4s, 8s, max 30s).
      - `fn is_connected(&self) -> bool`.
- [ ] `agent_board/src/agent_board.rs`: add WebSocket toggle state to panel.
      - 📡 icon in the panel header. Click toggles `websocket_enabled: bool`.
      - Persist toggle state to `~/.config/zed/agent_board.json`.
      - When ON: create `BoardWebSocket`, store in panel field.
      - When OFF: drop the `BoardWebSocket` (disconnects).

### W8 — Reply drain (fallback path via feeder poll)
- [ ] `agent_board/src/feeder.rs`: `sync_round` now also fetches replies from
      the room snapshot. For each reply where `target_device == device_name()`:
      - Call `peer_states::inject_web_reply(target_session_prefix, text)`.
      - (Reply stays in KV until TTL — it's idempotent on the injection side
        via dedup, same pattern as notification signatures.)
- [ ] `auto_prompt/src/peer_states.rs`: add:
      - `inject_web_reply(session_prefix: String, text: String)` — stores in
        a `LazyLock<RwLock<Vec<(String, String)>>>` process-global.
      - `drain_web_replies() -> Vec<(String, String)>` — drains + clears.

### W9 — Reply injection into agent threads (agent_panel)
- [ ] `agent_ui/src/agent_panel.rs`: extend `start_notification_drain` to
      also call `peer_states::drain_web_replies()` on each tick (10s).
      This is the fallback path — when WebSocket is active, replies arrive
      faster via the WS push path but still go through the same drain.
      For each `(session_prefix, text)`:
      - Resolve prefix → session_id via active threads.
      - Find the `AcpThread` matching that session_id.
      - Native: `thread.send(text)` + `set_end_turn_at_next_boundary(true)`.
      - Claude: `thread.send(text)`.
      - Not found: log + skip.

### W10 — Thread lookup by session_id prefix
- [ ] `agent_ui/src/agent_panel.rs`: add
      `fn thread_for_session_prefix(&self, prefix: &str, cx: &App) -> Option<Entity<AcpThread>>`.
      Scans all active conversation views, matches `session_id.starts_with(prefix)`.
      Logs a warning if multiple matches (collision).

### W11 — Zed chat panel: web reply indicator
- [ ] `agent_ui/src/conversation_view/thread_view.rs`: small badge in the
      chat panel showing web-originated replies.
      Format: `🌐 REPLY:[device:sess4] text` with accept/dismiss buttons.
      Gives visibility into web replies from within Zed, not just the browser.

### W12 — Tests
- [ ] `agent_board/src/types.rs`: `WebReply` serialization, `RoomSnapshot`
      with replies deserialization, old snapshots without replies (default).
- [ ] `auto_prompt/src/peer_states.rs`: `inject_web_reply` + `drain_web_replies`
      round-trip. Multiple replies. Drain clears. Different sessions.
- [ ] `agent_board/src/feeder.rs`: reply extraction from snapshot, targets
      local device → calls `inject_web_reply`.
- [ ] Worker JS: `verifyGoogleToken` with mock JWKS + fake JWT. Allowlist check.
- [ ] Session prefix resolution: exact match, no match, collision (first match
      + warning log).

## Perf/sec considerations

- **No continuous polling when WebSocket active**: both browser and Zed
  receive pushes. Polling only runs as fallback when WebSocket is off.
- **Durable Object cost**: single-user, low volume. DO persists per room,
  handles 1-5 WebSocket connections. Well within free tier limits.
- **WebSocket reconnect**: exponential backoff prevents thundering herd on
  worker restart. Max 30s backoff.
- **Relay latency**: WebSocket relay is <100ms (same-region DO). Reply from
  browser → agent thread injection is <500ms total (WS relay + thread.send).
- **KV still the source of truth**: WebSocket relay is ephemeral. KV
  persists everything with 7d TTL. New WebSocket clients get full state via
  `GET /v1/rooms/{room}` on connect, then incremental updates via WS.
- **Durable Object hibernation**: Cloudflare's Hibernating WebSockets API
  allows the DO to sleep between messages, reducing cost. The DO wakes on
  any WebSocket message or HTTP relay request.
- **HTTP POST relay overhead**: each existing POST (status/state/msg) adds
  one DO fetch call (~1ms) to relay to connected browsers. Negligible vs
  the existing KV write (~50ms).
- **4-char prefix collision**: 65,536 possible prefixes. For ≤5 concurrent
  agents, collision probability is <0.01%. Log warning makes any collision
  visible. Prefix length configurable if needed.

## Risks

- **Durable Object complexity**: DOs require a migration in wrangler.toml.
  First deploy creates the DO namespace. Subsequent deploys are zero-downtime.
  If the DO is unavailable, WebSocket relay fails → fallback to KV poll.
  HTTP POSTs still work (they write to KV first, then attempt relay).
- **WebSocket auth**: the first WS message carries the auth token. If the
  token is invalid, the DO closes the connection with code 4001. The browser
  must re-authenticate when the Google token expires (~1h). The JS client
  auto-refreshes the token via GIS before expiry.
- **Reply delivery guarantee**: replies are best-effort (at-most-once). If
  the target device's WebSocket is off AND the poll misses the reply before
  TTL expiry, the reply is lost. Mitigated by: 7d TTL is generous; the
  operator can re-post from the browser. No ack/retry mechanism for v1.
- **Session prefix collision**: two agents with the same first-4-char prefix.
  The reply goes to the first match. Mitigated by warning log + the operator
  seeing both agents in the dashboard and noticing if a reply goes to the
  wrong one.
- **Google JWKS fetch failure**: if the worker can't fetch JWKS (network
  issue), auth fails closed (401). The operator can use the Zed device path
  (ed25519) which doesn't depend on JWKS.

## Dependency direction
```
agent_ui ─→ auto_prompt ─→ (peer_states, plan_registry)
    │              │
    │              ▼ inject_web_reply (BY agent_board feeder or WS client)
    ├────→ agent_board ──→ (identity, client, feeder, ws_client, worker)
    │              │
    │              ▼
    └────→ acp_thread (AcpThread::send for injection)
```
No new crate dependencies. `agent_board` gains `websocket_client` module.
`auto_prompt::peer_states` gains `inject_web_reply` + `drain_web_replies`.

## GOAT gate
- [ ] Web UI loads at `GET /` and shows room dashboard.
- [ ] Google Sign-In works; only `katopz@gmail.com` accepted.
- [ ] WebSocket connects on page load; status indicator shows 🟢.
- [ ] Clicking an agent expands its state timeline (accordion).
- [ ] REPLY input populates `REPLY:[device:sess4]` when clicking an item.
- [ ] State updates from Zed appear in browser instantly via WebSocket (no refresh).
- [ ] Reply posted from browser reaches target Zed device:
      - WebSocket ON: <1s.
      - WebSocket OFF: <15s (poll fallback).
- [ ] Native agent thread receives the reply as a steering message.
- [ ] Claude agent thread receives the reply as a regular user message.
- [ ] Zed 📡 toggle: ON = instant replies, OFF = poll fallback.
- [ ] Zed chat panel shows 🌐 badge for web-originated replies.
- [ ] 4-char session prefix resolves correctly (exact match, no collision).
- [ ] Replies targeting a non-existent session are silently skipped (no crash).
- [ ] Wire contract: `WebReply` JSON round-trips between worker JS and Rust types.
- [ ] Old room snapshots without `replies` field still deserialize.
- [ ] Worker auto-relays HTTP POSTs to connected browser WebSockets (auto-accept).
