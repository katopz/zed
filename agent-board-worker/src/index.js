// Agent Board — Cloudflare Worker (KV-backed multi-device notepad for Zed agents).
//
// Wire contract (v1). All write endpoints require headers:
//   X-Device-Id   : hex(blake3(raw_ed25519_pubkey_32))
//   X-Timestamp   : unix seconds (string)
//   X-Sig         : base64( ed25519_sign( canonical_request_body + "|" + timestamp ) )
//   X-Pubkey      : base64( raw 32-byte ed25519 pubkey )
//
// The worker verifies X-Sig against the device allowlist (KV `device:` keys).
// GET is open (read-only). The board is single-user; the signature gate exists
// only to prevent spam, per the owner's explicit design.
//
// Signing note: clients sign the *raw request body text + "|" + timestamp*
// bytes directly. ed25519 signs arbitrary-length messages internally, so no
// pre-hashing is needed on either side. The Rust client in
// `crates/agent_board/src/identity.rs` must produce the exact same message.

import * as ed from "@noble/ed25519";

const MAX_MESSAGES = 10;
const STALE_STATUS_SECS = 300;
const TTL_SECS = 60 * 60 * 24 * 7; // 1 week

function b64decode(s) {
  return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
}

function nowKey() {
  // Sortable key suffix: base36 of millis + short random.
  return Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 8);
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
  return json(status, 200);
}

async function handlePostMsg(env, room, body, verified) {
  const msg = {
    v: 1,
    device_id: verified.deviceId,
    device_name: body.device_name ?? "",
    text: String(body.text ?? "").slice(0, 1024),
    ts: Date.now(),
  };
  await env.AGENT_BOARD.put(`room:${room}:msg:${nowKey()}`, JSON.stringify(msg), {
    expirationTtl: TTL_SECS,
  });
  return json(msg, 201);
}

async function handleGetRoom(env, room) {
  const [deviceList, msgList] = await Promise.all([
    env.AGENT_BOARD.list({ prefix: `room:${room}:device:`, limit: 64 }),
    env.AGENT_BOARD.list({ prefix: `room:${room}:msg:`, limit: MAX_MESSAGES + 5 }),
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
  return json({ v: 1, room, statuses, messages }, 200);
}

export default {
  async fetch(request, env) {
    // Reference ttlOf so the per-env override is honored (and kept from
    // appearing unused if the operator sets DEFAULT_TTL_SECONDS).
    await ttlOf(env);

    const url = new URL(request.url);
    const path = url.pathname;

    if (path === "/healthz") return json({ ok: true }, 200);

    const roomMatch = path.match(/^\/v1\/rooms\/([^/]+)$/);
    if (roomMatch && request.method === "GET") {
      return handleGetRoom(env, decodeURIComponent(roomMatch[1]));
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

    return json({ error: "not found" }, 404);
  },
};
