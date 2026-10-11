//! FSearch: a separate interactive console by default, or one-shot CLI.

use fsearch::{Catalog, DEFAULT_ENTRY_LIMIT, default_roots};
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(windows)]
mod console;
#[cfg(windows)]
mod tui;
#[cfg(windows)]
mod latency;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).peekable();
    let interactive = match args.peek().map(String::as_str) {
        None => true,
        Some("--interactive") => {
            args.next();
            true
        }
        Some("-h" | "--help" | "help") => {
            help();
            return ExitCode::SUCCESS;
        }
        _ => false,
    };
    if interactive {
        return interactive_main(args);
    }
    search_main(args)
}

fn interactive_main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut roots = Vec::new();
    let mut refresh = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => match args.next() {
                Some(path) => roots.push(PathBuf::from(path)),
                None => return fail("--root requires a directory"),
            },
            "--reindex" => refresh = true,
            _ => return fail(&format!("unexpected argument: {flag}")),
        }
    }
    if roots.is_empty() {
        roots = default_roots();
    }

    #[cfg(windows)]
    {
        // Called without arguments: open a *new* terminal window. That
        // child enters raw console mode and never launches another child.
        if std::env::args().nth(1).as_deref() != Some("--interactive") {
            return match console::open_window_with(&roots, refresh) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(&error.to_string()),
            };
        }
        return match tui::run(roots, refresh) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("fsearch: {error}");
                eprintln!("Press Enter to close...");
                let _ = std::io::stdin().read_line(&mut String::new());
                ExitCode::FAILURE
            }
        };
    }

    #[cfg(not(windows))]
    {
        let _ = (roots, refresh);
        fail("interactive search requires Windows")
    }
}

fn search_main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(query) = args.next() else { return ExitCode::SUCCESS };
    let mut roots = Vec::<PathBuf>::new();
    let mut refresh = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => match args.next() {
                Some(path) => roots.push(PathBuf::from(path)),
                None => return fail("--root requires a directory"),
            },
            "--reindex" => refresh = true,
            other => return fail(&format!("unexpected argument: {other}")),
        }
    }
    if roots.is_empty() {
        roots = default_roots();
    }
    let (catalog, cached) = match Catalog::open_or_build(&roots, DEFAULT_ENTRY_LIMIT, refresh) {
        Ok(result) => result,
        Err(error) => return fail(&error.to_string()),
    };
    for path in catalog.search(&query, 100) {
        println!("{path}");
    }
    let s = catalog.summary();
    eprintln!(
        "{} {} entries ({} directories, {} skipped{})",
        if cached { "Loaded" } else { "Indexed" },
        s.entries, s.directories, s.skipped,
        if s.truncated { ", index incomplete" } else { "" }
    );
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("fsearch: {message}");
    ExitCode::FAILURE
}

fn help() {
    println!("FSearch - native Windows file search");
    println!("  fsearch                              Open interactive search in a new terminal");
    println!("  fsearch --interactive                Interactive search in this terminal");
    println!("  fsearch --interactive --root D:\\     Search only the specified folder");
    println!("  fsearch readme --root D:\\            Print matching files");
    println!("  fsearch readme --reindex             Rebuild the saved index before searching");
}
