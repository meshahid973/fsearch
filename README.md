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

This opens a new terminal window. The first scan shows its progress, and you can type a search while it runs. Later launches reuse the saved index. Use the arrow keys to select, Enter to reveal a file in Explorer, Ctrl+R to refresh, or Esc to quit.

To use the current terminal instead:

```powershell
.\target\release\fsearch.exe --interactive
```

To print search results without opening an interactive window:

```powershell
.\target\release\fsearch.exe "minecraft"
.\target\release\fsearch.exe "readme" --root "D:\"
```

The index is saved in `%LOCALAPPDATA%\FSearch`. FSearch scans mounted fixed drives by default. New or deleted files require a refresh; searching inside file contents is not supported.

## License

MIT.
