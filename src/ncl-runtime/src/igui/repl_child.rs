//! REPL MDI child — split-pane graphical Common Lisp REPL.
//!
//! Layout (split by a draggable splitter):
//!
//!   ┌──────────────────────────────────────────┐
//!   │  TRANSCRIPT  (scrollable, append-only)   │
//!   ├──────────────── splitter ────────────────┤
//!   │  INPUT EDITOR  (multi-line, expeditor)   │
//!   └──────────────────────────────────────────┘
//!
//! Evaluation flow:
//!   1. User types; Enter on a *complete* form pushes the input to
//!      `PENDING_INPUTS` and fires `IGuiEvent::ReplSubmit`.
//!   2. The Lisp worker receives the event, calls `(repl-pop-input id)`
//!      to retrieve the string, evaluates it, and calls
//!      `(repl-output id text)` / `(repl-error id text)`.
//!   3. Those shims call `repl_child::append`, which queues a
//!      `PendingOutput` and posts `WM_REPL_FLUSH` to the MDI child.
//!   4. On `WM_REPL_FLUSH` the WndProc drains the queue into the
//!      transcript Vec and invalidates the window.

#![cfg(windows)]

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::DirectWrite::{
    IDWriteTextFormat, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_MEASURING_MODE_NATURAL, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, DefMDIChildProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW, KillTimer,
    LoadCursorW, PostMessageW, RegisterClassExW, SendMessageW, SetTimer, SetWindowLongPtrW,
    GWLP_USERDATA, IDC_IBEAM, MDICREATESTRUCTW, WM_CHAR, WM_CLOSE, WM_KEYDOWN, WM_KILLFOCUS,
    WM_MDICREATE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN,
    WM_TIMER, WM_USER, WNDCLASSEXW, WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows_numerics::Vector2;

use super::channels::{self, IGuiEvent};
use super::registry;
use super::renderer;
use super::IGuiError;

// ─── Window class ─────────────────────────────────────────────────────────────

pub(crate) const REPL_CLASS: PCWSTR = w!("NewCL.iGui.Repl");

/// WM_COMMAND ID for the Tools → REPL menu item and Ctrl+Shift+R accelerator.
pub const MENU_CMD_ID: u16 = 0x3002;

/// Flush pending output to the transcript. Posted from `append()` on
/// the worker thread.
const WM_REPL_FLUSH: u32 = WM_USER + 20;

/// Caret blink timer.
const CARET_TIMER_ID: usize = 0x5002;
const CARET_BLINK_MS: u32 = 530;

// ─── Palette ──────────────────────────────────────────────────────────────────

const fn cf(r: f32, g: f32, b: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a: 1.0 }
}
const fn cfa(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

const BG:             D2D1_COLOR_F = cf(0.102, 0.114, 0.137); // #1A1D23
const TRANSCRIPT_BG:  D2D1_COLOR_F = cf(0.086, 0.098, 0.118); // slightly darker
const SPLITTER_BG:    D2D1_COLOR_F = cf(0.150, 0.168, 0.196);
const COL_PROMPT:     D2D1_COLOR_F = cf(0.302, 0.608, 0.961); // #4D9BF5 accent blue
const COL_CONT:       D2D1_COLOR_F = cf(0.350, 0.400, 0.480); // continuation prompt dimmer
const COL_INPUT:      D2D1_COLOR_F = cf(0.878, 0.894, 0.910); // #E0E4E8
const COL_OUTPUT:     D2D1_COLOR_F = cf(0.720, 0.756, 0.792); // slightly dimmer
const COL_ERROR:      D2D1_COLOR_F = cf(0.941, 0.443, 0.471); // #F07178 salmon
const COL_BANNER:     D2D1_COLOR_F = cf(0.533, 0.553, 0.588); // muted gray
const COL_CURSOR:     D2D1_COLOR_F = cf(0.302, 0.608, 0.961); // same as prompt
const COL_SELECTION:  D2D1_COLOR_F = cfa(0.302, 0.608, 0.961, 0.25);

// ─── Typography ───────────────────────────────────────────────────────────────

const FONT_SIZE:    f32 = 13.5;
const LINE_HEIGHT:  f32 = 21.0;
const CELL_W:       f32 = 8.1;   // approximate monospace advance; refined at runtime
const MARGIN_H:     f32 = 10.0;  // left/right padding
const MARGIN_TOP:   f32 = 8.0;   // top padding inside each pane
const SPLITTER_H:   f32 = 4.0;
const MIN_TRANS_H:  f32 = 60.0;
const MIN_INPUT_H:  f32 = 36.0;
const PROMPT:       &str = "ncl> ";
const CONT_PROMPT:  &str = "...> ";
const PROMPT_W:     f32 = 42.0; // visual width of "ncl> " / "...> " at this font

// ─── Transcript entries ───────────────────────────────────────────────────────

/// Kind of transcript line. Determines colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppendKind {
    Banner,
    Input,
    Output,
    Error,
}

#[derive(Clone)]
struct TranscriptEntry {
    kind: AppendKind,
    text: String,
}

impl TranscriptEntry {
    /// Total lines this entry occupies (split on \n).
    fn line_count(&self) -> usize {
        self.text.split('\n').count().max(1)
    }
    fn height(&self) -> f32 {
        self.line_count() as f32 * LINE_HEIGHT + 3.0 // 3px gap after each entry
    }
}

// ─── Cross-thread queues ──────────────────────────────────────────────────────

struct PendingOutput {
    kind: AppendKind,
    text: String,
}

/// Worker-thread output → GUI-thread transcript.
static PENDING_OUTPUTS: OnceLock<Mutex<HashMap<i64, VecDeque<PendingOutput>>>> = OnceLock::new();
fn pending_outputs() -> &'static Mutex<HashMap<i64, VecDeque<PendingOutput>>> {
    PENDING_OUTPUTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// GUI-thread submitted inputs → worker-thread eval.
static PENDING_INPUTS: OnceLock<Mutex<HashMap<i64, VecDeque<String>>>> = OnceLock::new();
fn pending_inputs() -> &'static Mutex<HashMap<i64, VecDeque<String>>> {
    PENDING_INPUTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// HWND map so `append` can PostMessage to the right child.
static REPL_HWNDS: OnceLock<Mutex<HashMap<i64, isize>>> = OnceLock::new();
fn repl_hwnds() -> &'static Mutex<HashMap<i64, isize>> {
    REPL_HWNDS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn register_hwnd(id: i64, hwnd: HWND) {
    repl_hwnds().lock().unwrap().insert(id, hwnd.0 as isize);
}
fn unregister_hwnd(id: i64) {
    repl_hwnds().lock().unwrap().remove(&id);
}
fn hwnd_of(id: i64) -> Option<HWND> {
    repl_hwnds()
        .lock()
        .unwrap()
        .get(&id)
        .map(|&raw| HWND(raw as *mut _))
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Append output to a REPL child's transcript. Called from the Lisp
/// worker after evaluating a submitted form. Thread-safe.
pub fn append(child_id: i64, text: String, kind: AppendKind) {
    {
        let mut map = pending_outputs().lock().unwrap_or_else(|p| p.into_inner());
        map.entry(child_id)
            .or_default()
            .push_back(PendingOutput { kind, text });
    }
    if let Some(hwnd) = hwnd_of(child_id) {
        let _ = unsafe { PostMessageW(Some(hwnd), WM_REPL_FLUSH, WPARAM(0), LPARAM(0)) };
    }
}

/// Pop the next submitted input for `child_id`. Returns `None` if the
/// queue is empty. Called by the Lisp worker via `repl-pop-input`.
pub fn pop_input(child_id: i64) -> Option<String> {
    pending_inputs()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_mut(&child_id)?
        .pop_front()
}

// ─── Registration ─────────────────────────────────────────────────────────────

pub fn register_class() -> Result<(), IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| IGuiError::Win32(format!("GetModuleHandleW (repl): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_IBEAM) }
        .map_err(|e| IGuiError::Win32(format!("LoadCursorW (repl): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(repl_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: REPL_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

/// Open a REPL MDI child. Called on the GUI thread from the
/// `WM_IGUI_OPEN_REPL` handler in `window.rs`.
/// Open a new REPL child from the GUI thread (e.g. Tools menu / Ctrl+Shift+R).
/// Allocates a fresh child_id and fires up the window immediately.
pub fn open(mdi_client: HWND) {
    let child_id = registry::allocate_child_id();
    let mut title_w: Vec<u16> = "NCL REPL"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    open_from_gui_thread(mdi_client, &title_w, child_id);
    let _ = title_w; // suppress unused warning
}

pub fn open_from_gui_thread(mdi_client: HWND, title_w: &[u16], child_id: i64) -> bool {
    let title_ptr = PCWSTR::from_raw(title_w.as_ptr());
    let h_module = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => windows::Win32::Foundation::HANDLE(h.0),
        Err(e) => {
            eprintln!("[repl] GetModuleHandleW: {e}");
            return false;
        }
    };

    // Encode child_id in lParam so WM_NCCREATE can recover it.
    let bootstrap = Box::into_raw(Box::new(ReplBootstrap { child_id }));
    let create = MDICREATESTRUCTW {
        szClass: REPL_CLASS,
        szTitle: title_ptr,
        hOwner: h_module,
        x: CW_USEDEFAULT,
        y: CW_USEDEFAULT,
        cx: 800,
        cy: 560,
        style: WS_VISIBLE | WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(bootstrap as isize),
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
        eprintln!("[repl] WM_MDICREATE returned 0");
        let _ = unsafe { Box::from_raw(bootstrap) };
        return false;
    }
    true
}

// ─── Bootstrap ────────────────────────────────────────────────────────────────

struct ReplBootstrap {
    child_id: i64,
}

// ─── Per-window state ─────────────────────────────────────────────────────────

struct ReplState {
    child_id: i64,
    hwnd: HWND,
    target: Option<ID2D1HwndRenderTarget>,
    fmt: Option<IDWriteTextFormat>,       // normal weight
    fmt_bold: Option<IDWriteTextFormat>,  // bold weight (prompt)
    cell_w: f32,                          // measured advance width of one char

    // Transcript (append-only).
    transcript: Vec<TranscriptEntry>,
    scroll_y: f32, // pixels scrolled from top; 0 = top

    // Input editor.
    input: String,
    cursor_byte: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    saved_input: String,

    // Layout.
    width: f32,
    height: f32,
    split_y: f32, // y of the splitter top edge

    // Focus / caret.
    has_focus: bool,
    caret_on: bool,
}

impl ReplState {
    fn new(child_id: i64, hwnd: HWND) -> Box<Self> {
        Box::new(Self {
            child_id,
            hwnd,
            target: None,
            fmt: None,
            fmt_bold: None,
            cell_w: CELL_W,
            transcript: vec![TranscriptEntry {
                kind: AppendKind::Banner,
                text: format!(
                    "NCL {}  —  REPL",
                    env!("CARGO_PKG_VERSION")
                ),
            }],
            scroll_y: 0.0,
            input: String::new(),
            cursor_byte: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            width: 800.0,
            height: 560.0,
            split_y: 560.0 * 0.72,
            has_focus: false,
            caret_on: true,
        })
    }

    // ── Resource management ──────────────────────────────────────────────────

    fn ensure_resources(&mut self) -> bool {
        if self.target.is_none() {
            let mut rc = RECT::default();
            let _ = unsafe { GetClientRect(self.hwnd, &mut rc) };
            let w = (rc.right - rc.left) as u32;
            let h = (rc.bottom - rc.top) as u32;
            if w == 0 || h == 0 {
                return false;
            }
            let factory = &renderer::ctx().d2d.factory;
            match unsafe {
                factory.CreateHwndRenderTarget(
                    &D2D1_RENDER_TARGET_PROPERTIES {
                        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                        pixelFormat: D2D1_PIXEL_FORMAT {
                            format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            alphaMode: D2D1_ALPHA_MODE_IGNORE,
                        },
                        dpiX: 96.0,
                        dpiY: 96.0,
                        usage: D2D1_RENDER_TARGET_USAGE_NONE,
                        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                    },
                    &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                        hwnd: self.hwnd,
                        pixelSize: D2D_SIZE_U { width: w, height: h },
                        presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                    },
                )
            } {
                Ok(t) => {
                    self.target = Some(t);
                }
                Err(e) => {
                    eprintln!("[repl] CreateHwndRenderTarget: {e}");
                    return false;
                }
            }
        }

        if self.fmt.is_none() {
            let dw = &renderer::ctx().dwrite.factory;
            // Cascadia Mono preferred over Cascadia Code: same design, same
            // metrics, but no programming ligatures.  Ligatures break cursor
            // arithmetic (<=  renders as the single ≤ glyph but cell_w still
            // counts two chars) and would confuse the form-completeness parser
            // if a rendered glyph were ever fed back as input text.
            for family in ["Cascadia Mono", "Cascadia Code", "Consolas", "Courier New"] {
                let fam_w: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
                let loc_w: Vec<u16> = "en-us\0".encode_utf16().collect();
                let fmt = unsafe {
                    dw.CreateTextFormat(
                        PCWSTR(fam_w.as_ptr()),
                        None,
                        DWRITE_FONT_WEIGHT(400),
                        DWRITE_FONT_STYLE_NORMAL,
                        DWRITE_FONT_STRETCH_NORMAL,
                        FONT_SIZE,
                        PCWSTR(loc_w.as_ptr()),
                    )
                };
                if let Ok(fmt) = fmt {
                    let _ = unsafe { fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP) };
                    // Measure a single character to calibrate cell_w.
                    if let Some(w) = measure_char_width(&fmt, dw) {
                        self.cell_w = w;
                    }
                    // Bold variant for prompts.
                    let bold = unsafe {
                        dw.CreateTextFormat(
                            PCWSTR(fam_w.as_ptr()),
                            None,
                            DWRITE_FONT_WEIGHT(600),
                            DWRITE_FONT_STYLE_NORMAL,
                            DWRITE_FONT_STRETCH_NORMAL,
                            FONT_SIZE,
                            PCWSTR(loc_w.as_ptr()),
                        )
                    };
                    if let Ok(b) = bold {
                        let _ = unsafe { b.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP) };
                        self.fmt_bold = Some(b);
                    } else {
                        self.fmt_bold = Some(fmt.clone());
                    }
                    self.fmt = Some(fmt);
                    break;
                }
            }
        }

        self.target.is_some() && self.fmt.is_some()
    }

    fn handle_resize(&mut self, w: f32, h: f32) {
        let frac = if self.height > 0.0 {
            self.split_y / self.height
        } else {
            0.72
        };
        self.width = w;
        self.height = h;
        self.split_y = (h * frac)
            .max(MIN_TRANS_H)
            .min(h - MIN_INPUT_H - SPLITTER_H);
        if let Some(t) = self.target.as_ref() {
            let _ = unsafe {
                t.Resize(&D2D_SIZE_U {
                    width: w as u32,
                    height: h as u32,
                })
            };
        }
    }

    // ── Transcript ───────────────────────────────────────────────────────────

    fn drain_pending(&mut self) {
        let mut map = pending_outputs().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(q) = map.get_mut(&self.child_id) {
            while let Some(p) = q.pop_front() {
                self.transcript.push(TranscriptEntry {
                    kind: p.kind,
                    text: p.text,
                });
            }
        }
        self.clamp_scroll();
        self.auto_scroll_to_bottom();
    }

    fn transcript_total_height(&self) -> f32 {
        let mut h = MARGIN_TOP;
        for e in &self.transcript {
            h += e.height();
        }
        h + MARGIN_TOP
    }

    fn auto_scroll_to_bottom(&mut self) {
        let visible = (self.split_y - MARGIN_TOP).max(0.0);
        let total = self.transcript_total_height();
        if total > visible {
            self.scroll_y = total - visible;
        }
    }

    fn clamp_scroll(&mut self) {
        let visible = (self.split_y - MARGIN_TOP).max(0.0);
        let max_scroll = (self.transcript_total_height() - visible).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_scroll);
    }

    // ── Input editing ────────────────────────────────────────────────────────

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.input.insert_str(self.cursor_byte, s);
        self.cursor_byte += s.len();
        self.history_idx = None;
    }

    fn delete_back(&mut self) {
        if self.cursor_byte == 0 {
            return;
        }
        let mut i = self.cursor_byte - 1;
        while i > 0 && !self.input.is_char_boundary(i) {
            i -= 1;
        }
        self.input.drain(i..self.cursor_byte);
        self.cursor_byte = i;
    }

    fn delete_forward(&mut self) {
        if self.cursor_byte >= self.input.len() {
            return;
        }
        let end = next_char_boundary(&self.input, self.cursor_byte);
        self.input.drain(self.cursor_byte..end);
    }

    fn move_left(&mut self, extend: bool) {
        let _ = extend; // selection deferred to Phase 2
        if self.cursor_byte == 0 {
            return;
        }
        let mut i = self.cursor_byte - 1;
        while i > 0 && !self.input.is_char_boundary(i) {
            i -= 1;
        }
        self.cursor_byte = i;
    }

    fn move_right(&mut self, extend: bool) {
        let _ = extend;
        if self.cursor_byte >= self.input.len() {
            return;
        }
        self.cursor_byte = next_char_boundary(&self.input, self.cursor_byte);
    }

    fn move_home(&mut self) {
        if let Some(nl) = self.input[..self.cursor_byte].rfind('\n') {
            self.cursor_byte = nl + 1;
        } else {
            self.cursor_byte = 0;
        }
    }

    fn move_end(&mut self) {
        if let Some(nl) = self.input[self.cursor_byte..].find('\n') {
            self.cursor_byte += nl;
        } else {
            self.cursor_byte = self.input.len();
        }
    }

    /// Move by whole word (skip whitespace, then non-whitespace).
    fn move_word_right(&mut self) {
        let s = &self.input[self.cursor_byte..];
        let skip_ws: usize = s.chars().take_while(|c| c.is_whitespace()).map(|c| c.len_utf8()).sum();
        let skip_word: usize = s[skip_ws..]
            .chars()
            .take_while(|c| !c.is_whitespace())
            .map(|c| c.len_utf8())
            .sum();
        self.cursor_byte += skip_ws + skip_word;
    }

    fn move_word_left(&mut self) {
        let s = &self.input[..self.cursor_byte];
        // reverse char iteration
        let chars: Vec<char> = s.chars().collect();
        let mut skip = 0usize;
        let mut it = chars.iter().rev();
        while let Some(&c) = it.next() {
            if c.is_whitespace() { skip += c.len_utf8(); } else { break; }
        }
        while let Some(&c) = it.next() {
            if !c.is_whitespace() { skip += c.len_utf8(); } else { break; }
        }
        self.cursor_byte -= skip;
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.saved_input = self.input.clone();
                let idx = self.history.len() - 1;
                self.history_idx = Some(idx);
                self.input = self.history[idx].clone();
                self.cursor_byte = self.input.len();
            }
            Some(0) => {}
            Some(i) => {
                let idx = i - 1;
                self.history_idx = Some(idx);
                self.input = self.history[idx].clone();
                self.cursor_byte = self.input.len();
            }
        }
    }

    fn history_down(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) if i + 1 >= self.history.len() => {
                self.history_idx = None;
                self.input = std::mem::take(&mut self.saved_input);
                self.cursor_byte = self.input.len();
            }
            Some(i) => {
                let idx = i + 1;
                self.history_idx = Some(idx);
                self.input = self.history[idx].clone();
                self.cursor_byte = self.input.len();
            }
        }
    }

    /// Called when the user presses Enter.  Returns `Some(input)` when
    /// the form is complete and should be evaluated; `None` when a
    /// newline was inserted for continuation.
    fn try_submit(&mut self) -> Option<String> {
        if self.input.trim().is_empty() {
            self.input.clear();
            self.cursor_byte = 0;
            return None;
        }
        match form_complete(&self.input) {
            FormStatus::Complete => {
                let submitted = self.input.clone();
                if self.history.last().map(|s| s.as_str()) != Some(&submitted) {
                    self.history.push(submitted.clone());
                }
                self.history_idx = None;
                self.saved_input.clear();
                self.input.clear();
                self.cursor_byte = 0;
                // Echo the submitted form in the transcript.
                self.transcript.push(TranscriptEntry {
                    kind: AppendKind::Input,
                    text: submitted.clone(),
                });
                self.auto_scroll_to_bottom();
                Some(submitted)
            }
            FormStatus::Incomplete | FormStatus::InString => {
                // Insert newline + auto-indent.
                let before = self.input[..self.cursor_byte].to_string();
                let indent = compute_indent(&before);
                self.insert_char('\n');
                for c in indent.chars() {
                    self.insert_char(c);
                }
                None
            }
        }
    }

    // ── Painting ─────────────────────────────────────────────────────────────

    fn paint(&mut self) {
        if !self.ensure_resources() {
            return;
        }
        self.drain_pending();

        let Some(target) = self.target.clone() else {
            return;
        };
        let Some(fmt) = self.fmt.clone() else {
            return;
        };
        let fmt_bold = self.fmt_bold.clone().unwrap_or_else(|| fmt.clone());

        unsafe { target.BeginDraw() };

        // Full background.
        unsafe { target.Clear(Some(&BG)) };

        let w = self.width;
        let split = self.split_y;

        // ── Transcript pane ──────────────────────────────────────────────
        unsafe {
            target.PushAxisAlignedClip(
                &D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: split },
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }
        // Transcript background.
        if let Ok(b) = unsafe { target.CreateSolidColorBrush(&TRANSCRIPT_BG, None) } {
            unsafe {
                target.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: w, bottom: split }, &b)
            };
        }

        self.paint_transcript(&target, &fmt, w, split);

        unsafe { target.PopAxisAlignedClip() };

        // ── Splitter ─────────────────────────────────────────────────────
        if let Ok(sb) = unsafe { target.CreateSolidColorBrush(&SPLITTER_BG, None) } {
            unsafe {
                target.FillRectangle(
                    &D2D_RECT_F { left: 0.0, top: split, right: w, bottom: split + SPLITTER_H },
                    &sb,
                )
            };
        }

        // ── Input pane ───────────────────────────────────────────────────
        let input_top = split + SPLITTER_H;
        unsafe {
            target.PushAxisAlignedClip(
                &D2D_RECT_F { left: 0.0, top: input_top, right: w, bottom: self.height },
                D2D1_ANTIALIAS_MODE_ALIASED,
            );
        }

        self.paint_input(&target, &fmt, &fmt_bold, w, input_top);

        unsafe { target.PopAxisAlignedClip() };

        // ── EndDraw ──────────────────────────────────────────────────────
        if let Err(e) = unsafe { target.EndDraw(None, None) } {
            eprintln!("[repl] EndDraw: {e}");
            self.target = None;
        }
    }

    fn paint_transcript(
        &self,
        target: &ID2D1HwndRenderTarget,
        fmt: &IDWriteTextFormat,
        width: f32,
        clip_h: f32,
    ) {
        let dw = &renderer::ctx().dwrite.factory;
        let x = MARGIN_H;
        let max_w = (width - x * 2.0).max(1.0);
        let mut y = MARGIN_TOP - self.scroll_y;

        for entry in &self.transcript {
            let color = match entry.kind {
                AppendKind::Banner => COL_BANNER,
                AppendKind::Input => COL_INPUT,
                AppendKind::Output => COL_OUTPUT,
                AppendKind::Error => COL_ERROR,
            };
            // Prepend prompt for user input lines.
            let display_text: std::borrow::Cow<str> = if entry.kind == AppendKind::Input {
                format!("ncl> {}", entry.text).into()
            } else {
                entry.text.as_str().into()
            };
            for line in display_text.split('\n') {
                let y_bottom = y + LINE_HEIGHT;
                if y_bottom > 0.0 && y < clip_h {
                    draw_text(target, dw, line, fmt, x, y, max_w, color);
                }
                y += LINE_HEIGHT;
            }
            y += 3.0; // inter-entry gap
        }
    }

    fn paint_input(
        &self,
        target: &ID2D1HwndRenderTarget,
        fmt: &IDWriteTextFormat,
        fmt_bold: &IDWriteTextFormat,
        width: f32,
        input_top: f32,
    ) {
        let dw = &renderer::ctx().dwrite.factory;
        let x = MARGIN_H;
        let max_w = (width - x * 2.0).max(1.0);
        let text_x = x + PROMPT_W;
        let text_max_w = (width - text_x - MARGIN_H).max(1.0);
        let lines: Vec<&str> = self.input.split('\n').collect();
        let num_lines = lines.len();

        let mut y = input_top + MARGIN_TOP;

        // Cursor logical line (0-indexed).
        let cursor_line_idx = self.input[..self.cursor_byte].matches('\n').count();

        for (i, line) in lines.iter().enumerate() {
            let prompt_str = if i == 0 { PROMPT } else { CONT_PROMPT };
            let prompt_color = if i == 0 { COL_PROMPT } else { COL_CONT };

            // Draw prompt.
            draw_text(target, dw, prompt_str, fmt_bold, x, y, PROMPT_W + 4.0, prompt_color);

            // Draw line text.
            draw_text(target, dw, line, fmt, text_x, y, text_max_w, COL_INPUT);

            // Draw cursor on the correct line.
            if self.has_focus && self.caret_on && i == cursor_line_idx {
                let col_start = if i == 0 {
                    0usize
                } else {
                    // find the byte offset of the start of this line
                    self.input[..self.cursor_byte]
                        .rfind('\n')
                        .map(|p| p + 1)
                        .unwrap_or(0)
                };
                let col_bytes = self.cursor_byte - col_start;
                let col_chars = self.input[col_start..col_start + col_bytes].chars().count();
                let cx = text_x + col_chars as f32 * self.cell_w;
                if let Ok(cb) = unsafe { target.CreateSolidColorBrush(&COL_CURSOR, None) } {
                    unsafe {
                        target.FillRectangle(
                            &D2D_RECT_F {
                                left: cx,
                                top: y + 2.0,
                                right: cx + 2.0,
                                bottom: y + LINE_HEIGHT - 2.0,
                            },
                            &cb,
                        )
                    };
                }
            }

            y += LINE_HEIGHT;
        }

        // If cursor is at the very end and input is empty or cursor == len,
        // ensure the cursor is always visible even on an empty last line.
        if num_lines == 0 && self.has_focus && self.caret_on {
            let cx = text_x;
            if let Ok(cb) = unsafe { target.CreateSolidColorBrush(&COL_CURSOR, None) } {
                unsafe {
                    target.FillRectangle(
                        &D2D_RECT_F {
                            left: cx,
                            top: input_top + MARGIN_TOP + 2.0,
                            right: cx + 2.0,
                            bottom: input_top + MARGIN_TOP + LINE_HEIGHT - 2.0,
                        },
                        &cb,
                    )
                };
            }
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Draw a single line of text at (x, y) using a pre-built DWrite layout.
fn draw_text(
    target: &ID2D1HwndRenderTarget,
    dw: &windows::Win32::Graphics::DirectWrite::IDWriteFactory2,
    text: &str,
    fmt: &IDWriteTextFormat,
    x: f32,
    y: f32,
    max_w: f32,
    color: D2D1_COLOR_F,
) {
    if text.is_empty() {
        return;
    }
    let text_w: Vec<u16> = text.encode_utf16().collect();
    let layout = match unsafe {
        dw.CreateTextLayout(&text_w, fmt, max_w, LINE_HEIGHT * 2.0)
    } {
        Ok(l) => l,
        Err(_) => return,
    };
    let brush = match unsafe { target.CreateSolidColorBrush(&color, None) } {
        Ok(b) => b,
        Err(_) => return,
    };
    unsafe {
        target.DrawTextLayout(
            Vector2 { X: x, Y: y },
            &layout,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
        )
    };
}

/// Measure the advance width of a single 'm' to calibrate CELL_W.
fn measure_char_width(
    fmt: &IDWriteTextFormat,
    dw: &windows::Win32::Graphics::DirectWrite::IDWriteFactory2,
) -> Option<f32> {
    let text_w: Vec<u16> = "m".encode_utf16().collect();
    let layout = unsafe { dw.CreateTextLayout(&text_w, fmt, 1000.0, 1000.0) }.ok()?;
    let mut metrics = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
    unsafe { layout.GetMetrics(&mut metrics) }.ok()?;
    Some(metrics.width)
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    let mut i = pos + 1;
    while i <= s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Whether the cursor is on the up/down keys at the first/last line.
fn cursor_on_first_line(input: &str, cursor_byte: usize) -> bool {
    !input[..cursor_byte].contains('\n')
}
fn cursor_on_last_line(input: &str, cursor_byte: usize) -> bool {
    !input[cursor_byte..].contains('\n')
}

// ─── Form completeness check ──────────────────────────────────────────────────

#[derive(PartialEq)]
enum FormStatus {
    Complete,
    Incomplete,
    InString,
}

/// Cheap paren-balance / string scan to decide if a Lisp form is
/// complete. Does not understand all reader syntax but handles the
/// common cases (strings, line comments, #|...|#, character literals).
fn form_complete(s: &str) -> FormStatus {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return FormStatus::Complete;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut prev = '\0';
    let mut chars = trimmed.chars().peekable();

    while let Some(c) = chars.next() {
        // Block comment #| ... |#
        if in_block_comment {
            if prev == '|' && c == '#' {
                in_block_comment = false;
            }
            prev = c;
            continue;
        }
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            prev = c;
            continue;
        }
        if in_string {
            if prev != '\\' && c == '"' {
                in_string = false;
            }
            // Handle \\ escape (even number of backslashes = not escaped)
            prev = if c == '\\' && prev == '\\' { '\0' } else { c };
            continue;
        }
        match c {
            ';' => {
                in_line_comment = true;
            }
            '"' => {
                in_string = true;
            }
            '#' if chars.peek() == Some(&'|') => {
                let _ = chars.next();
                in_block_comment = true;
            }
            '#' if chars.peek() == Some(&'\\') => {
                // Character literal: consume next char, skip it.
                let _ = chars.next();
                let _ = chars.next();
            }
            '(' | '[' => {
                depth += 1;
            }
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return FormStatus::Complete; // unmatched close → submit
                }
            }
            _ => {}
        }
        prev = c;
    }

    if in_string {
        FormStatus::InString
    } else if depth > 0 {
        FormStatus::Incomplete
    } else {
        FormStatus::Complete
    }
}

/// Compute the indentation for the next line after `before` (text up
/// to the cursor).  Matches the leading whitespace of the most recent
/// unclosed `(` line, plus 2 spaces.
fn compute_indent(before: &str) -> String {
    // Find the most recent line.
    let last_line = before.rfind('\n').map(|i| &before[i + 1..]).unwrap_or(before);
    let leading: String = last_line.chars().take_while(|c| c.is_whitespace()).collect();
    // If the last non-whitespace char is '(' add extra 2 spaces.
    let extra = if last_line.trim_end().ends_with('(') {
        "  "
    } else {
        ""
    };
    format!("{leading}{extra}")
}

// ─── Key / char helpers ───────────────────────────────────────────────────────

fn key_down(vk: u16) -> bool {
    (unsafe { GetKeyState(vk as i32) } as i16) < 0
}

// ─── WndProc ──────────────────────────────────────────────────────────────────

unsafe extern "system" fn repl_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // Recover child_id from MDICREATESTRUCTW.lParam.
        use windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
        let cs = lparam.0 as *const CREATESTRUCTW;
        if cs.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let mdi_cs = unsafe { (*cs).lpCreateParams as *const MDICREATESTRUCTW };
        let child_id = if !mdi_cs.is_null() {
            let bootstrap_ptr = unsafe { (*mdi_cs).lParam.0 as *mut ReplBootstrap };
            if !bootstrap_ptr.is_null() {
                let b = unsafe { Box::from_raw(bootstrap_ptr) };
                b.child_id
            } else {
                registry::allocate_child_id()
            }
        } else {
            registry::allocate_child_id()
        };

        let state = ReplState::new(child_id, hwnd);
        let raw = Box::into_raw(state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        register_hwnd(child_id, hwnd);
        registry::register(child_id, hwnd, hwnd);

        // Start caret blink timer.
        let _ = unsafe { SetTimer(Some(hwnd), CARET_TIMER_ID, CARET_BLINK_MS, None) };

        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReplState;
    if raw.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *raw };

    match msg {
        WM_CLOSE => {
            // Destroy directly; don't delegate to DefMDIChildProcW which
            // cascades WM_CLOSE to the frame when this is the last child.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }

        WM_NCDESTROY => {
            let _ = unsafe { KillTimer(Some(hwnd), CARET_TIMER_ID) };
            channels::push(IGuiEvent::Close {
                child_id: state.child_id,
            });
            unregister_hwnd(state.child_id);
            registry::unregister(state.child_id);
            // Drain any remaining pending to avoid leaks.
            {
                let mut map = pending_inputs().lock().unwrap_or_else(|p| p.into_inner());
                map.remove(&state.child_id);
            }
            {
                let mut map = pending_outputs().lock().unwrap_or_else(|p| p.into_inner());
                map.remove(&state.child_id);
            }
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            let _ = unsafe { Box::from_raw(raw) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }

        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let _hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            state.paint();
            unsafe { EndPaint(hwnd, &ps) };
            LRESULT(0)
        }

        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as f32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as f32;
            state.handle_resize(w, h);
            channels::push(IGuiEvent::Resize {
                child_id: state.child_id,
                width: w as i64,
                height: h as i64,
            });
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }

        WM_SETFOCUS => {
            state.has_focus = true;
            state.caret_on = true;
            unsafe { InvalidateRect(Some(hwnd), None, false) };
            LRESULT(0)
        }

        WM_KILLFOCUS => {
            state.has_focus = false;
            unsafe { InvalidateRect(Some(hwnd), None, false) };
            channels::push(IGuiEvent::Focus {
                child_id: state.child_id,
                gained: false,
            });
            LRESULT(0)
        }

        WM_TIMER if wparam.0 == CARET_TIMER_ID => {
            state.caret_on = !state.caret_on;
            // Only invalidate the input pane area to keep it cheap.
            unsafe { InvalidateRect(Some(hwnd), None, false) };
            LRESULT(0)
        }

        WM_REPL_FLUSH => {
            state.drain_pending();
            unsafe { InvalidateRect(Some(hwnd), None, false) };
            LRESULT(0)
        }

        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u16;
            let ctrl = key_down(VK_CONTROL.0);
            let shift = key_down(VK_SHIFT.0);
            let mut handled = true;
            state.caret_on = true;

            if vk == VK_RETURN.0 {
                if shift {
                    // Shift+Enter → force newline without submission check.
                    state.insert_char('\n');
                } else if let Some(submitted) = state.try_submit() {
                    // Push to pending inputs so the worker can retrieve it.
                    pending_inputs()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .entry(state.child_id)
                        .or_default()
                        .push_back(submitted);
                    // Notify the Lisp worker.
                    channels::push(IGuiEvent::ReplSubmit {
                        child_id: state.child_id,
                    });
                }
            } else if vk == VK_BACK.0 {
                state.delete_back();
            } else if vk == VK_DELETE.0 {
                state.delete_forward();
            } else if vk == VK_LEFT.0 {
                if ctrl {
                    state.move_word_left();
                } else {
                    state.move_left(shift);
                }
            } else if vk == VK_RIGHT.0 {
                if ctrl {
                    state.move_word_right();
                } else {
                    state.move_right(shift);
                }
            } else if vk == VK_UP.0 {
                if cursor_on_first_line(&state.input, state.cursor_byte) {
                    state.history_up();
                } else {
                    // TODO: move cursor up a visual line (Phase 2)
                    state.move_home();
                }
            } else if vk == VK_DOWN.0 {
                if cursor_on_last_line(&state.input, state.cursor_byte) {
                    state.history_down();
                } else {
                    // TODO: move cursor down a visual line (Phase 2)
                    state.move_end();
                }
            } else if vk == VK_HOME.0 {
                state.move_home();
            } else if vk == VK_END.0 {
                state.move_end();
            } else if vk == VK_PRIOR.0 {
                // Page up in transcript.
                let page = (state.split_y - MARGIN_TOP).max(1.0);
                state.scroll_y = (state.scroll_y - page).max(0.0);
            } else if vk == VK_NEXT.0 {
                // Page down in transcript.
                let page = (state.split_y - MARGIN_TOP).max(1.0);
                let max_scroll = (state.transcript_total_height()
                    - (state.split_y - MARGIN_TOP))
                    .max(0.0);
                state.scroll_y = (state.scroll_y + page).min(max_scroll);
            } else if ctrl && vk == b'A' as u16 {
                state.cursor_byte = 0;
            } else if ctrl && vk == b'E' as u16 {
                state.cursor_byte = state.input.len();
            } else if ctrl && vk == b'K' as u16 {
                // Kill to end of line.
                if let Some(nl) = state.input[state.cursor_byte..].find('\n') {
                    let end = state.cursor_byte + nl;
                    state.input.drain(state.cursor_byte..end);
                } else {
                    state.input.truncate(state.cursor_byte);
                }
            } else if ctrl && vk == b'U' as u16 {
                // Kill to start of line.
                let line_start = state.input[..state.cursor_byte]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                state.input.drain(line_start..state.cursor_byte);
                state.cursor_byte = line_start;
            } else if ctrl && vk == b'L' as u16 {
                // Clear transcript.
                state.transcript.clear();
                state.scroll_y = 0.0;
            } else {
                handled = false;
            }

            if handled {
                unsafe { InvalidateRect(Some(hwnd), None, false) };
                LRESULT(0)
            } else {
                unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
            }
        }

        WM_CHAR => {
            let cp = wparam.0 as u32;
            // Ignore control characters (handled in WM_KEYDOWN above).
            if cp >= 0x20 && cp != 0x7F {
                if let Some(c) = char::from_u32(cp) {
                    state.insert_char(c);
                    state.caret_on = true;
                    unsafe { InvalidateRect(Some(hwnd), None, false) };
                }
            }
            LRESULT(0)
        }

        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}
