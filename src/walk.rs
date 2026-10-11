//! Filesystem enumeration. Windows uses FindFirstFileExW to collect directory
//! entries and attributes together, without a metadata call for every file.

use crate::ScanSummary;
use std::io;
use std::path::{Path, PathBuf};

pub fn scan(
    roots: &[PathBuf],
    limit: usize,
    mut on_entry: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<ScanSummary> {
    if limit == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "entry limit must be positive"));
    }
    if roots.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no search roots"));
    }
    let mut summary = ScanSummary::default();
    let mut pending = Vec::from(roots);
    for root in roots {
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a directory: {}", root.display())));
        }
    }
    while let Some(dir) = pending.pop() {
        summary.directories += 1;
        let result = list_directory(&dir, |path, directory, reparse| {
            on_entry(&path)?;
            summary.entries += 1;
            if summary.entries >= limit {
                summary.truncated = true;
                return Ok(false);
            }
            if directory && !reparse {
                pending.push(path);
            }
            Ok(true)
        });
        match result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied
                || error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(5) => {
                summary.skipped += 1;
            }
            Err(error) => return Err(error),
        }
        if summary.truncated {
            break;
        }
    }
    Ok(summary)
}

/// Visit one folder. Returning false from the callback stops enumeration.
#[cfg(windows)]
fn list_directory(
    directory: &Path,
    mut visit: impl FnMut(PathBuf, bool, bool) -> io::Result<bool>,
) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

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
