# 039 — agent-mail poison flags a job's own spawned infrastructure (docker VM)

Status: OPEN — filed 2026-11-12 (amo idle lane), evidence from a live exclusive job.
**Claimed:** zed-11bec7de@amo, 1794532000, expires 2026-11-14T18:00:00+00:00

## Evidence

Rig job `01a1cb9c` on amo (Exclusive, `~/seal-i200/proof.sh` for katgpt-rs-fb@m3, 2026-11-11 17:26Z start) shows **POISONED ×36** while running normally. Every poison entry names `com.apple.Virtualization.VirtualMachine` — the docker VM that **the job's own script spawns** (`guest_browser_e2e.sh` boots docker + wrangler dev). The poison detector (`rig::contention`) sees the VM as a foreign heavy contender because it is spawned via launchd, NOT a descendant of the job process, so tree exclusion cannot tell it apart from an intruder. Each watchdog tick re-poisons while docker runs. Result: every docker-using exclusive bench on this fleet self-poisons for its whole run (this job's lane must now adjudicate 36 false-positive poison records; earlier attempts r1/r2 of the same `i200-v2-r3` key likely died the same way).

## Fix sketch (needs a verdict on poison semantics before implementing)

- **Job-declared capability (preferred):** a `uses_vm: bool` (or the existing `--needs` vocabulary gaining `vm`/`docker`) on `JobSpec`; the poison probe exempts VM-infrastructure process classes (`com.apple.Virtualization.VirtualMachine`, `docker`, `com.docker.*`) when the job declared it. Mirrors `--needs cuda`: the job declares the environment it will itself occupy.
- **NOT tree exclusion** — the VM is never a descendant (launchd owns it).
- **NOT a blanket name exemption** — a foreign session's docker build during an exclusive bench is exactly the contention poison exists to catch; only the declaration may open the exemption.
- Lap-drop note: the declaration should also gate the quiet-gate preflight (a VM-capable job waits for a box with no FOREIGN VM, not a box with no VM at all) — otherwise a docker box is never "quiet" for anyone.

## Related finding (same evidence sweep, 2026-11-12)

- **bmo's system clock ran ~3.5 days behind** (real 2026-11-12 ~01:10 UTC, bmo 2026-11-08 ~16:49 UTC; now corrected by this session's operator check — verify `ntpdate`/`sntp` state on the box). Every bmo-born mail/lease/backoff timestamp was skewed; fleet claim-expiry arbitration keys on push, so no verdict was corrupted, but any bmo-local `--at` scheduling and lease arithmetic was wrong. Worth a fleet-preflight check (`rig status` could print clock delta vs the caller).
