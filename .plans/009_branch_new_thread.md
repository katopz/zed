# Branch New Thread (real history fork) + prompt-jump bookends

> **Status**: Implemented retroactively. Shipped as commit `76386141f5` on
> `develop` before this plan file existed; this document records the design for
> the trail.

## Problem

Two unrelated UI gaps in `ThreadView` (`agent_ui/src/conversation_view/thread_view.rs`):

1. **Prompt-jump strip had no end caps.** Commit `b84a400f78` added a
   `[1] [2] [3] ...` strip (one button per user prompt) that scrolled the
   matching prompt to the top of the viewport, but there was no way to jump to
   the absolute top or bottom of the thread without scrolling.

2. **"Branch New Thread" coexisted poorly with checkpoints.** Branch was only
   reachable from the turn-end separator, and it used
   `AgentInitialContent::ThreadSummary` — an `@mention` that the model
   summarises before continuing. That round-trip is slow, lossy, and useless
   when the user just wants an instant copy of the conversation so far.

## Solution

### A. Prompt-jump bookends
`render_user_prompt_jumps` now emits `[TOP] [1] [2] ... [BOTTOM]`:
- `[TOP]` → `scroll_to_top(cx)`
- `[BOTTOM]` → `scroll_to_end(cx)`

Same styling as the numbered buttons (XSmall label, muted color, hover bg).

### B. Branch on checkpoint rows
The checkpoint-row `h_flex` (previously "Restore Checkpoint" only) now also
renders a "Branch New Thread" button. Both buttons share the row.

### C. Eliminate LLM summarisation
`branch_to_new_thread` no longer constructs `ThreadSummary`. The native path
forks real history (D, below); the external-ACP path writes a verbatim
transcript into the composer via `AgentInitialContent::ContentBlock` with
`auto_submit: false` and `auto_prompt_enabled: false`. No model is invoked in
either case.

### D. Native history fork (the main work)
For native-agent threads (`ThreadView::as_native_thread` returns `Some`), fork
the *actual* `Thread.messages` into the new session so user/assistant turns,
tool cards, and thinking blocks survive as separate rendered entries.

Mechanism, reusing the canonical "resume from DB" path:
- `Thread::messages()` / `Thread::set_messages()` — read accessor + setter that
  also `clear_summary()` and `notify()`.
- `NativeAgentConnection::seed_history(session_id, messages, cx)` — looks up the
  session, calls `set_messages`, drives `thread.replay(cx)`, and pipes the
  resulting `ThreadEvent`s into the `AcpThread` through the existing
  `handle_thread_events`. Replay never re-executes tool calls (it only
  reconstructs UI state — see `thread.rs:1422`), so this is safe by
  construction.
- `branch_to_new_thread` orchestrates: capture the sliced message history
  (inclusive of the selected user message, or the whole thread for the
  turn-end button), create an empty session via the existing
  `external_thread` wiring, then seed. Because session establishment is async
  (the native thread isn't registered with `NativeAgent` until the connection
  establishes — see `create_agent_thread_inner` in `agent_panel.rs`), the seed
  is tried immediately and, if the thread isn't ready yet, deferred to a
  one-shot `RootThreadUpdated` subscription (mirrors the `model_override`
  path).
- `set_continued_from` is still called so the "from" chip from plan `007`
  survives.

### E. External ACP fallback
External ACP agents keep history server-side; ACP has no "fork at point"
protocol and replaying would re-execute tool calls. For these, the new thread
gets a flattened markdown transcript (entries up to the branch point rendered
via `entry.to_markdown(cx)`) as `ContentBlock` composer content. This is the
genuine ceiling for non-native agents.

## Design decisions (tradeoffs explicitly accepted)

- **Token-usage counter resets on the forked thread.** `set_messages` does not
  carry `request_token_usage`. A branched thread *is* a new session with a new
  context window; starting its usage at 0 is arguably correct, and the counter
  refreshes on the next turn anyway. Not worth coupling the fork path to usage
  bookkeeping for a cosmetic number.
- **Detached `RootThreadUpdated` subscription.** A `Cell<bool>` guard makes it
  one-shot, but the subscription object lives until the conversation view is
  dropped. This deliberately mirrors the existing `model_override` pattern
  rather than introducing a new lifetime-management scheme. Refactoring risks
  regressions for negligible gain.
- **Branch boundary is inclusive of the selected user message** (checkpoint-row
  button passes `Some(UserMessageId)`; the turn-end separator passes `None` =
  whole thread). Inclusive matches the user mental model of "branch *from here*,
  including this turn".

## Tasks

- [x] `[TOP]` / `[BOTTOM]` bookends in `render_user_prompt_jumps`
- [x] "Branch New Thread" button on the checkpoint row (alongside Restore)
- [x] Remove `ThreadSummary` mention path from `branch_to_new_thread`
- [x] `Thread::messages()` read accessor
- [x] `Thread::set_messages()` setter (with `clear_summary` + `notify`)
- [x] `NativeAgentConnection::seed_history()` (set_messages + replay + handle_thread_events)
- [x] Rewrite `branch_to_new_thread` for native fork + transcript fallback
- [x] Preserve continuation "from" chip via `set_continued_from`
- [x] Handle async session-ready timing (immediate-try + one-shot `RootThreadUpdated`)
- [x] `cargo check -p agent -p agent_ui`
- [x] `cargo test -p agent --lib thread::`
- [x] `cargo test -p agent_ui --lib` (rewind, continuation)
- [x] `./script/clippy -p agent -p agent_ui` (mandatory stronger linter)
- [x] Automated test `test_seed_history_forks_real_turns` (covers native fork of real history)
- [x] Extract `slice_messages_for_branch` helper from `branch_to_new_thread` (native inclusive-boundary logic)
- [x] Extract `transcript_entry_count` helper from `branch_to_new_thread` (external transcript slicing logic)
- [x] Unit tests for `slice_messages_for_branch` + `transcript_entry_count` (9 tests in `branch_boundary_tests`)
- [x] `./script/clippy -p agent_ui` (after extraction + tests)
- [ ] Manual GUI smoke test — only pure-rendering items remain (scroll viewport, button presence in element tree)

## Validation

| Check | Result |
|-------|--------|
| `cargo check -p agent -p agent_ui` | Clean |
| `cargo test -p agent --lib thread::` | 24/24 pass |
| `cargo test -p agent_ui --lib` (rewind, continuation) | Pass |
| `./script/clippy -p agent` (after adding the test) | Clean (`--deny warnings`, plus `cargo-machete` + `typos`) |
| `test_seed_history_forks_real_turns` (8 iterations) | Pass — native fork renders identically to source |
| `branch_boundary_tests` (9 tests) | Pass — inclusive boundary, whole-thread fork, not-found fallback, first-match semantics |
| `./script/clippy -p agent_ui` (after extraction + tests) | Clean (`--deny warnings`, plus `cargo-machete` + `typos`) |
| Manual GUI smoke | Only pure-rendering items remain (see smoke-item breakdown below) |

## Key Files

| File | Change |
|------|--------|
| `crates/agent/src/thread.rs` | `messages()` + `set_messages()` accessors |
| `crates/agent/src/agent.rs` | `NativeAgentConnection::seed_history()` + `test_seed_history_forks_real_turns` |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `branch_to_new_thread` rewrite, `[TOP]`/`[BOTTOM]`, checkpoint-row Branch button, turn-end call site, `slice_messages_for_branch`/`transcript_entry_count` helpers + `branch_boundary_tests` |

## Manual smoke-test item breakdown

The four original manual smoke items, re-assessed for automated coverage:

| # | Item | Status | How covered |
|---|------|--------|-------------|
| 1 | `[TOP]`/`[BOTTOM]` scroll correctly | Manual only | Viewport-coupled: `scroll_to_top`/`scroll_to_end` mutate a `ListState` whose item count is only synced during the `list()` element's layout pass (no agent_ui test renders/draws). Without rendering, `scroll_to_end` resolves to `item_ix == 0`. The target methods themselves are pre-existing and well-exercised, so only the wiring (TOP→`scroll_to_top`, BOTTOM→`scroll_to_end`) is at risk — verified by code inspection. |
| 2 | Checkpoint row shows both buttons | Logic automated, render manual | Requires a full `ThreadView` render harness (workspace + panel + checkpoint-bearing user message) that the agent_ui test suite doesn't provide (no `cx.draw` usage anywhere in the crate). The **branch-boundary logic** the checkpoint-row Branch button depends on is extracted into `slice_messages_for_branch` and covered by 5 unit tests. Code inspection confirms Restore and Branch are siblings in the same `h_flex`. |
| 3 | Native Branch forks real history | **Automated** | `test_seed_history_forks_real_turns` forks session A's messages into session B via `seed_history` and asserts B's `acp_thread.to_markdown()` == A's, plus matching entry/message counts. Text-only turns chosen deliberately; tool-card replay is transitively covered by `test_replay_tool_call_replays_image_content` since both share `Thread::replay()`. |
| 4 | External agent transcript fallback | Logic automated, render manual | The `to_markdown`/composer wiring is inline and needs rendering. The **transcript slicing logic** is extracted into `transcript_entry_count` and covered by 4 unit tests (inclusive boundary, whole-thread fallback for `None`, whole-thread degradation for unknown id, first-match semantics). |

The pure-rendering aspects (buttons actually appearing in the element tree, scroll actually moving the viewport) genuinely require a render/draw harness that no agent_ui test uses; building one would pioneer brittle infrastructure outside this codebase's conventions. The **logic** behind items 2–4 is now fully extracted and unit-tested, including a regression the extraction caught (`None` up-to must fork the whole conversation, not fall through to transcript).

## TL;DR

Branch New Thread now forks *real* message history into a new native session
(replaying through the safe resume-from-DB path so tool cards/thinking blocks
survive), falls back to a verbatim transcript for external ACP agents, drops
the slow LLM-summarisation round-trip entirely, and gains `[TOP]`/`[BOTTOM]`
bookends on the prompt-jump strip. The checkpoint row shows both Restore and
Branch. Two cosmetic tradeoffs (token-counter reset, detached subscription)
are accepted as design decisions.
