//! Command-line search against the saved index.

use fsearch::{Catalog, DEFAULT_ENTRY_LIMIT, default_roots};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(query) = args.next() else {
        help();
        return ExitCode::SUCCESS;
    };
    if matches!(query.as_str(), "-h" | "--help" | "help") {
        help();
        return ExitCode::SUCCESS;
    }

    let mut roots = Vec::<PathBuf>::new();
    let mut refresh = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => match args.next() {
                Some(path) => roots.push(PathBuf::from(path)),
                None => {
                    eprintln!("fsearch: --root requires a directory");
                    return ExitCode::FAILURE;
                }
            },
            "--reindex" => refresh = true,
            other => {
                eprintln!("fsearch: unexpected argument: {other}");
                help();
                return ExitCode::FAILURE;
            }
        }
    }
    if roots.is_empty() {
        roots = default_roots();
    }

    let (catalog, cached) = match Catalog::open_or_build(&roots, DEFAULT_ENTRY_LIMIT, refresh) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("fsearch: {error}");
            return ExitCode::FAILURE;
        }
    };
    for path in catalog.search(&query, 100) {
        println!("{path}");
    }
    let s = catalog.summary();
    eprintln!(
        "{} {} entries ({} directories, {} skipped{})",
        if cached { "Loaded" } else { "Indexed" },
        s.entries, s.directories, s.skipped,
        if s.truncated { ", limit reached" } else { "" }
    );
    ExitCode::SUCCESS
}

fn help() {
    println!("FSearch - fast Windows filename search");
    println!("Usage: fsearch <query> [--root DIRECTORY]... [--reindex]");
    println!("Without --root, searches local drives. Use --reindex to refresh saved results.");
    println!("Run fsearch-gui.exe to open the desktop app.");
}
