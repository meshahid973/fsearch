//! Keyboard-driven, native terminal search.
//!
//! Each key updates the results from the saved index. The input reader
//! blocks while idle: there is no polling or background render loop.

use fsearch::{Catalog, DEFAULT_ENTRY_LIMIT, IndexProgress, ScanSummary};
use std::sync::{Arc, mpsc};
use crate::console::Terminal;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[92m";
const GREY: &str = "\x1b[90m";
const BLUE: &str = "\x1b[94m";
const BOLD: &str = "\x1b[1m";
const BG: &str = "\x1b[48;2;19;19;19m";
const KEY_EVENT: u16 = 1;
const RESIZE_EVENT: u16 = 4;

pub fn run(roots: Vec<PathBuf>, force_refresh: bool) -> io::Result<()> {
    let terminal = Terminal::enter()?;
    let mut screen = io::stdout().lock();
    write!(screen, "\x1b[?1049h\x1b[?25l")?;
    let result = interact(&terminal, &mut screen, roots, force_refresh);
    write!(screen, "\x1b[0m\x1b[?25h\x1b[?1049l")?;
    screen.flush()?;
    result
}

fn interact(term: &Terminal, out: &mut impl Write, roots: Vec<PathBuf>, force_refresh: bool) -> io::Result<()> {
    let mut query = String::new();
    let Some((mut catalog, mut cached)) = load_index(term, out, &roots, force_refresh, &mut query)? else {
        return Ok(());
    };
    let mut selected = 0usize;
    let mut pending_high_surrogate: Option<u16> = None;

    loop {
        let started = Instant::now();
        let (_, height) = term.dimensions();
        let limit = height.saturating_sub(10).clamp(3, 50);
        let hits = catalog.search(&query, limit);
        selected = selected.min(hits.len().saturating_sub(1));
        let elapsed = started.elapsed();
        draw(term, out, &query, &hits, selected, elapsed, catalog.summary(), cached, &roots)?;

        let event = term.read()?;
        if event.event_type == RESIZE_EVENT {
            continue;
        }
        if event.event_type != KEY_EVENT || event.key.pressed == 0 {
            continue;
        }
        match event.key.virtual_key {
            0x1B => return Ok(()), // Esc
            0x08 => { query.pop(); selected = 0; }, // Backspace
            0x26 => selected = selected.saturating_sub(1), // Up
            0x28 => selected = (selected + 1).min(hits.len().saturating_sub(1)), // Down
            0x0D => {
                if let Some(path) = hits.get(selected) {
                    let _ = reveal(path);
                }
            }
            0x52 if event.key.modifiers & 0x000c != 0 => {
                // Release the mapped file before replacing the index on disk.
                drop(hits);
                drop(catalog);
                let Some((updated, from_disk)) = load_index(term, out, &roots, true, &mut query)? else {
                    return Ok(());
                };
                catalog = updated;
                cached = from_disk;
                selected = 0;
            }
            _ => {
                let unit = event.key.character;
                if unit == 0 || unit < 0x20 {
                    continue;
                }
                if (0xD800..=0xDBFF).contains(&unit) {
                    pending_high_surrogate = Some(unit);
                    continue;
                }
                let character = if let Some(high) = pending_high_surrogate.take() {
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                } else {
                    char::from_u32(u32::from(unit))
                };
                if let Some(character) = character {
                    if query.chars().count() < 160 {
                        query.push(character);
                        selected = 0;
                    }
                }
            }
        }
    }
}

/// The initial disk crawl is asynchronous. The UI remains responsive and
/// shows counters instead of a static loading screen.
fn load_index(
    term: &Terminal,
    out: &mut impl Write,
    roots: &[PathBuf],
    refresh: bool,
    query: &mut String,
) -> io::Result<Option<(Catalog, bool)>> {
    let progress = Arc::new(IndexProgress::default());
    let worker_progress = Arc::clone(&progress);
    let worker_roots = roots.to_vec();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = Catalog::open_or_build_with_progress(
            &worker_roots,
            DEFAULT_ENTRY_LIMIT,
            refresh,
            &worker_progress,
        );
        let _ = tx.send(result);
    });

    let mut last = String::new();
    let mut pending_high = None;
    loop {
        match rx.try_recv() {
            Ok(Ok(result)) => return Ok(Some(result)),
            Ok(Err(error)) => return Err(error),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("index worker stopped unexpectedly"));
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let snapshot = progress.snapshot();
        let phase = match snapshot.phase {
            0 => "Opening saved index",
            1 => "Scanning files",
            2 => "Saving index",
            3 => "Opening new index",
            _ => "Finishing",
        };
        let label = format!(
            "{phase}...\n\n    {} files  ·  {} folders  ·  {} skipped\n\n    Search: {query}\n\n    Type while indexing  ·  Esc to cancel",
            number(snapshot.entries),
            number(snapshot.directories),
            number(snapshot.skipped),
        );
        if label != last {
            show_message(term, out, &label)?;
            last = label;
        }
        let Some(event) = term.read_timeout(150)? else { continue };
        if event.event_type != KEY_EVENT || event.key.pressed == 0 { continue }
        match event.key.virtual_key {
            0x1B => {
                progress.cancel();
                // The worker completes its current directory, drops temporary
                // files and exits. Keep the terminal responsive while waiting.
                show_message(term, out, "Stopping index scan...")?;
                let _ = rx.recv();
                return Ok(None);
            }
            0x08 => { query.pop(); }
            _ => {
                let unit = event.key.character;
                if unit < 0x20 { continue }
                if (0xD800..=0xDBFF).contains(&unit) {
                    pending_high = Some(unit);
                    continue;
                }
                let character = if let Some(high) = pending_high.take() {
                    char::decode_utf16([high, unit]).next().and_then(Result::ok)
                } else {
                    char::from_u32(u32::from(unit))
                };
                if let Some(character) = character {
                    if query.chars().count() < 160 { query.push(character); }
                }
            }
        }
    }
}

fn draw(
    term: &Terminal,
    out: &mut impl Write,
    query: &str,
    hits: &[&str],
    selected: usize,
    duration: Duration,
    info: ScanSummary,
    cached: bool,
    roots: &[PathBuf],
) -> io::Result<()> {
    let (width, height) = term.dimensions();
    let width = width.min(240);
    let header = if roots.len() > 1 { "whole disk".to_string() }
        else { roots.first().map(|p| p.display().to_string()).unwrap_or_default() };
    let margin = if width > 70 { "    " } else { "  " };
    let count = number(info.entries);
    let mut buffer = String::new();
    buffer.push_str("\x1b[H\x1b[2J");
    // Paint entire viewport to give the terminal the same clean dark surface.
    for line in 1..=height {
        buffer.push_str(BG);
        buffer.push_str("\x1b[2K");
        if line < height { buffer.push('\n'); }
    }
    buffer.push_str("\x1b[H");
    buffer.push_str(&format!("\n\n{margin}{BOLD}FSearch{RESET}{BG}    {GREY}{}  ·  {} files and folders{RESET}{BG}\n\n",
        shorten(&header, width.saturating_sub(44)), count));
    buffer.push_str(&format!("{margin}{BLUE}›{RESET}{BG} {BOLD}fsearch{RESET}{BG} {} {GREEN}{:0.2} ms{RESET}{BG}\n",
        shorten(query, width.saturating_sub(33)), duration.as_secs_f64() * 1000.0));
    let bar = "─".repeat(width.saturating_sub(margin.len() + 4));
    buffer.push_str(&format!("{margin}{GREY}{bar}{RESET}{BG}\n"));
    let max = height.saturating_sub(10);
    for (i, path) in hits.iter().take(max).enumerate() {
        let idx = path.rfind(['\\', '/']);
        let name = idx.map_or(*path, |n| &path[n + 1..]);
        let location = idx.map_or("", |n| &path[..n]);
        let file = shorten(name, (width / 3).max(16));
        let folder = shorten(location, width.saturating_sub(file.chars().count() + margin.len() + 6));
        let marker = if i == selected { format!("{BLUE}›{RESET}{BG}") } else { " ".to_string() };
        buffer.push_str(&format!("{margin}{marker} {BOLD}{file}{RESET}{BG}  {GREY}{folder}{RESET}{BG}\n"));
    }
    let partial = if info.truncated { "  ·  index incomplete (limit reached)" } else { "" };
    let origin = if cached { "saved index" } else { "new index" };
    let status = format!("{origin} · {count} entries · {} skipped{partial}", info.skipped);
    let foot = "↑↓ select   Enter reveal   Ctrl+R reindex   Esc quit";
    buffer.push_str(&format!("\x1b[{};1H{margin}{GREY}{}{RESET}{BG}", height.saturating_sub(2), shorten(&status, width.saturating_sub(margin.len() + 1))));
    buffer.push_str(&format!("\x1b[{};1H{margin}{GREY}{}{RESET}{BG}", height.saturating_sub(1), shorten(foot, width.saturating_sub(margin.len() + 1))));
    buffer.push_str(&format!("\x1b[5;{}H\x1b[?25h", margin.len() + 13 + query.chars().count()));
    out.write_all(buffer.as_bytes())?;
    out.flush()
}

fn show_message(term: &Terminal, out: &mut impl Write, message: &str) -> io::Result<()> {
    let _ = term;
    write!(out, "\x1b[?25l\x1b[2J\x1b[H\n\n    {BOLD}FSearch{RESET}\n\n    {GREY}{message}{RESET}")?;
    out.flush()
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max < 2 {
        return "…".to_string();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

fn number(value: usize) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, char) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(char);
    }
    formatted
}

#[cfg(windows)]
fn reveal(path: &str) -> io::Result<()> {
    let windir = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Windows directory not found"))?;
    std::process::Command::new(PathBuf::from(windir).join("explorer.exe"))
        .arg("/select,").arg(path).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formatted_counts() {
        assert_eq!(number(7_643_622), "7,643,622");
    }
    #[test]
    fn short_paths_are_safe() {
        assert_eq!(shorten("abc", 2), "a…");
        assert_eq!(shorten("日本語", 2), "日…");
    }
}
