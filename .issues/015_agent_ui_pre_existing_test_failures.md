# Issue 015: two pre-existing agent_ui test failures on develop

Found while validating plan 025 (auto-allow countdown). Both fail on clean
develop (HEAD~1 of 3ffef69ab4, i.e. BEFORE the plan-025 changes), reproduced
twice each. Not regressions from 025 — but they block "green suite" gates.

## 1. `conversation_view::tests::test_close_session_returns_error_when_unsupported`

- Fails deterministically (solo and full suite).
- Panics at the `result.is_err()` assert: `close_session()` returns Ok even
  though `supports_close_session()` is false.
- Test originates from upstream #51479 (merged via 8823d2bcea). Likely
  fallout from that 369-commit upstream merge or the local auto_prompt
  commits (019/021) that touched conversation_view.

## 2. `conversation_view::tests::test_watchdog_does_not_fire_during_active_stream`

- Passes solo (3/3), fails in full-suite runs (2/2 full runs, with AND
  without plan-025 changes).
- Asserts watchdog didn't fire while streaming (`left: 1, right: 0`) —
  suggests real-time/load sensitivity under parallel test execution.

## Repro

```sh
cargo test -p agent_ui                       # both fail
cargo test -p agent_ui test_watchdog_does    # passes solo
cargo test -p agent_ui test_close_session    # fails solo
```

## Suspected areas

- close_session: capability check path in `AgentConnection` /
  `StubAgentConnection` vs upstream behavior change.
- watchdog: real-`Instant` vs fake-clock mixing under load.

Status: open (not blocking plan 025; fixed separately).
