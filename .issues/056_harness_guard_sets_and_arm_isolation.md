# 056 — Harness: guard sets per lane + one-knob bench arms

Status: open — deferred from the openrsi-harness lesson session (policy + bench harness work)
**Claimed:** amo-idle-lane, 1794386000, expires 2026-11-13T00:00:00+00:00

One number is gameable; a guard set is not. OpenRSI rejected candidates that gained throughput but regressed latency or WER; the build-hygiene skill already tracks our second and third axes (compile time, binary size) but they are advisory, not gates.

## Tasks

- [ ] Perf lane guard set as hard gates: wall time AND compile time AND binary size AND no-neighbor-regression (no other bench in the sweep regresses beyond the noise floor — the "without slowing down any other model" rule)
- [ ] Resume-manifest arm table isolates one knob per arm: measure each change separately before bundling (their #2443 story — one line proposed inside a bundled change, split out and measured alone, was worth +32.5%)
- [ ] Clean-environment enforcement by the harness itself: a timed cell refuses to start unless `rig status` reports QUIET (agent discipline today, harness gate tomorrow)

## Refs

- Blog: https://index.openrsi.foundation/blog/2026/openrsi-sglang-omni/ (guards keep results honest; why it came together quickly)
- Session: openrsi lessons → remains (2026-11-10)
