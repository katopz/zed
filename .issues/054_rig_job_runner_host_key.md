# 054 — rig-job runner SSH auth on amo fails with "Host key verification failed"

Status: FIXED 2026-11-06 (host-key half healed + mesh verified; LAN-leg finding
filed as 007 follow-up note below). Repro 2026-11-05, job 01a1ac98 (bundle-mac
on amo) died exit 255 in ~1s.
**Impact was:** `agent-mail rig run amo ...` unusable; direct ssh works fine.

## Symptoms

- `agent-mail rig run amo --shared -- ssh ... 'cmd'` queues, the executor on
  amo starts the job, but the job's inner `ssh` leg dies instantly and the
  log contains exactly one line: `Host key verification failed`.
- Retrieving the log from m3 (`agent-mail rig log amo <id>`, or scp of the
  path it prints) fails the same way, while plain `ssh katopz@100.111.186.76`
  from m3 works (key auth, no prompt).
- amo's `~/.ssh/known_hosts` keys the host by the name `amo`; the bare IP
  `100.111.186.76` entry predates a Tailscale IP change (m3's known_hosts had
  a stale IP entry too — `ssh-keygen -R 100.111.186.76` + reconnect re-prompted
  and only an interactive `yes` fixed it).

## Adjudicated root cause (2026-11-06 sweep)

Two independent halves, both healed:

1. **Host-key half (the reported symptom):** stale `known_hosts` entries on
   BOTH ends from Tailscale/LAN IP churn. Amo's known_hosts now carries the
   current keys for bmo (`100.100.117.44`) and m3's `100.111.186.76` → entry;
   m3's known_hosts carries `100.111.186.76` + `100.85.179.44`, its stale LAN
   entry is gone, and the missing `192.168.1.36` (4090 LAN) entry was accepted
   with one `accept-new` connect (now persisted). Verified by direct batch ssh
   per leg — no `Host key verification failed` anywhere in the mesh.
2. **Auth-user half (found by the sweep, silently co-blocking):** amo→m3 legs
   must use `kat@` (m3's local user; `katopz` on m3 is a second local account
   whose home carries NO authorized_keys → `Permission denied (publickey)`).
   amo's key (`F9cz+evY…`) was already in kat's authorized_keys; m3's OWN key
   (`Rbg+bRY7…`) was missing from its own `authorized_keys` (legs sshing back
   to m3 by IP from m3 itself failed) — appended 2026-11-06, backup at
   `/tmp/authorized_keys.bak`.

## Verification (2026-11-06, all via `agent-mail rig run amo --shared`)

- `rig run amo → ssh katopz@100.100.117.44` → `ok-from-bmo` (the issue's own
  recipe) — PASS
- amo→m3 `kat@100.125.16.103` → PASS · amo→bmo `katopz@100.100.117.44` →
  PASS · amo→4090 `katop@100.85.179.44` → PASS
- m3→amo / m3→bmo (katopz@) / m3→4090 → PASS
- Full amo job running all three fleet legs in one command → PASS

## Carried-forward finding (LAN legs, separate from 054)

amo cannot reach m3's LAN IPs (`192.168.1.34/.36`) at all — ping AND tcp/22
closed — while m3↔4090 LAN works and amo's own LAN stack is healthy (gateway
ok, its own IP `192.168.1.136` pings itself). m3's macOS firewall is in
stealth mode and drops the amo-originated legs; Tailscale legs are unaffected
so the fleet runner (`agent-mail rigs` = all Tailscale endpoints) keeps
working. LAN acceleration for amo→m3 needs a firewall allow-rule for amo's
LAN IP — deferred: the fleet routes over Tailscale by design.

## Durable hardening (runner-side, NOT landed here)

The fix-sketch option 2 (`HostKeyAlias` pinning or
`StrictHostKeyChecking=accept-new` for fleet-internal legs) lives in the
agent-mail runner source, outside this repo — re-file there if a fresh IP
churn re-exposes the batch-mode prompt-death class. The regression recipe
above IS the test: a rig job whose command sshes back to a fleet rig by IP.
