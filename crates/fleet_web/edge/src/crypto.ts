// Pure crypto helpers (no `fastly:*` imports) so node's test runner can load them.
import { blake3 } from "@noble/hashes/blake3.js";
import { bytesToHex, hexToBytes, utf8ToBytes } from "@noble/hashes/utils.js";

export { bytesToHex };

/** Parses a 32-byte key given as 64 hex chars; throws on anything else. */
export function parseKeyHex(hex: string): Uint8Array {
  const trimmed = hex.trim();
  if (!/^[0-9a-fA-F]{64}$/.test(trimmed)) {
    throw new Error("key must be 32 bytes of hex (64 chars)");
  }
  return hexToBytes(trimmed.toLowerCase());
}

/** Hex BLAKE3 keyed hash; byte-identical to Rust's `blake3::keyed_hash`. */
export function keyedHashHex(key: Uint8Array, message: string): string {
  return bytesToHex(blake3(utf8ToBytes(message), { key }));
}

export function hashHex(message: string): string {
  return bytesToHex(blake3(utf8ToBytes(message)));
}

/** Compares two strings without an early exit on the first differing char. */
export function constantTimeEqual(left: string, right: string): boolean {
  const length = Math.max(left.length, right.length);
  let difference = left.length ^ right.length;
  for (let index = 0; index < length; index++) {
    difference |= (left.charCodeAt(index) | 0) ^ (right.charCodeAt(index) | 0);
  }
  return difference === 0;
}

export function randomHex(byteCount: number): string {
  const bytes = new Uint8Array(byteCount);
  crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

export function base64UrlEncode(text: string): string {
  const bytes = utf8ToBytes(text);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** Returns null for anything that is not canonical unpadded base64url. */
export function base64UrlDecode(encoded: string): string | null {
  if (!/^[A-Za-z0-9_-]*$/.test(encoded)) return null;
  try {
    const padded = encoded.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((encoded.length + 3) % 4);
    const binary = atob(padded);
    const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return null;
  }
}
