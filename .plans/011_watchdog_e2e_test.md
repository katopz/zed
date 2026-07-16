# Plan 011: Watchdog E2E Test

- [x] Write E2E test for the stuck-thread watchdog
- [x] Write regression test for watchdog timer reset across generations (issue 004)
- [x] Explore codebase (done in prior session)
- [x] Design test approach (done in prior session)
- [x] Tests pass (30 iterations × 2 tests, default parallelism)
- [x] Clippy clean

## Context

The watchdog implementation (commits `c670e9f73f`, `b1560f34eb`, `97b74b3446` on
`develop`) is unit-tested (9 tests in `watchdog.rs`) but has no end-to-end test
that exercises the full flow: stuck worker → watchdog fires → reasoning LLM →
halt → cancel + inject timeout notice.

This is the only substantive remaining work item from the watchdog feature.

## Test Harness Already Available

| Component | Location | Why it works |
|-----------|----------|-------------|
| `StubAgentConnection` | `crates/acp_thread/src/connection.rs:~880-1013` | With no `next_prompt_updates`, its `prompt()` hangs on a oneshot channel until `end_turn()` — exactly the "stuck worker" scenario from `bug1.md` |
| `FakeLanguageModel` | `crates/language_model/src/fake_provider.rs` | `stream_completion` registers a pending completion in `current_completion_txs`; `send_last_completion_stream_text_chunk` / `end_last_completion_stream` feed the response |
| `init_test` | `crates/agent_ui/src/conversation_view.rs:5704-5716` | Already calls `LanguageModelRegistry::test(cx)` which sets up the `FakeLanguageModelProvider` as the default model |
| `invalidate_config_cache` | `crates/auto_prompt/src/auto_prompt.rs:156` | Public function to clear the cached `AutoPromptConfig` so env var override takes effect |
| `ZED_AUTO_PROMPT_WATCHDOG_TIMEOUT_SECS` | env var read in `config.rs:194-197` | Override the 600s default to a test-friendly value |
| `advance_clock` | `gpui::BackgroundExecutor::advance_clock` | Fire mock timers; used via `cx.executor().advance_clock(Duration)` |

## Test Design

**Location**: `crates/agent_ui/src/conversation_view.rs` — append to the
`pub(crate) mod tests` block (before line 9518, the module closing `}`).

**Name**: `test_watchdog_halts_stuck_thread`

### Steps

1. **Override config**: `std::env::set_var("ZED_AUTO_PROMPT_WATCHDOG_TIMEOUT_SECS", "1")`
   + `auto_prompt::invalidate_config_cache()` so the next `load_config_cached()`
   re-reads from env.

2. **Init**: `init_test(cx)` — sets up the registry with `FakeLanguageModelProvider`.

3. **Get model handle**: grab the `FakeLanguageModel` from the global registry:
   ```rust
   let fake_model = cx.update(|_window, cx| {
       LanguageModelRegistry::read_global(cx)
           .default_model()
           .expect("no default model")
           .model
           .clone()
   });
   ```
   Need `use language_model::LanguageModel;` in scope for `.as_fake()`.

4. **Stuck worker**: `StubAgentConnection::new()` with no `next_prompt_updates`
   → its `prompt()` will hang forever (never calls `end_turn`).

5. **Setup view**: `setup_conversation_view(StubAgentServer::new(connection.clone()), cx)`.

6. **Enable auto_prompt**:
   ```rust
   active_thread(&cv, cx).update(cx, |tv, cx| { tv.auto_prompt_enabled = true; cx.notify(); });
   ```

7. **Send message**: set editor text + `view.send(window, cx)`.

8. **`cx.run_until_parked()`** — thread is `Generating`, watchdog task is armed.

9. **Assertions (pre-timeout)**:
   - `thread.status() == ThreadStatus::Generating`
   - `view._watchdog_task.is_some()`

10. **Fire timer**: `cx.executor().advance_clock(Duration::from_secs(2))` (1s timeout + margin).

11. **`cx.run_until_parked()`** — watchdog timer fires → gathers context →
    calls `FakeLanguageModel::stream_completion` → parks on `stream.next().await`.

12. **Feed halt response**:
    ```rust
    fake_model.as_fake().send_last_completion_stream_text_chunk(
        r#"{"action":"halt","reason":"test: simulated hang"}"#
    );
    fake_model.as_fake().end_last_completion_stream();
    ```

13. **`cx.run_until_parked()`** — watchdog reads halt decision →
    `thread.cancel(cx)` (sends `Cancelled` on response_tx → resolves the hanging prompt) →
    awaits cancel task → `Stopped(Cancelled)` queued →
    `dispatch_action` sends timeout notice to same thread → new `Generating` cycle.

14. **Assertions (post-timeout)**:
    - `thread.status() == ThreadStatus::Generating` (from the new timeout-notice turn)
    - Thread markdown contains `"Watchdog timeout"`:
      ```rust
      let md = active_thread(&cv, cx).read_with(cx, |view, cx| view.thread.read(cx).to_markdown(cx));
      assert!(md.contains("Watchdog timeout"));
      ```

15. **Cleanup**:
    ```rust
    std::env::remove_var("ZED_AUTO_PROMPT_WATCHDOG_TIMEOUT_SECS");
    auto_prompt::invalidate_config_cache();
    ```

### Verification Points the Test Proves

- Watchdog is actually armed on `send_content` (the bug1.md fix)
- Timer fires and calls the reasoning LLM
- Halt decision triggers `thread.cancel()` — worker is actually cancelled
- Timeout notice is dispatched to the same thread
- A new generation starts from the timeout notice

## Risks / Notes

- **Env var race in parallel tests**: `#[gpui::test]` runs in parallel. The
  other 3 auto_prompt-related tests call `load_config_cached()` and could pick
  up `watchdog_timeout_secs=1` if they race with this test's set_var. This is
  **harmless** because those tests call `end_turn()` promptly, so their watchdog
  never fires (and even if it did, the reasoning call would fail → default
  `Continue`).
- **60s reasoning timeout**: `reason_about_stuck_thread` has its own 60s inner
  timeout (`REASONING_TIMEOUT_SECS`). Since we feed the response immediately
  after `run_until_parked()` without advancing the clock further, this timeout
  never fires.
- **No `advance_clock` past the reasoning timeout**: if the test ever hangs,
  it means the watchdog didn't reach `stream_completion` — check that
  `pending_completions().len() == 1` after step 11.
- **`FakeLanguageModel` not `Clone`**: keep the `Arc<dyn LanguageModel>` and
  call `.as_fake()` on a deref each time you need to interact.
- **`auto_prompt::` name resolution**: in `conversation_view::tests`, bare
  `auto_prompt::` resolves to the **external crate** (not `crate::auto_prompt`,
  which is the agent_ui module). Verify at compile time.

## Implementation Notes

Two tests landed in `conversation_view.rs`:

1. **`test_watchdog_halts_stuck_thread`** — the full HALT flow from the design
   above. Sets a 1s watchdog window via env var, hangs the worker, advances the
   mock clock 2s, feeds a halt JSON decision, asserts the worker was cancelled
   and a timeout-notice turn replaced it.

2. **`test_watchdog_resets_across_generations`** — regression test for issue 004
   (commit `b32d7ea083`). Verifies the watchdog task lifecycle across two
   sequential generations: armed → cancelled (None) on normal stop → re-armed
   (Some) on the next send. This directly covers Bug A (cancel by session_id)
   and Bug B (arm replaces stale task).

### Env-var race avoidance

`test_watchdog_halts_stuck_thread` sets `ZED_AUTO_PROMPT_WATCHDOG_TIMEOUT_SECS=1`.
The reset test deliberately does NOT set any env var — it relies on the default
600s window (never fires since the clock is never advanced). This avoids a
global env-var race when the two tests run in parallel. A `WatchdogEnvGuard`
ensures the HALT test's env vars are cleaned up on drop (including panics).

## What This Does NOT Cover

- **`continue` decision path**: this test only covers `halt`. A `continue` test
  would verify the watchdog reschedules. Could be added as a second test case
  that feeds `{"action":"continue",...}` and advances the clock again.
- **Underlying stream-hang root cause**: out of scope — the hang is inside the
  ACP server process, not this codebase.
- **Retry path arming**: `retry_generation` now arms the watchdog (commit
  `97b74b3446`), but this test exercises the `send_content` path. A separate
  test would be needed for the retry path.

## TL;DR

Write one async test in `conversation_view.rs` that: sets a 1s watchdog timeout
via env var, opens a stub thread that hangs forever, sends a message, advances
the mock clock 2s, feeds a halt JSON response to the `FakeLanguageModel`, then
asserts the worker was cancelled and a timeout-notice turn replaced it.
