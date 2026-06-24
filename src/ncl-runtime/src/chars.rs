//! Character predicates and conversions — the Rust-side primitives
//! underlying `Lisp/Library/characters.lisp`.
//!
//! Every primitive accepts a single `Word` and either:
//!   * returns T / NIL for a Boolean predicate,
//!   * returns a fixnum for `char-code`,
//!   * returns a character Word for `code-char` / case-conversion,
//!   * returns a fixnum (the digit weight 0..radix-1) or NIL for
//!     `digit-char-p`.
//!
//! Unlike Corman's `char-code-limit = 256`, NCL's CHAR is a full
//! Unicode scalar value (U+0000 … U+10FFFF). All the predicates
//! defer to Rust's `char` methods, which implement the Unicode
//! categories correctly. The Lisp-side wrappers (variadic char=,
//! char<, etc.) live in characters.lisp; only the unary primitives
//! that actually require Rust live here.
//!
//! Tier roughly equivalent to Corman's Sys/characters.lisp.

use crate::abi::signal_condition_string;
use crate::mutator::MutatorState;
use crate::word::Word;

/// Pull the i-th argument from the call frame.
fn arg(args: *const u64, i: u64) -> Word {
    Word::from_raw(unsafe { *args.add(i as usize) })
}

/// Demand a Character argument; signals a condition if it isn't.
/// Returns the underlying Rust `char` on success.
fn demand_char(mutator: *mut MutatorState, name: &str, w: Word) -> Result<char, u64> {
    match w.as_char() {
        Some(c) => Ok(c),
        None => Err(signal_condition_string(
            mutator,
            &format!("{name}: not a character: {w:?}"),
        )),
    }
}

/// Demand exactly N arguments; signals a condition otherwise.
fn require_arity(mutator: *mut MutatorState, name: &str, want: u64, got: u64) -> Option<u64> {
    if got != want {
        Some(signal_condition_string(
            mutator,
            &format!("{name}: expected {want} arg(s), got {got}"),
        ))
    } else {
        None
    }
}

// ─── Coercions: char-code / code-char ──────────────────────────────────

/// `(char-code ch)` — Unicode codepoint of CH, as a fixnum.
/// Corman aliases this as `char-int`.
pub extern "C-unwind" fn char_code_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "char-code", 1, n_args) {
        return e;
    }
    let c = match demand_char(mutator, "char-code", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    Word::fixnum(c as i64).raw()
}

/// `(code-char n)` — the character whose Unicode codepoint is N.
/// Returns NIL for codepoints outside U+0000..U+10FFFF or in the
/// surrogate range (D800..DFFF). Corman aliases as `int-char`.
pub extern "C-unwind" fn code_char_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "code-char", 1, n_args) {
        return e;
    }
    let n = match arg(args, 0).as_fixnum() {
        Some(n) => n,
        None => {
            return signal_condition_string(
                mutator,
                "code-char: argument must be an integer codepoint",
            );
        }
    };
    if !(0..=0x10_FFFF).contains(&n) {
        return Word::NIL.raw();
    }
    match char::from_u32(n as u32) {
        Some(c) => Word::char(c).raw(),
        None => Word::NIL.raw(), // surrogates, etc.
    }
}

// ─── Case conversion ────────────────────────────────────────────────────

/// `(char-upcase ch)` — uppercase mapping of CH.
///
/// Rust's `char::to_uppercase` returns an iterator because some
/// codepoints expand under Unicode case-folding (German ß →
/// "SS", Turkish dotless ı → "I", etc.). CL's `char-upcase` is
/// scalar-to-scalar by spec, so we take the first folded char and
/// drop any continuation. For ASCII this is exact; for the long
/// tail of CJK / accented Latin / Cyrillic it's the closest
/// faithful approximation.
pub extern "C-unwind" fn char_upcase_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "char-upcase", 1, n_args) {
        return e;
    }
    let c = match demand_char(mutator, "char-upcase", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let up = c.to_uppercase().next().unwrap_or(c);
    Word::char(up).raw()
}

/// `(char-downcase ch)` — lowercase mapping. Same single-char
/// approximation as `char-upcase`.
pub extern "C-unwind" fn char_downcase_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "char-downcase", 1, n_args) {
        return e;
    }
    let c = match demand_char(mutator, "char-downcase", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let dn = c.to_lowercase().next().unwrap_or(c);
    Word::char(dn).raw()
}

// ─── Predicates ─────────────────────────────────────────────────────────
//
// Each predicate takes one Word, demands a character, returns
// Word::T or Word::NIL.

macro_rules! char_predicate {
    ($name:ident, $err_name:literal, $method:ident) => {
        pub extern "C-unwind" fn $name(
            mutator: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            if let Some(e) = require_arity(mutator, $err_name, 1, n_args) {
                return e;
            }
            let c = match demand_char(mutator, $err_name, arg(args, 0)) {
                Ok(c) => c,
                Err(e) => return e,
            };
            if c.$method() { Word::T.raw() } else { Word::NIL.raw() }
        }
    };
}

char_predicate!(alpha_char_p_shim,   "alpha-char-p",  is_alphabetic);
char_predicate!(alphanumericp_shim,  "alphanumericp", is_alphanumeric);
char_predicate!(upper_case_p_shim,   "upper-case-p",  is_uppercase);
char_predicate!(lower_case_p_shim,   "lower-case-p",  is_lowercase);

/// `(both-case-p ch)` — T iff CH has *some* case mapping (i.e.,
/// `char-upcase` or `char-downcase` would not be the identity).
///
/// Corman implements this as `(or (lower-case-p x) (upper-case-p x))`.
/// We do the same — Rust's `is_uppercase`/`is_lowercase` already
/// know about the cased-letter category, so this is one fast path.
pub extern "C-unwind" fn both_case_p_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "both-case-p", 1, n_args) {
        return e;
    }
    let c = match demand_char(mutator, "both-case-p", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if c.is_uppercase() || c.is_lowercase() {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(graphic-char-p ch)` — T iff CH is a *graphic* (printable, not
/// a control character). Per CLHS: #\Space is graphic; #\Newline,
/// #\Tab, #\Return, #\Backspace, etc. are not. Rust's
/// `char::is_control` returns false for ' ' and true for those
/// control chars, so the answer is `!c.is_control()` on the nose.
pub extern "C-unwind" fn graphic_char_p_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(e) = require_arity(mutator, "graphic-char-p", 1, n_args) {
        return e;
    }
    let c = match demand_char(mutator, "graphic-char-p", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    if !c.is_control() { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(digit-char-p ch &optional radix)` — if CH is a digit in the
/// given radix (default 10), returns its numeric weight as a
/// fixnum (0..radix-1); otherwise NIL.
///
/// Radix is clamped to 2..=36 per CL. Corman signals on out-of-
/// range radix; we do the same.
pub extern "C-unwind" fn digit_char_p_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if !(1..=2).contains(&n_args) {
        return signal_condition_string(
            mutator,
            "digit-char-p: expected 1 or 2 args (char [radix])",
        );
    }
    let c = match demand_char(mutator, "digit-char-p", arg(args, 0)) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let radix = if n_args == 2 {
        match arg(args, 1).as_fixnum() {
            Some(r) if (2..=36).contains(&r) => r as u32,
            Some(_) => {
                return signal_condition_string(
                    mutator,
                    "digit-char-p: radix must be in 2..=36",
                );
            }
            None => {
                return signal_condition_string(
                    mutator,
                    "digit-char-p: radix must be an integer",
                );
            }
        }
    } else {
        10
    };
    match c.to_digit(radix) {
        Some(d) => Word::fixnum(d as i64).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(digit-char weight &optional radix)` — the inverse of
/// `digit-char-p`. Given an integer weight 0..radix-1, return the
/// canonical uppercase digit character (`'0'..'9'` then `'A'..'Z'`);
/// returns NIL if the weight is out of range for the radix.
pub extern "C-unwind" fn digit_char_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if !(1..=2).contains(&n_args) {
        return signal_condition_string(
            mutator,
            "digit-char: expected 1 or 2 args (weight [radix])",
        );
    }
    let weight = match arg(args, 0).as_fixnum() {
        Some(w) if w >= 0 => w,
        _ => {
            return signal_condition_string(
                mutator,
                "digit-char: weight must be a non-negative integer",
            );
        }
    };
    let radix = if n_args == 2 {
        match arg(args, 1).as_fixnum() {
            Some(r) if (2..=36).contains(&r) => r,
            _ => {
                return signal_condition_string(
                    mutator,
                    "digit-char: radix must be in 2..=36",
                );
            }
        }
    } else {
        10
    };
    if weight >= radix {
        return Word::NIL.raw();
    }
    let c = if weight < 10 {
        char::from_u32(b'0' as u32 + weight as u32).unwrap()
    } else {
        char::from_u32(b'A' as u32 + (weight as u32 - 10)).unwrap()
    };
    Word::char(c).raw()
}

/// `(%make-string-fill n ch)` — fresh young-heap string of N copies
/// of character CH, allocated and filled in one pass. The primitive
/// behind `make-string`; every Lisp-side construction loop
/// (string-upcase and friends fill the result in place with
/// `(setf (char ...))`) relies on this being O(n), not append-per-char.
pub extern "C-unwind" fn make_string_fill_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if let Some(cond) = require_arity(mutator, "%make-string-fill", 2, n_args) {
        return cond;
    }
    let n = match arg(args, 0).as_fixnum() {
        Some(v) if v >= 0 => v as usize,
        _ => {
            return signal_condition_string(
                mutator,
                "%make-string-fill: size must be a non-negative fixnum",
            );
        }
    };
    let c = match demand_char(mutator, "%make-string-fill", arg(args, 1)) {
        Ok(c) => c,
        Err(cond) => return cond,
    };
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_filled(m, n, c).raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise a unary-char predicate by hand, without going
    /// through the call protocol. Wraps the Word in the raw
    /// argument layout the shim expects.
    fn call_predicate(
        shim: extern "C-unwind" fn(*mut MutatorState, u64, *const u64, u64) -> u64,
        arg_word: Word,
    ) -> Word {
        let raw = arg_word.raw();
        let args_ptr: *const u64 = &raw;
        // We pass a null mutator: the shims only need it to signal
        // conditions on error, and the predicates we test never
        // hit that path.
        Word::from_raw(shim(std::ptr::null_mut(), 0, args_ptr, 1))
    }

    #[test]
    fn alpha_char_p_recognises_letters_across_scripts() {
        assert_eq!(call_predicate(alpha_char_p_shim, Word::char('a')), Word::T);
        assert_eq!(call_predicate(alpha_char_p_shim, Word::char('Z')), Word::T);
        // Cyrillic Г is U+0413; is_alphabetic returns true.
        assert_eq!(call_predicate(alpha_char_p_shim, Word::char('\u{0413}')), Word::T);
        assert_eq!(call_predicate(alpha_char_p_shim, Word::char('5')), Word::NIL);
        assert_eq!(call_predicate(alpha_char_p_shim, Word::char(' ')), Word::NIL);
    }

    #[test]
    fn upper_lower_case_p_are_complementary_for_cased_letters() {
        assert_eq!(call_predicate(upper_case_p_shim, Word::char('A')), Word::T);
        assert_eq!(call_predicate(upper_case_p_shim, Word::char('a')), Word::NIL);
        assert_eq!(call_predicate(lower_case_p_shim, Word::char('a')), Word::T);
        assert_eq!(call_predicate(lower_case_p_shim, Word::char('A')), Word::NIL);
        // Digits and punctuation are neither.
        assert_eq!(call_predicate(upper_case_p_shim, Word::char('7')), Word::NIL);
        assert_eq!(call_predicate(lower_case_p_shim, Word::char('7')), Word::NIL);
    }

    #[test]
    fn both_case_p_says_yes_to_cased_letters_no_to_punctuation() {
        assert_eq!(call_predicate(both_case_p_shim, Word::char('A')), Word::T);
        assert_eq!(call_predicate(both_case_p_shim, Word::char('z')), Word::T);
        assert_eq!(call_predicate(both_case_p_shim, Word::char('5')), Word::NIL);
        assert_eq!(call_predicate(both_case_p_shim, Word::char('!')), Word::NIL);
    }

    #[test]
    fn char_code_and_code_char_round_trip_ascii_and_unicode() {
        let raw = Word::char('Q').raw();
        let p: *const u64 = &raw;
        let code = Word::from_raw(char_code_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(code.as_fixnum(), Some(b'Q' as i64));

        let raw = Word::char('Г').raw();
        let p: *const u64 = &raw;
        let code = Word::from_raw(char_code_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(code.as_fixnum(), Some(0x0413));

        let raw = Word::fixnum(0x0413).raw();
        let p: *const u64 = &raw;
        let ch = Word::from_raw(code_char_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(ch.as_char(), Some('\u{0413}'));
    }

    #[test]
    fn code_char_rejects_surrogate_and_oob_codepoints() {
        // U+D800 (low surrogate) is not a valid scalar value.
        let raw = Word::fixnum(0xD800).raw();
        let p: *const u64 = &raw;
        let ch = Word::from_raw(code_char_shim(std::ptr::null_mut(), 0, p, 1));
        assert!(ch.is_nil());

        // Above U+10FFFF is out of Unicode.
        let raw = Word::fixnum(0x11_0000).raw();
        let p: *const u64 = &raw;
        let ch = Word::from_raw(code_char_shim(std::ptr::null_mut(), 0, p, 1));
        assert!(ch.is_nil());
    }

    #[test]
    fn char_upcase_and_downcase_round_trip_for_ascii() {
        let raw = Word::char('a').raw();
        let p: *const u64 = &raw;
        let up = Word::from_raw(char_upcase_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(up.as_char(), Some('A'));

        let raw = Word::char('Z').raw();
        let p: *const u64 = &raw;
        let dn = Word::from_raw(char_downcase_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(dn.as_char(), Some('z'));
    }

    #[test]
    fn digit_char_p_returns_weight_for_digits() {
        let raw = Word::char('7').raw();
        let p: *const u64 = &raw;
        let w = Word::from_raw(digit_char_p_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(w.as_fixnum(), Some(7));

        // 'A' is not a decimal digit ...
        let raw = Word::char('A').raw();
        let p: *const u64 = &raw;
        let w = Word::from_raw(digit_char_p_shim(std::ptr::null_mut(), 0, p, 1));
        assert!(w.is_nil());

        // ... but it is hex digit 10.
        let raws = [Word::char('A').raw(), Word::fixnum(16).raw()];
        let p: *const u64 = raws.as_ptr();
        let w = Word::from_raw(digit_char_p_shim(std::ptr::null_mut(), 0, p, 2));
        assert_eq!(w.as_fixnum(), Some(10));
    }

    #[test]
    fn digit_char_returns_canonical_digit() {
        // digit-char 7 → #\7
        let raw = Word::fixnum(7).raw();
        let p: *const u64 = &raw;
        let c = Word::from_raw(digit_char_shim(std::ptr::null_mut(), 0, p, 1));
        assert_eq!(c.as_char(), Some('7'));

        // digit-char 10 16 → #\A (hex digit ten)
        let raws = [Word::fixnum(10).raw(), Word::fixnum(16).raw()];
        let p: *const u64 = raws.as_ptr();
        let c = Word::from_raw(digit_char_shim(std::ptr::null_mut(), 0, p, 2));
        assert_eq!(c.as_char(), Some('A'));

        // digit-char 16 16 → NIL (16 is out of radix-16's range)
        let raws = [Word::fixnum(16).raw(), Word::fixnum(16).raw()];
        let p: *const u64 = raws.as_ptr();
        let c = Word::from_raw(digit_char_shim(std::ptr::null_mut(), 0, p, 2));
        assert!(c.is_nil());
    }
}
