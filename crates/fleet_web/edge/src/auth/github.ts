// GitHub OAuth (web flow). The access token is used once to read the login
// and then dropped; only the signed session cookie survives.
import { CacheOverride } from "fastly:cache-override";
import { devLogin, requireConfig, requireSecret, secretKey } from "../config.ts";
import { constantTimeEqual, randomHex } from "../crypto.ts";
import { clearCookie, readCookie, redirect, setCookie } from "../http.ts";
import { errorPage } from "../views/pages.tsx";
import {
  SESSION_COOKIE,
  SESSION_TTL_SECONDS,
  STATE_COOKIE,
  STATE_TTL_SECONDS,
  isAllowedLogin,
  nowSeconds,
  signSession,
} from "./session.ts";

const USER_AGENT = "fleet-web-edge (plan 051)";

async function sessionCookie(login: string): Promise<string> {
  const key = await secretKey("session_key");
  const expires = nowSeconds() + SESSION_TTL_SECONDS;
  return setCookie(SESSION_COOKIE, signSession(key, login, expires), SESSION_TTL_SECONDS);
}

function callbackUrl(url: URL): string {
  return `${url.origin}/auth/callback`;
}

export async function handleLogin(url: URL): Promise<Response> {
  const bypassLogin = devLogin(url);
  if (bypassLogin !== null) {
    return redirect("/", [await sessionCookie(bypassLogin), clearCookie(STATE_COOKIE)]);
  }
  const state = randomHex(32);
  const authorize = new URL("https://github.com/login/oauth/authorize");
  authorize.searchParams.set("client_id", requireConfig("github_client_id"));
  authorize.searchParams.set("redirect_uri", callbackUrl(url));
  authorize.searchParams.set("state", state);
  authorize.searchParams.set("scope", "read:user");
  authorize.searchParams.set("allow_signup", "false");
  return redirect(authorize.toString(), [setCookie(STATE_COOKIE, state, STATE_TTL_SECONDS)]);
}

export function handleLogout(): Response {
  return redirect("/signed-out", [clearCookie(SESSION_COOKIE), clearCookie(STATE_COOKIE)]);
}

function denied(message: string, status = 403): Response {
  const response = errorPage("Sign-in failed", message, status);
  response.headers.append("set-cookie", clearCookie(STATE_COOKIE));
  return response;
}

export async function handleCallback(request: Request, url: URL): Promise<Response> {
  const givenState = url.searchParams.get("state") ?? "";
  const expectedState = readCookie(request, STATE_COOKIE) ?? "";
  if (expectedState.length !== 64 || !constantTimeEqual(givenState, expectedState)) {
    return denied("The sign-in state did not match. Start again from /login.", 400);
  }
  const code = url.searchParams.get("code");
  if (!code) return denied(url.searchParams.get("error_description") ?? "GitHub returned no code.", 400);

  const tokenResponse = await fetch("https://github.com/login/oauth/access_token", {
    method: "POST",
    backend: "github",
    cacheOverride: new CacheOverride("pass"),
    headers: {
      accept: "application/json",
      "content-type": "application/x-www-form-urlencoded",
      "user-agent": USER_AGENT,
    },
    body: new URLSearchParams({
      client_id: requireConfig("github_client_id"),
      client_secret: await requireSecret("github_client_secret"),
      code,
      redirect_uri: callbackUrl(url),
    }).toString(),
  });
  if (!tokenResponse.ok) return denied(`GitHub token exchange failed (${tokenResponse.status}).`, 502);
  const token = (await tokenResponse.json()) as { access_token?: string; error?: string };
  if (!token.access_token) return denied(`GitHub token exchange failed (${token.error ?? "no token"}).`, 502);

  const userResponse = await fetch("https://api.github.com/user", {
    backend: "github_api",
    cacheOverride: new CacheOverride("pass"),
    headers: {
      accept: "application/vnd.github+json",
      authorization: `Bearer ${token.access_token}`,
      "user-agent": USER_AGENT,
      "x-github-api-version": "2022-11-28",
    },
  });
  if (!userResponse.ok) return denied(`GitHub user lookup failed (${userResponse.status}).`, 502);
  const user = (await userResponse.json()) as { login?: unknown };
  if (typeof user.login !== "string") return denied("GitHub returned no login.", 502);

  if (!isAllowedLogin(user.login, requireConfig("allowed_logins"))) {
    return denied(`@${user.login} is not on this fleet's allowlist.`);
  }
  return redirect("/", [await sessionCookie(user.login), clearCookie(STATE_COOKIE)]);
}
