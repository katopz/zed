# 030 — OpenAI-compatible K1–K4 next-limit prediction ring

Status: done — all crates tested (14 + 180 + 487 pass), clippy clean

## Problem

The Zed agent footer K1–K4 chips only render a ring **while a slot is in
backoff** (drain ring). Once the backoff lifts, the ring disappears entirely —
there is no indication of where the key sits in its quota cycle, unlike the
Claude agent which always shows usage rings (`render_claude_usage`, plan 017).

The user wants a Claude-style always-visible estimate ring derived from the
**last backoff record we got from the API** (the last limit hit) to **predict
the next hit**.

## API token-usage check (asked: "not sure api return token used too?")

Checked. The upstream OpenAI-compatible endpoint (Z.AI coding plan) returns on
a limit hit:

- HTTP 429 with a `retry-after` header (sometimes), or
- a body timestamp: `{"error":{"code":"1308","message":"Usage limit reached
  for 5 hour. Your limit will reset at 2026-09-19 23:42:39"}}` (1310 =
  weekly/monthly), parsed by `language_model::parse_body_retry_hint`.

It does **not** return quota token-usage/remaining data: no `x-ratelimit-*`
headers are read anywhere in `crates/`, and there is no usage endpoint for
this provider (unlike Claude's OAuth `/usage` backing `claude_usage.rs`). The
per-response `usage` object is per-thread context-window accounting (already
shown by `render_token_usage`), **not** per-key quota consumption.

**Verdict: token-used display is deferred** — the API gives us no quota
utilization to show. What it does give us (429 + reset hint) is what this plan
uses.

## Design

One observed limit hit gives us: hit at `H`, upstream-attested reset at
`R = H + W` (`W` = the hint duration). For an always-hammering agent (the
auto-prompt loop), the observed drain time ≈ the quota period, so the
predicted next hit is `R + W` (= `H + 2W`). Conservative for light usage
(warns early), exact-ish for heavy usage.

Ring lifecycle per slot:

1. **In backoff** — existing warning drain ring (`remaining / total`).
2. **After reset, before predicted next hit** — new prediction ring fills
   `0 → 1` as `(now - R) / W` progresses toward `R + W`; accent color, flips
   to warning at ≥ 85%. Tooltip shows reset + predicted times.
3. **No record / past predicted hit** — no ring (nothing honest to show).

- Only upstream-attested hits (hint `Some`) record; hintless 429s have no
  window to predict from.
- `record_success` **keeps** the record (a success proves quota is open now,
  the historical cycle stays the best estimator).
- `clear_slot_backoff` (user clear / key reset) wipes it (new key = new quota).

## Tasks

- [x] health.rs: `LimitHitRecord { hit_at, window }` + `KeyHealth.last_limit_hit`; record on hinted `record_rate_limit`; survive `record_success`
- [x] health.rs: persist `last_limit_hit_at_unix_secs` + `last_limit_window_secs`; schema v4→v5; v4 files derive the record from an in-flight upstream-attested window; v3 unattributable clearing also clears the record
- [x] SlotHealthStatus / ModelKeySlotStatus: expose `last_limit_hit_at` + `last_limit_window`; `limit_prediction()` / `prediction_fraction(now)` helpers on `ModelKeySlotStatus`
- [x] open_ai_compatible.rs: `clear_slot_backoff` wipes the record; map fields through `slot_status` / `key_slot_status`
- [x] thread_view.rs: prediction fill ring beside healthy enabled chips; prediction info in backoff + healthy tooltips; tick task extended (1s backoff / 15s prediction); fixed pre-existing stuck-task respawn guard
- [x] Tests: record/preserve semantics, persistence round-trip, v4 derivation migration, v3 clearing, shape snapshot bump, `prediction_fraction` boundaries
- [x] `./script/clippy` clean on touched crates; `cargo heal` for mechanical findings (none reported)
- [-] Token-used (quota utilization) display — deferred: the API returns no quota token data
- [x] Commit + push

## Notes

- Parsing the quota period from the 429 message ("for 5 hour" / "Weekly") would
  sharpen the estimate but threads a new field through the public
  `LanguageModelCompletionError` enum — deferred as a refinement.
- ConfigurationView (settings page) badges are left unchanged this round.
