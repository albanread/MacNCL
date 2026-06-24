//! Coverage for `Lisp/Library/characters.lisp` — the character
//! predicate / case-conversion / variadic-comparison surface ported
//! from Corman's Sys/characters.lisp.
//!
//! The Rust-side primitives have their own unit tests in
//! ncl_runtime::chars::tests. This file exercises the *Lisp*
//! wrappers — the variadic CHAR= / CHAR< / CHAR-EQUAL family, the
//! NAME-CHAR / CHAR-NAME lookup, and the (CHARACTER ...) coercion —
//! end-to-end through the JIT.

use std::path::PathBuf;

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn library_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("Lisp");
    p.push("Library");
    p.push(name);
    p
}

fn fresh_session_with_characters() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots with stdlib");
    s.activate();
    let path = library_path("characters.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/characters.lisp loads");
    TestSession::with_thread_name(s)
}

// ── Primitive coercions ────────────────────────────────────────────────

#[test]
fn char_code_and_code_char_round_trip_through_lisp() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(char-code #\A)").unwrap(), "65");
    assert_eq!(s.eval(r"(code-char 65)").unwrap(), "#\\A");
    // Round-trip through CHAR-INT / INT-CHAR aliases.
    assert_eq!(s.eval(r"(int-char (char-int #\Q))").unwrap(), "#\\Q");
}

#[test]
fn code_char_returns_nil_on_invalid_codepoint() {
    let mut s = fresh_session_with_characters();
    // U+D800 is a surrogate, not a valid Unicode scalar.
    assert_eq!(s.eval("(code-char #xD800)").unwrap(), "nil");
    // Above U+10FFFF.
    assert_eq!(s.eval("(code-char #x110000)").unwrap(), "nil");
}

// ── Case conversion ────────────────────────────────────────────────────

#[test]
fn char_upcase_and_downcase_work_on_ascii() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(char-upcase #\a)").unwrap(), "#\\A");
    assert_eq!(s.eval(r"(char-downcase #\Z)").unwrap(), "#\\z");
    // Non-letter passes through unchanged.
    assert_eq!(s.eval(r"(char-upcase #\5)").unwrap(), "#\\5");
}

// ── Predicates ─────────────────────────────────────────────────────────

#[test]
fn alpha_char_p_recognises_letters_and_rejects_others() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(alpha-char-p #\a)").unwrap(), "T");
    assert_eq!(s.eval(r"(alpha-char-p #\Z)").unwrap(), "T");
    assert_eq!(s.eval(r"(alpha-char-p #\5)").unwrap(), "nil");
    assert_eq!(s.eval(r"(alpha-char-p #\!)").unwrap(), "nil");
    assert_eq!(s.eval(r"(alpha-char-p #\ )").unwrap(), "nil");
}

#[test]
fn case_predicates_separate_lower_and_upper() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(upper-case-p #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(upper-case-p #\a)").unwrap(), "nil");
    assert_eq!(s.eval(r"(lower-case-p #\a)").unwrap(), "T");
    assert_eq!(s.eval(r"(lower-case-p #\A)").unwrap(), "nil");
    // Digits and punctuation aren't either.
    assert_eq!(s.eval(r"(upper-case-p #\5)").unwrap(), "nil");
    assert_eq!(s.eval(r"(lower-case-p #\5)").unwrap(), "nil");
}

#[test]
fn both_case_p_recognises_cased_letters_only() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(both-case-p #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(both-case-p #\a)").unwrap(), "T");
    assert_eq!(s.eval(r"(both-case-p #\5)").unwrap(), "nil");
    assert_eq!(s.eval(r"(both-case-p #\.)").unwrap(), "nil");
}

#[test]
fn alphanumericp_says_yes_to_letters_and_digits() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(alphanumericp #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(alphanumericp #\7)").unwrap(), "T");
    assert_eq!(s.eval(r"(alphanumericp #\_)").unwrap(), "nil");
    assert_eq!(s.eval(r"(alphanumericp #\space)").unwrap(), "nil");
}

#[test]
fn graphic_char_p_distinguishes_printable_from_control() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(graphic-char-p #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(graphic-char-p #\Space)").unwrap(), "T");
    assert_eq!(s.eval(r"(graphic-char-p #\Newline)").unwrap(), "nil");
    assert_eq!(s.eval(r"(graphic-char-p #\Tab)").unwrap(), "nil");
}

// ── digit-char-p / digit-char ──────────────────────────────────────────

#[test]
fn digit_char_p_returns_weight_or_nil() {
    let mut s = fresh_session_with_characters();
    // Decimal digits.
    assert_eq!(s.eval(r"(digit-char-p #\0)").unwrap(), "0");
    assert_eq!(s.eval(r"(digit-char-p #\7)").unwrap(), "7");
    // Letter — NIL in decimal.
    assert_eq!(s.eval(r"(digit-char-p #\A)").unwrap(), "nil");
    // ... but a weight in radix 16.
    assert_eq!(s.eval(r"(digit-char-p #\A 16)").unwrap(), "10");
    assert_eq!(s.eval(r"(digit-char-p #\F 16)").unwrap(), "15");
    // Lowercase hex digits work too (Rust's to_digit is case-insensitive).
    assert_eq!(s.eval(r"(digit-char-p #\f 16)").unwrap(), "15");
}

#[test]
fn digit_char_returns_canonical_digit() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval("(digit-char 0)").unwrap(), "#\\0");
    assert_eq!(s.eval("(digit-char 9)").unwrap(), "#\\9");
    assert_eq!(s.eval("(digit-char 10 16)").unwrap(), "#\\A");
    assert_eq!(s.eval("(digit-char 15 16)").unwrap(), "#\\F");
    // Out of range for the radix.
    assert_eq!(s.eval("(digit-char 16 16)").unwrap(), "nil");
}

// ── Variadic comparison chains ──────────────────────────────────────────

#[test]
fn char_eq_is_variadic_and_true_for_all_equal() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(char= #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(char= #\A #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(char= #\A #\A #\A #\A)").unwrap(), "T");
    assert_eq!(s.eval(r"(char= #\A #\B)").unwrap(), "nil");
    // (CHAR=) with no args is T per spec.
    assert_eq!(s.eval("(char=)").unwrap(), "T");
}

#[test]
fn char_lt_chains_strictly() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(char< #\A #\B #\C)").unwrap(), "T");
    assert_eq!(s.eval(r"(char< #\A #\C #\B)").unwrap(), "nil");
    // Equal-adjacent — strict < fails.
    assert_eq!(s.eval(r"(char< #\A #\A)").unwrap(), "nil");
    // <= permits equality.
    assert_eq!(s.eval(r"(char<= #\A #\A #\B)").unwrap(), "T");
}

#[test]
fn char_neq_is_pairwise_distinctness() {
    let mut s = fresh_session_with_characters();
    // All distinct.
    assert_eq!(s.eval(r"(char/= #\A #\B #\C)").unwrap(), "T");
    // Duplicate at the ends — caught by the all-pairs walk, not
    // just by adjacent comparison.
    assert_eq!(s.eval(r"(char/= #\A #\B #\A)").unwrap(), "nil");
    // Adjacent duplicates likewise.
    assert_eq!(s.eval(r"(char/= #\A #\A #\B)").unwrap(), "nil");
}

#[test]
fn char_equal_is_case_insensitive() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(char-equal #\A #\a)").unwrap(), "T");
    assert_eq!(s.eval(r"(char-equal #\A #\A #\a #\a)").unwrap(), "T");
    assert_eq!(s.eval(r"(char-equal #\A #\B)").unwrap(), "nil");
}

#[test]
fn char_lessp_orders_case_insensitively() {
    let mut s = fresh_session_with_characters();
    // 'a' < 'B' if we ignore case (both uppercase to A and B).
    assert_eq!(s.eval(r"(char-lessp #\a #\B)").unwrap(), "T");
    // Case-sensitive (#\a > #\B by codepoint), so plain char<
    // disagrees with char-lessp here.
    assert_eq!(s.eval(r"(char< #\a #\B)").unwrap(), "nil");
}

// ── Named characters ──────────────────────────────────────────────────

#[test]
fn char_name_and_name_char_round_trip_standard_names() {
    let mut s = fresh_session_with_characters();
    // char-name produces the canonical printable name.
    assert_eq!(s.eval(r"(char-name #\Space)").unwrap(), "\"Space\"");
    assert_eq!(s.eval(r"(char-name #\Newline)").unwrap(), "\"Newline\"");
    assert_eq!(s.eval(r"(char-name #\Tab)").unwrap(), "\"Tab\"");
    assert_eq!(s.eval(r"(char-name #\Return)").unwrap(), "\"Return\"");
    // name-char round-trips. The printer now emits the standard
    // names for these well-known control characters, so the
    // returned character round-trips through prin1 back to its
    // named form.
    assert_eq!(
        s.eval(r#"(name-char "Space")"#).unwrap(),
        "#\\Space",
    );
    assert_eq!(
        s.eval(r#"(name-char "newline")"#).unwrap(),
        "#\\Newline",
    );
    // Capitalisation in the lookup is irrelevant.
    assert_eq!(
        s.eval(r#"(name-char "TAB")"#).unwrap(),
        "#\\Tab",
    );
    // Unknown name → NIL.
    assert_eq!(s.eval(r#"(name-char "no-such-name")"#).unwrap(), "nil");
}

#[test]
fn character_coerces_string_or_one_letter_symbol() {
    let mut s = fresh_session_with_characters();
    assert_eq!(s.eval(r"(character #\X)").unwrap(), "#\\X");
    assert_eq!(s.eval(r#"(character "Q")"#).unwrap(), "#\\Q");
    // One-letter symbol — case-folded to uppercase by the reader.
    assert_eq!(s.eval("(character 'a)").unwrap(), "#\\A");
}

// ── Unicode reach ─────────────────────────────────────────────────────

#[test]
fn predicates_reach_beyond_ascii() {
    let mut s = fresh_session_with_characters();
    // Cyrillic Г is a letter (U+0413).
    assert_eq!(
        s.eval("(alpha-char-p (code-char #x0413))").unwrap(),
        "T",
    );
    // Greek lowercase π (U+03C0).
    assert_eq!(
        s.eval("(lower-case-p (code-char #x03C0))").unwrap(),
        "T",
    );
    // Devanagari digit ३ (U+0969) — Unicode considers it
    // alphanumeric / numeric.
    assert_eq!(
        s.eval("(alphanumericp (code-char #x0969))").unwrap(),
        "T",
    );
}
