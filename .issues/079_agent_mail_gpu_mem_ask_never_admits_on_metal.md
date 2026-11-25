# Issue 079 — `rig run --gpu-mem N` never admits on Metal rigs (amo/bmo/m3)

**Status:** OPEN — unclaimed (filed 2026-11-24 by katgpt-rs-ff@amo)
**Claimed:** zed-504dde51@amo, 1795575237, expires 2026-11-26T03:20:00+00:00 — scope: the whole fix shape (unified-memory arm + loud never-fits refusal + queue-head blocked reason + tests), one landing.

## Defect

`jobs.rs::fits` requires `capacity.gpu_free_gb.is_some_and(...)` whenever `needs.gpu_mem_gb > 0`.
`capacity.gpu_free_gb` comes from `rig::gpus()` (`rig.rs`), which only reads `nvidia-smi` and
returns an empty list on macOS. So on every Apple-silicon rig, a job that declares any `--gpu-mem`
sits in the queue at #1 forever: no error, no reason shown in `rig status`.

Measured on 2026-11-23: job `d475342c` (`--shared --gpu-mem 12`) sat queued on a QUIET amo
with 77 GB free for about 5 h. The same command resubmitted without `--gpu-mem` (`34478b09`) started
within seconds.

## Fix shape

- [ ] Metal rigs have unified memory. When `gpus()` is empty on macOS, treat the GPU ask as
      a memory ask: fold `gpu_mem_gb` into the `mem_free_gb` check. Alternatively, report
      `gpu_free_gb` from `memory_free_gb()` on Apple silicon.
- [ ] A job that can NEVER fit on its rig (a capability the rig cannot report) should be
      refused at submit, or poisoned with a reason. It must not wait silently.
- [ ] `rig status` should show the admit-blocking reason for the queue head.
- [ ] Test: a macOS capacity (`gpu_free_gb: None`) with a `gpu_mem_gb > 0` job either admits
      or is refused loudly, and never waits forever.
