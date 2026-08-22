# 002 — Auto-prompt stuck after tool call: worker LLM stream watchdog (RESOLVED)

## Status

Resolved. Watchdog feature shipped.

## Symptom

After a tool call completed with output, the worker agent never produced its
next response. Thread stayed in `ThreadStatus::Generating` forever — spinner
and "loading" indefinitely. `on_thread_stopped` never fired because the worker
stream hung before emitting `AcpThreadEvent::Stopped`.

## Root Cause

Auto-prompt only runs on `on_thread_stopped`. A hung worker LLM stream never
reaches that entry point. None of the existing timeouts
(`CHAIN_TIMEOUT_SECS`, 60s orchestration LLM timeout, 45s pending-question
timeout) cover a stuck worker — all three require `on_thread_stopped` to have
already fired.

## Solution

Stuck-thread watchdog in the `agent_ui` layer that runs independently of
`on_thread_stopped`:

1. Armed at generation start (`send_content` — the single funnel for all send
   paths: initial user send, auto_prompt continuations, queued messages,
   interrupt-and-send). Also armed in `retry_generation`.
2. On timeout: gathers context (last tool call + output, last assistant
   message), calls a headless reasoning LLM: "continue or halt?"
3. `continue` → reschedule watchdog for another window (escalating).
4. `halt` → `thread.cancel()`, inject timeout notification, auto_prompt picks
   up from the resulting `Stopped(Cancelled)`.

## Key files

- `crates/auto_prompt/src/watchdog.rs` — reasoning LLM call + prompt.
- `crates/auto_prompt/src/config.rs` — `watchdog_timeout_secs` (default 1800),
  `watchdog_enabled` (default true).
- `crates/agent_ui/src/auto_prompt/mod.rs` — `start_watchdog`,
  `cancel_watchdog_for_thread`.
- `crates/agent_ui/src/conversation_view.rs` — cancel on Stopped/Error.
- `crates/agent_ui/src/conversation_view/thread_view.rs` —
  `_watchdog_task: Option<Task<()>>`, `arm_watchdog`.

## Follow-up

Issue 004 fixed a timer-accumulation regression in this watchdog
(cancel-by-active-thread + re-arm guard). See `.docs/004_watchdog_timer_reset_fix.md`.

Issue 003 tracks the underlying cause (no idle timeout on the native agent's
SSE stream `events.next()`). The watchdog is recovery; issue 003 is the cure.
