# FSearch

Fast file search for Windows, written in Rust. FSearch has a native desktop app and a command-line tool.

FSearch saves its filename index in `%LOCALAPPDATA%\FSearch` and reuses it the next time you open the app. It searches local drives by default, or any folders you specify. Files in unreadable folders are skipped.

## Build

Install [Rust](https://rustup.rs/) and the Visual Studio C++ Build Tools.

```powershell
cargo build --release
```

## Run

Desktop app:

```powershell
.\target\release\fsearch-gui.exe
```

Command line:

```powershell
.\target\release\fsearch.exe "minecraft"
.\target\release\fsearch.exe "readme" --root "D:\"
.\target\release\fsearch.exe "notes" --root "C:\Users" --root "D:\Projects"
```

Click **Refresh** in the app, or add `--reindex` to a terminal search, to update the saved index. Automatic filesystem monitoring and searching inside file contents are not supported yet.

## License

MIT.
