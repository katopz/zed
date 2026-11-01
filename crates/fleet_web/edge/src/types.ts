// Origin ⇄ edge contract (plan 051). Field names mirror the Rust origin and
// agent_mail's `RigProbe`; everything is optional-tolerant because the data
// crosses a process boundary and is untrusted.

export interface Gpu {
  name: string;
  util_pct: number;
  mem_used_gb: number;
  mem_total_gb: number;
}

export interface RigAgent {
  name: string;
  status: string;
  cwd?: string;
  idle_minutes?: number | null;
  last_paragraph?: string | null;
  wake_scheduled?: boolean;
  auto_prompt?: boolean;
}

/** agent_mail's `JobLine` is `{ id, text }`; the structured fields are optional extras. */
export interface JobLine {
  id: string;
  text?: string;
  cmd?: string;
  lane?: string;
  waited?: string;
  started?: string;
}

export interface RigProbe {
  rig: string;
  os?: string;
  at?: string;
  cores: number;
  load1?: number | null;
  mem_total_gb?: number | null;
  mem_free_gb?: number | null;
  disk_free_gb?: number | null;
  gpus?: Gpu[];
  agents?: RigAgent[];
  quiet: boolean;
  blind?: string | null;
  not_quiet_because?: string[];
  owner_mail?: number;
  running?: JobLine[];
  queued?: JobLine[];
}

export interface FleetRig {
  name: string;
  role?: string | null;
  ok: boolean;
  error?: string | null;
  latency_ms?: number;
  probe?: RigProbe | null;
}

export interface Fleet {
  at: string;
  self_rig?: string;
  rigs: FleetRig[];
}

export interface ThreadMeta {
  rig: string;
  id: string;
  title: string;
  updated_at: string | number;
  created_at?: string | number | null;
  parent_id?: string | null;
  folders: string[];
  model?: string | null;
  messages: number;
  live: boolean;
}

export interface ThreadIndex {
  at: string;
  threads: ThreadMeta[];
}

export type BlockKind = "text" | "thinking" | "tool_use" | "tool_result" | "image" | "mention";

export interface Block {
  kind: BlockKind;
  text: string;
  name?: string | null;
  status?: string | null;
}

export interface Entry {
  role: "user" | "agent" | "resume";
  blocks: Block[];
}

export interface TokenUsage {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

export interface ThreadView extends ThreadMeta {
  tokens?: TokenUsage | null;
  entries: Entry[];
}

/** Island bootstrap payload, embedded in the dashboard as inert JSON. */
export interface FleetState {
  fleet: Fleet | null;
  error: string | null;
  /** Unix ms the data was rendered against; keeps hydration markup identical. */
  now: number;
}
