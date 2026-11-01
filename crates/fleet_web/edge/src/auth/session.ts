// Session cookie: `base64url(login).expUnix.hexsig`, sig = blake3_keyed(session_key, "login\nexp").
import { base64UrlDecode, base64UrlEncode, constantTimeEqual, keyedHashHex } from "../crypto.ts";

export const SESSION_COOKIE = "fw_session";
export const STATE_COOKIE = "fw_state";
export const SESSION_TTL_SECONDS = 7 * 24 * 60 * 60;
export const STATE_TTL_SECONDS = 10 * 60;

const LOGIN_PATTERN = /^[A-Za-z0-9-]{1,39}$/;

export function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

function signature(key: Uint8Array, login: string, expires: number): string {
  return keyedHashHex(key, `${login}\n${expires}`);
}

export function signSession(key: Uint8Array, login: string, expires: number): string {
  if (!LOGIN_PATTERN.test(login)) throw new Error("invalid GitHub login");
  return `${base64UrlEncode(login)}.${expires}.${signature(key, login, expires)}`;
}

/** Returns the login for a valid, unexpired cookie value, else null. */
export function verifySession(key: Uint8Array, value: string, now: number = nowSeconds()): string | null {
  const parts = value.split(".");
  if (parts.length !== 3) return null;
  const [encodedLogin, expiresText, givenSignature] = parts;
  if (!/^[0-9]{1,12}$/.test(expiresText) || !/^[0-9a-f]{64}$/.test(givenSignature)) return null;
  const login = base64UrlDecode(encodedLogin);
  if (login === null || !LOGIN_PATTERN.test(login)) return null;
  const expires = Number(expiresText);
  // The signature is checked before expiry so timing does not reveal which check failed.
  const valid = constantTimeEqual(signature(key, login, expires), givenSignature);
  if (!valid || expires <= now) return null;
  return login;
}

export function isAllowedLogin(login: string, allowedLogins: string): boolean {
  const wanted = login.toLowerCase();
  return allowedLogins
    .split(",")
    .map((entry) => entry.trim().toLowerCase())
    .some((entry) => entry.length > 0 && entry === wanted);
}
