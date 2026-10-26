# zed — boundary contract

> The single source of truth for what may live in and depend on this repo.
> Audited by the `boundary-guard` skill + `../riir-ai/scripts/ci_boundary_contract.sh`
> (workspace gate — is the dep graph what this file says, and is this file still
> honest?). Cross-repo rules LINK to their one canonical home — never copied.

**Visibility: public.** Upstream is `zed-industries/zed`; this fork ships public
source at `katopz/zed`. Public-leaf law applies: path-deps into the private
family (`../riir-*`, `../katgpt-*`) are FORBIDDEN — this fork must build and
publish without leaking private-local paths into its manifests.

## Owns

The **Zed editor dev fork** — the daily-driver build (`Zed Dev.app`) for the
rig's agent sessions. Upstream base plus a local feature lane maintained on
`develop`:

- `auto_prompt` — context-overflow detection, pending-question detection, the
  summary-handoff state machine, async decide windows (plans 001–010, 019–026,
  026 async, 028)
- `open_ai_compatible` multi-key lane — per-key 429 backoff, `reset_key_session`
  on new-thread creation, key-health UI, priority rotation, next-limit
  prediction ring (plans 001–008, 012, 027, 030)
- `agent_board` + web UI, war-room panel, activity bars (plans 013–015, 024–025)
- verdict-reviewer lane — `claude_code` offscreen backend + GLM native-subagent
  secondary reviewer, owner-gate decisions (plans 029, 014, 036)
- thread/session ergonomics — continuation indicator, pin sidebar, resume toast,
  stream-cutoff summary-first, subagent retry (plans 007, 009, 011, 021, 032, 035)
- agent-infrastructure `.agents/skills/` (gpui-test, lint-creator,
  zed-cherry-pick) and the root `.rules`

Local plans live in `.plans/` (numbered `NNN_*`, the rig convention).

## Does not own

| Concern | Correct home |
|---|---|
| GPU training / distillation / self-evolve | `../riir-train` |
| Inference substrates / game runtime / riir-gpu kernels | `../riir-ai` |
| Mining/corpus lanes (riir-refine batches, boundary-guard cadence) | `../riir-refine` |
| Chain / storage / SDK / dApps | the respective `../riir-*` repos |
| Upstream Zed features and platform crates | upstream `zed-industries/zed` — carried via the fork, never re-owned here |

## May depend on

| Crate | Location | Condition |
|---|---|---|
| (none cross-repo) | — | All path deps are intra-repo (`crates/*`, 245 crates). Measured 2026-10-26: zero `../riir-*` / `../katgpt-*` path deps in any manifest. Keep it that way — the public-leaf law forbids family path-deps from this repo |

## Inherited boundaries (links)

- Workspace contract: `../riir-ai/BOUNDARY.md`
- Boundary-guard skill + gate: `../riir-ai/scripts/ci_boundary_contract.sh`
  (auto-enumerates every `*/` repo with a root BOUNDARY.md)

## Drift ledger (target vs actual)

None open.
