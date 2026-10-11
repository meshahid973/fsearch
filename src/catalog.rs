//! A bounded filename catalog. Scanning runs on a caller-owned worker thread.
//!
//! Each entry stores one path, not separate copies of its name and directory.
//! Windows reparse-point directories are not followed to avoid junction loops.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Avoid unbounded allocations when indexing unusually large directory trees.
pub const DEFAULT_ENTRY_LIMIT: usize = 1_000_000;

#[derive(Debug)]
struct Entry {
    path: Box<str>,
    name_start: usize,
}

impl Entry {
    fn new(path: &Path) -> Self {
        let display = display_path(path);
        let name_start = display.rfind(['/', '\\']).map_or(0, |i| i + 1);
        Self { path: display.into_boxed_str(), name_start }
    }

    fn name(&self) -> &str {
        &self.path[self.name_start..]
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanSummary {
    pub entries: usize,
    pub directories: usize,
    pub skipped: usize,
    pub truncated: bool,
}

/// An in-memory catalog used by the initial CLI and native GUI.
///
/// Only paths are retained; no file contents are loaded or indexed.
#[derive(Debug, Default)]
pub struct Catalog {
    entries: Vec<Entry>,
    summary: ScanSummary,
}

impl Catalog {
    pub fn scan(root: &Path, limit: usize) -> io::Result<Self> {
        if limit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
        }
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "root must be a directory"));
        }

        let mut catalog = Self::default();
        let mut pending = vec![root];
        while let Some(dir) = pending.pop() {
            catalog.summary.directories += 1;
            let listing = match fs::read_dir(&dir) {
                Ok(listing) => listing,
                Err(_) => {
                    catalog.summary.skipped += 1;
                    continue;
                }
            };

            for child in listing {
                let child = match child {
                    Ok(child) => child,
                    Err(_) => {
                        catalog.summary.skipped += 1;
                        continue;
                    }
                };
                let path = child.path();
                let ty = match child.file_type() {
                    Ok(ty) => ty,
                    Err(_) => {
                        catalog.summary.skipped += 1;
                        continue;
                    }
                };

                if ty.is_dir() && !ty.is_symlink() && !is_reparse_point(&path) {
                    pending.push(path.clone());
                }
                catalog.entries.push(Entry::new(&path));
                if catalog.entries.len() >= limit {
                    catalog.summary.truncated = true;
                    break;
                }
            }
            if catalog.summary.truncated {
                break;
            }
        }
        catalog.summary.entries = catalog.entries.len();
        Ok(catalog)
    }

    pub fn summary(&self) -> ScanSummary {
        self.summary
    }

    /// Returns up to `limit` full paths, matching filenames (not parents).
    /// This first milestone uses case-insensitive ASCII substring matching.
    pub fn search(&self, query: &str, limit: usize) -> Vec<&str> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter(|entry| name_contains(entry.name(), query))
            .take(limit)
            .map(|entry| entry.path.as_ref())
            .collect()
    }
}

fn name_contains(name: &str, query: &str) -> bool {
    // No per-entry allocations in the hot search path.
    name.as_bytes().windows(query.len()).any(|part| part.eq_ignore_ascii_case(query.as_bytes()))
        || (!query.is_ascii() && name.to_lowercase().contains(&query.to_lowercase()))
}

// canonicalize() uses verbatim paths on Windows. Show ordinary Explorer paths.
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

#[cfg(windows)]
fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

#[cfg(not(windows))]
fn is_reparse_point(_path: &Path) -> bool {
    false
}

/// Search the current user's files unless a different root is chosen.
pub fn default_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_search_ignores_ascii_case() {
        assert!(name_contains("Minecraft Launcher.exe", "mineCRAFT"));
        assert!(!name_contains("Minecraft Launcher.exe", "readme"));
    }

    #[test]
    fn empty_search_does_not_return_entire_index() {
        let catalog = Catalog::default();
        assert!(catalog.search("", 100).is_empty());
        assert!(catalog.search("anything", 0).is_empty());
    }

    #[test]
    fn scan_indexes_nested_files() {
        let dir = std::env::temp_dir().join(format!("fsearch-test-{}", std::process::id()));
        let nested = dir.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Example.TXT"), b"test").unwrap();

        let catalog = Catalog::scan(&dir, 100).unwrap();
        assert_eq!(catalog.search("example.txt", 10).len(), 1);
        assert!(!catalog.summary().truncated);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn explorer_paths_hide_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\Search\file.txt")), r"C:\Search\file.txt");
        assert_eq!(display_path(Path::new(r"\\?\UNC\server\share\file.txt")), r"\\server\share\file.txt");
    }

    #[test]
    fn limit_is_enforced() {
        assert!(Catalog::scan(Path::new("."), 0).is_err());
    }
}
