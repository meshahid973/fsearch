//! Native Windows search window, built with standard Windows controls.
//! A worker loads or rebuilds the index while the UI stays responsive.

mod win32;

use crate::{Catalog, DEFAULT_ENTRY_LIMIT, default_roots};
use std::mem;
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};
use win32::*;

const SEARCH_ID: usize = 101;
const ROOTS_ID: usize = 102;
const REFRESH_ID: usize = 103;
const RESULTS_ID: usize = 104;
const SEARCH_TIMER: usize = 1;
const SCAN_READY: u32 = WM_APP + 1;
const RESULTS_LIMIT: usize = 200;

type ScanResult = Result<(Catalog, bool), String>;
type ResultSlot = Arc<Mutex<Option<ScanResult>>>;

struct App {
    search: Window,
    roots: Window,
    refresh: Window,
    results: Window,
    status: Window,
    catalog: Option<Catalog>,
    matches: Vec<String>,
    result_slot: ResultSlot,
    scanning: bool,
    summary: String,
}

pub fn run() -> Result<(), String> {
    unsafe {
        if InitCommonControlsEx(&InitControls { size: mem::size_of::<InitControls>() as u32, classes: 1 }) == 0 {
            return Err("could not initialize Windows controls".into());
        }
        let instance = GetModuleHandleW(ptr::null());
        if instance.is_null() {
            return Err("could not get Windows module handle".into());
        }
        let class_name = wide("FSearchNativeWindow");
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
            class_name: class_name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            return Err("could not register window".into());
        }
        let title = wide("FSearch");
        let hwnd = CreateWindowExW(
            0, class_name.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, 950, 620,
            ptr::null_mut(), ptr::null_mut(), instance, ptr::null(),
        );
        if hwnd.is_null() {
            return Err("could not create window".into());
        }

        let search = control(hwnd, instance, "EDIT", "", SEARCH_ID,
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL);
        let default = default_roots().iter().map(|r| r.to_string_lossy().into_owned())
            .collect::<Vec<_>>().join(";");
        let roots = control(hwnd, instance, "EDIT", &default, ROOTS_ID,
            WS_CHILD | WS_VISIBLE | WS_BORDER | ES_AUTOHSCROLL);
        let refresh = control(hwnd, instance, "BUTTON", "Refresh", REFRESH_ID,
            WS_CHILD | WS_VISIBLE);
        let results = control(hwnd, instance, "SysListView32", "", RESULTS_ID,
            WS_CHILD | WS_VISIBLE | WS_BORDER | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS);
        let status = control(hwnd, instance, "STATIC", "Loading index...", 0, WS_CHILD | WS_VISIBLE);
        if [search, roots, refresh, results, status].iter().any(|h| h.is_null()) {
            return Err("could not create search controls".into());
        }

        let font = GetStockObject(17); // DEFAULT_GUI_FONT
        for control in [search, roots, refresh, results, status] {
            SendMessageW(control, WM_SETFONT, font as usize, 1);
        }
        SendMessageW(results, LVM_SETEXTENDEDLISTVIEWSTYLE, 0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize);
        add_column(results, 0, "Name", 310);
        add_column(results, 1, "Location", 580);
        let cue = wide("Search files and folders");
        SendMessageW(search, 0x1501, 1, cue.as_ptr() as isize); // EM_SETCUEBANNER

        let app = Box::new(App {
            search, roots, refresh, results, status,
            catalog: None,
            matches: Vec::new(),
            result_slot: Arc::new(Mutex::new(None)),
            scanning: false,
            summary: String::new(),
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize);
        let mut rect: Rect = mem::zeroed();
        GetClientRect(hwnd, &mut rect);
        layout(hwnd, rect.right - rect.left, rect.bottom - rect.top);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        SetFocus(search);
        if let Some(app) = state(hwnd) {
            start_scan(hwnd, app, false);
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
    value.encode_utf16().chain(Some(0)).collect()
}

unsafe fn state(hwnd: Window) -> Option<&'static mut App> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    unsafe { pointer.as_mut() }
}

fn add_column(hwnd: Window, position: usize, label: &str, width: i32) {
    let mut text = wide(label);
    let mut column: Column = unsafe { mem::zeroed() };
    column.mask = LVCF_FMT | LVCF_WIDTH | LVCF_TEXT;
    column.width = width;
    column.text = text.as_mut_ptr();
    unsafe { SendMessageW(hwnd, LVM_INSERTCOLUMNW, position, &column as *const Column as isize); }
}

unsafe fn layout(hwnd: Window, width: i32, height: i32) {
    let Some(app) = (unsafe { state(hwnd) }) else { return };
    let w = width.max(420);
    let h = height.max(270);
    unsafe {
        MoveWindow(app.search, 14, 14, w - 28, 36, 1);
        MoveWindow(app.roots, 14, 61, w - 120, 29, 1);
        MoveWindow(app.refresh, w - 96, 61, 82, 29, 1);
        MoveWindow(app.results, 14, 105, w - 28, h - 149, 1);
        MoveWindow(app.status, 16, h - 30, w - 32, 20, 1);
        SendMessageW(app.results, LVM_SETCOLUMNWIDTH, 0, (w / 3).max(180) as isize);
        SendMessageW(app.results, LVM_SETCOLUMNWIDTH, 1, (w - (w / 3).max(180) - 52).max(180) as isize);
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

fn start_scan(hwnd: Window, app: &mut App, refresh: bool) {
    if app.scanning {
        return;
    }
    let roots = read_text(app.roots)
        .split(';')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if roots.is_empty() {
        status(app, "Enter one or more folders, separated by semicolons");
        return;
    }
    app.catalog = None;
    app.matches.clear();
    app.scanning = true;
    unsafe {
        EnableWindow(app.refresh, 0);
        SendMessageW(app.results, LVM_DELETEALLITEMS, 0, 0);
    }
    status(app, if refresh { "Updating index..." } else { "Opening index..." });

    let shared = Arc::clone(&app.result_slot);
    let window_id = hwnd as usize;
    std::thread::spawn(move || {
        let result = Catalog::open_or_build(&roots, DEFAULT_ENTRY_LIMIT, refresh)
            .map_err(|e| e.to_string());
        if let Ok(mut slot) = shared.lock() {
            *slot = Some(result);
        }
        unsafe { PostMessageW(window_id as Window, SCAN_READY, 0, 0); }
    });
}

fn scan_ready(app: &mut App) {
    let result = app.result_slot.lock().ok().and_then(|mut slot| slot.take());
    app.scanning = false;
    unsafe { EnableWindow(app.refresh, 1); }
    match result {
        Some(Ok((catalog, cached))) => {
            let s = catalog.summary();
            app.summary = format!(
                "{}  |  {} items  |  {} folders  |  {} skipped{}",
                if cached { "Saved index" } else { "Indexed" },
                s.entries, s.directories, s.skipped,
                if s.truncated { "  |  Limit reached" } else { "" },
            );
            app.catalog = Some(catalog);
            show_results(app);
        }
        Some(Err(error)) => status(app, &format!("Index error: {error}")),
        None => status(app, "Index unavailable"),
    }
}

fn show_results(app: &mut App) {
    unsafe { SendMessageW(app.results, LVM_DELETEALLITEMS, 0, 0); }
    app.matches.clear();
    let Some(catalog) = &app.catalog else { return };
    let query = read_text(app.search);
    if query.trim().is_empty() {
        status(app, &app.summary);
        return;
    }
    let found = catalog.search(&query, RESULTS_LIMIT);
    app.matches = found.into_iter().map(str::to_string).collect();
    for (row, path) in app.matches.iter().enumerate() {
        let split = path.rfind(['\\', '/']).unwrap_or(0);
        let (folder, name) = if split == 0 { ("", path.as_str()) } else { (&path[..split], &path[split + 1..]) };
        insert_result(app.results, row as i32, name, folder);
    }
    status(app, &format!("{} results  |  {}", app.matches.len(), app.summary));
}

fn insert_result(hwnd: Window, row: i32, name: &str, folder: &str) {
    let mut title = wide(name);
    let mut item: Item = unsafe { mem::zeroed() };
    item.mask = LVIF_TEXT;
    item.row = row;
    item.text = title.as_mut_ptr();
    unsafe { SendMessageW(hwnd, LVM_INSERTITEMW, 0, &item as *const Item as isize); }

    let mut location = wide(folder);
    item.column = 1;
    item.text = location.as_mut_ptr();
    unsafe { SendMessageW(hwnd, LVM_SETITEMTEXTW, row as usize, &item as *const Item as isize); }
}

fn reveal_selected(app: &App) {
    let index = unsafe { SendMessageW(app.results, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED) };
    if index < 0 {
        return;
    }
    let Some(path) = app.matches.get(index as usize) else { return };
    let Some(windows_dir) = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")) else {
        status(app, "Windows directory is unavailable");
        return;
    };
    if let Err(error) = std::process::Command::new(PathBuf::from(windows_dir).join("explorer.exe"))
        .arg("/select,").arg(path).spawn()
    {
        status(app, &format!("Explorer error: {error}"));
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
                    if id == REFRESH_ID && event == BN_CLICKED {
                        start_scan(hwnd, app, true);
                    } else if id == SEARCH_ID && event == EN_CHANGE {
                        KillTimer(hwnd, SEARCH_TIMER);
                        SetTimer(hwnd, SEARCH_TIMER, 90, None);
                    }
                }
                0
            }
            WM_NOTIFY => {
                if ! (lparam as *const Notification).is_null() {
                    let notification = &*(lparam as *const Notification);
                    if notification.id == RESULTS_ID && notification.code == NM_DBLCLK {
                        if let Some(app) = state(hwnd) {
                            reveal_selected(app);
                        }
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
    unsafe { MessageBoxW(ptr::null_mut(), wide(error).as_ptr(), wide("FSearch").as_ptr(), 0x10); }
}
