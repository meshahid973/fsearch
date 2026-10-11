# FSearch

Fast file search for Windows, written in Rust. Includes a native desktop app and a command-line tool.

## Build

Install [Rust](https://rustup.rs/) and the Visual Studio C++ Build Tools, then run:

```powershell
cargo build --release
```

Open the desktop app:

```powershell
.\target\release\fsearch-gui.exe
```

Search from the terminal:

```powershell
.\target\release\fsearch.exe "readme" --root "D:\"
```

## Usage

Choose a folder, let FSearch scan it, and type a filename to filter the results. Double-click a result to show it in File Explorer. Directories that Windows won't let the app read are skipped.

FSearch uses the Windows API and doesn't require a browser or a separate runtime.

## License

MIT
