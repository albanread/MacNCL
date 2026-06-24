//! crash_view — read-only MDI child for displaying a captured
//! crash dump (Rust panic or SEH exception from the Lisp worker).
//!
//! Process-wide list of pending dumps; the GUI thread opens the
//! window on demand (Tools → Crash dump, Ctrl+Shift+X, or
//! automatically when a new dump arrives).  Text is pre-formatted
//! by the caller and displayed verbatim.
//!
//! Ported from WF64's crash_view.rs.  Class name, title, and colour
//! scheme adapted for NewCormanLisp.

#![cfg(windows)]

use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, ID2D1SolidColorBrush, D2D1_BRUSH_PROPERTIES,
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, IDWriteTextLayout, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, IsWindow, LoadCursorW,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, CW_USEDEFAULT,
    GWLP_USERDATA, IDC_ARROW, MDICREATESTRUCTW, WHEEL_DELTA, WM_DPICHANGED_AFTERPARENT,
    WM_KEYDOWN, WM_MDIACTIVATE, WM_MDICREATE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_SIZE, WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use super::renderer;

/// WM_COMMAND id for opening this pane from the Tools menu / accelerator.
pub const MENU_CMD_ID: u16 = 0x3003;

const CLASS_NAME: PCWSTR = w!("NCL.iGui.CrashView");
/// Title bar: λ (U+03BB) matches the NCL lambda-theme background.
const TITLE: PCWSTR = w!("\u{03BB} crash dump");
const CAP: usize = 4096;

static CRASH_HWND: Mutex<Option<isize>> = Mutex::new(None);
static DUMPS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Push one dump section.  Each call is one logical crash report;
/// multiple calls concatenate with separator lines.  Safe from any
/// thread.  Posts WM_IGUI_CRASH_FLUSH so the GUI thread opens the
/// window if it is not already open.
pub fn push(text: impl Into<String>) {
    let txt = text.into();
    {
        let mut d = DUMPS.lock().expect("DUMPS poisoned");
        if d.len() >= CAP {
            d.remove(0);
        }
        d.push(txt);
    }
    super::window::post_crash_flush();
}

/// Snapshot all stored dumps (newest last).  Called during paint.
pub fn snapshot() -> Vec<String> {
    DUMPS.lock().map(|d| d.clone()).unwrap_or_default()
}

/// Drain the pending dumps (called when user presses Ctrl+L).
#[allow(dead_code)]
pub fn clear() {
    if let Ok(mut d) = DUMPS.lock() {
        d.clear();
    }
    request_repaint();
}

fn request_repaint() {
    if let Some(raw) = *CRASH_HWND.lock().expect("CRASH_HWND poisoned") {
        let hwnd = HWND(raw as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
    }
}

pub fn register_class() -> Result<(), super::IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (crash): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (crash): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(crash_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: CLASS_NAME,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

pub fn open(_frame: HWND, mdi_client: HWND) {
    if let Some(raw) = *CRASH_HWND.lock().expect("CRASH_HWND poisoned") {
        let hwnd = HWND(raw as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                SendMessageW(
                    mdi_client,
                    WM_MDIACTIVATE,
                    Some(WPARAM(hwnd.0 as usize)),
                    Some(LPARAM(0)),
                )
            };
            let _ = unsafe { BringWindowToTop(hwnd) };
            return;
        }
    }
    let h_instance = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => windows::Win32::Foundation::HANDLE(h.0),
        Err(e) => {
            eprintln!("[crash-view] GetModuleHandleW: {e}");
            return;
        }
    };
    let mut client_rect = RECT::default();
    let _ = unsafe { GetClientRect(mdi_client, &mut client_rect) };
    let w_full = (client_rect.right - client_rect.left).max(400);
    let h_full = (client_rect.bottom - client_rect.top).max(200);
    let width = (w_full * 7 / 10).max(480);
    let height = (h_full * 7 / 10).max(320);
    let x = (w_full - width) / 2;
    let y = (h_full - height) / 2;
    let create = MDICREATESTRUCTW {
        szClass: CLASS_NAME,
        szTitle: TITLE,
        hOwner: h_instance,
        x,
        y,
        cx: width,
        cy: height,
        style: WS_VISIBLE | WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(0),
    };
    let result = unsafe {
        SendMessageW(
            mdi_client,
            WM_MDICREATE,
            Some(WPARAM(0)),
            Some(LPARAM(&create as *const _ as isize)),
        )
    };
    if result.0 == 0 {
        eprintln!("[crash-view] WM_MDICREATE returned 0");
        let _ = CW_USEDEFAULT;
    }
}

/// Called from the frame WndProc on WM_IGUI_CRASH_FLUSH.  Opens the
/// window if not already open, then invalidates so the dump appears.
pub(super) fn flush_on_gui_thread(frame: HWND) {
    if CRASH_HWND.lock().ok().and_then(|g| *g).is_none() {
        if let Some(mdi) = super::window::mdi_client_hwnd() {
            open(frame, mdi);
        }
    }
    request_repaint();
}

// ─── Per-window state ────────────────────────────────────────────────

struct CrashWindowState {
    hwnd: HWND,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    cell_w: f32,
    cell_h: f32,
    scroll_offset: usize,
    client_w: u32,
    client_h: u32,
    dpi: u32,
}

impl CrashWindowState {
    fn new(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            target: None,
            text_format: None,
            cell_w: 8.0,
            cell_h: 16.0,
            scroll_offset: 0,
            client_w: 0,
            client_h: 0,
            dpi,
        }
    }

    fn ensure_target(&mut self) {
        if self.target.is_some() {
            return;
        }
        let factory = &renderer::ctx().d2d.factory;
        let rect = unsafe {
            let mut r = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut r);
            r
        };
        let size = D2D_SIZE_U {
            width: (rect.right - rect.left).max(1) as u32,
            height: (rect.bottom - rect.top).max(1) as u32,
        };
        self.client_w = size.width;
        self.client_h = size.height;
        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: self.dpi as f32,
            dpiY: self.dpi as f32,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: self.hwnd,
            pixelSize: size,
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        match unsafe { factory.CreateHwndRenderTarget(&rt_props, &hwnd_props) } {
            Ok(t) => self.target = Some(t),
            Err(e) => eprintln!("[crash-view] CreateHwndRenderTarget: {e}"),
        }
    }

    fn ensure_text_format(&mut self) {
        if self.text_format.is_some() {
            return;
        }
        let dw_factory = &renderer::ctx().dwrite.factory;
        let scale = self.dpi as f32 / 96.0;
        let font_size = 13.0_f32 * scale;
        match unsafe {
            dw_factory.CreateTextFormat(
                w!("Cascadia Mono"),
                None,
                DWRITE_FONT_WEIGHT(400),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size,
                w!("en-us"),
            )
        } {
            Ok(fmt) => {
                let _ = unsafe { fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP) };
                if let Ok(layout) = unsafe {
                    dw_factory.CreateTextLayout(
                        &"M".encode_utf16().collect::<Vec<u16>>(),
                        &fmt,
                        4096.0,
                        4096.0,
                    )
                } {
                    let mut metrics = DWRITE_TEXT_METRICS::default();
                    let _ = unsafe { layout.GetMetrics(&mut metrics) };
                    if metrics.width > 0.0 {
                        self.cell_w = metrics.width;
                    }
                    if metrics.height > 0.0 {
                        self.cell_h = metrics.height;
                    }
                }
                self.text_format = Some(fmt);
            }
            Err(e) => eprintln!("[crash-view] CreateTextFormat: {e}"),
        }
    }

    fn paint(&mut self) {
        self.ensure_target();
        self.ensure_text_format();
        let Some(target) = self.target.clone() else { return; };
        let Some(format) = self.text_format.clone() else { return; };
        let dw_factory = &renderer::ctx().dwrite.factory;

        unsafe { target.BeginDraw() };
        unsafe {
            target.Clear(Some(&D2D1_COLOR_F {
                r: 0.10,
                g: 0.04,
                b: 0.04,
                a: 1.0,
            }));
        }

        let fg     = make_brush(&target, 1.00, 0.85, 0.75, 1.0); // warm cream
        let header = make_brush(&target, 1.00, 0.55, 0.40, 1.0); // brick red

        let pad_x = 10.0_f32;
        let pad_y = 8.0_f32;
        let w = self.client_w as f32;
        let h = self.client_h as f32;
        let cell_h = self.cell_h;
        let cols_per_row = ((w - pad_x * 2.0) / self.cell_w).floor().max(1.0) as usize;
        let visible_rows = ((h - pad_y * 2.0) / cell_h).floor().max(1.0) as usize;

        let dumps = snapshot();
        let mut rows: Vec<(bool, String)> = Vec::with_capacity(64);
        for (i, dump) in dumps.iter().enumerate() {
            let header_text = format!("─── crash #{} ────────────────────────", i + 1);
            rows.push((true, header_text));
            for line in dump.lines() {
                wrap_into(&mut rows, line, cols_per_row);
            }
            rows.push((false, String::new())); // separator
        }
        if dumps.is_empty() {
            rows.push((false, "(no crashes captured)".to_string()));
        }

        let stream_len = rows.len();
        let bottom_idx = stream_len.saturating_sub(self.scroll_offset);
        let top_idx = bottom_idx.saturating_sub(visible_rows);
        for (row_screen, idx) in (top_idx..bottom_idx).enumerate() {
            let y = pad_y + row_screen as f32 * cell_h;
            let (is_header, ref text) = rows[idx];
            if text.is_empty() {
                continue;
            }
            let brush = if is_header { header.as_ref() } else { fg.as_ref() };
            draw_text(
                dw_factory,
                &target,
                &format,
                text,
                pad_x,
                y,
                w - pad_x * 2.0,
                cell_h,
                brush,
            );
        }

        let _ = unsafe { target.EndDraw(None, None) };
    }

    fn handle_wheel(&mut self, delta: i16) {
        let steps = (delta as i32 / WHEEL_DELTA as i32).abs().max(1) as usize;
        if delta > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(steps * 3);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(steps * 3);
        }
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn handle_resize(&mut self) {
        let mut r = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut r) };
        let new_w = (r.right - r.left).max(1) as u32;
        let new_h = (r.bottom - r.top).max(1) as u32;
        if new_w == self.client_w && new_h == self.client_h {
            return;
        }
        self.client_w = new_w;
        self.client_h = new_h;
        if let Some(t) = self.target.as_ref() {
            let _ = unsafe {
                t.Resize(&D2D_SIZE_U { width: new_w, height: new_h })
            };
        }
    }
}

fn wrap_into(rows: &mut Vec<(bool, String)>, line: &str, cols: usize) {
    if line.is_empty() {
        rows.push((false, String::new()));
        return;
    }
    let mut start = 0usize;
    let mut col = 0usize;
    for (i, _ch) in line.char_indices() {
        if col == cols {
            rows.push((false, line[start..i].to_string()));
            start = i;
            col = 0;
        }
        col += 1;
    }
    if start < line.len() {
        rows.push((false, line[start..].to_string()));
    }
}

fn make_brush(
    target: &ID2D1HwndRenderTarget,
    r: f32, g: f32, b: f32, a: f32,
) -> Option<ID2D1SolidColorBrush> {
    let color = D2D1_COLOR_F { r, g, b, a };
    let props = D2D1_BRUSH_PROPERTIES {
        opacity: 1.0,
        transform: windows_numerics::Matrix3x2::identity(),
    };
    unsafe { target.CreateSolidColorBrush(&color, Some(&props)) }.ok()
}

fn draw_text(
    dw_factory: &windows::Win32::Graphics::DirectWrite::IDWriteFactory,
    target: &ID2D1HwndRenderTarget,
    format: &IDWriteTextFormat,
    text: &str, x: f32, y: f32, w: f32, h: f32,
    brush: Option<&ID2D1SolidColorBrush>,
) {
    if text.is_empty() { return; }
    let Some(brush) = brush else { return; };
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout: Result<IDWriteTextLayout, _> = unsafe {
        dw_factory.CreateTextLayout(&wide, format, w.max(1.0), h.max(1.0))
    };
    let Ok(layout) = layout else { return; };
    let origin = windows_numerics::Vector2 { X: x, Y: y };
    unsafe { target.DrawTextLayout(origin, &layout, brush, D2D1_DRAW_TEXT_OPTIONS_CLIP) };
}

unsafe extern "system" fn crash_wnd_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let state = Box::new(CrashWindowState::new(hwnd));
        let raw = Box::into_raw(state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        *CRASH_HWND.lock().expect("CRASH_HWND poisoned") = Some(hwnd.0 as isize);
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut CrashWindowState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };
    match msg {
        WM_NCDESTROY => {
            *CRASH_HWND.lock().expect("CRASH_HWND poisoned") = None;
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = Box::from_raw(state_ptr);
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_PAINT => {
            state.paint();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_SIZE => {
            state.handle_resize();
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as i16) as i16;
            state.handle_wheel(delta);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL};
            let vk = wparam.0 as u32;
            let ctrl = (unsafe { GetKeyState(VK_CONTROL.0 as i32) } as i16) < 0;
            if ctrl && vk == 'L' as u32 {
                clear();
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DPICHANGED_AFTERPARENT => {
            state.dpi = unsafe { GetDpiForWindow(hwnd) };
            if state.dpi == 0 { state.dpi = 96; }
            state.text_format = None;
            state.target = None;
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

