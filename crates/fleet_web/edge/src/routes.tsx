import { APP_HASH, APP_JS } from "virtual:app-asset";
import { json } from "./http.ts";
import { ORIGIN_TTL, originGet } from "./origin.ts";
import type { Fleet, FleetState, ThreadIndex, ThreadView } from "./types.ts";
import { threadApiPath } from "./validate.ts";
import { dashboardPage, errorPage, threadPage } from "./views/pages.tsx";

export const APP_SCRIPT_SRC = `/static/app.js?v=${APP_HASH}`;

export async function dashboard(login: string): Promise<Response> {
  const [fleet, threads] = await Promise.all([
    originGet<Fleet>("/api/fleet", ORIGIN_TTL.fleet),
    originGet<ThreadIndex>("/api/threads", ORIGIN_TTL.threads),
  ]);
  const fleetState: FleetState = {
    fleet: fleet.ok ? fleet.data : null,
    error: fleet.ok ? null : fleet.error,
    now: Date.now(),
  };
  return dashboardPage({
    login,
    fleetState,
    threads: threads.ok ? (threads.data.threads ?? []) : null,
    threadsError: threads.ok ? null : threads.error,
    scriptSrc: APP_SCRIPT_SRC,
  });
}

export async function thread(login: string, rig: string, id: string): Promise<Response> {
  const path = threadApiPath(rig, id);
  if (path === null) return errorPage("Not found", "That is not a valid thread address.", 404, login);
  const result = await originGet<ThreadView>(path, ORIGIN_TTL.thread);
  if (result.ok) return threadPage(login, result.data, Date.now());
  if (result.status === 404) return errorPage("Not found", `No thread ${id} on ${rig}.`, 404, login);
  return errorPage("Origin offline", `Could not load the thread: ${result.error}.`, 502, login);
}

export async function apiFleet(): Promise<Response> {
  const result = await originGet<Fleet>("/api/fleet", ORIGIN_TTL.fleet);
  if (result.ok) return json(result.data);
  // Any origin failure is a gateway error to the browser; a 401 here would read as an expired session.
  return json({ error: result.error }, 502);
}

export function appScript(): Response {
  return new Response(APP_JS, {
    headers: {
      "content-type": "text/javascript; charset=utf-8",
      "cache-control": "private, max-age=31536000, immutable",
      etag: `"${APP_HASH}"`,
    },
  });
}
