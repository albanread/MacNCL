//! Coverage for extensions to `Lisp/Library/types.lisp`:
//! `ccase`, `deftype`, and the extended `typep` wrapper.

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

fn load(s: &mut Session, name: &str) {
    let path = library_path(name);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
}

fn fresh_session() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    // types.lisp depends on symbols.lisp for destructuring-bind (used
    // inside the deftype expander).
    load(&mut s, "symbols.lisp");
    load(&mut s, "types.lisp");
    TestSession::with_thread_name(s)
}

// ── nth-value (lives in lists.lisp but tested here for coverage) ──────

fn fresh_session_with_lists() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    load(&mut s, "symbols.lisp");   // destructuring-bind
    load(&mut s, "trees.lisp");     // dependencies of lists.lisp
    load(&mut s, "lists.lisp");
    TestSession::with_thread_name(s)
}

#[test]
fn nth_value_zero_is_first() {
    let mut s = fresh_session_with_lists();
    // floor returns (quotient remainder) as two values.
    assert_eq!(s.eval("(nth-value 0 (floor 17 5))").unwrap(), "3");
}

#[test]
fn nth_value_one_is_second() {
    let mut s = fresh_session_with_lists();
    // (values 10 20) returns two values; nth-value 1 should give 20.
    assert_eq!(s.eval("(nth-value 1 (values 10 20))").unwrap(), "20");
}

#[test]
fn nth_value_evaluates_n_once() {
    let mut s = fresh_session_with_lists();
    // Verify N is evaluated only once (no double-evaluation side effect).
    let prog = "
        (defparameter *n-evals* 0)
        (defun get-index ()
          (setq *n-evals* (+ *n-evals* 1))
          1)
        (nth-value (get-index) (values 'a 'b 'c))
        *n-evals*
    ";
    assert_eq!(s.eval(prog).unwrap(), "1");
}

#[test]
fn nth_value_form_evaluated_once() {
    let mut s = fresh_session_with_lists();
    let prog = "
        (defparameter *form-evals* 0)
        (defun mk-values ()
          (setq *form-evals* (+ *form-evals* 1))
          (values 10 20 30))
        (nth-value 2 (mk-values))
        *form-evals*
    ";
    assert_eq!(s.eval(prog).unwrap(), "1");
}

// ── extended typep sanity checks ──────────────────────────────────────

#[test]
fn typep_original_saved_correctly() {
    let mut s = fresh_session();
    // *%original-typep%* should remain bound to the Rust shim even
    // after (defun typep …) redefines the symbol.
    assert_eq!(
        s.eval("(funcall *%original-typep%* 5 'integer)").unwrap(),
        "T"
    );
}

#[test]
fn symbolp_works_after_typep_redefined() {
    let mut s = fresh_session();
    // core.lisp defines (defun symbolp (x) (typep x 'symbol)).
    // After types.lisp redefines typep, symbolp must not recurse.
    // The fix: %new-typep uses (consp type) not (symbolp type) for dispatch.
    assert_eq!(s.eval("(symbolp 'foo)").unwrap(), "T");
    let r = s.eval("(symbolp 42)").unwrap();
    assert!(r.eq_ignore_ascii_case("nil"), "got: {r}");
}

#[test]
fn typep_dispatches_through_new_wrapper() {
    let mut s = fresh_session();
    // Simple symbol type — delegated to Rust shim via %new-typep.
    assert_eq!(s.eval("(typep 5 'integer)").unwrap(), "T");
    let r = s.eval("(typep 5 'string)").unwrap();
    assert!(r.eq_ignore_ascii_case("nil"), "got: {r}");
}

#[test]
fn typep_compound_integer_range() {
    let mut s = fresh_session();
    assert_eq!(s.eval("(typep 5 '(integer 0 10))").unwrap(), "T");
    let r = s.eval("(typep 11 '(integer 0 10))").unwrap();
    assert!(r.eq_ignore_ascii_case("nil"), "got: {r}");
}

#[test]
fn typep_compound_or() {
    let mut s = fresh_session();
    assert_eq!(s.eval("(typep 5 '(or integer string))").unwrap(), "T");
    assert_eq!(s.eval("(typep \"hi\" '(or integer string))").unwrap(), "T");
    let r = s.eval("(typep 'foo '(or integer string))").unwrap();
    assert!(r.eq_ignore_ascii_case("nil"), "got: {r}");
}

// ── ccase ──────────────────────────────────────────────────────────────

#[test]
fn ccase_matches_first_clause() {
    let mut s = fresh_session();
    let prog = "
        (ccase 1
          (1 'one)
          (2 'two)
          (3 'three))
    ";
    assert_eq!(s.eval(prog).unwrap(), "ONE");
}

#[test]
fn ccase_matches_list_key() {
    let mut s = fresh_session();
    // A clause key can be a list of values.
    let prog = "
        (ccase 'b
          ((a b c) 'letter)
          (t       'other))
    ";
    assert_eq!(s.eval(prog).unwrap(), "LETTER");
}

#[test]
fn ccase_otherwise_clause() {
    let mut s = fresh_session();
    let prog = "
        (ccase 99
          (1 'one)
          (otherwise 'other))
    ";
    assert_eq!(s.eval(prog).unwrap(), "OTHER");
}

// ── deftype ────────────────────────────────────────────────────────────

#[test]
fn deftype_simple_alias() {
    let mut s = fresh_session();
    // A no-argument deftype that aliases an existing type.
    let prog = "
        (deftype natural () '(integer 0 *))
        (list (typep 5   'natural)
              (typep -1  'natural)
              (typep 1.0 'natural))
    ";
    // NCL prints nil in lowercase inside lists.
    let r = s.eval(prog).unwrap();
    let r = r.replace("NIL", "nil");
    assert_eq!(r, "(T nil nil)");
}

#[test]
fn deftype_with_parameter() {
    let mut s = fresh_session();
    // (deftype bounded-integer (lo hi) `(integer ,lo ,hi))
    let prog = "
        (deftype bounded-integer (lo hi)
          (list 'integer lo hi))
        (list (typep  5 '(bounded-integer 0 10))
              (typep 11 '(bounded-integer 0 10))
              (typep  0 '(bounded-integer 0 10)))
    ";
    let r = s.eval(prog).unwrap();
    let r = r.replace("NIL", "nil");
    assert_eq!(r, "(T nil T)");
}

#[test]
fn deftype_returns_name() {
    let mut s = fresh_session();
    // deftype should return the type name.
    assert_eq!(
        s.eval("(deftype my-type () 'integer)").unwrap(),
        "MY-TYPE"
    );
}

#[test]
fn deftype_with_optional_param() {
    let mut s = fresh_session();
    // (deftype maybe-bounded (&optional (hi 100)) `(integer 0 ,hi))
    let prog = "
        (deftype maybe-bounded (&optional (hi 100))
          (list 'integer 0 hi))
        (list (typep  50 'maybe-bounded)
              (typep 150 'maybe-bounded)
              (typep  99 '(maybe-bounded 99))
              (typep 100 '(maybe-bounded 99)))
    ";
    let r = s.eval(prog).unwrap();
    let r = r.replace("NIL", "nil");
    assert_eq!(r, "(T nil T nil)");
}
