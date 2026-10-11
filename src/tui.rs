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
            draw(term, out, &query, &hits, selected, elapsed, summary, cached, &roots, pending, accelerated, &mut history, &mut screen_state)?;
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
    history: &mut LatencyHistory,
    screen: &mut ScreenState,
) -> io::Result<()> {
    let (width, height) = term.dimensions();
    let width = width.min(240);
    let margin = if width > 70 { "    " } else { "  " };
    let count = number(info.entries);
    let mut buffer = String::with_capacity(width.saturating_mul(height).min(20_000));
    buffer.push_str("\x1b[?25l");

    // Paint the full background only once or after a resize. Search changes
    // leave the title alone and clear just the dynamic region below it.
    let fresh = screen.size.replace((width, height)) != Some((width, height));
    if fresh {
        buffer.push_str("\x1b[H\x1b[2J");
        for row in 1..=height {
            buffer.push_str(&format!("\x1b[{row};1H{BG}\x1b[2K"));
        }
        let header = if roots.len() > 1 {
            "whole disk".to_string()
        } else {
            roots.first().map(|p| p.display().to_string()).unwrap_or_default()
        };
        buffer.push_str(&format!(
            "\x1b[3;{}H{BOLD}FSearch{RESET}{BG}    {GREY}{}  ·  {} files and folders{RESET}{BG}",
            margin.len() + 1, shorten(&header, width.saturating_sub(44)), count,
        ));
    } else {
        buffer.push_str(&format!("\x1b[5;1H{BG}\x1b[0J"));
    }

    let latency = if query.is_empty() {
        "— ms".to_string()
    } else if pending {
        "searching…".to_string()
    } else {
        format!("{:.2} ms", duration.as_secs_f64() * 1000.0)
    };
    let latency_col = right_align_col(width, latency.chars().count(), margin.len());
    let query_col = margin.len() + 11; // › fsearch followed by one space
    let available = latency_col.saturating_sub(query_col + 2);
    let visible = query_tail(query, available);
    let color = if pending || query.is_empty() { GREY } else { GREEN };
    buffer.push_str(&format!(
        "\x1b[5;{}H{BLUE}›{RESET}{BG} {BOLD}fsearch{RESET}{BG} {}",
        margin.len() + 1, visible,
    ));
    buffer.push_str(&format!(
        "\x1b[5;{latency_col}H{color}{latency}{RESET}{BG}",
    ));

    let bar = "─".repeat(width.saturating_sub(margin.len() + 4));
    buffer.push_str(&format!("\x1b[6;{}H{GREY}{bar}{RESET}{BG}", margin.len() + 1));
    for (index, path) in hits.iter().take(result_limit(height)).enumerate() {
        let split = path.rfind(['\\', '/']);
        let name = split.map_or(path.as_str(), |i| &path[i + 1..]);
        let folder = split.map_or("", |i| &path[..i]);
        let name = shorten(name, (width / 3).max(16));
        let location = shorten(
            folder,
            width.saturating_sub(name.chars().count() + margin.len() + 6),
        );
        let marker = if index == selected { BLUE } else { GREY };
        let symbol = if index == selected { "›" } else { " " };
        buffer.push_str(&format!(
            "\x1b[{};{}H{marker}{symbol}{RESET}{BG} {BOLD}{name}{RESET}{BG}  {GREY}{location}{RESET}{BG}",
            index + 7, margin.len() + 1,
        ));
    }

    // The graph matches the original terminal demo: a green waveform and a
    // median. On wide screens it shares one row with the caption. On smaller
    // screens it stacks or drops the caption before touching result space.
    if height >= 16 {
        let caption = "every keystroke is a whole-disk search";
        let median = history.median_ms();
        let median_label = median.map_or_else(
            || "median — ms".to_string(),
            |ms| format!("median {ms:.1} ms"),
        );
        let median_col = right_align_col(width, median_label.chars().count(), margin.len());
        let wide = width >= 100;
        let chart_row = height - 4;
        if wide {
            buffer.push_str(&format!(
                "\x1b[{chart_row};{}H{GREY}{caption}{RESET}{BG}",
                margin.len() + 1,
            ));
        } else if width >= 70 {
            buffer.push_str(&format!(
                "\x1b[{};{}H{GREY}{caption}{RESET}{BG}",
                height - 5, margin.len() + 1,
            ));
        }
        let chart_col = if wide {
            margin.len() + caption.len() + 4
        } else {
            margin.len() + 1
        };
        let chart_width = median_col.saturating_sub(chart_col + 3).min(32);
        if chart_width >= 2 {
            let empty = history.is_empty();
            let graph = history.sparkline(chart_width);
            let graph = if empty { "▁" } else { graph };
            buffer.push_str(&format!(
                "\x1b[{chart_row};{chart_col}H{GREEN}{graph}{RESET}{BG}",
            ));
        }
        buffer.push_str(&format!(
            "\x1b[{chart_row};{median_col}H{GREY}median {GREEN}{}{RESET}{BG}",
            median.map_or_else(|| "— ms".to_string(), |ms| format!("{ms:.1} ms")),
        ));
    }

    let partial = if info.truncated { " · index incomplete" } else { "" };
    let source = if cached { "saved index" } else { "new index" };
    let mode = if accelerated { "fast lookup" } else { "scan fallback" };
    let status = if pending {
        format!("searching... · {source} · {mode} · {count} entries · {} skipped{partial}", info.skipped)
    } else {
        format!("{source} · {mode} · {count} entries · {} skipped{partial}", info.skipped)
    };
    let help = "↑↓ select   Enter reveal   Ctrl+R reindex   Esc quit";
    buffer.push_str(&format!(
        "\x1b[{};{}H{GREY}{}{RESET}{BG}",
        height - 2, margin.len() + 1,
        shorten(&status, width.saturating_sub(margin.len() + 2)),
    ));
    buffer.push_str(&format!(
        "\x1b[{};{}H{GREY}{}{RESET}{BG}",
        height - 1, margin.len() + 1,
        shorten(help, width.saturating_sub(margin.len() + 2)),
    ));
    let cursor_col = (query_col + visible.chars().count()).min(latency_col.saturating_sub(1));
    buffer.push_str(&format!("\x1b[5;{cursor_col}H\x1b[?25h"));
    out.write_all(buffer.as_bytes())?;
    out.flush()
}

fn right_align_col(width: usize, text_len: usize, right_margin: usize) -> usize {
    width.saturating_sub(text_len + right_margin).saturating_add(1).max(1)
}

/// Keep the end of long queries visible while the right-aligned ms stays put.
fn query_tail(query: &str, width: usize) -> String {
    if width == 0 { return String::new(); }
    let length = query.chars().count();
    if length <= width { return query.to_string(); }
    if width == 1 { return "…".to_string(); }
    let skip = length - width + 1;
    let mut visible = String::with_capacity(width * 3);
    visible.push('…');
    visible.extend(query.chars().skip(skip));
    visible
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
    fn aligns_latency_without_overlapping_query() {
        assert_eq!(right_align_col(120, 8, 4), 109);
        assert_eq!(right_align_col(40, 8, 2), 31);
        assert_eq!(query_tail("minecraft", 5), "…raft");
        assert_eq!(query_tail("meow", 5), "meow");
        assert_eq!(query_tail("hello", 0), "");
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
