//! Coverage for `Lisp/Library/describe.lisp` — `(describe obj)`
//! and the underlying introspection primitives (`type-of`,
//! `boundp`, `symbol-value`) that landed alongside.
//!
//! DESCRIBE's output is meant for humans and the exact shape
//! isn't part of the CL spec — different implementations format
//! differently. We assert against a STRING-OUTPUT-STREAM capture
//! checking for the substrings each describer must produce
//! (type label, value, structural counts), not against an exact
//! pretty-printed layout.

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

fn fresh_session_with_describe() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    // describe leans on: streams (make-string-output-stream),
    // characters (char-name), trees (none — but cheap), and
    // describe itself. Load the dependencies in order.
    for name in [
        "streams.lisp",
        "characters.lisp",
        "describe.lisp",
    ] {
        let path = library_path(name);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
    }
    TestSession::with_thread_name(s)
}

/// Capture DESCRIBE's output by passing an explicit
/// string-output-stream. FORM is wrapped so it sees `*stream*` as
/// the stream to describe to; the body of the form is responsible
/// for calling (describe x *stream*).
fn describe_to_string(s: &mut Session, target: &str) -> String {
    let prog = format!(
        "(let ((*stream* (make-string-output-stream)))
           (describe {} *stream*)
           (get-output-stream-string *stream*))",
        target
    );
    let r = s.eval(&prog).unwrap();
    // eval returns the printed form of the captured string, with
    // surrounding double-quotes and embedded escapes (e.g. \n for
    // newline). We strip the quotes; the tests substring-match
    // against literal tokens that don't include newlines.
    r.trim_matches('"').to_string()
}

// ── New primitives ────────────────────────────────────────────────────

#[test]
fn type_of_classifies_every_basic_kind() {
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    // Each TYPE-OF result is a symbol; the printed form is its name.
    assert_eq!(s.eval("(type-of 42)").unwrap(),       "FIXNUM");
    assert_eq!(s.eval("(type-of '(1 2))").unwrap(),   "CONS");
    assert_eq!(s.eval(r#"(type-of "hi")"#).unwrap(),  "STRING");
    assert_eq!(s.eval(r"(type-of #\a)").unwrap(),     "CHARACTER");
    assert_eq!(s.eval("(type-of 'foo)").unwrap(),     "SYMBOL");
    assert_eq!(s.eval("(type-of nil)").unwrap(),      "NULL");
    assert_eq!(s.eval("(type-of t)").unwrap(),        "BOOLEAN");
    assert_eq!(s.eval("(type-of #'car)").unwrap(),    "FUNCTION");
}

#[test]
fn boundp_separates_bound_from_unbound() {
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    let prog = "
        (defparameter *bound-1* 42)
        (list (boundp '*bound-1*) (boundp 'never-set-by-anyone))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(T nil)");
}

#[test]
fn symbol_value_reads_value_cell() {
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    let prog = "
        (defparameter *thing* 99)
        (symbol-value '*thing*)
    ";
    assert_eq!(s.eval(prog).unwrap(), "99");
}

#[test]
fn symbol_value_signals_for_unbound() {
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    let r = s.eval(
        "(handler-case (symbol-value 'definitely-not-bound)
           (error (c) :caught))",
    );
    assert_eq!(r.unwrap(), ":CAUGHT");
}

// ── DESCRIBE — per-type output ────────────────────────────────────────

#[test]
fn describe_fixnum_includes_value_hex_binary() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, "42");
    assert!(out.contains("FIXNUM"),  "got: {out}");
    assert!(out.contains("42"),      "got: {out}");
    assert!(out.contains("#x2A"),    "got: {out}");
    assert!(out.contains("#b101010"),"got: {out}");
}

#[test]
fn describe_fixnum_handles_zero_and_negative() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, "0");
    assert!(out.contains("#x0"),  "got: {out}");
    assert!(out.contains("#b0"),  "got: {out}");
    let out = describe_to_string(&mut s, "-7");
    assert!(out.contains("FIXNUM"), "got: {out}");
    assert!(out.contains("-7"),     "got: {out}");
    assert!(out.contains("#x-7"),   "got: {out}");
}

#[test]
fn describe_string_shows_length_and_content() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, r#""hello""#);
    assert!(out.contains("STRING"),    "got: {out}");
    assert!(out.contains("length"),    "got: {out}");
    assert!(out.contains("5"),         "got: {out}");
    // The string is printed in readable form (~S), so the quotes
    // appear in the captured output.
    assert!(out.contains(r#"\"hello\""#),  "got: {out}");
}

#[test]
fn describe_character_shows_glyph_and_code() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, r"#\A");
    assert!(out.contains("CHARACTER"), "got: {out}");
    assert!(out.contains("65"),        "got: {out}");
}

#[test]
fn describe_character_includes_name_for_named_chars() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, r"#\Newline");
    assert!(out.contains("CHARACTER"), "got: {out}");
    assert!(out.contains("10"),        "got: {out}");
    assert!(out.contains("Newline"),   "got: {out}");
}

#[test]
fn describe_cons_reports_length_and_head() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, "'(a b c)");
    assert!(out.contains("CONS"),    "got: {out}");
    assert!(out.contains("3"),       "got: {out}");
    assert!(out.contains("proper"),  "got: {out}");
}

#[test]
fn describe_cons_handles_dotted_pair() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, "(cons 1 2)");
    assert!(out.contains("CONS"),     "got: {out}");
    assert!(out.contains("improper"), "got: {out}");
}

#[test]
fn describe_symbol_shows_name_and_cells() {
    let mut s = fresh_session_with_describe();
    // Set up the symbol first, then describe it.
    s.eval("(defparameter *qq* 99)").unwrap();
    let out = describe_to_string(&mut s, "'*qq*");
    assert!(out.contains("SYMBOL"),  "got: {out}");
    assert!(out.contains("*QQ*"),    "got: {out}");
    assert!(out.contains("99"),      "got: {out}");
}

#[test]
fn describe_nil_and_t_are_special_cased() {
    let mut s = fresh_session_with_describe();
    let out = describe_to_string(&mut s, "nil");
    assert!(out.contains("NULL"),    "got: {out}");
    let out = describe_to_string(&mut s, "t");
    assert!(out.contains("BOOLEAN"), "got: {out}");
}

#[test]
fn describe_returns_its_argument() {
    let mut s = fresh_session_with_describe();
    // (describe x stream) returns x; the second eval-form is
    // the value the test asserts.
    let r = s
        .eval(
            "(let ((s (make-string-output-stream)))
               (describe 42 s))",
        )
        .unwrap();
    assert_eq!(r, "42");
}
