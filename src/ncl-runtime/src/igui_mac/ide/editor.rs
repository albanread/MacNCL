//! Mac-native Lisp editor pane.
//!
//! Ports the editing model of the Windows `igui::ledit` rich Lisp editor
//! onto the platform-neutral stack: the same rope buffer
//! (`crate::igui_text::RopeBuffer`), the same undo/coalesce semantics and
//! cursor/selection model — but rendered to `SurfaceCmd`s (Core Graphics +
//! Core Text) and driven by `IGuiEvent`s instead of Direct2D + Win32.
//!
//! Layered to keep each step compiling and testable headlessly:
//!   v1 (this): buffer, cursor, selection, movement, insert/delete, undo,
//!              clipboard, render → SurfaceCmd, key/char/mouse input.
//!   next:      syntax tokens (highlighting), paredit sexp ops, diagnostics.

use crate::igui_mac::events::vk;
use crate::igui_paint::{
    FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
};
use crate::igui_text::{codepoints_to_utf8, utf8_to_codepoints, RopeBuffer};

const UNDO_CAP: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Coalesce {
    Insert,
    Delete,
}

enum UndoOp {
    Inserted {
        start: usize,
        text: Vec<u32>,
        cursor_before: usize,
        cursor_after: usize,
    },
    Deleted {
        start: usize,
        text: Vec<u32>,
        cursor_before: usize,
        cursor_after: usize,
    },
}

/// Lisp syntax token classes, for highlighting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Symbol,
    Special, // defun, lambda, let, if, …
    Keyword, // :foo
    Number,
    StringLit,
    Char,    // #\x
    Comment, // ; …
    Paren,
    Quote, // ' ` , ,@ #'
}

/// Visual theme + font metrics for the editor.
#[derive(Clone)]
pub struct Theme {
    pub family: String,
    pub size: f32,
    /// Monospace cell width / line height / ascent, in points. Set by the
    /// window layer via `set_metrics` once the font is measured.
    pub cell_w: f32,
    pub cell_h: f32,
    pub ascent: f32,
    pub bg: Rgba,
    pub fg: Rgba,
    pub caret: Rgba,
    pub selection: Rgba,
    pub gutter_fg: Rgba,
    // Syntax colors.
    pub c_special: Rgba,
    pub c_keyword: Rgba,
    pub c_number: Rgba,
    pub c_string: Rgba,
    pub c_char: Rgba,
    pub c_comment: Rgba,
    pub c_paren: Rgba,
    pub c_quote: Rgba,
}

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            family: "Menlo".into(),
            size: 15.0,
            cell_w: 9.0,
            cell_h: 19.0,
            ascent: 14.0,
            bg: rgb(24, 26, 33),
            fg: rgb(220, 223, 228),
            caret: rgb(120, 200, 255),
            selection: Rgba { r: 0.40, g: 0.55, b: 0.85, a: 0.35 },
            gutter_fg: rgb(90, 96, 110),
            c_special: rgb(198, 160, 246), // purple — special forms
            c_keyword: rgb(244, 191, 117), // amber — :keywords
            c_number: rgb(166, 218, 149),  // green — numbers
            c_string: rgb(166, 218, 149),  // green — strings
            c_char: rgb(138, 222, 200),    // teal — char literals
            c_comment: rgb(110, 120, 135), // grey — comments
            c_paren: rgb(140, 150, 168),   // dim — brackets
            c_quote: rgb(238, 153, 160),   // pink — quotes
        }
    }
}

/// Special operators that get the `Special` color.
fn is_special(word: &str) -> bool {
    matches!(
        word,
        "defun" | "defmacro" | "defvar" | "defparameter" | "defconstant" | "defclass"
            | "defmethod" | "defgeneric" | "defstruct" | "lambda" | "let" | "let*" | "flet"
            | "labels" | "macrolet" | "if" | "when" | "unless" | "cond" | "case" | "ecase"
            | "typecase" | "and" | "or" | "not" | "progn" | "prog1" | "prog2" | "block"
            | "return" | "return-from" | "loop" | "do" | "do*" | "dolist" | "dotimes"
            | "setf" | "setq" | "push" | "pop" | "incf" | "decf" | "quote" | "function"
            | "funcall" | "apply" | "handler-case" | "unwind-protect" | "catch" | "throw"
            | "multiple-value-bind" | "destructuring-bind" | "with-slots" | "with-accessors"
            | "eval-when" | "declare" | "the" | "values" | "in-package" | "defpackage"
    )
}

#[inline]
fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '"' | ';' | '\'' | '`' | ',')
}

/// Tokenise one line's chars into `(start_col, end_col, Tok)` spans.
/// `in_string` tracks whether a multi-line string is open at line start;
/// returns the updated flag for the next line.
fn tokenize_line(chars: &[char], mut in_string: bool) -> (Vec<(usize, usize, Tok)>, bool) {
    let mut spans = Vec::new();
    let n = chars.len();
    let mut i = 0;
    if in_string {
        // Continuation of a multi-line string: scan to closing quote.
        let start = 0;
        while i < n {
            if chars[i] == '\\' {
                i += 2;
                continue;
            }
            if chars[i] == '"' {
                i += 1;
                in_string = false;
                break;
            }
            i += 1;
        }
        spans.push((start, i, Tok::StringLit));
    }
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            ';' => {
                spans.push((i, n, Tok::Comment));
                i = n;
            }
            '"' => {
                let start = i;
                i += 1;
                in_string = true;
                while i < n {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    if chars[i] == '"' {
                        i += 1;
                        in_string = false;
                        break;
                    }
                    i += 1;
                }
                spans.push((start, i, Tok::StringLit));
            }
            '#' if i + 1 < n && chars[i + 1] == '\\' => {
                let start = i;
                i += 3.min(n - i); // #\ + one char
                spans.push((start, i, Tok::Char));
            }
            '#' if i + 1 < n && chars[i + 1] == '\'' => {
                spans.push((i, i + 2, Tok::Quote));
                i += 2;
            }
            '(' | ')' | '[' | ']' => {
                spans.push((i, i + 1, Tok::Paren));
                i += 1;
            }
            '\'' | '`' => {
                spans.push((i, i + 1, Tok::Quote));
                i += 1;
            }
            ',' => {
                let end = if i + 1 < n && chars[i + 1] == '@' { i + 2 } else { i + 1 };
                spans.push((i, end, Tok::Quote));
                i = end;
            }
            ':' => {
                let start = i;
                i += 1;
                while i < n && !is_delim(chars[i]) {
                    i += 1;
                }
                spans.push((start, i, Tok::Keyword));
            }
            _ => {
                let start = i;
                while i < n && !is_delim(chars[i]) {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let tok = if word.parse::<f64>().is_ok()
                    || (word.starts_with(['+', '-']) && word.len() > 1
                        && word[1..].chars().all(|c| c.is_ascii_digit() || c == '.'))
                {
                    Tok::Number
                } else if is_special(&word) {
                    Tok::Special
                } else {
                    Tok::Symbol
                };
                spans.push((start, i, tok));
            }
        }
    }
    (spans, in_string)
}

/// A Lisp text editor over a rope buffer.
pub struct Editor {
    buffer: RopeBuffer,
    /// Cursor as a code-point offset into the rope.
    cursor: usize,
    /// Selection anchor; `anchor == cursor` ⇒ no selection.
    anchor: usize,
    /// Preferred column for vertical motion (code-point column).
    pref_col: usize,
    /// Top visible row.
    scroll_top: usize,
    dirty: bool,
    undo: Vec<UndoOp>,
    redo: Vec<UndoOp>,
    coalesce: Option<Coalesce>,
    clipboard: Vec<u32>,
    theme: Theme,
    /// Whether to draw a line-number gutter.
    pub show_gutter: bool,
    /// Rows that fit in the viewport, updated by `render`.
    visible_rows: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            buffer: RopeBuffer::new(),
            cursor: 0,
            anchor: 0,
            pref_col: 0,
            scroll_top: 0,
            dirty: false,
            undo: Vec::new(),
            redo: Vec::new(),
            coalesce: None,
            clipboard: Vec::new(),
            theme: Theme::default(),
            show_gutter: true,
            visible_rows: 1,
        }
    }

    pub fn with_text(text: &str) -> Self {
        let mut e = Self::new();
        e.buffer = RopeBuffer::from_utf8(text.as_bytes());
        e
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }
    pub fn set_theme(&mut self, t: Theme) {
        self.theme = t;
    }
    /// Set measured monospace metrics (from Core Text).
    pub fn set_metrics(&mut self, cell_w: f32, cell_h: f32, ascent: f32) {
        self.theme.cell_w = cell_w;
        self.theme.cell_h = cell_h;
        self.theme.ascent = ascent;
    }

    pub fn text(&self) -> String {
        self.buffer.to_utf8()
    }
    /// Replace the whole buffer, putting the cursor at the end and
    /// clearing history. Used by the REPL input line (recall / clear).
    pub fn set_text(&mut self, s: &str) {
        self.buffer = RopeBuffer::from_utf8(s.as_bytes());
        self.cursor = self.buffer.len();
        self.anchor = self.cursor;
        self.pref_col = self.cursor_rc().1;
        self.scroll_top = 0;
        self.undo.clear();
        self.redo.clear();
        self.coalesce = None;
        self.dirty = false;
    }
    pub fn clear(&mut self) {
        self.set_text("");
    }
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
    pub fn set_clean(&mut self) {
        self.dirty = false;
    }
    pub fn cursor_rc(&self) -> (usize, usize) {
        self.buffer.offset_to_line_col(self.cursor)
    }
    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    // ─── selection / cursor helpers ──────────────────────────────────

    fn selection_range(&self) -> Option<(usize, usize)> {
        if self.cursor == self.anchor {
            None
        } else if self.anchor < self.cursor {
            Some((self.anchor, self.cursor))
        } else {
            Some((self.cursor, self.anchor))
        }
    }

    fn set_cursor_offset(&mut self, offset: usize, extend: bool) {
        let clamped = offset.min(self.buffer.len());
        self.cursor = clamped;
        if !extend {
            self.anchor = clamped;
        }
        self.coalesce = None;
    }

    fn set_cursor_rc(&mut self, row: usize, col: usize, extend: bool) {
        let last_row = self.buffer.line_count().saturating_sub(1);
        let r = row.min(last_row);
        let c = col.min(self.line_len_cps(r));
        let offset = self.buffer.line_col_to_offset(r, c);
        self.set_cursor_offset(offset, extend);
    }

    fn line_len_cps(&self, row: usize) -> usize {
        match self.buffer.line_range(row) {
            Some((s, e)) => e - s,
            None => 0,
        }
    }

    fn line_text(&self, row: usize) -> String {
        codepoints_to_utf8(&self.buffer.get_line(row))
    }

    pub fn selected_text(&self) -> String {
        match self.selection_range() {
            Some((lo, hi)) => codepoints_to_utf8(&self.buffer.slice(lo, hi)),
            None => String::new(),
        }
    }

    // ─── mutation primitives ─────────────────────────────────────────

    fn splice_in_cps(&mut self, offset: usize, text: &[u32]) -> usize {
        if text.is_empty() {
            return offset;
        }
        self.buffer.insert(offset, text);
        offset + text.len()
    }

    fn splice_out(&mut self, lo: usize, hi: usize) -> Vec<u32> {
        if lo >= hi {
            return Vec::new();
        }
        let removed = self.buffer.slice(lo, hi);
        self.buffer.delete(lo, hi - lo);
        removed
    }

    fn push_undo(&mut self, op: UndoOp) {
        self.undo.push(op);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
    }

    fn delete_selection_to_undo(&mut self) -> bool {
        let Some((lo, hi)) = self.selection_range() else {
            return false;
        };
        let cursor_before = self.cursor;
        let removed = self.splice_out(lo, hi);
        self.cursor = lo;
        self.anchor = lo;
        self.pref_col = self.cursor_rc().1;
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

    fn do_insert(&mut self, text: &str, coalesce: Option<Coalesce>) {
        if self.selection_range().is_some() {
            self.delete_selection_to_undo();
        }
        let cps = utf8_to_codepoints(text.as_bytes());
        if cps.is_empty() {
            return;
        }
        let cursor_before = self.cursor;
        let start = cursor_before;
        let end = self.splice_in_cps(start, &cps);
        self.cursor = end;
        self.anchor = end;
        self.pref_col = self.cursor_rc().1;
        self.dirty = true;
        self.redo.clear();

        let extend_last = coalesce == Some(Coalesce::Insert)
            && self.coalesce == Some(Coalesce::Insert)
            && matches!(
                self.undo.last(),
                Some(UndoOp::Inserted { start: ps, text: pt, .. }) if ps + pt.len() == start
            );
        if extend_last {
            if let Some(UndoOp::Inserted { text: pt, cursor_after, .. }) = self.undo.last_mut() {
                pt.extend_from_slice(&cps);
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
    }

    pub fn insert_char(&mut self, cp: u32) {
        if let Some(c) = char::from_u32(cp) {
            let mut s = String::new();
            s.push(c);
            self.do_insert(&s, Some(Coalesce::Insert));
        }
    }

    pub fn insert_str(&mut self, s: &str) {
        self.do_insert(s, None);
    }

    pub fn insert_newline(&mut self) {
        // Auto-indent: copy the leading whitespace of the current line.
        let (row, _) = self.cursor_rc();
        let line = self.line_text(row);
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let mut s = String::from("\n");
        s.push_str(&indent);
        self.do_insert(&s, None);
    }

    pub fn backspace(&mut self) {
        if self.delete_selection_to_undo() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let lo = self.cursor - 1;
        let cursor_before = self.cursor;
        let removed = self.splice_out(lo, self.cursor);
        self.cursor = lo;
        self.anchor = lo;
        self.pref_col = self.cursor_rc().1;
        self.dirty = true;
        self.redo.clear();
        self.push_undo(UndoOp::Deleted {
            start: lo,
            text: removed,
            cursor_before,
            cursor_after: lo,
        });
        self.coalesce = Some(Coalesce::Delete);
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection_to_undo() {
            return;
        }
        if self.cursor >= self.buffer.len() {
            return;
        }
        let hi = self.cursor + 1;
        let cursor_before = self.cursor;
        let removed = self.splice_out(self.cursor, hi);
        self.dirty = true;
        self.redo.clear();
        self.push_undo(UndoOp::Deleted {
            start: self.cursor,
            text: removed,
            cursor_before,
            cursor_after: self.cursor,
        });
        self.coalesce = None;
    }

    pub fn undo(&mut self) {
        let Some(op) = self.undo.pop() else { return };
        match op {
            UndoOp::Inserted { start, text, cursor_before, .. } => {
                self.splice_out(start, start + text.len());
                self.cursor = cursor_before;
                self.anchor = cursor_before;
                self.redo.push(UndoOp::Inserted {
                    start,
                    text,
                    cursor_before,
                    cursor_after: start,
                });
            }
            UndoOp::Deleted { start, text, cursor_before, .. } => {
                let end = self.splice_in_cps(start, &text);
                self.cursor = cursor_before.min(self.buffer.len());
                self.anchor = self.cursor;
                self.redo.push(UndoOp::Deleted {
                    start,
                    text,
                    cursor_before,
                    cursor_after: end,
                });
            }
        }
        self.coalesce = None;
        self.dirty = true;
    }

    pub fn redo(&mut self) {
        let Some(op) = self.redo.pop() else { return };
        match op {
            UndoOp::Inserted { start, text, cursor_before, cursor_after } => {
                let end = self.splice_in_cps(start, &text);
                self.cursor = end;
                self.anchor = end;
                self.push_undo(UndoOp::Inserted { start, text, cursor_before, cursor_after });
            }
            UndoOp::Deleted { start, text, cursor_before, cursor_after } => {
                self.splice_out(start, start + text.len());
                self.cursor = start;
                self.anchor = start;
                self.push_undo(UndoOp::Deleted { start, text, cursor_before, cursor_after });
            }
        }
        self.coalesce = None;
        self.dirty = true;
    }

    // ─── movement ────────────────────────────────────────────────────

    fn move_left(&mut self, extend: bool) {
        if self.cursor > 0 {
            self.set_cursor_offset(self.cursor - 1, extend);
        } else if !extend {
            self.anchor = self.cursor;
        }
        self.pref_col = self.cursor_rc().1;
    }
    fn move_right(&mut self, extend: bool) {
        if self.cursor < self.buffer.len() {
            self.set_cursor_offset(self.cursor + 1, extend);
        }
        self.pref_col = self.cursor_rc().1;
    }
    fn move_up(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        if row > 0 {
            self.set_cursor_rc(row - 1, self.pref_col, extend);
        } else {
            self.set_cursor_offset(0, extend);
        }
    }
    fn move_down(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        if row + 1 < self.buffer.line_count() {
            self.set_cursor_rc(row + 1, self.pref_col, extend);
        } else {
            self.set_cursor_offset(self.buffer.len(), extend);
        }
    }
    fn move_home(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        self.set_cursor_rc(row, 0, extend);
        self.pref_col = 0;
    }
    fn move_end(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        let len = self.line_len_cps(row);
        self.set_cursor_rc(row, len, extend);
        self.pref_col = len;
    }
    fn page_up(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        let step = self.visible_rows.max(1);
        self.set_cursor_rc(row.saturating_sub(step), self.pref_col, extend);
    }
    fn page_down(&mut self, extend: bool) {
        let (row, _) = self.cursor_rc();
        let step = self.visible_rows.max(1);
        self.set_cursor_rc(row + step, self.pref_col, extend);
    }
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.buffer.len();
        self.coalesce = None;
    }

    // ─── clipboard ───────────────────────────────────────────────────

    pub fn copy(&mut self) {
        if let Some((lo, hi)) = self.selection_range() {
            self.clipboard = self.buffer.slice(lo, hi);
        }
    }
    pub fn cut(&mut self) {
        if self.selection_range().is_some() {
            self.copy();
            self.delete_selection_to_undo();
            self.redo.clear();
        }
    }
    pub fn paste(&mut self) {
        if !self.clipboard.is_empty() {
            let s = codepoints_to_utf8(&self.clipboard);
            self.do_insert(&s, None);
        }
    }

    // ─── input ───────────────────────────────────────────────────────

    /// Handle a key-down. `vkey` is a Win32 VK code (see
    /// `igui_mac::events`), `mods` the `igui_events::modifier::*` bits.
    /// Returns true if the view should be repainted.
    pub fn on_key(&mut self, vkey: i64, mods: i64) -> bool {
        use crate::igui_events::modifier;
        let shift = mods & modifier::SHIFT != 0;
        // Command (macOS) maps to the WIN bit; treat it as the editor's
        // accelerator modifier, like Ctrl on Windows.
        let cmd = mods & (modifier::WIN | modifier::CONTROL) != 0;

        if cmd {
            match vkey {
                0x41 => self.select_all(),         // Cmd-A
                0x43 => self.copy(),               // Cmd-C
                0x58 => self.cut(),                // Cmd-X
                0x56 => self.paste(),              // Cmd-V
                0x5A if shift => self.redo(),      // Cmd-Shift-Z
                0x5A => self.undo(),               // Cmd-Z
                _ => return false,
            }
            return true;
        }

        match vkey {
            vk::LEFT => self.move_left(shift),
            vk::RIGHT => self.move_right(shift),
            vk::UP => self.move_up(shift),
            vk::DOWN => self.move_down(shift),
            vk::HOME => self.move_home(shift),
            vk::END => self.move_end(shift),
            vk::PRIOR => self.page_up(shift),
            vk::NEXT => self.page_down(shift),
            vk::BACK => self.backspace(),
            vk::DELETE => self.delete_forward(),
            vk::RETURN => self.insert_newline(),
            vk::TAB => self.insert_str("  "),
            _ => return false,
        }
        true
    }

    /// Handle a typed character. Ignores control codes (those arrive as
    /// keys). Returns true if the view should repaint.
    pub fn on_char(&mut self, cp: u32) -> bool {
        if let Some(c) = char::from_u32(cp) {
            if c.is_control() {
                return false;
            }
            self.insert_char(cp);
            return true;
        }
        false
    }

    /// Place the cursor at a pixel position within the text area (the
    /// area `render` was given), selecting if `extend`.
    pub fn on_click(&mut self, x: f32, y: f32, area: Rect, extend: bool) {
        let gutter = self.gutter_w();
        let text_x = (x - area.x0 - gutter).max(0.0);
        let text_y = (y - area.y0).max(0.0);
        let row = self.scroll_top + (text_y / self.theme.cell_h) as usize;
        let col = ((text_x / self.theme.cell_w) + 0.5) as usize;
        self.set_cursor_rc(row, col, extend);
        self.pref_col = col;
    }

    /// Scroll by `lines` (positive = down).
    pub fn scroll(&mut self, lines: i64) {
        let max_top = self.buffer.line_count().saturating_sub(1);
        let new = self.scroll_top as i64 + lines;
        self.scroll_top = new.clamp(0, max_top as i64) as usize;
    }

    fn gutter_w(&self) -> f32 {
        if !self.show_gutter {
            return 0.0;
        }
        let digits = (self.buffer.line_count().max(1) as f32).log10().floor() as usize + 1;
        (digits.max(3) as f32 + 1.5) * self.theme.cell_w
    }

    fn ensure_cursor_visible(&mut self) {
        let (row, _) = self.cursor_rc();
        if row < self.scroll_top {
            self.scroll_top = row;
        } else if self.visible_rows > 0 && row >= self.scroll_top + self.visible_rows {
            self.scroll_top = row + 1 - self.visible_rows;
        }
    }

    // ─── render ──────────────────────────────────────────────────────

    /// Produce the `SurfaceCmd`s to draw the editor into `area`. Includes
    /// the background fill, optional gutter, visible text lines, the
    /// selection highlight, and the caret.
    pub fn render(&mut self, area: Rect) -> Vec<SurfaceCmd> {
        let t = self.theme.clone();
        let cell_h = t.cell_h.max(1.0);
        let cell_w = t.cell_w.max(1.0);
        let area_h = (area.y1 - area.y0).max(0.0);
        self.visible_rows = (area_h / cell_h).floor().max(1.0) as usize;
        self.ensure_cursor_visible();

        let gutter = self.gutter_w();
        let text_x0 = area.x0 + gutter;
        let mut cmds = Vec::new();
        cmds.push(SurfaceCmd::Clear { color: t.bg });
        cmds.push(SurfaceCmd::PushClipRect { rect: area });

        let line_count = self.buffer.line_count();
        let (cur_row, cur_col) = self.cursor_rc();
        let sel = self.selection_range();

        let mk_run = |s: String, x: f32, y: f32, color: Rgba| SurfaceCmd::DrawTextRun {
            run: TextRun {
                text: s,
                origin: Point { x, y },
                family: t.family.clone(),
                size: t.size,
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::Normal,
                locale: "en-us".into(),
                color,
                max_width: None,
                alignment: TextAlign::Leading,
                trimming: TextTrimming::None,
            },
        };

        for vis in 0..self.visible_rows {
            let row = self.scroll_top + vis;
            if row >= line_count {
                break;
            }
            let y = area.y0 + vis as f32 * cell_h;

            // Selection highlight for this row.
            if let Some((lo, hi)) = sel {
                if let Some((rs, re)) = self.buffer.line_range(row) {
                    let row_lo = lo.max(rs);
                    let row_hi = hi.min(re.max(rs));
                    // Extend selection one cell past EOL when the newline
                    // is inside the selection, for a visible line break.
                    let sel_to_eol = hi > re;
                    if row_lo < row_hi || (sel_to_eol && lo <= rs) {
                        let c0 = row_lo - rs;
                        let c1 = if sel_to_eol { (re - rs) + 1 } else { row_hi - rs };
                        cmds.push(SurfaceCmd::SelectionRange {
                            rect: Rect {
                                x0: text_x0 + c0 as f32 * cell_w,
                                y0: y,
                                x1: text_x0 + c1 as f32 * cell_w,
                                y1: y + cell_h,
                            },
                            color: t.selection,
                        });
                    }
                }
            }

            // Gutter line number.
            if self.show_gutter {
                cmds.push(mk_run(
                    format!("{:>1$}", row + 1, (gutter / cell_w) as usize - 1),
                    area.x0,
                    y,
                    t.gutter_fg,
                ));
            }

            // Line text.
            let line = self.line_text(row);
            if !line.is_empty() {
                cmds.push(mk_run(line, text_x0, y, t.fg));
            }
        }

        // Caret.
        if cur_row >= self.scroll_top && cur_row < self.scroll_top + self.visible_rows {
            let cy = area.y0 + (cur_row - self.scroll_top) as f32 * cell_h;
            let cx = text_x0 + cur_col as f32 * cell_w;
            cmds.push(SurfaceCmd::Caret {
                rect: Rect { x0: cx, y0: cy, x1: cx + 2.0, y1: cy + cell_h },
                color: t.caret,
            });
        }

        cmds.push(SurfaceCmd::PopClipRect);
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect { x0: 0.0, y0: 0.0, x1: 400.0, y1: 200.0 }
    }

    #[test]
    fn typing_inserts_and_moves_cursor() {
        let mut e = Editor::new();
        for c in "hello".chars() {
            e.on_char(c as u32);
        }
        assert_eq!(e.text(), "hello");
        assert_eq!(e.cursor_rc(), (0, 5));
    }

    #[test]
    fn newline_auto_indents() {
        let mut e = Editor::with_text("  foo");
        e.set_cursor_offset(5, false); // end of "  foo"
        e.insert_newline();
        assert_eq!(e.text(), "  foo\n  ");
        assert_eq!(e.cursor_rc(), (1, 2));
    }

    #[test]
    fn backspace_and_undo_redo() {
        let mut e = Editor::new();
        e.insert_str("abc");
        e.backspace();
        assert_eq!(e.text(), "ab");
        e.undo();
        assert_eq!(e.text(), "abc");
        e.undo();
        assert_eq!(e.text(), "");
        e.redo();
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn coalesced_typing_is_one_undo() {
        let mut e = Editor::new();
        for c in "word".chars() {
            e.insert_char(c as u32);
        }
        e.undo();
        assert_eq!(e.text(), "", "coalesced typing should undo in one step");
    }

    #[test]
    fn selection_copy_paste() {
        let mut e = Editor::with_text("abcdef");
        e.set_cursor_offset(0, false);
        e.set_cursor_offset(3, true); // select "abc"
        assert_eq!(e.selected_text(), "abc");
        e.copy();
        e.set_cursor_offset(6, false);
        e.paste();
        assert_eq!(e.text(), "abcdefabc");
    }

    #[test]
    fn vertical_motion_keeps_pref_col() {
        let mut e = Editor::with_text("longline\nx\nanother");
        // Position at col 6 the way a user would (horizontal motion sets
        // the preferred column).
        for _ in 0..6 {
            e.on_key(vk::RIGHT, 0);
        }
        assert_eq!(e.cursor_rc(), (0, 6));
        e.on_key(vk::DOWN, 0); // to short line "x" → col clamps to 1
        e.on_key(vk::DOWN, 0); // to "another" → pref_col 6 restored
        assert_eq!(e.cursor_rc(), (2, 6));
    }

    #[test]
    fn render_emits_text_and_caret() {
        let mut e = Editor::with_text("(defun hi () 42)");
        let cmds = e.render(area());
        let has_text = cmds
            .iter()
            .any(|c| matches!(c, SurfaceCmd::DrawTextRun { run } if run.text.contains("defun")));
        let has_caret = cmds.iter().any(|c| matches!(c, SurfaceCmd::Caret { .. }));
        assert!(has_text, "editor should render its text");
        assert!(has_caret, "editor should render a caret");
    }
}
