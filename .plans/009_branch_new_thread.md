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
- [ ] Manual GUI smoke test — only viewport-coupled items remain (see below)

## Validation

| Check | Result |
|-------|--------|
| `cargo check -p agent -p agent_ui` | Clean |
| `cargo test -p agent --lib thread::` | 24/24 pass |
| `cargo test -p agent_ui --lib` (rewind, continuation) | Pass |
| `./script/clippy -p agent` (after adding the test) | Clean (`--deny warnings`, plus `cargo-machete` + `typos`) |
| `test_seed_history_forks_real_turns` (8 iterations) | Pass — native fork renders identically to source |
| Manual GUI smoke | Partially covered — see smoke-item breakdown below |

## Key Files

| File | Change |
|------|--------|
| `crates/agent/src/thread.rs` | `messages()` + `set_messages()` accessors |
| `crates/agent/src/agent.rs` | `NativeAgentConnection::seed_history()` + `test_seed_history_forks_real_turns` |
| `crates/agent_ui/src/conversation_view/thread_view.rs` | `branch_to_new_thread` rewrite, `[TOP]`/`[BOTTOM]`, checkpoint-row Branch button, turn-end call site |

## Manual smoke-test item breakdown

The four original manual smoke items, re-assessed for automated coverage:

| # | Item | Status | How covered |
|---|------|--------|-------------|
| 1 | `[TOP]`/`[BOTTOM]` scroll correctly | Manual only | Viewport-coupled: `scroll_to_top`/`scroll_to_end` mutate a `ListState` against laid-out content. No unit-testable seam; both target methods are pre-existing and well-exercised. |
| 2 | Checkpoint row shows both buttons | Manual only | Requires a full `ThreadView` render harness (workspace + panel + checkpoint-bearing user message) that the agent_ui test suite doesn't provide. Code inspection confirms Restore and Branch are siblings in the same `h_flex`. |
| 3 | Native Branch forks real history | **Automated** | `test_seed_history_forks_real_turns` forks session A's messages into session B via `seed_history` and asserts B's `acp_thread.to_markdown()` == A's, plus matching entry/message counts. Text-only turns chosen deliberately; tool-card replay is transitively covered by `test_replay_tool_call_replays_image_content` since both share `Thread::replay()`. |
| 4 | External agent transcript fallback | Manual only | Slicing + `to_markdown` + `ContentBlock` path is inline in `branch_to_new_thread`, tightly coupled to UI. Not cleanly extractable without a refactor; code inspection confirms the `AgentInitialContent::ContentBlock` construction. |

Only item #3 was cleanly automatable; the rest depend on a rendered viewport or an inline UI coupling that the test harness can't reach.

## TL;DR

Branch New Thread now forks *real* message history into a new native session
(replaying through the safe resume-from-DB path so tool cards/thinking blocks
survive), falls back to a verbatim transcript for external ACP agents, drops
the slow LLM-summarisation round-trip entirely, and gains `[TOP]`/`[BOTTOM]`
bookends on the prompt-jump strip. The checkpoint row shows both Restore and
Branch. Two cosmetic tradeoffs (token-counter reset, detached subscription)
are accepted as design decisions.
