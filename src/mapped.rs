//! Read-only file mapping for the on-disk name catalog.

use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(windows)]
pub struct MappedFile {
    _file: File,
    mapping: *mut std::ffi::c_void,
    view: *const u8,
    len: usize,
}

// Both pointers refer to one immutable file view kept alive by this object.
#[cfg(windows)]
unsafe impl Send for MappedFile {}
#[cfg(windows)]
unsafe impl Sync for MappedFile {}

#[cfg(windows)]
impl MappedFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateFileMappingW(
                file: *mut std::ffi::c_void, security: *const std::ffi::c_void,
                protect: u32, max_high: u32, max_low: u32, name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn MapViewOfFile(
                handle: *mut std::ffi::c_void, access: u32,
                offset_high: u32, offset_low: u32, length: usize
            ) -> *const u8;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "index too large"))?;
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty index"));
        }
        let mapping = unsafe {
            CreateFileMappingW(file.as_raw_handle(), std::ptr::null(), 0x02, 0, 0, std::ptr::null())
        };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let view = unsafe { MapViewOfFile(mapping, 0x0004, 0, 0, 0) };
        if view.is_null() {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(mapping); }
            return Err(error);
        }
        Ok(Self { _file: file, mapping, view, len })
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.view, self.len) }
    }
}

#[cfg(windows)]
impl Drop for MappedFile {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn UnmapViewOfFile(view: *const std::ffi::c_void) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            UnmapViewOfFile(self.view.cast());
            CloseHandle(self.mapping);
        }
    }
}

#[cfg(not(windows))]
pub struct MappedFile {
    bytes: Box<[u8]>,
}

#[cfg(not(windows))]
impl MappedFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self { bytes: std::fs::read(path)?.into_boxed_slice() })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
