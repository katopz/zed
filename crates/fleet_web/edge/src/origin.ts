// Signed, edge-cached calls to the origin rig (`fleet-web serve`).
import { CacheOverride } from "fastly:cache-override";
import { configValue, secretKey } from "./config.ts";
import { originSignature } from "./origin_sig.ts";

export type OriginResult<T> = { ok: true; data: T } | { ok: false; status: number; error: string };

export const ORIGIN_TTL = {
  fleet: 15,
  threads: 30,
  thread: 30,
} as const;

const ORIGIN_TIMEOUT_MS = 8000;

/** `pathAndQuery` must already be validated and percent-encoded by the caller. */
export async function originGet<T>(pathAndQuery: string, ttlSeconds: number): Promise<OriginResult<T>> {
  let response: Response;
  try {
    const key = await secretKey("origin_key");
    const timestamp = Math.floor(Date.now() / 1000);
    // The backend decides where the request goes; the host only shapes the Host header.
    const host = configValue("origin_host") ?? "origin";
    response = await withTimeout(
      fetch(`https://${host}${pathAndQuery}`, {
        method: "GET",
        backend: "origin",
        cacheOverride: new CacheOverride("override", { ttl: ttlSeconds }),
        headers: {
          accept: "application/json",
          "x-fleet-ts": String(timestamp),
          "x-fleet-sig": originSignature(key, timestamp, "GET", pathAndQuery),
        },
      }),
      ORIGIN_TIMEOUT_MS,
    );
  } catch (error) {
    return { ok: false, status: 502, error: `origin unreachable: ${(error as Error).message.replace(/\.$/, "")}` };
  }
  if (!response.ok) {
    return { ok: false, status: response.status, error: `origin answered ${response.status}` };
  }
  try {
    return { ok: true, data: (await response.json()) as T };
  } catch {
    return { ok: false, status: 502, error: "origin sent invalid JSON" };
  }
}

function withTimeout<T>(promise: Promise<T>, milliseconds: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timed out after ${milliseconds} ms`)), milliseconds);
    promise.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}
