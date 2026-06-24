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
use crate::igui_mac::ide::sexp;
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

/// Active incremental-search state.
struct Search {
    query: String,
    /// Code-point offsets of every (case-insensitive) match start.
    matches: Vec<usize>,
    /// Index into `matches` of the current selection.
    idx: usize,
    /// Cursor offset when search began; new queries select the first match
    /// at/after this point (so the selection doesn't drift while typing).
    start: usize,
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
    /// Active incremental search, if the user is searching (Cmd-F).
    search: Option<Search>,
    /// Backing file, if the buffer was loaded from or saved to one.
    file_path: Option<String>,
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
            search: None,
            file_path: None,
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
    /// Move the cursor to a code-point offset (clamped), collapsing any
    /// selection.
    pub fn set_cursor(&mut self, offset: usize) {
        self.set_cursor_offset(offset, false);
        self.pref_col = self.cursor_rc().1;
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
    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    /// Load `path` into the buffer, replacing its contents.
    pub fn load_file(&mut self, path: &str) -> std::io::Result<()> {
        let s = std::fs::read_to_string(path)?;
        self.set_text(&s);
        self.set_cursor(0);
        self.file_path = Some(path.to_string());
        self.dirty = false;
        Ok(())
    }

    /// Save to the backing file. `Ok(Some(path))` on success, `Ok(None)`
    /// if there is no backing file yet.
    pub fn save(&mut self) -> std::io::Result<Option<String>> {
        match self.file_path.clone() {
            Some(p) => {
                std::fs::write(&p, self.text())?;
                self.dirty = false;
                Ok(Some(p))
            }
            None => Ok(None),
        }
    }

    /// A status string: `path • L:C • [modified]`.
    pub fn status(&self) -> String {
        let (r, c) = self.cursor_rc();
        let name = self.file_path.as_deref().unwrap_or("«unsaved»");
        let dirty = if self.dirty { " • modified" } else { "" };
        format!("{name}  {}:{}{dirty}", r + 1, c + 1)
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

    /// The top-level form whose paren range contains the cursor, as a
    /// string. Used by "eval form at point". `None` if the cursor isn't
    /// inside any top-level form. Bracket scan ignores strings/comments.
    pub fn current_form(&self) -> Option<String> {
        let cps = self.buffer.to_slice();
        let cur = self.cursor.min(cps.len());
        let mut depth: i32 = 0;
        let mut start: Option<usize> = None;
        let mut in_string = false;
        let mut in_comment = false;
        let mut escape = false;
        let mut best: Option<(usize, usize)> = None;
        for (i, &cp) in cps.iter().enumerate() {
            let c = char::from_u32(cp).unwrap_or(' ');
            if in_comment {
                if c == '\n' {
                    in_comment = false;
                }
                continue;
            }
            if in_string {
                if escape {
                    escape = false;
                } else if c == '\\' {
                    escape = true;
                } else if c == '"' {
                    in_string = false;
                }
                continue;
            }
            match c {
                ';' => in_comment = true,
                '"' => in_string = true,
                '(' | '[' => {
                    if depth == 0 {
                        start = Some(i);
                    }
                    depth += 1;
                }
                ')' | ']' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(s) = start.take() {
                            let e = i + 1;
                            if cur >= s && cur <= e {
                                best = Some((s, e));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        best.map(|(s, e)| codepoints_to_utf8(&cps[s..e]))
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
        self.do_insert("\n", None);
        self.reindent_line();
    }

    /// Lisp-aware indentation for a line starting at `pos`: align the body
    /// under the first argument of the enclosing form, or `open_col + 2`
    /// for a special form, or `open_col + 1` otherwise. Top level → 0.
    fn compute_indent(&self, pos: usize) -> usize {
        let cs = self.chars();
        let Some(open) = sexp::enclosing_open(&cs, pos) else {
            return 0;
        };
        let (open_line, open_col) = self.buffer.offset_to_line_col(open);
        let op_start = sexp::skip_ws_fwd(&cs, open + 1);
        if op_start >= pos || op_start >= cs.len() {
            return open_col + 1;
        }
        // Operator that is itself a list → align one past the open paren.
        if matches!(cs[op_start], '(' | '[') {
            return open_col + 1;
        }
        let mut op_end = op_start;
        while op_end < cs.len() && !is_delim(cs[op_end]) {
            op_end += 1;
        }
        let op: String = cs[op_start..op_end].iter().collect();
        if is_special(&op) {
            return open_col + 2;
        }
        // Align under the first argument if it's on the operator's line.
        let arg_start = sexp::skip_ws_fwd(&cs, op_end);
        if arg_start < pos && !matches!(cs.get(arg_start), Some(')') | Some(']') | None) {
            let (arg_line, arg_col) = self.buffer.offset_to_line_col(arg_start);
            if arg_line == open_line {
                return arg_col;
            }
        }
        op_start - self.buffer.line_col_to_offset(open_line, 0) // operator column
    }

    /// Re-indent the current line to its computed Lisp indentation.
    pub fn reindent_line(&mut self) {
        let (row, _) = self.cursor_rc();
        let line_start = self.buffer.line_col_to_offset(row, 0);
        let cs = self.chars();
        let mut ws = 0;
        while line_start + ws < cs.len() && matches!(cs[line_start + ws], ' ' | '\t') {
            ws += 1;
        }
        let want = self.compute_indent(line_start);
        if want == ws {
            return;
        }
        let cursor_in_line = self.cursor as isize - line_start as isize;
        if ws > 0 {
            self.edit_delete(line_start, line_start + ws);
        }
        if want > 0 {
            let spaces: Vec<u32> = std::iter::repeat_n(' ' as u32, want).collect();
            self.edit_insert(line_start, &spaces);
        }
        let delta = want as isize - ws as isize;
        let new_in_line = if cursor_in_line <= ws as isize {
            want as isize
        } else {
            cursor_in_line + delta
        };
        self.cursor = (line_start as isize + new_in_line).max(line_start as isize) as usize;
        self.cursor = self.cursor.min(self.buffer.len());
        self.anchor = self.cursor;
        self.pref_col = self.cursor_rc().1;
        self.dirty = true;
        self.redo.clear();
        self.coalesce = None;
    }

    /// Re-indent every line touched by the selection (or the current line).
    /// Processes top-down so each line sees the corrected indentation above.
    pub fn reindent_selection(&mut self) {
        let (lo, hi) = match self.selection_range() {
            Some((a, b)) => (
                self.buffer.offset_to_line_col(a).0,
                self.buffer.offset_to_line_col(b.saturating_sub(1).max(a)).0,
            ),
            None => {
                let r = self.cursor_rc().0;
                (r, r)
            }
        };
        for row in lo..=hi {
            let off = self.buffer.line_col_to_offset(row, 0);
            self.cursor = off;
            self.anchor = off;
            self.reindent_line();
        }
    }

    /// Duplicate the current line below the cursor.
    pub fn duplicate_line(&mut self) {
        let (row, col) = self.cursor_rc();
        let line = self.line_text(row);
        let line_end = self.buffer.line_col_to_offset(row, 0) + self.line_len_cps(row);
        let text = format!("\n{line}");
        self.edit_insert(line_end, &utf8_to_codepoints(text.as_bytes()));
        let nc = self.buffer.line_col_to_offset(row + 1, col);
        self.finish_structural_edit(nc);
    }

    /// Delete the current line.
    pub fn delete_line(&mut self) {
        let (row, _) = self.cursor_rc();
        let n = self.buffer.line_count();
        let line_start = self.buffer.line_col_to_offset(row, 0);
        let (lo, hi) = if row + 1 < n {
            (line_start, self.buffer.line_col_to_offset(row + 1, 0))
        } else if row > 0 {
            (line_start.saturating_sub(1), self.buffer.len())
        } else {
            (line_start, self.buffer.len())
        };
        self.edit_delete(lo, hi);
        self.finish_structural_edit(lo.min(self.buffer.len()));
    }

    /// Swap the current line with the one below.
    pub fn move_line_down(&mut self) {
        let (row, col) = self.cursor_rc();
        if row + 1 >= self.buffer.line_count() {
            return;
        }
        let cur = self.line_text(row);
        let next = self.line_text(row + 1);
        let start = self.buffer.line_col_to_offset(row, 0);
        let end = self.buffer.line_col_to_offset(row + 1, 0) + self.line_len_cps(row + 1);
        let replacement = format!("{next}\n{cur}");
        self.edit_delete(start, end);
        self.edit_insert(start, &utf8_to_codepoints(replacement.as_bytes()));
        let nc = self.buffer.line_col_to_offset(row + 1, col.min(self.line_len_cps(row + 1)));
        self.finish_structural_edit(nc);
    }

    /// Swap the current line with the one above.
    pub fn move_line_up(&mut self) {
        let (row, col) = self.cursor_rc();
        if row == 0 {
            return;
        }
        let prev = self.line_text(row - 1);
        let cur = self.line_text(row);
        let start = self.buffer.line_col_to_offset(row - 1, 0);
        let end = self.buffer.line_col_to_offset(row, 0) + self.line_len_cps(row);
        let replacement = format!("{cur}\n{prev}");
        self.edit_delete(start, end);
        self.edit_insert(start, &utf8_to_codepoints(replacement.as_bytes()));
        let nc = self.buffer.line_col_to_offset(row - 1, col.min(self.line_len_cps(row)));
        self.finish_structural_edit(nc);
    }

    /// Move to the start of the next word (Option-Right).
    pub fn word_right(&mut self, extend: bool) {
        let cs = self.chars();
        let mut i = self.cursor;
        while i < cs.len() && is_delim(cs[i]) {
            i += 1;
        }
        while i < cs.len() && !is_delim(cs[i]) {
            i += 1;
        }
        self.set_cursor_offset(i, extend);
        self.pref_col = self.cursor_rc().1;
    }

    /// Move to the start of the previous word (Option-Left).
    pub fn word_left(&mut self, extend: bool) {
        let cs = self.chars();
        let mut i = self.cursor;
        while i > 0 && is_delim(cs[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_delim(cs[i - 1]) {
            i -= 1;
        }
        self.set_cursor_offset(i, extend);
        self.pref_col = self.cursor_rc().1;
    }

    /// Delete from the cursor back to the previous word boundary
    /// (Option-Backspace).
    pub fn delete_word_back(&mut self) {
        if self.delete_selection_to_undo() {
            self.redo.clear();
            return;
        }
        let cs = self.chars();
        let mut i = self.cursor;
        while i > 0 && is_delim(cs[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_delim(cs[i - 1]) {
            i -= 1;
        }
        if i < self.cursor {
            self.edit_delete(i, self.cursor);
            self.finish_structural_edit(i);
        }
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

    // ─── s-expression navigation + paredit ───────────────────────────

    fn chars(&self) -> Vec<char> {
        self.buffer.to_slice().iter().filter_map(|&c| char::from_u32(c)).collect()
    }

    /// Delete `[lo, hi)`, recording undo.
    fn edit_delete(&mut self, lo: usize, hi: usize) {
        if lo >= hi {
            return;
        }
        let cursor_before = self.cursor;
        let removed = self.splice_out(lo, hi);
        self.push_undo(UndoOp::Deleted {
            start: lo,
            text: removed,
            cursor_before,
            cursor_after: lo,
        });
    }

    /// Insert `text` at `at`, recording undo.
    fn edit_insert(&mut self, at: usize, text: &[u32]) {
        if text.is_empty() {
            return;
        }
        let cursor_before = self.cursor;
        let end = self.splice_in_cps(at, text);
        self.push_undo(UndoOp::Inserted {
            start: at,
            text: text.to_vec(),
            cursor_before,
            cursor_after: end,
        });
    }

    fn finish_structural_edit(&mut self, new_cursor: usize) {
        self.cursor = new_cursor.min(self.buffer.len());
        self.anchor = self.cursor;
        self.pref_col = self.cursor_rc().1;
        self.dirty = true;
        self.redo.clear();
        self.coalesce = None;
    }

    pub fn move_forward_sexp(&mut self, extend: bool) {
        let cs = self.chars();
        if let Some(end) = sexp::forward_sexp(&cs, self.cursor) {
            self.set_cursor_offset(end, extend);
            self.pref_col = self.cursor_rc().1;
        }
    }
    pub fn move_backward_sexp(&mut self, extend: bool) {
        let cs = self.chars();
        if let Some(start) = sexp::backward_sexp(&cs, self.cursor) {
            self.set_cursor_offset(start, extend);
            self.pref_col = self.cursor_rc().1;
        }
    }

    /// Pull the next sibling sexp into the enclosing form (paredit
    /// slurp-forward): `(a| b) c` → `(a| b c)`.
    pub fn slurp_forward(&mut self) {
        let cs = self.chars();
        let Some((_open, close)) = sexp::enclosing(&cs, self.cursor) else { return };
        let Some((_s, nend)) = sexp::sexp_after(&cs, close + 1) else { return };
        let bracket = cs[close] as u32;
        // Move the close bracket from `close` to just after the slurped sexp.
        self.edit_delete(close, close + 1);
        self.edit_insert(nend - 1, &[bracket]);
        self.finish_structural_edit(self.cursor);
    }

    /// Push the last element out of the enclosing form (paredit
    /// barf-forward): `(a b| c)` → `(a b|) c`.
    pub fn barf_forward(&mut self) {
        let cs = self.chars();
        let Some((open, close)) = sexp::enclosing(&cs, self.cursor) else { return };
        let Some(last_start) = sexp::backward_sexp(&cs, close) else { return };
        if last_start <= open + 1 {
            return; // nothing left to barf
        }
        // Insert a close bracket before the last element (after trimming the
        // whitespace that precedes it), then remove the original close.
        let mut insert_pos = last_start;
        while insert_pos > open + 1 && cs[insert_pos - 1].is_whitespace() {
            insert_pos -= 1;
        }
        let bracket = cs[close] as u32;
        self.edit_delete(close, close + 1);
        self.edit_insert(insert_pos, &[bracket]);
        let nc = self.cursor.min(insert_pos);
        self.finish_structural_edit(nc);
    }

    /// Wrap the sexp at the cursor in a fresh `( … )` (paredit wrap-round).
    pub fn wrap_round(&mut self) {
        let cs = self.chars();
        let Some((s, e)) = sexp::sexp_after(&cs, self.cursor) else { return };
        self.edit_insert(e, &[')' as u32]); // higher offset first
        self.edit_insert(s, &['(' as u32]);
        self.finish_structural_edit(s + 1);
    }

    /// Remove the brackets of the enclosing form (paredit splice):
    /// `(a (b| c) d)` → `(a b| c d)`.
    pub fn splice(&mut self) {
        let cs = self.chars();
        let Some((open, close)) = sexp::enclosing(&cs, self.cursor) else { return };
        self.edit_delete(close, close + 1); // higher offset first
        self.edit_delete(open, open + 1);
        let nc = if self.cursor > open { self.cursor - 1 } else { self.cursor };
        self.finish_structural_edit(nc);
    }

    /// Replace the enclosing form with the sexp at the cursor (paredit
    /// raise): `(a (b| c) d)` with point on `(b c)` → `(b| c)`.
    pub fn raise(&mut self) {
        let cs = self.chars();
        let Some((s, e)) = sexp::sexp_after(&cs, self.cursor) else { return };
        let Some((open, close)) = sexp::enclosing(&cs, self.cursor) else { return };
        if s < open || e > close + 1 {
            return;
        }
        let inner: Vec<u32> = cs[s..e].iter().map(|&c| c as u32).collect();
        self.edit_delete(open, close + 1); // remove whole form
        self.edit_insert(open, &inner);
        self.finish_structural_edit(open);
    }

    /// Toggle `;; ` line comments over the current line or selection. If
    /// every affected (non-blank) line is already commented, uncomment;
    /// otherwise comment. Lines are edited bottom-up so offsets stay valid.
    pub fn toggle_comment(&mut self) {
        let (lo_row, hi_row) = match self.selection_range() {
            Some((a, b)) => {
                let ar = self.buffer.offset_to_line_col(a).0;
                let br = self.buffer.offset_to_line_col(b.saturating_sub(1).max(a)).0;
                (ar, br)
            }
            None => {
                let r = self.cursor_rc().0;
                (r, r)
            }
        };
        let all_commented = (lo_row..=hi_row).all(|r| {
            let t = self.line_text(r);
            let tr = t.trim_start();
            tr.is_empty() || tr.starts_with(';')
        });
        for r in (lo_row..=hi_row).rev() {
            let t = self.line_text(r);
            if t.trim().is_empty() {
                continue;
            }
            let lead_ws = t.chars().take_while(|c| *c == ' ' || *c == '\t').count();
            let line_start = self.buffer.line_col_to_offset(r, 0);
            let at = line_start + lead_ws;
            if all_commented {
                let cs = self.chars();
                let mut p = at;
                while p < cs.len() && cs[p] == ';' {
                    p += 1;
                }
                if p > at {
                    if p < cs.len() && cs[p] == ' ' {
                        p += 1;
                    }
                    self.edit_delete(at, p);
                }
            } else {
                self.edit_insert(at, &[';' as u32, ';' as u32, ' ' as u32]);
            }
        }
        self.cursor = self.cursor.min(self.buffer.len());
        self.anchor = self.cursor;
        self.pref_col = self.cursor_rc().1;
        self.dirty = true;
        self.redo.clear();
        self.coalesce = None;
    }

    /// The matching-bracket offset for a bracket adjacent to the cursor,
    /// for paren-match highlighting. Returns `(here, there)` bracket
    /// offsets, or `None`.
    fn match_paren(&self) -> Option<(usize, usize)> {
        let cs = self.chars();
        let cur = self.cursor;
        // Bracket just after the cursor.
        if cur < cs.len() {
            if matches!(cs[cur], '(' | '[') {
                if let Some(c) = sexp::matching_close(&cs, cur) {
                    return Some((cur, c));
                }
            }
        }
        // Bracket just before the cursor.
        if cur > 0 {
            if matches!(cs[cur - 1], ')' | ']') {
                if let Some(o) = sexp::matching_open(&cs, cur - 1) {
                    return Some((cur - 1, o));
                }
            }
        }
        None
    }

    // ─── incremental search ──────────────────────────────────────────

    pub fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    /// Start (or step) incremental search. If already searching, jump to
    /// the next match.
    pub fn start_search(&mut self) {
        if self.search.is_some() {
            self.search_next(false);
        } else {
            self.search = Some(Search {
                query: String::new(),
                matches: Vec::new(),
                idx: 0,
                start: self.cursor,
            });
        }
    }

    pub fn cancel_search(&mut self) {
        self.search = None;
    }

    fn recompute_matches(&mut self) {
        let Some(search) = self.search.as_ref() else { return };
        let q: Vec<char> = search.query.to_lowercase().chars().collect();
        let mut matches = Vec::new();
        if !q.is_empty() {
            let text = self.chars();
            let lower: Vec<char> = text.iter().flat_map(|c| c.to_lowercase()).collect();
            // to_lowercase can change length; fall back to per-char compare on
            // the original to keep offsets meaningful.
            if lower.len() == text.len() {
                let n = text.len();
                let m = q.len();
                if m <= n {
                    let mut i = 0;
                    while i + m <= n {
                        if lower[i..i + m] == q[..] {
                            matches.push(i);
                        }
                        i += 1;
                    }
                }
            }
        }
        // Pick the first match at/after the search start (stable while typing).
        let start = self.search.as_ref().map(|s| s.start).unwrap_or(0);
        let idx = matches.iter().position(|&m| m >= start).unwrap_or(0);
        if let Some(s) = self.search.as_mut() {
            s.matches = matches;
            s.idx = idx;
        }
        self.jump_to_current_match();
    }

    fn jump_to_current_match(&mut self) {
        if let Some(s) = self.search.as_ref() {
            if let Some(&off) = s.matches.get(s.idx) {
                let qlen = s.query.chars().count();
                self.cursor = (off + qlen).min(self.buffer.len());
                self.anchor = off;
                self.ensure_cursor_visible();
            }
        }
    }

    pub fn search_next(&mut self, backward: bool) {
        if let Some(s) = self.search.as_mut() {
            if s.matches.is_empty() {
                return;
            }
            let n = s.matches.len();
            s.idx = if backward {
                (s.idx + n - 1) % n
            } else {
                (s.idx + 1) % n
            };
        }
        self.jump_to_current_match();
    }

    fn search_push(&mut self, c: char) {
        if let Some(s) = self.search.as_mut() {
            s.query.push(c);
        }
        self.recompute_matches();
    }

    fn search_backspace(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.query.pop();
        }
        self.recompute_matches();
    }

    // ─── input ───────────────────────────────────────────────────────

    /// Handle a key-down. `vkey` is a Win32 VK code (see
    /// `igui_mac::events`), `mods` the `igui_events::modifier::*` bits.
    /// Returns true if the view should be repainted.
    pub fn on_key(&mut self, vkey: i64, mods: i64) -> bool {
        use crate::igui_events::modifier;
        let shift = mods & modifier::SHIFT != 0;
        // macOS Command (WIN bit) is the editor accelerator (clipboard,
        // undo); Control (CONTROL bit) drives paredit, as in Emacs/SLIME.
        let cmd = mods & modifier::WIN != 0;
        let ctrl = mods & modifier::CONTROL != 0;
        let alt = mods & modifier::ALT != 0;

        // Incremental-search mode swallows most keys.
        if self.search.is_some() && !cmd {
            match vkey {
                vk::ESCAPE => self.cancel_search(),
                vk::RETURN => self.search_next(shift),
                vk::BACK => self.search_backspace(),
                _ => self.cancel_search(), // any other key ends search, then falls through
            }
            if vkey == vk::ESCAPE || vkey == vk::RETURN || vkey == vk::BACK {
                return true;
            }
        }

        if cmd {
            match vkey {
                0x46 => self.start_search(),       // Cmd-F (start / next)
                0x47 => self.search_next(shift),   // Cmd-G (next / prev with Shift)
                0x41 => self.select_all(),         // Cmd-A
                0x43 => self.copy(),               // Cmd-C
                0x58 => self.cut(),                // Cmd-X
                0x56 => self.paste(),              // Cmd-V
                0x5A if shift => self.redo(),      // Cmd-Shift-Z
                0x5A => self.undo(),               // Cmd-Z
                0x44 => self.duplicate_line(),       // Cmd-D
                0x4B if shift => self.delete_line(), // Cmd-Shift-K
                vk::OEM_2 => self.toggle_comment(), // Cmd-/
                _ => return false,
            }
            return true;
        }

        if alt {
            match vkey {
                vk::UP => self.move_line_up(),         // Alt-Up
                vk::DOWN => self.move_line_down(),     // Alt-Down
                vk::LEFT => self.word_left(shift),     // Alt-Left
                vk::RIGHT => self.word_right(shift),   // Alt-Right
                vk::BACK => self.delete_word_back(),   // Alt-Backspace
                _ => return false,
            }
            return true;
        }

        if ctrl {
            match vkey {
                vk::RIGHT if shift => self.slurp_forward(), // Ctrl-Shift-Right
                vk::LEFT if shift => self.barf_forward(),   // Ctrl-Shift-Left
                vk::RIGHT => self.move_forward_sexp(false), // Ctrl-Right
                vk::LEFT => self.move_backward_sexp(false), // Ctrl-Left
                0x57 => self.wrap_round(), // Ctrl-W
                0x53 => self.splice(),     // Ctrl-S
                0x52 => self.raise(),      // Ctrl-R
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
            vk::TAB => {
                if self.selection_range().is_some() {
                    self.reindent_selection();
                } else {
                    self.reindent_line();
                }
            }
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
            if self.search.is_some() {
                self.search_push(c);
                return true;
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

        let tok_color = |tok: Tok| match tok {
            Tok::Special => t.c_special,
            Tok::Keyword => t.c_keyword,
            Tok::Number => t.c_number,
            Tok::StringLit => t.c_string,
            Tok::Char => t.c_char,
            Tok::Comment => t.c_comment,
            Tok::Paren => t.c_paren,
            Tok::Quote => t.c_quote,
            Tok::Symbol => t.fg,
        };
        let line_chars = |buf: &RopeBuffer, row: usize| -> Vec<char> {
            buf.get_line(row).iter().filter_map(|&c| char::from_u32(c)).collect()
        };

        // Recover the multi-line-string state at the first visible row by
        // tokenising everything above it (cheap for the files this targets).
        let mut in_string = false;
        for row in 0..self.scroll_top.min(line_count) {
            let (_, ns) = tokenize_line(&line_chars(&self.buffer, row), in_string);
            in_string = ns;
        }

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

            // Line text, syntax-highlighted (one run per token span).
            let chars = line_chars(&self.buffer, row);
            let (spans, ns) = tokenize_line(&chars, in_string);
            in_string = ns;
            for (s, e, tok) in spans {
                if s >= e {
                    continue;
                }
                let text: String = chars[s..e].iter().collect();
                cmds.push(mk_run(text, text_x0 + s as f32 * cell_w, y, tok_color(tok)));
            }
        }

        // Paren-match highlight: outline both brackets when the cursor is
        // beside one and its partner is visible.
        if let Some((a, b)) = self.match_paren() {
            for off in [a, b] {
                let (r, c) = self.buffer.offset_to_line_col(off);
                if r >= self.scroll_top && r < self.scroll_top + self.visible_rows {
                    let py = area.y0 + (r - self.scroll_top) as f32 * cell_h;
                    let px = text_x0 + c as f32 * cell_w;
                    cmds.push(SurfaceCmd::StrokeRect {
                        rect: Rect { x0: px, y0: py, x1: px + cell_w, y1: py + cell_h },
                        corner_radius: 2.0,
                        half_thickness: 0.75,
                        color: t.c_paren,
                    });
                }
            }
        }

        // Incremental-search match highlights + a search bar.
        if let Some(s) = self.search.as_ref() {
            let qlen = s.query.chars().count().max(1) as f32;
            for (mi, &m) in s.matches.iter().enumerate() {
                let (r, c) = self.buffer.offset_to_line_col(m);
                if r >= self.scroll_top && r < self.scroll_top + self.visible_rows {
                    let my = area.y0 + (r - self.scroll_top) as f32 * cell_h;
                    let mx = text_x0 + c as f32 * cell_w;
                    let color = if mi == s.idx {
                        Rgba { r: 0.95, g: 0.75, b: 0.2, a: 0.55 }
                    } else {
                        Rgba { r: 0.5, g: 0.5, b: 0.3, a: 0.30 }
                    };
                    cmds.push(SurfaceCmd::SelectionRange {
                        rect: Rect { x0: mx, y0: my, x1: mx + qlen * cell_w, y1: my + cell_h },
                        color,
                    });
                }
            }
            let bar_y = area.y1 - cell_h - 4.0;
            cmds.push(SurfaceCmd::FillRect {
                rect: Rect { x0: area.x0, y0: bar_y, x1: area.x1, y1: area.y1 },
                corner_radius: 0.0,
                color: Rgba { r: 0.12, g: 0.13, b: 0.16, a: 1.0 },
            });
            let total = s.matches.len();
            let cur = if total == 0 { 0 } else { s.idx + 1 };
            cmds.push(mk_run(
                format!("⌕ {}    {cur}/{total}", s.query),
                area.x0 + 8.0,
                bar_y + 2.0,
                t.caret,
            ));
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
    fn newline_indents_inside_special_form() {
        // Inside an (unclosed) special form, the body indents to col 2.
        let mut e = Editor::with_text("(defun foo ()");
        e.set_cursor(e.text().chars().count());
        e.insert_newline();
        assert_eq!(e.text(), "(defun foo ()\n  ");
        assert_eq!(e.cursor_rc(), (1, 2));
    }

    #[test]
    fn newline_aligns_under_first_arg() {
        // A function call aligns the body under the first argument.
        let mut e = Editor::with_text("(+ 1 2");
        e.set_cursor(e.text().chars().count());
        e.insert_newline();
        // open at col 0, operator "+" at col 1, first arg "1" at col 3.
        assert_eq!(e.text(), "(+ 1 2\n   ");
        assert_eq!(e.cursor_rc(), (1, 3));
    }

    #[test]
    fn top_level_newline_no_indent() {
        let mut e = Editor::with_text("foo");
        e.set_cursor(3);
        e.insert_newline();
        assert_eq!(e.text(), "foo\n");
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
    fn tokenizer_classifies_lisp() {
        let chars: Vec<char> = "(defun foo (:x) ; note".chars().collect();
        let (spans, _) = tokenize_line(&chars, false);
        let kinds: Vec<Tok> = spans.iter().map(|(_, _, t)| *t).collect();
        assert!(kinds.contains(&Tok::Paren), "{kinds:?}");
        assert!(kinds.contains(&Tok::Special), "defun should be Special: {kinds:?}");
        assert!(kinds.contains(&Tok::Symbol), "foo should be Symbol");
        assert!(kinds.contains(&Tok::Keyword), ":x should be Keyword");
        assert!(kinds.contains(&Tok::Comment), "; note should be Comment");
    }

    #[test]
    fn tokenizer_numbers_strings_chars() {
        let chars: Vec<char> = "(+ 42 -1.5 \"hi\" #\\a)".chars().collect();
        let (spans, in_str) = tokenize_line(&chars, false);
        let kinds: Vec<Tok> = spans.iter().map(|(_, _, t)| *t).collect();
        assert!(kinds.contains(&Tok::Number), "42/-1.5 numbers: {kinds:?}");
        assert!(kinds.contains(&Tok::StringLit), "string");
        assert!(kinds.contains(&Tok::Char), "#\\a char literal");
        assert!(!in_str, "string closed on same line");
    }

    #[test]
    fn paredit_slurp_forward() {
        let mut e = Editor::with_text("(a b) c");
        e.set_cursor(2); // inside (a b)
        e.slurp_forward();
        assert_eq!(e.text(), "(a b c)");
    }

    #[test]
    fn paredit_barf_forward() {
        let mut e = Editor::with_text("(a b c)");
        e.set_cursor(2);
        e.barf_forward();
        assert_eq!(e.text(), "(a b) c");
    }

    #[test]
    fn paredit_wrap_round() {
        let mut e = Editor::with_text("foo bar");
        e.set_cursor(0);
        e.wrap_round();
        assert_eq!(e.text(), "(foo) bar");
        assert_eq!(e.cursor_rc(), (0, 1));
    }

    #[test]
    fn paredit_splice() {
        let mut e = Editor::with_text("(a (b c) d)");
        e.set_cursor(4); // inside (b c)
        e.splice();
        assert_eq!(e.text(), "(a b c d)");
    }

    #[test]
    fn paredit_raise() {
        let mut e = Editor::with_text("(a (b c) d)");
        e.set_cursor(3); // on the '(' of (b c)
        e.raise();
        assert_eq!(e.text(), "(b c)");
    }

    #[test]
    fn incremental_search_finds_and_cycles() {
        let mut e = Editor::with_text("foo bar foo baz foo");
        e.set_cursor(0);
        e.start_search();
        for c in "foo".chars() {
            e.on_char(c as u32);
        }
        // Three matches at offsets 0, 8, 16. Cursor jumps to first at/after 0.
        let s = e.search.as_ref().unwrap();
        assert_eq!(s.matches, vec![0, 8, 16]);
        assert_eq!(s.idx, 0);
        e.search_next(false);
        assert_eq!(e.search.as_ref().unwrap().idx, 1);
        e.search_next(true); // wrap back
        assert_eq!(e.search.as_ref().unwrap().idx, 0);
        e.cancel_search();
        assert!(!e.is_searching());
    }

    #[test]
    fn search_is_case_insensitive() {
        let mut e = Editor::with_text("Foo FOO foo");
        e.set_cursor(0);
        e.start_search();
        for c in "foo".chars() {
            e.on_char(c as u32);
        }
        assert_eq!(e.search.as_ref().unwrap().matches, vec![0, 4, 8]);
    }

    #[test]
    fn word_movement_and_delete() {
        let mut e = Editor::with_text("foo bar baz");
        e.set_cursor(0);
        e.word_right(false);
        assert_eq!(e.cursor_rc(), (0, 3)); // end of "foo"
        e.word_right(false);
        assert_eq!(e.cursor_rc(), (0, 7)); // end of "bar"
        e.word_left(false);
        assert_eq!(e.cursor_rc(), (0, 4)); // start of "bar"
        e.set_cursor(7); // after "bar"
        e.delete_word_back();
        assert_eq!(e.text(), "foo  baz");
    }

    #[test]
    fn line_ops_duplicate_delete_move() {
        let mut e = Editor::with_text("aaa\nbbb\nccc");
        e.set_cursor_rc(1, 0, false); // on "bbb"
        e.duplicate_line();
        assert_eq!(e.text(), "aaa\nbbb\nbbb\nccc");
        e.set_cursor_rc(1, 0, false);
        e.delete_line();
        assert_eq!(e.text(), "aaa\nbbb\nccc");
        e.set_cursor_rc(1, 0, false);
        e.move_line_down();
        assert_eq!(e.text(), "aaa\nccc\nbbb");
        e.set_cursor_rc(2, 0, false);
        e.move_line_up();
        assert_eq!(e.text(), "aaa\nbbb\nccc");
    }

    #[test]
    fn comment_toggle_round_trip() {
        let mut e = Editor::with_text("(foo)");
        e.set_cursor(0);
        e.toggle_comment();
        assert_eq!(e.text(), ";; (foo)");
        e.toggle_comment();
        assert_eq!(e.text(), "(foo)");
    }

    #[test]
    fn forward_sexp_moves_cursor() {
        let mut e = Editor::with_text("foo (bar baz) qux");
        e.set_cursor(0);
        e.move_forward_sexp(false);
        assert_eq!(e.cursor_rc(), (0, 3)); // past foo
        e.move_forward_sexp(false);
        assert_eq!(e.cursor_rc(), (0, 13)); // past (bar baz)
    }

    #[test]
    fn render_highlights_special_form() {
        let mut e = Editor::with_text("(defun hi () 42)");
        let cmds = e.render(area());
        // The "defun" run should carry the special-form color, not fg.
        let special = e.theme().c_special;
        let found = cmds.iter().any(|c| matches!(c,
            SurfaceCmd::DrawTextRun { run }
                if run.text == "defun"
                && run.color.r == special.r && run.color.g == special.g));
        assert!(found, "defun should render in the special-form colour");
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
