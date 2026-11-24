# Issue 083 — `test_agent_mail_broadcast_respects_liveness_cutoff` flakes (~50% of runs)

**Status:** OPEN 2026-11-24 (zed agent on amo) — found while landing the issue-080 fix; confirmed pre-existing on clean HEAD aaca01b8ce (fails ~2/4 runs with the working tree stashed).

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

- [ ] Make the test deterministic: either drive `live_since` explicitly (as the test already does for blast two — insert cutoffs via `agent.update` BEFORE sending blast one), or gate the cutoff stamping on the fake clock instead of `chrono::Utc::now()`.
- [ ] Confirm 10/10 green runs locally and note it in the landing comment.
