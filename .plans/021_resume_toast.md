# 021 — Resume toast when the scheduled dispatch fires (plan 020 follow-up)

## Tasks
- [x] `AutoPromptDelayReason` (`UsageLimitReset` | `Backoff`) carried on
      `AutoPromptDecision::DispatchAfterDelay` so the UI can distinguish a
      limit schedule from a refusal backoff
- [x] `notify_with_sound` made `pub(crate)`; `run_auto_prompt` fires
      "Usage limit window reset — auto-continue resuming" (Info icon) right
      before the delayed dispatch submits — only for `UsageLimitReset`, only
      when not cancelled and the view is alive
- [x] Reason logged in the DispatchAfterDelay scheduling line
- [x] Validation: `cargo clippy -p auto_prompt -p agent_ui --all-targets
      --deny warnings` clean; auto_prompt 344+40 tests pass;
      `./script/clippy -p agent_ui` (release, all features, all targets,
      deny warnings) passes — webrtc-sys extraction unblocked by deleting
      the poisoned 27G dir left by the earlier disk-full incident

## Notes
- Toast mirrors the schedule notification ("… limit reached — auto-continue
  scheduled at …") through the same `notify_with_sound` channel (sound +
  visibility rules + notify_when_agent_waiting settings).
- `cargo machete` (run by script/clippy after clippy) reports pre-existing
  unused deps in `language_models` / `agent_board` — untouched by this plan.

## Outcome
Commit: see `git log` (feat(021)).
