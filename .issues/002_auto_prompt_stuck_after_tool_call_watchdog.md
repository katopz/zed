# Issue 002: Auto-prompt stuck after tool call — worker LLM stream watchdog

## Status
- [x] Root cause identified
- [x] Watchdog implemented
- [x] Tested (unit tests pass, compiles clean)
- [x] Coverage gap fixed: watchdog now armed at generation start (send_content),
      not just after `on_thread_stopped`. Covers initial user send + within-turn
      hangs (the original bug1.md scenario).

## Symptom

After a tool call (terminal/git command) completes with output, the worker
agent (Claude Code via ACP) never generates its next response. The thread
remains in `ThreadStatus::Generating` forever — the UI shows a spinner and
"loading" indefinitely. Auto-prompt never fires because `on_thread_stopped`
is only invoked on `AcpThreadEvent::Stopped`, which never arrives.

Bug log evidence: `bug1.md:16028-16042` — last visible entry is a completed
`git --no-pager log` tool call with valid output, followed by silence.

## Root Cause

There are two independent layers:

1. **Worker layer** (`AcpThread`) — runs the LLM stream + tool calls.
2. **Orchestration layer** (`auto_prompt`) — decides whether to chain after
   the worker stops.

Auto-prompt only runs on `on_thread_stopped`. If the worker LLM stream hangs
(after receiving tool output, or mid-stream), the thread never stops and
auto_prompt has no entry point. There is no watchdog that detects "the worker
has been generating for N minutes without producing any output or stopping."

### Existing timeouts (why they don't help)

| Timeout | Location | Scope |
|---------|----------|-------|
| `CHAIN_TIMEOUT_SECS=300` | `auto_prompt.rs:37` | Passive — only checked when `get_iteration()` runs (i.e. when thread eventually stops). Does nothing while stuck. |
| 60s LLM call timeout | `auto_prompt.rs:2316` | Only covers auto_prompt's OWN orchestration LLM call, not the worker. |
| 45s pending-question timeout | `pending_question.rs:501` | Same — only auto_prompt's own calls. |

All three require `on_thread_stopped` to have already fired. The stuck-worker
case never reaches that entry point.

## Proposed Solution

A **stuck-thread watchdog** that lives in the agent_ui layer (alongside
`on_thread_stopped`) and runs independently of the thread-stopped event.

### Design

```
Thread enters Generating
  └─► start_watchdog(thread, timeout=10min)
        │
        ├─ thread stops before timeout? ──► cancel watchdog, done
        │
        └─ timeout fires (10 min of generating, no stop)
              │
              ├─ gather context: last tool call + output, last assistant msg
              ├─ call headless reasoning LLM: "continue or halt?"
              │     system prompt: stuck-thread reasoning
              │     context: last tool call input/output, elapsed time
              │     output: { "action": "continue" | "halt", "reason": "..." }
              │
              ├─ action == "continue"
              │     └─ reschedule watchdog for another 10 min (cumulative 20)
              │
              └─ action == "halt"
                    ├─ cancel worker thread (thread.cancel())
                    ├─ on cancel-complete: thread emits Stopped(Cancelled)
                    ├─ inject timeout notification into same thread
                    │     "Your last tool call completed N minutes ago but you
                    │      produced no follow-up. The watchdog halted you.
                    │      Decide: retry, try another approach, or stop."
                    └─ auto_prompt picks up from on_thread_stopped and lets
                       its normal LLM decide next steps
```

### Key decisions

1. **Lives in agent_ui, not auto_prompt** — auto_prompt is a decision library
   with no UI entity access. The watchdog needs the thread entity + window +
   context to cancel and dispatch.

2. **Reasoning is LLM-based, not rule-based** — per user requirement. The
   reasoning LLM sees the command + output + elapsed time and decides. A
   simple "is there a spinner" check would fire on every long-running build.

3. **Escalating windows** — 10min → reason → 10min → reason → ... The reasoning
   LLM sees cumulative elapsed time. If it says "continue" at 10 min, the same
   question is asked at 20 min with "this is the 2nd timeout."

4. **Halt = cancel + inject** — on halt, we call `thread.cancel()` which
   triggers `AcpThreadEvent::Stopped(Cancelled)`. But Cancelled currently
   causes `decide_claude` to return `NoAction` (chain stop). So after cancel,
   we need to send the timeout message to the thread, which restarts
   generation with the timeout context. Auto-prompt then picks up normally.

5. **Config** — add `watchdog_timeout_secs` (default 600) and
   `watchdog_enabled` (default true) to `AutoPromptConfig`.

### Files to modify

| File | Change |
|------|--------|
| `crates/auto_prompt/src/watchdog.rs` | NEW — reasoning LLM call + prompt |
| `crates/auto_prompt/src/auto_prompt.rs` | export watchdog module, add config fields |
| `crates/auto_prompt/src/config.rs` | add `watchdog_timeout_secs`, `watchdog_enabled` |
| `crates/agent_ui/src/auto_prompt/mod.rs` | `start_watchdog`, `cancel_watchdog` |
| `crates/agent_ui/src/conversation_view.rs` | start watchdog on Generating, cancel on Stopped |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | add `_watchdog_task: Option<Task<()>>` |

### What this does NOT fix

- The underlying cause of the worker LLM stream hanging (likely a provider
  rate-limit, network stall, or ACP protocol issue). The watchdog is a
  recovery mechanism, not a cure for the stream bug.
- Non-auto-prompt threads (user-driven, no auto-prompt enabled). The watchdog
  only runs when `auto_prompt_enabled == true`.

### Follow-up fix (gap closure)

The initial watchdog implementation only armed the watchdog in
`conversation_view.rs` after `on_thread_stopped` dispatched a continuation.
This missed the **original bug1.md scenario**: a worker that hangs *mid-turn*
(after a tool call, before emitting `Stopped`) never triggers `on_thread_stopped`,
so no watchdog was armed.

**Fix**: The watchdog is now armed in `ThreadView::send_content` (the single
funnel point for all send paths: initial user send, auto_prompt continuations,
queued messages, interrupt-and-send). The arming happens right after
`thread.send()` when the thread enters `Generating`. The redundant arming in
`conversation_view.rs` Stopped/Error handlers was removed (send_content covers it).

This means the watchdog now protects EVERY generation, including the first one.
`retry_generation` is the only send path that doesn't go through `send_content`
(it calls `thread.retry()` directly) — retries are user-initiated and less likely
to be in auto_prompt mode, so this is a minor known gap.
