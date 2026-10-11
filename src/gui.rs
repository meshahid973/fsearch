//! Native Windows desktop UI: directory, search box, results, and progress.
//!
//! Scans run on a worker thread. UI updates only happen on the window thread;
//! workers communicate through a shared slot and one posted Win32 message.

mod win32;

use crate::{Catalog, DEFAULT_ENTRY_LIMIT};
use std::mem;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};
use win32::*;

const ROOT_ID: usize = 101;
const INDEX_ID: usize = 102;
const SEARCH_ID: usize = 103;
const SEARCH_TIMER: usize = 1;
const SCAN_READY: u32 = WM_APP + 1;
const RESULTS_LIMIT: usize = 200;

type ScanResult = Result<Catalog, String>;
type ResultSlot = Arc<Mutex<Option<ScanResult>>>;

struct App {
    root: Window,
    index_button: Window,
    search: Window,
    results: Window,
    status: Window,
    catalog: Option<Catalog>,
    result_slot: ResultSlot,
    scanning: bool,
}

/// Windows GUI. Search data is kept in-process only for this first milestone.
pub fn run(root: PathBuf) -> Result<(), String> {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        if instance.is_null() {
            return Err("could not get Windows module handle".into());
        }

        let name = wide("FSearchNativeWindow");
        let class = WindowClass {
            style: 0,
            procedure: Some(window_proc),
            class_extra: 0,
            window_extra: 0,
            instance,
            icon: ptr::null_mut(),
            cursor: LoadCursorW(ptr::null_mut(), 32512usize as *const u16),
            background: (COLOR_WINDOW + 1) as Handle,
            menu_name: ptr::null(),
            class_name: name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            return Err("could not register the FSearch window".into());
        }

        let title = wide("FSearch - Windows");
        let hwnd = CreateWindowExW(
            0, name.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, 900, 620,
            ptr::null_mut(), ptr::null_mut(), instance, ptr::null(),
        );
        if hwnd.is_null() {
            return Err("could not create the FSearch window".into());
        }

        let root_edit = control(hwnd, instance, "EDIT", &root.to_string_lossy(), ROOT_ID,
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL);
        let index_button = control(hwnd, instance, "BUTTON", "Index", INDEX_ID,
            WS_CHILD | WS_VISIBLE);
        let search = control(hwnd, instance, "EDIT", "", SEARCH_ID,
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL);
        let results = control(hwnd, instance, "LISTBOX", "", 0,
            WS_CHILD | WS_VISIBLE | WS_VSCROLL | LBS_NOTIFY | LBS_NOINTEGRALHEIGHT | WS_BORDER);
        let status = control(hwnd, instance, "STATIC", "Ready", 0, WS_CHILD | WS_VISIBLE);

        if [root_edit, index_button, search, results, status].iter().any(|h| h.is_null()) {
            return Err("could not create native controls".into());
        }

        let app = Box::new(App {
            root: root_edit,
            index_button,
            search,
            results,
            status,
            catalog: None,
            result_slot: Arc::new(Mutex::new(None)),
            scanning: false,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
        layout(hwnd, 900, 580);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        if let Some(app) = state(hwnd) {
            start_scan(hwnd, app);
        }

        let mut message: Message = mem::zeroed();
        loop {
            match GetMessageW(&mut message, ptr::null_mut(), 0, 0) {
                n if n > 0 => {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                0 => return Ok(()),
                _ => return Err("Windows message loop failed".into()),
            }
        }
    }
}

unsafe fn control(parent: Window, instance: Instance, class: &str, label: &str, id: usize, style: u32) -> Window {
    let class = wide(class);
    let label = wide(label);
    unsafe {
        CreateWindowExW(
            0, class.as_ptr(), label.as_ptr(), style,
            0, 0, 100, 25, parent, id as Handle, instance, ptr::null(),
        )
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn state(hwnd: Window) -> Option<&'static mut App> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    unsafe { pointer.as_mut() }
}

unsafe fn layout(hwnd: Window, width: i32, height: i32) {
    let Some(app) = (unsafe { state(hwnd) }) else { return };
    let w = width.max(400);
    let h = height.max(250);
    unsafe {
        MoveWindow(app.root, 12, 12, w - 116, 30, 1);
        MoveWindow(app.index_button, w - 94, 12, 82, 30, 1);
        MoveWindow(app.search, 12, 52, w - 24, 34, 1);
        MoveWindow(app.results, 12, 96, w - 24, h - 134, 1);
        MoveWindow(app.status, 12, h - 30, w - 24, 22, 1);
    }
}

fn read_text(hwnd: Window) -> String {
    unsafe {
        let length = GetWindowTextLengthW(hwnd).clamp(0, 4096) as usize;
        let mut buf = vec![0u16; length + 1];
        let count = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..count.max(0) as usize])
    }
}

fn status(app: &App, message: &str) {
    unsafe { SetWindowTextW(app.status, wide(message).as_ptr()); }
}

fn start_scan(hwnd: Window, app: &mut App) {
    if app.scanning {
        return;
    }
    let root = PathBuf::from(read_text(app.root).trim());
    if root.as_os_str().is_empty() {
        status(app, "Enter a directory to index");
        return;
    }
    app.catalog = None;
    app.scanning = true;
    unsafe {
        EnableWindow(app.index_button, 0);
        SendMessageW(app.results, LB_RESETCONTENT, 0, 0);
    }
    status(app, &format!("Indexing {} ...", root.display()));
    let shared = Arc::clone(&app.result_slot);
    std::thread::spawn(move || {
        let result = Catalog::scan(&root, DEFAULT_ENTRY_LIMIT).map_err(|e| e.to_string());
        if let Ok(mut slot) = shared.lock() {
            *slot = Some(result);
        }
        unsafe {
            PostMessageW(hwnd, SCAN_READY, 0, 0);
        }
    });
}

fn scan_ready(app: &mut App) {
    let result = app.result_slot.lock().ok().and_then(|mut slot| slot.take());
    app.scanning = false;
    unsafe { EnableWindow(app.index_button, 1); }
    match result {
        Some(Ok(catalog)) => {
            let summary = catalog.summary();
            app.catalog = Some(catalog);
            status(app, &format!(
                "{} files/folders indexed  |  {} directories  |  {} skipped{}",
                summary.entries, summary.directories, summary.skipped,
                if summary.truncated { "  |  entry limit reached" } else { "" },
            ));
            show_results(app);
        }
        Some(Err(error)) => status(app, &format!("Index failed: {error}")),
        None => status(app, "Indexing result unavailable"),
    }
}

fn show_results(app: &App) {
    unsafe { SendMessageW(app.results, LB_RESETCONTENT, 0, 0); }
    let Some(catalog) = &app.catalog else { return };
    let query = read_text(app.search);
    let results = catalog.search(&query, RESULTS_LIMIT);
    for path in &results {
        let text = wide(path);
        unsafe { SendMessageW(app.results, LB_ADDSTRING, 0, text.as_ptr() as isize); }
    }
    if !query.trim().is_empty() {
        status(app, &format!("Showing {} matches (maximum {})", results.len(), RESULTS_LIMIT));
    }
}

unsafe extern "system" fn window_proc(hwnd: Window, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        match msg {
            WM_SIZE => {
                let width = (lparam as u32 & 0xffff) as i32;
                let height = ((lparam as u32 >> 16) & 0xffff) as i32;
                layout(hwnd, width, height);
                0
            }
            WM_COMMAND => {
                if let Some(app) = state(hwnd) {
                    let id = wparam & 0xffff;
                    let event = wparam >> 16;
                    if id == INDEX_ID && event == BN_CLICKED {
                        start_scan(hwnd, app);
                    } else if id == SEARCH_ID && event == EN_CHANGE {
                        KillTimer(hwnd, SEARCH_TIMER);
                        SetTimer(hwnd, SEARCH_TIMER, 120, None);
                    }
                }
                0
            }
            WM_TIMER if wparam == SEARCH_TIMER => {
                KillTimer(hwnd, SEARCH_TIMER);
                if let Some(app) = state(hwnd) {
                    show_results(app);
                }
                0
            }
            SCAN_READY => {
                if let Some(app) = state(hwnd) {
                    scan_ready(app);
                }
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            WM_NCDESTROY => {
                KillTimer(hwnd, SEARCH_TIMER);
                let pointer = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut App;
                if !pointer.is_null() {
                    drop(Box::from_raw(pointer));
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

pub fn show_error(error: &str) {
    unsafe {
        MessageBoxW(ptr::null_mut(), wide(error).as_ptr(), wide("FSearch").as_ptr(), 0x10);
    }
}
