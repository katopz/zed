# Issue 083 — `test_agent_mail_broadcast_respects_liveness_cutoff` flakes (~50% of runs)

**Status:** FIXED 2026-11-25 (zed-504dde51@amo) — the explicit-cutoff arm of the issue's own fix direction landed: both sessions' `live_since` cutoffs are stamped via `agent.update` at `now − 1 min` BEFORE the first blast (the poller stamps with `or_insert(now)`, so the explicit entry wins and is never overwritten; the blast-two arm's existing pattern). The same-millisecond `sent_at >= cutoff` tie that dropped blast one from the live thread is structurally gone — every later blast is strictly post-cutoff. 10/10 local runs green.
**Lease closed:** zed-504dde51@amo, claimed + landed same session (2026-11-25).

## Symptom

`cargo test -p agent test_agent_mail_broadcast_respects_liveness_cutoff` fails about half the runs (fails 2/4 stashed, fails 2/3 with the 080 work tree):

```
thread 'tests::thread_mailbox_tests::test_agent_mail_broadcast_respects_liveness_cutoff' panicked:
assertion failed: `(left == right)` — left: 0, right: 1
```

The failing assert is the FIRST one after the first blast: `fake.pending_completions_for(&live_thread.model).len() == 1` — the broadcast wake did not start a turn within the pumped window.

## Likely cause

The test itself notes the race it fights: broadcast cutoffs are stamped with the REAL clock (`chrono::Utc.now()` in `live_since` and in `take_broadcasts_since`) while the poller runs on the executor's fake timers. `enable_agent_mail` + `advance_clock(1s)` tries to stamp cutoffs "before any mail is sent", but `run_until_parked` may drain zero or more poller ticks depending on timer phase; if the blast lands and is claimed in the same real-millisecond window the cutoff was stamped in, the `sent_at >= cutoff` comparison can drop the blast (or the stamp can land after the claim), so the expected wake count is 0 instead of 1.

## Tasks

- [x] Make the test deterministic — LANDED (test-only, no prod change): explicit `live_since` cutoffs at `now − 1 min` for BOTH threads via `agent.update` before blast one; the poller's `or_insert(now)` never overwrites an explicit entry (verified in `agent_mail_bridge.rs`), and the minute margin removes the real-clock tie class. The fake-clock alternative (gating `live_since` stamping on the executor clock) would have been a prod-code change touching the bridge's sync loop — rejected as the larger blast radius for a test-scoped race.
- [x] 10/10 green runs confirmed locally (2026-11-25, zed-504dde51@amo, debug lib posture, isolated /tmp target).
