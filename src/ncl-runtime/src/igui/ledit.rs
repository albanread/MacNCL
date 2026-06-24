//! ledit — fail-safe Lisp-aware editor.
//!
//! A minimal text editor that lives entirely on the UI thread of
//! the iGui frame. It does not consume `SurfaceCmd` batches, does
//! not touch the language-thread mailbox, and does not depend on
//! any Lisp code being loaded. The point is that even when the
//! rest of NewCormanLisp has a fault, the editor remains
//! responsive so the user can fix source files and reload them.
//!
//! Inherited verbatim from the sister NewCP repo's `redit` (which
//! served the same role for Component Pascal). Renamed to ledit
//! and growing a small set of Lisp-aware affordances:
//!
//!   - **auto-indent on Enter** — the new line gets the current
//!     line's leading whitespace, plus an extra two spaces if the
//!     cursor was preceded by an unmatched `(` on this line.
//!   - **paren-balance in the status line** — a count of open vs
//!     close parens so you can see at a glance whether an editing
//!     buffer is well-formed.
//!
//! Real structural editing (slurp / barf / wrap / unwrap, balanced
//! cursor motion, etc.) is deferred — it'll land as user-Lisp
//! once we have enough of an editor API to drive from outside.
//!
//! Architecture: a single-instance MDI child with its own WndProc
//! that handles WM_PAINT (Direct2D + DirectWrite, fixed grid) and
//! all input directly. State is heap-allocated on first
//! `WM_NCCREATE` and stored in `GWLP_USERDATA`.
//!
//! R1 scope:
//!   - open / save (Win32 common dialogs)
//!   - basic editing keys (arrows, Home/End, PgUp/PgDn, Enter,
//!     Backspace, Delete, Tab, printable chars)
//!   - mouse click to position cursor
//!   - vertical wheel scroll
//!   - line numbers in a left gutter
//!   - status line at bottom
//!   - no selection, no clipboard, no undo, no syntax colour, no
//!     compiler hookup (those land in R2/R3/R4)

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::sync::Mutex;

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
    DWRITE_FONT_WEIGHT, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, SetFocus, VK_DELETE, VK_DOWN, VK_END,
    VK_F5, VK_F7, VK_F8,
    VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
};

/// `WM_MOUSEMOVE`'s `wparam` low word; bit 0 = left button held. The
/// windows-rs constant lives in different modules across versions, so
/// we use the well-known winuser.h value directly.
const MK_LBUTTON: u32 = 0x0001;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, IsWindow, LoadCursorW,
    PostQuitMessage, RegisterClassExW, SendMessageW, SetWindowLongPtrW, CW_USEDEFAULT,
    GWLP_USERDATA, IDC_IBEAM, MDICREATESTRUCTW, WHEEL_DELTA, WM_CHAR, WM_COMMAND,
    WM_DPICHANGED_AFTERPARENT, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MDIACTIVATE, WM_MDICREATE, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN,
    WNDCLASSEXW, WNDCLASS_STYLES,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use super::renderer;
use super::rope_buffer::{codepoints_to_utf8, RopeBuffer};

/// WM_COMMAND id for the "Tools > ledit" frame menu entry. Outside
/// the user range (0x1000..=0x1FFF) and the MDI verb range
/// (0x2000..=0x2010) so it can never collide with a language-thread
/// menu spec.
pub const MENU_CMD_ID: u16 = 0x3000;

/// Edit-menu command IDs, dispatched to the active ledit MDI child
/// via frame WM_COMMAND forwarding. The range is contiguous so the
/// frame can recognise them with a single `(0x3100..=0x31FF)` check.
pub const EDIT_CMD_BASE: u16 = 0x3100;
pub const EDIT_CMD_END: u16 = 0x31FF;

pub const EDIT_CMD_UNDO: u16 = 0x3100;
pub const EDIT_CMD_REDO: u16 = 0x3101;
pub const EDIT_CMD_CUT: u16 = 0x3110;
pub const EDIT_CMD_COPY: u16 = 0x3111;
pub const EDIT_CMD_PASTE: u16 = 0x3112;
pub const EDIT_CMD_SELECT_ALL: u16 = 0x3113;
pub const EDIT_CMD_FORWARD_SEXP: u16 = 0x3120;
pub const EDIT_CMD_BACKWARD_SEXP: u16 = 0x3121;
pub const EDIT_CMD_SLURP_FORWARD: u16 = 0x3122;
pub const EDIT_CMD_BARF_FORWARD: u16 = 0x3123;
pub const EDIT_CMD_WRAP: u16 = 0x3130;
pub const EDIT_CMD_SPLICE: u16 = 0x3131;
pub const EDIT_CMD_RAISE: u16 = 0x3132;
/// Run-buffer: push the buffer (or selection) at the language
/// thread as an `EvalBuffer` event. The default event-loop handler
/// in `events.lisp` calls `eval-string` and writes the result to
/// the iGui log overlay.
pub const EDIT_CMD_RUN_BUFFER: u16 = 0x3140;
/// Run-form-at-point: extract the top-level form the cursor is
/// currently inside and push just that as an `EvalBuffer` event.
/// The canonical CL interactive-development action — Emacs's
/// `C-M-x`, equivalent of "evaluate the defun under the cursor."
pub const EDIT_CMD_RUN_FORM: u16 = 0x3141;

/// File-menu command IDs. Dispatched at the frame level; the frame
/// calls `ledit::do_file_cmd` which delegates to state methods.
/// Range is intentionally tight so the frame can range-check cheaply.
pub const FILE_CMD_BASE:    u16 = 0x3200;
pub const FILE_CMD_END:     u16 = 0x320F;
pub const FILE_CMD_NEW:     u16 = 0x3200;
pub const FILE_CMD_OPEN:    u16 = 0x3201;
pub const FILE_CMD_SAVE:    u16 = 0x3202;
pub const FILE_CMD_SAVE_AS: u16 = 0x3203;
pub const FILE_CMD_EXIT:    u16 = 0x3204;

const LEDIT_CLASS: PCWSTR = w!("NewCL.iGui.Ledit");
const TITLE_NEW: PCWSTR = w!("ledit — untitled");

/// HWND of the singleton ledit MDI child, if one exists. Used to
/// activate the existing instance instead of creating a second one.
static LEDIT_HWND: Mutex<Option<isize>> = Mutex::new(None);

// ─── Compile-check injection point ──────────────────────────────────
//
// The runtime crate sits below `newcp-parser` and `newcp-sema` in the
// dependency graph, so it cannot import them directly. Instead the
// driver (which already depends on both) hands ledit a closure that
// runs a check and returns diagnostics. This keeps the layering
// clean and lets us swap in different checkers (e.g. a fast
// parse-only check vs full semantic) later.

/// One diagnostic from the compile-check pass. Lines and columns are
/// 1-indexed to match what shows up in the status bar.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

type CheckFn = Box<dyn Fn(&str) -> Vec<Diagnostic> + Send + Sync + 'static>;

static CHECKER: Mutex<Option<CheckFn>> = Mutex::new(None);

/// Install a closure that takes the editor's full text and returns
/// diagnostics. Call once at startup, before `iGui::run`.
pub fn install_checker<F>(f: F)
where
    F: Fn(&str) -> Vec<Diagnostic> + Send + Sync + 'static,
{
    *CHECKER.lock().expect("CHECKER poisoned") = Some(Box::new(f));
}

fn run_checker(source: &str) -> Option<Vec<Diagnostic>> {
    let guard = CHECKER.lock().expect("CHECKER poisoned");
    let f = guard.as_ref()?;
    Some(f(source))
}

// ─── Public API ──────────────────────────────────────────────────────

/// Register the ledit MDI child WndClass. Called from
/// `child::register_classes`.
pub fn register_class() -> Result<(), super::IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (ledit): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_IBEAM) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (ledit): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(ledit_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: LEDIT_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

// The Tools menu and frame accelerator table now live in
// `tools_menu`, which knows about both ledit and the log view.

/// Open the ledit child, or activate it if already open. Called from
/// the frame WndProc when the user picks the menu item or hits the
/// shortcut. UI-thread only.
pub fn open(frame: HWND, mdi_client: HWND) {
    if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
        let hwnd = HWND(raw as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                SendMessageW(
                    mdi_client,
                    windows::Win32::UI::WindowsAndMessaging::WM_MDIACTIVATE,
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
            eprintln!("[ledit] GetModuleHandleW: {e}");
            return;
        }
    };
    let create = MDICREATESTRUCTW {
        szClass: LEDIT_CLASS,
        szTitle: TITLE_NEW,
        hOwner: h_instance,
        x: CW_USEDEFAULT,
        y: CW_USEDEFAULT,
        cx: CW_USEDEFAULT,
        cy: CW_USEDEFAULT,
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
        eprintln!("[ledit] WM_MDICREATE returned 0");
        return;
    }
    let _ = frame; // reserved for future use
}

/// Load `text` into the ledit buffer as if the user opened a file.
/// Opens or activates ledit first.  Called on the UI (GUI) thread by
/// the Demos menu handler in the frame WndProc.
pub fn load_content(frame: HWND, mdi_client: HWND, text: String, path: Option<PathBuf>) {
    open(frame, mdi_client); // ensure ledit window exists
    if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
        let hwnd = HWND(raw as *mut _);
        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
        if !state_ptr.is_null() {
            unsafe { &mut *state_ptr }.load_text(text, path);
        }
    }
}

/// Dispatch a File-menu command (FILE_CMD_*) to the ledit state.
/// Called by the frame WndProc.  Returns `true` if `cmd_id` was
/// handled (regardless of whether a ledit window exists).
pub fn do_file_cmd(cmd_id: u16, frame: HWND, mdi_client: HWND) -> bool {
    match cmd_id {
        FILE_CMD_NEW => {
            open(frame, mdi_client);
            if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
                let hwnd = HWND(raw as *mut _);
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
                if !state_ptr.is_null() {
                    unsafe { &mut *state_ptr }.file_new();
                }
            }
            true
        }
        FILE_CMD_OPEN => {
            open(frame, mdi_client);
            if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
                let hwnd = HWND(raw as *mut _);
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
                if !state_ptr.is_null() {
                    unsafe { &mut *state_ptr }.file_open();
                }
            }
            true
        }
        FILE_CMD_SAVE => {
            if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
                let hwnd = HWND(raw as *mut _);
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
                if !state_ptr.is_null() {
                    unsafe { &mut *state_ptr }.file_save();
                }
            }
            true
        }
        FILE_CMD_SAVE_AS => {
            if let Some(raw) = *LEDIT_HWND.lock().expect("LEDIT_HWND poisoned") {
                let hwnd = HWND(raw as *mut _);
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
                if !state_ptr.is_null() {
                    unsafe { &mut *state_ptr }.file_save_as();
                }
            }
            true
        }
        FILE_CMD_EXIT => {
            unsafe { PostQuitMessage(0) };
            true
        }
        _ => false,
    }
}

// ─── State ───────────────────────────────────────────────────────────

/// Cursor / anchor / selection bounds are all code-point offsets
/// into the rope. (row, col) is a presentation concept used only by
/// the renderer (which paints by visible row) and the status bar
/// (which displays `L:C` for the user). Everything else speaks
/// offsets.
type Pos = usize;

/// Edit operation for undo/redo. Stored after the edit has been
/// applied; `cursor_after` is the cursor at that point and
/// `cursor_before` is what it was before.
///
/// Offset-canonical. The Zig/Vec<String>-era variants carried
/// `(start, end)` as `(row, col)` pairs and a `String` payload;
/// here the payload is the raw code-point sequence so we can
/// re-splice it into the rope without any UTF-8/UTF-32 round trips
/// or row/col arithmetic.
#[derive(Clone, Debug)]
enum UndoOp {
    /// `text` was inserted at offset `start`, extending to
    /// `start + text.len()`. Reverse: delete `text.len()` code
    /// points at `start`.
    Inserted {
        start: usize,
        text: Vec<u32>,
        cursor_before: usize,
        cursor_after: usize,
    },
    /// `text` was deleted starting at offset `start`. Reverse:
    /// insert `text` at `start`. Covers backspace, delete-forward,
    /// and selection deletion (cut, replace-on-typing).
    Deleted {
        start: usize,
        text: Vec<u32>,
        cursor_before: usize,
        cursor_after: usize,
    },
    /// At offset `start`, `removed` was replaced by `inserted` in
    /// one atomic step. Used by paredit ops (slurp / barf / wrap
    /// / splice / raise) which logically modify the buffer in a
    /// single move and want a single Ctrl-Z to undo. Reverse:
    /// replace `inserted` with `removed` at `start`.
    Replaced {
        start: usize,
        removed: Vec<u32>,
        inserted: Vec<u32>,
        cursor_before: usize,
        cursor_after: usize,
    },
}

/// Coalescing hint set after a single-char edit so the next edit of
/// the same kind at the contiguous position can extend the previous
/// undo entry instead of pushing a new one. Cleared on movement,
/// click, paste, undo, redo, or any non-coalescing edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoalesceKind {
    Insert,
    Backspace,
}

const UNDO_CAP: usize = 1024;

/// Visual width of a tab stop in cells. The buffer keeps tabs as-is
/// (so files round-trip cleanly on save); the renderer and the
/// click-mapping use this constant to translate between buffer char
/// positions and on-screen cell columns. CP / BlackBox convention
/// is tab-width 2 (a 15-tab continuation indent in `Strings.cp`
/// aligns to ~30 cells, matching the opening paren of the line
/// above), so that's what we default to.
const TAB_WIDTH: usize = 2;

struct ReditState {
    hwnd: HWND,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,

    /// Canonical text storage. UTF-32 code points in an AVL-balanced
    /// rope; see `igui/rope_buffer.rs`. All positions in `cursor`,
    /// `anchor`, and the `UndoOp` records are code-point offsets
    /// into this rope.
    buffer: RopeBuffer,
    /// Cursor as a code-point offset. The (row, col) form is derived
    /// on demand via `buffer.offset_to_line_col(cursor)`.
    cursor: usize,
    /// Selection anchor. `anchor == cursor` ⇒ no active selection.
    anchor: usize,
    /// Preferred column for vertical motion. A code-point column.
    /// Set whenever the cursor moves horizontally; preserved across
    /// up/down so traversing short lines doesn't lose the target.
    pref_col: usize,
    /// Top visible row. Viewport positioning is a row concept, not
    /// an offset one — kept as a row index.
    scroll_top: usize,

    file_path: Option<PathBuf>,
    dirty: bool,

    client_w: u32,
    client_h: u32,
    /// Per-monitor DPI of the current monitor. Cached so we can avoid
    /// asking Win32 every paint, refreshed on `WM_DPICHANGED_AFTERPARENT`.
    dpi: u32,

    /// True while the user is dragging the mouse with the left button
    /// held. We capture the mouse so drags that leave the client area
    /// still extend the selection cleanly.
    selecting_drag: bool,

    undo: Vec<UndoOp>,
    redo: Vec<UndoOp>,
    coalesce: Option<CoalesceKind>,

    /// Per-line tokens for syntax highlighting. Lazily refreshed in
    /// paint() when `tokens_dirty` is set. Re-tokenizing the whole
    /// buffer on every edit is fine for the sub-MB files ledit is
    /// designed to handle.
    tokens: Vec<Vec<Token>>,
    tokens_dirty: bool,

    /// Diagnostics from the most recent compile check. Cleared when
    /// the buffer is edited (so stale errors don't lie to the user)
    /// and refreshed on F7 / after-save.
    diagnostics: Vec<Diagnostic>,
    /// True when the buffer has changed since the last check, so the
    /// status bar can show "(stale)" instead of pretending the
    /// diagnostics still apply.
    diagnostics_stale: bool,
}

impl ReditState {
    fn new(hwnd: HWND) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            target: None,
            text_format: None,
            cell_w: 8.0,
            cell_h: 16.0,
            ascent: 12.0,
            buffer: RopeBuffer::new(),
            cursor: 0,
            anchor: 0,
            pref_col: 0,
            scroll_top: 0,
            file_path: None,
            dirty: false,
            client_w: 0,
            client_h: 0,
            dpi,
            selecting_drag: false,
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: None,
            tokens: Vec::new(),
            tokens_dirty: true,
            diagnostics: Vec::new(),
            diagnostics_stale: true,
        }
    }

    fn ensure_resources(&mut self, w: u32, h: u32) {
        if self.text_format.is_none() {
            self.text_format = create_text_format();
            if let Some(fmt) = self.text_format.as_ref() {
                if let Some((cw, ch, asc)) = measure_cell(fmt) {
                    self.cell_w = cw;
                    self.cell_h = ch;
                    self.ascent = asc;
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
            Err(e) => eprintln!("[ledit] CreateHwndRenderTarget failed: {e}"),
        }
    }

    /// Apply a new monitor DPI: drop the current render target so the
    /// next paint recreates it at the new DPI, and re-measure the
    /// cell. Called from `WM_DPICHANGED_AFTERPARENT`.
    fn set_dpi(&mut self, dpi: u32) {
        if dpi == 0 || dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        // Drop the render target so it gets rebuilt with new dpiX/Y.
        // The text format is dpi-independent (sizes are DIPs) but the
        // measured cell rounds against pixel boundaries, so refresh
        // it on the next paint.
        self.target = None;
        if let Some(fmt) = self.text_format.as_ref() {
            if let Some((cw, ch, asc)) = measure_cell(fmt) {
                self.cell_w = cw;
                self.cell_h = ch;
                self.ascent = asc;
            }
        }
        self.invalidate();
    }

    fn invalidate(&self) {
        let _ = unsafe { InvalidateRect(Some(self.hwnd), None, false) };
    }

    /// Pixels-to-DIPs scale factor for the current monitor. The
    /// render target is configured with the monitor's DPI, so all
    /// drawing math runs in DIPs while Win32 hands us pixel
    /// dimensions and pixel-space mouse coordinates. This converts
    /// at the boundary.
    fn dip_scale(&self) -> f32 {
        if self.dpi == 0 {
            1.0
        } else {
            96.0 / (self.dpi as f32)
        }
    }

    fn px_to_dip(&self, px: i32) -> f32 {
        (px as f32) * self.dip_scale()
    }

    // ─── Selection / cursor helpers ──────────────────────────────

    /// Decode the canonical cursor offset to a (row, col) for the
    /// few places that genuinely need it (the renderer's paint loop
    /// and the status bar's `L:C` display). The rope answers this in
    /// O(log n).
    fn cursor_rc(&self) -> (usize, usize) {
        self.buffer.offset_to_line_col(self.cursor)
    }

    fn cursor_row(&self) -> usize {
        self.cursor_rc().0
    }

    fn cursor_col(&self) -> usize {
        self.cursor_rc().1
    }

    /// `Some((lo, hi))` if there's an active selection, else `None`.
    /// Offsets satisfy `lo <= hi`.
    fn selection_range(&self) -> Option<(usize, usize)> {
        if self.cursor == self.anchor {
            None
        } else if self.anchor < self.cursor {
            Some((self.anchor, self.cursor))
        } else {
            Some((self.cursor, self.anchor))
        }
    }

    /// Move the cursor to an offset and either extend the selection
    /// (anchor stays) or collapse it (anchor follows).
    fn set_cursor_offset(&mut self, offset: usize, extend: bool) {
        let clamped = offset.min(self.buffer.len());
        self.cursor = clamped;
        if !extend {
            self.anchor = clamped;
        }
        self.coalesce = None;
        self.ensure_cursor_visible();
        self.invalidate();
    }

    /// Move the cursor to (row, col). Clamps row to last line and
    /// col to that line's length. Used by motion methods that
    /// naturally think in (row, col) — vertical motion, click-to-
    /// position, page up/down.
    fn set_cursor_rc(&mut self, row: usize, col: usize, extend: bool) {
        let last_row = self.buffer.line_count().saturating_sub(1);
        let r = row.min(last_row);
        let line_len = self.line_len_cps(r);
        let c = col.min(line_len);
        let offset = self.buffer.line_col_to_offset(r, c);
        self.set_cursor_offset(offset, extend);
    }

    /// Code-point length of the row's content (not including any
    /// trailing newline).
    fn line_len_cps(&self, row: usize) -> usize {
        match self.buffer.line_range(row) {
            Some((s, e)) => e - s,
            None => 0,
        }
    }

    /// Fetch the row's content as UTF-8. Used by the tokenizer,
    /// auto-indent, paren balance, and tab-expansion helpers — all
    /// of which want a `&str`. The encode is O(line length); paint
    /// does this once per visible row.
    fn line_text(&self, row: usize) -> String {
        codepoints_to_utf8(&self.buffer.get_line(row))
    }

    /// Extract the text inside the current selection.
    fn selected_text(&self) -> String {
        let Some((lo, hi)) = self.selection_range() else {
            return String::new();
        };
        codepoints_to_utf8(&self.buffer.slice(lo, hi))
    }

    // ─── Mutation primitives (no undo bookkeeping) ───────────────
    //
    // The rope handles cross-line work atomically: `insert` and
    // `delete` are single operations regardless of whether the
    // affected range straddles line boundaries. The old
    // `splice_in` / `splice_out` (which manually edited a
    // `Vec<String>` line by line) collapse to one call each.

    /// Insert `text` (as UTF-8) at `offset`, returning the offset
    /// just after the inserted text. CRLF and lone CR are
    /// normalised to LF (the rope does this on `insert_utf8`).
    fn splice_in_utf8(&mut self, offset: usize, text: &str) -> usize {
        if text.is_empty() {
            return offset;
        }
        // Pre-count code points so we can return the post-insert
        // offset without having to re-derive it. We use the same
        // normalisation the rope applies internally.
        let cps = super::rope_buffer::utf8_to_codepoints(text.as_bytes());
        self.tokens_dirty = true;
        self.diagnostics_stale = true;
        self.buffer.insert(offset, &cps);
        offset + cps.len()
    }

    /// Insert raw code points at `offset`. Used by undo replay where
    /// the recorded payload is already `Vec<u32>`.
    fn splice_in_cps(&mut self, offset: usize, text: &[u32]) -> usize {
        if text.is_empty() {
            return offset;
        }
        self.tokens_dirty = true;
        self.diagnostics_stale = true;
        self.buffer.insert(offset, text);
        offset + text.len()
    }

    /// Delete the range `[lo, hi)` and return the removed text as
    /// raw code points (for the undo record).
    fn splice_out(&mut self, lo: usize, hi: usize) -> Vec<u32> {
        if lo >= hi {
            return Vec::new();
        }
        self.tokens_dirty = true;
        self.diagnostics_stale = true;
        let removed = self.buffer.slice(lo, hi);
        self.buffer.delete(lo, hi - lo);
        removed
    }

    /// Apply an insert and push a coalescible-or-fresh `Inserted`
    /// undo entry. `coalesce = Some(Insert)` enables typing
    /// coalescence; `None` for paste / newline / programmatic.
    fn do_insert(&mut self, text: &str, coalesce: Option<CoalesceKind>) {
        // Active selection: delete it first (one combined history
        // entry — Deleted + Inserted, replayed in that order on
        // undo).
        if self.selection_range().is_some() {
            self.delete_selection_to_undo();
        }
        let cursor_before = self.cursor;
        let start = cursor_before;
        let cps = super::rope_buffer::utf8_to_codepoints(text.as_bytes());
        if cps.is_empty() {
            return;
        }
        let end = self.splice_in_cps(start, &cps);
        self.cursor = end;
        self.anchor = end;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.redo.clear();

        // Coalesce contiguous single-character inserts into one undo
        // entry so typing a word is one Ctrl-Z away.
        let extend_last = coalesce == Some(CoalesceKind::Insert)
            && self.coalesce == Some(CoalesceKind::Insert)
            && matches!(
                self.undo.last(),
                Some(UndoOp::Inserted { start: prev_start, text: prev_text, .. })
                    if prev_start + prev_text.len() == start
            );
        if extend_last {
            if let Some(UndoOp::Inserted {
                text: prev_text,
                cursor_after,
                ..
            }) = self.undo.last_mut()
            {
                prev_text.extend_from_slice(&cps);
                *cursor_after = end;
            }
        } else {
            self.push_undo(UndoOp::Inserted {
                start,
                text: cps,
                cursor_before,
                cursor_after: end,
            });
        }
        self.coalesce = coalesce;
        self.ensure_cursor_visible();
        self.invalidate();
    }

    /// Delete the current selection. Pushes a Deleted entry. Caller
    /// is responsible for clearing the redo stack if appropriate.
    fn delete_selection_to_undo(&mut self) -> bool {
        let Some((lo, hi)) = self.selection_range() else {
            return false;
        };
        let cursor_before = self.cursor;
        let removed = self.splice_out(lo, hi);
        self.cursor = lo;
        self.anchor = lo;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.push_undo(UndoOp::Deleted {
            start: lo,
            text: removed,
            cursor_before,
            cursor_after: lo,
        });
        self.coalesce = None;
        true
    }

    fn push_undo(&mut self, op: UndoOp) {
        self.undo.push(op);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) {
        let Some(op) = self.undo.pop() else { return };
        let restore_cursor: usize;
        let mirror: UndoOp;
        match op {
            UndoOp::Inserted {
                start,
                text,
                cursor_before,
                cursor_after,
            } => {
                let len = text.len();
                self.splice_out(start, start + len);
                restore_cursor = cursor_before;
                mirror = UndoOp::Inserted {
                    start,
                    text,
                    cursor_before,
                    cursor_after,
                };
            }
            UndoOp::Deleted {
                start,
                text,
                cursor_before,
                cursor_after,
            } => {
                self.splice_in_cps(start, &text);
                restore_cursor = cursor_before;
                mirror = UndoOp::Deleted {
                    start,
                    text,
                    cursor_before,
                    cursor_after,
                };
            }
            UndoOp::Replaced {
                start,
                removed,
                inserted,
                cursor_before,
                cursor_after,
            } => {
                // Currently the buffer holds `inserted` at `start`.
                // Swap it back for `removed`.
                let hi = start + inserted.len();
                self.splice_out(start, hi);
                self.splice_in_cps(start, &removed);
                restore_cursor = cursor_before;
                mirror = UndoOp::Replaced {
                    start,
                    removed,
                    inserted,
                    cursor_before,
                    cursor_after,
                };
            }
        }
        self.cursor = restore_cursor;
        self.anchor = restore_cursor;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.coalesce = None;
        self.redo.push(mirror);
        self.ensure_cursor_visible();
        self.invalidate();
    }

    // ─── Compile check ───────────────────────────────────────────

    /// Run the installed checker (if any) against the current
    /// buffer. No-op when no checker is installed — keeps ledit
    /// useful as a plain editor in environments where the compiler
    /// hasn't been linked in.
    fn run_check(&mut self) {
        // Pull the whole buffer as a single UTF-8 string. The rope's
        // `to_utf8` is O(n) but the checker runs after manual F7 or
        // post-save, not on every keystroke, so this is fine.
        let text = self.buffer.to_utf8();
        match run_checker(&text) {
            Some(diags) => {
                self.diagnostics = diags;
                self.diagnostics_stale = false;
            }
            None => {
                // No checker installed; clear so we don't show stale.
                self.diagnostics.clear();
                self.diagnostics_stale = false;
            }
        }
        self.invalidate();
    }

    /// Move the cursor to the next diagnostic after the current row,
    /// wrapping to the first if we're past the last. F8 binding.
    fn jump_to_next_diagnostic(&mut self) {
        if self.diagnostics.is_empty() {
            return;
        }
        // Diagnostics may be unsorted; find the smallest line greater
        // than the current cursor's 1-indexed row, else fall back to
        // the smallest overall.
        let cur = self.cursor_row() + 1;
        let next = self
            .diagnostics
            .iter()
            .filter(|d| d.line > cur)
            .min_by_key(|d| (d.line, d.column))
            .or_else(|| {
                self.diagnostics
                    .iter()
                    .min_by_key(|d| (d.line, d.column))
            });
        if let Some(d) = next {
            let last = self.buffer.line_count().saturating_sub(1);
            let r = d.line.saturating_sub(1).min(last);
            let c = d.column.saturating_sub(1);
            self.set_cursor_rc(r, c, false);
            self.pref_col = self.cursor_col();
        }
    }

    /// First diagnostic on `line_1based`, if any. Used for the
    /// status line and the gutter mark.
    fn diagnostic_on_line(&self, line_1based: usize) -> Option<&Diagnostic> {
        self.diagnostics.iter().find(|d| d.line == line_1based)
    }

    fn redo(&mut self) {
        let Some(op) = self.redo.pop() else { return };
        let after: usize;
        let mirror: UndoOp;
        match op {
            UndoOp::Inserted {
                start,
                text,
                cursor_before,
                cursor_after,
            } => {
                self.splice_in_cps(start, &text);
                after = cursor_after;
                mirror = UndoOp::Inserted {
                    start,
                    text,
                    cursor_before,
                    cursor_after,
                };
            }
            UndoOp::Deleted {
                start,
                text,
                cursor_before,
                cursor_after,
            } => {
                // Redoing a deletion = re-delete the same range.
                let hi = start + text.len();
                self.splice_out(start, hi);
                after = cursor_after;
                mirror = UndoOp::Deleted {
                    start,
                    text,
                    cursor_before,
                    cursor_after,
                };
            }
            UndoOp::Replaced {
                start,
                removed,
                inserted,
                cursor_before,
                cursor_after,
            } => {
                // Currently the buffer holds `removed` at `start`
                // (undo restored it). Re-apply the replacement.
                let hi = start + removed.len();
                self.splice_out(start, hi);
                self.splice_in_cps(start, &inserted);
                after = cursor_after;
                mirror = UndoOp::Replaced {
                    start,
                    removed,
                    inserted,
                    cursor_before,
                    cursor_after,
                };
            }
        }
        self.cursor = after;
        self.anchor = after;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.coalesce = None;
        self.undo.push(mirror);
        self.ensure_cursor_visible();
        self.invalidate();
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

        // The render target's drawing space is in DIPs; convert the
        // pixel dimensions before doing layout math.
        let scale = self.dip_scale();
        let w_dip = (w as f32) * scale;
        let h_dip = (h as f32) * scale;

        unsafe { target.BeginDraw() };

        // Background.
        unsafe {
            target.Clear(Some(&D2D1_COLOR_F {
                r: 0.10,
                g: 0.11,
                b: 0.13,
                a: 1.0,
            }));
        }

        let fg = solid_brush(&target, 0.85, 0.88, 0.85, 1.0);
        let gutter_fg = solid_brush(&target, 0.45, 0.50, 0.55, 1.0);
        let gutter_bg = solid_brush(&target, 0.06, 0.07, 0.09, 1.0);
        let cursor_brush = solid_brush(&target, 0.95, 0.85, 0.40, 1.0);
        let status_bg = solid_brush(&target, 0.16, 0.18, 0.22, 1.0);
        let status_fg = solid_brush(&target, 0.80, 0.83, 0.88, 1.0);
        let sel_brush = solid_brush(&target, 0.20, 0.30, 0.55, 1.0);
        // Syntax-highlighting brushes. Order matches `TokenKind`.
        let kw_brush = solid_brush(&target, 0.55, 0.78, 1.00, 1.0);
        let num_brush = solid_brush(&target, 0.95, 0.70, 0.30, 1.0);
        let str_brush = solid_brush(&target, 0.65, 0.85, 0.55, 1.0);
        let cmt_brush = solid_brush(&target, 0.50, 0.55, 0.60, 1.0);
        // Error gutter mark — bright red. Greyed when stale (the
        // buffer has been edited since the last check).
        let err_brush = if self.diagnostics_stale {
            solid_brush(&target, 0.55, 0.30, 0.30, 1.0)
        } else {
            solid_brush(&target, 0.95, 0.30, 0.25, 1.0)
        };

        // Refresh tokens lazily, before any line is laid out.
        if self.tokens_dirty {
            self.tokens = tokenize_rope(&self.buffer);
            self.tokens_dirty = false;
        }

        let gutter_chars: f32 = 6.0;
        let gutter_w = gutter_chars * self.cell_w;
        let status_h = self.cell_h + 2.0;
        let content_top = 0.0;
        let content_bottom = h_dip - status_h;
        let visible_rows = ((content_bottom - content_top) / self.cell_h).floor() as usize;

        // Gutter background.
        if let (Some(target), Some(b)) = (Some(&target), gutter_bg.as_ref()) {
            unsafe {
                target.FillRectangle(
                    &D2D_RECT_F {
                        left: 0.0,
                        top: 0.0,
                        right: gutter_w,
                        bottom: content_bottom,
                    },
                    b,
                )
            };
        }

        // Selection rects, drawn under the text glyphs so the text
        // stays fully readable on top. The selection is held as a
        // pair of code-point offsets; we project it back into
        // (row, col) here for the per-row painting, then run it
        // through `buffer_col_to_display` so tabs in the indent
        // line up with the rendered glyphs.
        if let (Some((lo, hi)), Some(brush)) = (self.selection_range(), sel_brush.as_ref())
        {
            let (sr, sc) = self.buffer.offset_to_line_col(lo);
            let (er, ec) = self.buffer.offset_to_line_col(hi);
            for screen_row in 0..visible_rows {
                let line_idx = self.scroll_top + screen_row;
                if line_idx >= self.buffer.line_count() {
                    break;
                }
                if line_idx < sr || line_idx > er {
                    continue;
                }
                let line_text = self.line_text(line_idx);
                let line_chars = line_text.chars().count();
                let from_col = if line_idx == sr { sc } else { 0 };
                let to_col = if line_idx == er {
                    ec
                } else {
                    // Multi-line selection: paint past end-of-line
                    // out by one cell so the user sees the newline
                    // is part of the selection.
                    line_chars + 1
                };
                let from_display = buffer_col_to_display(&line_text, from_col);
                let to_display = if to_col > line_chars {
                    buffer_col_to_display(&line_text, line_chars) + 1
                } else {
                    buffer_col_to_display(&line_text, to_col)
                };
                let y = content_top + (screen_row as f32) * self.cell_h;
                let x0 = gutter_w + (from_display as f32) * self.cell_w;
                let x1 = gutter_w + (to_display as f32) * self.cell_w;
                unsafe {
                    target.FillRectangle(
                        &D2D_RECT_F {
                            left: x0,
                            top: y,
                            right: x1,
                            bottom: y + self.cell_h,
                        },
                        brush,
                    )
                };
            }
        }

        // Bracket flash. Painted between selection rects and line
        // glyphs so it lives UNDER the text — the user sees the
        // paren itself plus a soft box around it and its match. The
        // eye gets pair feedback without losing legibility.
        //
        // We highlight when the cursor sits on any delim, OR when
        // it sits just AFTER a close delim — covers the common case
        // of "I just typed `)` and want to confirm it landed on the
        // right open."
        let mut flash_targets: Vec<usize> = Vec::new();
        if let Some(cp) = self.buffer.char_at(self.cursor) {
            if cp == '(' as u32 || cp == ')' as u32 || cp == '[' as u32 || cp == ']' as u32
            {
                flash_targets.push(self.cursor);
            }
        }
        if self.cursor > 0 {
            if let Some(cp) = self.buffer.char_at(self.cursor - 1) {
                if cp == ')' as u32 || cp == ']' as u32 {
                    flash_targets.push(self.cursor - 1);
                }
            }
        }
        if !flash_targets.is_empty() {
            // Soft slate-blue. Saturation lower than the selection
            // colour so the two read distinctly when they overlap,
            // but bright enough to pop against the dark background.
            let match_brush = solid_brush(&target, 0.28, 0.34, 0.48, 1.0);
            if let Some(brush) = match_brush.as_ref() {
                for off in flash_targets {
                    if let Some(match_off) =
                        super::sexp_nav::matching_delim(&self.buffer, off)
                    {
                        for paint_off in [off, match_off] {
                            let (r, c) = self.buffer.offset_to_line_col(paint_off);
                            if r < self.scroll_top
                                || r >= self.scroll_top + visible_rows
                            {
                                continue;
                            }
                            let screen_row = r - self.scroll_top;
                            let line = self.line_text(r);
                            let display_col = buffer_col_to_display(&line, c);
                            let x = gutter_w + (display_col as f32) * self.cell_w;
                            let y = content_top + (screen_row as f32) * self.cell_h;
                            unsafe {
                                target.FillRectangle(
                                    &D2D_RECT_F {
                                        left: x,
                                        top: y,
                                        right: x + self.cell_w,
                                        bottom: y + self.cell_h,
                                    },
                                    brush,
                                )
                            };
                        }
                    }
                }
            }
        }

        // Lines.
        for screen_row in 0..visible_rows {
            let line_idx = self.scroll_top + screen_row;
            if line_idx >= self.buffer.line_count() {
                break;
            }
            let y = content_top + (screen_row as f32) * self.cell_h;

            // Gutter line number.
            let gutter_text = format!("{:>5} ", line_idx + 1);
            if let (Some(brush), Ok(layout)) = (
                gutter_fg.as_ref(),
                build_layout(&format, &gutter_text, gutter_w, self.cell_h),
            ) {
                unsafe {
                    target.DrawTextLayout(
                        windows_numerics::Vector2 { X: 0.0, Y: y },
                        &layout,
                        brush,
                        D2D1_DRAW_TEXT_OPTIONS_CLIP,
                    );
                }
            }

            // Error mark — a small red bar painted at the right edge
            // of the gutter on lines with diagnostics. Position it
            // inside the gutter so it doesn't overlap with text.
            if self.diagnostic_on_line(line_idx + 1).is_some() {
                if let Some(brush) = err_brush.as_ref() {
                    let bar_w = (self.cell_w * 0.4).max(2.0);
                    unsafe {
                        target.FillRectangle(
                            &D2D_RECT_F {
                                left: gutter_w - bar_w - 1.0,
                                top: y + 2.0,
                                right: gutter_w - 1.0,
                                bottom: y + self.cell_h - 2.0,
                            },
                            brush,
                        )
                    };
                }
            }

            // Line content. The layout is built from the
            // tab-expanded form so the cell grid matches the buffer's
            // visual columns; tokens recorded in buffer-char indices
            // are mapped through `buffer_col_to_display` before being
            // applied as drawing effects.
            let line = self.line_text(line_idx);
            if !line.is_empty() {
                let expanded = expand_line(&line);
                let max_w = w_dip - gutter_w;
                if let (Some(brush), Ok(layout)) = (
                    fg.as_ref(),
                    build_layout(&format, &expanded, max_w, self.cell_h),
                ) {
                    if let Some(line_tokens) = self.tokens.get(line_idx) {
                        for tok in line_tokens {
                            let kind_brush = match tok.kind {
                                TokenKind::Keyword => kw_brush.as_ref(),
                                TokenKind::Number => num_brush.as_ref(),
                                TokenKind::StringLit => str_brush.as_ref(),
                                TokenKind::Comment => cmt_brush.as_ref(),
                            };
                            let Some(b) = kind_brush else { continue };
                            let disp_start = buffer_col_to_display(&line, tok.start);
                            let disp_end = buffer_col_to_display(&line, tok.end);
                            let range = DWRITE_TEXT_RANGE {
                                startPosition: disp_start as u32,
                                length: (disp_end - disp_start) as u32,
                            };
                            let _ = unsafe { layout.SetDrawingEffect(b, range) };
                        }
                    }
                    unsafe {
                        target.DrawTextLayout(
                            windows_numerics::Vector2 { X: gutter_w, Y: y },
                            &layout,
                            brush,
                            D2D1_DRAW_TEXT_OPTIONS_CLIP,
                        );
                    }
                }
            }
        }

        // Cursor. cursor_col is a buffer char index — translate
        // through tab expansion so the bar lines up with the rendered
        // glyph the cursor is sitting before.
        let (cur_row, cur_col) = self.cursor_rc();
        if cur_row >= self.scroll_top && cur_row < self.scroll_top + visible_rows {
            let screen_row = cur_row - self.scroll_top;
            let line = self.line_text(cur_row);
            let display_col = buffer_col_to_display(&line, cur_col);
            let cx = gutter_w + (display_col as f32) * self.cell_w;
            let cy = content_top + (screen_row as f32) * self.cell_h;
            if let Some(brush) = cursor_brush.as_ref() {
                unsafe {
                    target.FillRectangle(
                        &D2D_RECT_F {
                            left: cx,
                            top: cy,
                            right: cx + 2.0,
                            bottom: cy + self.cell_h,
                        },
                        brush,
                    )
                };
            }
        }

        // Status line.
        if let Some(brush) = status_bg.as_ref() {
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
        let path_str = match self.file_path.as_ref() {
            Some(p) => p.display().to_string(),
            None => "<untitled>".to_string(),
        };
        let dirty_mark = if self.dirty { "*" } else { " " };

        // Diagnostic block: if the cursor is sitting on an errored
        // line, prefer that error's message; otherwise show the
        // count. "(stale)" annotates the count when the buffer has
        // changed since the last check.
        let here = self.diagnostic_on_line(cur_row + 1);
        let stale = if self.diagnostics_stale && !self.diagnostics.is_empty() {
            " (stale)"
        } else {
            ""
        };
        let diag_segment = match here {
            Some(d) => format!("⛔ {} ", d.message),
            None => match self.diagnostics.len() {
                0 => "F7 check  ".to_string(),
                1 => format!("1 error{stale}  F8 next  "),
                n => format!("{n} errors{stale}  F8 next  "),
            },
        };

        let parens_segment = match self.paren_balance() {
            0 => "()".to_string(),
            n if n > 0 => format!("(+{n})"),
            n => format!("({n})"),
        };

        let status = format!(
            " {dirty} {path}   Ln {row:4}, Col {col:2}   {nlines} lines   {parens}   {diag}",
            dirty = dirty_mark,
            path = path_str,
            row = cur_row + 1,
            col = cur_col + 1,
            nlines = self.buffer.line_count(),
            parens = parens_segment,
            diag = diag_segment,
        );
        if let (Some(brush), Ok(layout)) = (
            status_fg.as_ref(),
            build_layout(&format, &status, w_dip, status_h),
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

    // ─── Editing ─────────────────────────────────────────────────

    fn current_line_len(&self) -> usize {
        self.line_len_cps(self.cursor_row())
    }

    fn ensure_cursor_visible(&mut self) {
        // Recompute visible rows based on last known size. paint()
        // updates client_h on every frame, so this is good after at
        // least one paint. cell_h is in DIPs, so convert client_h
        // from pixels first.
        if self.client_h == 0 || self.cell_h <= 0.0 {
            return;
        }
        let status_h = self.cell_h + 2.0;
        let content_h_dip = (self.client_h as f32) * self.dip_scale() - status_h;
        let visible = (content_h_dip / self.cell_h).floor() as usize;
        if visible == 0 {
            return;
        }
        let cur_row = self.cursor_row();
        if cur_row < self.scroll_top {
            self.scroll_top = cur_row;
        } else if cur_row >= self.scroll_top + visible {
            self.scroll_top = cur_row + 1 - visible;
        }
    }

    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.do_insert(s, Some(CoalesceKind::Insert));
    }

    fn insert_newline(&mut self) {
        // Newline breaks the typing-coalesce chain so a subsequent
        // single-char insert starts a fresh undo entry.
        //
        // Auto-indent: copy the current line's leading whitespace,
        // and add two extra spaces if there's an unmatched `(` to
        // the left of the cursor on this line. Cheap heuristic;
        // good enough for hand-written Lisp without going through
        // a full parser. Strings and `;` comments are skipped so
        // parens inside them don't perturb the depth.
        let indent = self.compute_auto_indent();
        let mut text = String::with_capacity(1 + indent.len());
        text.push('\n');
        text.push_str(&indent);
        self.do_insert(&text, None);
    }

    fn compute_auto_indent(&self) -> String {
        let cur_row = self.cursor_row();
        let cur_col = self.cursor_col();
        if cur_row >= self.buffer.line_count() {
            return String::new();
        }
        let line = self.line_text(cur_row);
        let leading: String = line
            .chars()
            .take_while(|&c| c == ' ' || c == '\t')
            .collect();
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut after_backslash = false;
        let mut in_comment = false;
        for (i, c) in line.chars().enumerate() {
            if i >= cur_col {
                break;
            }
            if in_comment {
                continue;
            }
            if after_backslash {
                after_backslash = false;
                continue;
            }
            if c == '\\' && in_string {
                after_backslash = true;
                continue;
            }
            if c == '"' {
                in_string = !in_string;
                continue;
            }
            if in_string {
                continue;
            }
            if c == ';' {
                in_comment = true;
                continue;
            }
            if c == '(' {
                depth += 1;
            } else if c == ')' {
                depth -= 1;
            }
        }
        if depth > 0 {
            // Capped at +2 — deeply-nested forms shouldn't blow out
            // the indent column.
            let mut s = leading;
            s.push_str("  ");
            s
        } else {
            leading
        }
    }

    /// Buffer-wide paren count: opens minus closes, treating `;`
    /// comments and `"…"` string literals as opaque. 0 = balanced;
    /// positive = more opens than closes; negative = more closes
    /// (likely an editing accident).
    fn paren_balance(&self) -> i32 {
        let mut depth: i32 = 0;
        let mut in_string = false;
        let mut after_backslash = false;
        // Walk the rope's code points directly. We treat `\n` as a
        // comment terminator (matching the Vec<String> per-line
        // iteration the old code did).
        let mut in_comment = false;
        for cp in self.buffer.chars() {
            let c = match char::from_u32(cp) {
                Some(c) => c,
                None => continue,
            };
            if c == '\n' {
                in_comment = false;
                continue;
            }
            {
                if in_comment {
                    continue;
                }
                if after_backslash {
                    after_backslash = false;
                    continue;
                }
                if c == '\\' && in_string {
                    after_backslash = true;
                    continue;
                }
                if c == '"' {
                    in_string = !in_string;
                    continue;
                }
                if in_string {
                    continue;
                }
                if c == ';' {
                    in_comment = true;
                    continue;
                }
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                }
            }
            // Newline ends a `;` comment but not a string literal —
            // matches CL's reader semantics.
        }
        depth
    }

    fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.do_insert(text, None);
    }

    fn backspace(&mut self) {
        // If there's a selection, delete it (single Deleted entry).
        if self.delete_selection_to_undo() {
            self.redo.clear();
            self.ensure_cursor_visible();
            self.invalidate();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let cursor_before = self.cursor;
        // One code point back. The rope spans newlines transparently
        // so we don't need the old "if cursor_col > 0 else previous
        // line's length" arithmetic.
        let start = cursor_before - 1;
        let end = cursor_before;
        let removed = self.splice_out(start, end);
        self.cursor = start;
        self.anchor = start;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.redo.clear();

        // Coalesce contiguous backspaces: each new deletion is at
        // `end == prev_start`, so the previous record's prefix
        // (already at prev_start) extends back one more code point.
        let extend_last = self.coalesce == Some(CoalesceKind::Backspace)
            && matches!(
                self.undo.last(),
                Some(UndoOp::Deleted { start: prev_start, .. }) if *prev_start == end
            );
        if extend_last {
            if let Some(UndoOp::Deleted {
                start: prev_start,
                text: prev_text,
                cursor_after,
                ..
            }) = self.undo.last_mut()
            {
                let mut combined = removed;
                combined.extend_from_slice(prev_text);
                *prev_text = combined;
                *prev_start = start;
                *cursor_after = start;
            }
        } else {
            self.push_undo(UndoOp::Deleted {
                start,
                text: removed,
                cursor_before,
                cursor_after: start,
            });
        }
        self.coalesce = Some(CoalesceKind::Backspace);
        self.ensure_cursor_visible();
        self.invalidate();
    }

    fn delete_forward(&mut self) {
        if self.delete_selection_to_undo() {
            self.redo.clear();
            self.ensure_cursor_visible();
            self.invalidate();
            return;
        }
        if self.cursor >= self.buffer.len() {
            return;
        }
        let cursor_before = self.cursor;
        let start = cursor_before;
        let end = cursor_before + 1;
        let removed = self.splice_out(start, end);
        self.dirty = true;
        self.redo.clear();
        self.coalesce = None;
        self.push_undo(UndoOp::Deleted {
            start,
            text: removed,
            cursor_before,
            cursor_after: start,
        });
        self.invalidate();
    }

    // ─── Paredit: atomic structural edits ────────────────────────
    //
    // Each op produces ONE `Replaced` undo entry so a single Ctrl-Z
    // reverses the whole structural change. The supporting machinery
    // lives in `igui/sexp_nav.rs` (cursor → offset translations); the
    // mutations here are simple `splice_out` + `splice_in_cps` pairs
    // bracketed by `do_replace`.

    /// Apply a single Replaced edit and record the undo entry.
    /// `start..end` in the current buffer is replaced by `new_cps`;
    /// the cursor is moved to `cursor_after`.
    fn do_replace(
        &mut self,
        start: usize,
        end: usize,
        new_cps: Vec<u32>,
        cursor_after: usize,
    ) {
        let cursor_before = self.cursor;
        let removed = self.splice_out(start, end);
        self.splice_in_cps(start, &new_cps);
        self.cursor = cursor_after;
        self.anchor = cursor_after;
        self.pref_col = self.cursor_col();
        self.dirty = true;
        self.redo.clear();
        self.coalesce = None;
        self.push_undo(UndoOp::Replaced {
            start,
            removed,
            inserted: new_cps,
            cursor_before,
            cursor_after,
        });
        self.ensure_cursor_visible();
        self.invalidate();
    }

    /// Slurp forward: pull the next sexp outside the enclosing form
    /// into it. Close-delim moves to after the slurped sexp.
    ///
    ///   (foo |bar) baz quux  →  (foo |bar baz) quux
    fn slurp_forward(&mut self) {
        let Some((open, close)) = super::sexp_nav::enclosing_form(&self.buffer, self.cursor)
        else {
            return;
        };
        let _ = open;
        // Find the end of the next sexp after the close.
        let Some(next_end) = super::sexp_nav::forward_sexp(&self.buffer, close + 1) else {
            return; // nothing to slurp
        };
        // The close delim itself, as a code point.
        let close_cp = self.buffer.char_at(close).unwrap_or(')' as u32);
        // Replace `[close, next_end)` (the close-paren plus the
        // whitespace+sexp we're slurping) with the slurped material
        // followed by the close-paren — i.e. the close-paren moves
        // from `close` to `next_end - 1`.
        let between = self.buffer.slice(close + 1, next_end);
        let mut new_cps = between;
        new_cps.push(close_cp);
        // Cursor stays at its current offset — it didn't move
        // textually, only the close-paren around it did.
        let cursor_after = self.cursor;
        self.do_replace(close, next_end, new_cps, cursor_after);
    }

    /// Barf forward: push the last inner sexp out of the enclosing
    /// form. Close-delim moves to before the last inner sexp.
    ///
    ///   (foo |bar baz)  →  (foo |bar) baz
    fn barf_forward(&mut self) {
        let Some((open, close)) = super::sexp_nav::enclosing_form(&self.buffer, self.cursor)
        else {
            return;
        };
        // Walk inner sexps from open+1 forward, recording the start
        // of the last one. We stop when forward_sexp lands at or
        // past `close`.
        let mut last_inner_start: Option<usize> = None;
        let mut probe = open + 1;
        loop {
            // Skip atmosphere to find the next sexp start.
            let Some(sexp_end) = super::sexp_nav::forward_sexp(&self.buffer, probe) else {
                break;
            };
            if sexp_end > close {
                break;
            }
            // The sexp started somewhere in [probe, sexp_end); find
            // its actual start by going back one sexp.
            let sexp_start = match super::sexp_nav::backward_sexp(&self.buffer, sexp_end) {
                Some(s) => s,
                None => break,
            };
            last_inner_start = Some(sexp_start);
            probe = sexp_end;
        }
        let Some(last_start) = last_inner_start else {
            return; // nothing inside to barf
        };
        let close_cp = self.buffer.char_at(close).unwrap_or(')' as u32);
        // Trim trailing whitespace between the previous sexp's end
        // and `last_start` so the result reads as `(foo|bar)` with
        // single-space separation, not `(foo |bar)`. (Actually no —
        // we WANT to preserve the original whitespace before the
        // sexp we're barfing out. Don't trim.)
        //
        // Replace `[last_start, close+1)` (last inner sexp through
        // close-delim) with `) ` followed by the inner sexp.
        let inner = self.buffer.slice(last_start, close);
        // Strip trailing whitespace from `inner` so we don't end up
        // with `(foo bar )`.
        let mut inner_trim_end = inner.len();
        while inner_trim_end > 0 && is_ws_cp(inner[inner_trim_end - 1]) {
            inner_trim_end -= 1;
        }
        let inner_trimmed: Vec<u32> = inner[..inner_trim_end].to_vec();

        let mut new_cps = inner_trimmed;
        new_cps.push(close_cp);
        new_cps.push(' ' as u32);
        // Then the barfed sexp itself.
        let barfed = self.buffer.slice(last_start, close);
        // Strip leading whitespace from barfed (we already kept the
        // trimmed inner; the barfed sexp starts at last_start which
        // is past any leading whitespace).
        let _ = barfed; // unused — we use last_start..close + final trim above
        // ACTUALLY: we want to KEEP the original whitespace inside
        // (between inner_trimmed and what was barfed), so simpler
        // reformulation:
        // [open+1 .. close] currently contains "...inner_trim WS sexp"
        // After:             "...inner_trim) WS sexp"
        // i.e. just MOVE the close from position `close` to position
        // `inner_trim_end + open + 1` (the first whitespace cp after
        // the last inner sexp's predecessors).
        //
        // Compute target close position relative to `last_start`:
        // - inner spans [open+1, close)
        // - last sexp starts at last_start
        // - we want close at (the first non-ws after the previous sexp)
        //
        // Cleaner: between [open+1, last_start) is "previous content
        // ending in whitespace". We want to insert ) right after
        // that content (after trimming trailing whitespace).
        let before_last_sexp = self.buffer.slice(open + 1, last_start);
        let mut t_end = before_last_sexp.len();
        while t_end > 0 && is_ws_cp(before_last_sexp[t_end - 1]) {
            t_end -= 1;
        }
        let kept_prefix: Vec<u32> = before_last_sexp[..t_end].to_vec();
        let ws_between: Vec<u32> = before_last_sexp[t_end..].to_vec();
        let barfed_sexp = self.buffer.slice(last_start, close);

        let mut new_cps = kept_prefix;
        new_cps.push(close_cp);
        new_cps.extend_from_slice(&ws_between);
        new_cps.extend_from_slice(&barfed_sexp);

        // Cursor was somewhere in the original (open+1, close)
        // range. We want it at the equivalent offset in the result.
        // For simplicity: clamp into the new inner range.
        let new_inner_end = open + 1 + t_end; // offset of the new close
        let cursor_after = self.cursor.min(new_inner_end);
        self.do_replace(open + 1, close + 1, new_cps, cursor_after);
    }

    /// Wrap the next sexp at point with `( … )`. Cursor lands just
    /// after the new `(`.
    ///
    ///   |foo bar  →  (|foo) bar
    fn wrap_sexp(&mut self) {
        // Skip leading whitespace from cursor.
        let start = self.cursor;
        let Some(after_atmosphere) =
            super::sexp_nav::forward_sexp(&self.buffer, start)
        else {
            // Nothing to wrap — insert empty `()` at cursor.
            self.do_replace(start, start, vec!['(' as u32, ')' as u32], start + 1);
            return;
        };
        // We need the actual sexp start, not just its end. Use
        // backward_sexp from the end to find it.
        let sexp_start = super::sexp_nav::backward_sexp(&self.buffer, after_atmosphere)
            .unwrap_or(start);
        let sexp_end = after_atmosphere;
        let inner = self.buffer.slice(sexp_start, sexp_end);
        let mut new_cps = vec!['(' as u32];
        new_cps.extend_from_slice(&inner);
        new_cps.push(')' as u32);
        let cursor_after = sexp_start + 1;
        self.do_replace(sexp_start, sexp_end, new_cps, cursor_after);
    }

    /// Push the buffer (or selection if any) at the language thread
    /// as an `EvalBuffer` event. User-Lisp's `event-loop` macro has
    /// a built-in handler (`%handle-eval-buffer` in events.lisp)
    /// that calls `eval-string` on the source and writes the
    /// printed result to the iGui log overlay. Bound to Ctrl+R, F5,
    /// and the Edit → Run Buffer menu item.
    fn run_buffer(&self) {
        let (src, scope) = {
            let sel = self.selected_text();
            if sel.is_empty() {
                (self.buffer.to_utf8(), "buffer")
            } else {
                (sel, "selection")
            }
        };
        // Log immediately so the user sees the keystroke registered
        // even before the language thread services the event. The
        // default :eval-buffer handler in events.lisp will follow
        // up with "[eval] …" once it runs.
        super::log_view::append(&format!(
            "[ledit] run {} ({} chars)",
            scope,
            src.chars().count()
        ));
        crate::igui::channels::push(
            crate::igui::channels::IGuiEvent::EvalBuffer { source: src },
        );
    }

    /// Run only the top-level form the cursor is inside — Emacs's
    /// `C-M-x`. The standard CL interactive-development action:
    /// place the cursor anywhere inside a `(defun …)` or
    /// `(run-foo)` and one keystroke evaluates just that form.
    ///
    /// If the cursor is in atmosphere between top-level forms,
    /// nothing is sent.
    fn run_form_at_point(&self) {
        let Some((start, end)) =
            super::sexp_nav::top_level_form(&self.buffer, self.cursor)
        else {
            super::log_view::append(
                "[ledit] run form: cursor not inside a top-level form",
            );
            return;
        };
        let cps = self.buffer.slice(start, end);
        let src = super::rope_buffer::codepoints_to_utf8(&cps);
        super::log_view::append(&format!(
            "[ledit] run form ({} chars): {}",
            src.chars().count(),
            // Truncate the preview so the log line stays readable.
            preview_first_line(&src, 60),
        ));
        crate::igui::channels::push(
            crate::igui::channels::IGuiEvent::EvalBuffer { source: src },
        );
    }

    /// Splice (unwrap): delete the enclosing parens, leaving the
    /// inner content. Cursor stays where it was textually.
    ///
    ///   (foo |bar baz)  →  foo |bar baz
    fn splice_enclosing(&mut self) {
        let Some((open, close)) = super::sexp_nav::enclosing_form(&self.buffer, self.cursor)
        else {
            return;
        };
        let inner = self.buffer.slice(open + 1, close);
        // Cursor moves one cp left (the open was deleted before it).
        let cursor_after = self.cursor.saturating_sub(1);
        self.do_replace(open, close + 1, inner, cursor_after);
    }

    /// Raise: replace the enclosing form with the sexp at point.
    ///
    ///   (foo |bar baz)  →  |bar
    fn raise_sexp(&mut self) {
        let Some((open, close)) = super::sexp_nav::enclosing_form(&self.buffer, self.cursor)
        else {
            return;
        };
        // Find the sexp at or after the cursor.
        let Some(sexp_end) = super::sexp_nav::forward_sexp(&self.buffer, self.cursor) else {
            return;
        };
        let sexp_start = super::sexp_nav::backward_sexp(&self.buffer, sexp_end)
            .unwrap_or(self.cursor);
        // Clamp to enclosing form bounds.
        let sexp_start = sexp_start.max(open + 1);
        let sexp_end = sexp_end.min(close);
        if sexp_start >= sexp_end {
            return;
        }
        let kept = self.buffer.slice(sexp_start, sexp_end);
        let cursor_after = open;
        self.do_replace(open, close + 1, kept, cursor_after);
    }

    fn move_left(&mut self, extend: bool) {
        // If there's a selection and we're not extending, collapse to
        // the start (this is what most editors do — left arrow moves
        // to the beginning of the selection).
        if !extend {
            if let Some((lo, _)) = self.selection_range() {
                self.set_cursor_offset(lo, false);
                self.pref_col = self.cursor_col();
                return;
            }
        }
        if self.cursor > 0 {
            self.set_cursor_offset(self.cursor - 1, extend);
        }
        self.pref_col = self.cursor_col();
    }

    fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some((_, hi)) = self.selection_range() {
                self.set_cursor_offset(hi, false);
                self.pref_col = self.cursor_col();
                return;
            }
        }
        if self.cursor < self.buffer.len() {
            self.set_cursor_offset(self.cursor + 1, extend);
        }
        self.pref_col = self.cursor_col();
    }

    /// Ctrl-Right: jump over the next s-expression. Skips
    /// whitespace, line comments, and block comments, then either
    /// over a balanced `(…)`/`[…]`, a `"…"` string, a `#\X`
    /// character literal, a reader-macro-prefixed form (`'foo`,
    /// `` `foo``, `,foo`, `,@foo`, `#'foo`), or an atom. No-op if
    /// already at end of the enclosing sexp (sitting on a `)` or
    /// at end-of-buffer).
    fn move_forward_sexp(&mut self, extend: bool) {
        if let Some(target) = super::sexp_nav::forward_sexp(&self.buffer, self.cursor) {
            self.set_cursor_offset(target, extend);
            self.pref_col = self.cursor_col();
        }
    }

    /// Ctrl-Left: jump backward over the previous s-expression.
    /// Symmetric with `move_forward_sexp`.
    fn move_backward_sexp(&mut self, extend: bool) {
        if let Some(target) = super::sexp_nav::backward_sexp(&self.buffer, self.cursor) {
            self.set_cursor_offset(target, extend);
            self.pref_col = self.cursor_col();
        }
    }

    fn move_up(&mut self, extend: bool) {
        let r = self.cursor_row();
        if r == 0 {
            return;
        }
        // Don't reset pref_col across vertical moves — that's the
        // whole point of remembering it.
        let pref = self.pref_col;
        self.set_cursor_rc(r - 1, pref, extend);
        self.pref_col = pref;
    }

    fn move_down(&mut self, extend: bool) {
        let r = self.cursor_row();
        if r + 1 >= self.buffer.line_count() {
            return;
        }
        let pref = self.pref_col;
        self.set_cursor_rc(r + 1, pref, extend);
        self.pref_col = pref;
    }

    fn move_home(&mut self, extend: bool) {
        let r = self.cursor_row();
        self.set_cursor_rc(r, 0, extend);
        self.pref_col = 0;
    }

    fn move_end(&mut self, extend: bool) {
        let r = self.cursor_row();
        let n = self.line_len_cps(r);
        self.set_cursor_rc(r, n, extend);
        self.pref_col = self.cursor_col();
    }

    fn page_up(&mut self, extend: bool) {
        let visible = self.visible_rows().max(1);
        let r = self.cursor_row().saturating_sub(visible);
        let pref = self.pref_col;
        self.set_cursor_rc(r, pref, extend);
        self.pref_col = pref;
    }

    fn page_down(&mut self, extend: bool) {
        let visible = self.visible_rows().max(1);
        let last = self.buffer.line_count().saturating_sub(1);
        let r = (self.cursor_row() + visible).min(last);
        let pref = self.pref_col;
        self.set_cursor_rc(r, pref, extend);
        self.pref_col = pref;
    }

    fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.buffer.len();
        self.pref_col = self.cursor_col();
        self.coalesce = None;
        self.ensure_cursor_visible();
        self.invalidate();
    }

    // ─── Clipboard ───────────────────────────────────────────────

    fn cut(&mut self) {
        if self.selection_range().is_none() {
            return;
        }
        let text = self.selected_text();
        if !clipboard_set(self.hwnd, &text) {
            // If the clipboard write failed, leave the selection
            // alone — the user can retry. Don't push an undo for a
            // half-completed operation.
            return;
        }
        self.delete_selection_to_undo();
        self.redo.clear();
        self.coalesce = None;
        self.ensure_cursor_visible();
        self.invalidate();
    }

    fn copy(&self) {
        if self.selection_range().is_none() {
            return;
        }
        let _ = clipboard_set(self.hwnd, &self.selected_text());
    }

    fn paste(&mut self) {
        let Some(text) = clipboard_get(self.hwnd) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        // Replace selection (handled inside do_insert) with the
        // pasted text. No coalescing for paste.
        self.do_insert(&text, None);
    }

    fn visible_rows(&self) -> usize {
        if self.client_h == 0 || self.cell_h <= 0.0 {
            return 0;
        }
        let status_h = self.cell_h + 2.0;
        let content_h_dip = (self.client_h as f32) * self.dip_scale() - status_h;
        (content_h_dip / self.cell_h).floor() as usize
    }

    /// Translate a client-area pixel point to a logical buffer
    /// position, clamping to the buffer bounds. Mouse coordinates
    /// arrive from Win32 in physical pixels; cell metrics live in
    /// DIPs, so convert before doing the cell math. The display
    /// column the click lands on is then mapped back to a buffer
    /// char index via `display_col_to_buffer`, which handles tab
    /// expansion.
    fn pos_at_pixel(&self, x: i32, y: i32) -> (usize, usize) {
        let gutter_w = 6.0 * self.cell_w;
        let x_dip = self.px_to_dip(x);
        let y_dip = self.px_to_dip(y);
        let row_f = if self.cell_h > 0.0 {
            (y_dip / self.cell_h).floor()
        } else {
            0.0
        };
        let row = if row_f < 0.0 {
            self.scroll_top
        } else {
            self.scroll_top + row_f as usize
        };
        let row = row.min(self.buffer.line_count().saturating_sub(1));
        let cx = x_dip - gutter_w;
        let display_col = if cx <= 0.0 || self.cell_w <= 0.0 {
            0
        } else {
            (cx / self.cell_w).round() as usize
        };
        let line = self.line_text(row);
        let buf_col = display_col_to_buffer(&line, display_col);
        let n = line.chars().count();
        (row, buf_col.min(n))
    }

    fn click(&mut self, x: i32, y: i32, extend: bool) {
        let (r, c) = self.pos_at_pixel(x, y);
        self.set_cursor_rc(r, c, extend);
        self.pref_col = self.cursor_col();
    }

    fn drag_to(&mut self, x: i32, y: i32) {
        if !self.selecting_drag {
            return;
        }
        let (r, c) = self.pos_at_pixel(x, y);
        // While dragging, anchor stays put (it was set on
        // mouse-down), cursor follows the pointer. `set_cursor_rc`
        // with `extend=true` keeps the anchor pinned.
        let n = self.line_len_cps(r);
        let offset = self.buffer.line_col_to_offset(r, c.min(n));
        self.cursor = offset;
        self.pref_col = self.cursor_col();
        self.coalesce = None;
        self.ensure_cursor_visible();
        self.invalidate();
    }

    fn wheel(&mut self, delta: i32) {
        let lines = if WHEEL_DELTA != 0 {
            -(delta / WHEEL_DELTA as i32) * 3
        } else {
            0
        };
        if lines == 0 {
            return;
        }
        let max_top = self.buffer.line_count().saturating_sub(1);
        let new_top = (self.scroll_top as i32 + lines).max(0) as usize;
        self.scroll_top = new_top.min(max_top);
        self.invalidate();
    }

    // ─── File I/O ────────────────────────────────────────────────

    fn load_from(&mut self, path: PathBuf) {
        match std::fs::read(&path) {
            Ok(bytes) => {
                // The rope's `from_utf8` handles BOM stripping, CRLF
                // normalisation, and invalid-sequence U+FFFD
                // replacement in one pass — same behaviour as the
                // old hand-rolled `decode_utf8_lossy_with_bom` +
                // split('\n') + trim_end_matches('\r') pipeline.
                self.buffer = RopeBuffer::from_utf8(&bytes);
                self.cursor = 0;
                self.anchor = 0;
                self.pref_col = 0;
                self.scroll_top = 0;
                self.dirty = false;
                self.undo.clear();
                self.redo.clear();
                self.coalesce = None;
                self.tokens_dirty = true;
                self.diagnostics.clear();
                self.diagnostics_stale = true;
                self.file_path = Some(path);
                self.update_title();
                self.invalidate();
            }
            Err(e) => eprintln!("[ledit] read {path:?} failed: {e}", path = self.file_path),
        }
    }

    fn save_to(&mut self, path: PathBuf) -> bool {
        // Convert the rope to UTF-8 with CRLF line endings (Win32
        // convention). The rope's `to_utf8` emits LF; we replace.
        let lf_text = self.buffer.to_utf8();
        let text = lf_text.replace('\n', "\r\n");
        match std::fs::write(&path, text.as_bytes()) {
            Ok(()) => {
                self.file_path = Some(path);
                self.dirty = false;
                self.update_title();
                // Saving is a natural moment to refresh diagnostics:
                // the file the compiler will see now matches the
                // editor buffer, so checker output is meaningful.
                self.run_check();
                self.invalidate();
                true
            }
            Err(e) => {
                eprintln!("[ledit] save failed: {e}");
                false
            }
        }
    }

    fn update_title(&self) {
        let name = match self.file_path.as_ref() {
            Some(p) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<untitled>".to_string()),
            None => "<untitled>".to_string(),
        };
        let title = format!("ledit — {name}{star}", star = if self.dirty { " *" } else { "" });
        let mut w: Vec<u16> = title.encode_utf16().collect();
        w.push(0);
        unsafe {
            SendMessageW(
                self.hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_SETTEXT,
                Some(WPARAM(0)),
                Some(LPARAM(w.as_ptr() as isize)),
            )
        };
    }

    // ── File operations (called from do_file_cmd / load_content) ──────

    /// New file: clear the buffer and forget the file path.
    fn file_new(&mut self) {
        let len = self.buffer.len();
        if len > 0 {
            self.buffer.delete(0, len);
        }
        self.cursor = 0;
        self.anchor = 0;
        self.scroll_top = 0;
        self.undo.clear();
        self.redo.clear();
        self.coalesce = None;
        self.file_path = None;
        self.dirty = false;
        self.diagnostics.clear();
        self.diagnostics_stale = false;
        self.update_title();
        self.invalidate();
    }

    /// Open-file dialog then load the chosen file.
    fn file_open(&mut self) {
        let Some(path) = open_file_dialog(self.hwnd) else { return };
        match std::fs::read_to_string(&path) {
            Ok(text) => self.load_text(text, Some(path)),
            Err(e) => eprintln!("[ledit] open failed: {e}"),
        }
    }

    /// Save to current path, or fall back to Save As if no path yet.
    fn file_save(&mut self) {
        if let Some(p) = self.file_path.clone() {
            self.save_to(p);
        } else {
            self.file_save_as();
        }
    }

    /// Prompt for a path then save.
    fn file_save_as(&mut self) {
        let current = self.file_path.clone();
        let Some(path) = save_file_dialog(self.hwnd, current.as_deref()) else { return };
        self.save_to(path);
    }

    /// Load arbitrary text into the buffer, replacing everything.
    /// Called by `load_content` (Demos menu) and `file_open`.
    pub fn load_text(&mut self, text: String, path: Option<PathBuf>) {
        // Normalize CRLF → LF so the rope stays LF-only internally.
        let normalized = text.replace("\r\n", "\n");
        let len = self.buffer.len();
        if len > 0 {
            self.buffer.delete(0, len);
        }
        let cps: Vec<u32> = normalized.chars().map(|c| c as u32).collect();
        if !cps.is_empty() {
            self.buffer.insert(0, &cps);
        }
        self.cursor = 0;
        self.anchor = 0;
        self.scroll_top = 0;
        self.undo.clear();
        self.redo.clear();
        self.coalesce = None;
        self.file_path = path;
        self.dirty = false;
        self.diagnostics.clear();
        self.diagnostics_stale = false;
        self.update_title();
        self.invalidate();
    }
}

// ─── Win32 plumbing ──────────────────────────────────────────────────

unsafe extern "system" fn ledit_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let state = Box::new(ReditState::new(hwnd));
        let raw = Box::into_raw(state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        if let Ok(mut slot) = LEDIT_HWND.lock() {
            *slot = Some(hwnd.0 as isize);
        }
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ReditState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_PAINT => {
            // Use BeginPaint/EndPaint to satisfy the paint
            // notification, but draw via Direct2D to our HWND target.
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
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let _ = unsafe { SetFocus(Some(hwnd)) };
            // Shift+click extends, plain click collapses to point.
            state.click(x, y, shift_down());
            // Begin drag selection. Capture so we keep getting moves
            // even if the cursor leaves the client area.
            state.selecting_drag = true;
            unsafe { SetCapture(hwnd) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // wparam low word holds button state. We only care about
            // dragging while LBUTTON is held; if the user released
            // outside our window we'd otherwise stay in drag mode.
            let buttons = (wparam.0 & 0xFFFF) as u32;
            let lbutton_down = (buttons & MK_LBUTTON) != 0;
            if state.selecting_drag && lbutton_down {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                state.drag_to(x, y);
            } else if state.selecting_drag && !lbutton_down {
                // We missed the up edge somehow — release capture.
                let _ = unsafe { ReleaseCapture() };
                state.selecting_drag = false;
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if state.selecting_drag {
                let _ = unsafe { ReleaseCapture() };
                state.selecting_drag = false;
            }
            LRESULT(0)
        }
        WM_SETFOCUS | WM_MDIACTIVATE => {
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_MOUSEWHEEL => {
            let raw = ((wparam.0 >> 16) & 0xFFFF) as i16;
            state.wheel(raw as i32);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            handle_key(state, wparam.0 as u32);
            LRESULT(0)
        }
        WM_SYSKEYDOWN => {
            // Alt-letter chords for the paredit ops. Returning LRESULT(0)
            // suppresses the default Win32 behaviour (menu activation,
            // beep on unmapped Alt-letter) so unbound Alt-X falls through
            // cleanly. Bound chords: Alt-W = wrap, Alt-S = splice,
            // Alt-R = raise.
            let vk = wparam.0 as u32;
            if handle_alt_key(state, vk) {
                LRESULT(0)
            } else {
                unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_COMMAND => {
            // Edit-menu items forwarded from the frame WndProc.
            let cmd_id = (wparam.0 & 0xFFFF) as u16;
            if handle_edit_menu(state, cmd_id) {
                LRESULT(0)
            } else {
                unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_DPICHANGED_AFTERPARENT => {
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            if dpi != 0 {
                state.set_dpi(dpi);
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CHAR => {
            let cp = wparam.0 as u32;
            if let Some(c) = char::from_u32(cp) {
                handle_char(state, c);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Drop the heap state. Clear singleton slot if it matches.
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            if let Ok(mut slot) = LEDIT_HWND.lock() {
                if matches!(*slot, Some(h) if h == hwnd.0 as isize) {
                    *slot = None;
                }
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

fn shift_down() -> bool {
    (unsafe { GetKeyState(VK_SHIFT.0 as i32) } as i16) < 0
}

/// Dispatch an Edit-menu command forwarded from the frame's
/// WM_COMMAND handler. Returns `true` if we recognised the id.
fn handle_edit_menu(state: &mut ReditState, cmd_id: u16) -> bool {
    match cmd_id {
        EDIT_CMD_UNDO => state.undo(),
        EDIT_CMD_REDO => state.redo(),
        EDIT_CMD_CUT => state.cut(),
        EDIT_CMD_COPY => state.copy(),
        EDIT_CMD_PASTE => state.paste(),
        EDIT_CMD_SELECT_ALL => state.select_all(),
        EDIT_CMD_FORWARD_SEXP => state.move_forward_sexp(false),
        EDIT_CMD_BACKWARD_SEXP => state.move_backward_sexp(false),
        EDIT_CMD_SLURP_FORWARD => state.slurp_forward(),
        EDIT_CMD_BARF_FORWARD => state.barf_forward(),
        EDIT_CMD_WRAP => state.wrap_sexp(),
        EDIT_CMD_SPLICE => state.splice_enclosing(),
        EDIT_CMD_RAISE => state.raise_sexp(),
        EDIT_CMD_RUN_BUFFER => state.run_buffer(),
        EDIT_CMD_RUN_FORM => state.run_form_at_point(),
        _ => return false,
    }
    true
}

/// Dispatch an Alt-letter chord. Returns `true` if we consumed the
/// key. Alt-W/S/R are the paredit triad (wrap / splice / raise);
/// anything else falls through to the default handler.
fn handle_alt_key(state: &mut ReditState, vk: u32) -> bool {
    // ASCII letter virtual keys are 'A'=0x41 .. 'Z'=0x5A — the same
    // codes you'd see in a non-Alt WM_KEYDOWN for those keys.
    match vk {
        0x57 => {
            state.wrap_sexp();
            true
        }
        0x53 => {
            state.splice_enclosing();
            true
        }
        0x52 => {
            state.raise_sexp();
            true
        }
        _ => false,
    }
}

fn ctrl_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;
    (unsafe { GetKeyState(VK_CONTROL.0 as i32) } as i16) < 0
}

fn handle_key(state: &mut ReditState, vk: u32) {
    let vk16 = vk as u16;
    let extend = shift_down();
    let ctrl = ctrl_down();
    if vk16 == VK_LEFT.0 {
        if ctrl && extend {
            // Ctrl-Shift-Left → paredit barf forward
            state.barf_forward();
        } else if ctrl {
            state.move_backward_sexp(extend);
        } else {
            state.move_left(extend);
        }
    } else if vk16 == VK_RIGHT.0 {
        if ctrl && extend {
            // Ctrl-Shift-Right → paredit slurp forward
            state.slurp_forward();
        } else if ctrl {
            state.move_forward_sexp(extend);
        } else {
            state.move_right(extend);
        }
    } else if vk16 == VK_UP.0 {
        state.move_up(extend);
    } else if vk16 == VK_DOWN.0 {
        state.move_down(extend);
    } else if vk16 == VK_HOME.0 {
        state.move_home(extend);
    } else if vk16 == VK_END.0 {
        state.move_end(extend);
    } else if vk16 == VK_PRIOR.0 {
        state.page_up(extend);
    } else if vk16 == VK_NEXT.0 {
        state.page_down(extend);
    } else if vk16 == VK_DELETE.0 {
        // Shift+Delete is a Windows "cut to clipboard" alias. Honor
        // it because users from other editors expect it.
        if shift_down() && state.selection_range().is_some() {
            state.cut();
        } else {
            state.delete_forward();
        }
    } else if vk16 == VK_F5.0 {
        state.run_buffer();
    } else if vk16 == VK_RETURN.0 && ctrl {
        // Ctrl-Enter: run the top-level form at cursor. Equivalent
        // of Emacs C-M-x. WM_CHAR would deliver Ctrl+J (0x0A) for
        // plain Ctrl+Enter via the typical Win32 mapping, but we
        // catch it at the VK level here to be unambiguous.
        state.run_form_at_point();
    } else if vk16 == VK_F7.0 {
        // F7 — run compile check on the current buffer.
        state.run_check();
    } else if vk16 == VK_F8.0 {
        // F8 — jump to next diagnostic.
        state.jump_to_next_diagnostic();
    }
}

fn handle_char(state: &mut ReditState, c: char) {
    // WM_CHAR delivers control codes (Ctrl+A = 0x01, Ctrl+C = 0x03,
    // Ctrl+V = 0x16, ...) before any further processing. We dispatch
    // shortcuts here because by this point Win32 has already mapped
    // modifier state through.
    match c as u32 {
        0x01 => {
            // Ctrl+A — select all.
            state.select_all();
            return;
        }
        0x03 => {
            // Ctrl+C — copy.
            state.copy();
            return;
        }
        0x0F => {
            // Ctrl+O — open.
            if let Some(p) = open_file_dialog(state.hwnd) {
                state.load_from(p);
            }
            return;
        }
        0x13 => {
            // Ctrl+S — save (Shift+S = save as).
            if shift_down() || state.file_path.is_none() {
                if let Some(p) = save_file_dialog(state.hwnd, state.file_path.as_deref()) {
                    state.save_to(p);
                }
            } else if let Some(p) = state.file_path.clone() {
                state.save_to(p);
            }
            return;
        }
        0x16 => {
            // Ctrl+V — paste.
            state.paste();
            return;
        }
        0x12 => {
            // Ctrl+R — Run Buffer. Same path as Edit → Run Buffer
            // and the F5 accelerator: push source at the language
            // thread, default handler in events.lisp calls
            // eval-string and writes the result to the log overlay.
            state.run_buffer();
            return;
        }
        0x18 => {
            // Ctrl+X — cut.
            state.cut();
            return;
        }
        0x19 => {
            // Ctrl+Y — redo.
            state.redo();
            return;
        }
        0x1A => {
            // Ctrl+Z — undo.
            state.undo();
            return;
        }
        _ => {}
    }

    if c == '\r' {
        state.insert_newline();
        return;
    }
    if c == '\n' {
        return;
    }
    if c == '\t' {
        // Soft tab: insert spaces up to the next display tab stop so
        // typed indentation lines up with rendered tabs from
        // existing files. Single insert => one undo entry.
        let (cur_row, cur_col) = state.cursor_rc();
        let line = state.line_text(cur_row);
        let display_col = buffer_col_to_display(&line, cur_col);
        let pad = TAB_WIDTH - (display_col % TAB_WIDTH);
        let spaces: String = std::iter::repeat(' ').take(pad).collect();
        state.insert_str(&spaces);
        return;
    }
    if c == '\u{0008}' {
        state.backspace();
        return;
    }
    if (c as u32) < 0x20 {
        // Suppress other control characters not handled above.
        return;
    }
    state.insert_char(c);
}

// ─── File dialogs ───────────────────────────────────────────────────

/// Open/Save dialog filter spec. Win32 wants pairs of NUL-
/// terminated strings (display label, glob), with an extra NUL at
/// the end of the list. We default to Lisp; users can drop into
/// "All files" for everything else.
fn lisp_filter() -> Vec<u16> {
    let raw = "Lisp source (*.lisp;*.cl;*.lsp)\0*.lisp;*.cl;*.lsp\0\
               Text files (*.txt)\0*.txt\0\
               All files (*.*)\0*.*\0\0";
    raw.encode_utf16().collect()
}

/// Default extension when the user saves a new file without typing
/// one. Win32 appends this if the filename has no `.` of its own.
fn default_ext_lisp() -> Vec<u16> {
    "lisp\0".encode_utf16().collect()
}

fn open_file_dialog(owner: HWND) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    let filter = lisp_filter();
    let def_ext = default_ext_lisp();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrDefExt: PCWSTR(def_ext.as_ptr()),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };
    let ok = unsafe { GetOpenFileNameW(&mut ofn) }.as_bool();
    if !ok {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(OsString::from_wide(&buf[..n])))
}

fn save_file_dialog(owner: HWND, suggested: Option<&std::path::Path>) -> Option<PathBuf> {
    let mut buf = vec![0u16; 1024];
    if let Some(p) = suggested {
        let s: Vec<u16> = p.as_os_str().encode_wide().collect();
        let n = s.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&s[..n]);
    }
    let filter = lisp_filter();
    let def_ext = default_ext_lisp();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrDefExt: PCWSTR(def_ext.as_ptr()),
        Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_OVERWRITEPROMPT,
        ..Default::default()
    };
    let ok = unsafe { GetSaveFileNameW(&mut ofn) }.as_bool();
    if !ok {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(OsString::from_wide(&buf[..n])))
}

// ─── Helpers ─────────────────────────────────────────────────────────

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
                14.0,
                PCWSTR(locale_w.as_ptr()),
            )
        };
        if let Ok(f) = result {
            return Some(f);
        }
    }
    None
}

fn measure_cell(format: &IDWriteTextFormat) -> Option<(f32, f32, f32)> {
    // Lay out a single "M" to learn the cell metrics. Monospaced fonts
    // give equal advance for every character, so this is enough.
    let factory = &renderer::ctx().dwrite.factory;
    let text: Vec<u16> = "M".encode_utf16().collect();
    let layout = unsafe {
        factory.CreateTextLayout(&text, format, 1024.0, 1024.0)
    }
    .ok()?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    if unsafe { layout.GetMetrics(&mut metrics) }.is_err() {
        return None;
    }
    let mut line_metrics =
        [windows::Win32::Graphics::DirectWrite::DWRITE_LINE_METRICS::default(); 1];
    let mut actual: u32 = 0;
    let _ = unsafe { layout.GetLineMetrics(Some(&mut line_metrics), &mut actual) };
    let ascent = if actual > 0 {
        line_metrics[0].baseline
    } else {
        metrics.height * 0.8
    };
    Some((metrics.widthIncludingTrailingWhitespace, metrics.height, ascent))
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
    // For an editor we never want lines to wrap — long content should
    // be horizontally clipped at the right edge of the content area,
    // not wrap into the next row's slot. The DrawTextLayout call site
    // pairs this with `D2D1_DRAW_TEXT_OPTIONS_CLIP` so overflow gets
    // glyph-clipped rather than bleeding past `max_w`.
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

/// First line of `s` truncated to at most `n` characters, with `…`
/// appended if either truncation happened. Used by the run-form
/// log line so a 200-line `defun` doesn't fill the log overlay.
fn preview_first_line(s: &str, n: usize) -> String {
    let first = s.split('\n').next().unwrap_or("");
    let mut out: String = first.chars().take(n).collect();
    if first.chars().count() > n || s.contains('\n') {
        out.push('…');
    }
    out
}

/// True iff `cp` is one of the whitespace code points the paredit
/// trim logic treats as separator. Matches the rope's notion of
/// whitespace (space, tab, LF, CR, FF).
fn is_ws_cp(cp: u32) -> bool {
    matches!(cp, 0x20 | 0x09 | 0x0A | 0x0D | 0x0C)
}

/// Expand `\t` characters in `line` to spaces, padding to the next
/// `TAB_WIDTH` boundary on each tab. Returns the visual line we feed
/// into DirectWrite so the fixed cell grid actually lines up.
fn expand_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut col = 0usize;
    for c in line.chars() {
        if c == '\t' {
            let pad = TAB_WIDTH - (col % TAB_WIDTH);
            for _ in 0..pad {
                out.push(' ');
            }
            col += pad;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

/// Translate a buffer char index into its on-screen cell column,
/// expanding tabs the same way `expand_line` does. Used for cursor
/// drawing, selection rect endpoints, and token range mapping.
fn buffer_col_to_display(line: &str, char_col: usize) -> usize {
    let mut col = 0usize;
    for (i, c) in line.chars().enumerate() {
        if i == char_col {
            return col;
        }
        if c == '\t' {
            let pad = TAB_WIDTH - (col % TAB_WIDTH);
            col += pad;
        } else {
            col += 1;
        }
    }
    col
}

/// Inverse of `buffer_col_to_display`: given a display column (e.g.
/// from a mouse click), find the buffer char index that lands
/// closest. Used when translating mouse events into cursor moves.
fn display_col_to_buffer(line: &str, display_col: usize) -> usize {
    let mut col = 0usize;
    for (i, c) in line.chars().enumerate() {
        if col >= display_col {
            return i;
        }
        if c == '\t' {
            let pad = TAB_WIDTH - (col % TAB_WIDTH);
            // If the click landed inside a tab, snap to the closer
            // edge — keeps clicks on indented lines feel natural.
            if col + pad > display_col {
                let mid = col + pad / 2;
                return if display_col <= mid { i } else { i + 1 };
            }
            col += pad;
        } else {
            col += 1;
        }
    }
    line.chars().count()
}

// `decode_utf8_lossy_with_bom` was removed when `load_from` switched
// to `RopeBuffer::from_utf8`, which handles BOM, CRLF, and invalid
// sequences internally.

// ─── Component Pascal tokenizer (R3 syntax highlighting) ───────────

#[derive(Clone, Copy, Debug, PartialEq)]
enum TokenKind {
    Keyword,
    Number,
    StringLit,
    Comment,
}

#[derive(Clone, Debug)]
struct Token {
    /// Inclusive char index within the line.
    start: usize,
    /// Exclusive char index within the line.
    end: usize,
    kind: TokenKind,
}

/// All Component Pascal reserved words plus the standard built-in
/// type names. We treat them all the same — distinguishing types
/// (`INTEGER`, `REAL`, etc.) from keywords would mean baking the
/// full standard prelude in here for marginal aesthetic gain.
const CP_KEYWORDS: &[&str] = &[
    "ABSTRACT", "ARRAY", "BEGIN", "BY", "CASE", "CONST", "DIV", "DO", "ELSE", "ELSIF", "EMPTY",
    "END", "EXIT", "EXTENSIBLE", "FALSE", "FOR", "IF", "IMPORT", "IN", "IS", "LIMITED", "LOOP",
    "MOD", "MODULE", "NEW", "NIL", "OF", "OR", "OUT", "POINTER", "PROCEDURE", "RECORD", "REPEAT",
    "RETURN", "THEN", "TO", "TRUE", "TYPE", "UNTIL", "VAR", "WHILE", "WITH",
    // Built-in types / common pseudo-keywords
    "ANYPTR", "ANYREC", "BOOLEAN", "BYTE", "CHAR", "INTEGER", "INTSHORT", "LONGINT", "REAL",
    "SET", "SHORTCHAR", "SHORTINT", "SHORTREAL",
];

fn is_cp_keyword(word: &str) -> bool {
    // Linear scan — array is small (~50) and this runs once per
    // identifier. Fine for the file sizes we care about.
    CP_KEYWORDS.iter().any(|k| *k == word)
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Tokenize one line, given the comment-nesting depth carried in
/// from the previous line. Returns the produced tokens and the
/// depth to feed into the next line.
fn tokenize_line(line: &str, depth_in: u32) -> (Vec<Token>, u32) {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    let mut depth = depth_in;

    // If we entered this line already inside a comment, scan for
    // matching `*)` first.
    if depth > 0 {
        let start = 0;
        while i < chars.len() && depth > 0 {
            if i + 1 < chars.len() && chars[i] == '(' && chars[i + 1] == '*' {
                depth += 1;
                i += 2;
            } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == ')' {
                depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        tokens.push(Token {
            start,
            end: i,
            kind: TokenKind::Comment,
        });
    }

    while i < chars.len() {
        let c = chars[i];

        // Comment opener.
        if i + 1 < chars.len() && c == '(' && chars[i + 1] == '*' {
            let start = i;
            depth = 1;
            i += 2;
            while i < chars.len() && depth > 0 {
                if i + 1 < chars.len() && chars[i] == '(' && chars[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < chars.len() && chars[i] == '*' && chars[i + 1] == ')' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            tokens.push(Token {
                start,
                end: i,
                kind: TokenKind::Comment,
            });
            continue;
        }

        // String / char literal — single line, no escapes (CP syntax).
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            if i < chars.len() {
                i += 1; // consume the closing quote
            }
            tokens.push(Token {
                start,
                end: i,
                kind: TokenKind::StringLit,
            });
            continue;
        }

        // Number — decimal, hex (with H suffix), real (with E exponent),
        // char literal (with X suffix). CP-style: 0FFH, 1.5E2, 41X.
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '+'
                    || chars[i] == '-')
            {
                // Stop +/- only if it follows an E or D (the exponent
                // sign); otherwise it's an operator and we should
                // leave it for the next iteration.
                if (chars[i] == '+' || chars[i] == '-') && i > start {
                    let prev = chars[i - 1];
                    if !(prev == 'E' || prev == 'D' || prev == 'e' || prev == 'd') {
                        break;
                    }
                }
                i += 1;
            }
            tokens.push(Token {
                start,
                end: i,
                kind: TokenKind::Number,
            });
            continue;
        }

        // Identifier or keyword.
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < chars.len() && is_ident_cont(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if is_cp_keyword(&word) {
                tokens.push(Token {
                    start,
                    end: i,
                    kind: TokenKind::Keyword,
                });
            }
            // Plain identifiers stay default-colored — no token emitted.
            continue;
        }

        // Anything else (operators, whitespace, punctuation) is left
        // unstyled.
        i += 1;
    }

    (tokens, depth)
}

/// Tokenize the whole buffer. Comment depth is threaded through
/// lines so a `(* ... \n ... *)` block is one comment.
fn tokenize_rope(rope: &RopeBuffer) -> Vec<Vec<Token>> {
    let n = rope.line_count();
    let mut out = Vec::with_capacity(n);
    let mut depth: u32 = 0;
    for row in 0..n {
        let line = codepoints_to_utf8(&rope.get_line(row));
        let (tokens, next_depth) = tokenize_line(&line, depth);
        out.push(tokens);
        depth = next_depth;
    }
    out
}

// ─── Clipboard helpers ──────────────────────────────────────────────

/// Write `text` to the system clipboard as `CF_UNICODETEXT`. Returns
/// `true` on success. Each line is normalized to `\r\n` per Win32
/// convention so other apps see expected line endings.
fn clipboard_set(owner: HWND, text: &str) -> bool {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut wide: Vec<u16> = normalized.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();

    if unsafe { OpenClipboard(Some(owner)) }.is_err() {
        return false;
    }
    let mut ok = false;
    unsafe {
        let _ = EmptyClipboard();
        match GlobalAlloc(GMEM_MOVEABLE, bytes) {
            Ok(handle) => {
                let p = GlobalLock(handle) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    let _ = GlobalUnlock(handle);
                    let h = windows::Win32::Foundation::HANDLE(handle.0);
                    if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(h)).is_ok() {
                        ok = true;
                    }
                }
            }
            Err(e) => eprintln!("[ledit] GlobalAlloc failed: {e}"),
        }
        let _ = CloseClipboard();
    }
    ok
}

/// Read `CF_UNICODETEXT` from the clipboard. Returns `None` if no
/// such format is available or any step fails.
fn clipboard_get(owner: HWND) -> Option<String> {
    if unsafe { OpenClipboard(Some(owner)) }.is_err() {
        return None;
    }
    let result = unsafe {
        match GetClipboardData(CF_UNICODETEXT.0 as u32) {
            Ok(handle) => {
                let g = windows::Win32::Foundation::HGLOBAL(handle.0);
                let p = GlobalLock(g) as *const u16;
                if p.is_null() {
                    None
                } else {
                    // Walk to the NUL terminator (max 16 MiB to be
                    // defensive against malformed clipboard data).
                    let mut len = 0usize;
                    let cap = 16 * 1024 * 1024;
                    while len < cap && *p.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(p, len);
                    let s = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(g);
                    Some(s.replace("\r\n", "\n"))
                }
            }
            Err(_) => None,
        }
    };
    unsafe {
        let _ = CloseClipboard();
    }
    result
}
