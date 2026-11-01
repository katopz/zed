// Shared by SSR and the client island, so it must stay free of fastly imports.
import type { FleetRig, FleetState, JobLine, RigAgent } from "../types.ts";
import { clip, fixed, relativeTime } from "./format.ts";

type RigState = "QUIET" | "BUSY" | "OFFLINE";

function rigState(rig: FleetRig): RigState {
  if (!rig.ok || !rig.probe) return "OFFLINE";
  return rig.probe.quiet ? "QUIET" : "BUSY";
}

function JobRow({ job, kind }: { job: JobLine; kind: "running" | "queued" }) {
  const timing = kind === "running" ? job.started : job.waited;
  return (
    <li class="job">
      <span class="mono dim">{job.id}</span>
      {job.lane && <span class="tag">{job.lane}</span>}
      <span class="mono job-cmd">{clip(job.cmd ?? job.text ?? "", 160)}</span>
      {timing && <span class="dim">{kind === "running" ? `started ${timing}` : `waited ${timing}`}</span>}
    </li>
  );
}

function AgentRow({ agent }: { agent: RigAgent }) {
  return (
    <li class="agent">
      <div class="agent-head">
        <span class="agent-name">{agent.name}</span>
        <span class={`tag status-${agent.status.toLowerCase().replace(/[^a-z]/g, "")}`}>{agent.status}</span>
        {typeof agent.idle_minutes === "number" && <span class="dim">idle {Math.round(agent.idle_minutes)}m</span>}
        {agent.auto_prompt && <span class="tag accent">auto_prompt</span>}
      </div>
      {agent.last_paragraph && <div class="agent-last">{clip(agent.last_paragraph, 240)}</div>}
    </li>
  );
}

function RigCard({ rig }: { rig: FleetRig }) {
  const state = rigState(rig);
  const probe = rig.probe;
  return (
    <article class={`card rig rig-${state.toLowerCase()}`}>
      <header class="rig-head">
        <h3>{rig.name}</h3>
        {rig.role && <span class="dim">{rig.role}</span>}
        <span class={`badge badge-${state.toLowerCase()}`}>{state}</span>
      </header>
      {!probe ? (
        <p class="dim">{rig.error ? clip(rig.error, 200) : "no probe"}</p>
      ) : (
        <>
          <dl class="stats">
            <div>
              <dt>load</dt>
              <dd>
                {fixed(probe.load1)} / {probe.cores}
              </dd>
            </div>
            <div>
              <dt>free mem</dt>
              <dd>{fixed(probe.mem_free_gb)} GB</dd>
            </div>
            {(probe.gpus ?? []).map((gpu) => (
              <div key={gpu.name}>
                <dt title={gpu.name}>gpu</dt>
                <dd>{fixed(gpu.util_pct, 0)}%</dd>
              </div>
            ))}
            {(probe.owner_mail ?? 0) > 0 && (
              <div>
                <dt>owner mail</dt>
                <dd class="warn">{probe.owner_mail}</dd>
              </div>
            )}
          </dl>
          {probe.blind && <p class="warn">blind: {probe.blind}</p>}
          {(probe.not_quiet_because ?? []).length > 0 && (
            <ul class="reasons">
              {(probe.not_quiet_because ?? []).map((reason, index) => (
                <li key={index}>{clip(reason, 160)}</li>
              ))}
            </ul>
          )}
          {(probe.running ?? []).length > 0 && (
            <section>
              <h4>running</h4>
              <ul class="jobs">
                {(probe.running ?? []).map((job) => (
                  <JobRow key={job.id} job={job} kind="running" />
                ))}
              </ul>
            </section>
          )}
          {(probe.queued ?? []).length > 0 && (
            <section>
              <h4>queued</h4>
              <ul class="jobs">
                {(probe.queued ?? []).map((job) => (
                  <JobRow key={job.id} job={job} kind="queued" />
                ))}
              </ul>
            </section>
          )}
          {(probe.agents ?? []).length > 0 && (
            <section>
              <h4>agents</h4>
              <ul class="agents">
                {(probe.agents ?? []).map((agent) => (
                  <AgentRow key={agent.name} agent={agent} />
                ))}
              </ul>
            </section>
          )}
        </>
      )}
    </article>
  );
}

export function RigsSection({ state }: { state: FleetState }) {
  const { fleet, error, now } = state;
  return (
    <>
      <div class="section-head">
        <h2>Rigs</h2>
        {fleet && <span class="dim">as of {relativeTime(fleet.at, now)}</span>}
      </div>
      {error && <div class="banner">Origin offline: {error}. Showing {fleet ? "the last data" : "nothing yet"}.</div>}
      {fleet && (
        <div class="rig-grid">
          {fleet.rigs.map((rig) => (
            <RigCard key={rig.name} rig={rig} />
          ))}
        </div>
      )}
    </>
  );
}
