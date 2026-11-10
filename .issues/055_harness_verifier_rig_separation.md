# 055 — Harness: verifier rig separation + frozen baseline manifest

Status: open — deferred from the openrsi-harness lesson session (m3/amo/bmo policy; not zed code)

OpenRSI's single strongest control: "every step is verified... an independent verifier, which the agent never sees". Today the agent that produced an artifact also runs the benchmark and reads the result. `--worktree repo@sha` already ships artifacts by hash — the verifier lane should check out the hash itself, rebuild clean, re-run, and only then let a gain count.

## Tasks

- [ ] `agent-mail rig run` gains a verify lane: re-run a job's measured cell from a resume manifest on a rig **different** from the producing rig (BLAKE3 of the artifact + corpus, step cursor, arm table — the compute-law resume manifest is the carrier)
- [ ] Per-plan baseline block (commit hash, baseline numbers, noise floor) the harness pins and re-measures against; every claim is expressed as ratio-to-baseline, never absolute
- [ ] Result rows must carry `{rig, cores, os, toolchain, baseline_sha}` — a gain whose cell lacks these is not a result (their gains were deployment-dependent: +82.9% @ 8 cores vs +33.6% @ 16)
- [ ] amo referee records produced-rig vs verifier-rig in the job ledger

## Refs

- Blog: https://index.openrsi.foundation/blog/2026/openrsi-sglang-omni/ (Harnessed RSI, guards section)
- Session: openrsi lessons → remains (2026-11-10)
