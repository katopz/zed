// Client bundle: hydrates only the Rigs section as a signal island, plus a
// plain DOM filter over the server-rendered thread list.
import { signal } from "@preact/signals";
import { hydrate } from "preact";
import type { Fleet, FleetState } from "../types.ts";
import { RigsSection } from "../views/rigs.tsx";

const REFRESH_MS = 15_000;

function startRigsIsland(): void {
  const root = document.getElementById("rigs");
  const bootstrap = document.getElementById("fleet-data");
  if (!root || !bootstrap) return;
  const initial = JSON.parse(bootstrap.textContent ?? "null") as FleetState | null;
  if (!initial) return;

  const state = signal<FleetState>(initial);
  let lastFetch = Date.now();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let inFlight = false;
  let stopped = false;

  function RigsIsland() {
    return <RigsSection state={state.value} />;
  }

  async function refresh(): Promise<void> {
    if (inFlight || stopped) return;
    inFlight = true;
    lastFetch = Date.now();
    try {
      const response = await fetch("/api/fleet", { credentials: "same-origin", headers: { accept: "application/json" } });
      if (response.status === 401) {
        stopped = true;
        state.value = { ...state.value, error: "signed out, reload to sign in", now: Date.now() };
        return;
      }
      const body = (await response.json()) as Fleet | { error: string };
      state.value =
        "rigs" in body
          ? { fleet: body, error: null, now: Date.now() }
          : { ...state.value, error: body.error, now: Date.now() };
    } catch (error) {
      state.value = { ...state.value, error: (error as Error).message, now: Date.now() };
    } finally {
      inFlight = false;
      schedule();
    }
  }

  function schedule(): void {
    clearTimeout(timer);
    if (stopped || document.visibilityState !== "visible") return;
    timer = setTimeout(refresh, Math.max(0, REFRESH_MS - (Date.now() - lastFetch)));
  }

  document.addEventListener("visibilitychange", schedule);
  hydrate(<RigsIsland />, root);
  schedule();
}

function startThreadFilter(): void {
  const input = document.getElementById("thread-filter") as HTMLInputElement | null;
  if (!input) return;
  const apply = () => {
    const terms = input.value.toLowerCase().split(/\s+/).filter((term) => term.length > 0);
    for (const group of document.querySelectorAll<HTMLElement>(".thread-group")) {
      let visible = 0;
      for (const row of group.querySelectorAll<HTMLElement>(".thread-row")) {
        const haystack = row.dataset.search ?? "";
        const match = terms.every((term) => haystack.includes(term));
        row.hidden = !match;
        if (match) visible++;
      }
      group.hidden = visible === 0;
    }
  };
  input.addEventListener("input", apply);
  apply();
}

startRigsIsland();
startThreadFilter();
