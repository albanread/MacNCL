//! ANSI-test borrowed coverage for `Lisp/Library/xp.lisp`.
//!
//! Tests translated from Paul Dietz's CL ANSI test suite
//! (https://github.com/pfdietz/ansi-test, MIT-licensed,
//! Copyright 2004 Paul F. Dietz). Each `deftest` form becomes
//! one Rust `#[test]` that evals the test expression and asserts
//! the result equals the expected string.
//!
//! We can't run the .lsp files verbatim — they use packages
//! (`(in-package :cl-test)`, `find-package`), `with-standard-io-syntax`,
//! random fuzzers, and `signals-error`, none of which we have yet.
//! Each individual deftest, though, translates cleanly.
//!
//! Conventions:
//!   * `s.eval(...)` returns the printed representation of the
//!     value, so when the test expects a string result like `"(A)"`,
//!     the assertion target is `"\"(A)\""`.
//!   * Symbols come out uppercase under :upcase mode. Dietz uses
//!     `'|A|` to force lowercase; we use `'a` which reads as `A`.
//!   * NCL has no dynamic binding even for `defvar`'d names, so
//!     `(let ((*print-right-margin* 5)) …)` produces a LEXICAL
//!     local, not a dynamic rebinding. Tests that change print
//!     options must use setq with explicit save/restore.

use std::path::PathBuf;
use ncl_compiler::Session;
use ncl_tests::TestSession;

fn library_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); p.pop();
    p.push("Lisp"); p.push("Library"); p.push(name);
    p
}

fn fresh() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    for name in [
        "streams.lisp", "conditions.lisp", "loop.lisp",
        "sequences.lisp", "trees.lisp", "characters.lisp",
        "lists.lisp", "places.lisp", "numbers.lisp", "xp.lisp",
    ] {
        let src = std::fs::read_to_string(library_path(name)).unwrap();
        s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
    }
    TestSession::with_thread_name(s)
}

fn assert_prints(s: &mut Session, expr: &str, expected: &str) {
    let prog = format!("(with-output-to-string (s) {})", expr);
    let got = s.eval(&prog).unwrap_or_else(|e| panic!("eval failed: {e}\nexpr: {expr}"));
    let want = format!("\"{}\"", expected);
    assert_eq!(got, want, "\nexpr: {expr}");
}

// ── WRITE ─────────────────────────────────────────────────────────────
// translated from printer/write.lsp

#[test] fn ansi_write_2() { assert_prints(&mut fresh(), "(write 2 :stream s)", "2"); }
#[test] fn ansi_write_4() { assert_prints(&mut fresh(), "(write 4 :stream s)", "4"); }

// ── PRIN1 ─────────────────────────────────────────────────────────────
// translated from printer/prin1.lsp

#[test] fn ansi_prin1_3() { assert_prints(&mut fresh(), "(prin1 3 s)", "3"); }

// ── PRINC ─────────────────────────────────────────────────────────────
// translated from printer/princ.lsp

#[test] fn ansi_princ_3() { assert_prints(&mut fresh(), "(princ 3 s)", "3"); }

// ── PPRINT ────────────────────────────────────────────────────────────
// translated from printer/pprint.lsp

#[test]
fn ansi_pprint_3() {
    let mut s = fresh();
    let got = s.eval("(with-output-to-string (s) (pprint 3 s))").unwrap();
    assert_eq!(got, "\"\n3\"");
}

// ── PPRINT-FILL ───────────────────────────────────────────────────────
// translated from printer/pprint-fill.lsp

fn pprint_fill_test(s: &mut Session, args: &str, expected: &str) {
    let prog = format!(
        "(with-output-to-string (s) (pprint-fill s {}))", args);
    let got = s.eval(&prog).unwrap_or_else(|e| panic!("eval failed: {e}"));
    let want = format!("\"{}\"", expected);
    assert_eq!(got, want, "\nargs: {args}");
}

#[test] fn ansi_pprint_fill_3() { pprint_fill_test(&mut fresh(), "'(a)",         "(A)"); }
#[test] fn ansi_pprint_fill_4() { pprint_fill_test(&mut fresh(), "'(a) t",       "(A)"); }
#[test] fn ansi_pprint_fill_5() { pprint_fill_test(&mut fresh(), "'(a) nil",     "A"); }
#[test] fn ansi_pprint_fill_6() { pprint_fill_test(&mut fresh(), "'(1 2 3 4 5)", "(1 2 3 4 5)"); }

// ── PPRINT-LINEAR ─────────────────────────────────────────────────────
// translated from printer/pprint-linear.lsp

fn pprint_linear_test(s: &mut Session, args: &str, expected: &str) {
    let prog = format!(
        "(with-output-to-string (s) (pprint-linear s {}))", args);
    let got = s.eval(&prog).unwrap_or_else(|e| panic!("eval failed: {e}"));
    let want = format!("\"{}\"", expected);
    assert_eq!(got, want, "\nargs: {args}");
}

#[test] fn ansi_pprint_linear_3() { pprint_linear_test(&mut fresh(), "'(a)",         "(A)"); }
#[test] fn ansi_pprint_linear_4() { pprint_linear_test(&mut fresh(), "'(a) t",       "(A)"); }
#[test] fn ansi_pprint_linear_5() { pprint_linear_test(&mut fresh(), "'(a) nil",     "A"); }
#[test] fn ansi_pprint_linear_6() { pprint_linear_test(&mut fresh(), "'(1 2 3 4 5)", "(1 2 3 4 5)"); }

// ── Margin-wrap (PPRINT-LINEAR.12 / PPRINT-FILL.12) ───────────────────
//
// Dietz tests verify that with tight *print-right-margin* the output
// has enough newlines. In NCL we use setq+save/restore (no dynamic
// binding). Marked ignored: pprint-linear allocates closures that hit
// NCL's static area limit — needs NCL-level fix.

#[test]
#[ignore = "pprint-linear closure allocation hits NCL static-area limit — needs NCL-side fix (bigger static area or heap-allocated closures)"]
fn ansi_pprint_linear_12_margin_wrap_via_setq() {
    let mut s = fresh();
    let prog = r#"
      (let ((saved *print-right-margin*))
        (setq *print-right-margin* 5)
        (let ((result (with-output-to-string (out)
                        (pprint-linear out '(a b c d)))))
          (setq *print-right-margin* saved)
          result))"#;
    let got = s.eval(prog).unwrap();
    let unquoted = got.trim_matches('"');
    let newlines = unquoted.matches("\\n").count();
    assert!(newlines >= 1, "expected ≥1 newline, got {newlines}: {got}");
}

// ── Pin down the absence of dynamic binding ───────────────────────────
// This isn't an XP test per se — it documents a load-bearing assumption
// the XP port has to work around. If NCL grows dynamic binding, this
// test starts failing and we know to revisit the saved/restored
// formatter-fn / *string* dance.

#[test]
fn ncl_let_is_lexical_even_for_defvar() {
    let mut s = fresh();
    let r = s.eval(r#"
        (progn
          (defvar *test-x* 42)
          (defun read-test-x () *test-x*)
          (let ((*test-x* 99)) (read-test-x)))"#).unwrap();
    // The function still sees the global (42), not the let-binding (99).
    assert_eq!(r, "42",
        "NCL gained dynamic binding! Revisit XP's setq-based shims for *string* etc.");
}

