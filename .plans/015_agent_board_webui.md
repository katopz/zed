# 015 — Agent Board Web UI (browser dashboard + steering replies)

## Goal
A minimal single-page HTML dashboard served by the Cloudflare Worker so the
operator can see all agent threads across devices in a browser, click any
agent to expand its streaming timeline (accordion), and post **steering
replies** that get injected into the target agent's thread on the target
device.

**Example reply format** (visible in the web UI input bar):
```
REPLY:[m3:a1] stop and commit, tests are green
REPLY:[SHIKUWA:a1] switch to the develop branch first
```
- `m3` / `SHIKUWA` = device names (from `DeviceIdentity::device_name()`)
- `a1` = short agent label assigned by the UI (sequential per device)

**Security**: Google Sign-In (Google Identity Services). Only
`katopz@gmail.com` is allowed. No other accounts can view or post.

## Substrate-first note (CRITICAL)

### Existing steering mechanism (native agent)
`crates/agent/src/thread.rs` ALREADY implements steering:
- `Thread::set_end_turn_at_next_boundary(bool)` — when true, the current turn
  ends at the next message boundary instead of running to completion.
- `ThreadEventStream::send_user_message()` — queues a `UserMessage` event.
- The UI sets `end_turn_at_next_boundary` via `ThreadView::sync_queue_flag_to_native_thread`.

**This plan EXTENDS the existing steering mechanism — it does not build a parallel one.**
The web reply is just another source of queued messages that sets the steer flag.

### Existing ACP thread message injection
`crates/acp_thread/src/acp_thread.rs`:
- `AcpThread::send(Vec<acp::ContentBlock>)` — sends a user message.
- `AcpThread::send_command(...)` — sends without displaying a user-message bubble.
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

**The reply drain extends this same timer** — it also checks for new web
replies targeting this device's agents and injects them.

### Existing worker endpoints
`agent-board-worker/src/index.js`:
- `GET /v1/rooms/{room}` — room snapshot (statuses, messages, states).
- `POST /v1/rooms/{room}/status` — device heartbeat.
- `POST /v1/rooms/{room}/msg` — append a chat message.
- `POST /v1/rooms/{room}/state` — append an agent state broadcast.

**New endpoints are added alongside these.** The existing ed25519 signature
gate protects writes; the web UI adds a Google OAuth layer on top.

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
┌─────────────────────────────────────────────────────────────┐
│  Browser (any device)                                       │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  GET /  →  single-page HTML dashboard                 │  │
│  │  - Google Sign-In button (katopz@gmail.com only)      │  │
│  │  - Agent list (accordion: click to expand thread)     │  │
│  │  - REPLY input bar: REPLY:[device:agent] text         │  │
│  │  - Auto-refresh every 5s when expanded                 │  │
│  └───────────────────────────────────────────────────────┘  │
└──────────────────────────┬──────────────────────────────────┘
                           │ HTTPS (Google ID token in Authorization header)
                           ▼
┌─────────────────────────────────────────────────────────────┐
│  Cloudflare Worker                                          │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  GET /              → HTML page (static, no auth)      │  │
│  │  GET /v1/rooms/:id  → room snapshot (no auth, read)    │  │
│  │  POST /v1/rooms/:id/reply → store steering reply       │  │
│  │    Requires: valid Google ID token (katopz@gmail.com)  │  │
│  │    OR: valid ed25519 signature (existing devices)      │  │
│  └───────────────────────────────────────────────────────┘  │
│  KV: room:{room}:reply:{key} → reply JSON                   │
└──────────────────────────┬──────────────────────────────────┘
                           │ Poll every 15s
                           ▼
┌─────────────────────────────────────────────────────────────┐
│  Zed (m3 laptop)                                            │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  feeder::sync_round → finds replies targeting m3       │  │
│  │  → resolves agent label to session_id                  │  │
│  │  → injects into target AcpThread                       │  │
│  │    native: send() + set_end_turn_at_next_boundary      │  │
│  │    claude: send() as regular user message              │  │
│  └───────────────────────────────────────────────────────┘  │
│  Agent Panel (chat): new "Web Replies" badge/input row       │
│  shows pending + delivered replies from the web UI          │
└─────────────────────────────────────────────────────────────┘
```

## Security model

### Google OAuth (web UI → worker)
1. Browser loads `GET /` (static HTML, no auth needed — the page itself
   contains no secrets, just the Google Sign-In client).
2. User clicks "Sign in with Google" → Google Identity Services (GIS) popup.
3. GIS returns a **Google ID token** (JWT, ~1h expiry).
4. Browser stores token in `sessionStorage`, sends it as
   `Authorization: Bearer <google-id-token>` on every write request.
5. Worker verifies the JWT:
   - Fetch Google JWKS from `https://www.googleapis.com/oauth2/v3/certs`
   (cache in Worker global, refresh every 1h).
   - Verify signature + `iss: "https://accounts.google.com"` +
   `aud: <client-id>` + `exp` not expired.
   - Check `email == "katopz@gmail.com"` and `email_verified: true`.
6. If valid → proceed with the write. If not → 401.

### Existing ed25519 gate (Zed devices → worker)
Unchanged. Zed devices still sign writes with their SSH key. The Google
OAuth is a **second auth path** for the web UI only — the worker accepts
either a valid Google ID token OR a valid ed25519 signature on writes.

### No secrets in the worker
- Google Client ID is public (embedded in the HTML + worker config).
- No client secret needed for GIS ID token flow (token-only).
- The `katopz@gmail.com` allowlist is hardcoded in the worker.

## Tasks

### W1 — Worker HTML page + static assets
- [ ] `GET /` → returns a single-page HTML dashboard (inline CSS + JS, no
      external dependencies except Google Identity Services script).
      The page is static — all data is fetched via `GET /v1/rooms/{room}`
      from JS. No SSR.
- [ ] HTML includes:
      - Google Sign-In button (GIS script from accounts.google.com).
      - Room dashboard: list of devices, each with expandable agent states.
      - REPLY input bar (shows `REPLY:[device:agent] ____` when an agent
        item is clicked).
      - Auto-refresh toggle (default 5s when an accordion is expanded).
- [ ] HTML/CSS is minimal: system font, dark theme, responsive, no
      framework. Vanilla JS only.

### W2 — Agent label assignment + accordion view
- [ ] JS assigns short labels to agents: `a1`, `a2`, `a3`... per device,
      sequentially by first-seen order. Stored in a JS `Map<session_id, label>`.
- [ ] Each device is a collapsible section. Clicking a device expands its
      agents. Clicking an agent expands its state timeline (last 10 states,
      newest first).
- [ ] Clicking an agent item populates the REPLY input:
      `REPLY:[{device_name}:{label}] ` and focuses the input.
- [ ] The timeline auto-refreshes every 5s when expanded (re-fetches room
      snapshot, filters states by device+session).

### W3 — Google OAuth verification in worker
- [ ] `POST /v1/rooms/:room/reply` accepts either:
      (a) `Authorization: Bearer <google-id-token>` (web UI path), or
      (b) existing ed25519 headers (Zed device path).
- [ ] Worker verifies Google ID token:
      - Fetch + cache Google JWKS (1h TTL in global scope).
      - Verify JWT signature, `iss`, `aud`, `exp`.
      - Assert `email == ALLOWED_EMAIL` (env var `ALLOWED_EMAIL`, default
      `katopz@gmail.com`).
- [ ] Store reply as `room:{room}:reply:{sortable_key}` in KV with 7d TTL.
      Reply JSON: `{ v, target_device, target_session_id, text, author_email, ts }`.

### W4 — Reply wire type + client posting
- [ ] `agent_board/src/types.rs`: add `WebReply` struct:
      `{ v, target_device, target_session_id, text, author_email, ts }`.
- [ ] `RoomSnapshot` gains `#[serde(default)] replies: Vec<WebReply>`.
- [ ] `BoardClient::post_reply()` — POST to `/v1/rooms/{room}/reply` with
      ed25519 signature (for Zed-originated replies if needed in the future).
      The web UI uses the Google token path instead.

### W5 — Reply drain in feeder + session resolution
- [ ] `agent_board/src/feeder.rs`: `sync_round` now also fetches replies.
      For each reply where `reply.target_device == identity.device_name()`:
      - Resolve `target_session_id` to an active thread.
      - Call `auto_prompt::peer_states::inject_web_reply(session_id, text)`
      (new function — stores pending replies in a process-global, keyed by
      session_id).
      - Delete the reply from local tracking (mark as delivered) so it
      doesn't re-inject on the next poll.
- [ ] `auto_prompt/src/peer_states.rs`: add `inject_web_reply(session_id, text)`
      and `drain_web_replies() -> Vec<(String session_id, String text)>`.
      Stored in a `LazyLock<RwLock<HashMap<String, Vec<String>>>>` keyed by
      session_id.

### W6 — Reply injection into agent threads (agent_panel)
- [ ] `agent_ui/src/agent_panel.rs`: extend `start_notification_drain` to
      also call `peer_states::drain_web_replies()` on each tick (10s).
      For each `(session_id, text)`:
      - Find the `AcpThread` matching `session_id` (scan active threads,
      not just the active one — the reply can target any thread).
      - If found and it's a native agent: `thread.send(text)` +
      `set_end_turn_at_next_boundary(true)` (steering).
      - If found and it's a Claude agent: `thread.send(text)` (regular send).
      - If not found: log + skip (the agent thread may have closed).

### W7 — Thread lookup by session_id
- [ ] `agent_ui/src/agent_panel.rs`: add a method to find an `AcpThread`
      by session_id across all active conversation views (not just the
      active one). The reply can target any thread on this device.
      `fn thread_for_session(&self, session_id: &str, cx: &App) -> Option<Entity<AcpThread>>`

### W8 — Zed chat panel: web replies indicator
- [ ] `agent_ui/src/conversation_view/thread_view.rs`: add a small badge
      or input row in the chat panel showing pending web replies for the
      current thread. Format: `🌐 REPLY:[device:agent] text` with a
      "Deliver" button to manually inject (in case auto-inject was skipped).
      This gives the operator visibility into web-originated replies from
      within Zed, not just the browser.

### W9 — Worker contract tests
- [ ] `agent_board/src/types.rs`: add tests for `WebReply` serialization
      and `RoomSnapshot` with replies deserialization.
- [ ] Verify the exact JSON shapes match what the worker JS produces.
- [ ] Test that old snapshots without `replies` field still deserialize
      (`#[serde(default)]`).

### W10 — Reply drain + injection tests
- [ ] `auto_prompt/src/peer_states.rs`: tests for `inject_web_reply` +
      `drain_web_replies` round-trip. Multiple replies for the same
      session. Replies for different sessions. Drain clears pending.
- [ ] `agent_board/src/feeder.rs`: test that `sync_round` extracts
      replies targeting the local device and calls `inject_web_reply`.

### W11 — Google OAuth token verification test
- [ ] Worker JS: add a unit-testable `verifyGoogleToken` function that
      takes a mock JWKS provider + a fake JWT and returns the email.
- [ ] Test the allowlist check: `katopz@gmail.com` → allow, anything else
      → reject. (This is a pure function test — no real Google calls.)

## Perf/sec considerations

- **Poll cost**: the feeder already polls every 15s. Adding replies to the
  GET response adds ~1KB (10 replies × ~100 bytes). Negligible vs the
  existing 5KB snapshot.
- **Reply latency**: worst case 15s (poll interval) + 10s (drain timer) =
  25s from web post to agent injection. This is acceptable for "steering"
  — the operator is redirecting high-level strategy, not doing real-time
  control. If faster delivery is needed later, the drain timer can be
  reduced to 2-3s.
- **KV write rate**: replies are low-volume (a few per session). No risk
  of hitting KV limits.
- **JWKS cache**: fetched once per hour per Worker isolate. The global
  scope persists across requests within the same isolate. ~50KB of JSON,
  negligible memory.
- **HTML page size**: single-file inline CSS+JS, ~10-15KB gzipped. One-time
  fetch, cached by the browser.
- **No WebSocket/SSE**: the board is poll-based (KV). Real-time streaming
  would require Durable Objects or D1, which is overkill for a single-user
  dashboard. Auto-refresh every 5s is sufficient.
- **Session_id exposure**: the web UI displays session_ids in the DOM for
  the reply targeting. Since the GET endpoint is unauthenticated (read-only)
  and the board is single-user, this is not a privacy concern.

## Risks

- **Google OAuth complexity**: verifying JWTs in a Worker requires fetching
  JWKS, which is an outbound HTTP call. Workers support `fetch()` from
  within the handler. If JWKS fetch fails, writes fail closed (401) —
  the operator can always use the Zed device path (ed25519) instead.
- **Session resolution**: the web reply targets a session_id, but the agent
  thread may have been closed or replaced. The injection silently skips
  if no matching thread is found — the reply is logged but not lost (it
  remains in KV until TTL expiry).
- **Race condition**: two devices could both try to deliver the same reply
  if the target device name is ambiguous. Mitigated by: only the device
  whose `device_name()` matches `reply.target_device` picks up the reply.
  Since device names are distinct per device, only one device picks it up.
  The reply is marked as delivered in the local tracking (not in KV —
  other devices don't need to know it was delivered).
- **Reply spoofing**: a malicious actor could post a reply with a forged
  `target_device`. The reply would be stored in KV but never picked up
  (no device matches the forged name). The ed25519 gate prevents unsigned
  writes from Zed devices; the Google gate prevents unsigned writes from
  the web UI.

## Dependency direction
```
agent_ui ─→ auto_prompt ─→ (peer_states, plan_registry)
    │              │
    │              ▼ inject_web_reply (BY agent_board feeder)
    ├────→ agent_board ──→ (identity, client, feeder, worker)
    │              │
    │              ▼
    └────→ acp_thread (AcpThread::send for injection)
```
No new crate dependencies. `auto_prompt::peer_states` gains
`inject_web_reply` + `drain_web_replies` (same pattern as the existing
notification drain). `agent_ui` gains thread-by-session-id lookup.

## GOAT gate
- [ ] Web UI loads at `GET /` and shows room dashboard.
- [ ] Google Sign-In works; only `katopz@gmail.com` accepted.
- [ ] Clicking an agent expands its state timeline (accordion).
- [ ] REPLY input populates `REPLY:[device:agent]` when clicking an item.
- [ ] Reply posted from browser reaches the target Zed device within 25s.
- [ ] Native agent thread receives the reply as a steering message.
- [ ] Claude agent thread receives the reply as a regular user message.
- [ ] Zed chat panel shows a web-reply indicator for pending replies.
- [ ] Replies targeting a non-existent session are silently skipped (no crash).
- [ ] Wire contract: `WebReply` JSON round-trips between worker JS and Rust types.
- [ ] Old room snapshots without `replies` field still deserialize (`#[serde(default)]`).
