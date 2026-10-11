//! Windows console support for the interactive search interface.

use std::io;
use std::mem;
use std::os::windows::process::CommandExt;
use std::process::Command;

const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
const STD_OUTPUT_HANDLE: u32 = (-11i32) as u32;
const ENABLE_LINE_INPUT: u32 = 0x0002;
const ENABLE_ECHO_INPUT: u32 = 0x0004;
const ENABLE_WINDOW_INPUT: u32 = 0x0008;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

type Handle = *mut std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KeyEvent {
    pub pressed: i32,
    pub repeat: u16,
    pub virtual_key: u16,
    pub scan: u16,
    pub character: u16,
    pub modifiers: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct InputRecord {
    pub event_type: u16,
    _padding: u16,
    pub key: KeyEvent,
}

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
    fn ReadConsoleInputW(handle: Handle, records: *mut InputRecord, size: u32, read: *mut u32) -> i32;
    fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut BufferInfo) -> i32;
    fn SetConsoleOutputCP(codepage: u32) -> i32;
    fn WaitForSingleObject(handle: Handle, timeout_ms: u32) -> u32;
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
    input: Handle,
    output: Handle,
    saved_input: u32,
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
            let read_mode = (in_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) | ENABLE_WINDOW_INPUT;
            if SetConsoleMode(input, read_mode) == 0 {
                return Err(io::Error::last_os_error());
            }
            if SetConsoleMode(output, out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                SetConsoleMode(input, in_mode);
                return Err(io::Error::last_os_error());
            }
            SetConsoleOutputCP(65001);
            Ok(Self { input, output, saved_input: in_mode, saved_output: out_mode })
        }
    }

    /// Wait briefly for a key or a window-resize event without busy polling.
    pub fn read_timeout(&self, timeout_ms: u32) -> io::Result<Option<InputRecord>> {
        const WAIT_OBJECT_0: u32 = 0;
        const WAIT_TIMEOUT: u32 = 258;
        match unsafe { WaitForSingleObject(self.input, timeout_ms) } {
            WAIT_OBJECT_0 => self.read().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub fn read(&self) -> io::Result<InputRecord> {
        unsafe {
            let mut event = InputRecord::default();
            let mut read = 0;
            if ReadConsoleInputW(self.input, &mut event, 1, &mut read) == 0 || read != 1 {
                return Err(io::Error::last_os_error());
            }
            Ok(event)
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
            SetConsoleMode(self.input, self.saved_input);
            SetConsoleMode(self.output, self.saved_output);
        }
    }
}
