//! Filesystem enumeration. Windows uses FindFirstFileExW to collect directory
//! entries and attributes together, without a metadata call for every file.

use crate::ScanSummary;
use std::io;
use std::path::{Path, PathBuf};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::sync::mpsc;
use std::thread;

/// Counters updated during the initial scan. Readers never wait on the scanner.
#[derive(Default)]
pub struct Progress {
    pub entries: AtomicUsize,
    pub directories: AtomicUsize,
    pub skipped: AtomicUsize,
    pub cancelled: AtomicBool,
}

struct Work {
    pending: VecDeque<PathBuf>,
    unfinished: usize,
    closed: bool,
}

struct Batch {
    entries: Vec<(PathBuf, bool, bool)>,
    skipped: bool,
}

/// Enumerate whole directory trees with a small, fixed number of workers.
/// Batches and file writes are bounded; only pending directories are queued.
pub fn scan(
    roots: &[PathBuf],
    limit: usize,
    on_entry: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<ScanSummary> {
    scan_with_progress(roots, limit, &Progress::default(), on_entry)
}

pub fn scan_with_progress(
    roots: &[PathBuf],
    limit: usize,
    progress: &Progress,
    mut on_entry: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<ScanSummary> {
    if limit == 0 || roots.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "roots and limit must be nonempty"));
    }
    for root in roots {
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a directory: {}", root.display())));
        }
    }
    let queue = (Mutex::new(Work {
        pending: roots.iter().cloned().collect(),
        unfinished: roots.len(),
        closed: false,
    }), Condvar::new());
    let (tx, rx) = mpsc::sync_channel::<Batch>(12);
    let count = thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 6);
    thread::scope(|scope| {
        let queue = &queue;
        for _ in 0..count {
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let dir = {
                        let (lock, cv) = queue;
                        let mut work = lock.lock().unwrap();
                        while work.pending.is_empty() && !work.closed {
                            work = cv.wait(work).unwrap();
                        }
                        if work.closed {
                            break;
                        }
                        work.pending.pop_front().unwrap()
                    };
                    let mut entries = Vec::new();
                    let result = list_directory(&dir, |path, directory, reparse| {
                        if progress.cancelled.load(Ordering::Relaxed) { return Ok(false); }
                        entries.push((path, directory, reparse));
                        Ok(true)
                    });
                    let skipped = result.is_err();
                    {
                        let (lock, cv) = queue;
                        let mut work = lock.lock().unwrap();
                        if !work.closed {
                            for (path, directory, reparse) in &entries {
                                if *directory && !*reparse {
                                    work.pending.push_back(path.clone());
                                    work.unfinished += 1;
                                }
                            }
                        }
                        work.unfinished -= 1;
                        if work.unfinished == 0 {
                            work.closed = true;
                        }
                        cv.notify_all();
                    }
                    if tx.send(Batch { entries, skipped }).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);
        let mut summary = ScanSummary::default();
        let mut error = None;
        for batch in rx.iter() {
            if progress.cancelled.load(Ordering::Relaxed) {
                error = Some(io::Error::new(io::ErrorKind::Interrupted, "indexing cancelled"));
                break;
            }
            summary.directories += 1;
            progress.directories.fetch_add(1, Ordering::Relaxed);
            if batch.skipped {
                summary.skipped += 1;
                progress.skipped.fetch_add(1, Ordering::Relaxed);
            }
            for (path, _, _) in batch.entries {
                if let Err(err) = on_entry(&path) {
                    error = Some(err);
                    break;
                }
                summary.entries += 1;
                progress.entries.fetch_add(1, Ordering::Relaxed);
                if summary.entries >= limit {
                    summary.truncated = true;
                    break;
                }
            }
            if error.is_some() || summary.truncated {
                break;
            }
        }
        // Close before joining: blocked workers must stop sending batches.
        drop(rx);
        {
            let (lock, cv) = queue;
            let mut work = lock.lock().unwrap();
            work.closed = true;
            work.pending.clear();
            cv.notify_all();
        }
        if let Some(err) = error { Err(err) } else { Ok(summary) }
    })
}

/// Visit one folder. Returning false from the callback stops enumeration.
#[cfg(windows)]
fn list_directory(
    directory: &Path,
    mut visit: impl FnMut(PathBuf, bool, bool) -> io::Result<bool>,
) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct FindData {
        attributes: u32,
        created: FileTime,
        accessed: FileTime,
        modified: FileTime,
        size_high: u32,
        size_low: u32,
        reserved0: u32,
        reserved1: u32,
        name: [u16; 260],
        short_name: [u16; 14],
        file_type: u32,
        creator_type: u32,
        finder_flags: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FindFirstFileExW(
            pattern: *const u16, info_level: i32, data: *mut FindData,
            search_op: i32, filter: *const c_void, flags: u32
        ) -> *mut c_void;
        fn FindNextFileW(handle: *mut c_void, data: *mut FindData) -> i32;
        fn FindClose(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    const DIRECTORY: u32 = 0x10;
    const REPARSE: u32 = 0x400;
    const ERROR_NO_MORE_FILES: u32 = 18;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const LARGE_FETCH: u32 = 2;
    const INVALID_HANDLE: isize = -1;

    let pattern = directory.join("*");
    let pattern: Vec<u16> = pattern.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data = std::mem::MaybeUninit::<FindData>::uninit();
    let handle = unsafe { FindFirstFileExW(pattern.as_ptr(), 1, data.as_mut_ptr(), 0, std::ptr::null(), LARGE_FETCH) };
    if handle as isize == INVALID_HANDLE {
        let error = io::Error::last_os_error();
        // Empty folders return ERROR_FILE_NOT_FOUND.
        return if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) { Ok(()) } else { Err(error) };
    }

    // Handle cleanup also runs when a callback returns an error.
    struct SearchHandle(*mut c_void);
    impl Drop for SearchHandle {
        fn drop(&mut self) {
            unsafe { FindClose(self.0); }
        }
    }
    let _handle = SearchHandle(handle);
    loop {
        let entry = unsafe { data.assume_init_ref() };
        let length = entry.name.iter().position(|c| *c == 0).unwrap_or(entry.name.len());
        let name = &entry.name[..length];
        if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
            let path = directory.join(std::ffi::OsString::from_wide(name));
            let directory = entry.attributes & DIRECTORY != 0;
            let reparse = entry.attributes & REPARSE != 0;
            if !visit(path, directory, reparse)? {
                return Ok(());
            }
        }
        if unsafe { FindNextFileW(handle, data.as_mut_ptr()) } == 0 {
            let code = unsafe { GetLastError() };
            return if code == ERROR_NO_MORE_FILES { Ok(()) } else { Err(io::Error::from_raw_os_error(code as i32)) };
        }
    }
}

#[cfg(not(windows))]
fn list_directory(
    directory: &Path,
    mut visit: impl FnMut(PathBuf, bool, bool) -> io::Result<bool>,
) -> io::Result<()> {
    for child in std::fs::read_dir(directory)? {
        let child = child?;
        let kind = child.file_type()?;
        if !visit(child.path(), kind.is_dir(), kind.is_symlink())? {
            break;
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn local_drives() -> Vec<PathBuf> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for index in 0..26 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = b'A' + index as u8;
        let root = [letter as u16, ':' as u16, '\\' as u16, 0];
        // Include fixed and removable local drives, but not network shares.
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        if kind == 2 || kind == 3 {
            drives.push(PathBuf::from(format!("{}:\\", letter as char)));
        }
    }
    drives
}

#[cfg(not(windows))]
pub fn local_drives() -> Vec<PathBuf> {
    vec![crate::default_root()]
}
