//! Log view — a fail-safe transcript window for diagnostics.
//!
//! Like `redit`, this is an MDI child driven entirely from the UI
//! thread. The point is durability: log lines emitted from CP code
//! land in a process-wide Rust ring buffer, so a language-thread
//! panic (or any iGui-internal fault that doesn't take down the
//! process) leaves the log intact. The user can still see what was
//! happening in the moments before the crash.
//!
//! Design notes:
//!   - Buffer is `Mutex<Vec<LogEntry>>`, capped at `LOG_CAPACITY`
//!     entries. Once full, the oldest entry is dropped on each new
//!     push.
//!   - Adjacent identical messages coalesce: a repeat of the most
//!     recent line bumps that entry's `count` instead of pushing a
//!     new one. This keeps the display readable when something
//!     starts spinning out the same panic message.
//!   - The view is a singleton MDI child, opened from the Tools
//!     menu or via Ctrl+Shift+L. It positions itself on the left of
//!     the MDI client area by default (thin tall column) so it sits
//!     out of the way of CP windows.
//!   - Newest-at-top: scrollOffset 0 = newest visible at the top of
//!     the content area. Wheel scrolls back through history.

#![cfg(windows)]

use std::sync::Mutex;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
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
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, IsWindow, LoadCursorW,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW,
    MDICREATESTRUCTW, WHEEL_DELTA, WM_DPICHANGED_AFTERPARENT, WM_LBUTTONDOWN, WM_MDIACTIVATE,
    WM_MDICREATE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETFOCUS, WM_SIZE,
    WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use super::renderer;

/// Maximum entries kept in the ring. Older entries fall off the back
/// when the buffer fills. 16K covers typical crash bursts with room
/// to spare; at ~256 bytes per entry that's about 4 MB worst case.
const LOG_CAPACITY: usize = 16384;

/// Maximum chars stored per entry. Anything past this is truncated;
/// keeps the ring's memory bounded even if a CP caller hands us a
/// pathologically long string.
const LOG_LINE_MAX: usize = 256;

/// WM_COMMAND id for the "Tools > Log" menu entry. One past redit's
/// 0x3000.
pub const MENU_CMD_ID: u16 = 0x3001;

const LOG_CLASS: PCWSTR = w!("NewCL.iGui.Log");
const LOG_TITLE: PCWSTR = w!("Log");

/// HWND of the singleton log MDI child, when one is open.
static LOG_HWND: Mutex<Option<isize>> = Mutex::new(None);

// ─── Process-wide log state ─────────────────────────────────────────

#[derive(Clone, Debug)]
struct LogEntry {
    text: String,
    /// 1 for a fresh entry. Bumped each time the same text repeats
    /// adjacent to this one.
    count: u32,
    /// Wall-clock seconds since program start, captured the first
    /// time this entry was written. Cheap, monotonic, and good
    /// enough for an in-app log.
    seconds: f64,
}

struct LogState {
    /// Newest entry is `entries[entries.len() - 1]`.
    entries: Vec<LogEntry>,
    /// Total number of `append()` calls ever, including the ones
    /// that coalesced into a previous entry. Useful for debugging
    /// "how chatty was the run" without scrolling.
    total_appends: u64,
    /// Number of `append()` calls that coalesced rather than
    /// produced a new entry. `total_appends - coalesced =
    /// entries.len()` after any drops are accounted for.
    coalesced: u64,
    started: Instant,
}

/// Process-wide singleton. The ring is shared across all threads —
/// any CP code (or Rust panic handler, etc) can call `append`.
/// `Option` because `Instant` has no const constructor; the inner
/// state is lazy-initialized on first `append`.
static LOG: Mutex<Option<LogState>> = Mutex::new(None);

fn with_log<R>(f: impl FnOnce(&mut LogState) -> R) -> R {
    let mut guard = LOG.lock().expect("LOG poisoned");
    let state = guard.get_or_insert_with(|| LogState {
        entries: Vec::new(),
        total_appends: 0,
        coalesced: 0,
        started: Instant::now(),
    });
    f(state)
}

/// Append one line to the ring. If the line is identical to the
/// most recent entry, that entry's count is incremented instead. UI
/// is invalidated lazily when the view next paints.
pub fn append(line: &str) {
    let trimmed: &str = if line.len() > LOG_LINE_MAX {
        // Slice on a char boundary so we don't split a multi-byte
        // codepoint mid-sequence.
        let mut end = LOG_LINE_MAX;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        &line[..end]
    } else {
        line
    };
    with_log(|state| {
        state.total_appends += 1;
        if let Some(last) = state.entries.last_mut() {
            if last.text == trimmed {
                last.count += 1;
                state.coalesced += 1;
                request_repaint();
                return;
            }
        }
        let now = state.started.elapsed().as_secs_f64();
        if state.entries.len() >= LOG_CAPACITY {
            // Drop oldest. Vec::remove(0) is O(n) but fine — capped
            // at 16K, and we only hit this path once the ring is
            // full.
            state.entries.remove(0);
        }
        state.entries.push(LogEntry {
            text: trimmed.to_string(),
            count: 1,
            seconds: now,
        });
    });
    request_repaint();
}

/// Wipe the ring. Bound to nothing for now; a future "Clear" menu
/// item will call this.
#[allow(dead_code)]
pub fn clear() {
    with_log(|state| {
        state.entries.clear();
        state.total_appends = 0;
        state.coalesced = 0;
    });
    request_repaint();
}

/// Read-only snapshot of the counters, for the status footer.
fn snapshot_counters() -> (usize, u64, u64) {
    with_log(|state| (state.entries.len(), state.total_appends, state.coalesced))
}

fn request_repaint() {
    // Only invalidate if the singleton view is actually open. Other
    // windows don't care that the buffer changed.
    let raw = match LOG_HWND.lock() {
        Ok(g) => *g,
        Err(_) => return,
    };
    if let Some(r) = raw {
        let hwnd = HWND(r as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
    }
}

// ─── Window class registration & menu plumbing ──────────────────────

pub fn register_class() -> Result<(), super::IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (log): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (log): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(log_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: LOG_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

/// Open the log view (or activate it if already open). UI thread.
pub fn open(_frame: HWND, mdi_client: HWND) {
    if let Some(raw) = *LOG_HWND.lock().expect("LOG_HWND poisoned") {
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
            eprintln!("[log] GetModuleHandleW: {e}");
            return;
        }
    };

    // Default position: thin tall column on the left. The user can
    // drag/resize after.
    let mut client_rect = RECT::default();
    let _ = unsafe { GetClientRect(mdi_client, &mut client_rect) };
    let height = (client_rect.bottom - client_rect.top).max(200);
    let width = 360i32;

    let create = MDICREATESTRUCTW {
        szClass: LOG_CLASS,
        szTitle: LOG_TITLE,
        hOwner: h_instance,
        x: 0,
        y: 0,
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
        eprintln!("[log] WM_MDICREATE returned 0");
        let _ = CW_USEDEFAULT;
    }
}

// ─── Per-window state ──────────────────────────────────────────────

struct LogWindowState {
    hwnd: HWND,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    cell_w: f32,
    cell_h: f32,
    /// Number of entries scrolled past the top. 0 = newest visible.
    scroll_offset: usize,
    client_w: u32,
    client_h: u32,
    dpi: u32,
}

impl LogWindowState {
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

    fn dip_scale(&self) -> f32 {
        if self.dpi == 0 {
            1.0
        } else {
            96.0 / (self.dpi as f32)
        }
    }

    fn invalidate(&self) {
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn set_dpi(&mut self, dpi: u32) {
        if dpi == 0 || dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        self.target = None;
        if let Some(fmt) = self.text_format.as_ref() {
            if let Some((cw, ch)) = measure_cell(fmt) {
                self.cell_w = cw;
                self.cell_h = ch;
            }
        }
        self.invalidate();
    }

    fn ensure_resources(&mut self, w: u32, h: u32) {
        if self.text_format.is_none() {
            self.text_format = create_text_format();
            if let Some(fmt) = self.text_format.as_ref() {
                if let Some((cw, ch)) = measure_cell(fmt) {
                    self.cell_w = cw;
                    self.cell_h = ch;
                }
            }
        }
        if let Some(target) = self.target.as_ref() {
            let cur = unsafe { target.GetPixelSize() };
            if cur.width != w || cur.height != h {
                let _ = unsafe { target.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            return;
        }
        let dpi = self.dpi as f32;
        let factory = &renderer::ctx().d2d.factory;
        let target = unsafe {
            factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: dpi,
                    dpiY: dpi,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.hwnd,
                    pixelSize: D2D_SIZE_U { width: w, height: h },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )
        };
        match target {
            Ok(t) => self.target = Some(t),
            Err(e) => eprintln!("[log] CreateHwndRenderTarget failed: {e}"),
        }
    }

    fn paint(&mut self) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(self.hwnd, &mut rect) }.is_err() {
            return;
        }
        let w = (rect.right - rect.left) as u32;
        let h = (rect.bottom - rect.top) as u32;
        if w == 0 || h == 0 {
            return;
        }
        self.client_w = w;
        self.client_h = h;
        self.ensure_resources(w, h);

        let target = match self.target.clone() {
            Some(t) => t,
            None => return,
        };
        let format = match self.text_format.clone() {
            Some(f) => f,
            None => return,
        };

        let scale = self.dip_scale();
        let w_dip = (w as f32) * scale;
        let h_dip = (h as f32) * scale;

        unsafe { target.BeginDraw() };
        unsafe {
            target.Clear(Some(&D2D1_COLOR_F {
                r: 0.08,
                g: 0.09,
                b: 0.10,
                a: 1.0,
            }))
        };

        let fg = solid_brush(&target, 0.85, 0.88, 0.92, 1.0);
        let dim = solid_brush(&target, 0.45, 0.50, 0.55, 1.0);
        let count_brush = solid_brush(&target, 0.95, 0.78, 0.32, 1.0);
        let footer_bg = solid_brush(&target, 0.13, 0.15, 0.18, 1.0);
        let footer_fg = solid_brush(&target, 0.78, 0.82, 0.88, 1.0);

        let footer_h = self.cell_h + 2.0;
        let content_bottom = h_dip - footer_h;

        let visible_rows = (content_bottom / self.cell_h).floor() as usize;
        let pad_x: f32 = 4.0;

        // Snapshot the entries we need under the lock, then release
        // before doing any drawing — Direct2D calls can be slow and
        // we don't want to hold the log mutex during them.
        let (visible, total_entries, total_appends, coalesced) = with_log(|state| {
            let n = state.entries.len();
            let scroll = self.scroll_offset.min(n.saturating_sub(1));
            let start = n.saturating_sub(scroll + visible_rows);
            let end = n.saturating_sub(scroll);
            let slice = state.entries[start..end].to_vec();
            (slice, n, state.total_appends, state.coalesced)
        });

        // Newest first (top of viewport is the most recent within the
        // visible window). Snapshot is in oldest-first order, so
        // iterate in reverse.
        let mut y = 0.0f32;
        for entry in visible.iter().rev() {
            if y >= content_bottom {
                break;
            }
            let count_text = if entry.count > 1 {
                format!("(×{}) ", entry.count)
            } else {
                String::new()
            };
            // Draw the count badge first (orange), then the message.
            let mut x = pad_x;
            if !count_text.is_empty() {
                if let (Some(brush), Ok(layout)) = (
                    count_brush.as_ref(),
                    build_layout(&format, &count_text, w_dip - x, self.cell_h),
                ) {
                    unsafe {
                        target.DrawTextLayout(
                            windows_numerics::Vector2 { X: x, Y: y },
                            &layout,
                            brush,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        );
                    }
                }
                x += (count_text.chars().count() as f32) * self.cell_w;
            }
            let body_brush = if entry.count > 1 {
                fg.as_ref()
            } else {
                fg.as_ref()
            };
            if let (Some(brush), Ok(layout)) = (
                body_brush,
                build_layout(&format, &entry.text, w_dip - x, self.cell_h),
            ) {
                unsafe {
                    target.DrawTextLayout(
                        windows_numerics::Vector2 { X: x, Y: y },
                        &layout,
                        brush,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    );
                }
            }
            y += self.cell_h;
        }
        let _ = dim; // reserved for future timestamps

        // Footer.
        if let Some(brush) = footer_bg.as_ref() {
            unsafe {
                target.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: content_bottom,
                        right: w_dip,
                        bottom: h_dip,
                    },
                    brush,
                )
            };
        }
        let footer = format!(
            " {n} entries  {appends} appends  {co} coalesced",
            n = total_entries,
            appends = total_appends,
            co = coalesced,
        );
        if let (Some(brush), Ok(layout)) = (
            footer_fg.as_ref(),
            build_layout(&format, &footer, w_dip, footer_h),
        ) {
            unsafe {
                target.DrawTextLayout(
                    windows_numerics::Vector2 {
                        X: 0.0,
                        Y: content_bottom + 1.0,
                    },
                    &layout,
                    brush,
                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                );
            }
        }

        let _ = unsafe { target.EndDraw(None, None) };
    }

    fn wheel(&mut self, raw_delta: i32) {
        let lines = if WHEEL_DELTA != 0 {
            (raw_delta / (WHEEL_DELTA as i32)) * 3
        } else {
            0
        };
        if lines == 0 {
            return;
        }
        // Wheel up scrolls *back* into history (increases offset);
        // wheel down moves toward newest (decreases offset).
        let new_offset = (self.scroll_offset as i32) + lines;
        let max_off = with_log(|s| s.entries.len()).saturating_sub(1) as i32;
        let clamped = new_offset.clamp(0, max_off.max(0));
        self.scroll_offset = clamped as usize;
        self.invalidate();
    }
}

// ─── Win32 plumbing ─────────────────────────────────────────────────

unsafe extern "system" fn log_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let state = Box::new(LogWindowState::new(hwnd));
        let raw = Box::into_raw(state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        if let Ok(mut slot) = LOG_HWND.lock() {
            *slot = Some(hwnd.0 as isize);
        }
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut LogWindowState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_PAINT => {
            let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let _ = unsafe { windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut ps) };
            state.paint();
            let _ = unsafe { windows::Win32::Graphics::Gdi::EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as u32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
            state.client_w = w;
            state.client_h = h;
            if let Some(target) = state.target.as_ref() {
                let _ = unsafe { target.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(hwnd)) };
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let raw = ((wparam.0 >> 16) & 0xFFFF) as i16;
            state.wheel(raw as i32);
            LRESULT(0)
        }
        WM_SETFOCUS | WM_MDIACTIVATE => {
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DPICHANGED_AFTERPARENT => {
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            if dpi != 0 {
                state.set_dpi(dpi);
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_NCDESTROY => {
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            if let Ok(mut slot) = LOG_HWND.lock() {
                if matches!(*slot, Some(h) if h == hwnd.0 as isize) {
                    *slot = None;
                }
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Local copies of redit's render helpers ────────────────────────
//
// Deliberately duplicated here rather than factored into a shared
// module: the surface area is small (text format + cell measure +
// brush + layout build), and an upcoming refactor can pull them out
// once a third tool window appears that wants the same shapes.

fn create_text_format() -> Option<IDWriteTextFormat> {
    let factory = &renderer::ctx().dwrite.factory;
    for family in ["Cascadia Mono", "Consolas", "Lucida Console", "Courier New"] {
        let family_w: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
        let locale_w: Vec<u16> = "en-us".encode_utf16().chain(std::iter::once(0)).collect();
        let result = unsafe {
            factory.CreateTextFormat(
                PCWSTR(family_w.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT(400),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                12.5,
                PCWSTR(locale_w.as_ptr()),
            )
        };
        if let Ok(f) = result {
            return Some(f);
        }
    }
    None
}

fn measure_cell(format: &IDWriteTextFormat) -> Option<(f32, f32)> {
    let factory = &renderer::ctx().dwrite.factory;
    let text: Vec<u16> = "M".encode_utf16().collect();
    let layout =
        unsafe { factory.CreateTextLayout(&text, format, 1024.0, 1024.0) }.ok()?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
        return None;
    }
    Some((metrics.widthIncludingTrailingWhitespace, metrics.height))
}

fn build_layout(
    format: &IDWriteTextFormat,
    text: &str,
    max_w: f32,
    max_h: f32,
) -> Result<IDWriteTextLayout, windows::core::Error> {
    let factory = &renderer::ctx().dwrite.factory;
    let text_w: Vec<u16> = text.encode_utf16().collect();
    let layout =
        unsafe { factory.CreateTextLayout(&text_w, format, max_w.max(1.0), max_h.max(1.0)) }?;
    unsafe { layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP) }?;
    Ok(layout)
}

fn solid_brush(
    target: &ID2D1HwndRenderTarget,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> Option<ID2D1SolidColorBrush> {
    let color = D2D1_COLOR_F { r, g, b, a };
    let props = D2D1_BRUSH_PROPERTIES {
        opacity: 1.0,
        transform: windows_numerics::Matrix3x2 {
            M11: 1.0,
            M12: 0.0,
            M21: 0.0,
            M22: 1.0,
            M31: 0.0,
            M32: 0.0,
        },
    };
    unsafe { target.CreateSolidColorBrush(&color, Some(&props)) }.ok()
}
