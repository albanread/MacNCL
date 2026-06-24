//! `format` — CL's printf, ported in earnest for Tier 1.B.
//!
//! Architecture: the control string is lexed once into a `Vec<
//! Directive>`, then interpreted. This separates parsing from
//! execution, makes iteration (`~{ ~}`) and conditionals (`~[ ~]`)
//! easy because we have a flat sequence we can index into, and
//! keeps the interpreter loop free of `~`-escape gymnastics.
//!
//! Directives covered (with prefix args + `:` / `@` modifiers
//! where the spec gives them meaning):
//!
//!   ~A   aesthetic (princ-style)   — `~mincol[,colinc[,minpad[,'pad]]]A`,
//!                                    `@` right-aligns
//!   ~S   standard (prin1-style)    — same prefix-arg shape
//!   ~D   decimal integer           — `~mincol[,'pad[,'comma[,interval]]]D`,
//!                                    `:` inserts commas, `@` forces sign
//!   ~B   binary
//!   ~O   octal
//!   ~X   hexadecimal
//!   ~C   character                 — `:` spells out name (e.g. "Space")
//!   ~%   newline
//!   ~&   newline (simplified fresh-line — no column tracking yet)
//!   ~~   literal `~`
//!   ~P   plural — emits "" for 1, "s" for anything else.
//!                 `:` uses the previous arg instead of consuming.
//!                 `@` emits "y" / "ies" instead.
//!   ~T   tabulate — `~colT` pads with spaces to column.
//!                 (Approximate; we don't track real column.)
//!   ~{...~}    iteration over a list arg. `~^` inside breaks
//!              the iteration when an arg is exhausted.
//!   ~[...~;...~]
//!              conditional. `~n[…~]` selects clause N. `~:[…~]`
//!              picks clause 0 for NIL, clause 1 otherwise. `~@[…~]`
//!              tests arg, emits clause if non-nil (consuming it
//!              only on the nil branch).
//!   ~*         arg-pointer manipulation. `~n*` skip forward N,
//!              `~:*` skip backward 1, `~n:*` skip back N, `~@*`
//!              jump to absolute index N.
//!   ~?         recursive format. Consumes the next two args: a
//!              control string and an arg list.
//!
//! Deferred (signalled at runtime with a clear message):
//!   ~F ~E ~G   need the float tower
//!   ~R         needs bignum support to be useful beyond fixnums
//!   ~< ~>      column-aware justification (no column tracking yet)
//!   ~/foo/     user-defined directives

use crate::word::{Tag, Word};

// ─── Directive AST ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Directive {
    Literal(String),
    Newline,
    /// Whitespace-collapsing newline (`~\<newline>`): drops the newline
    /// and any following whitespace, useful for line continuations.
    /// Currently treated as plain newline; full handling can land later.
    Tilde,
    /// ~A and ~S share the same prefix-arg shape.
    Aesthetic(PadSpec),
    Standard(PadSpec),
    /// ~D ~B ~O ~X all share the same integer-formatting shape.
    Integer(IntSpec),
    Character {
        spelled: bool,
    },
    Plural {
        previous: bool, // `:` modifier
        y_form: bool,   // `@` modifier
    },
    /// `~F` — fixed-format float. `~w,dF` = width w, d decimal places.
    /// Width and decimals both optional; if absent we fall back to
    /// ryu's shortest representation.
    FixedFloat {
        width: Option<usize>,
        decimals: Option<usize>,
        force_sign: bool,
    },
    /// `~E` — exponential / scientific notation.
    ExpFloat {
        width: Option<usize>,
        decimals: Option<usize>,
        force_sign: bool,
    },
    /// `~G` — general format: ~F for reasonable magnitudes,
    /// ~E for very large or very small.
    GenFloat {
        width: Option<usize>,
        decimals: Option<usize>,
        force_sign: bool,
    },
    Tab {
        column: usize,
    },
    /// Marker for the start of a `~{ ... ~}` block. Holds the index
    /// (in the directive vector) of the matching `~}`.
    IterStart {
        end_idx: usize,
    },
    /// Closing `~}`. The interpreter never executes one directly —
    /// the loop body terminates by reaching this index.
    IterEnd,
    /// Force-end iteration when the rest is empty.
    IterEscape,
    /// `~[`. Holds the indices of every `~;` separator inside, then
    /// the matching `~]`. The variant chooses how to pick a clause.
    CondStart {
        clauses: Vec<usize>, // clause start indices in the directive vec
        end_idx: usize,
        flavour: CondFlavour,
        prefix_arg: Option<i64>, // ~5[ selects clause 5
    },
    /// Internal marker for clause boundaries. Never executed.
    CondSep,
    /// Internal marker for `~]`. Never executed.
    CondEnd,
    /// `~*` family.
    Skip {
        n: usize,
        absolute: bool, // `@*`
        backward: bool, // `:*`
    },
    Recursive,
    /// `~( ... ~)` case-conversion block. Renders the inner directives
    /// into a temporary buffer, then case-transforms it:
    ///   - `~(`   → lowercase
    ///   - `~:(`  → Capitalize Each Word
    ///   - `~@(`  → Capitalize first word (rest lowercased)
    ///   - `~:@(` → UPPERCASE
    /// `end_idx` is the matching `~)`, patched in pass 2.
    CaseConvStart {
        end_idx: usize,
        colon: bool,
        at: bool,
    },
    /// Closing `~)`. Never executed directly — the block terminates by
    /// reaching this index.
    CaseConvEnd,
    /// Anything not yet supported. The directive char is stored so the
    /// runtime can produce a precise error message.
    Unsupported(char),
}

#[derive(Debug, Clone, Copy)]
enum CondFlavour {
    /// `~[ ~] / ~5[ ~]` — pick by arg or prefix.
    Index,
    /// `~:[ ~]` — clause 0 for NIL, clause 1 otherwise.
    Bool,
    /// `~@[ ~]` — emit clause iff arg is non-nil (consumed only
    /// if nil, kept on the stream otherwise).
    NotNil,
}

#[derive(Debug, Clone, Copy)]
struct PadSpec {
    mincol: usize,
    pad_char: char,
    right_align: bool, // `@`
}

impl PadSpec {
    fn default() -> Self {
        Self { mincol: 0, pad_char: ' ', right_align: false }
    }
}

#[derive(Debug, Clone, Copy)]
struct IntSpec {
    radix: u32,
    mincol: usize,
    pad_char: char,
    comma_char: char,
    comma_interval: usize,
    use_commas: bool, // `:`
    force_sign: bool, // `@`
}

impl IntSpec {
    fn for_radix(radix: u32) -> Self {
        Self {
            radix,
            mincol: 0,
            pad_char: ' ',
            comma_char: ',',
            comma_interval: 3,
            use_commas: false,
            force_sign: false,
        }
    }
}

// ─── Lexer ──────────────────────────────────────────────────────────────────

/// Parsed prefix arguments collected before a directive char.
#[derive(Debug, Clone, Default)]
struct PrefixArgs {
    /// Each prefix-arg slot can be a number, a char, or absent.
    args: Vec<PrefixArg>,
    colon: bool,
    at_sign: bool,
}

#[derive(Debug, Clone)]
enum PrefixArg {
    Num(i64),
    Char(char),
    Empty,
}

impl PrefixArgs {
    fn num(&self, i: usize) -> Option<i64> {
        match self.args.get(i) {
            Some(PrefixArg::Num(n)) => Some(*n),
            _ => None,
        }
    }
    fn ch(&self, i: usize) -> Option<char> {
        match self.args.get(i) {
            Some(PrefixArg::Char(c)) => Some(*c),
            _ => None,
        }
    }
}

fn lex(ctrl: &str) -> Result<Vec<Directive>, String> {
    let chars: Vec<char> = ctrl.chars().collect();
    let mut directives = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c != '~' {
            buf.push(c);
            i += 1;
            continue;
        }
        if !buf.is_empty() {
            directives.push(Directive::Literal(std::mem::take(&mut buf)));
        }
        i += 1;
        let (prefix, next_i) = parse_prefix_args(&chars, i)?;
        i = next_i;
        if i >= chars.len() {
            return Err("trailing '~' with no directive".into());
        }
        let dc = chars[i].to_ascii_uppercase();
        i += 1;
        match directive_for(dc, &prefix) {
            Some(d) => directives.push(d),
            None => directives.push(Directive::Unsupported(dc)),
        }
    }
    if !buf.is_empty() {
        directives.push(Directive::Literal(buf));
    }

    // Second pass: link up `~{ ~}` and `~[ ~; ~]` brackets so the
    // interpreter can skip past them in O(1).
    resolve_brackets(&mut directives)?;
    Ok(directives)
}

fn parse_prefix_args(
    chars: &[char],
    mut i: usize,
) -> Result<(PrefixArgs, usize), String> {
    let mut pa = PrefixArgs::default();
    let mut current_slot: Option<PrefixArg> = None;
    loop {
        if i >= chars.len() {
            break;
        }
        let c = chars[i];
        match c {
            '0'..='9' | '-' => {
                let start = i;
                if c == '-' { i += 1; }
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let n: i64 = chars[start..i]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .map_err(|e| format!("bad prefix integer: {e}"))?;
                current_slot = Some(PrefixArg::Num(n));
            }
            '\'' => {
                i += 1;
                if i >= chars.len() {
                    return Err("'<char>' prefix arg with no char".into());
                }
                current_slot = Some(PrefixArg::Char(chars[i]));
                i += 1;
            }
            ',' => {
                pa.args.push(current_slot.unwrap_or(PrefixArg::Empty));
                current_slot = None;
                i += 1;
            }
            ':' => {
                pa.colon = true;
                i += 1;
            }
            '@' => {
                pa.at_sign = true;
                i += 1;
            }
            _ => break,
        }
    }
    if let Some(s) = current_slot {
        pa.args.push(s);
    }
    Ok((pa, i))
}

fn directive_for(dc: char, pa: &PrefixArgs) -> Option<Directive> {
    match dc {
        'A' => Some(Directive::Aesthetic(pad_spec(pa))),
        'S' => Some(Directive::Standard(pad_spec(pa))),
        'D' => Some(Directive::Integer(int_spec(pa, 10))),
        'B' => Some(Directive::Integer(int_spec(pa, 2))),
        'O' => Some(Directive::Integer(int_spec(pa, 8))),
        'X' => Some(Directive::Integer(int_spec(pa, 16))),
        'R' => {
            // ~nR — output in radix N. Prefix arg 0 is the radix
            // (2..=36); subsequent prefix args follow the same
            // shape as ~D (mincol, pad, comma, comma-interval).
            let radix = pa.num(0).unwrap_or(10);
            if !(2..=36).contains(&radix) {
                return Some(Directive::Unsupported('R'));
            }
            // Shift prefix args: the radix takes slot 0, so the
            // standard mincol etc. start at slot 1.
            let mut s = IntSpec::for_radix(radix as u32);
            if let Some(n) = pa.num(1) { s.mincol = n.max(0) as usize; }
            if let Some(c) = pa.ch(2)  { s.pad_char = c; }
            if let Some(c) = pa.ch(3)  { s.comma_char = c; }
            if let Some(n) = pa.num(4) { s.comma_interval = n.max(1) as usize; }
            s.use_commas = pa.colon;
            s.force_sign = pa.at_sign;
            Some(Directive::Integer(s))
        }
        'C' => Some(Directive::Character { spelled: pa.colon }),
        'P' => Some(Directive::Plural {
            previous: pa.colon,
            y_form: pa.at_sign,
        }),
        'F' => Some(Directive::FixedFloat {
            width: pa.num(0).map(|n| n.max(0) as usize),
            decimals: pa.num(1).map(|n| n.max(0) as usize),
            force_sign: pa.at_sign,
        }),
        'E' => Some(Directive::ExpFloat {
            width: pa.num(0).map(|n| n.max(0) as usize),
            decimals: pa.num(1).map(|n| n.max(0) as usize),
            force_sign: pa.at_sign,
        }),
        'G' => Some(Directive::GenFloat {
            width: pa.num(0).map(|n| n.max(0) as usize),
            decimals: pa.num(1).map(|n| n.max(0) as usize),
            force_sign: pa.at_sign,
        }),
        'T' => Some(Directive::Tab {
            column: pa.num(0).unwrap_or(1).max(0) as usize,
        }),
        '{' => Some(Directive::IterStart { end_idx: 0 }), // patched in pass 2
        '}' => Some(Directive::IterEnd),
        '(' => Some(Directive::CaseConvStart {
            end_idx: 0, // patched in pass 2
            colon: pa.colon,
            at: pa.at_sign,
        }),
        ')' => Some(Directive::CaseConvEnd),
        '^' => Some(Directive::IterEscape),
        '[' => Some(Directive::CondStart {
            clauses: Vec::new(),
            end_idx: 0,
            flavour: if pa.colon {
                CondFlavour::Bool
            } else if pa.at_sign {
                CondFlavour::NotNil
            } else {
                CondFlavour::Index
            },
            prefix_arg: pa.num(0),
        }),
        ';' => Some(Directive::CondSep),
        ']' => Some(Directive::CondEnd),
        '*' => Some(Directive::Skip {
            n: pa.num(0).unwrap_or(1).max(0) as usize,
            absolute: pa.at_sign,
            backward: pa.colon,
        }),
        '?' => Some(Directive::Recursive),
        '%' | '&' => Some(Directive::Newline),
        '~' => Some(Directive::Tilde),
        _ => None,
    }
}

fn pad_spec(pa: &PrefixArgs) -> PadSpec {
    let mut p = PadSpec::default();
    if let Some(n) = pa.num(0) { p.mincol = n.max(0) as usize; }
    if let Some(c) = pa.ch(3)  { p.pad_char = c; }
    p.right_align = pa.at_sign;
    p
}

fn int_spec(pa: &PrefixArgs, radix: u32) -> IntSpec {
    let mut s = IntSpec::for_radix(radix);
    if let Some(n) = pa.num(0) { s.mincol = n.max(0) as usize; }
    if let Some(c) = pa.ch(1)  { s.pad_char = c; }
    if let Some(c) = pa.ch(2)  { s.comma_char = c; }
    if let Some(n) = pa.num(3) { s.comma_interval = n.max(1) as usize; }
    s.use_commas = pa.colon;
    s.force_sign = pa.at_sign;
    s
}

/// Walk the directive list, patching `IterStart` and `CondStart`
/// with the indices of their matching closers. Nested `{ }` is
/// supported; nested `[ ]` is supported.
fn resolve_brackets(ds: &mut [Directive]) -> Result<(), String> {
    // First pass: brace matching.
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..ds.len() {
        match &ds[i] {
            Directive::IterStart { .. } => stack.push(i),
            Directive::IterEnd => {
                let start = stack.pop().ok_or("~} with no matching ~{")?;
                if let Directive::IterStart { end_idx } = &mut ds[start] {
                    *end_idx = i;
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("~{ with no matching ~}".into());
    }
    // Case-conversion block matching: ~( ~). Nesting supported.
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..ds.len() {
        match &ds[i] {
            Directive::CaseConvStart { .. } => stack.push(i),
            Directive::CaseConvEnd => {
                let start = stack.pop().ok_or("~) with no matching ~(")?;
                if let Directive::CaseConvStart { end_idx, .. } = &mut ds[start] {
                    *end_idx = i;
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("~( with no matching ~)".into());
    }
    // Second pass: bracket matching.
    let mut stack: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..ds.len() {
        match &ds[i] {
            Directive::CondStart { .. } => stack.push((i, Vec::new())),
            Directive::CondSep => {
                if let Some(top) = stack.last_mut() {
                    top.1.push(i);
                } else {
                    return Err("~; outside ~[ ~]".into());
                }
            }
            Directive::CondEnd => {
                let (start, clauses) =
                    stack.pop().ok_or("~] with no matching ~[")?;
                if let Directive::CondStart {
                    clauses: c_out,
                    end_idx,
                    ..
                } = &mut ds[start]
                {
                    *c_out = clauses;
                    *end_idx = i;
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("~[ with no matching ~]".into());
    }
    Ok(())
}

// ─── Interpreter ────────────────────────────────────────────────────────────

/// The argument cursor. The interpreter walks a flat Vec<Word> rather
/// than a linked list — gives O(1) random access for `~*` / `~@*`.
struct Cursor {
    args: Vec<Word>,
    i: usize,
}

impl Cursor {
    fn from_list(list: Word) -> Self {
        let mut args = Vec::new();
        let mut cur = list;
        while !cur.is_nil() {
            if cur.tag() != Tag::Cons {
                break;
            }
            let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
            args.push(Word::from_raw(unsafe { *p }));
            cur = Word::from_raw(unsafe { *p.add(1) });
        }
        Self { args, i: 0 }
    }
    fn pop(&mut self) -> Result<Word, String> {
        if self.i >= self.args.len() {
            return Err("not enough args for control string".into());
        }
        let a = self.args[self.i];
        self.i += 1;
        Ok(a)
    }
    fn peek(&self) -> Option<Word> {
        self.args.get(self.i).copied()
    }
    fn remaining(&self) -> usize {
        self.args.len() - self.i
    }
}

/// Run-format entry point — Word-arg-list flavour. Builds a Cursor
/// and dispatches into the directive interpreter.
pub fn run_format(
    m: &mut crate::mutator::MutatorState,
    dest: Word,
    ctrl: Word,
    args_list: Word,
) -> Word {
    if ctrl.tag() != Tag::String {
        panic!("format: control argument must be a string, got {ctrl:?}");
    }
    let src: String = crate::gc_string::chars_of(ctrl).collect();
    let directives = match lex(&src) {
        Ok(d) => d,
        Err(e) => panic!("format: {e}"),
    };
    let mut cursor = Cursor::from_list(args_list);
    let mut out = String::new();
    if let Err(e) = exec(&directives, 0, directives.len(), &mut cursor, &mut out, m) {
        panic!("format: {e}");
    }

    if dest.is_t() {
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        let _ = h.write_all(out.as_bytes());
        let _ = h.flush();
        Word::NIL
    } else if dest.is_nil() {
        crate::gc_string::alloc_string_in_young(m, &out)
    } else {
        // Lisp-level wrapper handles stream dests by passing NIL
        // here and pushing the result through stream-write-string.
        panic!(
            "format: native format only handles t (stdout) or nil (return \
             string). Stream destinations are routed via the Lisp wrapper."
        );
    }
}

/// Execute directives in the slice `ds[start..end]`. Recurses into
/// itself for `~{ ~}` and `~[ ~]` sub-ranges.
fn exec(
    ds: &[Directive],
    start: usize,
    end: usize,
    cur: &mut Cursor,
    out: &mut String,
    m: &mut crate::mutator::MutatorState,
) -> Result<(), String> {
    let mut i = start;
    while i < end {
        match &ds[i] {
            Directive::Literal(s) => out.push_str(s),
            Directive::Newline => out.push('\n'),
            Directive::Tilde => out.push('~'),
            Directive::Aesthetic(p) => {
                let arg = cur.pop()?;
                let rendered = crate::printer::format_word_aesthetic(arg);
                emit_padded(out, &rendered, p);
            }
            Directive::Standard(p) => {
                let arg = cur.pop()?;
                let rendered = crate::printer::format_word(arg);
                emit_padded(out, &rendered, p);
            }
            Directive::Integer(spec) => {
                let arg = cur.pop()?;
                let rendered = render_integer(arg, spec)?;
                out.push_str(&rendered);
            }
            Directive::Character { spelled } => {
                let arg = cur.pop()?;
                let c = arg
                    .as_char()
                    .ok_or_else(|| format!("~C: argument is not a character: {arg:?}"))?;
                if *spelled {
                    out.push_str(&char_name(c));
                } else {
                    out.push(c);
                }
            }
            Directive::Plural { previous, y_form } => {
                // Step back one if `:`. Otherwise consume one and look at it.
                let n = if *previous {
                    if cur.i == 0 {
                        return Err("~:P with no previous arg".into());
                    }
                    cur.args[cur.i - 1]
                } else {
                    cur.pop()?
                };
                let is_one = n.as_fixnum() == Some(1);
                if *y_form {
                    out.push_str(if is_one { "y" } else { "ies" });
                } else {
                    out.push_str(if is_one { "" } else { "s" });
                }
            }
            Directive::Tab { column } => {
                // Best-effort: pad with spaces until we reach the
                // target column. Counts from the LAST newline in
                // the output buffer.
                let col_now = out.rfind('\n').map(|nl| out.len() - nl - 1).unwrap_or(out.len());
                let needed = column.saturating_sub(col_now);
                for _ in 0..needed {
                    out.push(' ');
                }
            }
            Directive::FixedFloat { width, decimals, force_sign } => {
                let arg = cur.pop()?;
                let f = match crate::float::to_f64(arg) {
                    Some(f) => f,
                    None => return Err(format!("~F: not a real number: {arg:?}")),
                };
                let body = match decimals {
                    Some(d) => format!("{:.1$}", f, d),
                    None => format!("{}", f),
                };
                emit_float(out, &body, f, *width, *force_sign, ' ');
            }
            Directive::ExpFloat { width, decimals, force_sign } => {
                let arg = cur.pop()?;
                let f = match crate::float::to_f64(arg) {
                    Some(f) => f,
                    None => return Err(format!("~E: not a real number: {arg:?}")),
                };
                let body = match decimals {
                    Some(d) => format!("{:.1$E}", f, d),
                    None => format!("{:E}", f),
                };
                emit_float(out, &body, f, *width, *force_sign, ' ');
            }
            Directive::GenFloat { width, decimals, force_sign } => {
                let arg = cur.pop()?;
                let f = match crate::float::to_f64(arg) {
                    Some(f) => f,
                    None => return Err(format!("~G: not a real number: {arg:?}")),
                };
                // CL ~G rule: use fixed if the result fits and the
                // magnitude is reasonable, else exponential.
                let abs_f = f.abs();
                let use_exp =
                    abs_f != 0.0 && (abs_f < 1e-4 || abs_f >= 1e16);
                let body = if use_exp {
                    match decimals {
                        Some(d) => format!("{:.1$E}", f, d),
                        None => format!("{:E}", f),
                    }
                } else {
                    match decimals {
                        Some(d) => format!("{:.1$}", f, d),
                        None => format!("{}", f),
                    }
                };
                emit_float(out, &body, f, *width, *force_sign, ' ');
            }
            Directive::IterStart { end_idx } => {
                run_iter(ds, i + 1, *end_idx, cur, out, m)?;
                i = *end_idx; // jump past the ~}
            }
            Directive::IterEnd | Directive::IterEscape => {
                // Outside an iteration, ~} and ~^ are no-ops by
                // CL convention; just skip.
            }
            Directive::CondStart { clauses, end_idx, flavour, prefix_arg } => {
                run_cond(ds, i, clauses, *end_idx, *flavour, *prefix_arg,
                         cur, out, m)?;
                i = *end_idx;
            }
            Directive::CondSep | Directive::CondEnd => {
                // Encountered outside their parent — no-op.
            }
            Directive::CaseConvStart { end_idx, colon, at } => {
                // Render the inner directives into a temp buffer, then
                // case-transform the whole thing and append.
                let mut tmp = String::new();
                exec(ds, i + 1, *end_idx, cur, &mut tmp, m)?;
                out.push_str(&apply_case(&tmp, *colon, *at));
                i = *end_idx; // jump past the ~)
            }
            Directive::CaseConvEnd => {
                // Outside a ~( block, ~) is a no-op.
            }
            Directive::Skip { n, absolute, backward } => {
                if *absolute {
                    cur.i = *n;
                } else if *backward {
                    cur.i = cur.i.saturating_sub(*n);
                } else {
                    cur.i += *n;
                }
            }
            Directive::Recursive => {
                let ctrl_arg = cur.pop()?;
                let args_arg = cur.pop()?;
                if ctrl_arg.tag() != Tag::String {
                    return Err("~? expected a control string".into());
                }
                let inner_src: String =
                    crate::gc_string::chars_of(ctrl_arg).collect();
                let inner_ds = lex(&inner_src)?;
                let mut inner_cur = Cursor::from_list(args_arg);
                exec(&inner_ds, 0, inner_ds.len(), &mut inner_cur, out, m)?;
            }
            Directive::Unsupported(c) => {
                return Err(format!("directive ~{c} is not yet supported"));
            }
        }
        i += 1;
    }
    Ok(())
}

/// Case-transform a rendered `~( ... ~)` block body.
///   - neither flag (`~(`)   → lowercase
///   - colon only  (`~:(`)   → Capitalize Each Word
///   - at only     (`~@(`)   → Capitalize first word, lowercase the rest
///   - both        (`~:@(`)  → UPPERCASE
fn apply_case(s: &str, colon: bool, at: bool) -> String {
    match (colon, at) {
        (false, false) => s.to_lowercase(),
        (true, true) => s.to_uppercase(),
        (true, false) => {
            // Capitalize each word: first alphanumeric of each run
            // uppercased, the rest of the run lowercased. A "word" is a
            // maximal run of alphanumerics (CL's string-capitalize).
            let mut out = String::with_capacity(s.len());
            let mut in_word = false;
            for ch in s.chars() {
                if ch.is_alphanumeric() {
                    if in_word {
                        out.extend(ch.to_lowercase());
                    } else {
                        out.extend(ch.to_uppercase());
                        in_word = true;
                    }
                } else {
                    out.push(ch);
                    in_word = false;
                }
            }
            out
        }
        (false, true) => {
            // Capitalize the first word only: uppercase the first
            // alphabetic character, lowercase everything else.
            let mut out = String::with_capacity(s.len());
            let mut done_first = false;
            for ch in s.chars() {
                if !done_first && ch.is_alphabetic() {
                    out.extend(ch.to_uppercase());
                    done_first = true;
                } else {
                    out.extend(ch.to_lowercase());
                }
            }
            out
        }
    }
}

fn run_iter(
    ds: &[Directive],
    body_start: usize,
    body_end: usize,
    outer: &mut Cursor,
    out: &mut String,
    m: &mut crate::mutator::MutatorState,
) -> Result<(), String> {
    // The argument is a LIST to iterate over. `~^` inside the body
    // can abort the iteration when args are exhausted; we signal
    // that via a special error string and unwind once.
    let arg = outer.pop()?;
    let mut sub = Cursor::from_list(arg);
    loop {
        if sub.remaining() == 0 {
            break;
        }
        match exec_with_escape(ds, body_start, body_end, &mut sub, out, m) {
            Ok(_) => {}
            Err(e) if e == "__iter_escape__" => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Like exec but `~^` causes early return with a sentinel error
/// that run_iter catches.
fn exec_with_escape(
    ds: &[Directive],
    start: usize,
    end: usize,
    cur: &mut Cursor,
    out: &mut String,
    m: &mut crate::mutator::MutatorState,
) -> Result<(), String> {
    let mut i = start;
    while i < end {
        if let Directive::IterEscape = &ds[i] {
            if cur.remaining() == 0 {
                return Err("__iter_escape__".into());
            }
            i += 1;
            continue;
        }
        // Single-step exec for one directive.
        exec(ds, i, i + 1, cur, out, m)?;
        i += 1;
    }
    Ok(())
}

fn run_cond(
    ds: &[Directive],
    start: usize,
    clauses: &[usize],
    end_idx: usize,
    flavour: CondFlavour,
    prefix_arg: Option<i64>,
    cur: &mut Cursor,
    out: &mut String,
    m: &mut crate::mutator::MutatorState,
) -> Result<(), String> {
    // Clause boundaries: body starts at start+1 and is split by
    // every index in `clauses`. The closing `~]` is at end_idx.
    let mut bounds: Vec<usize> = Vec::with_capacity(clauses.len() + 2);
    bounds.push(start + 1);
    for &c in clauses {
        bounds.push(c + 1); // body of this clause starts AFTER the ~;
    }
    bounds.push(end_idx); // and ends at the ~]

    // Per-clause ranges: [bounds[k], bounds[k+1] - 1) — we exclude
    // the ~; / ~] markers themselves.
    let clause_starts: Vec<usize> = bounds[..bounds.len() - 1].to_vec();
    let clause_ends: Vec<usize> = clauses
        .iter()
        .copied()
        .chain(std::iter::once(end_idx))
        .collect();

    let n_clauses = clause_starts.len();

    match flavour {
        CondFlavour::Index => {
            let idx = match prefix_arg {
                Some(n) => n as i64,
                None => match cur.pop()? {
                    a => a
                        .as_fixnum()
                        .ok_or_else(|| "~[ selector must be a fixnum".to_string())?,
                },
            };
            let pick = if idx < 0 || (idx as usize) >= n_clauses {
                // Default clause = last one IFF the last separator
                // was ~:; — we don't track that yet, so just drop
                // out silently for out-of-range. Equivalent to no-op.
                return Ok(());
            } else {
                idx as usize
            };
            exec(ds, clause_starts[pick], clause_ends[pick], cur, out, m)?;
        }
        CondFlavour::Bool => {
            let arg = cur.pop()?;
            let pick = if arg.is_nil() { 0 } else { 1 };
            if pick < n_clauses {
                exec(ds, clause_starts[pick], clause_ends[pick], cur, out, m)?;
            }
        }
        CondFlavour::NotNil => {
            // ~@[: peek; if nil, consume and skip; else leave on
            // stream and emit clause 0.
            let arg = cur.peek().unwrap_or(Word::NIL);
            if arg.is_nil() {
                // consume, skip clause
                let _ = cur.pop();
            } else if n_clauses > 0 {
                exec(ds, clause_starts[0], clause_ends[0], cur, out, m)?;
            }
        }
    }
    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Render a float body with optional width and force-sign. The body
/// is the already-formatted number text (from Rust's `{:.5}` or
/// `{:E}`); we just add a `+` if needed and right-pad to width.
fn emit_float(out: &mut String, body: &str, f: f64, width: Option<usize>, force_sign: bool, pad: char) {
    let mut s = String::new();
    if force_sign && f.is_sign_positive() && !body.starts_with('+') {
        s.push('+');
    }
    s.push_str(body);
    let len = s.chars().count();
    if let Some(w) = width {
        if w > len {
            for _ in 0..(w - len) { out.push(pad); }
        }
    }
    out.push_str(&s);
}

fn emit_padded(out: &mut String, s: &str, p: &PadSpec) {
    let len = s.chars().count();
    let pad = p.mincol.saturating_sub(len);
    if p.right_align {
        for _ in 0..pad { out.push(p.pad_char); }
        out.push_str(s);
    } else {
        out.push_str(s);
        for _ in 0..pad { out.push(p.pad_char); }
    }
}

fn render_integer(arg: Word, spec: &IntSpec) -> Result<String, String> {
    // Accept fixnums and bignums uniformly. num-bigint's
    // to_str_radix handles all radices 2..=36.
    use num_traits::Signed;
    let n = match crate::bignum::integer_to_bigint(arg) {
        Some(n) => n,
        None => return Err(format!("integer directive expected an integer: {arg:?}")),
    };
    let abs_n = n.abs();
    let mut digits = abs_n.to_str_radix(spec.radix);
    // num-bigint produces lowercase hex; CL ~X uses uppercase.
    if spec.radix > 10 {
        digits = digits.to_uppercase();
    }
    if spec.use_commas {
        digits = insert_commas(&digits, spec.comma_char, spec.comma_interval);
    }
    let mut s = String::new();
    if n.sign() == num_bigint::Sign::Minus {
        s.push('-');
    } else if spec.force_sign {
        s.push('+');
    }
    s.push_str(&digits);
    let len = s.chars().count();
    if spec.mincol > len {
        let pad = spec.mincol - len;
        let mut out = String::new();
        for _ in 0..pad { out.push(spec.pad_char); }
        out.push_str(&s);
        Ok(out)
    } else {
        Ok(s)
    }
}

fn insert_commas(digits: &str, c: char, interval: usize) -> String {
    let chars: Vec<char> = digits.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len() + chars.len() / interval);
    for (i, &d) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i) % interval == 0 {
            out.push(c);
        }
        out.push(d);
    }
    out.into_iter().collect()
}

fn char_name(c: char) -> String {
    match c {
        ' ' => "Space".into(),
        '\n' => "Newline".into(),
        '\t' => "Tab".into(),
        '\r' => "Return".into(),
        '\0' => "Null".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_ok(s: &str) -> Vec<Directive> {
        lex(s).expect("lex")
    }

    #[test]
    fn lex_literal_only() {
        let ds = lex_ok("hello");
        assert_eq!(ds.len(), 1);
        matches!(ds[0], Directive::Literal(_));
    }

    #[test]
    fn lex_with_directives() {
        let ds = lex_ok("answer: ~D~%");
        assert_eq!(ds.len(), 3);
    }

    #[test]
    fn lex_iter_bracket_matching() {
        let ds = lex_ok("~{~A~^, ~}");
        // IterStart's end_idx must point at the IterEnd.
        if let Directive::IterStart { end_idx } = ds[0] {
            assert!(matches!(ds[end_idx], Directive::IterEnd));
        } else {
            panic!("expected IterStart at 0");
        }
    }
}
