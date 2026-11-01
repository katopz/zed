import assert from "node:assert/strict";
import { test } from "node:test";
import { isAllowedLogin, signSession, verifySession } from "../src/auth/session.ts";
import { base64UrlEncode, constantTimeEqual, parseKeyHex } from "../src/crypto.ts";

const key = parseKeyHex("02".repeat(32));
const otherKey = parseKeyHex("03".repeat(32));
const now = 1_700_000_000;

test("a signed session verifies until it expires", () => {
  const cookie = signSession(key, "katopz", now + 60);
  assert.match(cookie, /^[A-Za-z0-9_-]+\.\d+\.[0-9a-f]{64}$/);
  assert.equal(verifySession(key, cookie, now), "katopz");
  assert.equal(verifySession(key, cookie, now + 60), null, "expired at exp");
  assert.equal(verifySession(key, cookie, now + 3600), null);
});

test("tampering with any part invalidates the session", () => {
  const cookie = signSession(key, "katopz", now + 60);
  const [login, expires, signature] = cookie.split(".");
  assert.equal(verifySession(key, `${base64UrlEncode("attacker")}.${expires}.${signature}`, now), null);
  assert.equal(verifySession(key, `${login}.${Number(expires) + 86400}.${signature}`, now), null);
  const flipped = (signature[0] === "a" ? "b" : "a") + signature.slice(1);
  assert.equal(verifySession(key, `${login}.${expires}.${flipped}`, now), null);
  assert.equal(verifySession(otherKey, cookie, now), null, "wrong key");
  assert.equal(verifySession(key, `${cookie}.extra`, now), null);
  assert.equal(verifySession(key, "", now), null);
  assert.equal(verifySession(key, "a.b.c", now), null);
});

test("logins outside GitHub's alphabet are refused", () => {
  assert.throws(() => signSession(key, "evil\nname", now + 60));
  const forged = `${base64UrlEncode("evil\nname")}.${now + 60}.${"0".repeat(64)}`;
  assert.equal(verifySession(key, forged, now), null);
});

test("allowlist is case-insensitive and exact", () => {
  assert.ok(isAllowedLogin("KatOPZ", "someone, katopz ,other"));
  assert.ok(!isAllowedLogin("katop", "katopz"));
  assert.ok(!isAllowedLogin("katopz", ""));
  assert.ok(!isAllowedLogin("", " , "));
});

test("constant-time comparison", () => {
  assert.ok(constantTimeEqual("abc", "abc"));
  assert.ok(!constantTimeEqual("abc", "abd"));
  assert.ok(!constantTimeEqual("abc", "abcd"));
  assert.ok(!constantTimeEqual("", "a"));
});

test("keys must be 32 bytes of hex", () => {
  assert.throws(() => parseKeyHex("01".repeat(31)));
  assert.throws(() => parseKeyHex("zz".repeat(32)));
});
