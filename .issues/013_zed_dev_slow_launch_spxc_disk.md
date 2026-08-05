# Issue 013: `Zed Dev.app` (dev build) takes minutes to launch — Gatekeeper re-validation over slow SDXC1TB disk

> **Status: DEFERRED.** Real issue, not fixing now. Recorded for when it
> becomes a frequent annoyance. Confirmed it is **not a crash and not a
> hang** — the app does open and run correctly once the long first-launch
> delay passes. Originally filed as "commit `9665d7d0ae` not responding /
> crashes when open"; investigation cleared the commit and pointed at
> macOS launch-time checks over a slow external disk.

## Symptom

When opening the dev build (`Zed Dev.app`, bundle id
`dev.zed.Zed-Dev`, built from this repo) the app appears unresponsive
for a couple of minutes — Dock icon bounces / window doesn't appear /
Activity Monitor may briefly show "Not Responding". After waiting, the
app opens and works normally.

## What it is NOT

- **Not a crash.** No `.ips` crash report is written to
  `~/Library/Logs/DiagnosticReports/`, and `Zed.log` contains no panic /
  `thread '...' panicked`. The modified build at commit
  `9665d7d0ae67d78a65f39c17beeac95649f0231e` was launched 5+ times
  (bundle path, `/Applications`, via `open`, with the real GLM data dir)
  and reached "Rendered first frame" every time.
- **Not a hang/deadlock in the new code.** The commit's new render path
  (`render_key_status_buttons` → `LanguageModel::key_slot_status` →
  `State::slot_health_snapshot`) takes a single brief
  `parking_lot::Mutex` lock; the persist path locks `key_health` then
  `key_health_dirty` sequentially (never nested). No main-thread block
  found by inspection or by reproduction.
- **Not a panic in the serde change.** `PersistedKeyHealth.enabled` uses
  `#[serde(default = "default_true")]`; `reload_persisted_health`
  catches parse errors and returns `KeyHealthTracker::default()`. The
  on-disk `GLM.json` loads fine.

## Likely root cause (not yet fixed)

The dev build's launch path is `/Volumes/SDXC1TB/git/zed/target/.../Zed
Dev.app` — on a **slow external ExFAT SD card**. On first open after a
build/copy, macOS performs:

1. **Gatekeeper / notarization re-validation** of the 400 MB binary
   (it's ad-hoc / locally signed, so the assessment can be re-run each
   open), reading the whole binary off the slow card.
2. **Mmap + code-signing page-in** of the binary as the process starts,
   again reading from the slow card.
3. Possibly **Spotlight / fseventd** scanning the freshly-written app
   bundle on the external volume.

On the internal SSD (`/Applications`) the same binary opens in well
under a second. On the SD card the first launch of a freshly-built copy
takes minutes; subsequent launches of the *same* (cached) binary are
faster because the pages are already in the buffer cache.

## Evidence

- `ps`: running dev binary path is on `/Volumes/SDXC1TB`.
- Copying the app to `/Applications` and opening from there: opens in
  ~1s, no delay.
- `Zed.log` shows the dev build (`1.14.0+dev.9665d7d0ae…`) reaching
  `Rendered first frame` on every launch — i.e. it does complete, just
  slowly the first time after a build.
- No crash report, no panic in log (see "What it is NOT").

## Related (separate, also seen on launch)

`._*` AppleDouble resource-fork files (created by macOS on the ExFAT
volume) are parsed as JSON by the theme loader and by
`crates/zed/src/main.rs:~1867`, producing non-fatal `ERROR ... expected
value at line 1 column 1` lines on every launch. These are logged but
do not block launch and are unrelated to the delay. `.gitignore` now
excludes `._*`; a `dot_clean /Volumes/SDXC1TB` will clear the existing
ones.

## Reproduction

```sh
# Slow (minutes) — app binary on the slow external card:
open "/Volumes/SDXC1TB/git/zed/target/aarch64-apple-darwin/release/bundle/osx/Zed Dev.app"

# Fast (~1s) — same binary copied to the internal SSD:
cp -R "/Volumes/SDXC1TB/git/zed/target/aarch64-apple-darwin/release/bundle/osx/Zed Dev.app" /Applications/
open "/Applications/Zed Dev.app"
```

## Tasks

- [x] Confirm it is not a crash/hang in commit `9665d7d0ae` (cleared).
- [x] Record root-cause hypothesis (Gatekeeper/notarization + slow
      ExFAT card + Spotlight scan on freshly-built bundle).
- [-] Defer actual mitigation — not worth fixing now. Candidate
      mitigations if it becomes painful:
  - Build/copy the release bundle to `/Applications` (or any internal
    SSD path) before opening; don't open the bundle straight off the SD
    card.
  - Add the build output dir to Spotlight's privacy list to stop
    re-scans.
  - `xattr -cr` to strip quarantine on freshly-built local builds (only
    safe for locally-built, trusted binaries).
  - Keep the dev build on the internal disk (e.g. move `CARGO_TARGET_DIR`
    off the SD card for release bundle builds).

## Summary

Dev `Zed Dev.app` is slow to *first*-open because macOS re-validates a
400 MB locally-signed binary living on a slow ExFAT SD card. Not a
crash, not a code bug. Defer.
