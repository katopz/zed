# 003 — agent-mail cross-rig latency (plan 043)
Status: MEASURED 2026-10-30 — release `agent-mail` 0.1.0 on m3, amo, bmo (aarch64-apple-darwin) and 4090 (x86_64-pc-windows-msvc)

End-to-end `agent-mail send` wall time (process spawn → remote `deliver` → `delivered <id>` seen).

| Path | p50 | Notes |
|---|---|---|
| m3 local (same rig) | 17 ms | file write + rename; no SSH |
| Claude hook, empty inbox (`PostToolUse`) | 20 ms | runs on every tool call; scans `~/.claude/sessions` once |
| m3 → amo, cold | 909 ms | first SSH handshake opens the ControlMaster |
| m3 → amo, warm | 190 ms | Tailscale reports **no direct path** from m3 (DERP relay) — network-bound |
| m3 → bmo, warm | 198 ms | same relay |
| m3 → 4090, warm | 225 ms | same relay |
| amo → bmo, warm | 37–41 ms | direct path |
| 4090 → amo | 890–975 ms | Windows OpenSSH has no ControlMaster: full handshake per send |
| idle Claude wake (waiter poll 500 ms) → reply | ~12 s round trip | dominated by the woken model's turn |

Verdict: SSH-over-Tailscale is GOAT for agent messaging (zero $, zero new
ports, existing key auth); every path is ≥ 10× faster than the agent turns it
connects. Possible wins, none needed now: a direct Tailscale path for m3
(−150 ms), a persistent `ssh -M` on the 4090 under WSL (−800 ms).
