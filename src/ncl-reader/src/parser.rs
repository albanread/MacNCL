//! Parser: token stream → Lisp values.
//!
//! Drives the lexer, applies readtable case folding, resolves atoms
//! to numbers or package-qualified symbols, expands quote/backquote
//! sugar, and dispatches `#X` reader macros.

use std::sync::Arc;

use ncl_runtime::{universe, Package, Symbol, Value, Visibility};

use crate::lexer::{LexError, Lexer};
use crate::numbers::{parse_integer, parse_ratio_radix, try_parse_number};
use crate::readtable::Readtable;
use crate::token::{AtomText, Span, SpannedToken, Token};

#[derive(Debug, Clone, PartialEq)]
pub struct ReaderError {
    pub kind: ReaderErrorKind,
    pub span: Option<Span>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReaderErrorKind {
    Lex(crate::lexer::LexErrorKind),
    UnexpectedEof(&'static str),
    UnexpectedToken { found: String, expected: &'static str },
    DotNotAllowedHere,
    DotMisplaced,
    PackageNotFound(String),
    UnknownSharpDispatch { ch: char, prefix: Option<u32> },
    UnsupportedSharpDispatch { ch: char, prefix: Option<u32>, reason: &'static str },
    BadRadix(u32),
    BadRadixDigits { radix: u32, text: String },
    InvalidFeatureExpr,
    ReadEvalUnsupported,
    UnresolvedLabel(u32),
}

impl From<LexError> for ReaderError {
    fn from(e: LexError) -> ReaderError {
        ReaderError {
            kind: ReaderErrorKind::Lex(e.kind),
            span: Some(e.span),
        }
    }
}

pub struct Parser<'a> {
    lexer: Lexer<'a>,
    peeked: Option<SpannedToken>,
    pub readtable: Readtable,
    pub current_package: Arc<Package>,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Parser<'a> {
        let cl_user = universe()
            .find_package("COMMON-LISP-USER")
            .expect("COMMON-LISP-USER bootstrapped");
        Parser {
            lexer: Lexer::new(src),
            peeked: None,
            readtable: Readtable::default(),
            current_package: cl_user,
        }
    }

    pub fn with_package(mut self, pkg: Arc<Package>) -> Parser<'a> {
        self.current_package = pkg;
        self
    }

    /// Read every top-level form until EOF. Skipped forms (from
    /// non-matching `#+`/`#-`) are silently consumed.
    pub fn read_all(&mut self) -> Result<Vec<Value>, ReaderError> {
        let mut out = Vec::new();
        while self.peek_token()?.is_some() {
            if let Some(v) = self.read_form_or_skipped()? {
                out.push(v);
            }
            // Skipped: loop, check peek again. Importantly this lets
            // a trailing `#+falsefeature form` end the file cleanly
            // instead of erroring with EOF-while-looking-for-a-form.
        }
        Ok(out)
    }

    /// Read exactly one form. Errors on EOF. Internally retries past
    /// skipped feature-tests so the caller always gets a real form.
    pub fn read_form(&mut self) -> Result<Value, ReaderError> {
        loop {
            if let Some(v) = self.read_form_or_skipped()? {
                return Ok(v);
            }
            // Skipped — try again. If EOF after a skip, the next
            // call inside read_form_or_skipped will surface an
            // `UnexpectedEof("expecting form")` error.
        }
    }

    /// Consume one token's worth of input and return a value, or
    /// `None` if the consumption corresponded to a skipped form
    /// (e.g. `#+nope ...`). Used by `read_form` and `read_all`.
    fn read_form_or_skipped(&mut self) -> Result<Option<Value>, ReaderError> {
        let st = self.next_token_required("expecting form")?;
        match st.token {
            Token::LParen => self.read_list(st.span).map(Some),
            Token::Quote => self.wrap_one("QUOTE").map(Some),
            Token::Backquote => self.wrap_one("BACKQUOTE").map(Some),
            Token::Comma => self.wrap_one("UNQUOTE").map(Some),
            Token::CommaAt => self.wrap_one("UNQUOTE-SPLICING").map(Some),
            Token::CommaDot => self.wrap_one("UNQUOTE-NSPLICING").map(Some),
            Token::String(s) => Ok(Some(Value::String(Arc::new(s)))),
            Token::Char(c) => Ok(Some(Value::Char(c))),
            Token::Atom(a) => self.resolve_atom(&a).map(Some),
            Token::FfiBlock { header, body } => Ok(Some(Value::FfiBlock(Arc::new(
                ncl_runtime::FfiBlock { header, body },
            )))),
            Token::SharpDispatch { ch, prefix } => self.handle_sharp(ch, prefix, st.span),
            Token::SharpEquals(_) => Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: '=',
                    prefix: None,
                    reason: "circular structure (#N=) not supported in Phase 1c",
                },
                Some(st.span),
            )),
            Token::SharpSharp(_) => Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: '#',
                    prefix: None,
                    reason: "circular structure (#N#) not supported in Phase 1c",
                },
                Some(st.span),
            )),
            Token::Dot => Err(self.err_at(ReaderErrorKind::DotNotAllowedHere, Some(st.span))),
            Token::RParen => Err(self.err_at(
                ReaderErrorKind::UnexpectedToken {
                    found: ")".into(),
                    expected: "form",
                },
                Some(st.span),
            )),
        }
    }

    // -- token-stream helpers ------------------------------------------------

    fn peek_token(&mut self) -> Result<Option<&SpannedToken>, ReaderError> {
        if self.peeked.is_none() {
            self.peeked = self.lexer.next_token().map_err(ReaderError::from)?;
        }
        Ok(self.peeked.as_ref())
    }

    fn next_token(&mut self) -> Result<Option<SpannedToken>, ReaderError> {
        if let Some(t) = self.peeked.take() {
            return Ok(Some(t));
        }
        self.lexer.next_token().map_err(ReaderError::from)
    }

    fn next_token_required(&mut self, what: &'static str) -> Result<SpannedToken, ReaderError> {
        self.next_token()?.ok_or(ReaderError {
            kind: ReaderErrorKind::UnexpectedEof(what),
            span: None,
        })
    }

    fn err_at(&self, kind: ReaderErrorKind, span: Option<Span>) -> ReaderError {
        ReaderError { kind, span }
    }

    // -- list reading --------------------------------------------------------

    fn read_list(&mut self, _open_span: Span) -> Result<Value, ReaderError> {
        let mut items: Vec<Value> = Vec::new();
        let mut tail: Option<Value> = None;

        loop {
            let Some(peek) = self.peek_token()? else {
                return Err(ReaderError {
                    kind: ReaderErrorKind::UnexpectedEof("inside list"),
                    span: None,
                });
            };

            match &peek.token {
                Token::RParen => {
                    self.next_token()?;
                    return Ok(match tail {
                        None => Value::list(items),
                        Some(t) => build_dotted(items, t),
                    });
                }
                Token::Dot => {
                    if items.is_empty() {
                        let span = peek.span;
                        self.next_token()?;
                        return Err(self.err_at(ReaderErrorKind::DotMisplaced, Some(span)));
                    }
                    let dot_span = peek.span;
                    self.next_token()?; // consume dot
                    let cdr = self.read_form()?;
                    let after = self.next_token_required("after dotted-pair cdr")?;
                    if !matches!(after.token, Token::RParen) {
                        return Err(self.err_at(
                            ReaderErrorKind::DotMisplaced,
                            Some(dot_span),
                        ));
                    }
                    tail = Some(cdr);
                    return Ok(build_dotted(items, tail.unwrap()));
                }
                _ => {
                    items.push(self.read_form()?);
                }
            }
        }
    }

    // -- quote / backquote / unquote sugar ----------------------------------

    fn wrap_one(&mut self, head_name: &str) -> Result<Value, ReaderError> {
        let inner = self.read_form()?;
        let cl = universe().find_package("COMMON-LISP").unwrap();
        let (head, _) = cl.intern(head_name);
        Ok(Value::list([Value::Symbol(head), inner]))
    }

    // -- sharp dispatch ------------------------------------------------------

    /// Returns `Some(value)` if a value is produced, `None` if the
    /// dispatch was a `#+`/`#-` that skipped its form (caller loops
    /// to read the next form).
    fn handle_sharp(
        &mut self,
        ch: char,
        prefix: Option<u32>,
        span: Span,
    ) -> Result<Option<Value>, ReaderError> {
        match (ch, prefix) {
            ('\'', None) => Ok(Some(self.wrap_one("FUNCTION")?)),
            (':', None) => {
                let st = self.next_token_required("after #:")?;
                match st.token {
                    Token::Atom(a) => {
                        let folded = self.readtable.fold_atom(&a);
                        Ok(Some(Value::Symbol(Symbol::fresh_uninterned(Arc::from(
                            folded.as_str(),
                        )))))
                    }
                    other => Err(self.err_at(
                        ReaderErrorKind::UnexpectedToken {
                            found: format!("{other:?}"),
                            expected: "atom after #:",
                        },
                        Some(st.span),
                    )),
                }
            }
            ('(', None) => self.read_vector_literal().map(Some),
            ('+', None) => self.read_feature_test_then_form(true).map(|v| v),
            ('-', None) => self.read_feature_test_then_form(false).map(|v| v),
            ('.', None) => Err(self.err_at(
                ReaderErrorKind::ReadEvalUnsupported,
                Some(span),
            )),
            ('B' | 'b', None) => self.read_radix_atom(2).map(Some),
            ('O' | 'o', None) => self.read_radix_atom(8).map(Some),
            ('X' | 'x', None) => self.read_radix_atom(16).map(Some),
            ('R' | 'r', Some(n)) => {
                if !(2..=36).contains(&n) {
                    return Err(self.err_at(ReaderErrorKind::BadRadix(n), Some(span)));
                }
                self.read_radix_atom(n).map(Some)
            }
            ('A' | 'a', _) => Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: 'A',
                    prefix,
                    reason: "array literals (#A) not supported in Phase 1c",
                },
                Some(span),
            )),
            ('S' | 's', None) => Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: 'S',
                    prefix,
                    reason: "struct literals (#S) not supported in Phase 1c",
                },
                Some(span),
            )),
            ('P' | 'p', None) => Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: 'P',
                    prefix,
                    reason: "pathname literals (#P) not supported in Phase 1c",
                },
                Some(span),
            )),
            ('C' | 'c', None) => self.read_complex_literal(span).map(Some),
            _ => Err(self.err_at(
                ReaderErrorKind::UnknownSharpDispatch { ch, prefix },
                Some(span),
            )),
        }
    }

    fn read_vector_literal(&mut self) -> Result<Value, ReaderError> {
        let mut items = Vec::new();
        loop {
            let st = self.peek_token()?.ok_or(ReaderError {
                kind: ReaderErrorKind::UnexpectedEof("inside #(...)"),
                span: None,
            })?;
            if matches!(st.token, Token::RParen) {
                self.next_token()?;
                return Ok(Value::Vector(Arc::new(items)));
            }
            items.push(self.read_form()?);
        }
    }

    /// `#C(re im)` — complex literal. We don't carry a Value::Complex
    /// variant; instead, emit a `(complex re im)` Cons that the
    /// lowering / evaluation pass turns into a heap-allocated
    /// complex Word. The COMPLEX function (registered by
    /// ncl-compiler) handles auto-demote of exact-zero imag.
    fn read_complex_literal(&mut self, _span: crate::token::Span)
        -> Result<Value, ReaderError>
    {
        // Expect `(re im)` next — read the list as a regular form.
        let inner = self.read_form()?;
        // Walk the cons: extract first and second.
        let (re, im) = match &inner {
            Value::Cons(c1) => {
                let re = c1.car.clone();
                match &c1.cdr {
                    Value::Cons(c2) => {
                        let im = c2.car.clone();
                        if !matches!(c2.cdr, Value::Nil) {
                            return Err(self.err_at(
                                ReaderErrorKind::UnsupportedSharpDispatch {
                                    ch: 'C',
                                    prefix: None,
                                    reason: "#C requires exactly (re im)",
                                },
                                None,
                            ));
                        }
                        (re, im)
                    }
                    _ => return Err(self.err_at(
                        ReaderErrorKind::UnsupportedSharpDispatch {
                            ch: 'C',
                            prefix: None,
                            reason: "#C requires (re im)",
                        },
                        None,
                    )),
                }
            }
            _ => return Err(self.err_at(
                ReaderErrorKind::UnsupportedSharpDispatch {
                    ch: 'C',
                    prefix: None,
                    reason: "#C requires a list",
                },
                None,
            )),
        };
        // Build the cons `(COMPLEX re im)`. Use the current-package
        // symbol resolution so COMPLEX picks up the user-facing
        // function correctly.
        let complex_sym = self.current_package.intern_external("COMPLEX");
        Ok(Value::cons(
            Value::Symbol(complex_sym),
            Value::cons(re, Value::cons(im, Value::Nil)),
        ))
    }

    fn read_feature_test_then_form(&mut self, want: bool) -> Result<Option<Value>, ReaderError> {
        // Read the feature expression in KEYWORD-package context so
        // bare names like `:foo` and `foo` both resolve to keywords.
        let keyword = universe().find_package("KEYWORD").unwrap();
        let saved = std::mem::replace(&mut self.current_package, keyword);
        let expr = self.read_form();
        self.current_package = saved;
        let expr = expr?;

        let active = eval_feature(&expr).map_err(|kind| ReaderError { kind, span: None })?;
        if active == want {
            Ok(Some(self.read_form()?))
        } else {
            // Skip the next form structurally rather than reading it
            // through the value-producing path. This is the
            // *read-suppress* discipline: we MUST consume the right
            // tokens, but we MUST NOT enforce package existence,
            // intern symbols, or eval `#.` while doing so. A real
            // example: `#+Genera (pushnew ... zwei:*foo*)` mentions
            // a package we don't have — but the form is meant to be
            // skipped, so the reference is fine.
            self.skip_form()?;
            Ok(None)
        }
    }

    /// Read and discard one form structurally — never producing a
    /// `Value` and never resolving symbols. Implements the suppression
    /// mode required by `#+`/`#-` discard, where the reader must walk
    /// the form to keep the token stream balanced but must not intern,
    /// must not eval `#.`, and must accept references to packages
    /// that aren't registered.
    fn skip_form(&mut self) -> Result<(), ReaderError> {
        let st = self.next_token_required("inside skipped form")?;
        self.skip_after_token(&st.token)
    }

    fn skip_after_token(&mut self, tok: &Token) -> Result<(), ReaderError> {
        match tok {
            Token::LParen => self.skip_until_close_paren(),
            Token::Quote
            | Token::Backquote
            | Token::Comma
            | Token::CommaAt
            | Token::CommaDot => self.skip_form(),
            Token::SharpDispatch { ch, prefix } => self.skip_after_sharp(*ch, *prefix),
            Token::SharpEquals(_) => self.skip_form(), // `#N=` prefixes one form
            // Self-contained tokens — already consumed.
            Token::SharpSharp(_)
            | Token::Atom(_)
            | Token::String(_)
            | Token::Char(_)
            | Token::FfiBlock { .. }
            | Token::Dot => Ok(()),
            Token::RParen => Err(self.err_at(
                ReaderErrorKind::UnexpectedToken {
                    found: ")".into(),
                    expected: "form (during skip)",
                },
                None,
            )),
        }
    }

    fn skip_until_close_paren(&mut self) -> Result<(), ReaderError> {
        loop {
            let st = self.next_token_required("inside skipped list")?;
            if matches!(st.token, Token::RParen) {
                return Ok(());
            }
            self.skip_after_token(&st.token)?;
        }
    }

    fn skip_after_sharp(&mut self, ch: char, prefix: Option<u32>) -> Result<(), ReaderError> {
        let _ = prefix;
        match ch {
            // Reader-macro chars that prefix one form.
            '\'' | ':' | '.' | 'A' | 'a' | 'S' | 's' | 'P' | 'p' | 'C' | 'c' => self.skip_form(),
            // Vector literal — same shape as a list.
            '(' => self.skip_until_close_paren(),
            // Recursive feature test — even in skip mode we need to
            // consume its expression and form, but we don't evaluate.
            '+' | '-' => {
                self.skip_form()?;
                self.skip_form()
            }
            // Radix dispatchers consume one digit atom.
            'B' | 'b' | 'O' | 'o' | 'X' | 'x' | 'R' | 'r' => {
                self.next_token_required("digits after radix in skipped form")?;
                Ok(())
            }
            // Anything else: assume one form follows. Conservative.
            _ => self.skip_form(),
        }
    }

    fn read_radix_atom(&mut self, radix: u32) -> Result<Value, ReaderError> {
        let st = self.next_token_required("digits after radix dispatch")?;
        match st.token {
            Token::Atom(a) => {
                if a.has_any_escape() {
                    return Err(self.err_at(
                        ReaderErrorKind::BadRadixDigits {
                            radix,
                            text: a.raw,
                        },
                        Some(st.span),
                    ));
                }
                // Try integer first, then ratio (`numerator/denominator`,
                // sign-on-numerator). Without the ratio fallback,
                // `#o-101/75` would surface as BadRadixDigits because
                // parse_integer chokes on the `/`. See parse_ratio_radix.
                if let Some(n) = parse_integer(&a.raw, radix) {
                    return Ok(Value::Fixnum(n));
                }
                if let Some(r) = parse_ratio_radix(&a.raw, radix) {
                    return Ok(r);
                }
                Err(self.err_at(
                    ReaderErrorKind::BadRadixDigits {
                        radix,
                        text: a.raw,
                    },
                    Some(st.span),
                ))
            }
            other => Err(self.err_at(
                ReaderErrorKind::UnexpectedToken {
                    found: format!("{other:?}"),
                    expected: "digit atom",
                },
                Some(st.span),
            )),
        }
    }

    // -- atom resolution -----------------------------------------------------

    fn resolve_atom(&self, atom: &AtomText) -> Result<Value, ReaderError> {
        // Numbers are recognised first, but only if no escape was in
        // play — `|123|` is the symbol named "123", not the integer.
        if !atom.has_any_escape() {
            if let Some(v) = try_parse_number(&atom.raw) {
                return Ok(v);
            }
        }

        let folded = self.readtable.fold_atom(atom);
        let (pkg_part, name_part, internal) = split_package(&folded, &atom.escapes);

        let universe = universe();
        let target_pkg = match pkg_part {
            None => Arc::clone(&self.current_package),
            Some(s) if s.is_empty() => universe.find_package("KEYWORD").unwrap(),
            Some(s) => universe
                .find_package(&s)
                .ok_or_else(|| self.err_at(ReaderErrorKind::PackageNotFound(s), None))?,
        };

        let cl = universe.find_package("COMMON-LISP").unwrap();
        if Arc::ptr_eq(&target_pkg, &cl) && name_part == "NIL" {
            return Ok(Value::Nil);
        }
        // Through inheritance: `nil` resolved in CL-USER also lands here.
        if let Some((found, _)) = target_pkg.find(&name_part) {
            if Arc::ptr_eq(&target_pkg, &cl) || (name_part == "NIL" && {
                let (_, vis) = target_pkg.find(&name_part).unwrap();
                vis == Visibility::Inherited
            }) {
                if name_part == "NIL" {
                    return Ok(Value::Nil);
                }
            }
            // Single-colon access (`pkg:name`) requires external. We
            // relax this for Phase 1c — see module note. KEYWORD is
            // always external by convention.
            let _ = internal;
            return Ok(Value::Symbol(found));
        }

        // Not found: intern. KEYWORD interns externally and self-binds.
        let keyword = universe.find_package("KEYWORD").unwrap();
        if Arc::ptr_eq(&target_pkg, &keyword) {
            let sym = target_pkg.intern_external(&name_part);
            // Keywords self-evaluate — bind value cell to the symbol itself.
            *sym.value.lock().unwrap() = Some(Value::Symbol(Arc::clone(&sym)));
            return Ok(Value::Symbol(sym));
        }
        let (sym, _) = target_pkg.intern(&name_part);
        Ok(Value::Symbol(sym))
    }
}

// -- helpers -----------------------------------------------------------------

fn build_dotted(items: Vec<Value>, tail: Value) -> Value {
    items
        .into_iter()
        .rev()
        .fold(tail, |acc, v| Value::cons(v, acc))
}

/// Split a case-folded atom name on its first unescaped colon.
///
/// `escapes` indexes the *original* atom characters and the folded
/// characters identically (folding doesn't change length).
fn split_package(folded: &str, escapes: &[bool]) -> (Option<String>, String, bool) {
    let chars: Vec<char> = folded.chars().collect();
    let mut sep = None;
    for (i, c) in chars.iter().enumerate() {
        let escaped = escapes.get(i).copied().unwrap_or(false);
        if *c == ':' && !escaped {
            sep = Some(i);
            break;
        }
    }
    let Some(idx) = sep else {
        return (None, folded.to_string(), false);
    };
    let pkg: String = chars[..idx].iter().collect();
    let mut after = idx + 1;
    let mut internal = false;
    if let Some(c) = chars.get(after) {
        if *c == ':' && !escapes.get(after).copied().unwrap_or(false) {
            internal = true;
            after += 1;
        }
    }
    let name: String = chars[after..].iter().collect();
    (Some(pkg), name, internal)
}

/// Walk a Lisp value as a `(NOT x)` / `(AND ...)` / `(OR ...)` /
/// keyword-symbol expression and decide whether it is "active" for
/// `#+` / `#-`. Errors on malformed expressions.
fn eval_feature(value: &Value) -> Result<bool, ReaderErrorKind> {
    match value {
        Value::Symbol(s) => Ok(universe().has_feature(&s.name)),
        Value::Cons(_) => {
            let items = list_to_vec(value).ok_or(ReaderErrorKind::InvalidFeatureExpr)?;
            let head = match items.first() {
                Some(Value::Symbol(s)) => Arc::clone(&s.name),
                _ => return Err(ReaderErrorKind::InvalidFeatureExpr),
            };
            let args = &items[1..];
            match head.to_ascii_uppercase().as_str() {
                "NOT" if args.len() == 1 => Ok(!eval_feature(&args[0])?),
                "AND" => {
                    for a in args {
                        if !eval_feature(a)? { return Ok(false); }
                    }
                    Ok(true)
                }
                "OR" => {
                    for a in args {
                        if eval_feature(a)? { return Ok(true); }
                    }
                    Ok(false)
                }
                _ => Err(ReaderErrorKind::InvalidFeatureExpr),
            }
        }
        Value::Nil => Ok(false),
        _ => Err(ReaderErrorKind::InvalidFeatureExpr),
    }
}

fn list_to_vec(v: &Value) -> Option<Vec<Value>> {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => return Some(out),
            Value::Cons(c) => {
                out.push(c.car.clone());
                cur = c.cdr.clone();
            }
            _ => return None, // dotted tail — not a proper list
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncl_runtime::FfiBlock;

    fn read_one(src: &str) -> Value {
        let mut p = Parser::new(src);
        p.read_form().expect(src)
    }

    fn read_all(src: &str) -> Vec<Value> {
        Parser::new(src).read_all().expect(src)
    }

    fn read_err(src: &str) -> ReaderErrorKind {
        Parser::new(src).read_form().unwrap_err().kind
    }

    fn sym_name(v: &Value) -> String {
        match v {
            Value::Symbol(s) => s.name.to_string(),
            _ => panic!("not a symbol: {v:?}"),
        }
    }

    fn sym_pkg(v: &Value) -> String {
        match v {
            Value::Symbol(s) => s
                .home
                .as_ref()
                .map(|p| p.name.to_string())
                .unwrap_or_else(|| "<uninterned>".into()),
            _ => panic!("not a symbol: {v:?}"),
        }
    }

    #[test]
    fn integers_and_floats() {
        assert!(matches!(read_one("42"), Value::Fixnum(42)));
        assert!(matches!(read_one("-3"), Value::Fixnum(-3)));
        assert!(matches!(read_one("1.5"), Value::Float(_)));
    }

    #[test]
    fn nil_normalised() {
        assert!(read_one("nil").is_nil());
        assert!(read_one("NIL").is_nil());
        assert!(read_one("()").is_nil());
        // 'nil reads as (QUOTE NIL) — but the inner NIL is Value::Nil.
        let v = read_one("'nil");
        let items = list_to_vec(&v).unwrap();
        assert_eq!(sym_name(&items[0]), "QUOTE");
        assert!(items[1].is_nil());
    }

    #[test]
    fn upcase_default() {
        assert_eq!(sym_name(&read_one("foo")), "FOO");
        assert_eq!(sym_name(&read_one("Foo")), "FOO");
    }

    #[test]
    fn package_qualifiers() {
        let v = read_one("ccl::quit");
        assert_eq!(sym_name(&v), "QUIT");
        assert_eq!(sym_pkg(&v), "CORMANLISP");

        let v = read_one("cl:quote");
        assert_eq!(sym_name(&v), "QUOTE");
        assert_eq!(sym_pkg(&v), "COMMON-LISP");

        let v = read_one(":foo");
        assert_eq!(sym_pkg(&v), "KEYWORD");
        assert_eq!(sym_name(&v), "FOO");
    }

    #[test]
    fn keyword_self_evaluates() {
        let v = read_one(":bar");
        match &v {
            Value::Symbol(s) => {
                let val = s.value.lock().unwrap().clone().unwrap();
                assert!(Value::eq(&val, &v));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn uninterned_symbol() {
        let v = read_one("#:gensym-1");
        match &v {
            Value::Symbol(s) => {
                assert_eq!(&*s.name, "GENSYM-1");
                assert!(s.home.is_none());
            }
            _ => panic!(),
        }
        // Two #:foo reads produce distinct symbols.
        let a = read_one("#:gensym");
        let b = read_one("#:gensym");
        assert!(!Value::eq(&a, &b));
    }

    #[test]
    fn list_and_dotted() {
        let v = read_one("(1 2 3)");
        let items = list_to_vec(&v).unwrap();
        assert!(matches!(items[0], Value::Fixnum(1)));
        assert!(matches!(items[1], Value::Fixnum(2)));
        assert!(matches!(items[2], Value::Fixnum(3)));

        let v = read_one("(1 . 2)");
        match v {
            Value::Cons(c) => {
                assert!(matches!(c.car, Value::Fixnum(1)));
                assert!(matches!(c.cdr, Value::Fixnum(2)));
            }
            _ => panic!(),
        }

        let v = read_one("(1 2 . 3)");
        match v {
            Value::Cons(c1) => match &c1.cdr {
                Value::Cons(c2) => assert!(matches!(c2.cdr, Value::Fixnum(3))),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn dot_misplaced_errors() {
        assert!(matches!(read_err("(. 1)"), ReaderErrorKind::DotMisplaced));
        assert!(matches!(read_err("(1 . 2 3)"), ReaderErrorKind::DotMisplaced));
    }

    #[test]
    fn quote_family_expands() {
        let v = read_one("'foo");
        let items = list_to_vec(&v).unwrap();
        assert_eq!(sym_name(&items[0]), "QUOTE");
        assert_eq!(sym_pkg(&items[0]), "COMMON-LISP");
        assert_eq!(sym_name(&items[1]), "FOO");

        let v = read_one("`(a ,b ,@c)");
        let items = list_to_vec(&v).unwrap();
        assert_eq!(sym_name(&items[0]), "BACKQUOTE");

        let v = read_one("#'foo");
        let items = list_to_vec(&v).unwrap();
        assert_eq!(sym_name(&items[0]), "FUNCTION");
        assert_eq!(sym_name(&items[1]), "FOO");
    }

    #[test]
    fn radix_numbers() {
        assert!(matches!(read_one("#xFF"), Value::Fixnum(255)));
        assert!(matches!(read_one("#xff"), Value::Fixnum(255)));
        assert!(matches!(read_one("#o777"), Value::Fixnum(0o777)));
        assert!(matches!(read_one("#b1010"), Value::Fixnum(10)));
        assert!(matches!(read_one("#36rZZ"), Value::Fixnum(_)));
    }

    #[test]
    fn vector_literal() {
        let v = read_one("#(1 2 3)");
        match &v {
            Value::Vector(vec) => {
                assert_eq!(vec.len(), 3);
                assert!(matches!(vec[0], Value::Fixnum(1)));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn feature_test_active_keeps_form() {
        // :NEWCORMANLISP is in features → form is read.
        let v = read_one("#+newcormanlisp 42");
        assert!(matches!(v, Value::Fixnum(42)));
    }

    #[test]
    fn feature_test_inactive_skips_form() {
        let v = read_one("#+nope 42 99");
        // 42 was skipped, 99 is what we read.
        assert!(matches!(v, Value::Fixnum(99)));
    }

    #[test]
    fn feature_minus_inverts() {
        let v = read_one("#-newcormanlisp 42 99");
        assert!(matches!(v, Value::Fixnum(99)));
    }

    #[test]
    fn feature_expr_combinators() {
        let v = read_one("#+(and newcormanlisp 64-bit) 42");
        assert!(matches!(v, Value::Fixnum(42)));
        let v = read_one("#+(or x86 32-bit) 42 99");
        assert!(matches!(v, Value::Fixnum(99)));
        let v = read_one("#+(not x86) 42");
        assert!(matches!(v, Value::Fixnum(42)));
    }

    #[test]
    fn ffi_block_passes_through() {
        let src = "#! (:library \"k\")\nbody\n!#";
        let v = read_one(src);
        match &v {
            Value::FfiBlock(b) => {
                let _: &FfiBlock = b;
                assert!(b.header.contains(":library"));
                assert_eq!(b.body, "body\n");
            }
            _ => panic!("got {v:?}"),
        }
    }

    #[test]
    fn read_eval_unsupported_for_now() {
        assert!(matches!(read_err("#.(+ 1 2)"), ReaderErrorKind::ReadEvalUnsupported));
    }

    #[test]
    fn unicode_atom_upcases_ascii_only() {
        // ASCII letters are folded; non-ASCII (`é`) is left alone
        // because to_ascii_uppercase is a no-op outside ASCII.
        let v = read_one("café");
        assert_eq!(sym_name(&v), "CAFé");
    }

    #[test]
    fn realistic_program_parses() {
        let src = r#"
            (defun fact (n)
              (if (<= n 1)
                  1
                  (* n (fact (- n 1)))))
            (defun greet (name)
              (format t "Hello, ~A!~%" name))
        "#;
        let forms = read_all(src);
        assert_eq!(forms.len(), 2);
        for f in &forms {
            let items = list_to_vec(f).unwrap();
            assert_eq!(sym_name(&items[0]), "DEFUN");
        }
    }

    #[test]
    fn readtable_preserve() {
        let mut p = Parser::new("Foo BAR baz");
        p.readtable.case = crate::readtable::ReadtableCase::Preserve;
        let forms = p.read_all().unwrap();
        assert_eq!(sym_name(&forms[0]), "Foo");
        assert_eq!(sym_name(&forms[1]), "BAR");
        assert_eq!(sym_name(&forms[2]), "baz");
    }

    #[test]
    fn readtable_invert() {
        let mut p = Parser::new("foo BAR Mixed");
        p.readtable.case = crate::readtable::ReadtableCase::Invert;
        let forms = p.read_all().unwrap();
        assert_eq!(sym_name(&forms[0]), "FOO");   // uniform lower → upper
        assert_eq!(sym_name(&forms[1]), "bar");   // uniform upper → lower
        assert_eq!(sym_name(&forms[2]), "Mixed"); // mixed → preserve
    }

    #[test]
    fn unknown_package_errors() {
        assert!(matches!(
            read_err("nopackage:foo"),
            ReaderErrorKind::PackageNotFound(_)
        ));
    }

    #[test]
    fn unknown_sharp_dispatch_errors() {
        assert!(matches!(
            read_err("#?foo"),
            ReaderErrorKind::UnknownSharpDispatch { ch: '?', .. }
        ));
    }
}
