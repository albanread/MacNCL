//! Token types produced by the lexer.
//!
//! The tokenizer's job is structural: hand the parser a stream of
//! tokens and let the parser decide meaning (number vs symbol,
//! sharp-dispatch interpretation, package qualification). The tokens
//! that DO carry resolved content are the ones whose contents are
//! self-describing: strings (with escapes resolved), character
//! literals (with names resolved), and FFI blocks (captured verbatim).

/// Atom text as it appears in source, plus a per-character mask
/// recording which characters were escaped.
///
/// `raw` is the post-escape character sequence — `|foo bar|` produces
/// `raw = "foo bar"` with all seven escape bits set; `foo\ bar`
/// produces the same raw with only the space escaped.
///
/// The escape mask is needed to implement `:invert` readtable case
/// correctly (escaped characters are protected from inversion) and
/// to decide what counts toward "uniform case" detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomText {
    pub raw: String,
    pub escapes: Vec<bool>,
}

impl AtomText {
    pub fn new() -> AtomText {
        AtomText { raw: String::new(), escapes: Vec::new() }
    }

    pub fn push(&mut self, c: char, escaped: bool) {
        self.raw.push(c);
        self.escapes.push(escaped);
    }

    pub fn has_any_escape(&self) -> bool {
        self.escapes.iter().any(|&e| e)
    }
}

impl Default for AtomText {
    fn default() -> Self { Self::new() }
}

/// A token. Spans live in [`SpannedToken`].
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `'`
    Quote,
    /// `` ` ``
    Backquote,
    /// `,`
    Comma,
    /// `,@`
    CommaAt,
    /// `,.`
    CommaDot,
    /// Standalone `.` between whitespace — the dotted-pair marker.
    /// `1.` (a number with trailing dot) and `foo.bar` (a symbol with
    /// embedded dot) are atoms, not this.
    Dot,
    /// String literal contents, with `\` escapes resolved.
    String(String),
    /// Character literal — `#\x` resolves to the char `x`, `#\Space`
    /// resolves to ' ', etc.
    Char(char),
    /// An atom — could be a symbol (possibly package-qualified) or a
    /// number. The parser decides.
    Atom(AtomText),
    /// `#X` dispatch, with optional integer prefix (`#36rZZ` → prefix
    /// `Some(36)`, ch `'r'`). The parser handles each X.
    SharpDispatch { ch: char, prefix: Option<u32> },
    /// `#=N` — labelled-object marker for circular structure (the N
    /// is required by the syntax).
    SharpEquals(u32),
    /// `##N` — labelled-object reference.
    SharpSharp(u32),
    /// Corman-specific `#!...!#` foreign-declaration block. The header
    /// is captured up to the first newline, the body is everything
    /// after, all verbatim. The FFI parses both later.
    FfiBlock { header: String, body: String },
}

/// Byte offsets into the source. End is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub span: Span,
}
