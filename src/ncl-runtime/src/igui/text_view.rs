//! Text-grid MDI child — a monospaced character-cell window with a
//! cursor and terminal-style commands.
//!
//! Architecture (mirrors the graphics command-channel pattern in
//! `batch.rs`):
//!
//!   - Each text view owns a per-pane `PaneState` consisting of a
//!     **queue** of pending `TextCmd`s and the current **grid**
//!     (cells, cursor, pen, caret-visibility).
//!   - The language thread enqueues commands by calling
//!     `enqueue(child_id, cmd)` and never touches the grid or any
//!     HWND. After enqueueing it asks the GUI thread to flush via
//!     `window::post_text_flush(child_id)`, which posts a
//!     `WM_IGUI_TEXT_FLUSH` to the frame.
//!   - The frame WndProc (on the GUI thread) routes that message to
//!     `flush_on_gui_thread`, which drains the queue, applies each
//!     command to the grid, and calls `InvalidateRect` on the text
//!     view's window. WM_PAINT then re-renders from the grid.
//!
//! Locking: queue and grid both live behind one `Mutex<PaneState>`.
//! Lock-hold time is small on both sides — language thread holds it
//! just long enough to push one command; GUI thread holds it during
//! drain/apply and during the snapshot at the start of paint.
//!
//! Threading rule: anything that touches an HWND or a Win32 windowing
//! API runs on the GUI thread. The language thread holds `child_id`
//! as an opaque token. The single (and documented) exception in this
//! file is `flush_on_gui_thread`'s `InvalidateRect` — it runs on the
//! GUI thread by virtue of being called from a WndProc, so this is
//! consistent with the rule.

#![cfg(windows)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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
use windows::Win32::Graphics::Gdi::{InvalidateRect, BeginPaint, EndPaint, PAINTSTRUCT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    DefMDIChildProcW, GetClientRect, GetWindowLongPtrW, LoadCursorW, RegisterClassExW,
    SetWindowLongPtrW, CREATESTRUCTW, GWLP_USERDATA, IDC_ARROW, MDICREATESTRUCTW,
    WM_CHAR, WM_DPICHANGED_AFTERPARENT, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WNDCLASSEXW, WNDCLASS_STYLES,
};

use super::channels::{self, mouse_op, IGuiEvent};
use super::registry;
use super::renderer;
use super::window;

// ─── Command channel ────────────────────────────────────────────────

/// One unit of work the language thread asks the GUI thread to apply
/// to a text view's grid. Each shim in `lisp_shims.rs` produces one
/// of these and enqueues it; `flush_on_gui_thread` drains and applies
/// in order.
#[derive(Debug, Clone)]
enum TextCmd {
    WriteStr(String),
    WriteChar(u32),
    SetCursor { row: u32, col: u32 },
    ClearAll,
    ClearEol,
    ClearEos,
    Newline,
    ScrollUp(u32),
    SetPen { fg: u32, bg: u32 },
    ResetPen,
    SetCaretVisible(bool),
}

// ─── Cell + grid state ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Cell {
    codepoint: u32,
    fg: u32,
    bg: u32,
}

const DEFAULT_FG: u32 = 0xDCDCDCFF;
const DEFAULT_BG: u32 = 0x12161CFF;
const DEFAULT_CARET: u32 = 0xE6E664FF;

const BLANK: Cell = Cell {
    codepoint: b' ' as u32,
    fg: DEFAULT_FG,
    bg: DEFAULT_BG,
};

const TAB_WIDTH: u32 = 8;

/// The grid lives entirely on the GUI thread. WM_SIZE mutates it
/// directly (still on GUI thread); commands from the language thread
/// reach it via the queue + `flush_on_gui_thread` path.
struct GridState {
    cells: Vec<Cell>,
    rows: u32,
    cols: u32,
    cursor_row: u32,
    cursor_col: u32,
    pen_fg: u32,
    pen_bg: u32,
    caret_visible: bool,
}

impl GridState {
    fn new(rows: u32, cols: u32) -> Self {
        let count = (rows.max(1) as usize) * (cols.max(1) as usize);
        Self {
            cells: vec![BLANK; count],
            rows: rows.max(1),
            cols: cols.max(1),
            cursor_row: 0,
            cursor_col: 0,
            pen_fg: DEFAULT_FG,
            pen_bg: DEFAULT_BG,
            caret_visible: true,
        }
    }

    fn idx(&self, row: u32, col: u32) -> usize {
        (row as usize) * (self.cols as usize) + (col as usize)
    }

    fn resize(&mut self, rows: u32, cols: u32) {
        let new_rows = rows.max(1);
        let new_cols = cols.max(1);
        if new_rows == self.rows && new_cols == self.cols {
            return;
        }
        let mut next = vec![BLANK; (new_rows as usize) * (new_cols as usize)];
        let copy_rows = self.rows.min(new_rows);
        let copy_cols = self.cols.min(new_cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                let src = self.idx(r, c);
                let dst = (r as usize) * (new_cols as usize) + (c as usize);
                next[dst] = self.cells[src];
            }
        }
        self.cells = next;
        self.rows = new_rows;
        self.cols = new_cols;
        if self.cursor_row >= self.rows {
            self.cursor_row = self.rows - 1;
        }
        if self.cursor_col >= self.cols {
            self.cursor_col = self.cols - 1;
        }
    }
}

// ─── apply_cmd: GUI-thread interpreter for TextCmd ──────────────────

fn pen_blank(grid: &GridState) -> Cell {
    Cell {
        codepoint: b' ' as u32,
        fg: grid.pen_fg,
        bg: grid.pen_bg,
    }
}

fn clear_all(grid: &mut GridState) {
    let blank = pen_blank(grid);
    for c in grid.cells.iter_mut() {
        *c = blank;
    }
    grid.cursor_row = 0;
    grid.cursor_col = 0;
}

fn clear_to_eol(grid: &mut GridState) {
    let blank = pen_blank(grid);
    let row = grid.cursor_row;
    for c in grid.cursor_col..grid.cols {
        let i = grid.idx(row, c);
        grid.cells[i] = blank;
    }
}

fn clear_to_eos(grid: &mut GridState) {
    let blank = pen_blank(grid);
    let row = grid.cursor_row;
    for c in grid.cursor_col..grid.cols {
        let i = grid.idx(row, c);
        grid.cells[i] = blank;
    }
    for r in (grid.cursor_row + 1)..grid.rows {
        for c in 0..grid.cols {
            let i = grid.idx(r, c);
            grid.cells[i] = blank;
        }
    }
}

fn scroll_up(grid: &mut GridState, n: u32) {
    if n == 0 {
        return;
    }
    if n >= grid.rows {
        clear_all(grid);
        return;
    }
    let cols = grid.cols as usize;
    let shift = (n as usize) * cols;
    let total = grid.cells.len();
    grid.cells.copy_within(shift..total, 0);
    let blank = pen_blank(grid);
    for c in &mut grid.cells[total - shift..total] {
        *c = blank;
    }
    if grid.cursor_row >= n {
        grid.cursor_row -= n;
    } else {
        grid.cursor_row = 0;
    }
}

/// Move cursor to col 0 of the next row, scrolling up if needed.
fn newline(grid: &mut GridState) {
    grid.cursor_col = 0;
    if grid.cursor_row + 1 < grid.rows {
        grid.cursor_row += 1;
    } else {
        scroll_up(grid, 1);
        grid.cursor_row = grid.rows - 1;
    }
}

/// Backspace = erase-left. Moves cursor back one cell (within the
/// current line only) and writes a blank cell there with the
/// current pen. No-op if already at column 0.
fn erase_left(grid: &mut GridState) {
    if grid.cursor_col == 0 {
        return;
    }
    grid.cursor_col -= 1;
    let blank = pen_blank(grid);
    let i = grid.idx(grid.cursor_row, grid.cursor_col);
    grid.cells[i] = blank;
}

fn put_codepoint(grid: &mut GridState, cp: u32) {
    match cp {
        0x0A => newline(grid),
        0x0D => grid.cursor_col = 0,
        0x09 => {
            let next = ((grid.cursor_col / TAB_WIDTH) + 1) * TAB_WIDTH;
            grid.cursor_col = next.min(grid.cols.saturating_sub(1));
        }
        0x08 => erase_left(grid),
        cp if cp < 0x20 => {
            // Other C0 control codes — silently dropped for now.
        }
        cp => {
            if grid.cursor_col >= grid.cols {
                newline(grid);
            }
            let row = grid.cursor_row;
            let col = grid.cursor_col;
            let i = grid.idx(row, col);
            grid.cells[i] = Cell {
                codepoint: cp,
                fg: grid.pen_fg,
                bg: grid.pen_bg,
            };
            grid.cursor_col += 1;
        }
    }
}

fn apply_cmd(grid: &mut GridState, cmd: TextCmd) {
    match cmd {
        TextCmd::WriteStr(s) => {
            for ch in s.chars() {
                put_codepoint(grid, ch as u32);
            }
        }
        TextCmd::WriteChar(cp) => put_codepoint(grid, cp),
        TextCmd::SetCursor { row, col } => {
            grid.cursor_row = row.min(grid.rows.saturating_sub(1));
            grid.cursor_col = col.min(grid.cols.saturating_sub(1));
        }
        TextCmd::ClearAll => clear_all(grid),
        TextCmd::ClearEol => clear_to_eol(grid),
        TextCmd::ClearEos => clear_to_eos(grid),
        TextCmd::Newline => newline(grid),
        TextCmd::ScrollUp(n) => scroll_up(grid, n),
        TextCmd::SetPen { fg, bg } => {
            grid.pen_fg = fg;
            grid.pen_bg = bg;
        }
        TextCmd::ResetPen => {
            grid.pen_fg = DEFAULT_FG;
            grid.pen_bg = DEFAULT_BG;
        }
        TextCmd::SetCaretVisible(v) => grid.caret_visible = v,
    }
}

// ─── PaneState (queue + grid) ───────────────────────────────────────

struct PaneState {
    /// Pending commands awaiting application by the GUI thread.
    /// Mutated from the language thread (push) and the GUI thread
    /// (drain). Both sides hold the lock for microseconds at a time.
    queue: Vec<TextCmd>,
    /// Authoritative on-screen state. Mutated only on the GUI
    /// thread. Locked during the paint snapshot too.
    grid: GridState,
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

// ─── Language-thread enqueue API ────────────────────────────────────
//
// Each public function below mirrors a Lisp shim. They all do the
// same three things: look up the pane, push one command, ask the
// GUI thread to flush. They never see an HWND.

fn enqueue(child_id: i64, cmd: TextCmd) -> bool {
    let Some(pane) = get_pane(child_id) else {
        return false;
    };
    if let Ok(mut s) = pane.lock() {
        s.queue.push(cmd);
    } else {
        return false;
    }
    window::post_text_flush(child_id);
    true
}

pub fn write_str(child_id: i64, s: &str) -> bool {
    enqueue(child_id, TextCmd::WriteStr(s.to_string()))
}

pub fn write_char(child_id: i64, cp: u32) -> bool {
    enqueue(child_id, TextCmd::WriteChar(cp))
}

pub fn clear_all_cmd(child_id: i64) -> bool {
    enqueue(child_id, TextCmd::ClearAll)
}

pub fn clear_to_eol_cmd(child_id: i64) -> bool {
    enqueue(child_id, TextCmd::ClearEol)
}

pub fn clear_to_eos_cmd(child_id: i64) -> bool {
    enqueue(child_id, TextCmd::ClearEos)
}

pub fn newline_cmd(child_id: i64) -> bool {
    enqueue(child_id, TextCmd::Newline)
}

pub fn scroll_up_cmd(child_id: i64, n: u32) -> bool {
    enqueue(child_id, TextCmd::ScrollUp(n))
}

pub fn set_cursor(child_id: i64, row: u32, col: u32) -> bool {
    enqueue(child_id, TextCmd::SetCursor { row, col })
}

pub fn set_pen(child_id: i64, fg: u32, bg: u32) -> bool {
    enqueue(child_id, TextCmd::SetPen { fg, bg })
}

pub fn reset_pen(child_id: i64) -> bool {
    enqueue(child_id, TextCmd::ResetPen)
}

pub fn set_caret_visible(child_id: i64, visible: bool) -> bool {
    enqueue(child_id, TextCmd::SetCaretVisible(visible))
}

// ─── GUI-thread flush ───────────────────────────────────────────────

/// Called from `frame_wnd_proc` on the GUI thread when a
/// `WM_IGUI_TEXT_FLUSH` arrives. Drains the pending command queue,
/// applies each command to the grid, then `InvalidateRect`s the
/// child window so WM_PAINT picks up the changes.
pub(super) fn flush_on_gui_thread(child_id: i64) {
    let Some(pane) = get_pane(child_id) else {
        return;
    };
    {
        let mut s = match pane.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        // Drain into a local Vec so we hold the lock for the
        // shortest possible time. The grid mutations don't need
        // anyone else's lock; they're confined to s.grid.
        let cmds: Vec<TextCmd> = std::mem::take(&mut s.queue);
        let grid = &mut s.grid;
        for cmd in cmds {
            apply_cmd(grid, cmd);
        }
    }
    if let Some(hwnd) = registry::mdi_hwnd_of(child_id) {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

// ─── GUI-thread per-window state ────────────────────────────────────

const TEXT_CLASS: PCWSTR = w!("NewCL.iGui.TextView");

struct TextWindowState {
    hwnd: HWND,
    child_id: i64,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    cell_w: f32,
    cell_h: f32,
    client_w: u32,
    client_h: u32,
    dpi: u32,
    pane: Arc<Mutex<PaneState>>,
}

impl TextWindowState {
    fn new(hwnd: HWND, child_id: i64, pane: Arc<Mutex<PaneState>>) -> Self {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let dpi = if dpi == 0 { 96 } else { dpi };
        Self {
            hwnd,
            child_id,
            target: None,
            text_format: None,
            cell_w: 8.0,
            cell_h: 16.0,
            client_w: 0,
            client_h: 0,
            dpi,
            pane,
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
        self.recompute_grid();
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
            Err(e) => eprintln!("[text-view] CreateHwndRenderTarget failed: {e}"),
        }
    }

    /// Recompute (rows, cols) for the current pixel size + cell
    /// metrics and resize the grid to match. Called on WM_SIZE +
    /// WM_DPICHANGED_AFTERPARENT — both happen on the GUI thread, so
    /// it's safe to mutate the grid directly (still under the lock,
    /// because the language thread might be appending to the queue).
    fn recompute_grid(&mut self) {
        if self.client_w == 0 || self.client_h == 0 || self.cell_w <= 0.0 || self.cell_h <= 0.0 {
            return;
        }
        let scale = self.dip_scale();
        let w_dip = (self.client_w as f32) * scale;
        let h_dip = (self.client_h as f32) * scale;
        let cols = (w_dip / self.cell_w).floor().max(1.0) as u32;
        let rows = (h_dip / self.cell_h).floor().max(1.0) as u32;
        if let Ok(mut s) = self.pane.lock() {
            s.grid.resize(rows, cols);
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

        // Snapshot the grid under the lock then drop it before any
        // Direct2D calls (those can be slow; we don't want to block
        // language-thread enqueues for the whole paint).
        let snap = {
            let s = match self.pane.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            GridSnapshot {
                cells: s.grid.cells.clone(),
                rows: s.grid.rows,
                cols: s.grid.cols,
                cursor_row: s.grid.cursor_row,
                cursor_col: s.grid.cursor_col,
                caret_visible: s.grid.caret_visible,
            }
        };

        unsafe { target.BeginDraw() };

        let bg = unpack(DEFAULT_BG);
        unsafe {
            target.Clear(Some(&D2D1_COLOR_F {
                r: bg.r,
                g: bg.g,
                b: bg.b,
                a: bg.a,
            }))
        };

        let cw = self.cell_w;
        let ch = self.cell_h;

        // Background fill — coalesce horizontal runs of identical bg.
        for r in 0..snap.rows {
            let mut start = 0u32;
            while start < snap.cols {
                let bg = snap.cells[(r as usize) * (snap.cols as usize) + (start as usize)].bg;
                let mut end = start + 1;
                while end < snap.cols
                    && snap.cells[(r as usize) * (snap.cols as usize) + (end as usize)].bg == bg
                {
                    end += 1;
                }
                if bg != DEFAULT_BG {
                    let color = unpack(bg);
                    if let Some(brush) = solid_brush(&target, color.r, color.g, color.b, color.a) {
                        let x0 = (start as f32) * cw;
                        let x1 = (end as f32) * cw;
                        let y0 = (r as f32) * ch;
                        let y1 = ((r + 1) as f32) * ch;
                        unsafe {
                            target.FillRectangle(
                                &D2D_RECT_F {
                                    left: x0,
                                    top: y0,
                                    right: x1,
                                    bottom: y1,
                                },
                                &brush,
                            );
                        }
                    }
                }
                start = end;
            }
        }

        // Foreground glyphs — coalesce same-fg runs into one layout.
        for r in 0..snap.rows {
            let mut start = 0u32;
            let y = (r as f32) * ch;
            while start < snap.cols {
                let fg = snap.cells[(r as usize) * (snap.cols as usize) + (start as usize)].fg;
                let mut end = start + 1;
                while end < snap.cols
                    && snap.cells[(r as usize) * (snap.cols as usize) + (end as usize)].fg == fg
                {
                    end += 1;
                }
                let mut run = String::with_capacity((end - start) as usize);
                let mut all_space = true;
                for c in start..end {
                    let cp = snap.cells[(r as usize) * (snap.cols as usize) + (c as usize)]
                        .codepoint;
                    if let Some(ch) = char::from_u32(cp) {
                        if ch != ' ' {
                            all_space = false;
                        }
                        run.push(ch);
                    } else {
                        run.push(' ');
                    }
                }
                if !all_space {
                    let color = unpack(fg);
                    if let Some(brush) = solid_brush(&target, color.r, color.g, color.b, color.a) {
                        let max_w = ((end - start) as f32) * cw;
                        if let Ok(layout) = build_layout(&format, &run, max_w, ch) {
                            let x = (start as f32) * cw;
                            unsafe {
                                target.DrawTextLayout(
                                    windows_numerics::Vector2 { X: x, Y: y },
                                    &layout,
                                    &brush,
                                    D2D1_DRAW_TEXT_OPTIONS_CLIP,
                                );
                            }
                        }
                    }
                }
                start = end;
            }
        }

        // Caret.
        if snap.caret_visible
            && snap.cursor_row < snap.rows
            && snap.cursor_col < snap.cols
        {
            let caret = unpack(DEFAULT_CARET);
            if let Some(brush) = solid_brush(&target, caret.r, caret.g, caret.b, caret.a) {
                let x = (snap.cursor_col as f32) * cw;
                let y = (snap.cursor_row as f32) * ch;
                unsafe {
                    target.FillRectangle(
                        &D2D_RECT_F {
                            left: x,
                            top: y,
                            right: x + 1.5,
                            bottom: y + ch,
                        },
                        &brush,
                    );
                }
            }
        }

        let _ = unsafe { target.EndDraw(None, None) };
    }
}

struct GridSnapshot {
    cells: Vec<Cell>,
    rows: u32,
    cols: u32,
    cursor_row: u32,
    cursor_col: u32,
    caret_visible: bool,
}

// ─── Class registration & open ──────────────────────────────────────

pub fn register_class() -> Result<(), super::IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| super::IGuiError::Win32(format!("GetModuleHandleW (text): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| super::IGuiError::Win32(format!("LoadCursorW (text): {e}")))?;
    let cls = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(text_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: TEXT_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&cls) };
    Ok(())
}

struct TextBootstrap {
    child_id: i64,
    pane: Arc<Mutex<PaneState>>,
}

/// Language-thread entry: marshal a "open new text window" request to
/// the GUI thread via `window::open_text_child` (which posts a
/// `WM_IGUI_OPEN_TEXT` and blocks). Returns the child_id, or None if
/// the frame isn't up yet or WM_MDICREATE failed.
pub fn open(title: &str) -> Option<i64> {
    window::open_text_child(title)
}

/// GUI-thread half of `open`. Allocates the child_id, installs the
/// per-pane state (queue + grid), issues WM_MDICREATE.
pub(super) fn create_on_gui_thread(mdi: HWND, title_utf16: &[u16]) -> Option<i64> {
    let child_id = registry::allocate_child_id();
    let pane = Arc::new(Mutex::new(PaneState {
        queue: Vec::new(),
        grid: GridState::new(24, 80),
    }));
    install_pane(child_id, Arc::clone(&pane));

    let bootstrap = Box::into_raw(Box::new(TextBootstrap {
        child_id,
        pane: Arc::clone(&pane),
    }));

    let h_module = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => windows::Win32::Foundation::HANDLE(h.0),
        Err(e) => {
            eprintln!("[text-view] GetModuleHandleW: {e}");
            forget_pane(child_id);
            let _ = unsafe { Box::from_raw(bootstrap) };
            return None;
        }
    };

    let create = MDICREATESTRUCTW {
        szClass: TEXT_CLASS,
        szTitle: PCWSTR::from_raw(title_utf16.as_ptr()),
        hOwner: h_module,
        x: windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
        y: windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
        cx: windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
        cy: windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
        style: windows::Win32::UI::WindowsAndMessaging::WS_VISIBLE
            | windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
        lParam: LPARAM(bootstrap as isize),
    };

    let result = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            mdi,
            windows::Win32::UI::WindowsAndMessaging::WM_MDICREATE,
            Some(WPARAM(0)),
            Some(LPARAM(&create as *const _ as isize)),
        )
    };
    if result.0 == 0 {
        eprintln!("[text-view] WM_MDICREATE returned 0");
        forget_pane(child_id);
        let _ = unsafe { Box::from_raw(bootstrap) };
        return None;
    }
    Some(child_id)
}

// ─── Window proc ────────────────────────────────────────────────────

unsafe extern "system" fn text_wnd_proc(
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
        let bootstrap_ptr = unsafe { (*mdi_create).lParam.0 as *mut TextBootstrap };
        if bootstrap_ptr.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap = unsafe { Box::from_raw(bootstrap_ptr) };
        let child_id = bootstrap.child_id;
        let win_state = Box::new(TextWindowState::new(hwnd, child_id, bootstrap.pane));
        let raw = Box::into_raw(win_state) as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw) };
        registry::register(child_id, hwnd, hwnd);
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut TextWindowState;
    if state_ptr.is_null() {
        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }
    let state = unsafe { &mut *state_ptr };
    let child_id = state.child_id;

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
            if let Some(target) = state.target.as_ref() {
                let _ = unsafe { target.Resize(&D2D_SIZE_U { width: w, height: h }) };
            }
            state.recompute_grid();
            channels::push(IGuiEvent::Resize {
                child_id,
                width: w as i64,
                height: h as i64,
            });
            state.invalidate();
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_LBUTTONDOWN => {
            let _ = unsafe { SetFocus(Some(hwnd)) };
            window::push_mouse(child_id, mouse_op::LEFT_DOWN, 1, lparam);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            window::push_mouse(child_id, mouse_op::LEFT_UP, 1, lparam);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            window::push_mouse(child_id, mouse_op::RIGHT_DOWN, 2, lparam);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            window::push_mouse(child_id, mouse_op::RIGHT_UP, 2, lparam);
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            window::push_mouse(child_id, mouse_op::MIDDLE_DOWN, 3, lparam);
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            window::push_mouse(child_id, mouse_op::MIDDLE_UP, 3, lparam);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            window::push_mouse(child_id, mouse_op::MOVE, 0, lparam);
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            window::push_key(child_id, true, wparam, lparam);
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            window::push_key(child_id, false, wparam, lparam);
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CHAR => {
            channels::push(IGuiEvent::Char {
                child_id,
                codepoint: wparam.0 as i64,
                mods: window::current_modifiers(),
                time_ms: window::msg_time(),
            });
            LRESULT(0)
        }
        WM_SETFOCUS => {
            channels::push(IGuiEvent::Focus {
                child_id,
                gained: true,
            });
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KILLFOCUS => {
            channels::push(IGuiEvent::Focus {
                child_id,
                gained: false,
            });
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
            channels::push(IGuiEvent::Close { child_id });
            registry::unregister(child_id);
            forget_pane(child_id);
            let _ = unsafe { Box::from_raw(state_ptr) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Render helpers (local copies of log_view's, intentional) ───────

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

fn measure_cell(format: &IDWriteTextFormat) -> Option<(f32, f32)> {
    let factory = &renderer::ctx().dwrite.factory;
    let text: Vec<u16> = "M".encode_utf16().collect();
    let layout = unsafe { factory.CreateTextLayout(&text, format, 1024.0, 1024.0) }.ok()?;
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

#[derive(Clone, Copy)]
struct Rgba {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

fn unpack(packed: u32) -> Rgba {
    Rgba {
        r: ((packed >> 24) & 0xFF) as f32 / 255.0,
        g: ((packed >> 16) & 0xFF) as f32 / 255.0,
        b: ((packed >> 8) & 0xFF) as f32 / 255.0,
        a: (packed & 0xFF) as f32 / 255.0,
    }
}
