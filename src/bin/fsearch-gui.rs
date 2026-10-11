//! Small native Win32 front end. No browser runtime or UI toolkit.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    if let Err(error) = fsearch::gui::run() {
        fsearch::gui::show_error(&error);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("fsearch-gui is only supported on Windows");
}
