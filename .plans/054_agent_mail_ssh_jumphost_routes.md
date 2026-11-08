# 054 — Agent-mail SSH jump-host routes: bmo via amo when Tailscale is down
Status: DEPLOYED + e2e verified (bmo online via jump on the panel API; mail rides the jump) — owner-gated: Tailscale Funnel on amo is OFF so the public panel can't render (GUI toggle), and amo→m3 TCP is blackholed at the Tailscale relay (both pre-existing); `54c1ecf` — wait, commit e2c4d01028

## Owner ask (2026-11-08)
bmo is ON/OFF time to time; its Tailscale link drops while the box itself is up
(thunderbolt bridge amo `bridge0` 192.168.10.1/24 ↔ bmo 192.168.10.2, ~0.4 ms).
Today every agent-mail path (probe, mail send, jobs, thread publish) tries
bmo's single direct route `katopz@100.100.117.44`, times out, and the Rigs
panel shows OFFLINE. The Rigs panel should mention the jump route, and mail
should ride it too: `ssh -J katopz@100.111.186.76 katopz@192.168.10.2`.

## Design
- `SshRoute` (agent_mail): untagged serde enum — a plain string stays a direct
  destination; `{"target": ..., "jump": ...}` adds `-o ProxyJump=<jump>`.
  Old string-only rigs.json keeps parsing; `Rig.ssh: Vec<SshRoute>` tries
  routes in order with per-route failure labels (`target via jump: err`).
- All dial sites iterate routes: `send_remote`/`flush_outbox` (mail),
  `probe_remote` (panel + `rig status`), `ping_rigs`, `job_on_rig`,
  `on_orchestrator`, `rig_run`, `set_orchestrator_interval`,
  `publish::send_tar` (thread mirror). ProxyJump lives in `Mailroom::ssh` and
  `send_tar` so no site hand-rolls ssh args.
- Provenance: `RigProbe.via` (optional, injected by the prober) names the
  winning route when it was a jump; `FleetRig.routes` (short labels, jump
  resolved to the owning rig's name, user@ stripped) carries the configured
  routes to the edge so the panel card can mention them.
- One-time ops fix discovered by the e2e: amo's `known_hosts` had no entry for
  its own tailscale IP, and ProxyJump's inner hop does not inherit
  `accept-new` — `ssh-keyscan 100.111.186.76 >> known_hosts` on amo fixed the
  amo→(jump)→bmo path.

## Tasks
- [x] `SshRoute` type + serde compat + `Mailroom::ssh` ProxyJump; call sites iterate routes
- [x] fleet-web `send_tar` rides routes (thread mirror over jump)
- [x] Panel: `RigProbe.via`, `FleetRig.routes` (origin.rs), edge `types.ts` + `rigs.tsx` card line + `via` tag
- [x] Tests: serde back-compat, label resolution, fleet_summary wording (agent_mail 54 + fleet_web 12 green, clippy clean; `cargo shear` failures pre-exist on develop)
- [x] Edge: `npm run check` + `fastly compute build` + deploy → service v4 (FASTLY_API_SECRET in zed/.env; note `npm run build` alone does NOT repackage — `fastly compute build` does)
- [x] Deploy binaries: agent-mail (+fleet-web on amo) to m3, amo, bmo (bmo via jump scp); kickstarted `dev.agent-mail.fleet-web` on amo
- [x] Config: bmo jump route in rigs.json on m3, amo, bmo; bmo also gained m3/4090 jump-via-amo fallbacks for its reverse mail path (binary first, then config — old binaries can't parse object routes; 4090 stays direct-only, Windows OpenSSH lacks ProxyJump)
- [x] E2E: probe bmo ok via jump from m3 (1566 ms) and amo (QUIET); signed `/api/fleet` shows `bmo ok=True routes=['100.100.117.44','192.168.10.2 via amo'] via=amo`; bmo's stranded Nov-6 mail to m3 rescued + delivered locally, bmo outbox clear
- [ ] Commit+push develop (this commit)

## Owner-gated / deferred
- [x] **Tailscale Funnel on amo is OFF** — FIXED by owner 2026-11-08 (funnel re-enabled; the public panel loads again). Edge v4 renders: rig cards show the `routes:` line, a `via amo` tag when a probe rode the jump, and the offline banner names both routes.
- [x] **amo→m3 TCP blackholed** — ROOT CAUSE FOUND 2026-11-08: m3 runs Tailscale with **ShieldsUp enabled** (`"ShieldsUp": true` in debug prefs), which drops unsolicited inbound TCP from tailnet peers while ICMP (ping) still passes — exactly the observed asymmetry. Owner turned shields off in the Tailscale app; amo→m3 TCP verified open; panel probes of m3 succeed again.
- [x] Follow-up (same day, f2f6143db3): flag-shaped recipients can no longer become inbox folders (validate_name rejects leading `-`; `send` is getopt-style with `--to`, unknown-flag errors and a `--` separator), and rig-panel inbox folders are clickable and list their mail.
- [-] Probe latency for bmo: the dead direct route eats its 10 s `ConnectTimeout` before the jump route wins (~10.2 s per probe round; panel cache 15 s absorbs it). Defer: parallel-race routes or auto-demote a route after N consecutive failures.
- [-] 4090 jump support: Windows OpenSSH ProxyJump support is shaky; its rigs.json stays direct-only and it never dials bmo. Revisit only if the 4090 must reach bmo.
- [-] Gatekeeper note: cargo-built agent-mail binaries can get SIGKILLed on first exec on a Mac (stale provenance verdict; happened to the 055 binary on m3). Fix: `codesign -f -s - ~/.cargo/bin/agent-mail`. Applied on m3; amo/bmo re-signed preemptively.

## Notes
- ControlMaster `%C` hashes host/port/user only; bmo's two routes target
  different addresses so their mux sockets stay distinct.
- ProxyJump's inner hop runs with default options (no `accept-new`, no custom
  ControlPath) — every rig that serves as a jump source needs the jump host in
  its `known_hosts` once. amo is fixed; m3 already had the entry.
