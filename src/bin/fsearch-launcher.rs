//! Desktop entry point for the Windows terminal search.
//!
//! Unlike fsearch.exe, this launcher has no console of its own. It starts the
//! interactive search in one new terminal window and exits immediately.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

    let result = std::env::current_exe().and_then(|mut executable| {
        executable.set_file_name("fsearch.exe");
        Command::new(executable)
            .arg("--interactive")
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
    });

    if let Err(error) = result {
        show_error(&format!("Could not open FSearch.\n\n{error}"));
    }
}

#[cfg(windows)]
fn show_error(message: &str) {
    use std::ffi::c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(window: *mut c_void, text: *const u16, title: *const u16, flags: u32) -> i32;
    }
    let message: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "FSearch".encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(std::ptr::null_mut(), message.as_ptr(), title.as_ptr(), 0x10);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("FSearch for Windows is only available on Windows.");
}
