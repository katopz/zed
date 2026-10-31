# 048 — amo as rig orchestrator: zero-token watchdog + exception-driven LLM
Status: DONE — W1 + W2 live on amo (launchd, runtime A via Zed's bundled Claude Code, sonnet, lean context ~$0.006/run)

## Goal
amo (always on, builder) becomes the orchestrator instead of m3/the owner:
every 5 minutes it checks all rigs so a duplicate, drift, idle rig or stalled
agent never lasts more than ~5 minutes, and it mails the agents involved to
resolve it or take other work. Token cost must stay small.

## Verdict
**Split the check from the judgement.**

| Layer | Runs | Cost | Does |
|---|---|---|---|
| **Watchdog** (`agent-mail rig watch --once`, plain program) | every 5 min via launchd on amo | **0 tokens**, ~0.5 s | `rig status --json` for all rigs, diff vs last tick, fixed rules → incidents |
| **Orchestrator** (LLM) | **only when a new incident appears** | ~1–2k tokens in per incident | reads a tiny digest, mails the agents involved (advisory), exits |

Rejected: an LLM orchestrator polling every 5 min (288 wakes/day, growing
context, mostly "nothing changed").

## Watchdog rules (deterministic)
- **Duplicate**: same job key on two rigs (P2 keys); same command + same repo
  running on two rigs; two live agents with the same `**Claimed:**` item.
- **Drift**: a tracked repo at different commits across rigs, or dirty where
  it should be clean; `agent-mail` version or `rigs.json` mismatch.
- **Idle**: a QUIET rig while another rig has queued work it could take
  (resource fit, role; m3 excluded).
- **Stalled agent**: idle > 5 min, no scheduled wake pending (`later/` empty),
  last message is not a question to the owner and not a finished summary.
- **Poisoned / stuck**: P1 poison labels, dead leases, outbox backlog growing,
  rig offline.

Each incident gets a **fingerprint**; only *new* or *changed* fingerprints
wake the orchestrator. The same incident never re-wakes it within a cooldown
(proposed 30 min), and after 3 unresolved rounds it escalates to the owner
instead of looping.

## Digest (the only thing the LLM sees)
Per incident, only the agents and rigs involved:
- rig line: role, QUIET/BUSY, top heavy command (truncated);
- agent line: name, rig, repo, status, idle minutes, **last paragraph of its
  last assistant message, capped ~300 chars** (from the Claude transcript
  `~/.claude/projects/<proj>/<session>.jsonl`, read by the watchdog, no LLM);
- the rule that fired + the watchdog's suggested action.
Target ≤ 1.5k tokens per incident; the whole fleet is never dumped.

## Authority (keeps it safe)
- Advisory only: the orchestrator **mails** agents (they decide, matching the
  047 poison policy); it never kills processes or edits repos.
- Respects the owner: never assigns work to m3; never nudges an agent whose
  last message asks the owner a question; rate limit 1 nudge / agent / 30 min.
- Assigning "other work" needs a backlog: unclaimed `- [ ]` items in
  `.plans`/`.issues` across repos (reuse the claim lease: the orchestrator
  suggests, the agent claims).

## Owner decisions (2026-10-31)
- [x] Runtime **A**: a fresh `claude -p` per incident on amo (needs the Claude CLI + `claude setup-token` on amo once; until then digests land in the `orchestrator@amo` mailbox).
- [x] Nudge scope: **every agent** on every rig.
- [x] Drift repos: **`riir-*`, `seal-*`, `katgpt-rs`** under `~/git` (`E:/git` on the 4090).
- [x] **Event trigger on thread start** (owner idea): the Stop hook saves each session's last paragraph locally (no mail, no tokens); the SessionStart hook sends ONE event to the orchestrator rig carrying the previous thread's last paragraph from the same folder, which is the handoff summary. auto_prompt's end + new thread therefore yields one event, not two.
- [x] **Interval configurable, default 10 min**: launchd fires every 60 s and on new events (`WatchPaths` on the `_watch` inbox, launchd's 10 s throttle debounces bursts); the tick exits in milliseconds unless the interval (read at runtime from `~/.agent-mail/watch.json`) elapsed or an event arrived. A Zed setting writes that file on the orchestrator rig.

## Lessons applied
- launchd runs with a bare environment (the `cargo: command not found` swap
  failure): the plist pins absolute binary paths and `PATH`.
- Watchdog stale → `rig status` on any rig shows "orchestrator stale" (amo
  down = no orchestration, never silent).

## Tasks
- [x] W1 `agent-mail rig watch --once` (`crates/agent_mail/src/watch.rs`): exits in ms unless the interval elapsed or a handoff event arrived; tick lock; rules (offline except no-jobs rigs, blind, no-jobs-rig-busy, outbox backlog, stalled agent = idle ≥ interval with nothing scheduled and not asking the owner, duplicate command across rigs with homes/toolchain normalized, drift = repo+branch HEAD differs persisting 3 ticks, handoff events); fingerprints + 30 min cooldown; 3 rounds → one escalation mail to `owner@m3`, then silent until the incident clears (a stuck problem must not keep waking large-context agents)
- [x] W1 transcript tail: last assistant text from the last 128 KiB of `~/.claude/projects/<cwd>/<session>.jsonl`, last paragraph (extended to ≥ 80 chars), capped 400 chars on a char boundary; per agent in `rig probe`, plus Stop-hook save + SessionStart handoff event (one event per new thread, carrying the previous thread's paragraph from the same folder)
- [x] W1 launchd on amo (`rig watch --install`): StartInterval 60 + WatchPaths on the `_watch` inbox + ThrottleInterval 10 + AbandonProcessGroup (without it launchd killed the spawned orchestrator with the job: found live) + absolute PATH/HOME; `rig status` shows `orchestrator last watchdog tick N min ago` / `STALE`; interval via `agent-mail rig watch --interval N` and Zed setting `agent_mail_watch_interval_minutes` (pushed to the orchestrator rig over SSH on change; null leaves amo's default 10)
- [x] W2 digest + runtime A: `orchestrator_command` in amo's `~/.agent-mail/watch.json` = Zed's bundled Claude Code 2.1.312 (`claude -p --model sonnet --strict-mcp-config --disable-slash-commands --allowedTools 'Bash(agent-mail send:*)'`), digest on stdin, cwd `~/.agent-mail/watch`, `AGENT_MAIL_ORCHESTRATOR=1` makes its own hooks stand down (no self-trigger loop). Dry run first (no tools): good judgement (mailed 3 actionable agents, skipped done/owner-waiting/m3 ones), $0.091, 12.8 s, 1,412 output tokens; ~19k cached input is global CLAUDE.md + tool schemas
- [x] W2 backlog scan: when a stalled-agent incident wakes the orchestrator, the digest gains up to 8 unclaimed items (first `- [ ]` of each `.plans`/`.issues` file in `riir-*`/`seal-*`/`katgpt-rs` whose status is not DONE/CLOSED/… and with no live `**Claimed:**` lease, newest file first, ≤ 160 chars each); the orchestrator suggests one, the agent claims it. Zero cost when nobody is idle. Test: `backlog_skips_done_and_claimed_items`
- [x] bench: quiet tick 0 tokens, exits in ms; full tick 1.9 s (4-rig probe); digest for 7 incidents 2,633 chars (~660 tokens) — under the 1.5k target; LLM run ~$0.09 with sonnet, dominated by fixed context (owner CLAUDE.md), not the digest

## Live-run findings (2026-10-31, fixed in the follow-up commit)
- [x] A changed incident detail bypassed the cooldown; stalled agents' idle minutes change every tick, so 6 incidents escalated within 5 min. Now only a NEW fingerprint wakes immediately; known ones wait their 30 min cooldown.
- [x] Per-repo drift flooded the digest (27 incidents at once). Now ONE aggregate `drift` incident, persisting 6 ticks (~1 h) before it counts.
- [x] `--allowedTools 'Bash(agent-mail send:*)'` was denied in print mode (nothing sent, $0.07). Now `Bash(agent-mail:*)` + the digest demands one plain `agent-mail send` per Bash call (no `&&`, no heredoc). Live re-run: $0.062, 0 permission denials, 2 actionable agents mailed (katgpt-rs-49: Plan 069 P3; katgpt-rs-e3: report after 123 min idle), owner-waiting/m3 sessions skipped.
- [x] Escalations to `owner@m3` were invisible: `rig status` now shows `owner N escalation(s) waiting` (`agent-mail recv --as owner`).
- [x] The orchestrator read "no jobs on m3" as "never nudge m3 agents"; the digest now says: never start jobs on m3, but agents already there may be nudged (owner chose "every agent").
- [x] Handoff trigger verified live: a simulated SessionStart on m3 reached amo's watchdog within seconds (launchd WatchPaths) carrying the previous thread's last paragraph.
- [x] Lean orchestrator context (measured on amo, one-word prompt): default flags 29.9k input tokens / $0.053; `--tools Bash` 13.3k / $0.045; plus `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1` + `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` (new `orchestrator_env` in `watch.json`) **3.8k / $0.0062, 8.5× cheaper**, and no reflexive `## Summary` (4 output tokens). Safe because the digest carries every instruction the orchestrator needs

