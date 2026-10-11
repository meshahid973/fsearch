//! Persistent filename catalog.
//!
//! One compact file contains a header, the indexed roots, path offsets, and
//! UTF-8 paths. Searches read directly from a read-only memory map.

use crate::{mapped::MappedFile, walk};
use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"FSWIN002";
const HEADER: usize = 32;
pub const DEFAULT_ENTRY_LIMIT: usize = 1_000_000;
const RESULT_LIMIT: usize = 5000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub entries: usize,
    pub directories: usize,
    pub skipped: usize,
    pub truncated: bool,
}

pub struct Catalog {
    mapped: MappedFile,
    count: usize,
    offsets: usize,
    paths: usize,
    summary: ScanSummary,
}

impl Catalog {
    /// Load an existing snapshot or scan the given roots and save a new one.
    /// The boolean is true if the existing index was loaded.
    pub fn open_or_build(roots: &[PathBuf], limit: usize, refresh: bool) -> io::Result<(Self, bool)> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        let roots = normalize_roots(roots)?;
        let key = roots_key(&roots);
        let file = index_path(&key);
        if !refresh {
            if let Ok(index) = Self::load(&file, &key) {
                if index.summary.entries <= limit && !(index.summary.truncated && index.summary.entries < limit) {
                    return Ok((index, true));
                }
            }
        }
        Self::build_to_path(&roots, &file, limit)?;
        Self::load(&file, &key).map(|index| (index, false))
    }

    /// Build a snapshot for a single folder.
    pub fn scan(root: &Path, limit: usize) -> io::Result<Self> {
        Self::open_or_build(&[root.to_path_buf()], limit, true).map(|(index, _)| index)
    }

    fn load(file: &Path, expected_roots: &str) -> io::Result<Self> {
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
        let paths = offsets
            .checked_add(count.checked_add(1).and_then(|n| n.checked_mul(4)).ok_or_else(|| invalid("invalid index size"))?)
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
        Ok(Self {
            mapped, count, offsets, paths,
            summary: ScanSummary { entries: count, directories, skipped, truncated },
        })
    }

    fn build_to_path(roots: &[PathBuf], file: &Path, limit: usize) -> io::Result<()> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        fs::create_dir_all(file.parent().ok_or_else(|| invalid("missing index directory"))?)?;
        let pid = std::process::id();
        let spool_path = file.with_extension(format!("{pid}.paths"));
        let next_path = file.with_extension(format!("{pid}.next"));
        let result = (|| -> io::Result<()> {
            let mut spool = BufWriter::new(File::create(&spool_path)?);
            let mut offsets = vec![0u32];
            let mut position = 0u32;
            let summary = walk::scan(roots, limit, |path| {
                let display = display_path(path);
                let bytes = display.as_bytes();
                let length = u32::try_from(bytes.len()).map_err(|_| invalid("path too long"))?;
                position = position.checked_add(length).ok_or_else(|| invalid("index too large"))?;
                spool.write_all(bytes)?;
                offsets.push(position);
                Ok(())
            })?;
            spool.flush()?;
            drop(spool);

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
            for offset in offsets {
                out.write_all(&offset.to_le_bytes())?;
            }
            let mut input = File::open(&spool_path)?;
            io::copy(&mut input, &mut out)?;
            out.flush()?;
            drop(out);
            drop(input);
            // Replacing a mapped snapshot can fail while another process has
            // it open. Keep the old file intact if that happens.
            fs::rename(&next_path, file)?;
            Ok(())
        })();
        let _ = fs::remove_file(spool_path);
        let _ = fs::remove_file(next_path);
        result
    }

    pub fn summary(&self) -> ScanSummary {
        self.summary
    }

    /// Return the best matches by filename, without allocating per entry.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&str> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let limit = limit.min(RESULT_LIMIT);
        let needle = query.to_lowercase();
        let mut best = BinaryHeap::with_capacity(limit + 1);
        for i in 0..self.count {
            let Some(path) = self.path_at(i) else { continue };
            let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
            let Some(score) = rank(name, &needle) else { continue };
            let candidate = Reverse(Match { score, path });
            if best.len() < limit {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate > *worst) {
                best.pop();
                best.push(candidate);
            }
        }
        let mut found: Vec<_> = best.into_iter().map(|r| r.0).collect();
        found.sort_unstable_by(|a, b| b.cmp(a));
        found.into_iter().map(|m| m.path).collect()
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

fn rank(name: &str, needle: &str) -> Option<i32> {
    if !name.is_ascii() || !needle.is_ascii() {
        let name = name.to_lowercase();
        return match_rank(name.as_bytes(), needle.as_bytes());
    }
    let bytes = name.as_bytes();
    let query = needle.as_bytes();
    if bytes.eq_ignore_ascii_case(query) {
        return Some(3000);
    }
    let position = bytes.windows(query.len()).position(|part| part.eq_ignore_ascii_case(query));
    if let Some(at) = position {
        return Some(if at == 0 { 2200 } else { 1600 } - name.len().min(500) as i32 - at as i32);
    }
    // Fast ASCII fallback: compare with case folding, but allocate nothing.
    match_rank_ascii(bytes, query)
}


fn match_rank_ascii(name: &[u8], needle: &[u8]) -> Option<i32> {
    let mut next = 0usize;
    let mut gaps = 0i32;
    for &byte in name {
        if next < needle.len() && byte.eq_ignore_ascii_case(&needle[next]) {
            next += 1;
        } else if next > 0 && next < needle.len() {
            gaps += 1;
        }
    }
    if next == needle.len() {
        return Some(500 - gaps - name.len().min(200) as i32);
    }
    if needle.len() >= 4 {
        for window in name.windows(needle.len()) {
            let mut i = 0usize;
            let mut swapped = false;
            while i < needle.len() {
                if window[i].eq_ignore_ascii_case(&needle[i]) {
                    i += 1;
                } else if !swapped && i + 1 < needle.len()
                    && window[i].eq_ignore_ascii_case(&needle[i + 1])
                    && window[i + 1].eq_ignore_ascii_case(&needle[i])
                {
                    swapped = true;
                    i += 2;
                } else {
                    break;
                }
            }
            if i == needle.len() && swapped {
                return Some(400 - name.len().min(200) as i32);
            }
        }
    }
    None
}

fn match_rank(name: &[u8], needle: &[u8]) -> Option<i32> {
    if needle.is_empty() {
        return None;
    }
    if name == needle {
        return Some(3000);
    }
    if let Some(pos) = name.windows(needle.len()).position(|part| part == needle) {
        return Some(if pos == 0 { 2200 } else { 1600 } - name.len().min(500) as i32 - pos as i32);
    }
    let mut next = 0;
    let mut gaps = 0i32;
    for &byte in name {
        if next < needle.len() && byte == needle[next] {
            next += 1;
        } else if next > 0 && next < needle.len() {
            gaps += 1;
        }
    }
    if next == needle.len() {
        return Some(500 - gaps - name.len().min(200) as i32);
    }
    // Adjacent letter swaps are common typos, e.g. mian -> main.
    if needle.len() >= 4 {
        for swap in 0..needle.len() - 1 {
            let mut candidate = needle.to_vec();
            candidate.swap(swap, swap + 1);
            if name.windows(candidate.len()).any(|w| w == candidate) {
                return Some(400 - name.len().min(200) as i32);
            }
        }
    }
    None
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
        let roots = normalize_roots(&[dir.clone()]).unwrap();
        Catalog::build_to_path(&roots, &file, 100).unwrap();
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
        let roots = normalize_roots(&[dir.clone()]).unwrap();
        let file = dir.join("bound.dat");
        Catalog::build_to_path(&roots, &file, 1).unwrap();
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
    fn invalid_limit() {
        let dir = fixture();
        assert!(Catalog::scan(&dir, 0).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
