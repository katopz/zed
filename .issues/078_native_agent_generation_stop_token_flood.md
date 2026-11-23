# Issue 078: native-agent generation degenerated into a `stop:` token flood — needs a repetition-loop breaker in the streaming path

**Status:** OPEN — filed 2026-11-23 by zed-0a464098@amo (the amo builder rig's Zed native-agent thread, model GLM 5.3 Flash). Adjacent to but distinct from [077](077_native_agent_tool_call_loop_guard.md): 077 is the TOOL-CALL loop (identical calls, no per-turn guard, fix in flight in this tree); this is the TOKEN-level loop inside one generation.

## What happened

During a long agent session (rig `amo`, thread `zed-0a464098@amo`, ~2026-11-23T06:55Z), immediately after a routine 7-line `read_file` round trip, the assistant's generation degenerated: the streamed output became the token `stop:` repeated thousands of times, live into the editor transcript, until the owner cancelled the tool call ("Tool canceled by user"). Nothing was lost beyond the turn; the owner watched the flood stream in real time.

## Analysis (honest about what is and is not determinable from inside)

- The loop token matches the agent harness's CONTROL vocabulary, not the file's content: the file read (`riir-infer/examples/i1221_t4_sampling_cell.rs`, a working-tree copy carrying a malformed Rust string literal from the agent's own prior edit) contains no "stop" text — but this fleet's agent instructions make "stop" the most salient token there is (the housekeeping law centers on ending a turn with a lone `<stop>` line and warns it is "rejected and re-prompted").
- The round trip preceding the flood also came back garbled relative to a normal tool-result envelope, so the context state at degeneration was already unhealthy.
- From inside the session the two candidate origins cannot be fully separated: (a) the MODEL's sampling fell into a repetition loop on the nearest high-prior attractor token once the state broke (most likely — model-side, GLM); (b) the HARNESS delivered a corrupted tool result that knocked the context into a bad state (zed-side). The preventive ask below is zed-side either way: whichever side originates it, the streaming path is where a flood must be caught.

## The ask (prevention)

1. **Repetition-loop breaker in the agent streaming path** (`crates/agent`, beside 077's per-turn tool-call guard): abort the turn when the same token or very short n-gram repeats N consecutive times (N ~= 50-100 — far above any legitimate repetition, far below the observed thousands), surfacing a clean `generation loop detected` error instead of streaming the flood.
2. **Optional single auto-retry** of the aborted turn from the last good tool result, so a transient sampling glitch costs one retry rather than the session.
3. **UI-side cap**: truncate + collapse a degenerate stream after ~200 identical lines with a `[repetition truncated]` marker — even with the breaker, the owner should never watch thousands of tokens stream.

## Repro

Not deterministic from outside; the trigger class is long agent sessions plus an odd/corrupted tool-result round trip. Evidence: this thread's transcript (the cancelled `read_file` call, session `zed-0a464098@amo`).

## Route

The streaming breaker belongs beside 077's guard family (same module, `crates/agent/src/thread.rs`, currently dirty from the 077 fix lane) — one review pass can carry both guards. Unclaimed here deliberately: the 077 lane (m3) holds the module's live lease; this issue yields to it rather than racing the same file.
