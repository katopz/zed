# Issue 060 — `rig log <SHORT-ID>` fails in the plan-057 binary; full UUID required

**Status:** OPEN — filed 2026-11-12 (zed-061faf21@amo) during the post-drain verification of plan 057.

## Symptom

`agent-mail rig log bmo 6a300a3e` (and every other short id tried — 11 of them,
including jobs with logs and done-records on disk) fails with:

```
agent-mail: no log for job 6a300a3e: No such file or directory (os error 2)
```

The log files exist on the rig, named by FULL UUID
(`~/.agent-mail/jobs/logs/01a1c9ac-80aa-7882-b833-6df26a300a3e.log`). The
short-id → full-UUID resolution that the pre-057 binary performed is gone or
broken in the tiered-queue rewrite (`zed f66b3e1646`, crates/agent_mail).
`rig status` short-id rendering still works; only `rig log` lookup is affected.
Done-record lookup by short id presumably has the same gap (the done dir is
full-UUID keyed too).

## Evidence

- 2026-11-12 bmo post-drain check: 11/11 short ids failed via `rig log`, while
  `ls ~/.agent-mail/jobs/done | grep <short>` matched all 11 full-UUID records
  — the data is fine, the resolver is not.
- Reproduce: any completed job id prefix vs its log path.

## Fix direction

Restore prefix-resolution in the `rig log` (and done-record) path: exact match
first, then unique-prefix match over the logs/done dirs; ambiguity (2+ matches)
→ list candidates and refuse, never guess.

## Open questions

- [ ] Fix the resolver in crates/agent_mail + a test (unique prefix resolves,
      ambiguous prefix refuses, missing id errors clean).
- [ ] Decide whether `rig cancel`/`rig set` accept short ids on the new binary
      (same resolver, same gap if so).
