# FSearch

Lightweight file search for Windows, written in Rust. Search filenames across local drives in a terminal window.

## Download

- [Windows installer](https://github.com/meshahid973/fsearch/releases/latest/download/FSearch-Setup-0.2.0.exe) — installs FSearch with a Start Menu shortcut.
- [Portable ZIP](https://github.com/meshahid973/fsearch/releases/latest/download/FSearch-Windows-x64.zip) — extract and double-click `fsearch-launcher.exe`.

Runs on Windows 10/11 x64. Neither download requires Rust or administrator access. The downloads are currently unsigned, so Windows SmartScreen may show a warning.

## Use

Type to search. Use ↑/↓ to select, Enter to reveal a file in Explorer, Ctrl+R to rebuild the index, or Esc to quit.

The first launch indexes accessible files on your local fixed drives. Later launches reuse the saved index in `%LOCALAPPDATA%\FSearch`. New and deleted files appear after a manual refresh. Searching inside file contents is not supported.

## Build from source

Install [Rust](https://rustup.rs/) and Visual Studio C++ Build Tools, then run:

```powershell
cargo build --release --locked
.\target\release\fsearch-launcher.exe
```

Command-line searches:

```powershell
.\target\release\fsearch.exe "minecraft"
.\target\release\fsearch.exe "readme" --root "D:\"
```

## License

MIT. See [LICENSE](LICENSE).
