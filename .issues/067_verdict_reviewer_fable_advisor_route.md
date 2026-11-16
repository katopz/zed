# 067 — verdict reviewer: opt-in "cheap executor + Fable advisor" route for the `claude_code` lane

Status: LANDED default-off 2026-11-16 (zed-b3@m3) — `agent.verdict_advisor` routes the `claude_code` reviewer through executor+advisor; contract proven against the real adapter (0.107.0). Open: first verdict through a rebuilt Zed, the PSA quota measurement, bench 001 A/B. Code: `2db38f44e8`.

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

- [x] `new_session_with_meta` on `AgentConnection` + v1 ACP implementation (`AcpConnection::create_session`)
- [x] `agent.verdict_advisor` setting (`VerdictAdvisorSetting`)
- [x] `ClaudeCodeReviewer` advisor route + fallback + label (`claude_code+advisor` via `VerdictReviewer::session_label`)
- [x] Tests: meta pinned on the session, fallback on refusal, plain session while unset (`verdict_reviewer.rs`); meta merge (`claude_session.rs`, `v1.rs`)
- [ ] Test that a pinned session skips saved default config options end to end (needs a fake agent that answers `session/new` with config options; today covered by reading `create_session` only)
- [x] Contract trial against the real `claude-agent-acp` 0.107.0 over stdio with the exact `_meta` Zed sends: executor ran `claude-sonnet-5-5` (`model_usage`), the `advisor` tool call fired, reply started `#Verdict: AGREE`
- [ ] First real `request_verdict` through a rebuilt Zed with the setting on (the installed Zed Dev build predates this change)
- [-] Measure the PSA claim (Fable weekly meter before/after N advisor verdicts) — needs the owner's usage page
- [-] Bench 001 A/B (agreement quality + abort rate, advisor route vs plain) before any default flip

## How to turn it on

    "agent": { "verdict_advisor": { "executor_model": "sonnet", "advisor_model": "fable" } }

Needs a Zed build containing this change. Scorer rows carry `reviewer: "claude_code+advisor"`.

## Caveats

- The session's model picker reads `default` even though the executor is pinned; the session is hidden, so this is cosmetic.

- The executor's tokens still hit the normal session window; issue 037's 5-hour failures are not fixed by this.
- The verdict is written by the executor with Fable's advice; the executor decides whether to consult.
- Pairing rules exist ("cannot be used as an advisor when the request model is ..."); only sonnet+fable is proven.

## Refs

- `crates/agent_ui/src/verdict_reviewer.rs`, `crates/agent/src/tools/request_verdict_tool.rs`
- `.issues/037_verdict_reviewer_lane_reliability.md`, `.benchmarks/001_verdict_ping_pong_goat.md`
