//! Console entry point. The GUI lives in the separate fsearch-gui binary.
use fsearch::{Catalog, DEFAULT_ENTRY_LIMIT, default_root};
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

    let mut root = default_root();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => match args.next() {
                Some(path) => root = PathBuf::from(path),
                None => {
                    eprintln!("fsearch: --root requires a directory");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("fsearch: unexpected argument: {other}");
                help();
                return ExitCode::FAILURE;
            }
        }
    }

    let index = match Catalog::scan(&root, DEFAULT_ENTRY_LIMIT) {
        Ok(index) => index,
        Err(error) => {
            eprintln!("fsearch: could not scan {}: {error}", root.display());
            return ExitCode::FAILURE;
        }
    };
    for path in index.search(&query, 100) {
        println!("{path}");
    }
    let summary = index.summary();
    eprintln!("Indexed {} entries ({} directories, {} skipped{})", summary.entries, summary.directories, summary.skipped, if summary.truncated { ", entry limit reached" } else { "" });
    ExitCode::SUCCESS
}

fn help() {
    println!("FSearch - Windows filename search (phase 1)");
    println!("Usage: fsearch <filename> [--root DIRECTORY]");
    println!("The first version scans the selected tree for each CLI invocation.");
    println!("Run fsearch-gui.exe for a native desktop interface.");
}
