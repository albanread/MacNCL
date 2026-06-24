//! Per-child cursor state and DPI tracking.
//!
//! The language thread sets a child's cursor via `iGui.SetCursor` and
//! reads the child's effective DPI via `iGui.GetDpi`. Both pieces of
//! state are owned here, in process-wide tables keyed by `child_id`.
//! The render-host WndProc reads the cursor on `WM_SETCURSOR` and
//! refreshes the DPI cache on `WM_DPICHANGED`.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    LoadCursorW, SetCursor, HCURSOR, IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP,
    IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT,
};

use super::channels::{self, IGuiEvent};

/// Cursor kind constants exposed to CP via `iGui.Cr*`. Stable
/// `i32` values — append-only.
pub mod kind {
    pub const ARROW: i32 = 0;
    pub const IBEAM: i32 = 1;
    pub const CROSSHAIR: i32 = 2;
    pub const HAND: i32 = 3;
    pub const WAIT: i32 = 4;
    pub const RESIZE_NS: i32 = 5;
    pub const RESIZE_EW: i32 = 6;
    pub const RESIZE_NESW: i32 = 7;
    pub const RESIZE_NWSE: i32 = 8;
    pub const SIZE_ALL: i32 = 9;
    pub const NOT_ALLOWED: i32 = 10;
    pub const HELP: i32 = 11;
    pub const APP_STARTING: i32 = 12;
}

fn cursor_id_for(k: i32) -> PCWSTR {
    match k {
        x if x == kind::IBEAM => IDC_IBEAM,
        x if x == kind::CROSSHAIR => IDC_CROSS,
        x if x == kind::HAND => IDC_HAND,
        x if x == kind::WAIT => IDC_WAIT,
        x if x == kind::RESIZE_NS => IDC_SIZENS,
        x if x == kind::RESIZE_EW => IDC_SIZEWE,
        x if x == kind::RESIZE_NESW => IDC_SIZENESW,
        x if x == kind::RESIZE_NWSE => IDC_SIZENWSE,
        x if x == kind::SIZE_ALL => IDC_SIZEALL,
        x if x == kind::NOT_ALLOWED => IDC_NO,
        x if x == kind::HELP => IDC_HELP,
        x if x == kind::APP_STARTING => IDC_APPSTARTING,
        _ => IDC_ARROW,
    }
}

// ─── Per-child cursor table ─────────────────────────────────────────

static CURSOR_KIND: Mutex<Option<HashMap<i64, i32>>> = Mutex::new(None);

pub fn set_kind(child_id: i64, k: i32) {
    let mut guard = CURSOR_KIND.lock().expect("CURSOR_KIND poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(child_id, k);
}

pub fn get_kind(child_id: i64) -> i32 {
    let guard = CURSOR_KIND.lock().expect("CURSOR_KIND poisoned");
    guard
        .as_ref()
        .and_then(|m| m.get(&child_id).copied())
        .unwrap_or(kind::ARROW)
}

#[allow(dead_code)] // called when a child closes
pub fn forget_cursor(child_id: i64) {
    let mut guard = CURSOR_KIND.lock().expect("CURSOR_KIND poisoned");
    if let Some(m) = guard.as_mut() {
        m.remove(&child_id);
    }
}

/// Apply the cursor kind currently registered for `child_id`. Called
/// from the render-host WndProc on `WM_SETCURSOR`.
pub fn apply(child_id: i64) {
    let k = get_kind(child_id);
    let id = cursor_id_for(k);
    if let Ok(hcur) = unsafe { LoadCursorW(None, id) } {
        let _: HCURSOR = unsafe { SetCursor(Some(hcur)) };
    }
}

// ─── Per-child DPI table ────────────────────────────────────────────

static DPI: Mutex<Option<HashMap<i64, (u32, u32)>>> = Mutex::new(None);

pub fn set_dpi(child_id: i64, dpi_x: u32, dpi_y: u32) {
    let mut guard = DPI.lock().expect("DPI poisoned");
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(child_id, (dpi_x, dpi_y));
}

pub fn get_dpi(child_id: i64) -> Option<(u32, u32)> {
    let guard = DPI.lock().expect("DPI poisoned");
    guard.as_ref().and_then(|m| m.get(&child_id).copied())
}

#[allow(dead_code)]
pub fn forget_dpi(child_id: i64) {
    let mut guard = DPI.lock().expect("DPI poisoned");
    if let Some(m) = guard.as_mut() {
        m.remove(&child_id);
    }
}

/// Sample the DPI from a window handle and store it for `child_id`.
/// Called on render-host WM_NCCREATE, WM_DPICHANGED, and
/// WM_DPICHANGED_AFTERPARENT. Pushes a `dpi-change` event onto the
/// mailbox so the language thread can react.
pub fn refresh_for(child_id: i64, hwnd: HWND) {
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    if dpi == 0 {
        return;
    }
    set_dpi(child_id, dpi, dpi);
    channels::push(IGuiEvent::DpiChange {
        child_id,
        dpi_x: dpi as i64 * 100,
        dpi_y: dpi as i64 * 100,
    });
}
