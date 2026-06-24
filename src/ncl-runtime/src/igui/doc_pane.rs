//! doc_pane — a generic, Lisp-writable Markdown pane.
//!
//! A `text_view`-shaped pane that renders Markdown (and embedded
//! Mermaid diagrams) instead of a character grid. The pane owns a
//! per-pane `PaneState` holding the current Markdown `source` and a
//! `gen` counter that bumps on every edit. The language thread calls
//! `set_markdown` / `append_markdown` (never touches an HWND), then
//! asks the GUI thread to repaint via
//! `window::post_doc_flush(child_id)` → `WM_IGUI_DOC_FLUSH`. The frame
//! WndProc routes that to `flush_on_gui_thread`, which just
//! `InvalidateRect`s the child. `WM_PAINT` re-parses + re-lays out
//! from the source whenever the `gen` it last drew is stale.
//!
//! Rendering goes through the shared `docpane` crate (forked from
//! NewFactor's `crates/docpane`), so the pane's render target comes
//! from *docpane's* D2D factory (`render::factory()`) — Mermaid
//! geometry and the target then share one factory.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, LoadCursorW, RegisterClassExW, SendMessageW,
    SetWindowLongPtrW, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MDICREATESTRUCTW,
    WM_LBUTTONDOWN, WM_MDICREATE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SIZE,
    WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use docpane::layout::Layout;
use docpane::{layout as dlayout, parser, render, theme};

use super::{registry, window};

const DOC_CLASS: PCWSTR = w!("NclDocPane");
const WHEEL_STEP: f32 = 48.0;

// ─── Per-pane shared state (queue-of-one: the latest source) ─────────

/// The Markdown a pane should render. Mutated from the language thread
/// (set/append), snapshotted on the GUI thread at paint time. `gen`
/// rises on every edit so the window knows when its cached layout is
/// stale without diffing the (potentially large) source string.
struct PaneState {
    source: String,
    generation: u64,
}

static PANES: OnceLock<Mutex<HashMap<i64, Arc<Mutex<PaneState>>>>> = OnceLock::new();

fn panes() -> &'static Mutex<HashMap<i64, Arc<Mutex<PaneState>>>> {
    PANES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_pane(child_id: i64) -> Option<Arc<Mutex<PaneState>>> {
    panes().lock().ok()?.get(&child_id).cloned()
}

fn install_pane(child_id: i64, pane: Arc<Mutex<PaneState>>) {
    if let Ok(mut map) = panes().lock() {
        map.insert(child_id, pane);
    }
}

fn forget_pane(child_id: i64) {
    if let Ok(mut map) = panes().lock() {
        map.remove(&child_id);
    }
}

// ─── Language-thread API ─────────────────────────────────────────────
//
// All three look up the pane, edit the source under the lock, bump the
// generation, then post a repaint. They never see an HWND.

/// Replace the pane's Markdown with `md`. Returns false if there's no
/// such pane (closed, or never opened).
pub fn set_markdown(child_id: i64, md: &str) -> bool {
    let Some(pane) = get_pane(child_id) else {
        return false;
    };
    if let Ok(mut s) = pane.lock() {
        s.source.clear();
        s.source.push_str(md);
        s.generation = s.generation.wrapping_add(1);
    } else {
        return false;
    }
    window::post_doc_flush(child_id);
    true
}

/// Append `md` to the pane's existing Markdown (e.g. streaming a log).
pub fn append_markdown(child_id: i64, md: &str) -> bool {
    let Some(pane) = get_pane(child_id) else {
        return false;
    };
    if let Ok(mut s) = pane.lock() {
        s.source.push_str(md);
        s.generation = s.generation.wrapping_add(1);
    } else {
        return false;
    }
    window::post_doc_flush(child_id);
    true
}

// ─── GUI-thread flush ────────────────────────────────────────────────

/// Called from `frame_wnd_proc` on the GUI thread when a
/// `WM_IGUI_DOC_FLUSH` arrives. The source already lives in the shared
/// `PaneState`; all we do here is invalidate so WM_PAINT re-reads it.
pub(super) fn flush_on_gui_thread(child_id: i64) {
    if let Some(hwnd) = registry::mdi_hwnd_of(child_id) {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

// ─── Per-window state ────────────────────────────────────────────────

struct DocWindowState {
    hwnd: HWND,
    child_id: i64,
    target: Option<ID2D1HwndRenderTarget>,
    pane: Arc<Mutex<PaneState>>,
    content: Option<Layout>,
    /// (x_base, width) the content was laid out at — relayout on change.
    laid_out: (f32, f32),
    /// The `PaneState.gen` the current layout was built from.
    laid_gen: u64,
    scroll_y: f32,
    max_scroll: f32,
    client_w: u32,
    client_h: u32,
    dpi: u32,
}

impl DocWindowState {
    fn new(hwnd: HWND, child_id: i64, pane: Arc<Mutex<PaneState>>) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            child_id,
            target: None,
            pane,
            content: None,
            laid_out: (-1.0, -1.0),
            laid_gen: u64::MAX,
            scroll_y: 0.0,
            max_scroll: 0.0,
            client_w: 0,
            client_h: 0,
            dpi,
        }
    }

    fn dip_scale(&self) -> f32 {
        if self.dpi == 0 { 1.0 } else { 96.0 / (self.dpi as f32) }
    }
    fn viewport(&self) -> (f32, f32) {
        let s = self.dip_scale();
        (self.client_w as f32 * s, self.client_h as f32 * s)
    }
    fn invalidate(&self) {
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    fn ensure_target(&mut self, w: u32, h: u32) {
        if let Some(t) = self.target.as_ref() {
            let cur = unsafe { t.GetPixelSize() };
            if cur.width != w || cur.height != h {
                let _ = unsafe { t.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            return;
        }
        let dpi = self.dpi as f32;
        let target = unsafe {
            render::factory().CreateHwndRenderTarget(
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
            Err(e) => eprintln!("[doc-pane] CreateHwndRenderTarget failed: {e}"),
        }
    }

    fn relayout(&mut self, x_base: f32, width: f32, viewport_h: f32) {
        let cur_gen = self.pane.lock().map(|s| s.generation).unwrap_or(0);
        let stale = self.content.is_none()
            || cur_gen != self.laid_gen
            || (self.laid_out.0 - x_base).abs() > 0.5
            || (self.laid_out.1 - width).abs() > 0.5;
        if stale {
            let md = self
                .pane
                .lock()
                .ok()
                .map(|s| s.source.clone())
                .unwrap_or_default();
            let md = if md.trim().is_empty() {
                "# (empty)\n\nThis pane has no content yet.\n".to_string()
            } else {
                md
            };
            let blocks = parser::parse(&md);
            self.content =
                Some(dlayout::layout(&blocks, x_base, width, 0.0, render::measure_text));
            self.laid_out = (x_base, width);
            self.laid_gen = cur_gen;
        }
        let total = self.content.as_ref().map(|l| l.total_h).unwrap_or(0.0);
        self.max_scroll = (total - viewport_h).max(0.0);
        if self.scroll_y > self.max_scroll {
            self.scroll_y = self.max_scroll;
        }
    }

    fn scroll_by(&mut self, dips: f32) {
        let prev = self.scroll_y;
        self.scroll_y = (self.scroll_y + dips).clamp(0.0, self.max_scroll);
        if (self.scroll_y - prev).abs() > 0.01 {
            self.invalidate();
        }
    }

    fn hit_link(&self, x: f32, y: f32) -> Option<String> {
        let c = self.content.as_ref()?;
        let dy = y + self.scroll_y;
        c.hits
            .iter()
            .find(|h| x >= h.x0 && x <= h.x1 && dy >= h.y0 && dy <= h.y1)
            .map(|h| h.href.clone())
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
        self.ensure_target(w, h);

        let (vw, vh) = self.viewport();
        let cx = theme::H_PAD;
        let cw = (vw - 2.0 * theme::H_PAD).max(1.0);
        self.relayout(cx, cw, vh);

        let target = match self.target.clone() {
            Some(t) => t,
            None => return,
        };
        unsafe {
            target.BeginDraw();
            let bg = theme::hex(theme::BG);
            target.Clear(Some(std::ptr::addr_of!(bg)));
            let base: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget = &target;
            if let Some(c) = self.content.as_ref() {
                let _ = render::draw_document(base, c, self.scroll_y, vh);
            }
            let _ = target.EndDraw(None, None);
        }
    }
}

// ─── Class registration & open ───────────────────────────────────────

pub fn register_class() -> Result<(), super::IGuiError> {
    if let Err(e) = render::init() {
        eprintln!("[doc-pane] render::init failed: {e}");
    }
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (doc): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (doc): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(doc_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: DOC_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

struct DocBootstrap {
    child_id: i64,
    pane: Arc<Mutex<PaneState>>,
}

/// Language-thread entry: open a new (empty) Markdown pane. Returns the
/// child id Lisp uses with `set_markdown` / `append_markdown`.
pub fn open(title: &str) -> Option<i64> {
    window::open_doc_child(title)
}

pub(super) fn create_on_gui_thread(mdi: HWND, title_utf16: &[u16]) -> Option<i64> {
    let child_id = registry::allocate_child_id();
    let pane = Arc::new(Mutex::new(PaneState {
        source: String::new(),
        generation: 0,
    }));
    install_pane(child_id, Arc::clone(&pane));

    let bootstrap = Box::into_raw(Box::new(DocBootstrap {
        child_id,
        pane: Arc::clone(&pane),
    }));

    let h_module = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HANDLE(h.0),
        Err(e) => {
            eprintln!("[doc-pane] GetModuleHandleW: {e}");
            forget_pane(child_id);
            let _ = unsafe { Box::from_raw(bootstrap) };
            return None;
        }
    };
    let create = MDICREATESTRUCTW {
        szClass: DOC_CLASS,
        szTitle: PCWSTR::from_raw(title_utf16.as_ptr()),
        hOwner: h_module,
        x: CW_USEDEFAULT,
        y: CW_USEDEFAULT,
        cx: CW_USEDEFAULT,
        cy: CW_USEDEFAULT,
        style: WS_VISIBLE | WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(bootstrap as isize),
    };
    let result = unsafe {
        SendMessageW(mdi, WM_MDICREATE, Some(WPARAM(0)), Some(LPARAM(&create as *const _ as isize)))
    };
    if result.0 == 0 {
        eprintln!("[doc-pane] WM_MDICREATE returned 0");
        forget_pane(child_id);
        let _ = unsafe { Box::from_raw(bootstrap) };
        return None;
    }
    Some(child_id)
}

// ─── Window proc ─────────────────────────────────────────────────────

unsafe extern "system" fn doc_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        let mdi_create = unsafe { (*create).lpCreateParams as *const MDICREATESTRUCTW };
        if mdi_create.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap_ptr = unsafe { (*mdi_create).lParam.0 as *mut DocBootstrap };
        if bootstrap_ptr.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap = unsafe { Box::from_raw(bootstrap_ptr) };
        let child_id = bootstrap.child_id;
        let win_state = Box::new(DocWindowState::new(hwnd, child_id, bootstrap.pane));
        let raw = Box::into_raw(win_state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        // Doc pane has no separate render-host HWND: the MDI child *is*
        // the render target. Register the same HWND in both slots so
        // `mdi_hwnd_of` / `render_hwnd_of` both resolve.
        registry::register(child_id, hwnd, hwnd);
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut DocWindowState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };
    let scale = state.dip_scale();

    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _ = unsafe { BeginPaint(hwnd, &mut ps) };
            state.paint();
            let _ = unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as u32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
            state.client_w = w;
            state.client_h = h;
            if let Some(t) = state.target.as_ref() {
                let _ = unsafe { t.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            state.content = None; // content width tracks the window
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f32;
            state.scroll_by(-(delta / 120.0) * WHEEL_STEP);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // A generic Lisp-fed pane has no folder context, so there's
            // no link navigation here. We still hit-test so a future
            // revision can open external URLs.
            let dx = (lparam.0 & 0xFFFF) as i16 as f32 * scale;
            let dy = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 * scale;
            let _ = state.hit_link(dx, dy);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            registry::unregister(state.child_id);
            forget_pane(state.child_id);
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}
