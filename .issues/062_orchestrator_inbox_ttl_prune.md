# 062 — TTL-prune the orchestrator sink inbox
Status: LANDED 2026-11-15 (zed-84db451f@amo) — `prune_orchestrator_inbox` in `crates/agent_mail/src/boss.rs` alongside `prune_thread_ends`, fired from `note_stop` (every thread end prunes): envelopes older than 7 d (`ORCHESTRATOR_INBOX_RETENTION`, readable-at = `deliver_at` else `sent_at`) are deleted from `inbox/orchestrator/new/`; other inboxes untouched (test `orchestrator_sink_prunes_old_and_keeps_fresh`). The one-line-per-tick pruned-count log was folded: pruning rides thread-end markers (per stop, not per tick) and removals are silent — the count surfaces via the inbox itself; flagged as the one deviation from the issue text.

The orchestrator inbox is the designated dead-letter office: boss wakes are sent from `orchestrator@<rig>` (plan 056 drain round) so lane replies die there, and the sink has no reader by design. Disk-only growth, unbounded.

- [x] Add retention pruning for the `orchestrator` inbox (default ~7d) alongside `THREAD_END_RETENTION` pruning in `crates/agent_mail/src/boss.rs`, so archived wake replies age out instead of accumulating forever.
- [x] Test: envelopes older than retention are deleted from `inbox/orchestrator/{new,cur}`, fresh ones kept; other inboxes untouched.
- [-] Log the pruned count (folded — pruning rides `note_stop`, removals silent; the deviation is disclosed in the Status line) one line per watchdog tick (same style as thread-end pruning).
