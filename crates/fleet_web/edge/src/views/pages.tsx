import { html } from "../http.ts";
import type { FleetState, ThreadMeta, ThreadView } from "../types.ts";
import { inertJson, renderDocument } from "./layout.tsx";
import { RigsSection } from "./rigs.tsx";
import { ThreadsSection } from "./threads.tsx";
import { ThreadPage } from "./thread.tsx";

export interface DashboardData {
  login: string;
  fleetState: FleetState;
  threads: ThreadMeta[] | null;
  threadsError: string | null;
  scriptSrc: string;
}

export function dashboardPage(data: DashboardData): Response {
  const body = renderDocument({
    title: "Fleet",
    login: data.login,
    scriptSrc: data.scriptSrc,
    children: (
      <>
        <section id="rigs">
          <RigsSection state={data.fleetState} />
        </section>
        <script type="application/json" id="fleet-data" dangerouslySetInnerHTML={{ __html: inertJson(data.fleetState) }} />
        <ThreadsSection threads={data.threads} error={data.threadsError} now={data.fleetState.now} />
      </>
    ),
  });
  return html(body);
}

export function threadPage(login: string, thread: ThreadView, now: number): Response {
  const body = renderDocument({
    title: `${thread.title || "Untitled"} · ${thread.rig}`,
    login,
    children: <ThreadPage thread={thread} now={now} />,
  });
  return html(body);
}

export function errorPage(title: string, message: string, status: number, login: string | null = null): Response {
  const body = renderDocument({
    title,
    login,
    children: (
      <div class="center">
        <h2>{title}</h2>
        <p class="dim">{message}</p>
        <p>
          <a href="/">Back to the fleet</a>
        </p>
      </div>
    ),
  });
  return html(body, status);
}

export function signedOutPage(): Response {
  const body = renderDocument({
    title: "Signed out",
    children: (
      <div class="center">
        <h2>Signed out</h2>
        <p>
          <a href="/login">Sign in with GitHub</a>
        </p>
      </div>
    ),
  });
  return html(body);
}
