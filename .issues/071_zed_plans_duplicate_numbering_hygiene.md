# 071 — zed `.plans/` duplicate-number hygiene: 3 true collisions renumbered, era-cross recorded, counter seeded

Status: CLOSED (2026-11-18, zed-eb3b7571@amo) — executed in the same commit; the .plans/ counter (.highwater) now exists and the era-cross question is decided.

## Census (tracked files, measured 2026-11-18)

Two naming eras share `.plans/`: era-1 two-digit (`01_auto_prompt` … `06_subagent_retry_after_completion`) and era-2 zero-padded (`001_openai_compatible_dual_api_key` … `057_tiered_job_queue`). Findings:

1. **011 collision** — `011_pin_thread_sidebar` vs `011_watchdog_e2e_test`. Winner by first allocation (0a102e7a11 09:46:15 vs 39fffc0bbe 10:12:04, same day) + BOUNDARY.md cites it ("pin sidebar … plans 007, 009, 011, 021, 032, 035"). Loser had 0 inbound refs → renumbered **058**.
2. **025 collision** — `025_permission_prompt_auto_allow_countdown` vs `025_war_room_fixes`. Winner by first allocation (3ffef69ab4 01:30:45 vs 3316fabc86 21:51:23) + a live code cite (`agent_ui/src/conversation_view/fork.rs:89` "after the countdown (plan 025…)"). Loser had 0 full-stem refs; BOUNDARY.md's war-room range "plans 013–015, 024–025" updated in the same commit → renumbered **059**.
3. **026 collision** — `026_auto_prompt_async_decide_windows_warning` vs `026_web_threads_tab`. Winner by first allocation (73753f1f9b 08-22 vs ca4b3991e0 08-24; citation weight not consulted — history does not tie). Loser had 9 inbound bare-number refs (agent_panel/fork.rs ×3, auto_prompt/peer_states.rs ×2, agent_board/types.rs ×3, agent_board/client.rs ×1, .docs/012) — all rewritten to **060** in the same commit. BOUNDARY.md's "026 async" names the winner and stays.
4. **NOT a collision: the 024 family** — `024_war_room_panel.md` + five `024_war_room_activity_bar_*.png` screenshots are one plan's sidecars (family-explicit), left untouched.
5. **Era-cross (01–06 vs 001–006): DECIDED, no rename.** The numeric values coincide across the two-era boundary but the string tokens are unique (`01_auto_prompt` vs `001_openai_compatible_dual_api_key`) — no citation ambiguity exists at the string level, six historical files plus every "plan 01" prose ref would churn for zero gain, and no gate enforces this repo (zed is outside the katgpt-rs workspace population). Any future tooling that strips leading zeros is the thing to fix, not the files. Recorded here as the decision of record.
6. **Missing counter fixed** — `.plans/` had no `.highwater` at all; seeded at **060** (the highest allocated number after the moves).

## Rule stack applied

First allocation (`git log --diff-filter=A --format=%ai`, earliest wins); citation weight only when history ties (it tied nowhere); the 024 group adjudicated family by content (one plan, sidecar extensions).

## Execution (this commit)

- `git mv`: 011_watchdog → 058, 025_war_room_fixes → 059, 026_web_threads_tab → 060 (allocation order preserved: 058 < 059 < 060 matches the losers' original dates 07-16 < 08-21 < 08-24).
- Ref rewrites: the 9 bare-026 sites above + BOUNDARY.md war-room range.
- `.plans/.highwater` seeded 060; `.issues/.highwater` bumped to 071 with this allocation.
