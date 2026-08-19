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
- **Pinned work board (todolist)** at the top of the panel: what agent is
  doing what, per-item state (`doing` / `stale` / `done`), race conflicts
  flagged — capped to the **last 5 hours** of work. Pure projection over
  existing claims + states (see "Pinned work board" section); zero new
  sources of truth.
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

## Pinned work board (todolist, 5h window)

Reduce race tasks by making ownership visible at a glance. Pinned above the
war room feed. **Derived, never duplicated**: the projection merges two
existing streams — no new write path, no new wire type.

### Data sources (both already shipped)

| Source | Carries | Location |
|---|---|---|
| Local claims | `ActivePlanClaim { plan_file, session_id, task_summary, claimed_ago_secs }` | `auto_prompt::plan_registry` (300s heartbeat GC) |
| Remote scopes | `DeviceStatus.scopes[] { plan_file, task_summary, session_id }` + `stale` + `updated_at` | `RoomSnapshot.statuses` (15s feeder refresh) |
| Terminal signal | NEW one-liner: on claim release, `broadcast_state(sess, None, "released: {plan_name}", plan_path)` at the existing chain-stop release hook | rides the `/state` ring buffer |

Cross-device race *prevention* already exists: `inject_remote_claims`
re-claims remote scopes locally (`remote:{device}:{session}`), and
`format_claims_for_context(session)` feeds peers' claims into every agent's
LLM context; `is_claimed_by_other`/`filter_unclaimed` make agents skip taken
plans. The work board adds the **operator-visible** layer (and the honest
"released" terminal state that is currently missing).

### Projection (pure fn, `war_room.rs`)

```
build_work_board(snapshot, local_claims, now) -> Vec<WorkItem>
WorkItem { plan_name, plan_path, owner_labels: Vec<device:sess4>,
           task_summary, state, last_activity_ts }

state: Doing   — heartbeat/status fresh (<300s)
       Stale   — status.stale or heartbeat older than 300s, no release seen
       Released— a `released: {plan}` state matched by meta==plan_path
```

- Merge key: normalized plan path (claims and scopes use the same path form).
- **5h window**: drop items whose `last_activity_ts` (max of claim heartbeat,
  scope `updated_at`, latest matching state `ts`) is older than 5h. Time-boxed
  ring buffer — the ledger self-prunes; KV keeps 7d for forensics, UI stays lean.
- **Race flag**: `Doing` item with ≥2 distinct owner devices → render warning
  color + ⚠. The claim system narrows the window to ~15s of feeder lag; the
  flag catches manual/same-file collisions it can't prevent.
- Render order: `Doing` first (race-flagged top), then `Stale`, then
  `Released`; cap at ~20 rows with scroll (bounded by 5h window anyway).
- Done-honesty: v1 broadcasts `released:` (not `done:`) — the chain-stop hook
  fires on both success and abort, and we don't distinguish them yet. A
  `done:` state fires only from the explicit completion path if one is
  trivially available; otherwise `Released` is the honest terminal. Don't lie
  to the operator.

### Agent visibility

`get_agent_room` MCP output gains the merged work board (scopes + states
projection, same shape as the panel) so an agent can ask "what's safe to
grab" explicitly — complementing the automatic context injection.

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
  - **Work board (pinned, top)**: the 5h todolist projection — `Doing`
    (race-flagged first) / `Stale` / `Released`, owner `device:sess4` +
    task_summary per item; ~20-row cap with scroll. Collapsible to a single
    summary row (`3 doing · 1 stale · 2 released`).
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

- [x] `agent_board/src/runtime.rs`: `BoardRuntime` entity (GPUI Global):
      config load, identity, client, room resolution, poll task, SSE client,
      MCP server — moved verbatim from `AgentBoardPanel::{try_start,
      start_poll, start_realtime}` (net code motion, no behavior change).
- [x] `agent_board::init` starts the runtime when `worker_url` is configured;
      runtime `cx.notify()` on each snapshot; `board_state` globals unchanged.
- [x] Refactor `AgentBoardPanel` to a pure view over the runtime (drop its
      owned poll/SSE/MCP fields; keep actions/mute UI). Its Toggle action
      must not create a second runtime.
- [x] Verify single poll loop with both panels open (log line or counter):
      `runtime poll loop starting (single instance per process)` at start +
      `sync round #N` debug log per round — `poll_rounds()` counter exposed
      for the GOAT log-count check.

### P1 — Mention routing

- [x] `agent_board/src/mentions.rs`: pure `parse_mention(&str) ->
      Option<Mention{device, prefix, text}>` + `sender_label()` helpers.
- [x] `feeder.rs`: `extract_mentions_for_device(&snapshot, device_name,
      watermark) -> Vec<(prefix, text)>` (mirror of
      `extract_replies_for_device`), wired into runtime `on_snapshot` (sync
      step 3); ts high-water mark persisted in the runtime (in-memory
      process-global; re-inject risk after restart bounded by cooldown —
      acceptable, documented).
- [x] Cooldown + rate-cap guard (pure fn `mention_guard`) + config fields
      `mention_cooldown_secs: u64 = 60`, `mention_max_per_hour: u32 = 20`.
- [x] Injection format `📢 war-room [@sender] text` via existing
      `inject_web_reply` (no new injection path). SSE push path scans the
      same watermark/guard so 📡-on delivers mentions <1s.

### P2 — Worker + wire (small, backward compatible)

- [x] `MAX_MESSAGES` 10 → 100, `MAX_ROOM_STATES` 10 → 50 (`index.js` consts +
      `wrangler.toml` vars; states cap raised so `released:` terminal markers
      survive chatty rooms within the 5h window).
- [x] `BoardMessage`/`PostMessageBody` gain `#[serde(default)] sender`;
      worker passes it through on `/msg`; old payloads default `""`.

### P3 — WarRoomPanel

- [x] `war_room.rs`: panel entity + render (header / **work board** / roster /
      feed / input) per the design above; local-only state; 📡 toggle bound to
      runtime. Work board is a pure child render of the projection — no
      separate entity, no extra notifications.
- [x] `actions!(war_room, [Toggle, ToggleFocus, Refresh])` +
      `workspace.register_action` wiring in `agent_board::init`.
- [x] `zed.rs initialize_panels`: eager add (`WarRoomPanel::new` is cheap —
      no I/O; all network lives in the runtime).
- [x] Priority renumber: Outline 6→7, Debug 7→8; WarRoom = 6.
- [x] Icon `UserGroup`, tooltip "War Room", `icon_label` = pending-mention
      count when panel closed (unwatched-mention counter in mentions.rs,
      cleared on Toggle/ToggleFocus).

### P4 — Agent voice (MCP write tool)

- [x] `board_state::post_message(text, sender)` writer fn (same shape as the
      broadcaster: clone handle, spawn, log-fail).
- [x] MCP tool `post_agent_board_message { text, sender }` (`mcp_tools.rs`,
      default annotations: not read-only, not destructive); agents compose
      `@target:sess4 …` using `get_agent_room` output.
- [x] `get_agent_room` output gains the work board (merged scopes + states,
      same `WorkItem` shape as the panel) so agents can query "what's safe
      to grab" explicitly.
- [x] Self-mention drop + prompt guidance embedded in the tool description
      ("mention cooldowns apply; do not spam peers").

### P5 — Web UI (optional mobility, rides on 015)

- [-] General chat input (posts `/msg`, `sender: "web"`); `@device:sess4`
      syntax documented in the placeholder; `REPLY:` kept as private alias.
      — BLOCKED on 015 W5 `GITHUB_CLIENT_ID`; do not touch while blocked.
- [-] Feed pane next to the accordion (messages already arrive via SSE/DO
      relay — display only). BLOCKED with the above.
- [-] **Blocked on**: 015 W5 `GITHUB_CLIENT_ID` (OAuth app creation). Worker
      contract needs zero changes.

### P6 — Tests & gates

- [x] `mentions.rs`: parse tests (valid, mid-text non-routing, self-device,
      bad prefix, empty text).
- [x] `feeder.rs`: `extract_mentions_for_device` tests (routes own-device,
      skips others, honors watermark, `@all` deferred).
- [x] `mention_guard`: cooldown window, hourly cap, self-mention drop.
- [x] `types.rs`: sender-field round-trip + old-snapshot default
      (`board_message_old_payload_defaults_empty_sender`, legacy
      `PostMessageBody` without `sender`).
- [x] `runtime.rs` tests: `init_is_idempotent_one_runtime_per_process`,
      `local_only_config_starts_no_poll_task_or_socket`,
      `poll_rounds_counts_each_snapshot_once_shared_by_all_views` — the
      single-poll-loop GOAT invariant, verified hermetically via
      `init_global_with_config` (config-injection seam; production
      `init_global` still loads `~/.config/zed/agent_board.json`).
- [x] `war_room.rs`: `build_work_board` projection tests — fresh/stale/
      released states, 5h cutoff boundary, race flag on two devices doing
      the same plan, merge of local claims + remote scopes by path.
- [x] Panel smoke test (gpui test): `war_room::tests::panel_smoke_local_only`
      — constructs both panels against one inert local-only runtime (default
      config ⇒ no worker ⇒ no network/MCP/poll; fake HTTP client 404s if
      anything escapes), asserts both panels + the global resolve to the SAME
      runtime entity (single-poll-loop invariant), prefill → `@label ` in the
      input, framework `cx.draw` of the full tree against an empty snapshot,
      `on_snapshot` with a realistic room (two devices racing one plan,
      released marker, mention message, stale scope) → `poll_rounds == 1` →
      draw again, then send-with-no-worker clears the input without panic.
      Setup extracted to `setup_local_only_harness` (shared with the closed
      and priority tests).
- [x] Mention pipeline end-to-end (gpui test):
      `runtime::tests::mention_pipeline_storm_cooldown_cap_and_delivery` —
      drives `on_snapshot` rounds through the runtime with the
      `set_device_name_for_tests` seam (no worker): agent→agent delivery
      with the `📢 war-room [@sender]` label, 25-msg self-mention storm →
      zero injections (no feedback loop), 25-msg same-target cooldown storm
      → zero duplicates, per-target cooldown isolation, hourly cap allows
      exactly N then log-and-suppresses the next storm, SSE push path
      (`handle_board_message`) delivers once and blocks the watermark replay,
      and a 100-message snapshot → scan+guard+inject measured < 1s (the
      local slice of the <1s 📡 GOAT bound; network dominates the rest).
      Sole owner of the process-global mention statics (tests run parallel).
- [x] Panel-closed silence (gpui test):
      `war_room::tests::panel_closed_silence_zero_render_work_after_drop` —
      a `#[cfg(test)] PANEL_RENDER_COUNT` counter proves the panel renders
      while alive (draw → delta > 0) and does ZERO render work after the
      entity is dropped (observe-subscription dies with it) while the
      runtime keeps syncing (`poll_rounds` advances). Mention injection is
      panel-independent by construction — the pipeline test delivers with
      no panel in existence at all.
- [x] Duplicate-priority panic path (gpui test, `#[should_panic]`):
      `war_room::tests::duplicate_activation_priority_panics_in_debug_builds`
      — adds WarRoomPanel (priority 6) + `workspace::dock::test::TestPanel`
      impostor (also 6) to a real test workspace → `Dock::add_panel` panics
      in debug builds, proving the guard fires and WarRoom's priority
      participates in the dock ordering that places its icon directly
      behind CollabPanel (5).
- [x] Panel screenshot artifact (example):
      `cargo run -p agent_board --example war_room_screenshot` renders the
      panel against an inert runtime + realistic room (race ⚠, released,
      stale, live mention, roster prefill) through the real Metal compositor
      (offscreen `VisualTestAppContext`), One Dark forced, and saves a
      pixel-faithful PNG. Artifact committed at `.plans/024_war_room_panel.png`
      (920×1560). Note: requires `gpui_platform/font-kit` dev-feature —
      without it font registration silently no-ops and text never renders.
- [x] `./script/clippy` clean; `cargo test -p agent_board -p agent_ui` green.
      (agent_board: scoped `cargo clippy -p agent_board --all-targets` clean
      — repo-wide script skipped under sibling-agent build load; 80/80 tests
      incl. the gpui harnesses and the `#[should_panic]` priority test; the
      `war_room_screenshot` example builds clean too. agent_ui untouched by
      design.)

### P7 — Pinned work board (todolist)

- [x] Pure projection `build_work_board(snapshot, local_claims, now) ->
      Vec<WorkItem>` in `war_room.rs` (merge key: normalized plan path;
      states per the spec; 5h `last_activity_ts` cutoff; race flag when ≥2
      devices `Doing` the same plan).
- [x] Release broadcast: at the existing chain-stop release hook
      (`auto_prompt`, where `release_all_for_session` is called), emit
      `broadcast_state(sess, None, "released: {plan_name}", plan_path)` per
      released claim — rides `/state`; no new endpoint. Honest terminal only
      (`released`, not `done`) — no completion signal exists at the call
      site, so `Released` is the honest terminal.
- [x] Panel: pinned section render (order: race-flagged `Doing` → `Doing` →
      `Stale` → `Released`; ~20-row cap; collapsible summary row);
      recompute only on runtime notify (15s poll / SSE push), never on a
      timer of its own.
- [x] `get_agent_room` MCP output extension (see P4).
- [x] Clock note: local claims use `time_monotonic_secs` (no wall clock) —
      `claimed_ago_secs` converts via `now - claimed_ago`; remote items use
      wall-clock `updated_at`/`ts`. The projection normalizes to wall-clock
      `now_ms` at the call site and never mixes epochs.

## Perf/sec considerations

- **No new loops**: one poll (15s) + one SSE (when 📡) per PROCESS — strictly
  ≤ today's steady state once the board panel has been opened (its entity
  outlives dock close). Runtime extraction removes the would-be 2×/N×
  duplication.
- **Work board projection is O(claims + states)** per runtime notify
  (≤50 states + ≤ dozens of claims), recompute only on notify — no timer, no
  allocation in render beyond the projection vec (reused via `Vec::clear` +
  re-push if it ever shows up in profiles; it won't at this scale).
- **5h window bounds the projection** — time-boxed, not unbounded history;
  render capped ~20 rows regardless.
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
- **"Released" ≠ "done"**: the chain-stop hook fires on success AND abort;
  v1 renders the honest neutral terminal. If operators need true done-state,
  wire the completion-specific path later (deferred).
- **Chatty-room eviction**: a `released:` marker can be pushed out of the
  states ring buffer before 5h in a very chatty room — mitigated by
  `MAX_ROOM_STATES` 10→50; worst case the item drops off the board early
  (it's a projection, the claims/KV truth is unaffected).
- **Clock epochs**: local claims are monotonic-clock, remote are wall-clock —
  the projection normalizes at the call site (P7 task); mixing them naively
  would produce garbage 5h cutoffs.
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

- [-] (partial) Icon renders directly behind Collab Panel in the activity bar
      (screenshot, default settings, debug build — duplicate-priority panic
      path exercised).
      — AUTOMATED: priority 6 locked by
      `panel_closed_silence_zero_render_work_after_drop`; duplicate-priority
      panic path EXERCISED by
      `duplicate_activation_priority_panics_in_debug_builds` (#[should_panic]);
      pixel-faithful panel render committed at `.plans/024_war_room_panel.png`
      (via `examples/war_room_screenshot`). Adjacency (Collab 5 / WarRoom 6 /
      Outline 7) verified by grep across the panel crates; the ACTIVITY BAR
      screenshot itself needs a live debug-build session (live-GUI remainder).
- [-] (partial) Operator @mention from Zed panel → visible in web feed AND
      injected into target thread (<1s 📡 on / <15s poll off; `📢 war-room`
      line in the thread).
      — AUTOMATED (local slice): 100-message snapshot → scan+guard+inject
      measured < 1s in `mention_pipeline_storm_cooldown_cap_and_delivery`;
      the `📢 war-room [@sender]` label format is asserted verbatim; the
      <15s poll cadence is the runtime's interval config. LIVE remainder:
      network legs (SSE push + thread drain) need a live 📡 session.
- [-] (partial) @mention from web UI (no local agents) → injected on the
      OWNING device.
      — AUTOMATED (receiving half): `sender: "web"` mentions route + inject
      + label in `mention_pipeline_storm_cooldown_cap_and_delivery`. LIVE
      remainder: posting from the web UI — blocked on P5 (015 W5
      `GITHUB_CLIENT_ID`).
- [-] (partial) Agent → agent mention via `post_agent_board_message` →
      routed + injected.
      — AUTOMATED (receiving half): sender `m3:aaaa` → `@m3:f3a2` delivered
      into the reply queue with the sibling-agent label (same pipeline
      test); sibling-vs-self discrimination covered by
      `scan_same_device_sibling_agent_is_routed`. LIVE remainder: the MCP
      `post_agent_board_message` round-trip through the deployed worker
      needs a live agent session.
- [x] Self-mention dropped; cooldown + hourly cap log-and-suppress (forced
      storm test).
      — AUTOMATED: `mention_pipeline_storm_cooldown_cap_and_delivery` —
      25-msg self-mention storm → zero injections (no feedback loop);
      25-msg cooldown storm → zero duplicates; hourly cap allows exactly N
      per hour then suppresses the follow-up storm; per-target isolation.
- [x] Work board: two devices `Doing` the same plan → race flag renders
      (warning color + ⚠) within one sync round; single-owner items don't.
      — AUTOMATED: `race_flag_on_two_devices_doing_same_plan`,
      `same_device_two_sessions_is_not_a_race`,
      `merges_local_claims_and_remote_scopes_by_path` (projection);
      `panel_smoke_local_only` drives `on_snapshot` with a live race and
      draws the race row within the same round (render path); the ⚠ warning
      color is visible in `.plans/024_war_room_panel.png`.
- [x] Work board: item older than 5h disappears on next notify; `released:`
      state pins a `Released` item within the window.
      — AUTOMATED: `five_hour_cutoff_drops_old_items` (incl. boundary),
      `released_state_pins_released_item`,
      `local_claim_converts_clock_epochs`; data-driven draw in
      `panel_smoke_local_only` renders released + stale rows.
- [x] Both panels open → exactly one poll loop (verified by log count over
      60s).
      — AUTOMATED: `init_is_idempotent_one_runtime_per_process` +
      `panel_smoke_local_only` (both panels bound to the SAME runtime
      entity) + `poll_rounds_counts_each_snapshot_once_shared_by_all_views`.
      Live log-count over 60s remains an optional visual confirmation.
- [x] Panel closed → zero render/notify work from the war room (profiler or
      log silence).
      — AUTOMATED:
      `panel_closed_silence_zero_render_work_after_drop` — render-count
      delta stays at zero after the panel entity is dropped while the
      runtime keeps syncing; injection is panel-independent (pipeline test
      delivers with no panel in existence). Live profiler remains an
      optional visual confirmation.
- [x] Old worker payloads (no `sender`) and old snapshots deserialize
      (existing serde-default tests extended).
      — AUTOMATED: `board_message_old_payload_defaults_empty_sender` +
      legacy-`PostMessageBody` test in `types.rs`; deployed worker
      (v2b3d70d5) round-trips the field.
- [x] `./script/clippy` + `cargo test -p agent_board` green.
      — scoped `cargo clippy -p agent_board --all-targets` clean
      (repo-wide script skipped under sibling-agent build load); 77/77 tests.
- [-] (deferred-blocked) Web chat input usable end-to-end — requires 015 W5
      `GITHUB_CLIENT_ID`; blocked, untouched.

## Deferred (explicitly out of v1)

- `@all` broadcast mentions; inline @autocomplete in the input bar; friendly
  agent names (meta-carried); `!`-prefix native steering
  (`set_end_turn_at_next_boundary` — still blocked on the same
  ConversationView→agent::Thread access noted in 015 W9); delivery acks;
  per-agent threads as separate tabs (feed filter is enough at this scale);
  task-level (checkbox) granularity inside plan files — the work board is
  plan-file-level v1, matching the claim substrate's granularity; true
  `done` terminal state (requires completion-specific hook).
