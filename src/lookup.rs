//! Persistent, memory-mapped filename lookup tables.
//!
//! The lookup is derived from the existing catalog, so upgrading the search
//! engine does not require rescanning the filesystem. The only large temporary
//! allocations exist while a lookup is being built; warm queries map the file.

use crate::mapped::MappedFile;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"FSLOOK03";
const UNICODE_GRAM: u32 = 0x0100_0000;
const HEADER: usize = 32;
const GRAM_ENTRY: usize = 16;

pub struct Lookup {
    file: MappedFile,
    count: usize,
    directory: usize,
    postings: usize,
    gram_count: usize,
}

impl Lookup {
    pub fn open_or_build<'a>(
        source: &Path,
        count: usize,
        mut name_at: impl FnMut(usize) -> Option<&'a str>,
        on_build: impl FnOnce(),
        mut cancelled: impl FnMut() -> bool,
    ) -> io::Result<Self> {
        let meta = fs::metadata(source)?;
        let source_size = meta.len();
        let source_modified = modified_ns(&meta);
        let path = source.with_extension("lookup");
        if let Ok(lookup) = Self::open(&path, count, source_size, source_modified) {
            return Ok(lookup);
        }
        on_build();

        // One posting per distinct 3-byte gram per filename. The postings are
        // ordered by row ID so they can be intersected without hash tables.
        let mut grams: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let mut unique = Vec::new();
        for id in 0..count {
            if id % 2048 == 0 && cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "lookup preparation cancelled"));
            }
            let Some(name) = name_at(id) else { continue };
            if !name.is_ascii() {
                grams.entry(UNICODE_GRAM).or_default().push(id as u32);
            }
            unique.clear();
            for bytes in name.as_bytes().windows(3) {
                // An ASCII query must never be filtered out of a filename
                // containing Unicode elsewhere in its name.
                if bytes.iter().all(u8::is_ascii) {
                    unique.push(gram(bytes));
                }
            }
            unique.sort_unstable();
            unique.dedup();
            for key in &unique {
                grams.entry(*key).or_default().push(id as u32);
            }
        }

        let path_tmp = source.with_extension(format!("{}.lookup-tmp", std::process::id()));
        let result = (|| -> io::Result<()> {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "lookup preparation cancelled"));
            }
            let mut out = BufWriter::new(File::create(&path_tmp)?);
            out.write_all(MAGIC)?;
            out.write_all(&source_size.to_le_bytes())?;
            out.write_all(&source_modified.to_le_bytes())?;
            write_u32(&mut out, count)?;
            write_u32(&mut out, grams.len())?;
            let mut pos: u64 = 0;
            for (key, postings) in &grams {
                out.write_all(&key.to_le_bytes())?;
                out.write_all(&pos.to_le_bytes())?;
                write_u32(&mut out, postings.len())?;
                pos += postings.len() as u64;
            }
            for postings in grams.values() {
                if cancelled() {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "lookup preparation cancelled"));
                }
                for id in postings {
                    out.write_all(&id.to_le_bytes())?;
                }
            }
            out.flush()?;
            drop(out);
            // The source snapshot is never changed by this step; an incomplete
            // lookup is simply discarded and rebuilt on the next launch.
            // Rename over the old lookup only after the new file is complete.
            // If replacement fails, the existing lookup remains intact.
            fs::rename(&path_tmp, &path)?;
            Ok(())
        })();
        let _ = fs::remove_file(&path_tmp);
        result?;
        Self::open(&path, count, source_size, source_modified)
    }

    fn open(path: &Path, count: usize, size: u64, modified: u64) -> io::Result<Self> {
        let file = MappedFile::open(path)?;
        let b = file.bytes();
        if b.len() < HEADER
            || &b[..8] != MAGIC
            || u64_at(b, 8) != Some(size)
            || u64_at(b, 16) != Some(modified)
            || u32_at(b, 24) != Some(count as u32)
        {
            return Err(invalid("outdated lookup"));
        }
        let gram_count = u32_at(b, 28).unwrap_or(0) as usize;
        let directory = HEADER;
        let postings = directory.checked_add(gram_count.checked_mul(GRAM_ENTRY)
            .ok_or_else(|| invalid("gram overflow"))?)
            .ok_or_else(|| invalid("gram overflow"))?;
        if postings > b.len() { return Err(invalid("truncated lookup")); }
        let posting_count = (b.len() - postings) / 4;
        let mut prev = None;
        for i in 0..gram_count {
            let start = directory + i * GRAM_ENTRY;
            let key = u32_at(b, start).ok_or_else(|| invalid("bad gram"))?;
            if prev.is_some_and(|last| key <= last) { return Err(invalid("unsorted grams")); }
            prev = Some(key);
            let offset = u64_at(b, start + 4).ok_or_else(|| invalid("bad offset"))?;
            let length = u32_at(b, start + 12).ok_or_else(|| invalid("bad length"))?;
            if offset.checked_add(u64::from(length)).is_none_or(|end| end > posting_count as u64) {
                return Err(invalid("out of bounds posting"));
            }
        }
        Ok(Self { file, count, directory, postings, gram_count })
    }

    /// A dense posting list costs more to materialize, sort and rank than a
    /// direct scan of the memory-mapped catalog. Use indexed lookup only when
    /// at least one query gram meaningfully narrows the candidate set.
    pub fn selective(&self, needle: &str) -> bool {
        if !needle.is_ascii() || needle.len() < 3 { return false; }
        let smallest = needle.as_bytes().windows(3)
            .filter_map(|gram_bytes| self.postings_for(gram(gram_bytes)).map(|(_, n)| n))
            .min()
            .unwrap_or(0);
        smallest <= self.count.saturating_div(32).max(96)
    }

    /// Return row IDs containing every trigram in the ASCII query. We choose
    /// the shortest posting list first and verify actual substring matches
    /// later; gram matches alone are never considered sufficient.
    pub fn substring_candidates(&self, needle: &str) -> Option<Vec<u32>> {
        if !needle.is_ascii() || needle.len() < 3 { return None; }
        let unicode = self.postings_for(UNICODE_GRAM);
        let mut keys: Vec<u32> = needle.as_bytes().windows(3).map(gram).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut lists = Vec::new();
        let mut missing = false;
        for key in keys {
            match self.postings_for(key) {
                Some(range) => lists.push(range),
                None => { missing = true; break; }
            }
        }
        let mut matches = Vec::new();
        if !missing {
            lists.sort_unstable_by_key(|r| r.1);
            for i in 0..lists[0].1 {
                let Some(id) = self.posting_id(lists[0], i) else { continue };
                if lists[1..].iter().all(|range| self.posting_contains(*range, id)) {
                    matches.push(id);
                }
            }
        }
        // Unicode case folding can turn a non-ASCII character into ASCII.
        // Always verify these filenames, even when an ASCII gram is absent.
        if let Some(range) = unicode {
            for i in 0..range.1 {
                if let Some(id) = self.posting_id(range, i) { matches.push(id); }
            }
            matches.sort_unstable();
            matches.dedup();
        }
        Some(matches)
    }

    /// Single-edit typo candidates for longer ASCII queries. A substitution
    /// damages at most three trigrams; an adjacent swap damages at most four.
    /// We require five distinct query trigrams, otherwise callers must run
    /// an exhaustive fallback. Final ranking verifies actual edits.
    /// A bounded output prevents exceptionally common grams from spiking RAM.
    pub fn fuzzy_candidates(&self, needle: &str) -> Option<Vec<u32>> {
        if !needle.is_ascii() || needle.len() < 7 { return None; }
        let mut keys: Vec<u32> = needle.as_bytes().windows(3).map(gram).collect();
        keys.sort_unstable();
        keys.dedup();
        let mut lists: Vec<(usize, usize)> = keys.into_iter()
            .filter_map(|key| self.postings_for(key))
            .collect();
        // An adjacent swap can invalidate up to four trigrams. Unless five
        // distinct query trigrams exist, the candidate union is incomplete
        // and the caller must use its exhaustive fallback.
        if needle.as_bytes().windows(3)
            .map(gram).collect::<std::collections::BTreeSet<_>>().len() < 5 {
            return None;
        }
        lists.sort_unstable_by_key(|r| r.1);
        let mut out = Vec::new();
        let mut budget = 0usize;
        for list in lists.into_iter().take(5) {
            budget = budget.saturating_add(list.1);
            if budget > 2_000_000 { return None; }
            for i in 0..list.1 {
                if let Some(id) = self.posting_id(list, i) { out.push(id); }
            }
        }
        // Non-ASCII filenames can case-fold to an ASCII spelling. They
        // must remain candidates even when none of their raw-byte trigrams
        // match the typo query.
        if let Some(unicode) = self.postings_for(UNICODE_GRAM) {
            if budget.saturating_add(unicode.1) > 2_000_000 { return None; }
            for i in 0..unicode.1 {
                if let Some(id) = self.posting_id(unicode, i) { out.push(id); }
            }
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    fn postings_for(&self, key: u32) -> Option<(usize, usize)> {
        let b = self.file.bytes();
        let mut low = 0;
        let mut high = self.gram_count;
        while low < high {
            let mid = low + (high - low) / 2;
            let pos = self.directory + mid * GRAM_ENTRY;
            if u32_at(b, pos)? < key { low = mid + 1; } else { high = mid; }
        }
        if low >= self.gram_count { return None; }
        let pos = self.directory + low * GRAM_ENTRY;
        if u32_at(b, pos)? != key { return None; }
        Some((u64_at(b, pos + 4)? as usize, u32_at(b, pos + 12)? as usize))
    }

    fn posting_id(&self, range: (usize, usize), index: usize) -> Option<u32> {
        if index >= range.1 { return None; }
        u32_at(self.file.bytes(), self.postings + (range.0 + index) * 4)
    }

    fn posting_contains(&self, range: (usize, usize), needle: u32) -> bool {
        let mut low = 0;
        let mut high = range.1;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.posting_id(range, mid).unwrap_or(u32::MAX) < needle {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        self.posting_id(range, low) == Some(needle)
    }
}

fn modified_ns(meta: &fs::Metadata) -> u64 {
    use std::time::UNIX_EPOCH;
    meta.modified().ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64)
}
fn gram(bytes: &[u8]) -> u32 {
    (u32::from(bytes[0].to_ascii_lowercase()) << 16)
        | (u32::from(bytes[1].to_ascii_lowercase()) << 8)
        | u32::from(bytes[2].to_ascii_lowercase())
}
fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}
fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}
fn write_u32(out: &mut impl Write, value: usize) -> io::Result<()> {
    out.write_all(&u32::try_from(value).map_err(|_| invalid("count overflow"))?.to_le_bytes())
}
fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
