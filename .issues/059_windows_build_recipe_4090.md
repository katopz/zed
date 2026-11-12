# 059 — Windows build recipe for this fork on the 4090 (three load-bearing quirks)

Status: open — capture landed 2026-11-12 after job `01a1cfa6` built zed.exe in 41m17s; the three quirks below are mirror-local patches that a fresh checkout or a cargo-checkout reset will silently lose

The Windows exe built successfully on 4090 after five failed attempts. Each failure taught one requirement; none is documented in-repo, and two live OUTSIDE the repo (cargo checkout + config are mirror-local). This issue tracks folding them into something durable.

## The recipe (verified working 2026-11-12, job 01a1cfa6)

1. **Toolchain by cwd, never `--manifest-path`**: rustup resolves the toolchain from the CWD, not the manifest path. `cargo build --manifest-path E:/git/zed/...` from anywhere else falls back to default stable (1.93.0 on 4090) → MSRV failure (wasmtime/cranelift 48.x need ≥1.95; zed pins `channel = "1.98.1"`). Always `cd E:\git\zed` first, then plain `cargo build --release --package zed --bin zed`.
2. **clang-cl via `.cargo/config.toml` `[env]`** (mirror-local, uncommitted on 4090): cc-rs must pick clang-cl for the webrtc-sys C++ bridge (MSVC cl chokes on modern cxxbridge output — `C2146` at `shared_ptr_if_destructible<T, true>`). PATH-prepending and cmd `set CC` got swallowed by quoting layers; the cargo `[env]` block is the guaranteed-delivery form:
   ```toml
   [env]
   CC = "C:\\Program Files\\LLVM\\bin\\clang-cl.exe"
   CXX = "C:\\Program Files\\LLVM\\bin\\clang-cl.exe"
   ```
3. **`jsep.h` ODR class-key fix** (lives in the CARGO CHECKOUT, wiped on re-checkout): `webrtc-sys/include/livekit/jsep.h:38` says `class PeerContext;` while the cxx-generated header says `struct PeerContext;`. Under clang-cl the TUs mangle per first-seen declaration — MSVC mangling distinguishes `class` (V) from `struct` (U) — so the defining and referencing TUs produced DIFFERENT symbols for the same ctor: `LNK1120` with 4 livekit_ffi unresolveds. Fix: `class PeerContext;` → `struct PeerContext;`. macOS is immune (Itanium mangling ignores the class-key) — which is why the Mac DMG built clean all along.

## Tasks

- [ ] Decide the durable home for quirk 2 + 3: commit the `[env]` block to `.cargo/config.toml` (harmless on macOS/Linux — `[env]` CC/CXX would also affect them! Must be target-scoped `[env]'cfg(target_os = "windows")'`-equivalent or documented as 4090-local) and carry the jsep.h fix via a `[patch]` fork of livekit-rust-sdks OR an upstream PR
- [ ] Verify upstream status: check whether zed-industries/livekit-rust-sdks post-`0a1c519` already fixed the jsep.h class-key (then a lockfile bump replaces the checkout patch)
- [ ] The deployed `C:\Users\katop\OneDrive\Desktop\zed.exe` swap: staged as `zed-new.exe` (fresh 2026-11-12 19:47 build, 360MB) — swap blocked by the running instance (since 15:21), retry armed

## Refs

- Build log: `C:\Users\katop\.agent-mail\jobs\logs\01a1cfa6-8ce9-7a11-b895-f84763c6bd18.log` (exit 0, 41m17s)
- Symbol-level evidence: archive `jsep.o` defines `??0NativeSetRemoteSdpObserver@livekit_ffi@@QEAA@V?$Box@VPeerContext` (V=class) while `peer_connection.o` references `...U?$Box@UPeerContext` (U=struct)
- Session: zed-ca210ada@m3, 2026-11-12 (the auto_prompt summary-law + build-and-distribute thread)
