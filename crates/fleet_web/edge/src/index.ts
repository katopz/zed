/// <reference types="@fastly/js-compute" />
import { handleCallback, handleLogin, handleLogout } from "./auth/github.ts";
import { SESSION_COOKIE, isAllowedLogin, verifySession } from "./auth/session.ts";
import { ConfigError, configValue, devLogin, secretKey } from "./config.ts";
import { json, readCookie, redirect, wantsHtml, withSecurityHeaders } from "./http.ts";
import { apiFleet, appScript, dashboard, thread } from "./routes.tsx";
import { errorPage, signedOutPage } from "./views/pages.tsx";

addEventListener("fetch", (event) => event.respondWith(handle(event.request)));

async function handle(request: Request): Promise<Response> {
  try {
    return withSecurityHeaders(await route(request));
  } catch (error) {
    console.error(`fleet-web: ${(error as Error).stack ?? error}`);
    const message = error instanceof ConfigError ? "The edge is misconfigured; check the config and secret stores." : "Unexpected error.";
    return withSecurityHeaders(errorPage("Error", message, 500));
  }
}

/** The signed-in login, re-checked against the allowlist so removals apply at once. */
async function currentLogin(request: Request, url: URL): Promise<string | null> {
  const cookie = readCookie(request, SESSION_COOKIE);
  if (!cookie) return null;
  const login = verifySession(await secretKey("session_key"), cookie);
  if (login === null) return null;
  const bypassLogin = devLogin(url);
  if (bypassLogin !== null && bypassLogin.toLowerCase() === login.toLowerCase()) return login;
  return isAllowedLogin(login, configValue("allowed_logins") ?? "") ? login : null;
}

async function route(request: Request): Promise<Response> {
  if (request.method !== "GET" && request.method !== "HEAD") {
    return new Response("method not allowed", { status: 405, headers: { allow: "GET, HEAD" } });
  }
  const url = new URL(request.url);
  const path = url.pathname;

  switch (path) {
    case "/login":
      return handleLogin(url);
    case "/auth/callback":
      return handleCallback(request, url);
    case "/logout":
      return handleLogout();
    case "/signed-out":
      return signedOutPage();
    case "/favicon.ico":
      return new Response(null, { status: 204 });
  }

  const login = await currentLogin(request, url);
  if (login === null) {
    return wantsHtml(request, path) ? redirect("/login") : json({ error: "unauthorized" }, 401);
  }

  if (path === "/") return dashboard(login);
  if (path === "/api/fleet") return apiFleet();
  if (path === "/static/app.js") return appScript();
  const threadMatch = /^\/t\/([^/]+)\/([^/]+)\/?$/.exec(path);
  if (threadMatch) return thread(login, threadMatch[1], threadMatch[2]);
  return path.startsWith("/api/") ? json({ error: "not found" }, 404) : errorPage("Not found", "No such page.", 404, login);
}
