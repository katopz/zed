# 043 — Cross-rig agent mail (m3 ↔ amo/bmo ↔ 4090)
Status: DONE (phase 1) — m3/amo/bmo/4090 send+receive verified; inbound to m3 + Zed native wake tracked in `.issues/044` — impl `5a9284e937`

## Goal

Agents on different machines message each other the way same-rig agents do
(`SendMessage` for Claude Code, `send_message` for native Zed threads, plan 035):
M3 → M5 (amo/bmo), M3 → 4090, 4090 → M5, and under Work Remote (plan 037/042)
an agent running on the host is just an agent on that rig. Both Claude Code and
Zed agents must know the capability exists.

## Verdict: transport

| Option | Perf | Security | $ | Verdict |
|---|---|---|---|---|
| **SSH over Tailscale (LAN first), maildir on the target** | ~100–300 ms cold, ~10 ms with ControlMaster | reuses existing key auth + WireGuard; no new port, no daemon | 0 | **GOAT** |
| Tunnel Claude's local peer socket (`/tmp/cc-socks/<pid>.sock` + token) | best | private, undocumented, per-version; Claude-only | 0 | rejected: brittle |
| Claude Remote Control / cloud sessions | ok | routes via Anthropic cloud; Claude-only | plan quota | rejected: not Zed-native, not zero-dep |
| Tailnet TCP listener + blake3 MAC | ~2 ms | new port + shared secret + always-on daemon per rig | 0 | rejected: extra attack surface for latency nobody needs (agent turns are seconds) |
| Git repo / Cloudflare KV as bus | seconds / ok | ok | 0 / billing risk | rejected |

Delivery to the receiving agent (no daemon, files only):
- Claude Code: user-level hooks — `PostToolUse`/`UserPromptSubmit` inject mail as
  `additionalContext` mid-turn; `Stop` runs an `async` + `asyncRewake` waiter that
  wakes an **idle** session (exit 2) as soon as mail lands. One waiter per session.
- Zed native agents: run `agent-mail recv` from the terminal tool (AGENTS.md); an
  in-Zed inbox poller that wakes idle native threads is phase 2.

## Design

- `crates/agent_mail` (fork-owned, std + serde, cross-platform incl. Windows), bin `agent-mail`.
- Address `name@rig`; `name` = Claude session name (`~/.claude/sessions/$CLAUDE_PID.json`)
  or any label; `*@rig` / `@rig` = first free agent on that rig (`_any` inbox).
- Store `~/.agent-mail/inbox/<name>/{tmp,new,cur}` (maildir: write tmp → rename new;
  claim = rename new → cur, so two claimers never both get a message). Names are
  `[A-Za-z0-9._-]{1,64}` (path-traversal guard).
- Rigs in `~/.agent-mail/rigs.json`: `self` + per-rig ssh targets (tried in order) + `bin`.
- Remote send: `ssh -o BatchMode=yes <target> <bin> deliver <base64url envelope>` —
  argv, not stdin, so PowerShell (4090 default shell) and POSIX shells behave the same;
  base64url is shell-inert. Body capped at 16 KiB.

## Tasks

- [x] `crates/agent_mail`: envelope, maildir, rigs, ssh send, `deliver`, `recv`/`peek`, `whoami`, `rigs` ping. Success = the remote's `delivered <id>` line, not ssh's exit: the 4090's OpenSSH client run inside an sshd session delivers but never exits (same quirk as its git-over-ssh), so the client is killed once the marker arrives
- [x] `claude-hook` (inject) + `claude-hook --wait` (asyncRewake idle wake, single waiter via lock token; exits when the session pid dies or after 24 h)
- [x] unit tests (7): name/id traversal guard, claim-once, address parse, base64url shell-inert round trip, body limits, hook JSON shape, waiter exit 2 — pass on macOS and Windows
- [x] install on m3, amo, bmo (copied arm64 binary), 4090 (native build `C:\Users\katop\agent-mail-build`, standalone manifest); `rigs.json` on each (4090 LAN entry dropped: Tailscale already goes direct on LAN, the dead LAN IP cost 4 s per send)
- [x] Claude hooks in `~/.claude/settings.json` on each rig (merged, existing hooks kept): SessionStart+Stop async waiter, UserPromptSubmit+PostToolUse inject
- [x] teach agents: `~/.claude/CLAUDE.md` + Zed `AGENTS.md` section on m3, amo; CLAUDE.md on bmo, 4090. 4090 Zed AGENTS.md locked by its running Zed → staged (`.issues/044`)
- [x] e2e: m3→amo/bmo/4090, amo→bmo/4090, bmo→amo/4090, 4090→amo/bmo all deliver (4090 key added to amo/bmo `authorized_keys`, bmo key added to the 4090's `administrators_authorized_keys`). Claude inject path verified in Zed's Claude 2.1.310 (mail appeared as PostToolUse context); idle wake verified: idle peer woken by its waiter, replied `pong` in ~12 s. `* → m3` blocked: Remote Login off on m3 (`.issues/044`)
- [x] latency bench `.benchmarks/003_agent_mail_cross_rig_latency.md`
- [-] phase 2: Zed native inbox poller → `send_message_to_thread` for idle native threads (`.issues/044`)
