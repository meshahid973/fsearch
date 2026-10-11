//! Minimal Win32 declarations used by the GUI.
//! Keeping these here avoids an additional crate and any runtime framework.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

pub type Handle = *mut c_void;
pub type Window = Handle;
pub type Instance = Handle;

#[repr(C)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
pub struct Message {
    pub hwnd: Window,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub point: Point,
    pub private: u32,
}

#[repr(C)]
pub struct WindowClass {
    pub style: u32,
    pub procedure: Option<unsafe extern "system" fn(Window, u32, usize, isize) -> isize>,
    pub class_extra: i32,
    pub window_extra: i32,
    pub instance: Instance,
    pub icon: Handle,
    pub cursor: Handle,
    pub background: Handle,
    pub menu_name: *const u16,
    pub class_name: *const u16,
}

pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const ES_AUTOHSCROLL: u32 = 0x0080;
pub const LBS_NOTIFY: u32 = 0x0001;
pub const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
pub const COLOR_WINDOW: isize = 5;
pub const CW_USEDEFAULT: i32 = i32::MIN;
pub const SW_SHOW: i32 = 5;

pub const WM_SIZE: u32 = 0x0005;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_NCDESTROY: u32 = 0x0082;
pub const WM_APP: u32 = 0x8000;
pub const GWLP_USERDATA: i32 = -21;
pub const EN_CHANGE: usize = 0x0300;
pub const BN_CLICKED: usize = 0;
pub const LB_ADDSTRING: u32 = 0x0180;
pub const LB_RESETCONTENT: u32 = 0x0184;

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterClassW(class: *const WindowClass) -> u16;
    pub fn CreateWindowExW(
        extended: u32, class: *const u16, title: *const u16, style: u32,
        x: i32, y: i32, width: i32, height: i32,
        parent: Window, menu: Handle, instance: Instance, param: *const c_void
    ) -> Window;
    pub fn DefWindowProcW(window: Window, message: u32, wparam: usize, lparam: isize) -> isize;
    pub fn GetMessageW(message: *mut Message, window: Window, min: u32, max: u32) -> i32;
    pub fn TranslateMessage(message: *const Message) -> i32;
    pub fn DispatchMessageW(message: *const Message) -> isize;
    pub fn PostQuitMessage(exit_code: i32);
    pub fn PostMessageW(window: Window, message: u32, wparam: usize, lparam: isize) -> i32;
    pub fn ShowWindow(window: Window, show: i32) -> i32;
    pub fn UpdateWindow(window: Window) -> i32;
    pub fn MoveWindow(window: Window, x: i32, y: i32, width: i32, height: i32, repaint: i32) -> i32;
    pub fn SetWindowTextW(window: Window, text: *const u16) -> i32;
    pub fn GetWindowTextLengthW(window: Window) -> i32;
    pub fn GetWindowTextW(window: Window, text: *mut u16, limit: i32) -> i32;
    pub fn SendMessageW(window: Window, message: u32, wparam: usize, lparam: isize) -> isize;
    pub fn SetWindowLongPtrW(window: Window, index: i32, value: isize) -> isize;
    pub fn GetWindowLongPtrW(window: Window, index: i32) -> isize;
    pub fn EnableWindow(window: Window, enabled: i32) -> i32;
    pub fn SetTimer(window: Window, id: usize, millis: u32, callback: Option<unsafe extern "system" fn(Window, u32, usize, u32)>) -> usize;
    pub fn KillTimer(window: Window, id: usize) -> i32;
    pub fn LoadCursorW(instance: Instance, cursor_name: *const u16) -> Handle;
    pub fn MessageBoxW(window: Window, text: *const u16, title: *const u16, style: u32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetModuleHandleW(module_name: *const u16) -> Instance;
}
