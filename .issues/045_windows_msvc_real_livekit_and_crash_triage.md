# Issue 045: Windows msvc build fix — real LiveKit client enabled; crash triage + rebuild toolchain

Status: DONE 2026-10-31 — fix `e67ac117eb`, tooling `655bc14343`/`4245c05cad`/`af839a6598`, docs `5b98612959`/`93abc30aa4`

## Problem

After syncing develop (agent_mail arc, 2026-10-30), `cargo build --release` on
windows-msvc failed in `livekit_client`: `pub mod test` was gated on plain
`target_os = "windows"` (stale 2025-era cfg from upstream #27126) while the
`livekit_api` dependency had become `windows-gnu`-only (upstream #64826). Two
consequences of the stale gate:

1. msvc release builds broke outright once any Cargo.lock change invalidated
   livekit_client's fingerprint (last good compile predated 2025-09-27).
2. Worse, silently: since the fork's Sept 24 build, windows-msvc binaries linked
   the **mock** LiveKit client (`mock_client` + `test.rs`), never the real one.

## Fix

`e67ac117eb` — mirror upstream's 5 cfg lines: mock/test only under
`all(target_os = "windows", target_env = "gnu")`; msvc now compiles the real
`livekit_client` (libwebrtc path — which the `CXXFLAGS=/permissive` +
`msvc_stl_shim` workarounds in `docs/src/development/windows-notes.md` exist to
support). Verified byte-identical to upstream main.

## Tooling shipped this arc

- `script/dev-rebuild-windows.ps1` (`655bc14343`, rollback hardening `af839a6598`):
  fetch/ff-only pull (aborts on dirty tree — sibling-WIP safe), build with
  `CXXFLAGS=/permissive` set natively in PowerShell (Git Bash MSYS path
  conversion mangles the bash prefix form into `C:/Program Files/Git/permissive`),
  verifies built exe's ProductVersion embeds HEAD, rename-while-running swap,
  backup pruning, and PDB archival (`target/release/pdb-archive/<sha>.pdb`,
  keep 2) so deployed builds stay symbolizable (`4245c05cad`).
- WinDbg installed (`winget install Microsoft.WinDbg`); cdb at
  `C:\Program Files\WindowsApps\Microsoft.WinDbg_*\amd64\cdb.exe`. Triage guide
  in windows-notes.md (`93abc30aa4`).

## Forensics on the 2026-10-25 crash (closed)

`zed.exe.19656.dmp` (stale Sept 24 build): `INVALID_POINTER_WRITE c0000005`,
write to near-null `0xa`, IP `zed+0x12c5dd7c`, `FRAME_ONE_INVALID` (corrupted
return chain) — memory-corruption class, FFI/native-webrtc suspect. Binary and
its PDB are gone; unsymbolizable. Post-fix builds archive PDBs, so this dead end
cannot recur.

## Upstream drift (measured 2026-10-30 20:15Z)

Fork is **54 commits behind** zed/zed main (merge-base `ec86047d67`, Oct 27).
Notable for this box: `9cb5f90e3c` synthetic WM_MOUSEMOVE suppression (Windows
input), `f0210f1621` UAX #14 line breaking, `7f9a39a9c0` livekit-rust-sdks bump
(will churn Cargo.lock → webrtc-sys recompile → needs the PowerShell
CXXFLAGS wrapper; livekit_client cfg already upstream-aligned), `2daea4d488`
auto-compaction repeat fix, `2580a00264` gpui frame pacing. Sync is a curated
sibling operation — not executed unilaterally here.

## Follow-ups

- [x] Align livekit_client cfg with upstream (fix msvc build + real client)
- [x] PDB-per-deployed-build archival
- [x] cdb + symbolization workflow documented
- [x] rustc.exe Oct 29 dump triaged — one-off LLVM ICE (`generic_gep_type_iterator`),
      transient, no action
- [-] Observe first restart on the real-LiveKit msvc build — owner-gated (user
      must restart Zed); if a fresh dump appears, symbolize with
      `pdb-archive/655bc14343.pdb`. Post-restart health check (one line):
      `powershell -NoProfile -Command "(Get-Process zed).Id; (Get-Item \"$env:USERPROFILE\OneDrive\Desktop\zed.exe\").VersionInfo.ProductVersion; Get-ChildItem \"$env:LOCALAPPDATA\CrashDumps\" -Filter 'zed*' | Sort LastWriteTime -Desc | Select -First 2 Name,LastWriteTime"`
- [-] If restart shows RAM ballooning + freezes, check Zed.log for LSP
      notification loops first (upstream #57477 pattern) before suspecting LiveKit
