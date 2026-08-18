# 014 — language_models: 3 pre-existing lib-test failures (upstream-sync fallout)

Proven pre-existing on clean `develop` @ `285c0b588d` (before any plan-022
changes): the same 3 tests fail with identical panics with pristine
`Cargo.toml`/`Cargo.lock` checked out. Surfaced only now because
`language_models --all-targets` had not been compiled since upstream sync
`cc07356651` — `-p agent_ui` gates build deps' libs, not their tests.

Repro:

    cargo test -p language_models --lib

Result: `144 passed; 3 failed` (0.3s — fast assertion/setup failures).

## 1. `tests::test_compatible_provider_changes_kind_and_unregisters`

Panic: `no state of type fs::GlobalFs exists` at `gpui/src/app.rs:1916`
(caller: `crates/language_models/src/language_models.rs:497`,
`fn ...(cx: &mut App)` → `init_test(cx)`).

Hypothesis: upstream moved `fs` behind a gpui global (`GlobalFs`); the fork's
`init_test` never registers it. Likely fix: register a `FakeFs` global in
`init_test` (or port however upstream's equivalents now bootstrap fs).

## 2. `tests::test_compatible_provider_id_collision_resolves_when_one_entry_is_removed`

Same panic/site/hypothesis as #1.

## 3. `provider::open_ai_compatible::health::tests::reload_persisted_health_v1_schema_migrates_with_healthy_quaternary`

Panic at `health.rs:1767`: `primary backoff must be preserved (was 14454.5s in
v1 file)` — asserts `loaded.primary.backoff_until.is_some()` after migrating a
v1 fixture with `saved_at_unix_secs: 1700000000` (Nov 2023) and
`backoff_remaining_secs: 14454.5`.

Hypothesis: time-bomb test. The migration (or load-time expiry) computes the
backoff window relative to `saved_at`; by 2026 the window has long elapsed, so
`backoff_until` is cleared → `None`. Likely fix: make the fixture timestamp
relative to now, or make load-time expiry preserve `backoff_until` for
already-expired windows (decide intent — this is fork Issue-007 regression
coverage, commit `9b063ddf` context).

## Fix-when-done note

Ref the fixing commit hash here, then remove this file.
