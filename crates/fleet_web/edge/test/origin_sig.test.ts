import assert from "node:assert/strict";
import { test } from "node:test";
import { parseKeyHex } from "../src/crypto.ts";
import { originSignature } from "../src/origin_sig.ts";

// Cross-check with Rust: blake3::keyed_hash(&[1u8; 32], b"1700000000\nGET\n/api/fleet").to_hex()
// (also reproducible with `b3sum --keyed`).
const EXPECTED = "86c1282cc2414132c075b91d9e0399a38609ef0f4f993fd303ae97338d6cc2d5";

test("origin signature matches the fixed blake3 keyed vector", () => {
  const signature = originSignature(parseKeyHex("01".repeat(32)), 1_700_000_000, "GET", "/api/fleet");
  console.log(`origin signature vector: ${signature}`);
  assert.equal(signature, EXPECTED);
});

test("signature covers timestamp, method and path", () => {
  const key = parseKeyHex("01".repeat(32));
  assert.notEqual(originSignature(key, 1_700_000_001, "GET", "/api/fleet"), EXPECTED);
  assert.notEqual(originSignature(key, 1_700_000_000, "GET", "/api/threads"), EXPECTED);
  assert.equal(originSignature(key, 1_700_000_000, "get", "/api/fleet"), EXPECTED, "method is upper-cased");
});
