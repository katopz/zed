# 001 — Agent Board (multi-device / multi-agent notepad)

## Goal
A cross-device, cross-agent "notepad board" that mirrors the existing in-process
`auto_prompt::plan_registry` claims to/from Cloudflare KV, plus an append-only
short-message feed (last 10). Lets agents on the M3 laptop and the 4090 box (or
any two devices) see what the other is working on and reason about it during
`auto_prompt`. Single user (katopz); auth = ed25519 SSH-key signatures against a
pubkey allowlist.

## Substrate-first note (CRITICAL)
`crates/auto_prompt/src/plan_registry.rs` ALREADY implements:
- claim / release / heartbeat / stale-reap (300s)
- `is_claimed_by_other(plan, session)`
- `format_claims_for_context(session)` → the exact "these plans are taken, don't
  pick them" string that `auto_prompt`'s orchestration LLM already consumes.

**This board is a network-backed MIRROR of plan_registry, not a parallel system.**
- Remote claims are injected INTO `plan_registry` via `try_claim` using a
  `remote:{device_id}:{session_id}` composite session id. Zero `auto_prompt` core
  changes required for requirement #6 — the existing context injection picks them up.
- Local claims are read via `active_claims()` (filtering out `remote:` prefix) and
  POSTed to the worker so other devices can see them.

So requirement #6 ("other agent can reason about other msg when auto_prompt
fires") is delivered by the existing substrate + a feeder loop. No new context
plumbing in `auto_prompt`.

## What the user spec missed (gap analysis)
1. **KV write contention** — storing all msgs under one key = last-write-wins =
   lost messages under concurrent agents. Fix: per-message keys
   `room:{room}:msg:{ts}` + KV `list()`, and per-device latest-wins status keys
   `room:{room}:device:{device_id}`.
2. **KV eventual consistency** — up to ~60s global propagation. Poll, don't treat
   as a real-time lock. Heartbeat reaper handles staleness.
3. **SSH-pubkey "auth" must prove private-key possession** — sending a pubkey
   hash as identity is just a password. Fix: sign
   `blake3(canonical_json(payload))` with ed25519 SSH key; worker verifies
   against allowlist of known 32-byte pubkeys.
4. **Heartbeat ≠ append** — active status must be latest-wins per device, not a
   growing log. Modeled as two shapes: status (latest per device) + msg feed
   (append, last 10).
5. **Stale reaping** — a crashed device leaves "working" forever. TTL (1 week) is
   archival; active staleness window (5 min, matching plan_registry) governs
   "is this claim live". Board re-heartbeats remote claims into plan_registry
   every poll; local GC reaps dead ones after 300s.
6. **Offline degradation** — if the worker is unreachable, fall back to the
   existing in-process plan_registry (still works on one machine). Remote layer
   is strictly additive.
7. **Device id ≠ session id** — device id = `blake3(ed25519_pubkey_32)`; multiple
   threads on one device share a device id but distinct session ids.
8. **SSID hashing on macOS is gated by CoreLocation** since Sonoma — fragile.
   `location_hash = blake3(hostname + primary_iface_mac)` via sysinfo is a stable
   best-effort; true SSID is optional/flagged.
9. **"foo"/"bar" collapse** — set-room and join-room are the same KV op (KV has no
   join semantics). Implemented as one `SetRoom` action; `JoinRoom` is an alias
   that opens to the same prompt. Honors user intent without a fake distinction.
10. **Room persistence** — room name saved to settings so both devices reconnect
    on launch. Default room = `zed-agent-board`.
11. **Schema versioning** — every payload carries `v: 1`.

## Architecture

```
   Device A (M3)                          Device B (4090)
  ┌─────────────────────┐                ┌─────────────────────┐
  │ agent_ui / threads  │                │ agent_ui / threads  │
  │        │            │                │        │            │
  │ auto_prompt          │                │ auto_prompt          │
  │  └ plan_registry ◀──┼── feeder ──┐   │  └ plan_registry     │
  │     (in-process)    │   inject   │   │     (in-process)    │
  └────────┬────────────┘            │   └────────┬────────────┘
           │ active_claims()         │            │ active_claims()
       ┌───▼──────── agent_board ────▼────────────▼───┐
       │  panel (gpui) + SetRoom/JoinRoom/Refresh     │
       │  identity.rs (ssh-key sign, device id)       │
       │  client.rs  (http_client + KV)               │
       │  feeder     (poll → plan_registry)           │
       └─────────────────────┬────────────────────────┘
                             │ HTTPS, ed25519-signed
                   ┌─────────▼──────────┐
                   │ Cloudflare Worker  │
                   │  KV: AGENT_BOARD   │
                   │  /rooms/{room}     │
                   │  /devices (pubkeys)│
                   └────────────────────┘
```

Dependency direction: `agent_board` → `auto_prompt` (reads/writes plan_registry),
`http_client`, gpui, workspace, sysinfo. `auto_prompt` NEVER imports `agent_board`.

## Wire contract (v1)

All writes carry headers:
  `X-Device-Id: <blake3(pubkey) hex>`
  `X-Timestamp: <unix secs>`
  `X-Sig: <base64(ed25519_sign(blake3(canonical_json(body) + timestamp)))>`
  `X-Pubkey: <base64(raw 32-byte ed25519 pubkey)>`

Worker verifies sig against allowlist (KV `devices`). Canonical JSON = keys sorted,
no whitespace.

- `POST /v1/rooms/{room}/status`  body: `{v, device_name, location_hash, project_path, scopes:[{session_id, plan_file, task_summary, scope_kind}], last_active}`  → latest-wins per device
- `POST /v1/rooms/{room}/msg`     body: `{v, text}`  → append, key `room:{room}:msg:{ts}_{rand}`
- `GET  /v1/rooms/{room}`         → `{statuses:[...], messages:[last 10]}`
- `POST /v1/devices`              body: `{pubkey_b64}` self-register (first device open; after ≥1 device, only existing devices may add)

KV TTL: 1 week on all keys.

## Tasks

### Worker (JS, Cloudflare)
- [x] `agent-board-worker/src/index.js` — routes + ed25519 verify (@noble/ed25519)
- [x] `agent-board-worker/wrangler.toml` — KV namespace `AGENT_BOARD`, 7d TTL
- [x] `agent-board-worker/package.json`
- [x] `agent-board-worker/README.md` — deploy + bootstrap-device steps

### Rust crate `agent_board`
- [x] Add workspace deps: `blake3`, `ssh-key`, `ed25519-dalek`, `sha2`
- [x] `crates/agent_board/Cargo.toml`
- [x] `crates/agent_board/src/types.rs` — Status, Scope, BoardMessage, RoomSnapshot
- [x] `crates/agent_board/src/identity.rs` — load ed25519 ssh key, device id, sign, location_hash
- [x] `crates/agent_board/src/client.rs` — http_client KV client (signed)
- [x] `crates/agent_board/src/feeder.rs` — poll → plan_registry injection + post local claims
- [x] `crates/agent_board/src/agent_board.rs` — panel (Render), actions SetRoom/JoinRoom/Refresh/PostMessage, settings, init, background poll task
- [x] Register crate in root Cargo.toml (members + workspace.dependencies)
- [x] Wire `agent_board::init()` into the app (zed crate app init) + keymap actions

### Integration
- [x] Feeder injects remote claims as `try_claim(plan, "remote:{dev}:{sess}", summary)`
- [x] Feeder re-heartbeats on each poll; relies on plan_registry 300s GC for dead remotes
- [x] Panel shows: room name, active device statuses, last 10 messages

### Validation
- [x] `cargo clippy -p agent_board --all-targets` clean
- [x] `cargo test -p agent_board` — 4 tests pass (identity device-id, feeder classify/prefix)
- [x] `cargo check -p zed` — app crate compiles with agent_board wired in
- [-] `npx wrangler dev` local smoke — not run (wrangler not installed in this env; README documents it)
- [-] Manual cross-device claim visibility — deferred until worker deployed

### GOAT gate
- [x] Feature flag `agent-board` (default ON, individually disable-able)
- [-] Benchmark: feeder poll round-trip < 500ms — deferred (needs live worker)
- [-] Promote to default once cross-device claim visibility proven — stays default-on (additive, local-only fallback)
