// Worker integration tests via Miniflare 3.
//
// These cover GOAT-gate items that can be verified without a browser or live
// GitHub device-flow sign-in:
//   - GET / returns the HTML dashboard with expected elements.
//   - POST /status writes to KV and returns 200 (the DO relay is best-effort;
//     we verify the handler succeeds, not the DO fan-out itself).
//   - GET /v1/rooms/:room returns the room snapshot after a status POST.
//   - POST /reply stores the reply so a subsequent GET includes it.
//
// Auth: the ed25519 signature gate is bootstrapped (first device
// self-registers). We generate a real ed25519 keypair, sign each request, and
// let the worker register it on the first POST.

import { describe, it, expect, beforeAll } from "vitest";
import { Miniflare } from "miniflare";
import * as esbuild from "esbuild";
import * as ed from "@noble/ed25519";

// ── ed25519 helpers ──────────────────────────────────────────────────────

const TEXT = new TextEncoder();

function b64(bytes) {
  return btoa(String.fromCharCode(...new Uint8Array(bytes)));
}

async function makeDevice() {
  const priv = ed.utils.randomPrivateKey();
  const pub = await ed.getPublicKeyAsync(priv);
  // The worker doesn't verify the *format* of Device-Id (only that it's
  // consistent), so a plain hex of the pubkey works for testing.
  const deviceId = Buffer.from(pub).toString("hex");
  return { priv, pub, deviceId };
}

async function signHeaders(device, bodyText) {
  const ts = Math.floor(Date.now() / 1000).toString();
  const message = TEXT.encode(bodyText + "|" + ts);
  const sig = await ed.signAsync(message, device.priv);
  return {
    "X-Device-Id": device.deviceId,
    "X-Timestamp": ts,
    "X-Sig": b64(sig),
    "X-Pubkey": b64(device.pub),
    "Content-Type": "application/json",
  };
}

// ── Miniflare harness ────────────────────────────────────────────────────

async function makeMf() {
  // Bundle the worker with esbuild so Miniflare can resolve `@noble/ed25519`.
  const bundled = await esbuild.build({
    entryPoints: ["src/index.js"],
    bundle: true,
    format: "esm",
    platform: "neutral",
    write: false,
  });
  const script = bundled.outputFiles[0].text;

  const mf = new Miniflare({
    modules: true,
    script,
    compatDate: "2024-12-01",
    kvNamespaces: ["AGENT_BOARD"],
    durableObjects: {
      ROOM_COORDINATOR: "RoomCoordinator",
    },
    bindings: {
      GOOGLE_CLIENT_ID: "",
      ALLOWED_EMAIL: "katopz@gmail.com",
      DEFAULT_TTL_SECONDS: "604800",
    },
  });
  await mf.ready;
  return mf;
}

// ── Tests ────────────────────────────────────────────────────────────────

describe("GET / (W1 dashboard)", () => {
  let mf;

  beforeAll(async () => {
    mf = await makeMf();
  });

  it("returns HTML with the dashboard shell", async () => {
    const res = await mf.dispatchFetch("http://localhost/");
    expect(res.status).toBe(200);
    const html = await res.text();
    expect(html).toContain("<!DOCTYPE html>");
    expect(html).toContain("Agent Board");
    expect(html).toContain('id="dash"');
    expect(html).toContain('id="replybar"');
    expect(html).toContain('placeholder="REPLY:[device:sess4] message"');
  });

  it("includes the GitHub device-flow sign-in affordances", async () => {
    const res = await mf.dispatchFetch("http://localhost/");
    const html = await res.text();
    expect(html).toContain('id="ghbtn"');
    expect(html).toContain("Sign in with GitHub");
    expect(html).toContain("/auth/github/device");
    expect(html).toContain("/auth/github/poll");
  });
});

describe("room snapshot round-trip (W6 wire contract)", () => {
  let mf;
  let device;

  beforeAll(async () => {
    mf = await makeMf();
    device = await makeDevice();
  });

  async function postStatus(body) {
    const bodyText = JSON.stringify(body);
    const headers = await signHeaders(device, bodyText);
    const res = await mf.dispatchFetch(
      "http://localhost/v1/rooms/test-room/status",
      { method: "POST", headers, body: bodyText },
    );
    return { res, json: await res.json() };
  }

  async function postReply(body) {
    const bodyText = JSON.stringify(body);
    const headers = await signHeaders(device, bodyText);
    const res = await mf.dispatchFetch(
      "http://localhost/v1/rooms/test-room/reply",
      { method: "POST", headers, body: bodyText },
    );
    return { res, json: await res.json() };
  }

  async function getRoom() {
    const res = await mf.dispatchFetch(
      "http://localhost/v1/rooms/test-room",
    );
    return { res, json: await res.json() };
  }

  it("POST /status writes device status", async () => {
    const body = {
      device_name: "test-device",
      location_hash: "abc123",
      project_path: "/project",
      scopes: [],
    };
    const { res, json } = await postStatus(body);
    expect(res.status).toBe(200);
    expect(json.device_name).toBe("test-device");
  });

  it("GET /v1/rooms/:room returns the snapshot with the device", async () => {
    const { json } = await getRoom();
    expect(json.v).toBe(1);
    expect(json.room).toBe("test-room");
    expect(Array.isArray(json.statuses)).toBe(true);
    const found = json.statuses.find(
      (s) => s.device_name === "test-device",
    );
    expect(found).toBeTruthy();
  });

  it("POST /reply stores a reply so GET includes it", async () => {
    const { res, json } = await postReply({
      target_device: "test-device",
      target_session_prefix: "f3a2",
      text: "stop and commit",
    });
    expect(res.status).toBe(201);
    expect(json.target_device).toBe("test-device");
    expect(json.target_session_prefix).toBe("f3a2");
    expect(json.text).toBe("stop and commit");

    const { json: room } = await getRoom();
    expect(room.replies.length).toBeGreaterThanOrEqual(1);
    const reply = room.replies.find(
      (r) => r.target_device === "test-device",
    );
    expect(reply).toBeTruthy();
    expect(reply.text).toBe("stop and commit");
  });

  it("GET on an empty room returns empty arrays, not errors", async () => {
    const res = await mf.dispatchFetch(
      "http://localhost/v1/rooms/fresh-room",
    );
    const json = await res.json();
    expect(res.status).toBe(200);
    expect(json.statuses).toEqual([]);
    expect(json.messages).toEqual([]);
    expect(json.states).toEqual([]);
    expect(json.replies).toEqual([]);
  });
});

describe("auth gate (ed25519 bootstrap)", () => {
  let mf;

  beforeAll(async () => {
    mf = await makeMf();
  });

  it("rejects unsigned POST /status with 401", async () => {
    const res = await mf.dispatchFetch(
      "http://localhost/v1/rooms/auth-room/status",
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ device_name: "x" }),
      },
    );
    expect(res.status).toBe(401);
  });
});
