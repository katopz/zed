# 011: Manual auto-prompt static dispatch + pre-gathered start context

- **Commits**: `6606142b41` (static manual path + start context), `96d9895e7b` (periodic machine sampler)
- **Follows**: `.docs/008` (unified overflow flow), `.plans/027` (summary-first fast path, `CONTINUE_REMAINS_DECISION`)
- **Status**: implemented and closed
- **Date**: 2026-08-30

## Symptom

Clicking the manual sparkle button called the orchestrator LLM and processed
for a while, but the verdict was effectively always the fixed directive
("Continue remains and make decisions for best perf/sec prod grade") — the
summary-first fast path (plan 027) already owned that decision whenever the
last message was a voluntary summary. The LLM round-trip was pure latency for
a predetermined answer.

Separately, every continuation prompt forced the worker agent to spend tool
calls at turn start probing the machine (`nvidia-smi`, `powermetrics`) and
polling the agent board to learn what sibling agents were doing.

## Fix

1. **Manual click = static dispatch, zero LLM** (`6606142b41`)
   - `on_manual_auto_prompt` short-circuits past the entire decide phase
     (no plan/doc reads, no orchestrator call) inside `run_auto_prompt` and
     dispatches the fallback action immediately.
   - The fallback prompt is now `CONTINUE_REMAINS_DECISION` (shared with the
     summary fast path + overflow Phase 2), grammar-refined:
     "Continue remaining work with best perf/sec, prod-grade decisions
     (SOLID, DRY); file issues/plans as needed to ensure full coverage. If
     nothing remains, check other repos for unfinished tasks or do
     housekeeping (doc-sync, riir-clippy mining, code-smell hunt, online
     search to distill perf/sec or unblock deferred work)."
   - `dispatch_action` still chooses same-thread vs new-thread from the token
     count, so an overflowing thread forks instead of looping.

2. **Start context stamped onto every continuation prompt** (`6606142b41`)
   - `start_context_block` (agent_ui/src/auto_prompt/mod.rs) assembles:
     - Machine line via `system_specs::machine_context_line`: hostname/OS,
       CPU brand + cores + usage, RAM used/total, GPU name
       (`window.gpu_specs()`), power/AC state (`pmset` on macOS, sysfs on
       Linux, omitted elsewhere).
     - Local sibling agents actively generating
       (`AgentPanel::active_thread_activity`): title + 160-char snippet of
       the latest assistant message.
     - Remote board peers via `peer_states::unmuted_states_for_context()`
       (existing substrate, reused — no new plumbing).
   - Appended to BOTH same-thread continuations and new-thread first prompts,
     auto AND manual. The worker makes resource/fleet-aware decisions with
     zero probing tool calls.

3. **Periodic machine sampler** (`96d9895e7b`)
   - `system_specs::spawn_periodic_sampler`, spawned once per process from
     `agent_ui::init`: 15s interval; power probe throttled to ~1/min
     (last-known state carried between probes). Prompt building only reads
     the cache — never blocks the main thread, and the first prompt after
     startup already has real CPU/RAM numbers.

## Test/verify

- `test_manual_auto_prompt_dispatches_static_continuation` asserts zero
  orchestrator completions on click + immediate directive dispatch (replaces
  `test_manual_auto_prompt_consults_orchestrator`).
- clippy clean (deny warnings) for `system_specs`, `auto_prompt`, `agent_ui`;
  auto_prompt 394/394, system_specs 2/2 pass.
- agent_ui has a machine-specific nondeterministic flake (foreign
  `async-io`/`async-process` threads trip the deterministic test scheduler);
  affects clean `develop` equally and the failing subset rotates between
  runs — pre-existing, not caused by this work.

## Follow-ups

- None open. Windows power probe landed in this doc's scope
  (`GetSystemPowerStatus`, `Win32_System_Power`) — compile-unverified locally
  (no Windows toolchain on the M3 build host; API usage verified against the
  vendored windows-0.61 crate sources); zed's Windows CI build will confirm.
