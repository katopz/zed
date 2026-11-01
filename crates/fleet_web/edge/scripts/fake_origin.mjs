// Smoke-test origin on 127.0.0.1:8787 serving contract-shaped sample JSON.
// It does NOT check signatures; it only logs whether they were sent.
import { createServer } from "node:http";

const port = Number(process.env.PORT ?? 8787);
const now = new Date();
const ago = (minutes) => new Date(now.getTime() - minutes * 60_000).toISOString();

const fleet = {
  at: now.toISOString(),
  self_rig: "amo",
  rigs: [
    {
      name: "amo", role: "builder", ok: true, latency_ms: 12,
      probe: {
        rig: "amo", os: "macos", at: now.toISOString(), cores: 28, load1: 9.4, mem_total_gb: 96, mem_free_gb: 41.2,
        gpus: [], quiet: false, not_quiet_because: ["cargo (cpu 340%)"], owner_mail: 2,
        agents: [{ name: "fleet-web", status: "running", cwd: "/Users/kat/git/zed", idle_minutes: 0.4, auto_prompt: true, last_paragraph: "Building the origin <b>now</b>." }],
        running: [{ id: "j-17", text: "cargo test -p fleet_web (shared, started 3m ago)" }],
        queued: [{ id: "j-18", cmd: "cargo bench", lane: "exclusive", waited: "1m" }],
      },
    },
    { name: "4090", role: "cuda", ok: true, latency_ms: 80, probe: { rig: "4090", cores: 32, load1: 0.5, mem_free_gb: 50, gpus: [{ name: "RTX 4090", util_pct: 97, mem_used_gb: 20, mem_total_gb: 24 }], quiet: true, agents: [], running: [], queued: [] } },
    { name: "bmo", role: "runner", ok: false, error: "ssh: connect timed out", latency_ms: 5000 },
  ],
};

const threads = [
  { rig: "m3", id: "0195f0e2-7a1c-7b3e-9d2a-1f2e3d4c5b6a", title: "Fleet web edge app", updated_at: ago(1), folders: ["/Users/kat/git/zed"], model: "claude-opus-5-5", messages: 12, live: true },
  { rig: "m3", id: "0195f0e2-0000-7000-8000-000000000002", title: "Old <script>alert(1)</script> thread", updated_at: ago(300), folders: ["/Users/kat/git/riir-ai"], model: null, messages: 3, live: false },
  { rig: "amo", id: "0195f0e2-0000-7000-8000-000000000003", title: "Bench rematch", updated_at: ago(20), folders: [], model: "sonnet", messages: 40, live: false },
];

const view = {
  ...threads[0],
  tokens: { input: 123456, output: 7890, cache_read: 100000, cache_write: 2000 },
  entries: [
    { role: "user", blocks: [{ kind: "text", text: "Build the **edge** app <script>alert('xss')</script>" }, { kind: "mention", text: "plan 051", name: "051_fleet_web_fastly.md" }] },
    { role: "agent", blocks: [
      { kind: "thinking", text: "Reading the plan first." },
      { kind: "text", text: "Here is the plan:\n\n- one\n- two\n\n```ts\nconst x = 1;\n```\n[bad](javascript:alert(1))" },
      { kind: "tool_use", name: "Bash", status: "completed", text: "{\"command\": \"ls\"}" },
      { kind: "tool_result", name: "Bash", text: "<img src=x onerror=alert(1)>\nfile.txt" },
    ] },
    { role: "resume", blocks: [] },
    { role: "agent", blocks: [{ kind: "text", text: "Done." }] },
  ],
};

createServer((request, response) => {
  const signed = Boolean(request.headers["x-fleet-ts"] && request.headers["x-fleet-sig"]);
  console.log(`${request.method} ${request.url} signed=${signed}`);
  const send = (status, body) => {
    response.writeHead(status, { "content-type": "application/json" });
    response.end(JSON.stringify(body));
  };
  const match = /^\/api\/threads\/([^/]+)\/([^/]+)$/.exec(request.url ?? "");
  if (request.url === "/api/fleet") return send(200, fleet);
  if (request.url === "/api/threads") return send(200, { at: now.toISOString(), threads });
  if (match) {
    const found = threads.find((thread) => thread.rig === match[1] && thread.id === match[2]);
    return found ? send(200, { ...view, ...found }) : send(404, { error: "not found" });
  }
  send(404, { error: "not found" });
}).listen(port, "127.0.0.1", () => console.log(`fake origin on http://127.0.0.1:${port}`));
