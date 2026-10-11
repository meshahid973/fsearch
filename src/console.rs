//! Windows console support for the interactive search interface.

use std::io;
use std::fs::File;
use std::os::windows::io::AsRawHandle;
use std::mem;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::Duration;
use crossterm::event::{self, Event};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

type Handle = *mut std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BufferInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: Rect,
    max_window: Coord,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(id: u32) -> Handle;
    fn SetStdHandle(id: u32, handle: Handle) -> i32;
    fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut BufferInfo) -> i32;
    fn SetConsoleOutputCP(codepage: u32) -> i32;
}

pub fn open_window_with(roots: &[std::path::PathBuf], refresh: bool) -> io::Result<()> {
    // The initial process exits; only this child owns the new terminal.
    // The --interactive flag prevents a spawn loop.
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command.arg("--interactive");
    for root in roots {
        command.arg("--root").arg(root);
    }
    if refresh {
        command.arg("--reindex");
    }
    command.creation_flags(CREATE_NEW_CONSOLE).spawn()?;
    Ok(())
}

pub struct Terminal {
    output: Handle,
    saved_output: u32,
    original_input: Handle,
    _console_input: File,
}

// Console handles are used only by the thread that owns the TUI.
impl Terminal {
    pub fn enter() -> io::Result<Self> {
        // CREATE_NEW_CONSOLE can leave inherited standard handles pointing
        // at the calling terminal. Always bind stdin to this window's input.
        let console_input = File::open("CONIN$")?;
        unsafe {
            let original_input = GetStdHandle(STD_INPUT_HANDLE);
            if SetStdHandle(STD_INPUT_HANDLE, console_input.as_raw_handle()) == 0 {
                return Err(io::Error::last_os_error());
            }
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut input_mode = 0;
            let mut output_mode = 0;
            if output.is_null()
                || GetConsoleMode(console_input.as_raw_handle(), &mut input_mode) == 0
                || GetConsoleMode(output, &mut output_mode) == 0
            {
                let error = io::Error::last_os_error();
                SetStdHandle(STD_INPUT_HANDLE, original_input);
                return Err(error);
            }
            if let Err(error) = enable_raw_mode() {
                SetStdHandle(STD_INPUT_HANDLE, original_input);
                return Err(error);
            }
            if SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                let error = io::Error::last_os_error();
                let _ = disable_raw_mode();
                SetStdHandle(STD_INPUT_HANDLE, original_input);
                return Err(error);
            }
            SetConsoleOutputCP(65001);
            Ok(Self {
                output,
                saved_output: output_mode,
                original_input,
                _console_input: console_input,
            })
        }
    }

    /// Wait for console events without spinning.
    pub fn read_timeout(&self, timeout_ms: u32) -> io::Result<Option<Event>> {
        if event::poll(Duration::from_millis(u64::from(timeout_ms)))? {
            event::read().map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        unsafe {
            let mut info: BufferInfo = mem::zeroed();
            if GetConsoleScreenBufferInfo(self.output, &mut info) != 0 {
                return (
                    (i32::from(info.window.right) - i32::from(info.window.left) + 1).max(32) as usize,
                    (i32::from(info.window.bottom) - i32::from(info.window.top) + 1).max(12) as usize,
                );
            }
        }
        (100, 30)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            let _ = disable_raw_mode();
            SetConsoleMode(self.output, self.saved_output);
            SetStdHandle(STD_INPUT_HANDLE, self.original_input);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind};
    use std::fs::OpenOptions;

    #[repr(C)]
    struct SyntheticKey {
        kind: u16,
        padding: u16,
        pressed: i32,
        repeat: u16,
        virtual_key: u16,
        scan_code: u16,
        character: u16,
        modifiers: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WriteConsoleInputW(handle: Handle, events: *const SyntheticKey, count: u32, written: *mut u32) -> i32;
    }

    #[test]
    fn reads_key_from_new_console() {
        const CHILD_ENV: &str = "FSEARCH_CONSOLE_TEST_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let child = Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("console::tests::reads_key_from_new_console")
                .env(CHILD_ENV, "1")
                .creation_flags(CREATE_NEW_CONSOLE)
                .status()
                .unwrap();
            assert!(child.success(), "new console failed to read injected key");
            return;
        }

        // Runner stdin/stdout may be redirected. Bind output to the new
        // console before testing the same input setup used by the app.
        let output = OpenOptions::new().write(true).open("CONOUT$").unwrap();
        unsafe { assert_ne!(SetStdHandle(STD_OUTPUT_HANDLE, output.as_raw_handle()), 0); }
        let terminal = Terminal::enter().unwrap();
        let writer = OpenOptions::new().write(true).open("CONIN$").unwrap();
        let event = SyntheticKey {
            kind: 1,
            padding: 0,
            pressed: 1,
            repeat: 1,
            virtual_key: 0x41,
            scan_code: 0,
            character: b'a' as u16,
            modifiers: 0,
        };
        let mut written = 0;
        unsafe {
            assert_ne!(WriteConsoleInputW(writer.as_raw_handle(), &event, 1, &mut written), 0);
        }
        assert_eq!(written, 1);

        for _ in 0..10 {
            if let Some(Event::Key(key)) = terminal.read_timeout(100).unwrap() {
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('a') {
                    return;
                }
            }
        }
        panic!("console key event was not delivered");
    }
}
