//! Persistent filename catalog.
//!
//! One compact file contains a header, the indexed roots, path offsets, and
//! UTF-8 paths. Searches read directly from a read-only memory map.

use crate::{mapped::MappedFile, walk, lookup::Lookup};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FSWIN003";
const HEADER: usize = 32;
pub const DEFAULT_ENTRY_LIMIT: usize = 50_000_000;
const RESULT_LIMIT: usize = 5000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub entries: usize,
    pub directories: usize,
    pub skipped: usize,
    pub truncated: bool,
}

/// Live counters during an initial whole-drive scan.
#[derive(Default)]
pub struct IndexProgress {
    walk: walk::Progress,
    phase: AtomicU8,
}

impl IndexProgress {
    pub fn cancel(&self) {
        self.walk.cancelled.store(true, AtomicOrdering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.walk.cancelled.load(AtomicOrdering::Relaxed)
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            entries: self.walk.entries.load(AtomicOrdering::Relaxed),
            directories: self.walk.directories.load(AtomicOrdering::Relaxed),
            skipped: self.walk.skipped.load(AtomicOrdering::Relaxed),
            phase: self.phase.load(AtomicOrdering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
pub struct ProgressSnapshot {
    pub entries: usize,
    pub directories: usize,
    pub skipped: usize,
    /// 0: opening, 1: scanning, 2: saving, 3: loading, 4: complete, 5: building lookup
    pub phase: u8,
}

pub struct Catalog {
    mapped: MappedFile,
    count: usize,
    offsets: usize,
    masks: usize,
    paths: usize,
    lookup: Option<Lookup>,
    summary: ScanSummary,
}

impl Catalog {
    /// Load an existing snapshot or scan the given roots and save a new one.
    /// The boolean is true if the existing index was loaded.
    pub fn open_or_build(roots: &[PathBuf], limit: usize, refresh: bool) -> io::Result<(Self, bool)> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        Self::open_or_build_with_progress(roots, limit, refresh, &IndexProgress::default())
    }

    pub fn open_or_build_with_progress(
        roots: &[PathBuf],
        limit: usize,
        refresh: bool,
        progress: &IndexProgress,
    ) -> io::Result<(Self, bool)> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        let roots = normalize_roots(roots)?;
        let key = roots_key(&roots);
        let file = index_path(&key);
        if !refresh
            && let Ok(index) = Self::load_with_progress(&file, &key, Some(progress))
            && index.summary.entries <= limit
            && !(index.summary.truncated && index.summary.entries < limit)
        {
            progress.phase.store(4, AtomicOrdering::Relaxed);
            return Ok((index, true));
        }
        progress.phase.store(1, AtomicOrdering::Relaxed);
        Self::build_to_path(&roots, &file, limit, progress)?;
        progress.phase.store(3, AtomicOrdering::Relaxed);
        let index = Self::load_with_progress(&file, &key, Some(progress))?;
        progress.phase.store(4, AtomicOrdering::Relaxed);
        Ok((index, false))
    }

    /// Build a snapshot for a single folder.
    pub fn scan(root: &Path, limit: usize) -> io::Result<Self> {
        Self::open_or_build(&[root.to_path_buf()], limit, true).map(|(index, _)| index)
    }

    #[cfg(test)]
    fn load(file: &Path, expected_roots: &str) -> io::Result<Self> {
        Self::load_with_progress(file, expected_roots, None)
    }

    fn load_with_progress(
        file: &Path, expected_roots: &str, progress: Option<&IndexProgress>,
    ) -> io::Result<Self> {
        let mapped = MappedFile::open(file)?;
        let b = mapped.bytes();
        if b.len() < HEADER || &b[..8] != MAGIC {
            return Err(invalid("unsupported index"));
        }
        let roots_len = word(b, 8).ok_or_else(|| invalid("invalid header"))? as usize;
        let count = word(b, 12).ok_or_else(|| invalid("invalid count"))? as usize;
        let directories = word(b, 16).ok_or_else(|| invalid("invalid directory count"))? as usize;
        let skipped = word(b, 20).ok_or_else(|| invalid("invalid skipped count"))? as usize;
        let truncated = word(b, 24) == Some(1);
        let offsets = HEADER.checked_add(roots_len).ok_or_else(|| invalid("invalid root size"))?;
        let masks = offsets
            .checked_add(count.checked_add(1).and_then(|n| n.checked_mul(4)).ok_or_else(|| invalid("invalid index size"))?)
            .ok_or_else(|| invalid("invalid index size"))?;
        let paths = masks
            .checked_add(count.checked_mul(8).ok_or_else(|| invalid("invalid mask size"))?)
            .ok_or_else(|| invalid("invalid index size"))?;
        if paths > b.len() || b.get(HEADER..offsets) != Some(expected_roots.as_bytes()) {
            return Err(invalid("index belongs to another folder"));
        }
        let data_size = b.len() - paths;
        if data_size > u32::MAX as usize {
            return Err(invalid("index paths exceed format limit"));
        }
        let mut previous = 0usize;
        for i in 0..=count {
            let current = word(b, offsets + i * 4).ok_or_else(|| invalid("truncated offsets"))? as usize;
            if current < previous || current > data_size {
                return Err(invalid("invalid offset"));
            }
            previous = current;
        }
        if word(b, offsets) != Some(0) || previous != data_size {
            return Err(invalid("incomplete index"));
        }
        let mut catalog = Self {
            mapped, count, offsets, masks, paths, lookup: None,
            summary: ScanSummary { entries: count, directories, skipped, truncated },
        };
        // A missing or corrupt accelerator must never make the working name
        // catalog unavailable. Queries can fall back to the original scan.
        match Lookup::open_or_build(file, count, |id| {
            catalog.path_at(id).map(|p| p.rsplit(['\\', '/']).next().unwrap_or(p))
        }, || {
            if let Some(progress) = progress {
                progress.phase.store(5, AtomicOrdering::Relaxed);
            }
        }, || progress.is_some_and(IndexProgress::is_cancelled)) {
            Ok(lookup) => catalog.lookup = Some(lookup),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Err(error),
            // Missing disk space or an invalid optional lookup must not
            // prevent searching the already validated file catalog.
            Err(_) => {}
        }
        Ok(catalog)
    }

    fn build_to_path(roots: &[PathBuf], file: &Path, limit: usize, progress: &IndexProgress) -> io::Result<()> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        fs::create_dir_all(file.parent().ok_or_else(|| invalid("missing index directory"))?)?;
        let pid = std::process::id();
        let spool_path = file.with_extension(format!("{pid}.paths"));
        let offsets_path = file.with_extension(format!("{pid}.offsets"));
        let masks_path = file.with_extension(format!("{pid}.masks"));
        let next_path = file.with_extension(format!("{pid}.next"));
        let result = (|| -> io::Result<()> {
            let mut spool = BufWriter::new(File::create(&spool_path)?);
            // Stream offsets to disk rather than allocating 4 bytes for every
            // file (48 MB for a 12-million-entry catalog).
            let mut offsets = BufWriter::new(File::create(&offsets_path)?);
            let mut masks = BufWriter::new(File::create(&masks_path)?);
            offsets.write_all(&0u32.to_le_bytes())?;
            let mut position = 0u32;
            let summary = walk::scan_with_progress(roots, limit, &progress.walk, |path| {
                let display = display_path(path);
                let bytes = display.as_bytes();
                let length = u32::try_from(bytes.len()).map_err(|_| invalid("path too long"))?;
                position = position.checked_add(length).ok_or_else(|| invalid("index too large"))?;
                spool.write_all(bytes)?;
                offsets.write_all(&position.to_le_bytes())?;
                let name = display.rsplit(['\\', '/']).next().unwrap_or(&display);
                masks.write_all(&filename_mask(name).to_le_bytes())?;
                Ok(())
            })?;
            spool.flush()?;
            offsets.flush()?;
            masks.flush()?;
            drop(spool);
            drop(offsets);
            drop(masks);
            progress.phase.store(2, AtomicOrdering::Relaxed);

            let key = roots_key(roots);
            let keylen = u32::try_from(key.len()).map_err(|_| invalid("root list too long"))?;
            let mut out = BufWriter::new(File::create(&next_path)?);
            out.write_all(MAGIC)?;
            out.write_all(&keylen.to_le_bytes())?;
            write_u32(&mut out, summary.entries)?;
            write_u32(&mut out, summary.directories)?;
            write_u32(&mut out, summary.skipped)?;
            out.write_all(&(summary.truncated as u32).to_le_bytes())?;
            out.write_all(&0u32.to_le_bytes())?;
            out.write_all(key.as_bytes())?;
            let mut offsets = File::open(&offsets_path)?;
            copy_with_cancel(&mut offsets, &mut out, progress)?;
            let mut masks = File::open(&masks_path)?;
            copy_with_cancel(&mut masks, &mut out, progress)?;
            let mut input = File::open(&spool_path)?;
            copy_with_cancel(&mut input, &mut out, progress)?;
            out.flush()?;
            drop(out);
            drop(input);
            // Replacing a mapped snapshot can fail while another process has
            // it open. Keep the old file intact if that happens.
            fs::rename(&next_path, file)?;
            Ok(())
        })();
        let _ = fs::remove_file(spool_path);
        let _ = fs::remove_file(offsets_path);
        let _ = fs::remove_file(masks_path);
        let _ = fs::remove_file(next_path);
        result
    }

    pub fn summary(&self) -> ScanSummary {
        self.summary
    }

    pub fn accelerated(&self) -> bool {
        self.lookup.is_some()
    }

    /// Rank the best filenames, using the accelerator whenever possible.
    /// Rare or non-ASCII queries still have an exact, exhaustive fallback.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&str> {
        self.search_with_cancel(query, limit, || false)
    }

    /// Interrupt a superseded search between candidate batches.
    pub fn search_with_cancel(
        &self, query: &str, limit: usize, mut cancelled: impl FnMut() -> bool,
    ) -> Vec<&str> {
        let query = query.trim();
        if query.is_empty() || limit == 0 { return Vec::new(); }
        let limit = limit.min(RESULT_LIMIT);
        let needle = query.to_lowercase();
        let mut best = BinaryHeap::with_capacity(limit + 1);
        let mut used_index = false;

        if let Some(lookup) = &self.lookup
            && needle.is_ascii() && needle.len() >= 3
            && lookup.selective(&needle)
            && let Some(ids) = lookup.substring_candidates(&needle)
        {
            used_index = true;
            for (n, id) in ids.into_iter().enumerate() {
                if n % 1024 == 0 && cancelled() { return Vec::new(); }
                self.offer(id as usize, &needle, limit, false, &mut best);
            }
        }

        // When there are no substring hits, use overlapping trigrams to
        // narrow one-edit typo candidates on sufficiently long queries.
        let mut fuzzy_indexed = false;
        if used_index && best.is_empty() && needle.chars().count() >= 7
            && let Some(ids) = self.lookup.as_ref()
                .and_then(|lookup| lookup.fuzzy_candidates(&needle))
        {
            fuzzy_indexed = true;
            for (n, id) in ids.into_iter().enumerate() {
                if n % 1024 == 0 && cancelled() { return Vec::new(); }
                self.offer(id as usize, &needle, limit, true, &mut best);
            }
        }

        // One- and two-character searches, Unicode and short typo queries
        // retain the exhaustive fallback for correctness.
        if !used_index || (best.is_empty() && needle.chars().count() >= 5 && !fuzzy_indexed) {
            // Use a mask before touching the UTF-8 path section.
            let required = if needle.is_ascii() { filename_mask(&needle) } else { 0 };
            let allow_typo = needle.chars().count() >= 5;
            let bytes = self.mapped.bytes();
            for i in 0..self.count {
                if i % 4096 == 0 && cancelled() { return Vec::new(); }
                if required != 0 {
                    let Some(mask) = word64(bytes, self.masks + i * 8) else { continue };
                    let missing = (required & !mask).count_ones();
                    if missing > if allow_typo { 1 } else { 0 } { continue; }
                }
                self.offer(i, &needle, limit, true, &mut best);
            }
        }

        let mut found: Vec<_> = best.into_iter().map(|r| r.0).collect();
        found.sort_unstable_by(|a, b| b.cmp(a));
        found.into_iter().map(|m| m.path).collect()
    }

    fn offer<'a>(
        &'a self, id: usize, needle: &str, limit: usize, allow_fuzzy: bool,
        best: &mut BinaryHeap<Reverse<Match<'a>>>,
    ) {
        let Some(path) = self.path_at(id) else { return };
        let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
        let Some(score) = rank(name, needle) else { return };
        if !allow_fuzzy && score < 1000 { return; }
        let candidate = Reverse(Match { score, path });
        if best.len() < limit {
            best.push(candidate);
        } else if best.peek().is_some_and(|worst| candidate.0 > worst.0) {
            best.pop();
            best.push(candidate);
        }
    }

    fn path_at(&self, i: usize) -> Option<&str> {
        let bytes = self.mapped.bytes();
        let start = word(bytes, self.offsets + 4 * i)? as usize;
        let end = word(bytes, self.offsets + 4 * (i + 1))? as usize;
        std::str::from_utf8(bytes.get(self.paths + start..self.paths + end)?).ok()
    }
}

#[derive(Eq, PartialEq)]
struct Match<'a> {
    score: i32,
    path: &'a str,
}
impl PartialOrd for Match<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Match<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score.cmp(&other.score).then_with(|| other.path.cmp(self.path))
    }
}

/// Rank useful filename matches. Very loose subsequence matches were
/// returning unrelated files (e.g. "haha" matching "hashmap").
///
/// Matching is exact > prefix > substring > one typo (for 5+ characters).
fn rank(name: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return None;
    }
    if !name.is_ascii() || !query.is_ascii() {
        let lower = name.to_lowercase();
        // A Unicode filename may lowercase into an entirely ASCII name
        // (e.g. the Kelvin sign K → k). Preserve typo matching after folding.
        if lower.is_ascii() && query.is_ascii() {
            return rank_ascii(lower.as_bytes(), query.as_bytes());
        }
        return rank_unicode(&lower, query);
    }
    rank_ascii(name.as_bytes(), query.as_bytes())
}

fn rank_ascii(name: &[u8], query: &[u8]) -> Option<i32> {
    let length = name.len().min(500) as i32;
    if name.eq_ignore_ascii_case(query) {
        return Some(4000);
    }
    if query.len() <= name.len()
        && let Some(index) = name.windows(query.len())
            .position(|part| part.eq_ignore_ascii_case(query))
    {
        return Some(if index == 0 { 3000 } else { 2000 } - length - index as i32);
    }
    if query.len() < 5 || query.len() > name.len() {
        return None;
    }
    // A single typo is reasonable for longer queries, but scattered letters
    // are not a match. Check contiguous windows without allocating memory.
    let mut best = None;
    for window in name.windows(query.len()) {
        let mut wrong = None;
        let mut second = None;
        for (i, (&actual, &expected)) in window.iter().zip(query).enumerate() {
            if !actual.eq_ignore_ascii_case(&expected) {
                if wrong.is_none() {
                    wrong = Some(i);
                } else if second.is_none() {
                    second = Some(i);
                } else {
                    break;
                }
            }
        }
        let score = match (wrong, second) {
            (Some(_), None) => Some(400 - length),
            (Some(a), Some(b)) if b == a + 1
                && window[a].eq_ignore_ascii_case(&query[b])
                && window[b].eq_ignore_ascii_case(&query[a]) => Some(500 - length),
            _ => None,
        };
        best = best.max(score);
    }
    best
}

fn rank_unicode(name: &str, query: &str) -> Option<i32> {
    if name == query {
        return Some(4000);
    }
    name.find(query).map(|pos| if pos == 0 { 3000 } else { 2000 } - name.len().min(500) as i32 - pos as i32)
}

fn normalize_roots(roots: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    if roots.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no search roots"));
    }
    let mut normalized = Vec::with_capacity(roots.len());
    for root in roots {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a directory: {}", root.display())));
        }
        normalized.push(root);
    }
    normalized.sort_by_key(|path| display_path(path).to_lowercase());
    normalized.dedup_by(|a, b| display_path(a).eq_ignore_ascii_case(&display_path(b)));
    Ok(normalized)
}

fn roots_key(roots: &[PathBuf]) -> String {
    roots.iter().map(|path| display_path(path).to_lowercase()).collect::<Vec<_>>().join("\0")
}

fn index_path(key: &str) -> PathBuf {
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("FSearch");
    let hash = key.as_bytes().iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3));
    directory.join(format!("{hash:016x}.idx"))
}

fn display_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(network) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{network}");
        }
        if let Some(local) = path.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    path.into_owned()
}

/// A necessary-letter filter. Non-ASCII names use all bits, guaranteeing
/// that Unicode filenames aren't accidentally rejected.
fn filename_mask(text: &str) -> u64 {
    if !text.is_ascii() { return u64::MAX; }
    text.bytes().fold(0u64, |mask, byte| {
        let byte = byte.to_ascii_lowercase();
        let bit = match byte {
            b'a'..=b'z' => u32::from(byte - b'a'),
            b'0'..=b'9' => 26 + u32::from(byte - b'0'),
            b'.' => 36, b'-' => 37, b'_' => 38, b' ' => 39,
            _ => 40,
        };
        mask | (1u64 << bit)
    })
}

fn word64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}

/// Copy large index sections in chunks so Esc remains responsive while
/// the snapshot is being assembled, not just during directory enumeration.
fn copy_with_cancel(
    input: &mut impl Read, output: &mut impl Write, progress: &IndexProgress,
) -> io::Result<()> {
    let mut block = [0u8; 64 * 1024];
    loop {
        if progress.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "indexing cancelled"));
        }
        let size = input.read(&mut block)?;
        if size == 0 { return Ok(()); }
        output.write_all(&block[..size])?;
    }
}

fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}
fn write_u32(out: &mut impl Write, number: usize) -> io::Result<()> {
    out.write_all(&u32::try_from(number).map_err(|_| invalid("entry count too large"))?.to_le_bytes())
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn default_root() -> PathBuf {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn default_roots() -> Vec<PathBuf> {
    let drives = walk::local_drives();
    if drives.is_empty() { vec![default_root()] } else { drives }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fsearch-{}-{:?}", std::process::id(), std::thread::current().id()));
        fs::create_dir_all(dir.join("folder")).unwrap();
        fs::write(dir.join("folder").join("Minecraft.txt"), "hello").unwrap();
        fs::write(dir.join("folder").join("日本語.txt"), "hello").unwrap();
        dir
    }

    #[test]
    fn ranks_exact_before_partial() {
        assert!(rank("main.rs", "main.rs") > rank("main.rs.bak", "main.rs"));
        assert!(rank("main.rs", "mian.rs").is_some());
    }

    #[test]
    fn persistent_index_and_unicode() {
        let dir = fixture();
        let file = dir.join("index.dat");
        let roots = normalize_roots(std::slice::from_ref(&dir)).unwrap();
        Catalog::build_to_path(&roots, &file, 100, &IndexProgress::default()).unwrap();
        let catalog = Catalog::load(&file, &roots_key(&roots)).unwrap();
        assert_eq!(catalog.search("minecraft", 10).len(), 1);
        assert_eq!(catalog.search("日本語", 10).len(), 1);
        assert!(catalog.search("", 10).is_empty());
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_corrupt_header() {
        let dir = fixture();
        let file = dir.join("bad.dat");
        fs::write(&file, b"invalid").unwrap();
        assert!(Catalog::load(&file, "root").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_scan() {
        let dir = fixture();
        let roots = normalize_roots(std::slice::from_ref(&dir)).unwrap();
        let file = dir.join("bound.dat");
        Catalog::build_to_path(&roots, &file, 1, &IndexProgress::default()).unwrap();
        let catalog = Catalog::load(&file, &roots_key(&roots)).unwrap();
        assert_eq!(catalog.summary().entries, 1);
        assert!(catalog.summary().truncated);
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reuses_and_refreshes_snapshot() {
        let dir = fixture();
        let file = dir.join("later.rs");
        let roots = vec![dir.clone()];
        let (loaded, cached) = Catalog::open_or_build(&roots, 1000, true).unwrap();
        assert!(!cached);
        assert!(loaded.search("later", 10).is_empty());
        drop(loaded);
        fs::write(&file, "created").unwrap();
        let (old, cached) = Catalog::open_or_build(&roots, 1000, false).unwrap();
        assert!(cached);
        assert!(old.search("later", 10).is_empty());
        drop(old);
        let (fresh, cached) = Catalog::open_or_build(&roots, 1000, true).unwrap();
        assert!(!cached);
        assert_eq!(fresh.search("later", 10).len(), 1);
        drop(fresh);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn avoids_unrelated_subsequence_matches() {
        assert!(rank("shahzain.mp3", "haha").is_none());
        assert!(rank("hashmap.rs", "haha").is_none());
        assert!(rank("haha.txt", "haha").is_some());
        assert!(rank("main.rs", "mian.rs").is_some());
    }

    #[test]
    fn ranks_exact_prefix_and_typo() {
        assert!(rank("fsearch", "fsearch") > rank("fsearch.rs", "fsearch"));
        assert!(rank("fsearch.rs", "fsearch") > rank("my-fsearch.rs", "fsearch"));
        assert!(rank("fsearch.rs", "fsearch") > rank("fsearhc.rs", "fsearch"));
    }

    #[test]
    fn name_filter_keeps_possible_matches() {
        let exact = filename_mask("Minecraft Launcher.exe");
        let query = filename_mask("minecraft");
        assert_eq!((query & !exact).count_ones(), 0);
        let typo = filename_mask("minecrafx");
        assert!((query & !typo).count_ones() <= 1);
        assert_eq!(filename_mask("日本語"), u64::MAX);
    }


    // Opt-in performance check against a synthetic catalog. It avoids touching
    // the user's filesystem, so we can compare algorithms on the same data.
    #[test]
    #[ignore = "run with cargo test --release -- --ignored --nocapture"]
    fn benchmark_search() {
        use std::time::Instant;
        let root = std::env::temp_dir().join(format!("fsearch-bench-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("bench.idx");
        let roots = "synthetic-root";
        let count = std::env::var("FSEARCH_BENCH_FILES").ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(300_000)
            .clamp(1, 3_000_000);
        let mut offsets = Vec::with_capacity(count as usize + 1);
        let mut masks = Vec::with_capacity(count as usize);
        let mut paths = Vec::new();
        offsets.push(0u32);
        for i in 0..count {
            let name = if i % 1000 == 0 { format!("minecraft-{i}.exe") } else { format!("project-{i:08}.rs") };
            paths.extend_from_slice(format!("C:\\bench\\{name}").as_bytes());
            offsets.push(paths.len() as u32);
            masks.push(filename_mask(&name));
        }
        let mut out = BufWriter::new(File::create(&file).unwrap());
        out.write_all(MAGIC).unwrap();
        out.write_all(&(roots.len() as u32).to_le_bytes()).unwrap();
        out.write_all(&count.to_le_bytes()).unwrap();
        out.write_all(&1u32.to_le_bytes()).unwrap();
        out.write_all(&0u32.to_le_bytes()).unwrap();
        out.write_all(&0u32.to_le_bytes()).unwrap();
        out.write_all(&0u32.to_le_bytes()).unwrap();
        out.write_all(roots.as_bytes()).unwrap();
        for offset in offsets { out.write_all(&offset.to_le_bytes()).unwrap(); }
        for mask in masks { out.write_all(&mask.to_le_bytes()).unwrap(); }
        out.write_all(&paths).unwrap();
        out.flush().unwrap();
        drop(out);
        let catalog = Catalog::load(&file, roots).unwrap();
        for query in ["minecraft", "project", "proj", "minecarft"] {
            let _ = catalog.search(query, 30);
            let mut durations = Vec::new();
            let mut full_scan = Vec::new();
            for _ in 0..7 {
                let before = Instant::now();
                std::hint::black_box(catalog.search(query, 30));
                durations.push(before.elapsed());

                let started = Instant::now();
                let mut best = BinaryHeap::new();
                for id in 0..catalog.count {
                    catalog.offer(id, query, 30, true, &mut best);
                }
                std::hint::black_box(best);
                full_scan.push(started.elapsed());
            }
            durations.sort_unstable();
            full_scan.sort_unstable();
            println!("{query}: indexed {:.3} ms vs linear {:.3} ms ({} entries)",
                durations[3].as_secs_f64() * 1000.0, full_scan[3].as_secs_f64() * 1000.0, count);
        }
        drop(catalog);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn indexed_results_remain_accurate() {
        let dir = fixture();
        fs::create_dir_all(dir.join("another")).unwrap();
        for (where_, name) in [
            ("folder", "cat.txt"),
            ("another", "cat.txt"),
            ("folder", "my-cat.doc"),
            ("folder", "category.rs"),
            ("folder", "猫-cat.txt"),
            ("folder", "Kitty.txt"),
            ("folder", "caterpillar.txt"),
            ("folder", "Minecraft.exe"),
            ("folder", "ab_x_bc.txt"),
        ] {
            fs::write(dir.join(where_).join(name), "test").unwrap();
        }
        let (catalog, _) = Catalog::open_or_build(std::slice::from_ref(&dir), 200, true).unwrap();
        assert!(catalog.lookup.is_some(), "lookup must build for the fixture");
        let cat = catalog.search("cat", 50);
        assert_eq!(cat.len(), 6);
        assert!(cat[0].ends_with(r"cat.txt") || cat[0].ends_with("cat.txt"));
        assert_eq!(cat.iter().filter(|p| p.ends_with("cat.txt")).count(), 3);
        assert!(catalog.search("minecraft", 10).iter().any(|p| p.ends_with("Minecraft.exe")));
        assert_eq!(catalog.search("minecarft", 10).len(), 2);
        assert_eq!(catalog.search("日本語", 10).len(), 1);
        assert!(catalog.search("kitty", 10).iter().any(|p| p.ends_with("Kitty.txt")));
        assert!(catalog.search("xyzcatx", 10).is_empty());
        assert!(!catalog.search("ca", 10).is_empty());
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_lookup_is_rebuilt() {
        let dir = fixture();
        let roots = vec![dir.clone()];
        let (catalog, _) = Catalog::open_or_build(&roots, 100, true).unwrap();
        assert!(catalog.accelerated());
        let canonical = normalize_roots(&roots).unwrap();
        let lookup_path = index_path(&roots_key(&canonical)).with_extension("lookup");
        drop(catalog);
        fs::write(&lookup_path, b"corrupt lookup").unwrap();
        let (catalog, cached) = Catalog::open_or_build(&roots, 100, false).unwrap();
        assert!(cached, "the filesystem catalog should not be rebuilt");
        assert!(catalog.accelerated(), "the lookup should be recovered");
        assert_eq!(catalog.search("minecraft", 10).len(), 1);
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_search_returns_promptly() {
        let dir = fixture();
        let (catalog, _) = Catalog::open_or_build(std::slice::from_ref(&dir), 100, true).unwrap();
        let mut checks = 0usize;
        let result = catalog.search_with_cancel("a", 10, || {
            checks += 1;
            true
        });
        assert!(checks > 0);
        assert!(result.is_empty());
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repetitive_typo_falls_back_safely() {
        let dir = fixture();
        fs::write(dir.join("folder").join("aaaaaab.log"), "test").unwrap();
        let (catalog, _) = Catalog::open_or_build(std::slice::from_ref(&dir), 1000, true).unwrap();
        assert!(catalog.accelerated());
        assert!(catalog.search("aaaaaac", 20)
            .iter().any(|p| p.ends_with("aaaaaab.log")));
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unicode_typo_is_not_lost_by_trigrams() {
        let dir = fixture();
        fs::write(dir.join("folder").join("Kittenzz.txt"), "test").unwrap();
        let (catalog, _) = Catalog::open_or_build(std::slice::from_ref(&dir), 1000, true).unwrap();
        assert!(catalog.search("kittenxz", 20)
            .iter().any(|p| p.ends_with("Kittenzz.txt")));
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn index_copy_is_cancellable() {
        let progress = IndexProgress::default();
        let mut contents = Vec::new();
        let mut source = std::io::Cursor::new(vec![42u8; 200_000]);
        copy_with_cancel(&mut source, &mut contents, &progress).unwrap();
        assert_eq!(contents.len(), 200_000);
        progress.cancel();
        let result = copy_with_cancel(&mut std::io::Cursor::new(b"new"), &mut Vec::new(), &progress);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn cancelling_lookup_preparation_keeps_catalog() {
        let dir = fixture();
        let roots = normalize_roots(std::slice::from_ref(&dir)).unwrap();
        let file = dir.join("lookup-cancel.idx");
        Catalog::build_to_path(&roots, &file, 100, &IndexProgress::default()).unwrap();
        let progress = IndexProgress::default();
        progress.cancel();
        let err = match Catalog::load_with_progress(&file, &roots_key(&roots), Some(&progress)) {
            Ok(_) => panic!("lookup builder ignored cancellation"),
            Err(err) => err,
        };
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        // The original filename snapshot remains readable after cancellation.
        let catalog = Catalog::load(&file, &roots_key(&roots)).unwrap();
        assert_eq!(catalog.search("minecraft", 10).len(), 1);
        drop(catalog);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_limit() {
        let dir = fixture();
        assert!(Catalog::scan(&dir, 0).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
