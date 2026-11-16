# 067 — verdict reviewer: opt-in "cheap executor + Fable advisor" route for the `claude_code` lane

Status: OPEN — filed 2026-11-16 (zed-b3@m3), owner-directed. Implementation in flight in the same session.

## What

`request_verdict`'s `claude_code` reviewer runs an off-screen Claude Code session on the user's default
model (Fable), so every verdict spends Fable weekly quota. Claude Code's server-side advisor tool lets a
cheaper executor (Sonnet) consult Fable mid-turn. The owner's PSA reading is that advisor use does not
count against the Fable weekly limit. Goal: let the reviewer lane run `executor = sonnet, advisor = fable`
behind an opt-in setting, keep today's lane untouched as the default and as the fallback.

A GLM (native) thread cannot call the advisor itself: it is a server tool inside a Claude session, not an
endpoint. The only route is GLM -> `request_verdict` -> `claude_code` reviewer session.

## Evidence (2026-11-16, m3)

- Zed's ACP lane runs Claude Code 2.1.323 (`external_agents/registry/claude-acp/v_0.107.0_*`), which has
  `--advisor` / `advisorModel`. The PATH `claude` (2.1.89) predates Fable; do not trial with it.
- `claude -p --model sonnet --advisor fable` works on this account: stream shows `server_tool_use advisor`,
  usage `iterations` carries `{"type":"advisor_message","model":"claude-fable-5-2"}`. The advice itself is
  returned encrypted (`advisor_redacted_result`), so it cannot be logged or scored directly.
- `claude-agent-acp` spreads `_meta.claudeCode.options` from `session/new` into the SDK options
  (`model`, `settings`, `env`), and surfaces the advisor call as a tool call named `advisor`.

## Design

- Setting `agent.verdict_advisor = { "executor_model": "sonnet", "advisor_model": "fable" }`, unset by
  default (the GOAT gate: default-off until bench 001 says otherwise).
- `AgentConnection::new_session_with_meta` carries `_meta` on `session/new`; connections that cannot
  carry it refuse instead of dropping it silently.
- A session created with caller-pinned options skips the user's saved default config options (a saved
  default model would put Fable back in the executor seat).
- Any failure to create the advisor session falls back to today's plain lane and logs why.
- Reviewer label reports the route actually used (`claude_code` vs `claude_code+advisor`) so
  `script/verdict_scorer.py` can split the two populations.

## Tasks

- [ ] `new_session_with_meta` on `AgentConnection` + v1 ACP implementation
- [ ] `agent.verdict_advisor` setting
- [ ] `ClaudeCodeReviewer` advisor route + fallback + label
- [ ] Tests: meta reaches `session/new`; defaults skipped; fallback on refusal
- [ ] Live trial: one real verdict through the advisor route, advisor call observed
- [-] Measure the PSA claim (Fable weekly meter before/after N advisor verdicts) — needs the owner's usage page
- [-] Bench 001 A/B (agreement quality + abort rate, advisor route vs plain) before any default flip

## Caveats

- The executor's tokens still hit the normal session window; issue 037's 5-hour failures are not fixed by this.
- The verdict is written by the executor with Fable's advice; the executor decides whether to consult.
- Pairing rules exist ("cannot be used as an advisor when the request model is ..."); only sonnet+fable is proven.

## Refs

- `crates/agent_ui/src/verdict_reviewer.rs`, `crates/agent/src/tools/request_verdict_tool.rs`
- `.issues/037_verdict_reviewer_lane_reliability.md`, `.benchmarks/001_verdict_ping_pong_goat.md`
