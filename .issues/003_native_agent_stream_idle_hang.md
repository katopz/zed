# Issue 003: Native agent SSE stream idle hang — no idle timeout on `events.next()`

## Status
- [x] Root cause identified (evidence: 19 watchdog incidents, 10/11 halts correlate with completed-tool-call + dead stream)
- [ ] Fix proposed (add idle timeout to stream event loop)
- [ ] Tested
- [ ] GOAT verified

## Symptom

The native agent (Zed built-in, `crates/agent`) goes completely silent after a
tool call completes. The thread stays in `Generating` forever. The watchdog
(eventually implemented in issue 002) catches it and recovers, but the root
cause is an **unbounded wait on the SSE stream** — there is no idle timeout.

## Evidence (from `/tmp/zed_auto_prompt/` watchdog logs — 19 incidents)

Model in use: **`glm-5.2`** (OpenAI-compatible provider, likely Zhipu AI).

### Correlation matrix

| Scenario | HALTs | CONTINUES |
|----------|-------|-----------|
| **Tool output returned (tool done)** → stream dead | **10** | 1 |
| **No output yet** (tool still running, legitimate slow work) | 1 | 7 |

**10 out of 11 halts** follow the exact pattern: a tool call (terminal/read/
edit/grep) completes with its output, then the next `stream_completion` call to
the LLM provider opens an HTTP connection that never delivers SSE events.

### Representative incidents

| ts | tool | output | verdict |
|----|------|--------|---------|
| 00:31 | terminal (write+run PoC) | 1012 chars, program finished | halt |
| 01:04 | read_file (lora_still) | 1010 chars, lines returned | halt |
| 01:22 | grep (role_transport) | 18 chars "No matches found" | halt |
| 01:37 | edit_file (orchard/systems) | 1008 chars, edit applied | halt |
| 01:45 | terminal (cargo clippy) | 764 chars, "Finished" | halt |
| 01:46 | terminal (cargo test timeout) | 156 chars, timeout error | halt |
| 01:47 | terminal (bench loop) | 886 chars, full results | halt |
| 02:00 | terminal (git add+status) | 179 chars, status shown | halt |
| 02:00 | terminal (git checkout+stash) | 580 chars, branch info | halt |
| 02:00 | edit_file (birth_death.rs) | 999 chars, edit applied | halt |

Every one of these is a near-instant tool call that returned output, followed by
10 minutes of silence from the LLM stream. The watchdog correctly identified
"this is a stream hang, not slow work."

## Root Cause

`crates/agent/src/thread.rs`, `run_turn_internal` (line ~2272).

The agent's main loop structure:

```
loop {
    // Build request with tool results
    // model.stream_completion(request, cx).await  ← HTTP POST, returns stream

    loop {
        // select! between:
        //   events.next()        ← HANGS HERE if SSE idle
        //   tool_results.next()  ← empty after tools done
        //   cancellation_rx      ← only escape (watchdog uses this)
    }

    // process tool results, loop back to next stream_completion
}
```

The inner `select!` (line ~2353):

```rust
let first_event = futures::select! {
    event = events.next().fuse() => event,
    tool_result = ...select_next_some(&mut tool_results) => { ... },
    _ = cancellation_rx.changed().fuse() => { ... },
};
```

**There is no idle timeout.** If the provider (GLM-5.2 via OpenAI-compatible
endpoint) accepts the HTTP connection (200 OK) but then stops sending SSE
`data:` lines, `events.next()` blocks forever. The `reader.lines()` stream
(`crates/open_ai/src/open_ai.rs:780`) has no read deadline.

The TCP connection stays open (no RST/FIN), no data flows, and the `select!`
has no timer branch. Only `cancellation_rx` (user click or watchdog halt) can
break the deadlock.

### Why this is NOT a provider bug (even though the provider causes it)

Network providers (GLM, Anthropic, OpenAI, any SSE endpoint) can stall mid-stream
due to: load balancer idle timeouts, server-side queuing, network partition,
proxy buffering, or provider bugs. A robust client MUST have an idle timeout.
Every major SDK (OpenAI Python, Anthropic Python) implements client-side read
timeouts. Zed's native agent has none.

## Proposed Fix

Add an **idle timeout** to the stream event loop in `run_turn_internal`. If no
event arrives within N seconds (configurable, default 60s), the stream is
considered dead:

1. Log a warning with provider + elapsed time
2. Drop the stalled stream
3. Return an error so `retry_completion_error` can kick in (with backoff)
4. OR: emit a `CompletionError` that the retry loop handles

### Implementation sketch

In `run_turn_internal`, the inner `select!` at line ~2353 should race against
a timer:

```rust
let idle_timeout = cx.background_executor().timer(Duration::from_secs(
    config.stream_idle_timeout_secs.unwrap_or(60)
));

let first_event = futures::select! {
    event = events.next().fuse() => event,
    tool_result = ... => { ... },
    _ = cancellation_rx.changed().fuse() => { ... },
    _ = idle_timeout.fuse() => {
        log::warn!("Stream idle timeout (no events in {}s), treating as dead", 60);
        // Break out, set error so retry loop fires
        error = Some(LanguageModelCompletionError::Other(anyhow!("stream idle timeout")));
        break;
    }
};
```

The same timeout must apply to the batch-collection loop at line ~2392
(`while let Some(event) = events.next().now_or_never()`) — though that only
drains already-buffered events so it's less critical.

### Config

Add to agent settings:
```json
{
  "agent": {
    "stream_idle_timeout_secs": 60  // default; 0 = disabled (current behavior)
  }
}
```

### What this fixes

- The native agent will **auto-recover** from stalled streams via the existing
  retry logic, no watchdog needed for this specific failure mode.
- The watchdog (issue 002) remains as a defense-in-depth for OTHER hang modes
  (stuck tool calls, subagent hangs, deadlocks).

### What this does NOT fix

- The provider-side cause of stalls (GLM/Zhipu server, network, proxy).
- The watchdog timeout for slow-but-legitimate work (e.g. 20-min cargo builds).

## Severity

HIGH — this happens frequently (10 incidents in a single session). Without the
watchdog, the agent is completely wedged. With the watchdog, there's a 10-min
delay per incident plus the reasoning LLM cost.

## Files to modify

| File | Change |
|------|--------|
| `crates/agent/src/thread.rs` | Add idle timeout to `select!` in `run_turn_internal` |
| `crates/agent_settings/src/agent_settings.rs` | Add `stream_idle_timeout_secs` setting |
| `crates/agent/src/thread.rs` (tests) | Test that idle stream times out and retries |

## Key Files Reference

- `crates/agent/src/thread.rs:2272` — `run_turn_internal` (main loop)
- `crates/agent/src/thread.rs:2353` — the `select!` with no timeout
- `crates/agent/src/thread.rs:2462` — tool result processing
- `crates/agent/src/thread.rs:2501` — loop continuation after tool results
- `crates/open_ai/src/open_ai.rs:780` — `reader.lines()` (no read deadline)
- `crates/anthropic/src/anthropic.rs:514` — same pattern (no read deadline)
- `crates/language_models/src/provider/open_ai_compatible.rs:580` — GLM path
