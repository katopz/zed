# 054 — rig-job runner SSH auth on amo fails with "Host key verification failed"

Status: OPEN — repro 2026-11-05, job 01a1ac98 (bundle-mac on amo) died exit 255 in ~1s
**Impact:** `agent-mail rig run amo ...` unusable until fixed; direct ssh works fine.

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

## Suspected root cause

The runner's executor invokes `ssh <ip>` with `StrictHostKeyChecking` on and a
`known_hosts` that only has the host under another name (`amo`), so the new
IP looks like a MITM. Batch-mode (no tty) makes it fail instead of prompt.

## Fix sketch

1. On amo: `ssh-keygen -R 100.111.186.76` then re-add the current key
   (`ssh-keyscan -H 100.111.186.76 >> ~/.ssh/known_hosts`), and/or add a
   `Host amo-rig` block in `~/.ssh/config` with `HostKeyAlias amo`.
2. Better: make the runner pin `HostKeyAlias` (name-based, like m3's config)
   or pass `-o StrictHostKeyChecking=accept-new` for fleet-internal legs.
3. Regression test: a rig job whose command sshes back to a fleet rig by IP.

## Verification

- `agent-mail rig run amo --shared -- ssh katopz@100.100.117.44 'echo ok'`
  returns `ok` and the job log has no `Host key verification failed`.
