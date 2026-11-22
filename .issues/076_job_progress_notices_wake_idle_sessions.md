# Issue 076 — agent-mail job PROGRESS notices (STARTED, POISONED-still-running) wake idle Claude sessions

**Status:** FIXED 2026-11-22 (katgpt-rs-8d@amo) — see Fix below; issue kept for the install step.

## Symptom

A Claude session that submits rig jobs is woken by the async `Stop` hook (`agent-mail claude-hook --wait`, asyncRewake) for EVERY job notice: STARTED, POISONED (job still running), then DONE/FAILED. Each wake is a full model turn; with the owner's every-response `## Summary` rule, an amo coordinator session emitted ~25 near-identical summaries in one afternoon for no decision (owner report, 2026-11-22). Each exclusive job costs up to 3 wakes, and only one of them (the terminal) needs a decision.

## Root cause

`jobs.rs::job_mail` sent every notice with the job's `spec.notify_mode` (default = a waking mode). The hook already honours `MailMode::Fyi` (never forces a turn on `Stop`/`SubagentStop`; delivered at the next tool/prompt boundary) — job progress notices just never used it.

## Fix

- [x] `NoticeKind { Progress, Terminal }` + `notice_mode()`: STARTED and first-POISON → `MailMode::Fyi`; terminal (DONE/FAILED/CANCELLED/TIMEOUT) keeps `spec.notify_mode`. Unit test pins the table for all three configured modes. agent_mail tests 103 + 6 green, clippy `-D warnings` clean.
- [ ] Install: `cargo build --release -p agent_mail` → `~/.cargo/bin/agent-mail` on amo at a quiet moment, then ship the binary to bmo / 4090 (the orchestrator lane's usual install path; same pending install as sprint!087's `6e069c4812`).

## Not in scope (noted)

The subagent "waiting on its own background work" task notifications are Claude Code harness events, not agent-mail; they also wake the parent. That noise is upstream, not fixable here.
