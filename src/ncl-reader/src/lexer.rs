//! Lexer: source string → stream of [`SpannedToken`].
//!
//! Standard ANSI-CL syntax tables for the default readtable, plus the
//! Corman-specific `#!...!#` block reader. Numbers are NOT parsed
//! here — atoms come out as raw text and the parser tries
//! parse-as-number first, falling back to symbol. This keeps numeric
//! syntax case-insensitive independently of readtable-case.

use crate::token::{AtomText, Span, SpannedToken, Token};

#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LexErrorKind {
    UnexpectedEof(&'static str),
    UnterminatedString,
    UnterminatedBlockComment,
    UnterminatedFfiBlock,
    UnterminatedMultiEscape,
    UnknownCharacterName(String),
    /// `#=` and `##` require a numeric prefix.
    MissingLabelPrefix(char),
}

pub struct Lexer<'a> {
    src: &'a str,
    /// Current byte offset into `src`.
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer { src, pos: 0 }
    }

    /// Pull the next token, or `Ok(None)` at EOF.
    pub fn next_token(&mut self) -> Result<Option<SpannedToken>, LexError> {
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(c) = self.peek_char() else { return Ok(None); };

            return match c {
                ';' => { self.skip_line_comment(); continue; }
                '(' => Ok(Some(self.single(Token::LParen, start))),
                ')' => Ok(Some(self.single(Token::RParen, start))),
                '\'' => Ok(Some(self.single(Token::Quote, start))),
                '`' => Ok(Some(self.single(Token::Backquote, start))),
                ',' => Ok(Some(self.read_comma(start))),
                '"' => Ok(Some(self.read_string(start)?)),
                '#' => match self.read_sharp(start)? {
                    SharpResult::Token(t) => Ok(Some(t)),
                    SharpResult::Skipped => continue,
                },
                _ => Ok(Some(self.read_atom_or_dot(start)?)),
            };
        }
    }

    pub fn into_iter(self) -> LexerIter<'a> { LexerIter { lex: self } }

    // -- character-level helpers ---------------------------------------------

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// Look ahead `n` characters (0 = same as `peek_char`).
    fn peek_nth(&self, n: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(n)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn span_to_here(&self, start: usize) -> Span {
        Span { start, end: self.pos }
    }

    fn single(&mut self, tok: Token, start: usize) -> SpannedToken {
        self.advance();
        SpannedToken { token: tok, span: self.span_to_here(start) }
    }

    // -- whitespace and comments ---------------------------------------------

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if is_whitespace(c) { self.advance(); } else { break; }
        }
    }

    fn skip_line_comment(&mut self) {
        // Caller sees ';'. Eat to EOL or EOF.
        while let Some(c) = self.advance() {
            if c == '\n' { break; }
        }
    }

    /// Block comment `#|...|#`, nestable. Caller has consumed `#|`.
    fn skip_block_comment(&mut self, start: usize) -> Result<(), LexError> {
        let mut depth = 1usize;
        while depth > 0 {
            let Some(c) = self.advance() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedBlockComment,
                    span: self.span_to_here(start),
                });
            };
            match (c, self.peek_char()) {
                ('#', Some('|')) => { self.advance(); depth += 1; }
                ('|', Some('#')) => { self.advance(); depth -= 1; }
                _ => {}
            }
        }
        Ok(())
    }

    // -- comma family --------------------------------------------------------

    fn read_comma(&mut self, start: usize) -> SpannedToken {
        self.advance(); // consume ','
        let tok = match self.peek_char() {
            Some('@') => { self.advance(); Token::CommaAt }
            Some('.') => { self.advance(); Token::CommaDot }
            _ => Token::Comma,
        };
        SpannedToken { token: tok, span: self.span_to_here(start) }
    }

    // -- string literal ------------------------------------------------------

    fn read_string(&mut self, start: usize) -> Result<SpannedToken, LexError> {
        self.advance(); // consume opening '"'
        let mut s = String::new();
        loop {
            let Some(c) = self.advance() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedString,
                    span: self.span_to_here(start),
                });
            };
            match c {
                '"' => break,
                '\\' => {
                    // CL strings have only `\` as a single-escape char:
                    // the next character stands for itself, including `\`
                    // and `"`. No `\n`-style escapes — newlines are
                    // literal newlines inside the string.
                    let Some(esc) = self.advance() else {
                        return Err(LexError {
                            kind: LexErrorKind::UnexpectedEof("after \\ in string"),
                            span: self.span_to_here(start),
                        });
                    };
                    s.push(esc);
                }
                _ => s.push(c),
            }
        }
        Ok(SpannedToken {
            token: Token::String(s),
            span: self.span_to_here(start),
        })
    }

    // -- sharp dispatch ------------------------------------------------------

    fn read_sharp(&mut self, start: usize) -> Result<SharpResult, LexError> {
        self.advance(); // consume '#'
        // Optional integer prefix.
        let mut prefix: Option<u32> = None;
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() {
                let d = c.to_digit(10).unwrap();
                prefix = Some(prefix.unwrap_or(0).saturating_mul(10).saturating_add(d));
                self.advance();
            } else {
                break;
            }
        }
        let Some(disp) = self.advance() else {
            return Err(LexError {
                kind: LexErrorKind::UnexpectedEof("after #"),
                span: self.span_to_here(start),
            });
        };

        match disp {
            '\\' => Ok(SharpResult::Token(self.finish_char_literal(start)?)),
            '|' => { self.skip_block_comment(start)?; Ok(SharpResult::Skipped) }
            '!' => Ok(SharpResult::Token(self.read_ffi_block(start)?)),
            '=' => {
                let Some(p) = prefix else {
                    return Err(LexError {
                        kind: LexErrorKind::MissingLabelPrefix('='),
                        span: self.span_to_here(start),
                    });
                };
                Ok(SharpResult::Token(SpannedToken {
                    token: Token::SharpEquals(p),
                    span: self.span_to_here(start),
                }))
            }
            '#' => {
                let Some(p) = prefix else {
                    return Err(LexError {
                        kind: LexErrorKind::MissingLabelPrefix('#'),
                        span: self.span_to_here(start),
                    });
                };
                Ok(SharpResult::Token(SpannedToken {
                    token: Token::SharpSharp(p),
                    span: self.span_to_here(start),
                }))
            }
            ch => Ok(SharpResult::Token(SpannedToken {
                token: Token::SharpDispatch { ch, prefix },
                span: self.span_to_here(start),
            })),
        }
    }

    /// `#\` already consumed. Read the character literal.
    fn finish_char_literal(&mut self, start: usize) -> Result<SpannedToken, LexError> {
        // ANSI rule: read one character unconditionally, then if it is
        // a constituent and the next is also a constituent, keep
        // reading constituents to form a name.
        let Some(first) = self.advance() else {
            return Err(LexError {
                kind: LexErrorKind::UnexpectedEof("after #\\"),
                span: self.span_to_here(start),
            });
        };
        let mut name = String::new();
        name.push(first);
        if is_constituent(first) {
            while let Some(c) = self.peek_char() {
                if is_constituent(c) { name.push(c); self.advance(); } else { break; }
            }
        }

        let resolved = if name.chars().count() == 1 {
            first
        } else {
            resolve_char_name(&name).ok_or_else(|| LexError {
                kind: LexErrorKind::UnknownCharacterName(name.clone()),
                span: self.span_to_here(start),
            })?
        };

        Ok(SpannedToken {
            token: Token::Char(resolved),
            span: self.span_to_here(start),
        })
    }

    /// `#!` already consumed. Read until matching `!#`.
    /// The header is everything from after `#!` up to (and not
    /// including) the first newline. The body is everything from after
    /// that newline up to (and not including) the closing `!#`.
    fn read_ffi_block(&mut self, start: usize) -> Result<SpannedToken, LexError> {
        let mut header = String::new();
        // Header up to first newline.
        loop {
            let Some(c) = self.peek_char() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedFfiBlock,
                    span: self.span_to_here(start),
                });
            };
            if c == '\n' {
                self.advance();
                break;
            }
            // Defensive: a `!#` immediately after `#!` (no newline)
            // means an empty body and empty header; treat as end.
            if c == '!' && self.peek_nth(1) == Some('#') {
                self.advance(); self.advance();
                return Ok(SpannedToken {
                    token: Token::FfiBlock { header, body: String::new() },
                    span: self.span_to_here(start),
                });
            }
            self.advance();
            header.push(c);
        }

        // Body until `!#`.
        let mut body = String::new();
        loop {
            let Some(c) = self.advance() else {
                return Err(LexError {
                    kind: LexErrorKind::UnterminatedFfiBlock,
                    span: self.span_to_here(start),
                });
            };
            if c == '!' && self.peek_char() == Some('#') {
                self.advance(); // consume '#'
                return Ok(SpannedToken {
                    token: Token::FfiBlock {
                        header: header.trim().to_string(),
                        body,
                    },
                    span: self.span_to_here(start),
                });
            }
            body.push(c);
        }
    }

    // -- atom (and standalone dot) -------------------------------------------

    fn read_atom_or_dot(&mut self, start: usize) -> Result<SpannedToken, LexError> {
        let mut atom = AtomText::new();
        let mut had_escape_anywhere = false;

        loop {
            let Some(c) = self.peek_char() else { break; };
            if is_whitespace(c) || is_terminating_macro(c) { break; }
            self.advance();

            match c {
                '\\' => {
                    // single-escape: next char literal, escaped flag set
                    let Some(esc) = self.advance() else {
                        return Err(LexError {
                            kind: LexErrorKind::UnexpectedEof("after \\ in atom"),
                            span: self.span_to_here(start),
                        });
                    };
                    atom.push(esc, true);
                    had_escape_anywhere = true;
                }
                '|' => {
                    // multi-escape: consume until matching `|`
                    had_escape_anywhere = true;
                    loop {
                        let Some(inner) = self.advance() else {
                            return Err(LexError {
                                kind: LexErrorKind::UnterminatedMultiEscape,
                                span: self.span_to_here(start),
                            });
                        };
                        match inner {
                            '|' => break,
                            '\\' => {
                                let Some(esc) = self.advance() else {
                                    return Err(LexError {
                                        kind: LexErrorKind::UnexpectedEof(
                                            "after \\ in multi-escape",
                                        ),
                                        span: self.span_to_here(start),
                                    });
                                };
                                atom.push(esc, true);
                            }
                            _ => atom.push(inner, true),
                        }
                    }
                }
                _ => atom.push(c, false),
            }
        }

        // Standalone `.` between whitespace is the dotted-pair marker,
        // not an atom. The atom raw is exactly "." with no escapes.
        if !had_escape_anywhere && atom.raw == "." {
            return Ok(SpannedToken {
                token: Token::Dot,
                span: self.span_to_here(start),
            });
        }

        if atom.raw.is_empty() {
            // We were called on a character that turned out to be
            // entirely consumed without producing anything — shouldn't
            // happen under the dispatch above, but guard for safety.
            return Err(LexError {
                kind: LexErrorKind::UnexpectedEof("empty atom"),
                span: self.span_to_here(start),
            });
        }

        Ok(SpannedToken {
            token: Token::Atom(atom),
            span: self.span_to_here(start),
        })
    }
}

/// Result of `read_sharp`: produced a token, or skipped (block comment).
enum SharpResult {
    Token(SpannedToken),
    Skipped,
}

pub struct LexerIter<'a> { lex: Lexer<'a> }

impl<'a> Iterator for LexerIter<'a> {
    type Item = Result<SpannedToken, LexError>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.lex.next_token() {
            Ok(Some(t)) => Some(Ok(t)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

// -- syntax classification ---------------------------------------------------

/// CL whitespace: space, tab, newline, return, formfeed, vertical-tab.
fn is_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
}

/// Token-terminating macro chars in the default readtable.
fn is_terminating_macro(c: char) -> bool {
    matches!(c, '(' | ')' | '\'' | '`' | ',' | ';' | '"')
}

/// Constituent chars (used for character-literal name termination
/// and elsewhere): everything that isn't whitespace or a
/// terminating-macro char.
fn is_constituent(c: char) -> bool {
    !is_whitespace(c) && !is_terminating_macro(c)
}

/// Map a character literal name (like `Space`, `Newline`) to its
/// resolved character. Comparison is case-insensitive. Returns `None`
/// for unknown names.
fn resolve_char_name(name: &str) -> Option<char> {
    match name.to_ascii_lowercase().as_str() {
        "space" | "sp" => Some(' '),
        "newline" | "nl" => Some('\n'),
        "tab" => Some('\t'),
        "linefeed" | "lf" => Some('\n'),
        "return" | "cr" => Some('\r'),
        "page" | "ff" => Some('\x0c'),
        "backspace" | "bs" => Some('\x08'),
        "rubout" | "delete" | "del" => Some('\x7f'),
        "null" | "nul" => Some('\x00'),
        "escape" | "esc" => Some('\x1b'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Token> {
        Lexer::new(src)
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    fn lex_err(src: &str) -> LexErrorKind {
        let mut l = Lexer::new(src);
        loop {
            match l.next_token() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("expected error, hit EOF: {src:?}"),
                Err(e) => return e.kind,
            }
        }
    }

    #[test]
    fn empty_and_whitespace() {
        assert!(lex("").is_empty());
        assert!(lex("   \t\n\r ").is_empty());
    }

    #[test]
    fn parens_and_quote_family() {
        let t = lex("()' ` , ,@ ,.");
        assert_eq!(t, vec![
            Token::LParen,
            Token::RParen,
            Token::Quote,
            Token::Backquote,
            Token::Comma,
            Token::CommaAt,
            Token::CommaDot,
        ]);
    }

    #[test]
    fn line_comment_skipped() {
        let t = lex("foo ; this is a comment\n bar");
        assert_eq!(t.len(), 2);
        match (&t[0], &t[1]) {
            (Token::Atom(a), Token::Atom(b)) => {
                assert_eq!(a.raw, "foo");
                assert_eq!(b.raw, "bar");
            }
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn nested_block_comments() {
        let t = lex("foo #| outer #| inner |# still outer |# bar");
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn unterminated_block_comment_errors() {
        assert!(matches!(lex_err("#| abc"), LexErrorKind::UnterminatedBlockComment));
    }

    #[test]
    fn simple_string() {
        let t = lex(r#""hello""#);
        match &t[..] {
            [Token::String(s)] => assert_eq!(s, "hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn string_escapes() {
        let t = lex(r#""she said \"hi\\bye\"""#);
        match &t[..] {
            [Token::String(s)] => assert_eq!(s, "she said \"hi\\bye\""),
            _ => panic!(),
        }
    }

    #[test]
    fn string_with_newline() {
        // CL strings allow literal newlines.
        let t = lex("\"line1\nline2\"");
        match &t[..] {
            [Token::String(s)] => assert_eq!(s, "line1\nline2"),
            _ => panic!(),
        }
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(matches!(lex_err(r#""hi"#), LexErrorKind::UnterminatedString));
    }

    #[test]
    fn atom_simple() {
        let t = lex("foo");
        match &t[..] {
            [Token::Atom(a)] => {
                assert_eq!(a.raw, "foo");
                assert!(!a.has_any_escape());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn atom_terminates_on_paren() {
        let t = lex("foo)");
        assert_eq!(t.len(), 2);
        match (&t[0], &t[1]) {
            (Token::Atom(a), Token::RParen) => assert_eq!(a.raw, "foo"),
            _ => panic!(),
        }
    }

    #[test]
    fn atom_with_single_escape() {
        let t = lex(r"foo\ bar");
        match &t[..] {
            [Token::Atom(a)] => {
                assert_eq!(a.raw, "foo bar");
                assert_eq!(a.escapes, vec![false, false, false, true, false, false, false]);
            }
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn atom_with_multi_escape() {
        let t = lex(r"|hello world|");
        match &t[..] {
            [Token::Atom(a)] => {
                assert_eq!(a.raw, "hello world");
                assert!(a.escapes.iter().all(|&e| e));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn atom_mixed_escape() {
        // foo|BAR|baz — outer chars unescaped, BAR escaped.
        let t = lex(r"foo|BAR|baz");
        match &t[..] {
            [Token::Atom(a)] => {
                assert_eq!(a.raw, "fooBARbaz");
                assert_eq!(
                    a.escapes,
                    vec![false, false, false, true, true, true, false, false, false]
                );
            }
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn standalone_dot_vs_dotted_atom() {
        let t = lex("(1 . 2)");
        assert_eq!(t.len(), 5);
        assert!(matches!(t[2], Token::Dot));

        let t = lex("3.14");
        match &t[..] {
            [Token::Atom(a)] => assert_eq!(a.raw, "3.14"),
            _ => panic!(),
        }

        let t = lex("foo.bar");
        match &t[..] {
            [Token::Atom(a)] => assert_eq!(a.raw, "foo.bar"),
            _ => panic!(),
        }
    }

    #[test]
    fn package_qualified_atom_kept_intact() {
        // `ccl::quit` — the lexer keeps it as one atom; the parser
        // splits on `:`.
        let t = lex("ccl::quit");
        match &t[..] {
            [Token::Atom(a)] => assert_eq!(a.raw, "ccl::quit"),
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn char_literal_single() {
        for (src, want) in [
            (r"#\a", 'a'),
            (r"#\(", '('),
            (r"#\)", ')'),
            (r"#\\", '\\'),
            (r#"#\""#, '"'),
            (r"#\ ", ' '),
        ] {
            let t = lex(src);
            match &t[..] {
                [Token::Char(c)] => assert_eq!(*c, want, "src {src:?}"),
                _ => panic!("src {src:?} got {t:?}"),
            }
        }
    }

    #[test]
    fn char_literal_named() {
        for (src, want) in [
            (r"#\Space", ' '),
            (r"#\NEWLINE", '\n'),
            (r"#\tab", '\t'),
            (r"#\Return", '\r'),
            (r"#\Rubout", '\x7f'),
        ] {
            let t = lex(src);
            match &t[..] {
                [Token::Char(c)] => assert_eq!(*c, want, "src {src:?}"),
                _ => panic!("src {src:?} got {t:?}"),
            }
        }
    }

    #[test]
    fn unknown_char_name_errors() {
        assert!(matches!(
            lex_err(r"#\Notarealname"),
            LexErrorKind::UnknownCharacterName(_)
        ));
    }

    #[test]
    fn sharp_dispatch_basics() {
        // #' and #: and #( and #+ and #-
        let t = lex("#' #: #( #+ #- #.");
        let chs: Vec<char> = t.iter().filter_map(|tok| match tok {
            Token::SharpDispatch { ch, prefix: None } => Some(*ch),
            _ => None,
        }).collect();
        assert_eq!(chs, vec!['\'', ':', '(', '+', '-', '.']);
    }

    #[test]
    fn sharp_radix_with_prefix() {
        let t = lex("#36rZZ");
        assert_eq!(t.len(), 2);
        match (&t[0], &t[1]) {
            (Token::SharpDispatch { ch: 'r', prefix: Some(36) }, Token::Atom(a)) => {
                assert_eq!(a.raw, "ZZ");
            }
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn sharp_label_markers() {
        let t = lex("#1=foo #1#");
        assert_eq!(t.len(), 3);
        assert!(matches!(t[0], Token::SharpEquals(1)));
        match &t[1] {
            Token::Atom(a) => assert_eq!(a.raw, "foo"),
            _ => panic!(),
        }
        assert!(matches!(t[2], Token::SharpSharp(1)));
    }

    #[test]
    fn sharp_label_missing_prefix_errors() {
        assert!(matches!(lex_err("#=foo"), LexErrorKind::MissingLabelPrefix('=')));
        assert!(matches!(lex_err("##foo"), LexErrorKind::MissingLabelPrefix('#')));
    }

    #[test]
    fn ffi_block_simple() {
        let src = "#! (:library \"ole32\" :pascal \"WINAPI\")\nint Foo(int x);\n!#";
        let t = lex(src);
        match &t[..] {
            [Token::FfiBlock { header, body }] => {
                assert_eq!(header, "(:library \"ole32\" :pascal \"WINAPI\")");
                assert_eq!(body, "int Foo(int x);\n");
            }
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn ffi_block_with_following_form() {
        let src = "#! (:library \"k\")\nbody\n!# (foo)";
        let t = lex(src);
        assert_eq!(t.len(), 4);
        assert!(matches!(t[0], Token::FfiBlock { .. }));
        assert!(matches!(t[1], Token::LParen));
        match &t[2] {
            Token::Atom(a) => assert_eq!(a.raw, "foo"),
            _ => panic!(),
        }
        assert!(matches!(t[3], Token::RParen));
    }

    #[test]
    fn ffi_block_unterminated_errors() {
        assert!(matches!(
            lex_err("#! header\nbody no closer"),
            LexErrorKind::UnterminatedFfiBlock
        ));
    }

    #[test]
    fn realistic_form() {
        let src = r#"(defun fact (n)
  (if (<= n 1)
      1
      (* n (fact (- n 1)))))"#;
        // Just check it tokenises without error and balances parens.
        let toks = lex(src);
        let opens = toks.iter().filter(|t| matches!(t, Token::LParen)).count();
        let closes = toks.iter().filter(|t| matches!(t, Token::RParen)).count();
        assert_eq!(opens, closes);
        assert_eq!(opens, 7);
    }

    #[test]
    fn span_covers_token() {
        let mut l = Lexer::new("  foo  ");
        let st = l.next_token().unwrap().unwrap();
        assert_eq!(st.span.start, 2);
        assert_eq!(st.span.end, 5);
    }

    #[test]
    fn unicode_atom() {
        let t = lex("café");
        match &t[..] {
            [Token::Atom(a)] => assert_eq!(a.raw, "café"),
            _ => panic!("got {t:?}"),
        }
    }

    #[test]
    fn sharp_after_atom_chars_treated_as_constituent() {
        // `foo#bar` is one atom: `#` is non-terminating mid-token.
        // It only triggers dispatch at token start.
        let t = lex("foo#bar");
        match &t[..] {
            [Token::Atom(a)] => assert_eq!(a.raw, "foo#bar"),
            _ => panic!("got {t:?}"),
        }
    }
}
