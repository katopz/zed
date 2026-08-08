# 001 — Agent Board (multi-device / multi-agent notepad + shared state)

## Goal
A cross-device, cross-agent "notepad board" that mirrors the existing in-process
`auto_prompt::plan_registry` claims to/from Cloudflare KV, plus an append-only
short-message feed (last 10). Lets agents on the M3 laptop and the 4090 box (or
any two devices) see what the other is working on and reason about it during
`auto_prompt`. Single user (katopz); auth = ed25519 SSH-key signatures against a
pubkey allowlist.

**Phase 2 adjustment (operator spec):** The board now behaves like a **shared
thread history panel** — agents yell their last response / summary to the board
so other agents know what they're thinking or coding at that moment. Agents talk
to each other by posting to the board. Key posting moments:
- When starting a plan (agent announces what it's about to do)
- When summary occurs (auto_prompt already has summary detection via
  `truncate_last_paragraphs` + `SUMMARY_MARKERS` — hook here to post the summary)

Both **Claude agent** (claude-acp) and **native agent** (zed) see this board.

## Substrate-first note (CRITICAL)
`crates/auto_prompt/src/plan_registry.rs` ALREADY implements:
- claim / release / heartbeat / stale-reap (300s)
- `is_claimed_by_other(plan, session)`
- `format_claims_for_context(session)` → the exact "these plans are taken, don't
  pick them" string that `auto_prompt`'s orchestration LLM already consumes.

**This board is a network-backed MIRROR of plan_registry, not a parallel system.**
- Remote claims are injected INTO `plan_registry` via `try_claim` using a
  `remote:{device_id}:{session_id}` composite session id. Zero `auto_prompt` core
  changes required for the base context injection — the existing context builder
  picks them up.
- Local claims are read via `active_claims()` (filtering out `remote:` prefix) and
  POSTed to the worker so other devices can see them.

`crates/auto_prompt/src/claude_agent.rs` ALREADY has summary detection:
- `truncate_last_paragraphs` detects `## summary` / `# summary` / `summary:` /
  `tl;dr` headings (`SUMMARY_MARKERS`) and returns just the summary section.
- This is the hook point for Phase 2: when a summary is detected, post it to the
  board so peer agents can see what this agent concluded.

`crates/context_server/src/listener.rs` has `McpServer` + `McpServerTool`:
- Unix socket-based MCP server with `add_tool<T: McpServerTool>()`.
- Used for Phase 2 point 9: expose room data via MCP, default-on.

## Substrate inventory (from substrate-first skill)
| Concept | Exists as | Location |
|---|---|---|
| SSH key → device identity | `DeviceIdentity` | `agent_board/src/identity.rs` |
| Signed KV client | `BoardClient` | `agent_board/src/client.rs` |
| Room snapshot | `RoomSnapshot` | `agent_board/src/types.rs` |
| Plan claim sync | `feeder::sync_round` | `agent_board/src/feeder.rs` |
| Panel UI | `AgentBoardPanel` | `agent_board/src/agent_board.rs` |
| MCP server | `McpServer` | `context_server/src/listener.rs` |
| Summary detection | `truncate_last_paragraphs` + `SUMMARY_MARKERS` | `auto_prompt/src/claude_agent.rs` |
| Continue/stop decision | `decide_claude_with_hidden_thread` | `auto_prompt/src/claude_agent.rs` |

## Phase 1 — Base board (COMPLETED)
Original scope: mirror plan_registry claims + short message feed across devices.

### Worker (JS, Cloudflare)
- [x] `agent-board-worker/src/index.js` — routes + ed25519 verify (@noble/ed25519)
- [x] `agent-board-worker/wrangler.toml` — KV namespace `AGENT_BOARD`, 7d TTL
- [x] `agent-board-worker/package.json`
- [x] `agent-board-worker/README.md` — deploy + bootstrap-device steps

### Rust crate `agent_board`
- [x] Add workspace deps: `blake3`, `ssh-key`, `ed25519-dalek`, `sha2`
- [x] `crates/agent_board/Cargo.toml`
- [x] `crates/agent_board/src/types.rs` — Status, Scope, BoardMessage, RoomSnapshot
- [x] `crates/agent_board/src/identity.rs` — load ed25519 ssh key, device id, sign, location_hash
- [x] `crates/agent_board/src/client.rs` — http_client KV client (signed)
- [x] `crates/agent_board/src/feeder.rs` — poll → plan_registry injection + post local claims
- [x] `crates/agent_board/src/agent_board.rs` — panel, actions, settings, init, poll task
- [x] Register crate in root Cargo.toml + wire `agent_board::init()` into app

### Integration
- [x] Feeder injects remote claims as `try_claim(plan, "remote:{dev}:{sess}", summary)`
- [x] Feeder re-heartbeats on each poll; relies on plan_registry 300s GC for dead remotes
- [x] Panel shows: room name, active device statuses, last 10 messages

### Validation
- [x] `cargo clippy -p agent_board --all-targets` clean
- [x] `cargo test -p agent_board` — 4 tests pass
- [x] `cargo check -p zed` — app crate compiles
- [-] `npx wrangler dev` local smoke — deferred (wrangler not installed)
- [-] Manual cross-device claim visibility — deferred until worker deployed

### GOAT gate
- [x] Feature flag `agent-board` (default ON, individually disable-able)
- [-] Benchmark: feeder poll round-trip < 500ms — deferred (needs live worker)

---

## Phase 2 — Shared agent state (adjustment: operator spec, 10 points)

### Refined flow
1. **foo** (device A) with ssh-key auto-joins room = `hash(ssh-key)`.
2. **bar** (device B) with ssh-key auto-joins room = `hash(ssh-key)`.
3. **foo** can see all **bar** agents/sub-agents states as messages in chat.
4. **bar** can see all **foo** agents/sub-agents states as messages in chat.
5. **foo/bar** can select what to mute (per-agent, per-sub-agent, per-device).
6. All **unmuted** states go into the auto_prompt context.
7. Rooms contain only the **last 10 agent states** (ring buffer).
8. Each state ≤ **256 chars**, metadata ≤ **256 chars**.
9. Room data accessible via **MCP** (added by default, included in system prompt).
10. `claude-hidden-orchestrator` feature is **on by default** — no flag needed.

### Behavior: agents talk to each other via the board
The board behaves like a **shared thread history panel** — each agent yells its
last response / summary to the board so other agents know what it's thinking or
coding at that moment. Agents talk to each other by posting to the board.

**Posting trigger points:**
- **Plan start**: when an agent starts working on a plan, post
  `"starting: {plan_name} — {first_paragraph_of_plan}"` to the board.
- **Summary occurrence**: when auto_prompt detects a summary (via
  `truncate_last_paragraphs` + `SUMMARY_MARKERS`), post the summary to the board.
  This is the "what I just finished" signal.
- **Both Claude agent and native agent** post + read the board.

### What this phase reuses vs builds
- REUSE: `DeviceIdentity`, `BoardClient`, identity/room auth, MCP server infra,
  `truncate_last_paragraphs` summary detection.
- EXTEND: room derivation (manual name → `hash(ssh-key)`), state payload shape
  (plan claims → agent/sub-agent states), message cap (1024 → 256), MCP tool.
- BUILD NEW: chat message injection for agent states, muting UI + persistence,
  auto_prompt context integration for unmuted states, board-post hooks at plan
  start + summary occurrence.

### Tasks

#### P2.1 — Room = hash(ssh-key) (points 1-2)
- [x] `DeviceIdentity::room_id()` → `blake3(raw_ed25519_pubkey_32)` hex
      (same as device_id — same key = same room).
- [x] `AgentBoardConfig`: deprecate `room` field; derive from identity when empty.
- [x] `feeder::sync_round`: use derived room id (via panel's `resolved_room`).
- [x] The worker already keys by room name — the room id is just a string.

#### P2.2 — Agent state broadcasting (points 3-4)
- [x] Define `AgentStateMessage` (extends `BoardMessage`):
      `{ v, device_id, device_name, session_id, sub_agent_id?, state_text (≤256), meta (≤256), ts }`.
- [x] **Both Claude agent and native agent** post state to the board.
      - Claude: hook into `claude_decision_hidden` / `decide_claude_async` —
        when a summary is detected (`SUMMARY_MARKERS`), post it.
        (`maybe_broadcast_summary_to_board` in claude_agent.rs)
      - Native: hook into `auto_prompt::decide_with_llm` — same summary hook.
        (`peer_states::broadcast_state` call in Phase 2 branch of decide_with_llm)
- [x] **Plan start hook**: when auto_prompt detects a new plan file claim
      (`plan_registry::try_claim` via `auto_claim_plan`), post `"starting: {plan}"` to the board.
- [x] Ring buffer on the worker: only last 10 state messages returned.
      (`handleGetRoom` fetches `room:{room}:state:*`, sorts by `ts` desc, splices to `MAX_ROOM_STATES`)

#### P2.3 — Chat visibility (points 3-4)
- [x] When a remote `AgentStateMessage` arrives via the poll loop, inject it as
      a system/chat message into the active agent thread(s) on this device.
      (`auto_prompt::peer_states::drain_unseen_notifications` + `AgentPanel` foreground timer
      + `AcpThread::push_agent_board_notification`)
- [x] Format: `[peer] {device_name} / {agent_label}: {state_text}`
      (truncated to 256 chars).
- [x] Both Claude and native agent threads receive these injections.
      (Injected into the active `AcpThread`, which is the unified surface for both)

#### P2.4 — Muting (points 5-6)
- [x] `AgentBoardConfig` gains `muted: Vec<MuteKey>` where `MuteKey` is
      `{device_id?, session_id?, sub_agent_id?}`.
- [x] Panel UI: each agent state row has a mute toggle (🔊/🔇 click toggles).
- [x] Persist muted set to `~/.config/zed/agent_board.json` (via config save).
- [x] `feeder::sync_round` filters: only inject unmuted states into context.
      (Via `peer_states::set_muted` + `unmuted_states_for_context` filter)

#### P2.5 — Auto_prompt context integration (point 6)
- [x] `auto_prompt` exposes a new field on `LlmCallData`:
      `peer_agent_states: Option<String>` — formatted unmuted state text.
- [x] Both `claude_decision_hidden` AND the native `decide_with_llm` populate it
      from the board's latest unmuted state snapshot.
      (All 4 LlmCallData construction sites populate from peer_states)
- [x] The hidden-thread orchestrator prompt includes peer states so the judge
      can reason about what other agents are doing.
      (peer_agent_states flows into the hidden path's LlmCallData)

#### P2.6 — Bounds enforcement (points 7-8)
- [x] Worker: `AgentStateMessage.state_text` + `meta` capped at 256 bytes
      (char-boundary safe truncation via `truncateToByteBudget` in worker JS).
- [x] Worker: only last 10 state messages returned per room (ring buffer).
      (`handleGetRoom` sorts by `ts` desc, splices to `MAX_ROOM_STATES`)
- [x] Client-side: truncate before posting (defense in depth).
      `truncate_to_byte_budget` in types.rs + applied in BoardBroadcaster.

#### P2.7 — MCP server (point 9)
- [x] New MCP tool `get_agent_room` on a default-on `McpServer`:
      - Input: `{}` (no args — returns current room snapshot).
      - Output: `{ room, devices: [{device_id, device_name, states: [...]}] }`.
      (`GetAgentRoom` tool in `mcp_tools.rs`, registered on `McpServer` during `try_start`)
- [ ] Register the MCP server as a default context server so all agents
      (native + Claude Code) can call it.
      (Deferred — `McpServer` uses a Unix socket but `ContextServerConfiguration`
      only supports stdio/HTTP. Needs a transport bridge or new config type.)
- [ ] Include room summary in the system prompt:
      "You are in room X with N other agents. Their states: [last 10]."
      (Deferred — peer states already injected via `LlmCallData.peer_agent_states`;
      MCP tool provides on-demand query as a complement.)

### Perf/sec considerations
- **Poll cadence**: 15s default is the floor for KV eventual consistency. Agent
  states lag by up to 15s — acceptable for "what are others doing" awareness,
  NOT for real-time coordination. Don't reduce below 5s (KV write rate limits).
- **Post frequency**: posting at plan-start + summary-occurrence is 2-5 posts
  per plan lifecycle. This is low-volume — no risk of flooding the board or
  hitting KV write limits.
- **Chat injection cost**: injecting a remote state message is a single
  `push_entry` call (O(1) append to the entries vec). No parsing, no LLM call.
  The cost is only paid when a new remote state arrives (every ~15s at most).
- **Context bloat**: unmuted states enter auto_prompt context. With 256 char cap
  × 10 states = max 2,560 chars of peer context. This is well under the 4,000 char
  budget for `LAST_MESSAGE_BUDGET_CHARS`. No token-count risk.
- **MCP socket lifecycle**: `McpServer` holds a `tempdir` that cleans up on drop.
  No leak risk. The Unix socket is per-app-session.
- **No background polling for MCP**: the MCP tool is request-response. The agent
  calls `get_agent_room` when it wants fresh data. No persistent loop.
- **Both agent types**: Claude Code threads and native Zed agent threads both
  access the board via the same `AgentBoardPanel` singleton. No duplication.
- **KV read amplification**: each poll reads the full room snapshot (10 states +
  10 messages). At 256 chars each, that's ~5KB per poll — trivial.

### Risks
- **Chat noise**: injecting remote agent states into chat could be noisy.
  Mitigated by muting + 256 char cap + last-10 ring buffer.
- **KV eventual consistency**: up to ~60s global propagation. Poll, don't treat
  as real-time. Heartbeat reaper handles staleness.
- **Privacy**: agent states include task summaries. All devices in a room share
  the same ssh-key → single-user multi-device → no privacy concern.

### Dependency direction
```
agent_ui ─→ auto_prompt ─→ (hidden orchestrator, plan_registry)
    │              │
    │              ▼ peer_agent_states (injected BY agent_board)
    ├────→ agent_board ──→ (identity, client, feeder, MCP)
    │              │
    │              ▼
    └────→ context_server (McpServer)
```
`auto_prompt` gains a `peer_agent_states` field populated BY `agent_board`.
`agent_board` reads auto_prompt's `LlmCallData` shape, not vice versa.

### GOAT gate (Phase 2)
- [ ] Two devices with same ssh-key auto-join same room.
- [x] Agent states visible in chat on both devices.
      (`AgentThreadEntry::AgentBoardNotification` injected by `AgentPanel` foreground timer;
      `drain_unseen_notifications` dedupes by signature so heartbeats don't re-fire)
- [ ] Muting works: muted states don't appear in chat or context.
- [ ] MCP tool returns room data.
- [ ] Both Claude + native agent post + read the board.
- [ ] Plan-start + summary-occurrence posts fire correctly.
- [x] 256 char + last-10 bounds enforced.
      (Client-side: `truncate_to_byte_budget` + `MAX_STATE_TEXT_BYTES`. Worker-side:
      `truncateToByteBudget` + `MAX_ROOM_STATES` ring buffer in `handleGetRoom` +
      `handlePostState`. The worker now also serves the `POST /state` endpoint
      that was previously missing — state broadcasts were silently 404'ing before.)
