# 024 — War Room Panel (conversational agent board in Zed)

## Goal

Pivot the agent board from a **static status board** (Plan 013/015) to a
**conversational war room**: a new Collab-Panel-shaped dock panel in Zed where
the operator AND agents chat in one shared room feed. Anyone (operator at a
Zed window, agent via MCP, operator on a phone via the Plan-015 web UI with
**no local agents running**) can post messages and **@mention-tag any agent**
to command it. Agents can ask/answer/command **each other** the same way.

Success looks like:

```
war room feed (Zed panel + web UI, same data):
  katopz@web      : @SHIKUWA:b1c9 run cargo clippy before you commit
  SHIKUWA:b1c9    : ▶ state: running cargo clippy (3 warnings)
  m3:f3a2 (agent) : @SHIKUWA:b1c9 your patch broke my test — mind rebasing?
  katopz@m3       : both of you stop after the current step
```

- New activity-bar icon **directly behind Collab Panel** (`UserGroup` icon,
  activation_priority 6).
- Plan 015 web UI stays the optional mobility client — it needs no changes to
  the worker contract, only a chat input that emits the same @mention syntax.
- Lean: no new crates, no new worker endpoints, no new polling loops. One
  process-global board runtime (extracted from the existing panel), everything
  else is pure routing on top of existing substrate.

## Relationship to 013 / 015

| Plan | What it shipped | 024 builds on it |
|---|---|---|
| 013 | KV board: rooms, statuses, states, 15s feeder, mute | runtime + feed reused as-is |
| 015 | Web UI, SSE realtime, `/reply` steering, session-prefix routing | mention routing reuses the exact reply-injection pipeline |

The static `AgentBoardPanel` (013) stays for at-a-glance status; the war room
is the conversational surface. Both become views over one shared runtime
(today the poll loop is **owned by the panel entity** — adding a second panel
without refactor would double-poll; see P0).

## Substrate-first check (substrate-first skill)

Searched vocabulary: `panel / dock / activation_priority / add_panel`,
`CollabPanel / initialize_panels`, `chat / channel / message`,
`mention / at-sign / REPLY / tag`, `agent_board / board_state / feeder /
realtime_client / mcp_tools`, `inject_web_reply / drain_web_replies /
thread_for_session_prefix / push_agent_board_notification / AcpThread::send`.

| Concept | Exists as | Location | Verdict |
|---|---|---|---|
| Dock panel registration | `Panel` trait + `add_panel` + `initialize_panels` | `workspace/src/dock.rs`, `zed/src/zed.rs` | consume |
| Signed board client | `BoardClient` (`post_message`, `post_reply`, `get_room`) | `agent_board/src/client.rs` | consume |
| Poll + reply drain | `feeder::sync_round`, `extract_replies_for_device` | `agent_board/src/feeder.rs` | extend (mention scan) |
| Global snapshot cache | `board_state::{set,current}_room_snapshot`, `register_writer` | `agent_board/src/board_state.rs` | consume/extend |
| Realtime push | `RealtimeClient` (SSE) | `agent_board/src/realtime_client.rs` | consume (moves into runtime) |
| Session-prefix injection | `inject_web_reply` → `AgentPanel::start_notification_drain` → `thread_for_session_prefix` → `AcpThread::send` | `auto_prompt/src/peer_states.rs`, `agent_ui/src/agent_panel.rs` | consume — mentions reuse this exact pipeline |
| Agent read tool | `get_agent_room` MCP tool | `agent_board/src/mcp_tools.rs` | extend (add write tool) |
| Mute/gate | `MuteKey` + `PeerStateMute` | `agent_board` + `auto_prompt` | consume (loop-guard backstop) |

Decision: **consume + extend**. No parallel system. The only new substrate is
(a) a pure mention parser, (b) a mention-scan step inside the existing sync
round, (c) a per-session cooldown guard, (d) the runtime extraction (which is
a DRY fix, not new functionality).

Architectural rules checked: no game-domain code involved; sync boundary n/a
(this is operator tooling); worker writes remain ed25519-signed by devices;
web writes remain GitHub-allowlisted. Clean.

## Architecture

```
┌───────────────────────── Zed process (one per DEVICE) ──────────────────────┐
│                                                                              │
│  agent_board::runtime::BoardRuntime  (Entity, GPUI Global, SINGLETON)        │
│  - owns: BoardClient, DeviceIdentity, room, 15s poll task, SSE client,       │
│    MCP server (get_agent_room + post_agent_board_message)                    │
│  - started once at init when worker_url configured (same gate as try_start)  │
│  - each sync_round:                                                          │
│      1. GET room snapshot (existing)                                         │
│      2. drain replies  → inject_web_reply        (existing, Plan 015)        │
│      3. NEW: scan new messages (ts watermark) for `@mydevice:xxxx` mentions  │
│         → cooldown/rate guard → inject_web_reply  (same pipeline as 2)       │
│      4. set_room_snapshot + cx.notify  → panels re-render                    │
│                                                                              │
│  Views (observe runtime, no network ownership):                              │
│  - AgentBoardPanel (013, refactored to view)                                 │
│  - WarRoomPanel (NEW): roster + feed + @mention input + 📡 toggle            │
│                                                                              │
│  Delivery to agent threads: EXISTING 10s AgentPanel drain:                   │
│  drain_web_replies → thread_for_session_prefix → AcpThread::send             │
└──────────────┬───────────────────────────────────────────────┬───────────────┘
               │ ed25519-signed POST /msg /reply /state        │ SSE push
               ▼                                               ▲
┌──────────────────────────────────────────────────────────────────────────────┐
│ Cloudflare Worker (Plan 015, UNCHANGED contract)                              │
│ - POST /msg already exists — mention text is just text in the feed           │
│ - KV ring buffer MAX_MESSAGES 10 → 100 (chat needs a backlog)                │
│ - SSE/DO relay broadcasts messages → war room is realtime when 📡 on         │
└──────────────┬───────────────────────────────────────────────────────────────┘
               │ GitHub-allowlisted (015 W5; needs GITHUB_CLIENT_ID)
               ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│ Web UI (Plan 015, optional mobility)                                          │
│ - NEW: general chat input posting /msg (plain chat + `@device:sess4` command)│
│ - REPLY:[device:sess4] stays as the private-steering alias                   │
│ - operator on a phone, ZERO agents running locally, can command any agent    │
│   owned by any signed-in device — the OWNING device performs injection        │
└──────────────────────────────────────────────────────────────────────────────┘
```

Key invariant: **the device that owns the target session performs the
injection.** Every other participant (web user, remote operator, peer agent)
only writes text to the feed; routing is self-selecting per device. This is
what makes "no local agents" work for free.

## Mention protocol

```
@<device_name>:<session_prefix4> <text>        → public command (shows in feed)
@all <text>                                    → broadcast to every agent on every device (v1.1, defer)
REPLY:[<device>:<sess4>] <text>                → private steering (Plan 015, unchanged)
```

- Parse rule (v1): a message **routes** iff its first token matches
  `^@([\w-]+):([0-9a-zA-Z]{4})\b`. Mentions mid-text are display-highlighted
  only (no injection) — keeps routing deterministic and parse O(1).
- Prefix resolution reuses 015's exact machinery (`starts_with` +
  collision warning; 65,536 prefixes, negligible collision at ≤5 agents).
- Injected message format into the target thread:
  `📢 war-room [@{sender_label}] {text}` — mirrors the existing
  `🌐 REPLY:[{prefix}] {text}` shape so W11-style inline rendering works.
- Sender labeling: `BoardMessage`/`PostMessageBody` gain
  `#[serde(default)] sender: String` (`"operator"`, `"web"`, or the posting
  agent's `device:sess4`). Old snapshots deserialize unchanged (serde default).

## Loop guard (agent ↔ agent commands)

Two agents commanding each other can ping-pong forever (token burn). Guards,
checked at injection time in the runtime, before `inject_web_reply`:

1. **Cooldown**: per target session, min `mention_cooldown_secs` between
   mention-injections (default 60s, `AgentBoardConfig` field).
2. **Rate cap**: max `mention_max_per_hour` per session (default 20).
3. **Self-mention**: an agent mentioning its own session is dropped (log).
4. Backstop: the existing 013 mute system (`Muted` in config) lets the
   operator kill any noisy agent instantly.

Throttled mentions are logged (`warn`) and still visible in the feed — the
operator sees the storm even when injection is suppressed.

## Panel design (WarRoomPanel)

- Crate/module: `agent_board/src/war_room.rs` (no new crate). Actions in a
  `war_room` namespace (`Toggle`, `ToggleFocus`, `Refresh`).
- Left dock, `default_size` 320px, `starts_open` false, key_context
  `"WarRoom"`, `panel_key`/`persistent_name` `"WarRoomPanel"`.
- Icon: `IconName::UserGroup` (already exists in `crates/icons` — zero new
  assets). Tooltip: `"War Room"`.
- **activation_priority 6** — directly behind CollabPanel (5). Requires the
  duplicate-priority fix below.
- Added eagerly in `zed.rs initialize_panels` so the icon is always present.
- Layout (Collab-Panel-shaped):
  - Header: room name, connection state, 📡 toggle (binds the shared runtime,
    replaces the per-panel one), refresh.
  - Roster (contacts-like): devices → agents (`device:sess4` + state_text).
    Click an agent → input pre-filled with `@device:sess4 `.
  - Feed (channel-like): last N messages (N = worker cap, 100), own messages
    accented, `@mentions` highlighted, agent-state changes rendered as dim
    event lines interleaved by ts (conversational feel for free — states
    already broadcast).
  - Input bar: single-line `Editor` (CollabPanel channel-editor pattern),
    Enter = send → `POST /msg`. No inline autocomplete in v1 (roster click
    covers it); defer.
- Local-only mode when unconfigured: roster empty, feed shows the existing
  "not connected (local-only)" hint (mirrors AgentBoardPanel).

### activation_priority collision (why two one-line upstream diffs)

`Dock::add_panel` **panics in debug builds** on duplicate priorities. Current
left-dock priorities: Project 1, Terminal 2, Git 3, Collab 5, Outline 6,
Debug 7, AgentBoard 10. There is no free integer between 5 and 6, so:

- `outline_panel.rs`: 6 → **7**
- `debugger_panel.rs`: 7 → **8**
- `war_room.rs`: **6**

Resulting order (deterministic in every dock configuration): Collab 5 →
**WarRoom 6** → Outline 7 → Debug 8 → AgentBoard 10.

Fallback if the renumber is rejected: priority 8/9 (behind Debug, still behind
Collab but not adjacent) — zero upstream-file churn, but not the requested
"directly behind Collab".

## Tasks

### P0 — Runtime singleton (DRY + perf precondition)

- [ ] `agent_board/src/runtime.rs`: `BoardRuntime` entity (GPUI Global):
      config load, identity, client, room resolution, poll task, SSE client,
      MCP server — moved verbatim from `AgentBoardPanel::{try_start,
      start_poll, start_realtime}` (net code motion, no behavior change).
- [ ] `agent_board::init` starts the runtime when `worker_url` is configured;
      runtime `cx.notify()` on each snapshot; `board_state` globals unchanged.
- [ ] Refactor `AgentBoardPanel` to a pure view over the runtime (drop its
      owned poll/SSE/MCP fields; keep actions/mute UI). Its Toggle action
      must not create a second runtime.
- [ ] Verify single poll loop with both panels open (log line or counter).

### P1 — Mention routing

- [ ] `agent_board/src/mentions.rs`: pure `parse_mention(&str) ->
      Option<Mention{device, prefix, text}>` + `sender_label()` helpers.
- [ ] `feeder.rs`: `extract_mentions_for_device(&snapshot, device_name,
      watermark) -> Vec<(prefix, text)>` (mirror of
      `extract_replies_for_device`), wired into `sync_round` step 3; ts
      high-water mark persisted in the runtime (in-memory; re-inject risk
      after restart bounded by cooldown — acceptable, documented).
- [ ] Cooldown + rate-cap guard (pure fn `mention_guard`) + config fields
      `mention_cooldown_secs: u64 = 60`, `mention_max_per_hour: u32 = 20`.
- [ ] Injection format `📢 war-room [@sender] text` via existing
      `inject_web_reply` (no new injection path).

### P2 — Worker + wire (small, backward compatible)

- [ ] `MAX_MESSAGES` 10 → 100 (`index.js` const + `wrangler.toml` var).
- [ ] `BoardMessage`/`PostMessageBody` gain `#[serde(default)] sender`;
      worker passes it through on `/msg`; old payloads default `""`.

### P3 — WarRoomPanel

- [ ] `war_room.rs`: panel entity + render (header / roster / feed / input)
      per the design above; local-only state; 📡 toggle bound to runtime.
- [ ] `actions!(war_room, [Toggle, ToggleFocus, Refresh])` +
      `workspace.register_action` wiring in `agent_board::init`.
- [ ] `zed.rs initialize_panels`: eager add (`WarRoomPanel::new` is cheap —
      no I/O; all network lives in the runtime).
- [ ] Priority renumber: Outline 6→7, Debug 7→8; WarRoom = 6.
- [ ] Icon `UserGroup`, tooltip "War Room", `icon_label` = pending-mention
      count when panel closed (cheap: unwatched-mention counter in runtime,
      cleared on open).

### P4 — Agent voice (MCP write tool)

- [ ] `board_state::post_message(text, sender)` writer fn (same shape as the
      broadcaster: clone handle, spawn, log-fail).
- [ ] MCP tool `post_agent_board_message { text }` (`mcp_tools.rs`, default
      annotations: not read-only, not destructive); agents compose
      `@target:sess4 …` using `get_agent_room` output.
- [ ] Self-mention drop + prompt guidance embedded in the tool description
      ("mention cooldowns apply; do not spam peers").

### P5 — Web UI (optional mobility, rides on 015)

- [ ] General chat input (posts `/msg`, `sender: "web"`); `@device:sess4`
      syntax documented in the placeholder; `REPLY:` kept as private alias.
- [ ] Feed pane next to the accordion (messages already arrive via SSE/DO
      relay — display only).
- [ ] **Blocked on**: 015 W5 `GITHUB_CLIENT_ID` (OAuth app creation). Worker
      contract needs zero changes.

### P6 — Tests & gates

- [ ] `mentions.rs`: parse tests (valid, mid-text non-routing, self-device,
      bad prefix, empty text).
- [ ] `feeder.rs`: `extract_mentions_for_device` tests (routes own-device,
      skips others, honors watermark, `@all` deferred).
- [ ] `mention_guard`: cooldown window, hourly cap, self-mention drop.
- [ ] `types.rs`: sender-field round-trip + old-snapshot default.
- [ ] Panel smoke test (gpui test): render with empty runtime (local-only),
      roster click fills input, send posts via mocked client.
- [ ] `./script/clippy` clean; `cargo test -p agent_board -p agent_ui` green.

## Perf/sec considerations

- **No new loops**: one poll (15s) + one SSE (when 📡) per PROCESS — strictly
  ≤ today's steady state once the board panel has been opened (its entity
  outlives dock close). Runtime extraction removes the would-be 2×/N×
  duplication.
- **Mention scan is O(new messages)** per round (≤ cap), pure string prefix
  check — nanoseconds.
- **Panel render only when open**; feed bounded at 100 rows; roster bounded
  by devices×agents (≤ dozens). No UniformList needed at these sizes; plain
  column with scroll (revisit >500).
- **Eager panel add is render-only** — no I/O at `initialize_panels`.
- **MCP tool** is agent-initiated; no polling added.
- Worker: KV list+sort per GET unchanged; MAX_MESSAGES 100 raises per-room KV
  list size ~10× but stays trivial at single-operator volume (free tier).

## Risks

- **Priority renumber touches two upstream files** — small merge friction
  with zed upstream; contained to two one-line diffs (fallback documented).
- **Agent loop storms**: guards mitigate, mute backstops; a determined agent
  can still rotate sessions — operator-visible in feed (visibility is the
  control).
- **At-most-once delivery** (inherited from 015): a mention missed while a
  device is offline is still in the feed (7d TTL) — operator re-posts. No
  ack in v1.
- **Watermark is in-memory**: restart re-scans the last snapshot; cooldown
  bounds duplicate injection; worst case one re-injected command per session.
- **KV eventual consistency** (≤60s): feed may lag; injection path rides the
  15s poll / SSE push regardless.
- **Feed cap 100**: war room is a chat, not a transcript — history beyond
  that lives in each agent's thread (which is the system of record).

## Dependency direction

```
zed.rs ──► agent_board (war_room panel + runtime)  [existing dep]
agent_board ──► auto_prompt::peer_states (inject)  [existing]
agent_board ──► workspace::dock (Panel)            [existing]
agent_ui (AgentPanel drain) ──► auto_prompt        [existing, untouched]
worker / web UI ──► (contract unchanged; msg cap + sender field only)
```

No new crates. `agent_ui` is NOT touched except tests. `workspace`, `outline_panel`,
`debugger_ui`: two one-line priority diffs only.

## GOAT gate

- [ ] Icon renders directly behind Collab Panel in the activity bar
      (screenshot, default settings, debug build — duplicate-priority panic
      path exercised).
- [ ] Operator @mention from Zed panel → visible in web feed AND injected
      into target thread (<1s 📡 on / <15s poll off; `📢 war-room` line in
      the thread).
- [ ] @mention from web UI (no local agents) → injected on the OWNING device.
- [ ] Agent → agent mention via `post_agent_board_message` → routed + injected.
- [ ] Self-mention dropped; cooldown + hourly cap log-and-suppress (forced
      storm test).
- [ ] Both panels open → exactly one poll loop (verified by log count over
      60s).
- [ ] Panel closed → zero render/notify work from the war room (profiler or
      log silence).
- [ ] Old worker payloads (no `sender`) and old snapshots deserialize
      (existing serde-default tests extended).
- [ ] `./script/clippy` + `cargo test -p agent_board` green.
- [ ] (deferred-blocked) Web chat input usable end-to-end — requires 015 W5
      `GITHUB_CLIENT_ID`; mark `- [-]` until unblocked.

## Deferred (explicitly out of v1)

- `@all` broadcast mentions; inline @autocomplete in the input bar; friendly
  agent names (meta-carried); `!`-prefix native steering
  (`set_end_turn_at_next_boundary` — still blocked on the same
  ConversationView→agent::Thread access noted in 015 W9); delivery acks;
  per-agent threads as separate tabs (feed filter is enough at this scale).
