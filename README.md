# FSearch for Windows

A lightweight native Windows file-search prototype built in Rust. No Electron,
browser runtime, background service, or third-party runtime dependencies.

## Build

Install the Rust MSVC toolchain and Visual Studio C++ build tools. In PowerShell:

```powershell
git pull origin main
cargo test
cargo build --release
.\target\release\fsearch-gui.exe
```

For the console version:

```powershell
.\target\release\fsearch.exe "readme" --root "D:\"
```

## What works now (Phase 1)

- Native Win32 GUI with editable scan root, search box and results.
- Background directory scan; the UI remains responsive while indexing.
- Case-insensitive ASCII filename substring matching.
- Scans accessible subdirectories without following Windows directory reparse points.
- Search results are limited to 200 in the GUI; scans have a safety cap of
  1 million entries. Permissions failures are counted.
- One-shot CLI for script use. `fsearch --help` explains its arguments.

## Not yet implemented

This is the Windows foundation, **not a replacement for the upstream speed
benchmarks**. Scans are not persisted between launches, and the CLI scans anew
on each invocation. The original mmap index, typo-tolerant ranking, content
index, change journal, and daemon are not integrated into the Windows port yet.
They remain accessible in the pre-port Git history.

Upcoming milestones: compact persistent index, fast ranking, Windows filesystem
notifications, robust incremental recovery, and memory/CPU benchmarks.

## Implementation layout

- `src/catalog.rs`: bounded, directory-scoped filename catalog.
- `src/main.rs`: command-line entry point.
- `src/bin/fsearch-gui.rs`: native GUI executable entry point.
- `src/gui.rs`: window lifecycle, background scan, and search display.
- `src/gui/win32.rs`: isolated Win32 ABI declarations.

The GUI uses standard Win32 controls instead of a web-based UI framework.
