//! S-expression navigation over the rope buffer.
//!
//! Pure functions on `(&RopeBuffer, offset)` → `Option<usize>`. No
//! mutation, no GUI state, no row/col arithmetic. ledit binds these
//! to keys; the paredit editing primitives (slurp / barf / wrap /
//! splice / raise) compose them with `RopeBuffer::insert` and
//! `RopeBuffer::delete` to do the actual structural edits.
//!
//! Lifted from the sister winscheme repo's `grid_logic.zig`
//! (`findForwardSexpEnd`, `findBackwardSexpStart`,
//! `findMatchingDelimiter`, `findOpenForClose`, `findCloseForOpen`).
//! Translated from the Zig (row, col) line-list model to our
//! offset-canonical rope. The state-machine logic — string /
//! block-comment / line-comment / escape tracking and the balanced
//! delim stack — is preserved one-to-one; only the indexing model
//! changed.
//!
//! Syntax coverage
//! ---------------
//!   * Round and square delimiters: `(…)` and `[…]`. `[]` is not
//!     standard Common Lisp but a number of CL dialects (and the
//!     reader macros people write) treat them as alternative
//!     groupers; treating them as balanced costs us nothing and is
//!     useful for any vendor extension that emits them.
//!   * Line comments: `;` to end of line.
//!   * Block comments: `#| … |#`, nested.
//!   * String literals: `"…"` with `\` escape.
//!   * Character literals: `#\X` where X is one printable code point
//!     or a multi-character name (`#\Newline`, `#\Space`, etc.).
//!     The whole `#\name` sequence reads as one token.
//!   * Reader-macro prefixes that bind tighter than the next sexp:
//!     `'`, `` ` ``, `,`, `,@`, `#'`. `forward_sexp` over `'(a b c)`
//!     jumps past the entire quoted list, not just past the quote.
//!
//! Out of scope for v1 (TODO):
//!   * `#nA(…)` ranked-array literals
//!   * `#+/#-` reader conditionals
//!   * Dispatched constituents (`#:`, `#S(…)`, `#(…)`) — `#(` is
//!     handled implicitly because we treat the `#` as a prefix and
//!     the `(` opens a balanced group; the result is correct.

use super::rope_buffer::RopeBuffer;

// ─── Code-point predicates ──────────────────────────────────────────────

#[inline]
fn is_open_delim(cp: u32) -> bool {
    cp == '(' as u32 || cp == '[' as u32
}

#[inline]
fn is_close_delim(cp: u32) -> bool {
    cp == ')' as u32 || cp == ']' as u32
}

#[inline]
fn matching_close(open: u32) -> Option<u32> {
    match open {
        x if x == '(' as u32 => Some(')' as u32),
        x if x == '[' as u32 => Some(']' as u32),
        _ => None,
    }
}

#[inline]
fn delims_match(open: u32, close: u32) -> bool {
    matching_close(open) == Some(close)
}

#[inline]
fn is_whitespace(cp: u32) -> bool {
    matches!(cp, 0x20 | 0x09 | 0x0A | 0x0D | 0x0C)
}

/// A position where a token (atom or delim) ends and the next thing
/// starts. Whitespace, delimiters, line comments, and quote /
/// quasiquote / unquote sentinels are terminators.
#[inline]
fn token_terminator(cp: u32) -> bool {
    if is_whitespace(cp) || is_open_delim(cp) || is_close_delim(cp) {
        return true;
    }
    matches!(
        cp,
        x if x == ';' as u32
            || x == '"' as u32
            || x == '\'' as u32
            || x == '`' as u32
            || x == ',' as u32
    )
}

// ─── Low-level buffer access ────────────────────────────────────────────

#[inline]
fn cp_at(buf: &RopeBuffer, offset: usize) -> Option<u32> {
    buf.char_at(offset)
}

/// Peek at the cp two positions back. Used to detect that an offset
/// is inside a `#\X` character literal (where X is a printable
/// single cp) — we want backward-sexp to land on the `#`, not the
/// `\` or `X`.
#[inline]
fn is_after_hash_backslash(buf: &RopeBuffer, offset: usize) -> bool {
    if offset < 2 {
        return false;
    }
    cp_at(buf, offset - 2) == Some('#' as u32)
        && cp_at(buf, offset - 1) == Some('\\' as u32)
}

/// True iff `[offset, offset+2)` is the start of a character literal
/// — i.e. `buf[offset] == '#'` and `buf[offset+1] == '\\'`. The
/// caller is responsible for the typical `#\` is at the start of an
/// atom invariant; we don't check what precedes `offset`.
#[inline]
fn is_char_literal_start(buf: &RopeBuffer, offset: usize) -> bool {
    cp_at(buf, offset) == Some('#' as u32) && cp_at(buf, offset + 1) == Some('\\' as u32)
}

/// Given `offset` at the `#` of a `#\NAME` character literal,
/// return the offset just past the last cp of NAME. The first cp
/// after `\` is always part of the literal (so `#\(` is the
/// character `(`); subsequent identifier-continuation cps are
/// consumed up to the first terminator.
fn char_literal_end(buf: &RopeBuffer, offset: usize) -> usize {
    // Skip the `#\` prefix.
    let mut o = offset + 2;
    // Always consume one cp (could be any printable, including a
    // delim like `(` or whitespace).
    if cp_at(buf, o).is_some() {
        o += 1;
    }
    // If the first cp was an identifier-start, consume the rest of
    // the name (digits, letters, `-`, `?`, `!`).
    while let Some(c) = cp_at(buf, o) {
        if is_char_name_cont(c) {
            o += 1;
        } else {
            break;
        }
    }
    o
}

#[inline]
fn is_char_name_cont(cp: u32) -> bool {
    matches!(
        char::from_u32(cp),
        Some(c) if c.is_ascii_alphanumeric() || c == '-' || c == '_'
    )
}

/// `#'` is the function-quote reader macro — `#'foo` is sugar for
/// `(function foo)`. `forward_sexp` should jump past the entire
/// `#'foo`, not just past the `#'`.
#[inline]
fn is_fn_quote_prefix(buf: &RopeBuffer, offset: usize) -> bool {
    cp_at(buf, offset) == Some('#' as u32) && cp_at(buf, offset + 1) == Some('\'' as u32)
}

// ─── forward_sexp ───────────────────────────────────────────────────────

/// Find the offset just past the end of the next s-expression
/// starting at (or right after) `cursor`. Returns `None` if
/// `cursor` is at end-of-buffer with nothing to jump over.
///
/// Semantics mirror Emacs `forward-sexp`:
///   * Skip leading whitespace, line-comments, block-comments.
///   * If the next cp is an open delim, jump to past its matching
///     close.
///   * If the next cp is a close delim, fail (you're already at
///     the end of the enclosing sexp).
///   * If the next sequence is a reader-macro prefix
///     (`'` `` ` `` `,` `,@` `#'`), step over it and recurse.
///   * If the next cp opens a string literal, jump past the closing
///     `"`.
///   * If the next cps are `#\…`, jump past the character-literal
///     name.
///   * Otherwise treat the run as an atom and jump to the first
///     token terminator.
pub fn forward_sexp(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    let mut o = skip_atmosphere_forward(buf, cursor)?;
    let total = buf.len();
    if o >= total {
        return None;
    }
    loop {
        let cp = cp_at(buf, o)?;
        // Reader-macro prefixes: step over and recurse.
        if cp == '\'' as u32 || cp == '`' as u32 {
            o += 1;
            continue;
        }
        if cp == ',' as u32 {
            o += 1;
            if cp_at(buf, o) == Some('@' as u32) {
                o += 1;
            }
            continue;
        }
        if is_fn_quote_prefix(buf, o) {
            o += 2;
            continue;
        }
        if is_char_literal_start(buf, o) {
            return Some(char_literal_end(buf, o));
        }
        if cp == '"' as u32 {
            return Some(skip_string_forward(buf, o));
        }
        if is_close_delim(cp) {
            // Already at the end of the enclosing sexp.
            return None;
        }
        if is_open_delim(cp) {
            let close = close_for_open(buf, o)?;
            return Some(close + 1);
        }
        // Atom: scan to first terminator.
        let mut p = o;
        while let Some(c) = cp_at(buf, p) {
            if token_terminator(c) {
                break;
            }
            p += 1;
        }
        return Some(p);
    }
}

// ─── backward_sexp ──────────────────────────────────────────────────────

/// Find the offset of the start of the previous s-expression
/// ending at (or right before) `cursor`. Returns `None` at start
/// of buffer.
pub fn backward_sexp(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    let mut o = skip_atmosphere_backward(buf, cursor)?;
    if o == 0 {
        // Possibly the first cp IS the sexp start.
        return Some(0);
    }
    // We're sitting one past the end of a sexp. Step back one cp to
    // be on its last cp.
    o -= 1;
    let cp = cp_at(buf, o)?;
    if is_close_delim(cp) {
        let open = open_for_close(buf, o)?;
        // Account for any leading reader-macro prefix.
        return Some(absorb_reader_prefix_backward(buf, open));
    }
    if is_open_delim(cp) {
        // Sitting on an unmatched open — treat its position as the
        // start. (Caller can decide what to do with degenerate
        // input.)
        return Some(o);
    }
    if cp == '"' as u32 {
        let start = string_start_backward(buf, o);
        return Some(absorb_reader_prefix_backward(buf, start));
    }
    // Atom: walk back to the first terminator.
    while o > 0 {
        let prev = cp_at(buf, o - 1)?;
        if token_terminator(prev) {
            break;
        }
        o -= 1;
    }
    // `#\X` literal handling: if we landed on a cp whose two
    // preceding cps are `#\`, step back to the `#`.
    if is_after_hash_backslash(buf, o + 1) {
        // Re-derive: o currently is on the cp after `\`. We want
        // the `#`.
        if o >= 2 {
            o -= 2;
        }
    }
    Some(absorb_reader_prefix_backward(buf, o))
}

/// If `pos` is immediately preceded by a reader-macro prefix
/// (`'` `` ` `` `,` `,@` `#'`), return the offset of that prefix.
/// Otherwise return `pos` unchanged. Iterates so chains like
/// `'`',foo` are absorbed in one shot.
fn absorb_reader_prefix_backward(buf: &RopeBuffer, pos: usize) -> usize {
    let mut o = pos;
    loop {
        if o == 0 {
            return 0;
        }
        let prev = match cp_at(buf, o - 1) {
            Some(c) => c,
            None => return o,
        };
        if prev == '\'' as u32 || prev == '`' as u32 || prev == ',' as u32 {
            o -= 1;
            // `,@` — consume the `,` we're sitting before too.
            if prev == '@' as u32 && o > 0 && cp_at(buf, o - 1) == Some(',' as u32) {
                o -= 1;
            }
            continue;
        }
        // `#'` two-cp prefix.
        if o >= 2
            && cp_at(buf, o - 2) == Some('#' as u32)
            && cp_at(buf, o - 1) == Some('\'' as u32)
        {
            o -= 2;
            continue;
        }
        return o;
    }
}

// ─── matching_delim ─────────────────────────────────────────────────────

/// If `cursor` is on a delimiter, return the offset of its match.
/// Used by the paren-flash render hook and the balance check.
pub fn matching_delim(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    let cp = cp_at(buf, cursor)?;
    if is_open_delim(cp) {
        close_for_open(buf, cursor)
    } else if is_close_delim(cp) {
        open_for_close(buf, cursor)
    } else {
        None
    }
}

// ─── enclosing_open ─────────────────────────────────────────────────────

/// Find the offset of the open delimiter of the form the cursor is
/// currently inside, or `None` at top level. Used by every paredit
/// editing op — slurp / barf / wrap / splice / raise all need to
/// know where the enclosing parens are.
///
/// Algorithm: scan from buffer start to `cursor`, maintain a stack
/// of open delims (skipping comments and strings via the shared
/// `Scanner`). The stack's top at end of scan is the innermost
/// enclosing open. The cursor sitting ON an open delim counts as
/// being inside it (so cursor at `(foo …` returns the offset of
/// that `(`).
pub fn enclosing_open(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    let mut stack: Vec<usize> = Vec::new();
    let mut scanner = Scanner::starting_at(buf, 0);
    while let Some(tok) = scanner.next_lex_token() {
        let pos = scanner.last_token_start;
        if pos >= cursor {
            // Cursor sitting on an open delim is INSIDE that form.
            // Cursor sitting on a close delim is INSIDE the form
            // that delim closes — that form is the second-from-top
            // of the stack (we haven't popped yet). Handle this by
            // including the open in the result before processing.
            if pos == cursor {
                match tok {
                    LexTok::Open(_, _) => return Some(pos),
                    LexTok::Close(_, _) => return stack.last().copied(),
                    LexTok::Other => {}
                }
            }
            break;
        }
        match tok {
            LexTok::Open(_, _) => stack.push(pos),
            LexTok::Close(_, _) => {
                stack.pop();
            }
            LexTok::Other => {}
        }
    }
    stack.last().copied()
}

/// Convenience: enclosing `(open, close)` pair, or None at top
/// level / unmatched.
pub fn enclosing_form(buf: &RopeBuffer, cursor: usize) -> Option<(usize, usize)> {
    let open = enclosing_open(buf, cursor)?;
    let close = close_for_open(buf, open)?;
    Some((open, close))
}

/// Find the OUTERMOST enclosing open at `cursor` — i.e. the start
/// of the top-level form the cursor is inside. Returns `None` at
/// file scope (cursor between top-level forms, in whitespace or
/// comments).
///
/// Used by "send top-level form to eval" — the canonical CL
/// development action (Emacs's `C-M-x`). The user puts the cursor
/// anywhere inside `(run-draw-square)`, hits the key, and only
/// that form goes to the language thread.
pub fn outermost_enclosing_open(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    let mut stack: Vec<usize> = Vec::new();
    let mut scanner = Scanner::starting_at(buf, 0);
    while let Some(tok) = scanner.next_lex_token() {
        let pos = scanner.last_token_start;
        if pos > cursor {
            break;
        }
        if pos == cursor {
            // Cursor sitting ON an open delim: that open is part
            // of the form we want.
            if matches!(tok, LexTok::Open(_, _)) {
                stack.push(pos);
            }
            break;
        }
        match tok {
            LexTok::Open(_, _) => stack.push(pos),
            LexTok::Close(_, _) => {
                stack.pop();
            }
            LexTok::Other => {}
        }
    }
    stack.first().copied()
}

/// `(open, end)` of the top-level form at `cursor`, where `end` is
/// exclusive (just past the closing delim). `None` at file scope.
pub fn top_level_form(buf: &RopeBuffer, cursor: usize) -> Option<(usize, usize)> {
    let open = outermost_enclosing_open(buf, cursor)?;
    let close = close_for_open(buf, open)?;
    Some((open, close + 1))
}

// ─── close_for_open ─────────────────────────────────────────────────────

/// Given `open_pos` on an open delim, find the offset of the
/// matching close. Returns `None` on imbalance or EOF before close.
pub fn close_for_open(buf: &RopeBuffer, open_pos: usize) -> Option<usize> {
    let open_cp = cp_at(buf, open_pos)?;
    let target_close = matching_close(open_cp)?;
    let mut expected: Vec<u32> = vec![target_close];
    let mut scanner = Scanner::starting_after(buf, open_pos);

    while let Some(tok) = scanner.next_lex_token() {
        match tok {
            LexTok::Open(_, _) => {
                let cp = scanner.cp_at(scanner.last_token_start);
                if let Some(c) = matching_close(cp.unwrap_or(0)) {
                    expected.push(c);
                }
            }
            LexTok::Close(_, _) => {
                let cp = scanner.cp_at(scanner.last_token_start).unwrap_or(0);
                let want = match expected.last() {
                    Some(w) => *w,
                    None => return None,
                };
                if cp != want {
                    return None;
                }
                expected.pop();
                if expected.is_empty() {
                    return Some(scanner.last_token_start);
                }
            }
            LexTok::Other => {}
        }
    }
    None
}

// ─── open_for_close ─────────────────────────────────────────────────────

/// Given `close_pos` on a close delim, find the offset of the
/// matching open. Scans from buffer start; O(close_pos). For typical
/// editor use this is fine; a sibling-cache optimisation is a future
/// concern.
pub fn open_for_close(buf: &RopeBuffer, close_pos: usize) -> Option<usize> {
    let target_close = cp_at(buf, close_pos)?;
    if !is_close_delim(target_close) {
        return None;
    }
    let mut stack: Vec<(u32, usize)> = Vec::new();
    let mut scanner = Scanner::starting_at(buf, 0);

    while let Some(tok) = scanner.next_lex_token() {
        let pos = scanner.last_token_start;
        if pos > close_pos {
            return None;
        }
        match tok {
            LexTok::Open(cp, _) => stack.push((cp, pos)),
            LexTok::Close(cp, _) => {
                let (open_cp, open_pos) = stack.pop()?;
                if !delims_match(open_cp, cp) {
                    return None;
                }
                if pos == close_pos {
                    return Some(open_pos);
                }
            }
            LexTok::Other => {}
        }
    }
    None
}

// ─── Scanner: shared lexer state machine ────────────────────────────────
//
// Tracks string / block-comment / line-comment / escape state and
// emits one of three tokens at each "interesting" code point. The
// linear-scan paren-matchers above use this; the non-emitting
// `skip_atmosphere_*` helpers below pump it without listening for
// emitted tokens (they care about the in_string / in_comment
// invariants but not about delims).

#[derive(Clone, Copy)]
enum LexTok {
    /// An open delim at `last_token_start`. Payload: the cp.
    Open(u32, ()),
    /// A close delim at `last_token_start`.
    Close(u32, ()),
    /// Anything else (atom char, etc.) — emitted so callers can see
    /// progress; usually they ignore it.
    Other,
}

struct Scanner<'a> {
    buf: &'a RopeBuffer,
    o: usize,
    in_string: bool,
    escape: bool,
    block_depth: u32,
    last_token_start: usize,
}

impl<'a> Scanner<'a> {
    fn starting_at(buf: &'a RopeBuffer, o: usize) -> Self {
        Scanner {
            buf,
            o,
            in_string: false,
            escape: false,
            block_depth: 0,
            last_token_start: o,
        }
    }

    fn starting_after(buf: &'a RopeBuffer, o: usize) -> Self {
        Scanner {
            buf,
            o: o + 1,
            in_string: false,
            escape: false,
            block_depth: 0,
            last_token_start: o + 1,
        }
    }

    fn cp_at(&self, offset: usize) -> Option<u32> {
        self.buf.char_at(offset)
    }

    fn peek(&self, k: usize) -> Option<u32> {
        self.buf.char_at(self.o + k)
    }

    /// Advance to the next interesting code point and report what
    /// it was. Returns `None` at EOF.
    fn next_lex_token(&mut self) -> Option<LexTok> {
        loop {
            let cp = self.peek(0)?;
            if self.block_depth > 0 {
                if cp == '#' as u32 && self.peek(1) == Some('|' as u32) {
                    self.block_depth += 1;
                    self.o += 2;
                    continue;
                }
                if cp == '|' as u32 && self.peek(1) == Some('#' as u32) {
                    self.block_depth -= 1;
                    self.o += 2;
                    continue;
                }
                self.o += 1;
                continue;
            }
            if self.in_string {
                if self.escape {
                    self.escape = false;
                    self.o += 1;
                    continue;
                }
                if cp == '\\' as u32 {
                    self.escape = true;
                    self.o += 1;
                    continue;
                }
                if cp == '"' as u32 {
                    self.in_string = false;
                    self.o += 1;
                    continue;
                }
                self.o += 1;
                continue;
            }
            // Line comment: skip to EOL.
            if cp == ';' as u32 {
                while let Some(c) = self.peek(0) {
                    if c == '\n' as u32 {
                        break;
                    }
                    self.o += 1;
                }
                continue;
            }
            // Block-comment open.
            if cp == '#' as u32 && self.peek(1) == Some('|' as u32) {
                self.block_depth = 1;
                self.o += 2;
                continue;
            }
            // Character literal `#\X...` — skip in one shot so the
            // `(` in `#\(` doesn't open a fake group.
            if cp == '#' as u32 && self.peek(1) == Some('\\' as u32) {
                let end = char_literal_end(self.buf, self.o);
                self.o = end;
                continue;
            }
            // String open.
            if cp == '"' as u32 {
                self.in_string = true;
                self.escape = false;
                self.o += 1;
                continue;
            }
            // Now: real tokens we report.
            if is_open_delim(cp) {
                self.last_token_start = self.o;
                self.o += 1;
                return Some(LexTok::Open(cp, ()));
            }
            if is_close_delim(cp) {
                self.last_token_start = self.o;
                self.o += 1;
                return Some(LexTok::Close(cp, ()));
            }
            // Atom-ish. Step one and report.
            self.last_token_start = self.o;
            self.o += 1;
            return Some(LexTok::Other);
        }
    }
}

// ─── Atmosphere skippers ────────────────────────────────────────────────
//
// "Atmosphere" = whitespace, line comments, block comments. These
// are private to `forward_sexp` / `backward_sexp`.

fn skip_atmosphere_forward(buf: &RopeBuffer, start: usize) -> Option<usize> {
    let mut o = start;
    let total = buf.len();
    let mut block_depth: u32 = 0;
    while o < total {
        let cp = cp_at(buf, o)?;
        if block_depth > 0 {
            if cp == '#' as u32 && cp_at(buf, o + 1) == Some('|' as u32) {
                block_depth += 1;
                o += 2;
                continue;
            }
            if cp == '|' as u32 && cp_at(buf, o + 1) == Some('#' as u32) {
                block_depth -= 1;
                o += 2;
                continue;
            }
            o += 1;
            continue;
        }
        if is_whitespace(cp) {
            o += 1;
            continue;
        }
        if cp == ';' as u32 {
            while let Some(c) = cp_at(buf, o) {
                o += 1;
                if c == '\n' as u32 {
                    break;
                }
            }
            continue;
        }
        if cp == '#' as u32 && cp_at(buf, o + 1) == Some('|' as u32) {
            block_depth = 1;
            o += 2;
            continue;
        }
        return Some(o);
    }
    Some(o)
}

fn skip_atmosphere_backward(buf: &RopeBuffer, cursor: usize) -> Option<usize> {
    let mut o = cursor;
    while o > 0 {
        let prev = cp_at(buf, o - 1)?;
        if !is_whitespace(prev) {
            // We're not on whitespace. The Zig version doesn't try
            // to skip backward over line comments — backward
            // movement across `; … \n` would be ambiguous anyway
            // (you'd land somewhere inside the comment text). Stop
            // here.
            break;
        }
        o -= 1;
    }
    Some(o)
}

fn skip_string_forward(buf: &RopeBuffer, open_quote: usize) -> usize {
    let mut o = open_quote + 1;
    let mut escape = false;
    while let Some(cp) = cp_at(buf, o) {
        if escape {
            escape = false;
            o += 1;
            continue;
        }
        if cp == '\\' as u32 {
            escape = true;
            o += 1;
            continue;
        }
        if cp == '"' as u32 {
            return o + 1;
        }
        o += 1;
    }
    o
}

fn string_start_backward(buf: &RopeBuffer, close_quote: usize) -> usize {
    // Walk back past the string body. We can't perfectly handle
    // backslashes from this direction (the parity matters), but
    // for sensible source code the heuristic "find the previous
    // unescaped `"` is correct.
    let mut o = close_quote;
    while o > 0 {
        o -= 1;
        let cp = match cp_at(buf, o) {
            Some(c) => c,
            None => break,
        };
        if cp == '"' as u32 {
            // Check parity of preceding backslashes; an even count
            // means this quote isn't escaped.
            let mut bs = 0usize;
            let mut p = o;
            while p > 0 && cp_at(buf, p - 1) == Some('\\' as u32) {
                bs += 1;
                p -= 1;
            }
            if bs % 2 == 0 {
                return o;
            }
        }
    }
    0
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::igui::rope_buffer::RopeBuffer;

    fn rope(s: &str) -> RopeBuffer {
        RopeBuffer::from_utf8(s.as_bytes())
    }

    // forward_sexp ───────────────────────────────────────────────

    #[test]
    fn forward_sexp_over_atom() {
        let b = rope("foo bar");
        assert_eq!(forward_sexp(&b, 0), Some(3));
    }

    #[test]
    fn forward_sexp_skips_leading_whitespace() {
        let b = rope("   foo bar");
        assert_eq!(forward_sexp(&b, 0), Some(6));
    }

    #[test]
    fn forward_sexp_over_list() {
        let b = rope("(a b c) extra");
        assert_eq!(forward_sexp(&b, 0), Some(7));
    }

    #[test]
    fn forward_sexp_over_nested_list() {
        let b = rope("(a (b (c d) e) f)");
        assert_eq!(forward_sexp(&b, 0), Some(b.len()));
    }

    #[test]
    fn forward_sexp_over_quoted_list() {
        let b = rope("'(a b c) extra");
        assert_eq!(forward_sexp(&b, 0), Some(8));
    }

    #[test]
    fn forward_sexp_over_quasi_unquote() {
        let b = rope("`,foo bar");
        // ` then , then foo  → all part of one sexp
        assert_eq!(forward_sexp(&b, 0), Some(5));
    }

    #[test]
    fn forward_sexp_over_unquote_splice() {
        let b = rope(",@foo bar");
        assert_eq!(forward_sexp(&b, 0), Some(5));
    }

    #[test]
    fn forward_sexp_over_fn_quote() {
        let b = rope("#'foo bar");
        assert_eq!(forward_sexp(&b, 0), Some(5));
    }

    #[test]
    fn forward_sexp_over_string() {
        let b = rope("\"hello \\\"world\\\"\" rest");
        assert_eq!(forward_sexp(&b, 0), Some(17));
    }

    #[test]
    fn forward_sexp_over_char_literal_single() {
        let b = rope("#\\a foo");
        assert_eq!(forward_sexp(&b, 0), Some(3));
    }

    #[test]
    fn forward_sexp_over_char_literal_named() {
        let b = rope("#\\Newline foo");
        assert_eq!(forward_sexp(&b, 0), Some(9));
    }

    #[test]
    fn forward_sexp_skips_line_comment() {
        let b = rope("; comment line\nfoo bar");
        assert_eq!(forward_sexp(&b, 0), Some(18));
    }

    #[test]
    fn forward_sexp_skips_block_comment() {
        let b = rope("#| hi |# foo bar");
        assert_eq!(forward_sexp(&b, 0), Some(12));
    }

    #[test]
    fn forward_sexp_skips_nested_block_comment() {
        let b = rope("#| outer #| inner |# more |# foo");
        assert_eq!(forward_sexp(&b, 0), Some(32));
    }

    #[test]
    fn forward_sexp_on_close_returns_none() {
        let b = rope(") foo");
        assert_eq!(forward_sexp(&b, 0), None);
    }

    #[test]
    fn forward_sexp_eof_returns_none() {
        let b = rope("   ");
        assert_eq!(forward_sexp(&b, 0), None);
    }

    // backward_sexp ──────────────────────────────────────────────

    #[test]
    fn backward_sexp_over_atom() {
        let b = rope("foo bar");
        assert_eq!(backward_sexp(&b, 7), Some(4));
    }

    #[test]
    fn backward_sexp_over_list() {
        let b = rope("(a b c)");
        assert_eq!(backward_sexp(&b, 7), Some(0));
    }

    #[test]
    fn backward_sexp_over_nested_list() {
        let b = rope("(a (b c) d)");
        assert_eq!(backward_sexp(&b, b.len()), Some(0));
    }

    #[test]
    fn backward_sexp_over_quoted_list() {
        let b = rope("'(a b c)");
        // After the closing paren, jumping back should land on the quote.
        assert_eq!(backward_sexp(&b, b.len()), Some(0));
    }

    #[test]
    fn backward_sexp_at_start_returns_none() {
        let b = rope("foo");
        assert_eq!(backward_sexp(&b, 0), None);
    }

    // matching_delim ─────────────────────────────────────────────

    #[test]
    fn matching_delim_open_to_close() {
        let b = rope("(a (b c) d)");
        assert_eq!(matching_delim(&b, 0), Some(10));
    }

    #[test]
    fn matching_delim_close_to_open() {
        let b = rope("(a (b c) d)");
        assert_eq!(matching_delim(&b, 10), Some(0));
    }

    #[test]
    fn matching_delim_nested_inner() {
        let b = rope("(a (b c) d)");
        assert_eq!(matching_delim(&b, 3), Some(7));
        assert_eq!(matching_delim(&b, 7), Some(3));
    }

    #[test]
    fn matching_delim_square_brackets() {
        let b = rope("[a b c]");
        assert_eq!(matching_delim(&b, 0), Some(6));
        assert_eq!(matching_delim(&b, 6), Some(0));
    }

    #[test]
    fn matching_delim_ignores_string() {
        let b = rope("(a \")\" b)");
        // The `)` inside the string shouldn't match the opening `(`.
        assert_eq!(matching_delim(&b, 0), Some(8));
    }

    #[test]
    fn matching_delim_ignores_line_comment() {
        let b = rope("(a ; ) inside comment\n b)");
        assert_eq!(matching_delim(&b, 0), Some(b.len() - 1));
    }

    #[test]
    fn matching_delim_ignores_block_comment() {
        let b = rope("(a #| ) |# b)");
        assert_eq!(matching_delim(&b, 0), Some(b.len() - 1));
    }

    #[test]
    fn matching_delim_ignores_char_literal_paren() {
        let b = rope("(a #\\) b)");
        // `#\)` is one token; the real close is at the end.
        assert_eq!(matching_delim(&b, 0), Some(b.len() - 1));
    }

    #[test]
    fn matching_delim_on_non_delim() {
        let b = rope("(a b c)");
        assert_eq!(matching_delim(&b, 1), None);
    }

    #[test]
    fn matching_delim_unbalanced_returns_none() {
        let b = rope("((a)");
        assert_eq!(matching_delim(&b, 0), None);
    }

    // enclosing_open / enclosing_form ────────────────────────────

    #[test]
    fn enclosing_open_top_level() {
        let b = rope("foo bar");
        assert_eq!(enclosing_open(&b, 0), None);
        assert_eq!(enclosing_open(&b, 4), None);
    }

    #[test]
    fn enclosing_open_simple() {
        //  0123456789
        // "(foo bar)"
        let b = rope("(foo bar)");
        assert_eq!(enclosing_open(&b, 4), Some(0));
        assert_eq!(enclosing_open(&b, 8), Some(0)); // on close delim
    }

    #[test]
    fn enclosing_open_nested() {
        //  0         1
        //  0123456789012
        // "(a (b c) d)"
        let b = rope("(a (b c) d)");
        assert_eq!(enclosing_open(&b, 5), Some(3)); // inside inner
        assert_eq!(enclosing_open(&b, 7), Some(3)); // on inner close
        assert_eq!(enclosing_open(&b, 9), Some(0)); // outside inner
    }

    #[test]
    fn enclosing_open_on_open_returns_itself() {
        let b = rope("(a (b c) d)");
        assert_eq!(enclosing_open(&b, 0), Some(0));
        assert_eq!(enclosing_open(&b, 3), Some(3));
    }

    #[test]
    fn enclosing_form_returns_pair() {
        let b = rope("(a (b c) d)");
        assert_eq!(enclosing_form(&b, 5), Some((3, 7)));
        assert_eq!(enclosing_form(&b, 9), Some((0, 10)));
        assert_eq!(enclosing_form(&b, 12), None);
    }

    #[test]
    fn enclosing_open_ignores_string_paren() {
        let b = rope("(a \"(\" b)");
        assert_eq!(enclosing_open(&b, 5), Some(0));
    }

    #[test]
    fn enclosing_open_ignores_comment_paren() {
        let b = rope("(a ;\n b)");
        assert_eq!(enclosing_open(&b, 7), Some(0));
    }

    // top_level_form ─────────────────────────────────────────────

    #[test]
    fn top_level_form_inside_nested() {
        //  0         1         2
        //  0123456789012345678901
        // "(defun f () (+ 1 2))"
        let b = rope("(defun f () (+ 1 2))");
        // Cursor inside the inner (+ 1 2) form.
        assert_eq!(top_level_form(&b, 14), Some((0, 20)));
    }

    #[test]
    fn top_level_form_on_outer_open() {
        let b = rope("(defun f () (+ 1 2))");
        assert_eq!(top_level_form(&b, 0), Some((0, 20)));
    }

    #[test]
    fn top_level_form_between_forms() {
        //  "(a)\n(b)"
        //   0123 4567
        let b = rope("(a)\n(b)");
        // Cursor at the newline between forms — top level, no form.
        assert_eq!(top_level_form(&b, 3), None);
    }

    #[test]
    fn top_level_form_picks_second_form() {
        let b = rope("(a)\n(b)");
        // Cursor inside the second form's `b`.
        assert_eq!(top_level_form(&b, 5), Some((4, 7)));
    }
}
