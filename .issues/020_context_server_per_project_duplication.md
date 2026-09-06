# 020: Context servers duplicated per project — global MCP fleet × per-project isolation × multi-repo farm

**Status:** Open — design change, not a hot fix. Detection (evidence) 2026-09-06; fix needs an app-level refcounted server registry. Owner sign-off on the tradeoff before implementing.

## Symptom (measured 2026-09-06, running instance PID 85723)

Single Zed Dev window, one workspace. 8 MCP processes for 4 configured servers
(`mcp-server-zai-{web-reader,vision,zread,web-search}`), all direct children of
the zed process, in **two full generations spawned 1s apart at boot**:

| Gen | PIDs | Spawned | cwd (lsof) | Belongs to |
|---|---|---|---|---|
| 1 | 91896, 92286, 92396, 92462 | 13:46:41 | `/Users/katopz/git/katgpt-rs` | agent farm's cross-repo project |
| 2 | 93921, 93927, 93939, 93945 | 13:46:42 | `/Volumes/SDXC1TB/git/zed` | the workspace's own project |

Cost: ~4 extra node processes (~100–200 MB RSS + event loops) per additional
project. The auto-prompt farm routinely spans N repos → N× the global fleet.

## Root cause

`ContextServerStore` is **per-Project** (`Project::local` creates one, project.rs
L1231). Server settings are resolved globally (user settings + extension
descriptors), and every project's `maintain_servers` expands the FULL global
fleet (`registry.context_server_descriptors()` → `configured_servers` →
`servers_to_start`). The per-store guards (`run_server` stops Starting/Running
first; `update_servers_task` serializes `maintain_servers`) are correct — this
is not a race — but nothing dedupes **across stores**.

The farm workflow makes this a real tax: `run_auto_prompt` dispatches threads
into sibling repos, the agent panel materializes a project per worktree, and
each project spawns its own copy of every globally-configured server.

## Design direction (SOLID/DRY: shared substrate behind per-project views)

App-level **refcounted context-server registry keyed by resolved configuration
(BLAKE3 of command/args/env/url + resolved working dir)**:

- `ContextServerHandle` = `{ registry_key, Arc<ContextServer>, refcount }` in a
  global (`App`-level) store; projects request handles instead of spawning.
- Two projects with byte-identical resolved config share one process; refcount
  drops to 0 → stop (with drain so a restarting project doesn't kill a shared
  server mid-flight).
- Config change in project A must NOT restart the shared server out from under
  project B → key mismatch means A gets its own private instance (fork-on-write,
  same pattern as CoW).
- Project-scoped servers (per-worktree settings, remote projects) keep today's
  dedicated path — only globally-configured, locally-spawned stdio/HTTP servers
  are shareable.
- Env resolution order is already hashed into the key via resolved command/env —
  beware project-specific env overrides silently de-duplicating (they fork by
  key, which is correct).

Non-goals: no cross-app sharing, no pooling of HTTP-remote servers beyond
process-level (they're already just local bridges here), no change to the
extension API surface.

## Success criteria

- 1 window + farm across N repos ⇒ exactly 4 MCP processes (one per unique
  resolved config), regardless of N.
- Restart-on-config-change stays project-private; other projects unaffected.
- `lsof cwd` shows the first-spawned project's root; late joiners hold handles,
  not processes.

## Refs

- `.docs/006_auto_prompt_cpu_drain_p0_fixes.md` — the deferred P1 item was
  resolved as by-design with this evidence (2026-09-06); this issue is the
  design-fix follow-up.
- `crates/project/src/context_server_store.rs` — `new_internal` (per-store
  subscriptions), `maintain_servers` (fleet expansion), `run_server` (guard).
- `crates/project/src/project.rs` L1231 — per-project store construction.
