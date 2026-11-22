# Issue 077: zed native agent loops identical tool calls (slop loop) with no guard

**Claimed:** m3 (this session — filing + fix in one pass)

## Status
- [x] Root cause identified (no per-turn tool-call repetition guard in `crates/agent` `Thread`)
- [x] Fix implemented — `ToolLoopGuard` in `RunningTurn`: hash (tool name + raw input), limit 10 identical calls per turn, refuse + stop turn + notify model
- [x] Tests added (loop trips on 11th identical call; distinct calls never trip) — `test_tool_loop_guard_stops_identical_calls`, `test_tool_loop_guard_allows_distinct_calls`
- [x] `cargo clippy -p agent` clean (release, all-targets, all-features, deny warnings; the 9 cargo-shear hits are pre-existing, in other crates — `agent_acp_server`, `auto_prompt`, `eval_cli`, …)
- [x] Committed on `develop` + pushed — `01d0b094106eeecc7a53e6b3a6814c190299b7c6`

Note 2026-11-22: full `cargo test -p agent --lib` run shows 848 passed + 1 pre-existing flake
(`test_agent_mail_broadcast_respects_liveness_cutoff`, fails 2/6 on clean HEAD too — real-clock
race in the test itself, unrelated to this fix).

## Symptom

Screenshot 2026-11-22 (thread `da4b7cf1-96a4-4a28-abaa-ea952ba4fcc0`, `zed-a3e3b452@m3`):
the zed native agent made 13+ consecutive identical tool calls in a single turn —
`git log --format=%h | sed -n '41,53p'` rendered as "Run Command" cards, one after
another — without ever changing approach. Nothing stops it: each round the model
re-emits the same call, the tool runs again, and the turn continues until the user
notices and mashes stop.

Scope: **zed native agent only** (`crates/agent::Thread`). External ACP agents
(claude code, gemini, …) run their own loops and are untouched.

## Root cause

`Thread::run_turn_internal` (`crates/agent/src/thread.rs`) loops
`stream completion → execute tool uses → report results → next round` with no
budget on repetition. A model stuck emitting the same tool call (same name +
same arguments) is re-executed indefinitely.

## Fix

Per-turn `ToolLoopGuard` on `RunningTurn`:

- Signature = `DefaultHasher(name, raw_input)` — hash-compare, exactly the
  "detect by hash" approach (md5-class; std SipHash, no new dep).
- One execution counted per tool call *started* (streaming calls count once at
  first partial input; streaming deltas and the final `send_full` handoff never
  re-count).
- Limit **10 identical calls per turn** (user-specified). Calls 1–10 run
  normally; call #11 (same signature) is refused with an `is_error` tool result
  that tells the model to stop repeating and change approach ("send msg to ai"),
  and the turn is stopped cleanly (`EndTurn`) without issuing another model
  request. Already-running in-flight tools complete naturally; their results
  still attach so history stays well-formed (every tool_use keeps a result).
- Telemetry: `Agent Tool Loop Guard Tripped` event + `log::warn`.

## Files modified

| File | Change |
|------|--------|
| `crates/agent/src/thread.rs` | `ToolLoopGuard` struct + `TOOL_LOOP_CALL_LIMIT = 10`; wired into `handle_tool_use_event` (both execution start points) and `run_turn_internal` (turn-stop check) |
| `crates/agent/src/tests/mod.rs` | `test_tool_loop_guard_stops_identical_calls`, `test_tool_loop_guard_allows_distinct_calls` |

## Severity

MEDIUM. Wasted tokens/quota per slop loop, UI floods with identical cards, no
forward progress until the user intervenes manually.
