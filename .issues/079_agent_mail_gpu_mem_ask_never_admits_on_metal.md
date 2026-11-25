# Issue 079 — `rig run --gpu-mem N` never admits on Metal rigs (amo/bmo/m3)

**Status:** FIXED 2026-11-25 (zed-504dde51@amo, this landing) — all four fix-shape rows landed + tests; amo's installed binary carries the fix and the live admission proof ran. Remaining: the 4090 only (ride issue 081's rebuild — m3 rebuilt from source 2026-11-25 37.5 s; bmo got amo's binary over the bridge per the no-source-on-bmo rule; both probes answer).
**Lease closed:** zed-504dde51@amo, claimed 1795575237 — folded at the same-session landing (all four fix-shape rows + tests + amo install + the live admission proof).

## Defect

`jobs.rs::fits` requires `capacity.gpu_free_gb.is_some_and(...)` whenever `needs.gpu_mem_gb > 0`.
`capacity.gpu_free_gb` comes from `rig::gpus()` (`rig.rs`), which only reads `nvidia-smi` and
returns an empty list on macOS. So on every Apple-silicon rig, a job that declares any `--gpu-mem`
sits in the queue at #1 forever: no error, no reason shown in `rig status`.

Measured on 2026-11-23: job `d475342c` (`--shared --gpu-mem 12`) sat queued on a QUIET amo
with 77 GB free for about 5 h. The same command resubmitted without `--gpu-mem` (`34478b09`) started
within seconds.

## Fix shape

- [x] Metal rigs have unified memory — LANDED: `rig::gpu_free_gb()` (rig.rs) returns the
      discrete nvidia-smi max when GPUs exist, else the macOS unified-memory free-RAM number
      (`memory_free_gb()`), else None; pure core `gpu_free_from(&[Gpu], macos, mem_free)` is
      unit-tested both directions. `Capacity` (scheduler + the new queue-head render) reads it.
- [x] A job that can NEVER fit on its rig is refused at submit — LANDED:
      `jobs::ensure_gpu_ask_servable(gpu_mem_gb, gpu_capable)` called at `rig enqueue`
      (main.rs — the rig-side path, so remote submissions refuse with the rig's OWN view).
      `rig::gpu_capable()` = macOS or nvidia-smi on PATH (presence, not health — a probe
      hiccup never rejects a real submission). Refusal names the reason and never queues.
- [x] `rig status` shows the admit-blocking reason for the queue head — LANDED: the first
      Shared job's line gains ` · BLOCKED: <axis>` (`jobs::blocking_axis` — cores / memory /
      gpu-memory, first failing axis, same construction as `fits`; running reservations
      folded in). Verified live: amo's head renders `BLOCKED: memory` under the running job.
- [x] Tests — LANDED: `gpu_free_falls_back_to_unified_memory_on_macos` (discrete wins,
      macOS fallback, non-macOS None), `gpu_ask_against_an_incapable_rig_refuses_at_enqueue`
      (measured-incident shape refuses, capable accepts, no-ask never refused),
      `blocking_axis_names_cores_memory_then_gpu` (axis order + reservations). Lib 112/112,
      clippy -D warnings clean.

## Landing record (2026-11-25, zed-504dde51@amo)

- Diff: `rig.rs` (+43: gpu_free_gb / gpu_free_from / gpu_capable / command_on_path),
  `jobs.rs` (+196 net incl. tests: Capacity wiring, blocking_axis, ensure_gpu_ask_servable,
  render_capacity, the queue-head BLOCKED tag, 3 tests), `main.rs` (+7: the enqueue-side
  refusal). rustfmt 2024 clean; release build 27.5 s (isolated /tmp target).
- **Install lesson (recorded for every future agent-mail install):** `cp` over the live
  `~/.cargo/bin/agent-mail` path left the new binary SIGKILLed at exec (rc 137, "Killed: 9")
  while the IDENTICAL bytes ran from /tmp and under a fresh name in the same dir — macOS
  caches signature validation per inode+path. The correct install is cp-to-temp + `mv -f`
  (rename) onto the live path; verified 3× green after the mv. The running old-inode
  processes (claude-hook, rig exec, rig kick) were never disturbed.
- **Live admission proof:** `rig run amo --shared --gpu-mem 1 -- true` → job `01a210a5`
  admitted immediately (the measured incident's exact shape queued 5 h on 2026-11-23),
  exit 0, ended in 2 s. Queue-head render: `#1 cebad5d8 … · BLOCKED: memory` names the
  axis while amo's 22 GB free is under the head's ask.
