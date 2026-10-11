//! Windows console support for the interactive search interface.

use std::io;
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
}

// Console handles are used only by the thread that owns the TUI.
impl Terminal {
    pub fn enter() -> io::Result<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let (mut in_mode, mut out_mode) = (0, 0);
            if input.is_null() || output.is_null()
                || GetConsoleMode(input, &mut in_mode) == 0
                || GetConsoleMode(output, &mut out_mode) == 0
            {
                return Err(io::Error::last_os_error());
            }
            // Use the terminal library to handle Windows Terminal/ConPTY keys.
            enable_raw_mode()?;
            if SetConsoleMode(output, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                let error = io::Error::last_os_error();
                let _ = disable_raw_mode();
                return Err(error);
            }
            SetConsoleOutputCP(65001);
            Ok(Self { output, saved_output: out_mode })
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
        }
    }
}
