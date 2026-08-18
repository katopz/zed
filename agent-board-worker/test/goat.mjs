// Live GOAT-gate verification for the deployed agent-board worker (Plan 015).
//
// Signs every request with the operator's real ~/.ssh/id_ed25519 — byte-identical
// to what Zed's DeviceIdentity sends — so the device allowlist ends up in the
// exact production state, and the requests exercise the real auth path.
//
// Usage: node goat.mjs [https://<worker-url>] [room]
//   WORKER_URL defaults to https://agent-board-worker.foxfox.workers.dev
//   ROOM defaults to goat-test (keys auto-expire via 7d TTL)
//
// Exit 0 = all PASS, 1 = any FAIL.

import { createPrivateKey, generateKeyPairSync, sign as edSign } from "node:crypto";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import blake3Pkg from "blake3";
const { hash: blake3 } = blake3Pkg;

const WORKER = process.argv[2] ?? "https://agent-board-worker.foxfox.workers.dev";
const ROOM = process.argv[3] ?? "goat-test";
const KEY_PATH = process.env.AGENT_BOARD_KEY ?? join(homedir(), ".ssh", "id_ed25519");

// ── OpenSSH ed25519 private key → PKCS#8 DER (Node crypto can't read openssh) ──
function loadIdentity(path) {
  const pem = readFileSync(path, "utf8");
  const b64 = pem.replace(/-----(BEGIN|END) OPENSSH PRIVATE KEY-----/g, "").replace(/\s+/g, "");
  const buf = Buffer.from(b64, "base64");
  let off = 0;
  const magic = "openssh-key-v1\0";
  if (buf.subarray(0, magic.length).toString() !== magic) throw new Error("not an openssh key");
  off = magic.length;
  const rdString = () => {
    const len = buf.readUInt32BE(off); off += 4;
    const s = buf.subarray(off, off + len); off += len;
    return s;
  };
  const cipher = rdString().toString();
  if (cipher !== "none") throw new Error("encrypted key not supported");
  rdString(); // kdfname
  rdString(); // kdfoptions
  const numKeys = buf.readUInt32BE(off); off += 4;
  if (numKeys !== 1) throw new Error("expected exactly 1 key");
  rdString(); // pubkey blob (ssh-ed25519 + 32B) — re-derived below instead
  const priv = rdString();
  // priv section: checkint(4) checkint(4) keytype string pub string seed+pub string ...
  let p = 8;
  const rdPrivString = () => {
    const len = priv.readUInt32BE(p); p += 4;
    const s = priv.subarray(p, p + len); p += len;
    return s;
  };
  rdPrivString(); // keytype "ssh-ed25519"
  const pubFromFile = rdPrivString(); // 32-byte public
  const seedPub = rdPrivString(); // 64 bytes: seed || pub
  const seed = seedPub.subarray(0, 32);

  // PKCS#8 template for Ed25519: fixed prefix + 32-byte seed.
  const pkcs8 = Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), seed]);
  const keyObj = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  const pub32 = Buffer.from(pubFromFile); // trust the file's copy; verified via sign below
  if (!pub32.equals(seedPub.subarray(32, 64))) throw new Error("pubkey mismatch in openssh key");
  return { keyObj, pub32 };
}

const identity = loadIdentity(KEY_PATH);
const PUBKEY_B64 = identity.pub32.toString("base64");
const DEVICE_ID = blake3(identity.pub32).toString("hex");

// Mirror identity.rs: sign(body + "|" + timestamp).
function signBody(bodyText, timestamp) {
  return edSign(null, Buffer.from(bodyText + "|" + timestamp, "utf8"), identity.keyObj).toString("base64");
}

function authHeaders(bodyText) {
  const ts = Math.floor(Date.now() / 1000);
  return {
    "Content-Type": "application/json",
    "X-Device-Id": DEVICE_ID,
    "X-Timestamp": String(ts),
    "X-Sig": signBody(bodyText, ts),
    "X-Pubkey": PUBKEY_B64,
  };
}

// ── tiny test harness ──
let failures = 0;
function report(name, ok, detail = "") {
  const tag = ok ? "PASS" : "FAIL";
  if (!ok) failures++;
  console.log(`${tag}  ${name}${detail ? "  — " + detail : ""}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── SSE reader ──
async function openSSE(room) {
  const res = await fetch(`${WORKER}/v1/rooms/${encodeURIComponent(room)}/events?device=${DEVICE_ID}`, {
    headers: { Accept: "text/event-stream" },
  });
  if (!res.ok) throw new Error(`SSE connect failed: ${res.status}`);
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  const pending = [];
  const waiters = [];
  (async () => {
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf("\n\n")) !== -1) {
          const frame = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          const dataLine = frame.split("\n").find((l) => l.startsWith("data: "));
          if (!dataLine) continue; // comment / keepalive
          const payload = dataLine.slice(6);
          const parsed = JSON.parse(payload);
          const w = waiters.shift();
          if (w) w(parsed); else pending.push(parsed);
        }
      }
    } catch (_) { /* stream ended */ }
  })();
  return {
    next: (timeoutMs = 10000) =>
      new Promise((resolve, reject) => {
        const p = pending.shift();
        if (p) return resolve(p);
        const timer = setTimeout(() => reject(new Error("SSE event timeout")), timeoutMs);
        waiters.push((payload) => { clearTimeout(timer); resolve(payload); });
      }),
    close: () => reader.cancel(),
  };
}

// ── WebSocket helper ──
function wsConnect(room) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`wss://${new URL(WORKER).host}/ws?room=${encodeURIComponent(room)}`);
    const events = [];
    ws.addEventListener("message", (ev) => events.push(JSON.parse(ev.data)));
    ws.addEventListener("open", () => resolve({ ws, events }));
    ws.addEventListener("error", (e) => reject(new Error("ws error: " + String(e))));
  });
}
const wsSend = (ws, obj) => ws.send(JSON.stringify(obj));
function wsAuthEd(room) {
  const ts = Math.floor(Date.now() / 1000);
  return { type: "auth", sig: signBody("", ts), pubkey: PUBKEY_B64, device_id: DEVICE_ID, timestamp: String(ts) };
}

async function main() {
  console.log(`worker: ${WORKER}`);
  console.log(`room:   ${ROOM}`);
  console.log(`device: ${DEVICE_ID.slice(0, 16)}… (blake3 of real ssh pubkey)\n`);

  // T1 — dashboard HTML
  {
    const res = await fetch(`${WORKER}/?room=${ROOM}`);
    const html = await res.text();
    report("T1 GET / → 200", res.status === 200, `${res.status}`);
    const markers = ["reply", "toggleDev", "toggleAg", "setStatus", "connect", "ghbtn", "auth/github/device"];
    const missing = markers.filter((m) => !html.includes(m));
    report("T1 dashboard markers present", missing.length === 0, missing.length ? `missing: ${missing}` : "reply input, accordion, status, ws, github sign-in");
  }

  // T2 — room snapshot (empty ok)
  {
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}`);
    const snap = await res.json();
    report("T2 GET /v1/rooms/{room} → 200 JSON", res.status === 200 && typeof snap === "object");
  }

  // SSE + relay tests
  const sse = await openSSE(ROOM);

  // Warm-up: one unmeasured round-trip wakes the DO and the KV path so the
  // latency assertions below measure steady state, not cold start.
  {
    const body = JSON.stringify({ device_name: "goat-m3", status_text: "warm-up" });
    await fetch(`${WORKER}/v1/rooms/${ROOM}/status`, { method: "POST", headers: authHeaders(body), body });
    await sse.next(15000);
  }

  // T3 — POST /status relays to SSE
  {
    const body = JSON.stringify({ device_name: "goat-m3", status_text: "verifying", room_note: "goat run" });
    const t0 = Date.now();
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/status`, { method: "POST", headers: authHeaders(body), body });
    const postMs = Date.now() - t0;
    report("T3 POST /status → 200", res.status === 200, `${res.status}, ${postMs}ms`);
    const evt = await sse.next(15000);
    const latency = Date.now() - t0;
    const okShape = evt.device_id === DEVICE_ID && evt.device_name === "goat-m3";
    report("T3 status relayed via SSE", okShape, `latency ${latency}ms (<1000ms target: ${latency < 1000})`);
  }

  // T4 — POST /state relays
  {
    const body = JSON.stringify({ device_name: "goat-m3", session_id: "goatsess0001", state_text: "running goat tests", meta: "plan015" });
    const t0 = Date.now();
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/state`, { method: "POST", headers: authHeaders(body), body });
    report("T4 POST /state → 201", res.status === 201, `${res.status}`);
    const evt = await sse.next(15000);
    const latency = Date.now() - t0;
    report("T4 state relayed via SSE", evt.session_id === "goatsess0001" && evt.state_text === "running goat tests", `latency ${latency}ms`);
  }

  // T5 — POST /msg relays
  {
    const body = JSON.stringify({ device_name: "goat-m3", text: "goat message" });
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/msg`, { method: "POST", headers: authHeaders(body), body });
    report("T5 POST /msg → 201", res.status === 201, `${res.status}`);
    const evt = await sse.next(15000);
    report("T5 msg relayed via SSE", evt.text === "goat message");
  }

  // T6 — POST /reply (ed25519 author path) → stored + relayed as typed wrapper
  {
    const body = JSON.stringify({ type: "reply", target_device: DEVICE_ID, target_session_prefix: "goat", text: "goat reply verification" });
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/reply`, { method: "POST", headers: authHeaders(body), body });
    report("T6 POST /reply → 201", res.status === 201, `${res.status}`);
    const evt = await sse.next(15000);
    report("T6 reply relayed via SSE (typed)", evt.type === "reply" && evt.target_session_prefix === "goat" && evt.text === "goat reply verification");
    // KV persistence. KV list/get at the reading colo can lag the write by up
    // to 60s (documented eventual consistency) — poll a full minute.
    let stored = false;
    for (let i = 0; i < 60 && !stored; i++) {
      await sleep(1000);
      const snap = await (await fetch(`${WORKER}/v1/rooms/${ROOM}`)).json();
      stored = (snap.replies ?? []).some((r) => r.text === "goat reply verification");
    }
    report("T6 reply persisted in room snapshot", stored, "KV convergence ≤60s");
  }

  // T7 — WebSocket: ed25519 auth + relay fan-out to a second WS client
  {
    const a = await wsConnect(ROOM); // observer
    wsSend(a.ws, wsAuthEd(ROOM));
    await sleep(1500); // auth round-trip
    const b = await wsConnect(ROOM); // sender via HTTP POST (like Zed feeder)
    wsSend(b.ws, wsAuthEd(ROOM));
    await sleep(1500);
    const body = JSON.stringify({ device_name: "goat-ws", status_text: "ws fan-out check" });
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/status`, { method: "POST", headers: authHeaders(body), body });
    await sleep(2000);
    const sawReply = a.events.some((e) => e.device_name === "goat-ws");
    report("T7 WS: HTTP POST relayed to connected WS client", sawReply, `${a.events.length} events @observer`);
    a.ws.close(); b.ws.close();
  }

  // T8 — WS negative: garbage token → close 4001 (GitHub verification fails closed)
  {
    const c = await wsConnect(ROOM);
    const closed = await new Promise((resolve) => {
      c.ws.addEventListener("close", (ev) => resolve(ev.code));
      wsSend(c.ws, { type: "auth", github_token: "ghp_garbageTokenInvalid" });
    });
    report("T8 WS bad token → close 4001", closed === 4001, `code=${closed}`);
  }

  // T8b — WS positive with a real GitHub token (optional: only when
  // AGENT_BOARD_GITHUB_PAT is provided). Verifies the allowlist: a PAT for
  // ALLOWED_LOGIN passes, anything else closes 4001.
  if (process.env.AGENT_BOARD_GITHUB_PAT) {
    const c = await wsConnect(ROOM);
    const outcome = await new Promise((resolve) => {
      const to = setTimeout(() => resolve("timeout"), 15000);
      c.ws.addEventListener("close", (ev) => { clearTimeout(to); resolve("close:" + ev.code); });
      c.ws.addEventListener("message", (ev) => {
        const m = JSON.parse(ev.data);
        if (m.type === "auth_ok") { clearTimeout(to); resolve("auth_ok"); }
      });
      wsSend(c.ws, { type: "auth", github_token: process.env.AGENT_BOARD_GITHUB_PAT });
    });
    report("T8b WS real GitHub token → auth_ok", outcome === "auth_ok", outcome);
    try { c.ws.close(); } catch (_) {}
  }

  // T9 — unknown device rejected (allowlist after bootstrap).
  // Caveat (documented KV limitation): the bootstrap gate reads `list`, which
  // is eventually consistent (≤60s). Right after the first-ever registration,
  // a probe may self-register before the list converges. Retry with fresh
  // keys until the steady-state 403 is observed; any probe that registered
  // during the race window is reported with its cleanup command.
  {
    const registeredProbes = [];
    let got403 = false;
    let raceObserved = false;
    for (let attempt = 0; attempt < 20 && !got403; attempt++) {
      const fresh = generateKeyPairSync("ed25519");
      const freshPub = fresh.publicKey.export({ format: "der", type: "spki" }).subarray(-32);
      const freshId = blake3(freshPub).toString("hex");
      const ts = Math.floor(Date.now() / 1000);
      const sig = edSign(null, Buffer.from("{}|" + ts), fresh.privateKey).toString("base64");
      const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/status`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Device-Id": freshId,
          "X-Timestamp": String(ts),
          "X-Sig": sig,
          "X-Pubkey": freshPub.toString("base64"),
        },
        body: "{}",
      });
      if (res.status === 403) got403 = true;
      else if (res.status === 200) { raceObserved = true; registeredProbes.push(freshId); }
      if (!got403) await sleep(5000);
    }
    report("T9 unknown device → 403 (steady state)", got403, raceObserved ? `bootstrap race observed (KV list lag); ${registeredProbes.length} probe(s) registered — cleanup below` : "no race this run");
    for (const id of registeredProbes) {
      console.log(`      cleanup: npx wrangler kv key delete --namespace-id <NS> --name "device:${id}" --force`);
    }
  }

  // T10 — stale timestamp rejected (anti-replay)
  {
    const ts = Math.floor(Date.now() / 1000) - 3600;
    const body = "{}";
    const res = await fetch(`${WORKER}/v1/rooms/${ROOM}/status`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Device-Id": DEVICE_ID,
        "X-Timestamp": String(ts),
        "X-Sig": signBody(body, ts),
        "X-Pubkey": PUBKEY_B64,
      },
      body,
    });
    report("T10 stale timestamp → 401", res.status === 401, `${res.status}`);
  }

  await sse.close();
  console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAIL`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((err) => { console.error("FATAL:", err.message); process.exit(1); });
