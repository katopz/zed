---
title: Windows Build Notes
description: "Additional notes for building Zed on Windows, including the MSVC STL shim workaround."
---

# Windows Build Notes

This page covers additional setup required to build Zed on Windows that is not included in the main [Building Zed for Windows](./windows.md) guide.

## Building with the MSVC STL Shim

If you are building with **Visual Studio Build Tools** (rather than the full Visual Studio IDE), you need three extra steps before running `cargo build`:

1. Initialize the MSVC environment
2. Set `CXXFLAGS=/permissive` for the `webrtc-sys` crate
3. Compile and link the MSVC STL shim library

### Quick Start

From a **cmd** or **PowerShell** prompt, run:

```bat
"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" x64
set CXXFLAGS=/permissive
cargo build --release
```

The remaining sections explain the two workarounds and how to set them up.

---

## Workaround 1: CXXFLAGS=/permissive

The `webrtc-sys` crate generates C++ bridge code via cxxbridge. MSVC 14.41 enables strict C++ conformance mode by default, which rejects `::rust::is_destructible<T>::value` inside a template parameter in the generated code.

Setting `CXXFLAGS=/permissive` before building relaxes this check and allows the generated code to compile.

You must set this variable every time `webrtc-sys` needs to be recompiled (e.g. after `cargo clean`).

---

## Workaround 2: MSVC STL Shim Library

### Why It Is Needed

The `webrtc-sys` crate links against a prebuilt native WebRTC library (`webrtc.lib`) that is downloaded during the build. This library was compiled with a newer version of MSVC than the 14.41 toolchain installed by the Visual Studio Build Tools.

The newer MSVC uses vectorized algorithm helpers in its C++ standard library. These are internal `__std_*` symbols that the compiler emits automatically when optimizing calls to `std::search`, `std::find_end`, `std::remove`, and similar algorithms. The prebuilt `webrtc.lib` references five such symbols that do not exist in MSVC 14.41's static libraries:

| Missing Symbol | Purpose |
|---|---|
| `__std_search_1` | Vectorized `std::search` for 1-byte elements |
| `__std_find_first_of_trivial_pos_1` | Vectorized `std::find_first_of` returning position for 1-byte elements |
| `__std_find_end_1` | Vectorized `std::find_end` for 1-byte elements |
| `__std_find_end_2` | Vectorized `std::find_end` for 2-byte elements |
| `__std_remove_8` | Vectorized `std::remove` for 8-byte elements |

Without these symbols, the linker reports errors like:

```
error LNK2001: unresolved external symbol __std_search_1
error LNK2001: unresolved external symbol __std_find_first_of_trivial_pos_1
error LNK2001: unresolved external symbol __std_find_end_1
error LNK2001: unresolved external symbol __std_find_end_2
error LNK2019: unresolved external symbol __std_remove_8
fatal error LNK1120: 5 unresolved externals
```

### The Shim

The `msvc_stl_shim/` directory in the repository root contains a small C++ file that provides fallback implementations of these five symbols. Each function delegates to the standard `<algorithm>` implementation that is available in MSVC 14.41.

The shim source is at `msvc_stl_shim/shim.cpp`. It must be compiled into a static library (`.lib`) and passed to the linker.

### Setting Up the Shim

**Step 1: Compile the shim**

Run this after initializing the MSVC environment with `vcvarsall.bat x64`:

```bat
cl.exe /MT /O2 /EHsc /std:c++20 /c msvc_stl_shim\shim.cpp /Fomsvc_stl_shim\shim.obj
lib.exe /OUT:msvc_stl_shim\msvc_stl_shim.lib msvc_stl_shim\shim.obj
```

**Step 2: Create a parent `.cargo/config.toml`**

Cargo merges `config.toml` files from parent directories. Create `E:\git\.cargo\config.toml` (one directory above the Zed repo) with the following content to add the shim library to the linker search path:

```toml
[target.'cfg(target_os = "windows")']
rustflags = [
    "-C", "link-arg=/LIBPATH:E:/git/zed/msvc_stl_shim",
    "-C", "link-arg=msvc_stl_shim.lib",
]
```

Adjust the path to match your actual checkout location. Use forward slashes to avoid TOML escape issues.

The directory structure should look like:

```
E:\git\
├── .cargo\
│   └── config.toml        ← parent config (links the shim)
└── zed\                    ← Zed repository
    ├── .cargo\
    │   └── config.toml    ← project config (unchanged)
    ├── msvc_stl_shim\
    │   ├── shim.cpp
    │   ├── shim.obj
    │   └── msvc_stl_shim.lib
    └── ...
```

### When the Shim Can Be Removed

This workaround is only needed because the prebuilt WebRTC library was compiled with a newer MSVC than what is currently available via the Build Tools installer. Once MSVC 14.42 (or later) ships with vectorized algorithm symbols in its standard libraries, the shim will no longer be necessary and can be removed along with the parent `.cargo/config.toml`.