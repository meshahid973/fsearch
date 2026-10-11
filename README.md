# FSearch

Fast, lightweight file search for Windows. FSearch indexes filenames on local drives and lets you search them in a terminal window. Written in Rust.

## Build

Install [Rust](https://rustup.rs/) and the Visual Studio C++ Build Tools.

```powershell
cargo build --release
```

## Run

```powershell
.\target\release\fsearch.exe
```

This opens a new terminal window. Type to search, use the arrow keys to select a result, press Enter to reveal it in Explorer, Ctrl+R to rebuild the index, or Esc to quit.

To use the current terminal instead:

```powershell
.\target\release\fsearch.exe --interactive
```

To print search results without opening an interactive window:

```powershell
.\target\release\fsearch.exe "minecraft"
.\target\release\fsearch.exe "readme" --root "D:\"
```

The index is saved in `%LOCALAPPDATA%\FSearch`. New and deleted files are picked up when you reindex; automatic live monitoring is not available yet.

## License

MIT.
