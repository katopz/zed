# 026 — Web Threads tab (browser mirror of the agent thread panel)

## Goal
A new "Threads" tab in the worker dashboard that mirrors the Zed agent thread
panel: full timeline per session (user / assistant / tool entries as markdown),
live via SSE, with prompt/steer, Stop and Retry buttons — operable from the
phone with no local agents running.

Also settles the `.env` question:
- `worker_url` stays in `~/.config/zed/agent_board.json` (single client-side
  source of truth; the `<account>.workers.dev` subdomain is account-specific
  and cannot be derived from the project name). Short form
  (`name.account.workers.dev` without scheme) is normalized to `https://`.
- Worker env: `.dev.vars` (local) / `wrangler.toml [vars]` (deployed) —
  wrangler does not read `.env`; documented in `.dev.vars.example`.

## Substrate reused (no parallel systems)
- Timeline source: `AcpThread::entries()` + `AgentThreadEntry::to_markdown()`.
- Control: `AcpThread::cancel()` / `retry()` / `send()` — same methods the
  thread UI buttons use, reached through the existing 10s drain that already
  resolves threads by session prefix (Plan 015).
- Transport: extend the existing `AgentStateBroadcaster` trait in
  `auto_prompt::peer_states` (the established agent_ui↔agent_board bridge)
  with a default-noop `broadcast_thread_update`; `agent_board` implements it
  via the existing signed client — no new dep direction.
- Worker: new KV prefix + 2 endpoints, same ed25519 auth, same SSE relay.

## Tasks
- [x] `peer_states`: `ThreadEntry {seq, role, text}` + `broadcast_thread_update`
      (trait method, default noop) + forwarding fn + tests.
- [x] `agent_board` broadcaster impl → `POST /v1/rooms/:room/thread`
      (batched entries, 4KB/entry cap, device+session key).
- [x] `agent_board::client`: `post_thread` + short-form `worker_url`
      normalization (no scheme ⇒ `https://`).
- [x] `agent_ui` drain: per-session tail fingerprint (len + hash of last
      entry) sends the last 3 entries on change (streaming-safe upsert);
      roles user/assistant/tool, skips `AgentBoardNotification` echo;
      `!stop` / `!retry` reply commands → `cancel().detach()` / `retry().await`.
- [x] Worker: `handlePostThread` (upsert by seq, cap 100, TTL) +
      `GET /v1/rooms/:room/threads` + SSE relay `{type:"thread", ...doc}`
      (routed BEFORE the state branch — thread docs also carry session_id).
- [x] Web UI: Board | Threads tabs; session list + chat timeline (role
      bubbles) + Send/Stop/Retry bar; SSE live-append; 15s poll fallback;
      read-only mode disables the action bar.
- [x] Tests: peer_states forwarding + default-noop; drain/`!stop`/`!retry`
      exercised via existing agent_ui suites compile-green (behavioral test
      pending live worker); URL normalization covered by client tests.
- [x] clippy + tests green; `script/bundle-mac`; commit + push.

## Validation
- `./script/clippy -p agent_board -p auto_prompt -p agent_ui`
- `cargo test -p agent_board -p auto_prompt`
- `node --check agent-board-worker/src/index.js`
- Manual: open dashboard → Threads tab → see live entries; Stop/Retry from
  phone affects the running thread.

## Deploy + smoke test (2026-08-24)
- [x] `npx wrangler deploy` → version `dfaf970c-2eed-4dd3-bd4c-4395e3608edd`
      on `agent-board-worker.foxfox.workers.dev`.
- [x] Routes live: `GET /v1/rooms/:room/threads` → 200 (empty then populated);
      `POST /v1/rooms/:room/thread` → 401 unsigned / 200 signed; SSE
      `/v1/rooms/:room/events` → `: connected` + keepalives.
- [x] E2E signed posts (real `~/.ssh/id_ed25519`, same `body|ts` ed25519
      scheme as the Rust client): initial 2 entries → 200; streaming growth
      (re-send seq 2 longer + seq 3) → 3 entries, seq 2 REPLACED (no dupes);
      `GET /threads` lists the doc; zeroed sig → 401 `bad signature`.
- [x] SSE relay: live `data: {"type":"thread",...}` frame observed during a
      signed POST.
- [x] Dashboard HTML: `tab-board`/`tab-threads` buttons, `#tsessions`, thread
      JS (`upsertThread`/`threadLabel`) all present.
- [x] New bundle (Aug 24 build) launched side-by-side via
      `--user-data-dir /tmp/zed-smoke-data`: process stable, ESTABLISHED
      Cloudflare connections (SSE + poll), room heartbeat fresh (age ~2s).
- [-] In-app drain E2E + browser write path (Send/`!stop`/`!retry`) —
      owner-gated: needs a human-typed agent prompt in the new build + GitHub
      sign-in in the browser. Smoke thread `smoke-e2e` (4 entries) left in KV
      as a live demo artifact; expires via 7d TTL.
- Note: Cloudflare bot mode 403s `Python-urllib` UAs on this domain — use a
  custom `User-Agent` when scripting against the worker.
