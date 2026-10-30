# 044 — agent-mail: waking idle Zed native threads
Status: OPEN — plan 043 phase 2; m3 inbound + 4090 Zed AGENTS.md DONE 2026-10-30 (see plan 043)

## Zed native threads (phase 2)
Claude Code sessions are woken by the async `Stop` waiter hook. Native Zed
threads have no hooks; they read mail via `agent-mail recv --as <name>`. Wake
them like plan 035's mailbox does: a NativeAgent background task polls
`~/.agent-mail/inbox/<thread name>/new` (500 ms, same as the waiter) and feeds
claimed mail through `send_message_to_thread` (deliver when idle, queue when
generating). Reuse `agent_mail::Mailroom::claim` + `render` (lib crate).
- [ ] decide thread naming (title slug vs session id) so senders can address it
- [ ] poller in `crates/agent` behind a setting, off by default
- [ ] test: idle native thread wakes on mail; generating thread queues
