//! Keyboard-driven, native terminal search.
//!
//! Each key updates the results from the saved index. The input reader
//! blocks while idle: there is no polling or background render loop.

use fsearch::{Catalog, DEFAULT_ENTRY_LIMIT, IndexProgress, ScanSummary};
use std::sync::{Arc, mpsc};
use std::sync::atomic::{AtomicU64, Ordering};
use crate::console::Terminal;
use crate::latency::LatencyHistory;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[92m";
const GREY: &str = "\x1b[90m";
const BLUE: &str = "\x1b[94m";
const BOLD: &str = "\x1b[1m";
const BG: &str = "\x1b[48;2;19;19;19m";

pub fn run(roots: Vec<PathBuf>, force_refresh: bool) -> io::Result<()> {
    let terminal = Terminal::enter()?;
    let mut screen = io::stdout().lock();
    write!(screen, "\x1b[?1049h\x1b[?25l")?;
    let result = interact(&terminal, &mut screen, roots, force_refresh);
    write!(screen, "\x1b[0m\x1b[?25h\x1b[?1049l")?;
    screen.flush()?;
    result
}

#[derive(Default)]
struct ScreenState {
    size: Option<(usize, usize)>,
}

struct SearchResult {
    generation: u64,
    paths: Vec<String>,
    elapsed: Duration,
}

struct SearchWorker {
    queries: mpsc::Sender<(u64, String, usize)>,
    results: mpsc::Receiver<SearchResult>,
    handle: std::thread::JoinHandle<()>,
    newest: Arc<AtomicU64>,
}

fn start_search_worker(catalog: Catalog) -> SearchWorker {
    let (queries, rx) = mpsc::channel::<(u64, String, usize)>();
    let (tx, results) = mpsc::channel();
    let newest = Arc::new(AtomicU64::new(0));
    let worker_newest = Arc::clone(&newest);
    let handle = std::thread::spawn(move || {
        while let Ok(mut request) = rx.recv() {
            // Only the newest queued query matters while someone is typing.
            while let Ok(next) = rx.try_recv() {
                request = next;
            }
            let started = Instant::now();
            let paths = catalog.search_with_cancel(&request.1, request.2, || {
                worker_newest.load(Ordering::Relaxed) != request.0
            }).into_iter().map(str::to_owned).collect();
            if worker_newest.load(Ordering::Relaxed) != request.0 { continue; }
            if tx.send(SearchResult {
                generation: request.0,
                paths,
                elapsed: started.elapsed(),
            }).is_err() {
                break;
            }
        }
    });
    SearchWorker { queries, results, handle, newest }
}

impl SearchWorker {
    fn stop(self) {
        self.newest.store(u64::MAX, Ordering::Relaxed);
        drop(self.queries);
        let _ = self.handle.join();
    }
}

fn interact(term: &Terminal, out: &mut impl Write, roots: Vec<PathBuf>, force_refresh: bool) -> io::Result<()> {
    let mut query = String::new();
    let Some((catalog, mut cached)) = load_index(term, out, &roots, force_refresh, &mut query)? else {
        return Ok(());
    };
    let mut summary = catalog.summary();
    let mut accelerated = catalog.accelerated();
    let mut worker = start_search_worker(catalog);
    let mut selected = 0usize;
    let mut elapsed = Duration::ZERO;
    let mut history = LatencyHistory::new();
    let mut hits: Vec<String> = Vec::new();
    let mut generation = 0u64;
    let mut pending = false;
    let mut redraw = true;
    let mut screen_state = ScreenState::default();

    if !query.is_empty() {
        generation += 1;
        let (_, height) = term.dimensions();
        worker.newest.store(generation, Ordering::Relaxed);
        worker.queries.send((generation, query.clone(), result_limit(height)))
            .map_err(|_| io::Error::other("search worker stopped"))?;
        pending = true;
    }
    loop {
        while let Ok(found) = worker.results.try_recv() {
            if found.generation != generation { continue; }
            elapsed = found.elapsed;
            if !query.is_empty() { history.push(found.elapsed); }
            hits = found.paths;
            selected = selected.min(hits.len().saturating_sub(1));
            pending = false;
            redraw = true;
        }
        if redraw {
            draw(term, out, &query, &hits, selected, elapsed, summary, cached, &roots, pending, accelerated, &history, &mut screen_state)?;
            redraw = false;
        }

        let Some(event) = term.read_timeout(65)? else { continue };
        let Event::Key(event) = event else {
            if matches!(event, Event::Resize(..)) { redraw = true; }
            continue;
        };
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) { continue; }
        let mut changed = false;
        match event.code {
            KeyCode::Esc => {
                worker.stop();
                return Ok(());
            }
            KeyCode::Backspace => {
                changed = apply_text_key(&mut query, &event);
            }
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                redraw = true;
            }
            KeyCode::Down => {
                selected = (selected + 1).min(hits.len().saturating_sub(1));
                redraw = true;
            }
            KeyCode::Enter => {
                if !pending {
                    if let Some(path) = hits.get(selected) {
                        let _ = reveal(path);
                    }
                }
            }
            KeyCode::Char('r' | 'R') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                worker.stop(); // release the old file mapping before replacement
                let Some((updated, from_disk)) = load_index(term, out, &roots, true, &mut query)? else {
                    return Ok(());
                };
                summary = updated.summary();
                accelerated = updated.accelerated();
                cached = from_disk;
                history.clear();
                worker = start_search_worker(updated);
                hits.clear();
                pending = false;
                selected = 0;
                changed = true;
            }
            KeyCode::Char(_) => {
                changed = apply_text_key(&mut query, &event);
            }
            _ => {}
        }
        if changed {
            generation += 1;
            selected = 0;
            hits.clear();
            if query.is_empty() {
                pending = false;
                elapsed = Duration::ZERO;
            } else {
                let (_, height) = term.dimensions();
                worker.newest.store(generation, Ordering::Relaxed);
                worker.queries.send((generation, query.clone(), result_limit(height)))
                    .map_err(|_| io::Error::other("search worker stopped"))?;
                pending = true;
            }
            redraw = true;
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
            5 => "Preparing fast lookup",
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
        let Some(Event::Key(key)) = term.read_timeout(150)? else { continue };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) { continue; }
        match key.code {
            KeyCode::Esc => {
                progress.cancel();
                show_message(term, out, "Stopping index scan...")?;
                let _ = rx.recv();
                return Ok(None);
            }
            KeyCode::Backspace | KeyCode::Char(_) => {
                apply_text_key(query, &key);
            }
            _ => {}
        }
    }
}

fn apply_text_key(query: &mut String, event: &KeyEvent) -> bool {
    match event.code {
        KeyCode::Backspace => query.pop().is_some(),
        KeyCode::Char(character)
            if !event.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && query.chars().count() < 160 =>
        {
            query.push(character);
            true
        }
        _ => false,
    }
}

/// Keep result rows clear of the footer and the latency history.
fn result_limit(height: usize) -> usize {
    if height >= 16 {
        height.saturating_sub(13).clamp(3, 50)
    } else {
        height.saturating_sub(10).clamp(3, 50)
    }
}

fn draw(
    term: &Terminal,
    out: &mut impl Write,
    query: &str,
    hits: &[String],
    selected: usize,
    duration: Duration,
    info: ScanSummary,
    cached: bool,
    roots: &[PathBuf],
    pending: bool,
    accelerated: bool,
    history: &LatencyHistory,
    screen: &mut ScreenState,
) -> io::Result<()> {
    let (width, height) = term.dimensions();
    let width = width.min(240);
    let header = if roots.len() > 1 { "whole disk".to_string() }
        else { roots.first().map(|p| p.display().to_string()).unwrap_or_default() };
    let margin = if width > 70 { "    " } else { "  " };
    let count = number(info.entries);
    let mut buffer = String::new();
    buffer.push_str("\x1b[?25l");
    // The first frame and resizes paint the full dark surface. Subsequent
    // keystrokes clear only the content region, not the whole terminal.
    if screen.size.replace((width, height)) != Some((width, height)) {
        buffer.push_str("\x1b[H\x1b[2J");
        for line in 1..=height {
            buffer.push_str(BG);
            buffer.push_str("\x1b[2K");
            if line < height { buffer.push('\n'); }
        }
    } else {
        buffer.push_str(&format!("\x1b[3;1H{BG}\x1b[0J"));
    }
    buffer.push_str("\x1b[H");
    buffer.push_str(&format!("\n\n{margin}{BOLD}FSearch{RESET}{BG}    {GREY}{}  ·  {} files and folders{RESET}{BG}\n\n",
        shorten(&header, width.saturating_sub(44)), count));
    buffer.push_str(&format!("{margin}{BLUE}›{RESET}{BG} {BOLD}fsearch{RESET}{BG} {} {GREEN}{:0.2} ms{RESET}{BG}\n",
        shorten(query, width.saturating_sub(33)), duration.as_secs_f64() * 1000.0));
    let bar = "─".repeat(width.saturating_sub(margin.len() + 4));
    buffer.push_str(&format!("{margin}{GREY}{bar}{RESET}{BG}\n"));
    let max = result_limit(height);
    for (i, path) in hits.iter().take(max).enumerate() {
        let idx = path.rfind(['\\', '/']);
        let name = idx.map_or(path.as_str(), |n| &path[n + 1..]);
        let location = idx.map_or("", |n| &path[..n]);
        let file = shorten(name, (width / 3).max(16));
        let folder = shorten(location, width.saturating_sub(file.chars().count() + margin.len() + 6));
        let marker = if i == selected { format!("{BLUE}›{RESET}{BG}") } else { " ".to_string() };
        buffer.push_str(&format!("{margin}{marker} {BOLD}{file}{RESET}{BG}  {GREY}{folder}{RESET}{BG}\n"));
    }
    // Reserve two bottom rows for the green latency sparkline. A small
    // terminal omits it rather than letting the chart overlap search results.
    if height >= 16 {
        // Leave space for the peak label to avoid wrapping on narrow windows.
        let graph_width = width.saturating_sub(margin.len() + 27).min(56);
        if let Some(stats) = history.stats() {
            let title = format!(
                "Latency  ·  {} searches  ·  avg {:.2} ms  ·  p95 {:.2} ms",
                stats.count, stats.average_ms, stats.p95_ms,
            );
            buffer.push_str(&format!(
                "\x1b[{};1H{margin}{GREY}{}{RESET}{BG}",
                height - 5, shorten(&title, width.saturating_sub(margin.len() + 2)),
            ));
            buffer.push_str(&format!(
                "\x1b[{};1H{margin}{GREEN}{}{RESET}{BG}  {GREY}peak {:.2} ms{RESET}{BG}",
                height - 4, history.sparkline(graph_width), stats.peak_ms,
            ));
        } else {
            buffer.push_str(&format!(
                "\x1b[{};1H{margin}{GREY}Latency  ·  waiting for searches{RESET}{BG}",
                height - 5,
            ));
        }
    }
    let partial = if info.truncated { "  ·  index incomplete (limit reached)" } else { "" };
    let origin = if cached { "saved index" } else { "new index" };
    let mode = if accelerated { "fast lookup" } else { "scan fallback" };
    let status = if pending {
        format!("searching...  ·  {origin} · {mode} · {count} entries · {} skipped{partial}", info.skipped)
    } else {
        format!("{origin} · {mode} · {count} entries · {} skipped{partial}", info.skipped)
    };
    let foot = "↑↓ select   Enter reveal   Ctrl+R reindex   Esc quit";
    buffer.push_str(&format!("\x1b[{};1H{margin}{GREY}{}{RESET}{BG}", height.saturating_sub(2), shorten(&status, width.saturating_sub(margin.len() + 1))));
    buffer.push_str(&format!("\x1b[{};1H{margin}{GREY}{}{RESET}{BG}", height.saturating_sub(1), shorten(foot, width.saturating_sub(margin.len() + 1))));
    buffer.push_str(&format!("\x1b[5;{}H\x1b[?25h", margin.len() + 11 + query.chars().count()));
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
    fn keyboard_accepts_text_and_backspace() {
        let mut query = String::new();
        assert!(apply_text_key(&mut query, &KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)));
        assert!(apply_text_key(&mut query, &KeyEvent::new(KeyCode::Char('日'), KeyModifiers::NONE)));
        assert_eq!(query, "m日");
        assert!(apply_text_key(&mut query, &KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)));
        assert_eq!(query, "m");
        assert!(!apply_text_key(&mut query, &KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        assert_eq!(query, "m");
    }

    #[test]
    fn latency_graph_reserves_space_for_results() {
        assert_eq!(result_limit(30), 17);
        assert_eq!(result_limit(16), 3);
        assert_eq!(result_limit(12), 3);
        assert_eq!(result_limit(100), 50);
    }

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
