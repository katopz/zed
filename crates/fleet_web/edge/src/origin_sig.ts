import { keyedHashHex } from "./crypto.ts";

/** `x-fleet-sig` = hex blake3_keyed(origin_key, "{ts}\n{METHOD}\n{path_and_query}"). */
export function originSignature(key: Uint8Array, timestamp: number, method: string, pathAndQuery: string): string {
  return keyedHashHex(key, `${timestamp}\n${method.toUpperCase()}\n${pathAndQuery}`);
}
