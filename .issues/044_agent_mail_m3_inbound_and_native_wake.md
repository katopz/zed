# 044 — agent-mail: inbound to m3 + waking idle Zed native threads
Status: OPEN — plan 043 follow-ups; m3 inbound waits on the owner (Remote Login)

## m3 inbound (owner decision)
Every link works except `* → m3`: macOS Remote Login is off on the m3
(`ssh kat@100.125.16.103` times out). Enable with System Settings → General →
Sharing → Remote Login (or `sudo systemsetup -setremotelogin on`), then append
the amo/bmo/4090 public keys to `~/.ssh/authorized_keys` on the m3. Until then
m3 agents can send but replies must be read on the other rig, or the other agent
replies by a rig the m3 can poll.

## 4090 Zed AGENTS.md
`%APPDATA%\Zed\AGENTS.md` is held open by the running Zed on the 4090, so the
mail section is staged at `C:\Users\katop\agent-mail-build\AGENTS.md.with-mail`;
copy it over when Zed there is closed. (Claude's `~/.claude/CLAUDE.md` there is done.)

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
