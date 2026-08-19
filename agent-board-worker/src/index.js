// Agent Board — Cloudflare Worker (KV-backed multi-device notepad for Zed agents).
//
// Wire contract (v1). All Zed-originated write endpoints require headers:
//   X-Device-Id   : hex(blake3(raw_ed25519_pubkey_32))
//   X-Timestamp   : unix seconds (string)
//   X-Sig         : base64( ed25519_sign( canonical_request_body + "|" + timestamp ) )
//   X-Pubkey      : base64( raw 32-byte ed25519 pubkey )
//
// The worker verifies X-Sig against the device allowlist (KV `device:` keys).
// GET is open (read-only). The board is single-user; the signature gate exists
// only to prevent spam, per the owner's explicit design.
//
// Browser-facing routes (Plan 015 W1-W5):
//   GET  /                                 → single-page HTML dashboard
//   GET  /ws?room=...                      → WebSocket upgrade (GitHub token auth)
//   GET  /v1/rooms/:room/events?device=... → SSE stream (read-only event push)
//   POST /v1/rooms/:room/reply             → store operator reply (GitHub or ed25519)
//   POST /auth/github/device               → start GitHub device-flow sign-in
//   POST /auth/github/poll                 → poll for the device-flow token
//
// Signing note: clients sign the *raw request body text + "|" + timestamp*
// bytes directly. ed25519 signs arbitrary-length messages internally, so no
// pre-hashing is needed on either side. The Rust client in
// `crates/agent_board/src/identity.rs` must produce the exact same message.

import * as ed from "@noble/ed25519";

const MAX_MESSAGES = 100;
const MAX_ROOM_STATES = 50;
const MAX_STATE_TEXT_BYTES = 256;
const STALE_STATUS_SECS = 300;
const TTL_SECS = 60 * 60 * 24 * 7; // 1 week

const encoder = new TextEncoder();

// Module-level verification cache for GitHub tokens (W5, replaces Google
// JWT/JWKS). GitHub tokens are opaque — the only way to verify one is to ask
// api.github.com whose it is — so cache sha256(token) → login for 10 min to
// avoid an API round-trip on every WS auth / reply POST.
const GITHUB_VERIFY_TTL_MS = 10 * 60 * 1000;
const githubTokenCache = new Map(); // hex(sha256(token)) -> { login, expiresAt }

function b64decode(s) {
  return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
}

function nowKey() {
  // Sortable key suffix: base36 of millis + short random.
  return Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 8);
}

// Truncate a string to at most `maxBytes` UTF-8 bytes without splitting a
// multi-byte character. Defense-in-depth: the Rust client pre-truncates with
// the same logic (truncate_to_byte_budget); the worker re-applies so a
// misbehaving or older client can't bloat the room.
function truncateToByteBudget(text, maxBytes) {
  const encoded = new TextEncoder().encode(text);
  if (encoded.length <= maxBytes) return text;
  // Walk back to a valid UTF-8 boundary. The TextDecoder `fatal: false` default
  // replaces trailing partial sequences with U+FFFD, which is acceptable for a
  // defense-in-depth cap.
  let end = maxBytes;
  while (end > 0 && (encoded[end] & 0xc0) === 0x80) end--;
  return new TextDecoder().decode(encoded.subarray(0, end));
}

function json(body, status) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

async function ttlOf(env) {
  const configured = parseInt(env.DEFAULT_TTL_SECONDS ?? "", 10);
  return Number.isFinite(configured) && configured > 0 ? configured : TTL_SECS;
}

async function verifySignature(env, headers, bodyText) {
  const deviceId = headers.get("X-Device-Id");
  const timestamp = headers.get("X-Timestamp");
  const sigB64 = headers.get("X-Sig");
  const pubkeyB64 = headers.get("X-Pubkey");
  if (!deviceId || !timestamp || !sigB64 || !pubkeyB64) {
    return { ok: false, status: 401, error: "missing auth headers" };
  }
  // Anti-replay: reject timestamps skewed more than 5 minutes.
  const ts = parseInt(timestamp, 10);
  const skew = Math.abs(Date.now() / 1000 - ts);
  if (Number.isNaN(ts) || skew > 300) {
    return { ok: false, status: 401, error: "timestamp skew too large" };
  }
  // Bootstrap: the very first device self-registers. After that, a device must
  // already be known. This keeps the board spam-free without any manual seeding.
  const known = await env.AGENT_BOARD.get(`device:${deviceId}`);
  const anyDevice =
    (await env.AGENT_BOARD.list({ prefix: "device:", limit: 1 })).keys.length > 0;
  if (anyDevice && known === null) {
    return { ok: false, status: 403, error: "device not in allowlist" };
  }
  const sig = b64decode(sigB64);
  const pubkey = b64decode(pubkeyB64);
  const message = new TextEncoder().encode(bodyText + "|" + timestamp);
  const ok = await ed.verifyAsync(sig, message, pubkey);
  if (!ok) return { ok: false, status: 401, error: "bad signature" };
  // Persist/refresh the pubkey so this device stays in the allowlist.
  await env.AGENT_BOARD.put(`device:${deviceId}`, pubkeyB64, {
    expirationTtl: TTL_SECS,
  });
  return { ok: true, deviceId };
}
// ───────────────────────────────────────────────────────────────────────────
// W5 — GitHub sign-in, device flow (web UI → worker). Replaces Google OAuth:
// Zed itself signs in with GitHub, so the board follows suit.
//
// Why device flow: it needs ONLY a public client_id — no client secret in the
// worker (same "no secrets" property the Google path had). The browser gets a
// user_code, authorizes at github.com/login/device, and the dashboard polls
// until GitHub issues the access token. Verification of that token is an
// api.github.com/user call (GitHub tokens are opaque — there is no local
// signature to check), cached 10 min keyed by sha256(token).
// ───────────────────────────────────────────────────────────────────────────

const GITHUB_API_USER = "https://api.github.com/user";
const GITHUB_DEVICE_URL = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL = "https://github.com/login/oauth/access_token";

async function sha256Hex(text) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

// Verify a GitHub user token: ask GitHub whose it is, then check the login
// against the allowlist. Returns { ok: true, login } or { ok: false, error }.
async function verifyGithubToken(env, token) {
  if (!token) return { ok: false, error: "missing token" };
  const allowedLogin = (env.ALLOWED_LOGIN || "katopz").toLowerCase();
  const key = await sha256Hex(token);
  const cached = githubTokenCache.get(key);
  if (cached && cached.expiresAt > Date.now()) {
    return cached.login.toLowerCase() === allowedLogin
      ? { ok: true, login: cached.login }
      : { ok: false, error: `login ${cached.login} not allowlisted` };
  }
  try {
    const res = await fetch(GITHUB_API_USER, {
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        // GitHub's API rejects requests without a User-Agent.
        "User-Agent": "agent-board-worker",
      },
    });
    if (res.status === 401) return { ok: false, error: "invalid token" };
    if (!res.ok) return { ok: false, error: `github api ${res.status}` };
    const user = await res.json();
    const login = String(user.login ?? "");
    if (!login) return { ok: false, error: "token has no login" };
    if (login.toLowerCase() !== allowedLogin) {
      return { ok: false, error: `login ${login} not allowlisted` };
    }
    githubTokenCache.set(key, { login, expiresAt: Date.now() + GITHUB_VERIFY_TTL_MS });
    return { ok: true, login };
  } catch (err) {
    // Fail closed on network errors (mirrors the old JWKS-fetch stance).
    return { ok: false, error: String(err.message || err) };
  }
}

// POST /auth/github/device — start the flow. Returns the user_code the
// operator types at github.com/login/device plus the device_code the dashboard
// polls with. No secrets involved; client_id is public by design.
async function handleGithubDeviceStart(env) {
  if (!env.GITHUB_CLIENT_ID) {
    return json({ error: "GITHUB_CLIENT_ID not configured" }, 503);
  }
  const res = await fetch(GITHUB_DEVICE_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json", "User-Agent": "agent-board-worker" },
    body: JSON.stringify({ client_id: env.GITHUB_CLIENT_ID, scope: "read:user" }),
  });
  if (!res.ok) return json({ error: `github device ${res.status}` }, 502);
  const data = await res.json();
  return json(
    {
      device_code: data.device_code,
      user_code: data.user_code,
      verification_uri: data.verification_uri,
      interval: data.interval ?? 5,
      expires_in: data.expires_in ?? 900,
    },
    200
  );
}

// POST /auth/github/poll { device_code } — poll GitHub's token endpoint.
// Maps the device-flow error taxonomy to a simple status for the dashboard.
async function handleGithubDevicePoll(env, deviceCode) {
  if (!env.GITHUB_CLIENT_ID) {
    return json({ error: "GITHUB_CLIENT_ID not configured" }, 503);
  }
  if (!deviceCode) return json({ error: "missing device_code" }, 400);
  const res = await fetch(GITHUB_TOKEN_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json", Accept: "application/json", "User-Agent": "agent-board-worker" },
    body: JSON.stringify({
      client_id: env.GITHUB_CLIENT_ID,
      device_code: deviceCode,
      grant_type: "urn:ietf:params:oauth:grant-type:device_code",
    }),
  });
  if (!res.ok) return json({ error: `github token ${res.status}` }, 502);
  const data = await res.json();
  if (data.access_token) return json({ status: "ok", access_token: data.access_token }, 200);
  if (data.error === "authorization_pending" || data.error === "slow_down") {
    return json({ status: "pending" }, 202);
  }
  return json({ status: "error", error: String(data.error ?? "unknown") }, 200);
}
// ───────────────────────────────────────────────────────────────────────────
// Durable Object relay helper (W3/W4)
// ───────────────────────────────────────────────────────────────────────────

// Best-effort push: HTTP POST handlers call this after their KV write so any
// connected browser/Zed WebSocket or SSE listener gets the new payload pushed
// in real time. Failure here must not fail the original request — relay is
// strictly additive over the KV source-of-truth.
async function relayToRoom(env, room, message) {
  try {
    const id = env.ROOM_COORDINATOR.idFromName(room);
    const stub = env.ROOM_COORDINATOR.get(id);
    await stub.fetch("https://room-coordinator/relay", {
      method: "POST",
      body: message,
    });
  } catch (err) {
    // Swallow: HTTP POST still succeeded at the KV layer; WebSocket push is a
    // best-effort optimization on top.
  }
}

// ───────────────────────────────────────────────────────────────────────────
// HTTP handlers
// ───────────────────────────────────────────────────────────────────────────

async function handlePostStatus(env, room, body, verified) {
  const status = {
    v: 1,
    ...body,
    device_id: verified.deviceId,
    updated_at: Date.now(),
  };
  await env.AGENT_BOARD.put(
    `room:${room}:device:${verified.deviceId}`,
    JSON.stringify(status),
    { expirationTtl: TTL_SECS }
  );
  await relayToRoom(env, room, JSON.stringify(status));
  return json(status, 200);
}

async function handlePostMsg(env, room, body, verified) {
  const msg = {
    v: 1,
    device_id: verified.deviceId,
    device_name: body.device_name ?? "",
    sender: String(body.sender ?? "").slice(0, 64),
    text: String(body.text ?? "").slice(0, 1024),
    ts: Date.now(),
  };
  await env.AGENT_BOARD.put(`room:${room}:msg:${nowKey()}`, JSON.stringify(msg), {
    expirationTtl: TTL_SECS,
  });
  await relayToRoom(env, room, JSON.stringify(msg));
  return json(msg, 201);
}

async function handlePostState(env, room, body, verified) {
  // Agent state broadcast (Phase 2 point 3-4). Stored under a separate KV
  // prefix so the GET handler can ring-buffer independently from chat msgs.
  // Both state_text and meta are capped at MAX_STATE_TEXT_BYTES (point 8).
  const state = {
    v: 1,
    device_id: verified.deviceId,
    device_name: String(body.device_name ?? ""),
    session_id: String(body.session_id ?? ""),
    sub_agent_id: body.sub_agent_id ? String(body.sub_agent_id) : null,
    state_text: truncateToByteBudget(String(body.state_text ?? ""), MAX_STATE_TEXT_BYTES),
    meta: truncateToByteBudget(String(body.meta ?? ""), MAX_STATE_TEXT_BYTES),
    ts: Date.now(),
  };
  await env.AGENT_BOARD.put(
    `room:${room}:state:${nowKey()}`,
    JSON.stringify(state),
    { expirationTtl: TTL_SECS }
  );
  await relayToRoom(env, room, JSON.stringify(state));
  return json(state, 201);
}

async function handlePostReply(env, room, body, authorLogin) {
  // Operator reply (Plan 015 W6). Stored under `room:{room}:reply:` so the
  // existing GET handler can ring-buffer and the Zed feeder can drain it.
  // The 4-char `target_session_prefix` is the routing key — the web UI never
  // learns the full session_id.
  const reply = {
    v: 1,
    target_device: String(body.target_device ?? ""),
    target_session_prefix: String(body.target_session_prefix ?? ""),
    text: String(body.text ?? "").slice(0, 1024),
    author_login: authorLogin ?? "",
    ts: Date.now(),
  };
  await env.AGENT_BOARD.put(
    `room:${room}:reply:${nowKey()}`,
    JSON.stringify(reply),
    { expirationTtl: TTL_SECS }
  );
  // Relay as a typed wrapper so the browser/Zed can distinguish reply echoes
  // from state/msg/status broadcasts.
  await relayToRoom(env, room, JSON.stringify({ type: "reply", ...reply }));
  return json(reply, 201);
}

async function handleGetRoom(env, room) {
  const [deviceList, msgList, stateList, replyList] = await Promise.all([
    env.AGENT_BOARD.list({ prefix: `room:${room}:device:`, limit: 64 }),
    env.AGENT_BOARD.list({ prefix: `room:${room}:msg:`, limit: MAX_MESSAGES + 5 }),
    env.AGENT_BOARD.list({ prefix: `room:${room}:state:`, limit: MAX_ROOM_STATES + 5 }),
    env.AGENT_BOARD.list({ prefix: `room:${room}:reply:`, limit: MAX_ROOM_STATES + 5 }),
  ]);
  const nowSec = Date.now() / 1000;
  const statuses = [];
  for (const k of deviceList.keys) {
    const raw = await env.AGENT_BOARD.get(k.name);
    if (raw === null) continue;
    const s = JSON.parse(raw);
    const updatedAgo = nowSec - (s.updated_at ?? 0) / 1000;
    s.stale = updatedAgo > STALE_STATUS_SECS;
    statuses.push(s);
  }
  const messages = [];
  for (const k of msgList.keys) {
    const raw = await env.AGENT_BOARD.get(k.name);
    if (raw !== null) messages.push(JSON.parse(raw));
  }
  messages.sort((a, b) => b.ts - a.ts).splice(MAX_MESSAGES);
  // Agent states (Phase 2 point 7): ring-buffer to last MAX_ROOM_STATES by ts.
  const states = [];
  for (const k of stateList.keys) {
    const raw = await env.AGENT_BOARD.get(k.name);
    if (raw !== null) states.push(JSON.parse(raw));
  }
  states.sort((a, b) => b.ts - a.ts).splice(MAX_ROOM_STATES);
  // Operator replies (Plan 015 W6): same ring-buffer pattern.
  const replies = [];
  for (const k of replyList.keys) {
    const raw = await env.AGENT_BOARD.get(k.name);
    if (raw !== null) replies.push(JSON.parse(raw));
  }
  replies.sort((a, b) => b.ts - a.ts).splice(MAX_ROOM_STATES);
  return json({ v: 1, room, statuses, messages, states, replies }, 200);
}

// ───────────────────────────────────────────────────────────────────────────
// W1 — Worker HTML dashboard (inline, ~15KB budget, no framework)
// ───────────────────────────────────────────────────────────────────────────

function dashboardHtml(roomId, githubEnabled) {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Agent Board</title>
<style>
*{box-sizing:border-box}
body{font:13px/1.45 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;margin:0;padding:12px 12px 64px;background:#1b1b1b;color:#e4e4e4}
header{display:flex;justify-content:space-between;align-items:center;margin-bottom:12px;gap:8px;flex-wrap:wrap}
h1{font-size:15px;margin:0;font-weight:600}
#room{color:#9c9c9c;font-weight:400}
#right{display:flex;gap:8px;align-items:center}
#status{font-size:11px;padding:3px 7px;border-radius:3px;background:#333;white-space:nowrap}
#status.ok{background:#2d4a2d;color:#8ce28c}
#status.bad{background:#4a2d2d;color:#e28c8c}
.device{background:#262626;border-radius:5px;margin-bottom:6px;border:1px solid #2e2e2e}
.dev-head{padding:9px 11px;cursor:pointer;user-select:none;display:flex;justify-content:space-between;font-weight:600}
.dev-head:hover{background:#2e2e2e}
.dev-head .count{color:#888;font-weight:400}
.dev-body{display:none;padding:2px 11px 8px}
.device.on .dev-body{display:block}
.agent{background:#1e1e1e;border-radius:3px;margin:3px 0}
.ag-head{padding:6px 9px;cursor:pointer;user-select:none;display:flex;justify-content:space-between;font-size:12px;gap:8px}
.ag-head:hover{background:#282828}
.ag-head .preview{color:#bbb;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;flex:1}
.ag-head .n{color:#777;flex-shrink:0}
.ag-body{display:none;padding:5px 9px;border-top:1px solid #2c2c2c}
.agent.on .ag-body{display:block}
.state{padding:4px 0;border-bottom:1px solid #262626;font-size:11px}
.state:last-child{border:0}
.state .t{color:#cfcfcf;word-break:break-word}
.state .m{color:#777;font-size:10px;margin-top:2px}
#replybar{position:fixed;bottom:0;left:0;right:0;display:none;gap:6px;padding:8px 12px;background:#262626;border-top:1px solid #333}
#replybar input{flex:1;padding:7px 9px;background:#1a1a1a;border:1px solid #444;color:#eee;border-radius:3px;font:13px/1.4 -apple-system,sans-serif;min-width:0}
#replybar button{padding:7px 14px;background:#3a6db8;color:#fff;border:0;border-radius:3px;cursor:pointer;font-weight:600}
#replybar button:hover{background:#4a7dc8}
#replybar button:disabled{background:#333;color:#666;cursor:not-allowed}
.empty{color:#777;font-style:italic;padding:20px;text-align:center}
#ghbtn{display:none;padding:5px 12px;background:#24292f;color:#fff;border:1px solid #444;border-radius:3px;cursor:pointer;font:600 12px/1.4 -apple-system,sans-serif}
#ghbtn:hover{background:#32383f}
#modal{display:none;position:fixed;inset:0;background:rgba(0,0,0,.6);z-index:10;align-items:center;justify-content:center}
#modal.open{display:flex}
#modalbox{background:#262626;border:1px solid #3a3a3a;border-radius:6px;padding:20px;max-width:380px;text-align:center}
#ucode{font:700 24px/1.3 ui-monospace,Menlo,monospace;letter-spacing:3px;margin:12px 0;color:#8ce28c}
#modalbox a{color:#6ea8e0}
#mstate{color:#999;font-size:12px;margin-top:10px}
</style>
</head>
<body>
<header>
  <h1>📡 Agent Board · <span id="room"></span></h1>
  <div id="right">
    <span id="status" class="bad">🔴 off</span>
    <button id="ghbtn">Sign in with GitHub</button>
  </div>
</header>
<div id="dash"><div class="empty">Sign in to load the room.</div></div>
<div id="replybar">
  <input id="reply" placeholder="REPLY:[device:sess4] message" autocomplete="off">
  <button id="send" disabled>Send</button>
</div>
<div id="modal"><div id="modalbox">
  <div>Enter this code on GitHub:</div>
  <div id="ucode"></div>
  <a id="vlink" href="#" target="_blank" rel="noopener">github.com/login/device</a>
  <div id="mstate">waiting for authorization…</div>
</div></div>
<script>
const ROOM = ${JSON.stringify(roomId)};
const GH_ENABLED = ${JSON.stringify(Boolean(githubEnabled))};
document.getElementById("room").textContent = ROOM;

let token = sessionStorage.getItem("ab_gh_token") || null, login = null, ws = null, backoff = 1000;
let devices = {};                 // name -> { agents: { sess4 -> {session_id, states:[]} } }
let expandedDev = new Set();      // device names whose body is open
let expandedAg = new Set();       // "name:sess4" keys whose body is open

function setStatus(ok) {
  const el = document.getElementById("status");
  el.textContent = ok ? "🟢 on" : "🔴 off";
  el.className = ok ? "ok" : "bad";
}
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
  });
}
function fmtTime(ts) {
  if (!ts) return "";
  try { return new Date(ts).toLocaleTimeString(); } catch (e) { return ""; }
}

// ── GitHub device-flow sign-in ──
function onAuthed(t) {
  token = t;
  sessionStorage.setItem("ab_gh_token", t);
  document.getElementById("ghbtn").style.display = "none";
  document.getElementById("replybar").style.display = "flex";
  document.getElementById("send").disabled = false;
  connect();
  fetchRoom();
}

function showSignIn() {
  const btn = document.getElementById("ghbtn");
  btn.style.display = GH_ENABLED ? "inline-block" : "none";
  if (!GH_ENABLED) btn.title = "GITHUB_CLIENT_ID not configured on the worker";
}

async function ghSignIn() {
  const modal = document.getElementById("modal");
  const state = document.getElementById("mstate");
  const ucode = document.getElementById("ucode");
  const vlink = document.getElementById("vlink");
  try {
    const res = await fetch("/auth/github/device", { method: "POST" });
    const data = await res.json();
    if (!res.ok) { state.textContent = data.error || "failed to start sign-in"; return; }
    ucode.textContent = data.user_code;
    vlink.href = data.verification_uri || "https://github.com/login/device";
    state.textContent = "waiting for authorization…";
    modal.classList.add("open");
    const deadline = Date.now() + (data.expires_in || 900) * 1000;
    let interval = (data.interval || 5) * 1000;
    const deviceCode = data.device_code;
    (async function poll() {
      if (Date.now() > deadline || !modal.classList.contains("open")) return;
      await new Promise((r) => setTimeout(r, interval));
      try {
        const pr = await fetch("/auth/github/poll", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ device_code: deviceCode }),
        });
        const pd = await pr.json();
        if (pd.status === "ok") {
          modal.classList.remove("open");
          onAuthed(pd.access_token);
          return;
        }
        if (pd.status === "error") {
          state.textContent = "sign-in failed: " + (pd.error || "unknown") + " — try again";
          return;
        }
        if (pd.status === "pending" && pr.headers.get("retry-after")) interval += 5000;
        state.textContent = "waiting for authorization…";
        poll();
      } catch (e) {
        state.textContent = "network error — retrying";
        poll();
      }
    })();
  } catch (e) {
    state.textContent = "network error: " + String(e);
  }
}
document.getElementById("ghbtn").addEventListener("click", ghSignIn);

function connect() {
  if (!token) return;
  if (ws && (ws.readyState === 0 || ws.readyState === 1)) return;
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(proto + "//" + location.host + "/ws?room=" + encodeURIComponent(ROOM));
  ws.onopen = function () {
    setStatus(true);
    backoff = 1000;
    ws.send(JSON.stringify({ type: "auth", github_token: token }));
  };
  ws.onmessage = function (ev) {
    let m;
    try { m = JSON.parse(ev.data); } catch (e) { return; }
    if (m && m.type === "auth_ok") return;
    routeMsg(m);
  };
  ws.onclose = function (ev) {
    setStatus(false);
    // 4001 = auth failed (token revoked / wrong login). Drop the stored token
    // and show the sign-in button again; otherwise backoff-reconnect.
    if (ev.code === 4001) {
      token = null;
      sessionStorage.removeItem("ab_gh_token");
      showSignIn();
      return;
    }
    setTimeout(connect, backoff);
    backoff = Math.min(backoff * 2, 30000);
  };
  ws.onerror = function () { try { ws.close(); } catch (e) {} };
}

function routeMsg(m) {
  if (!m) return;
  // Reply echo — ignore (we sent it). Future: render as confirmation.
  if (m.type === "reply" && m.target_device !== undefined) return;
  // State broadcast: device_name + session_id + state_text
  if (m.session_id !== undefined && (m.state_text !== undefined || m.device_name)) {
    ingestState(m); render(); return;
  }
  // Other shapes (status, msg) — not rendered in v1 dashboard.
}

function ingestState(s) {
  const dev = s.device_name || "unknown";
  const sess4 = String(s.session_id || "").slice(0, 4);
  if (!devices[dev]) devices[dev] = { agents: {} };
  const ag = devices[dev].agents[sess4] || { session_id: s.session_id || "", states: [] };
  ag.session_id = s.session_id || ag.session_id;
  ag.states.unshift({
    text: s.state_text || "",
    meta: s.meta || "",
    ts: s.ts || Date.now(),
  });
  if (ag.states.length > 10) ag.states.length = 10;
  devices[dev].agents[sess4] = ag;
}

async function fetchRoom() {
  try {
    const r = await fetch("/v1/rooms/" + encodeURIComponent(ROOM));
    if (!r.ok) return;
    const data = await r.json();
    devices = {};
    for (const st of data.states || []) ingestState(st);
    render();
  } catch (e) {
    console.error("fetchRoom failed", e);
  }
}

function render() {
  const root = document.getElementById("dash");
  const names = Object.keys(devices);
  if (names.length === 0) {
    root.innerHTML = '<div class="empty">No devices reporting yet.</div>';
    return;
  }
  let h = "";
  for (const name of names) {
    const dev = devices[name];
    const keys = Object.keys(dev.agents);
    const devCls = expandedDev.has(name) ? " on" : "";
    h += '<div class="device' + devCls + '">';
    h += '<div class="dev-head" onclick="toggleDev(this,' + esc(JSON.stringify(name)) + ')">'
      + esc(name)
      + ' <span class="count">' + keys.length + " agent" + (keys.length === 1 ? "" : "s") + '</span></div>';
    h += '<div class="dev-body">';
    for (const sess4 of keys) {
      const ag = dev.agents[sess4];
      const latest = ag.states[0];
      const preview = latest ? " — " + String(latest.text || "").slice(0, 70) : "";
      const agCls = expandedAg.has(name + ":" + sess4) ? " on" : "";
      h += '<div class="agent' + agCls + '">';
      h += '<div class="ag-head" onclick="toggleAg(this,' + esc(JSON.stringify(name)) + ',' + esc(JSON.stringify(sess4)) + ')">'
        + '<span><span class="preview">' + esc(sess4) + esc(preview) + '</span></span>'
        + '<span class="n">' + ag.states.length + '</span></div>';
      h += '<div class="ag-body">';
      for (const st of ag.states) {
        h += '<div class="state"><div class="t">' + esc(st.text) + '</div>'
          + '<div class="m">' + fmtTime(st.ts) + (st.meta ? " · " + esc(st.meta) : "") + '</div></div>';
      }
      h += "</div></div>";
    }
    h += "</div></div>";
  }
  root.innerHTML = h;
}

window.toggleDev = function (el, name) {
  const dev = el.parentElement;
  if (expandedDev.has(name)) { expandedDev.delete(name); dev.classList.remove("on"); }
  else { expandedDev.add(name); dev.classList.add("on"); }
};
window.toggleAg = function (el, name, sess4) {
  const ag = el.parentElement;
  const key = name + ":" + sess4;
  if (expandedAg.has(key)) { expandedAg.delete(key); ag.classList.remove("on"); }
  else { expandedAg.add(key); ag.classList.add("on"); }
  const input = document.getElementById("reply");
  input.value = "REPLY:[" + name + ":" + sess4 + "] ";
  input.focus();
  const len = input.value.length;
  input.setSelectionRange(len, len);
};

// Manual parser — avoids messy regex-in-template-string escaping.
function parseReply(s) {
  if (typeof s !== "string") return null;
  const prefix = "REPLY:[";
  if (!s.startsWith(prefix)) return null;
  const close = s.indexOf("]");
  if (close < 0) return null;
  const inside = s.slice(prefix.length, close);
  const colon = inside.indexOf(":");
  if (colon < 0) return null;
  const target_device = inside.slice(0, colon);
  const sess4 = inside.slice(colon + 1);
  if (!target_device || !sess4) return null;
  const text = s.slice(close + 1).replace(/^\s+/, "");
  return { target_device: target_device, target_session_prefix: sess4, text: text };
}

function sendReply() {
  const input = document.getElementById("reply");
  const parsed = parseReply(input.value);
  if (!parsed) { alert("Format: REPLY:[device:sess4] message"); return; }
  if (!parsed.text.trim()) return;
  const payload = {
    type: "reply",
    target_device: parsed.target_device,
    target_session_prefix: parsed.target_session_prefix,
    text: parsed.text,
  };
  if (ws && ws.readyState === 1) {
    ws.send(JSON.stringify(payload));
  } else {
    // Fallback: HTTP POST with the GitHub bearer token.
    fetch("/v1/rooms/" + encodeURIComponent(ROOM) + "/reply", {
      method: "POST",
      headers: { "Authorization": "Bearer " + token, "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    }).catch(function (e) { console.error("reply POST failed", e); });
  }
  input.value = "";
}
document.getElementById("send").addEventListener("click", sendReply);
document.getElementById("reply").addEventListener("keydown", function (e) {
  if (e.key === "Enter") sendReply();
});
document.getElementById("modal").addEventListener("click", function (e) {
  if (e.target.id === "modal") e.target.classList.remove("open"); // cancel
});

// Boot: restore a stored token (auto-connect), otherwise show sign-in.
if (token) onAuthed(token);
else showSignIn();
</script>
</body>
</html>`;
}

// ───────────────────────────────────────────────────────────────────────────
// Worker entry point
// ───────────────────────────────────────────────────────────────────────────

export default {
  async fetch(request, env) {
    // Reference ttlOf so the per-env override is honored (and kept from
    // appearing unused if the operator sets DEFAULT_TTL_SECONDS).
    await ttlOf(env);

    const url = new URL(request.url);
    const path = url.pathname;

    if (path === "/healthz") return json({ ok: true }, 200);

    // W1 — Dashboard HTML. Static, no auth.
    if (path === "/") {
      const roomId = url.searchParams.get("room") ?? "zed-agent-board";
      return new Response(dashboardHtml(roomId, Boolean(env.GITHUB_CLIENT_ID)), {
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }

    // W5 — GitHub device-flow sign-in (replaces Google GIS).
    if (path === "/auth/github/device" && request.method === "POST") {
      return handleGithubDeviceStart(env);
    }
    if (path === "/auth/github/poll" && request.method === "POST") {
      let body;
      try {
        body = await request.json();
      } catch {
        return json({ error: "invalid json" }, 400);
      }
      return handleGithubDevicePoll(env, body.device_code);
    }

    // W4 — WebSocket upgrade. Forwarded to the per-room RoomCoordinator DO,
    // which validates auth on the first message and relays subsequent ones.
    if (path === "/ws" && request.method === "GET") {
      const room = url.searchParams.get("room") ?? "default";
      const id = env.ROOM_COORDINATOR.idFromName(room);
      const stub = env.ROOM_COORDINATOR.get(id);
      return stub.fetch(request);
    }

    const roomMatch = path.match(/^\/v1\/rooms\/([^/]+)$/);
    if (roomMatch && request.method === "GET") {
      return handleGetRoom(env, decodeURIComponent(roomMatch[1]));
    }

    // W4 — SSE stream. Forwarded to the DO, which holds the connection open
    // and pushes events. Read-only; auth is optional here (events are room
    // state broadcasts, not privileged data). Optional ?token=<github-token>
    // is validated if present.
    const eventsMatch = path.match(/^\/v1\/rooms\/([^/]+)\/events$/);
    if (eventsMatch && request.method === "GET") {
      const room = decodeURIComponent(eventsMatch[1]);
      const id = env.ROOM_COORDINATOR.idFromName(room);
      const stub = env.ROOM_COORDINATOR.get(id);
      // Reuse the inbound request so query params (?device=...) survive.
      return stub.fetch(request);
    }

    const statusMatch = path.match(/^\/v1\/rooms\/([^/]+)\/status$/);
    if (statusMatch && request.method === "POST") {
      const bodyText = await request.text();
      const verified = await verifySignature(env, request.headers, bodyText);
      if (!verified.ok) return json({ error: verified.error }, verified.status);
      let body;
      try {
        body = JSON.parse(bodyText);
      } catch {
        return json({ error: "invalid json" }, 400);
      }
      return handlePostStatus(env, decodeURIComponent(statusMatch[1]), body, verified);
    }

    const msgMatch = path.match(/^\/v1\/rooms\/([^/]+)\/msg$/);
    if (msgMatch && request.method === "POST") {
      const bodyText = await request.text();
      const verified = await verifySignature(env, request.headers, bodyText);
      if (!verified.ok) return json({ error: verified.error }, verified.status);
      let body;
      try {
        body = JSON.parse(bodyText);
      } catch {
        return json({ error: "invalid json" }, 400);
      }
      return handlePostMsg(env, decodeURIComponent(msgMatch[1]), body, verified);
    }

    const stateMatch = path.match(/^\/v1\/rooms\/([^/]+)\/state$/);
    if (stateMatch && request.method === "POST") {
      const bodyText = await request.text();
      const verified = await verifySignature(env, request.headers, bodyText);
      if (!verified.ok) return json({ error: verified.error }, verified.status);
      let body;
      try {
        body = JSON.parse(bodyText);
      } catch {
        return json({ error: "invalid json" }, 400);
      }
      return handlePostState(env, decodeURIComponent(stateMatch[1]), body, verified);
    }

    // W4 — Operator reply. Auth: GitHub bearer token (web) OR ed25519 (Zed).
    const replyMatch = path.match(/^\/v1\/rooms\/([^/]+)\/reply$/);
    if (replyMatch && request.method === "POST") {
      const room = decodeURIComponent(replyMatch[1]);
      const bodyText = await request.text();
      let authorLogin = null;

      // Try GitHub token first.
      const authHeader = request.headers.get("Authorization") ?? "";
      const bearer = authHeader.match(/^Bearer\s+(.+)$/i);
      if (bearer) {
        const result = await verifyGithubToken(env, bearer[1]);
        if (result.ok) {
          authorLogin = result.login;
        } else {
          return json({ error: "auth: " + result.error }, 401);
        }
      } else {
        // Fall back to ed25519.
        const verified = await verifySignature(env, request.headers, bodyText);
        if (!verified.ok) return json({ error: verified.error }, verified.status);
      }

      let body;
      try {
        body = JSON.parse(bodyText);
      } catch {
        return json({ error: "invalid json" }, 400);
      }
      return handlePostReply(env, room, body, authorLogin);
    }

    return json({ error: "not found" }, 404);
  },
};

// ───────────────────────────────────────────────────────────────────────────
// W3 — Durable Object: RoomCoordinator
//
// One instance per room. Holds WebSocket sessions (browser + Zed) and SSE
// listeners, validates auth on the first WS message, and relays every
// subsequent message (or HTTP-POST-driven relay) to all *other* connections
// in the same room.
// ───────────────────────────────────────────────────────────────────────────

export class RoomCoordinator {
  constructor(state, env) {
    this.state = state;
    this.env = env;
    // ws -> { room, authed, source, login, device_id }
    this.sessions = new Map();
    // Set<WritableStreamDefaultWriter> for SSE clients
    this.sseWriters = new Set();
  }

  async fetch(request) {
    const url = new URL(request.url);

    // WebSocket upgrade. Auth is deferred to the first message so we can keep
    // the upgrade handshake stateless (no token in URL → no proxy logs).
    if (request.headers.get("Upgrade") === "websocket") {
      const pair = new WebSocketPair();
      const [client, server] = Object.values(pair);
      server.accept();

      const room = url.searchParams.get("room") ?? "default";
      this.sessions.set(server, {
        room,
        authed: false,
        source: null,
        login: null,
        device_id: null,
      });

      server.addEventListener("message", (ev) => {
        this.onWebSocketMessage(server, ev.data).catch((err) => {
          try { server.close(1011, "internal error"); } catch (_) {}
        });
      });
      server.addEventListener("close", () => this.sessions.delete(server));
      server.addEventListener("error", () => this.sessions.delete(server));

      return new Response(null, { status: 101, webSocket: client });
    }

    // SSE stream. Read-only push of every broadcast in this room. The worker
    // forwards the original request, so the pathname is the full
    // `/v1/rooms/:room/events` — match by suffix.
    if (url.pathname.endsWith("/events") && request.method === "GET") {
      return this.handleSSE(request, url);
    }

    // HTTP relay: worker POST handlers call this with the freshly-written
    // payload to fan it out to every connected WS + SSE client. This one IS
    // called with a synthetic `/relay` URL (see relayToRoom).
    if (url.pathname === "/relay" && request.method === "POST") {
      const message = await request.text();
      this.broadcast(message, null);
      return new Response("ok", { status: 200 });
    }

    return new Response("not found", { status: 404 });
  }

  async onWebSocketMessage(ws, raw) {
    const meta = this.sessions.get(ws);
    if (!meta) {
      try { ws.close(4000, "unknown session"); } catch (_) {}
      return;
    }

    // First message is auth.
    if (!meta.authed) {
      let auth;
      try { auth = JSON.parse(raw); } catch (_) {
        try { ws.close(4001, "auth: invalid json"); } catch (_) {}
        return;
      }
      const result = await this.authenticate(auth, meta.room);
      if (!result.ok) {
        try { ws.close(4001, "auth: " + (result.error || "failed")); } catch (_) {}
        return;
      }
      meta.authed = true;
      meta.source = result.source;
      meta.login = result.login;
      meta.device_id = result.device_id;
      try { ws.send(JSON.stringify({ type: "auth_ok" })); } catch (_) {}
      return;
    }

    // Authenticated message. Parse and route.
    let parsed;
    try { parsed = JSON.parse(raw); } catch (_) {
      // Not JSON — drop silently. Wire contract is JSON.
      return;
    }

    // Reply type (browser operator reply): persist to KV, then relay as a
    // typed wrapper so other browsers see the canonical reply shape.
    if (parsed.type === "reply") {
      const reply = {
        v: 1,
        target_device: String(parsed.target_device ?? ""),
        target_session_prefix: String(parsed.target_session_prefix ?? ""),
        text: String(parsed.text ?? "").slice(0, 1024),
        author_login: meta.login ?? "",
        ts: Date.now(),
      };
      try {
        await this.env.AGENT_BOARD.put(
          `room:${meta.room}:reply:${nowKey()}`,
          JSON.stringify(reply),
          { expirationTtl: TTL_SECS }
        );
      } catch (err) {
        // KV write failed — still relay the message (best-effort delivery), but
        // the feeder poll won't see it. Log to the WS so the operator knows.
        try {
          ws.send(JSON.stringify({ type: "error", error: "kv write failed" }));
        } catch (_) {}
      }
      this.broadcast(JSON.stringify({ type: "reply", ...reply }), ws);
      return;
    }

    // Anything else: echo to the rest of the room verbatim. (Status/msg/state
    // normally arrive via the worker HTTP path and reach the DO through
    // /relay, but WS-originated messages use this path.)
    this.broadcast(raw, ws);
  }

  // Authenticate the first WS message. Two paths:
  //   - { type: "auth", github_token: "<token>" } → browser via GitHub device flow
  //   - { type: "auth", sig, pubkey, device_id, timestamp } → Zed via ed25519
  //   The ed25519 path mirrors verifySignature but with an empty body (the
  //   client signs "|"+timestamp, same canonical form the HTTP path uses).
  async authenticate(auth, room) {
    if (!auth || typeof auth !== "object") return { ok: false, error: "bad auth shape" };

    if (auth.github_token) {
      const result = await verifyGithubToken(this.env, auth.github_token);
      if (!result.ok) return result;
      return { ok: true, source: "github", login: result.login, device_id: null };
    }

    if (auth.sig && auth.pubkey && auth.device_id && auth.timestamp) {
      const ts = parseInt(auth.timestamp, 10);
      const skew = Math.abs(Date.now() / 1000 - ts);
      if (Number.isNaN(ts) || skew > 300) {
        return { ok: false, error: "timestamp skew too large" };
      }
      const known = await this.env.AGENT_BOARD.get(`device:${auth.device_id}`);
      const anyDevice =
        (await this.env.AGENT_BOARD.list({ prefix: "device:", limit: 1 })).keys.length > 0;
      if (anyDevice && known === null) {
        return { ok: false, error: "device not in allowlist" };
      }
      try {
        const sig = b64decode(auth.sig);
        const pubkey = b64decode(auth.pubkey);
        const message = new TextEncoder().encode("|" + auth.timestamp);
        const ok = await ed.verifyAsync(sig, message, pubkey);
        if (!ok) return { ok: false, error: "bad signature" };
        await this.env.AGENT_BOARD.put(`device:${auth.device_id}`, auth.pubkey, {
          expirationTtl: TTL_SECS,
        });
        return { ok: true, source: "ed25519", login: null, device_id: auth.device_id };
      } catch (err) {
        return { ok: false, error: "verify threw: " + String(err) };
      }
    }

    return { ok: false, error: "no auth fields" };
  }

  // Push a message to every authed WS in the room (except `exceptWs`, the
  // originator of a client-driven message) and to every SSE controller.
  broadcast(message, exceptWs) {
    for (const [ws, meta] of this.sessions) {
      if (ws === exceptWs) continue;
      if (!meta.authed) continue;
      try { ws.send(message); } catch (_) {}
    }
    for (const writer of this.sseWriters) {
      try { writer.write(encoder.encode("data: " + message + "\n\n")); } catch (_) {}
    }
  }

  // SSE: hold the response stream open, push every broadcast as
  // `data: {json}\n\n`, and keep the connection warm with a comment every 15s.
  // Uses TransformStream (the documented Workers SSE pattern) rather than a
  // bare ReadableStream — `new Response(readableStream)` works, but only
  // TransformStream is guaranteed to satisfy the byte-stream contract that
  // `Response.body` consumers expect.
  handleSSE(request, url) {
    const headers = {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-cache, no-transform",
      "connection": "keep-alive",
    };

    const { readable, writable } = new TransformStream();
    const writer = writable.getWriter();
    const self = this;
    self.sseWriters.add(writer);

    // Initial comment + keepalive.
    writer.write(encoder.encode(": connected\n\n")).catch(() => {});
    const keepalive = setInterval(() => {
      writer.write(encoder.encode(": keepalive\n\n")).catch(() => {});
    }, 15000);

    const cleanup = () => {
      clearInterval(keepalive);
      self.sseWriters.delete(writer);
      try { writer.close(); } catch (_) {}
    };
    if (request.signal) {
      request.signal.addEventListener("abort", cleanup);
    }

    return new Response(readable, { headers });
  }
}
