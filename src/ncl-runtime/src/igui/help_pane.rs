//! help_pane — an MDI child that browses a folder of Markdown documents,
//! with the DocCrate-style sidebar: a list of pages on the left, the
//! current page on the right, a draggable divider to resize the sidebar,
//! and a ‹/› toggle (with a reveal tab) to collapse and restore it.
//! Click a sidebar row or an in-page link to navigate; wheel to scroll.
//!
//! The render core renders one document from text; this host owns the
//! browser chrome — folder scan, the row sidebar, navigation — drawn
//! with `docpane::render`'s `fill_rect` / `draw_text` primitives plus
//! `draw_document` for the page body.
//!
//! Factory note: the pane's render target comes from *docpane's* D2D
//! factory (`render::factory()`), so Mermaid geometry and the target
//! share one factory.

#![cfg(windows)]

use std::path::{Path, PathBuf};

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
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, LoadCursorW, RegisterClassExW, SendMessageW,
    SetCursor, SetWindowLongPtrW, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, IDC_HAND,
    IDC_SIZEWE, MDICREATESTRUCTW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MDICREATE, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SIZE, WNDCLASSEXW, WNDCLASS_STYLES,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use docpane::layout::Layout;
use docpane::{layout as dlayout, parser, render, theme};

use super::{registry, window};

const HELP_CLASS: PCWSTR = w!("NclHelpPane");
const WHEEL_STEP: f32 = 48.0;
// Sidebar chrome geometry (DIPs).
const SB_MIN: f32 = 120.0; // min sidebar width when dragging
const TAB_W: f32 = 16.0; // reveal-tab width when collapsed
const BTN_W: f32 = 16.0; // toggle button
const BTN_H: f32 = 24.0;
const DIV_HIT: f32 = 5.0; // grab zone each side of the divider

struct DocFile {
    name: String,
    path: PathBuf,
}

fn scan(dir: &Path) -> Vec<DocFile> {
    let mut files: Vec<DocFile> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("md") {
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("untitled")
                    .to_string();
                files.push(DocFile { name, path: p });
            }
        }
    }
    files.sort_by(|a, b| {
        let ap = a.name.eq_ignore_ascii_case("index") || a.name.eq_ignore_ascii_case("readme");
        let bp = b.name.eq_ignore_ascii_case("index") || b.name.eq_ignore_ascii_case("readme");
        match (ap, bp) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
    files
}

/// `index` → "Index", `let-algebra` → "Let Algebra".
fn pretty(name: &str) -> String {
    name.replace(['-', '_'], " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_href(href: &str, current: &Path, docs_dir: &Path) -> Option<PathBuf> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return None;
    }
    let href = href.split('#').next().unwrap_or("");
    if href.is_empty() {
        return None;
    }
    let base = current.parent().unwrap_or(docs_dir);
    let cand = base.join(href);
    if cand.exists() {
        return Some(cand);
    }
    let with = base.join(format!("{href}.md"));
    if with.exists() {
        return Some(with);
    }
    None
}

fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

// ─── Per-window state ────────────────────────────────────────────────
struct HelpWindowState {
    hwnd: HWND,
    child_id: i64,
    target: Option<ID2D1HwndRenderTarget>,
    docs_dir: PathBuf,
    files: Vec<DocFile>,
    current: usize,
    content: Option<Layout>,
    /// (x_base, width) the content was laid out at — relayout on change.
    laid_out: (f32, f32),
    scroll_y: f32,
    max_scroll: f32,
    // Sidebar chrome
    sidebar_w: f32,
    sidebar_saved: f32,
    dragging_div: bool,
    hover_sidebar: Option<usize>,
    hover_toggle: bool,
    client_w: u32,
    client_h: u32,
    dpi: u32,
}

impl HelpWindowState {
    fn new(hwnd: HWND, child_id: i64, path: &str) -> Self {
        let p = PathBuf::from(path);
        let (docs_dir, initial) = if p.is_dir() {
            (p.clone(), None)
        } else {
            (
                p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")),
                Some(p.clone()),
            )
        };
        let files = scan(&docs_dir);
        let current = initial
            .and_then(|ip| files.iter().position(|f| same_file(&f.path, &ip)))
            .unwrap_or(0);
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            child_id,
            target: None,
            docs_dir,
            files,
            current,
            content: None,
            laid_out: (-1.0, -1.0),
            scroll_y: 0.0,
            max_scroll: 0.0,
            sidebar_w: theme::SIDEBAR_W,
            sidebar_saved: theme::SIDEBAR_W,
            dragging_div: false,
            hover_sidebar: None,
            hover_toggle: false,
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
    fn hidden(&self) -> bool {
        self.sidebar_w < 1.0
    }
    fn current_path(&self) -> Option<PathBuf> {
        self.files.get(self.current).map(|f| f.path.clone())
    }

    /// Left edge of the content area (DIPs) — past the sidebar, or past
    /// the reveal tab when collapsed.
    fn content_x(&self) -> f32 {
        (if self.hidden() { TAB_W } else { self.sidebar_w + 1.0 }) + theme::H_PAD
    }

    fn toggle_btn_rect(&self, vh: f32) -> (f32, f32, f32, f32) {
        let by = (vh / 2.0 - BTN_H / 2.0).round();
        if self.hidden() {
            (0.0, by, TAB_W, BTN_H)
        } else {
            (self.sidebar_w - BTN_W / 2.0, by, BTN_W, BTN_H)
        }
    }
    fn on_toggle_btn(&self, x: f32, y: f32, vh: f32) -> bool {
        let (bx, by, bw, bh) = self.toggle_btn_rect(vh);
        x >= bx && x <= bx + bw && y >= by && y <= by + bh
    }
    fn on_divider(&self, x: f32) -> bool {
        !self.hidden() && (x - self.sidebar_w).abs() < DIV_HIT
    }
    fn hit_sidebar_row(&self, x: f32, y: f32) -> Option<usize> {
        if self.hidden() || x >= self.sidebar_w {
            return None;
        }
        let i = (y - 36.0) / theme::SIDEBAR_ITEM_H;
        if i < 0.0 {
            return None;
        }
        let i = i as usize;
        (i < self.files.len()).then_some(i)
    }
    fn hit_link(&self, x: f32, y: f32) -> Option<String> {
        let c = self.content.as_ref()?;
        let dy = y + self.scroll_y;
        c.hits
            .iter()
            .find(|h| x >= h.x0 && x <= h.x1 && dy >= h.y0 && dy <= h.y1)
            .map(|h| h.href.clone())
    }

    fn toggle_sidebar(&mut self) {
        if self.hidden() {
            self.sidebar_w = if self.sidebar_saved >= SB_MIN {
                self.sidebar_saved
            } else {
                theme::SIDEBAR_W
            };
        } else {
            self.sidebar_saved = self.sidebar_w;
            self.sidebar_w = 0.0;
        }
        self.content = None; // content width changed
        self.invalidate();
    }

    fn navigate(&mut self, idx: usize) {
        if idx >= self.files.len() || idx == self.current {
            return;
        }
        self.current = idx;
        self.scroll_y = 0.0;
        self.content = None;
        self.invalidate();
    }
    fn nav_href(&mut self, href: &str) {
        let cur = self.current_path().unwrap_or_else(|| self.docs_dir.clone());
        if let Some(p) = resolve_href(href, &cur, &self.docs_dir) {
            if let Some(idx) = self.files.iter().position(|f| same_file(&f.path, &p)) {
                self.navigate(idx);
            }
        }
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
            Err(e) => eprintln!("[help-pane] CreateHwndRenderTarget failed: {e}"),
        }
    }

    fn relayout(&mut self, x_base: f32, width: f32, viewport_h: f32) {
        if self.content.is_none() || (self.laid_out.0 - x_base).abs() > 0.5
            || (self.laid_out.1 - width).abs() > 0.5
        {
            let md = self
                .current_path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_else(|| "# (no document)\n".to_string());
            let blocks = parser::parse(&md);
            self.content =
                Some(dlayout::layout(&blocks, x_base, width, 0.0, render::measure_text));
            self.laid_out = (x_base, width);
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

    unsafe fn draw_sidebar(&self, t: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget, vh: f32) {
        // Edition-2024: the body of an `unsafe fn` is no longer
        // implicitly an unsafe block. Group the D2D draw calls inside
        // one explicit unsafe block.
        let sw = self.sidebar_w;
        unsafe {
            render::fill_rect(t, 0.0, 0.0, sw, vh, theme::SIDEBAR_BG);
            // "DOCS" header
            render::draw_text(
                t, 14.0, 13.0, sw - 18.0, 16.0, "DOCS", theme::BODY_FONT, 10.5, true, false,
                theme::TEXT_DIM, false,
            );
            for (i, f) in self.files.iter().enumerate() {
                let y = 36.0 + i as f32 * theme::SIDEBAR_ITEM_H;
                let sel = i == self.current;
                let hov = self.hover_sidebar == Some(i);
                if sel {
                    render::fill_rect(t, 0.0, y, sw - 1.0, theme::SIDEBAR_ITEM_H, theme::SIDEBAR_SEL);
                    render::fill_rect(t, 0.0, y, 2.0, theme::SIDEBAR_ITEM_H, theme::LINK);
                } else if hov {
                    render::fill_rect(t, 0.0, y, sw - 1.0, theme::SIDEBAR_ITEM_H, theme::SIDEBAR_HVR);
                }
                let col = if sel { theme::TEXT_BRIGHT } else { theme::TEXT };
                render::draw_text(
                    t, 14.0, y + 5.0, sw - 24.0, theme::SIDEBAR_ITEM_H, &pretty(&f.name),
                    theme::BODY_FONT, theme::SIDEBAR_FONT_SIZE, sel, false, col, false,
                );
            }
            // Divider
            render::fill_rect(t, sw - 1.0, 0.0, 1.5, vh, theme::BORDER);
        }
    }

    unsafe fn draw_toggle(&self, t: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget, vh: f32) {
        let (bx, by, bw, bh) = self.toggle_btn_rect(vh);
        let (bg, fg) = if self.hover_toggle {
            (theme::SIDEBAR_SEL, theme::TEXT_BRIGHT)
        } else {
            (theme::SIDEBAR_BG, theme::TEXT_DIM)
        };
        let glyph = if self.hidden() { "\u{203A}" } else { "\u{2039}" }; // › ‹
        unsafe {
            render::fill_rect(t, bx, by, bw, bh, bg);
            render::draw_text(t, bx, by, bw, bh, glyph, theme::BODY_FONT, 12.0, true, false, fg, true);
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
        self.ensure_target(w, h);

        let (vw, vh) = self.viewport();
        let cx = self.content_x();
        let cw = (vw - cx - theme::H_PAD).max(1.0);
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

            if self.hidden() {
                // Reveal tab strip
                render::fill_rect(base, 0.0, 0.0, TAB_W, vh, theme::SIDEBAR_BG);
                render::fill_rect(base, TAB_W - 1.0, 0.0, 1.0, vh, theme::BORDER);
            } else {
                self.draw_sidebar(base, vh);
            }
            if let Some(c) = self.content.as_ref() {
                let _ = render::draw_document(base, c, self.scroll_y, vh);
            }
            self.draw_toggle(base, vh);
            let _ = target.EndDraw(None, None);
        }
    }
}

// ─── Class registration ─────────────────────────────────────────────
pub fn register_class() -> Result<(), super::IGuiError> {
    if let Err(e) = render::init() {
        eprintln!("[help-pane] render::init failed: {e}");
    }
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (doc): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (doc): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(help_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: HELP_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

struct HelpBootstrap {
    child_id: i64,
    path: String,
}

/// Open a help-pane.  `path` is a docs folder or a `.md` file.
pub fn open(title: &str, path: &str) -> Option<i64> {
    window::open_help_child(title, path)
}

pub(super) fn create_on_gui_thread(mdi: HWND, title_utf16: &[u16], path: &str) -> Option<i64> {
    let child_id = registry::allocate_child_id();
    let bootstrap = Box::into_raw(Box::new(HelpBootstrap { child_id, path: path.to_owned() }));
    let h_module = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => HANDLE(h.0),
        Err(e) => {
            eprintln!("[help-pane] GetModuleHandleW: {e}");
            let _ = unsafe { Box::from_raw(bootstrap) };
            return None;
        }
    };
    let create = MDICREATESTRUCTW {
        szClass: HELP_CLASS,
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
        eprintln!("[help-pane] WM_MDICREATE returned 0");
        let _ = unsafe { Box::from_raw(bootstrap) };
        return None;
    }
    Some(child_id)
}

// ─── Window proc ────────────────────────────────────────────────────
unsafe extern "system" fn help_wnd_proc(
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
        let bootstrap_ptr = unsafe { (*mdi_create).lParam.0 as *mut HelpBootstrap };
        if bootstrap_ptr.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap = unsafe { Box::from_raw(bootstrap_ptr) };
        let child_id = bootstrap.child_id;
        let win_state = Box::new(HelpWindowState::new(hwnd, child_id, &bootstrap.path));
        let raw = Box::into_raw(win_state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        registry::register(child_id, hwnd, hwnd);
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut HelpWindowState;
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
        WM_MOUSEMOVE => {
            let dx = (lparam.0 & 0xFFFF) as i16 as f32 * scale;
            let dy = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 * scale;
            let (vw, vh) = state.viewport();
            if state.dragging_div {
                state.sidebar_w = dx.clamp(SB_MIN, (vw - 160.0).max(SB_MIN));
                state.content = None;
                state.invalidate();
                set_cursor(IDC_SIZEWE);
                return LRESULT(0);
            }
            let prev_sb = state.hover_sidebar;
            let prev_tg = state.hover_toggle;
            state.hover_sidebar = state.hit_sidebar_row(dx, dy);
            state.hover_toggle = state.on_toggle_btn(dx, dy, vh);
            let cursor = if state.on_divider(dx) {
                IDC_SIZEWE
            } else if state.hover_toggle
                || state.hover_sidebar.is_some()
                || state.hit_link(dx, dy).is_some()
            {
                IDC_HAND
            } else {
                IDC_ARROW
            };
            set_cursor(cursor);
            if state.hover_sidebar != prev_sb || state.hover_toggle != prev_tg {
                state.invalidate();
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let dx = (lparam.0 & 0xFFFF) as i16 as f32 * scale;
            let dy = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 * scale;
            let (_vw, vh) = state.viewport();
            if state.hidden() {
                if dx < TAB_W {
                    state.toggle_sidebar();
                    return LRESULT(0);
                }
            } else {
                if state.on_toggle_btn(dx, dy, vh) {
                    state.toggle_sidebar();
                    return LRESULT(0);
                }
                if state.on_divider(dx) {
                    state.dragging_div = true;
                    let _ = unsafe { SetCapture(hwnd) };
                    return LRESULT(0);
                }
                if let Some(idx) = state.hit_sidebar_row(dx, dy) {
                    state.navigate(idx);
                    return LRESULT(0);
                }
            }
            if let Some(href) = state.hit_link(dx, dy) {
                state.nav_href(&href);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if state.dragging_div {
                state.dragging_div = false;
                let _ = unsafe { ReleaseCapture() };
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            registry::unregister(state.child_id);
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

fn set_cursor(id: PCWSTR) {
    if let Ok(c) = unsafe { LoadCursorW(None, id) } {
        unsafe { SetCursor(Some(c)) };
    }
}
