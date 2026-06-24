//! Coverage for `Lisp/Library/symbols.lisp` — symbol property lists,
//! standard control macros, and macro-writing utilities (with-gensyms,
//! once-only).

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

fn fresh_session_with_symbols() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    let path = library_path("symbols.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/symbols.lisp loads");
    TestSession::with_thread_name(s)
}

// ── Property lists ────────────────────────────────────────────────────

#[test]
fn get_and_setf_get_roundtrip() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (setf (get 'my-sym :color) 'red)
        (get 'my-sym :color)
    ";
    assert_eq!(s.eval(prog).unwrap(), "RED");
}

#[test]
fn remprop_removes_indicator() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (setf (get 'ps-sym :x) 42)
        (let ((found-before (get 'ps-sym :x 'missing))
              (removed      (remprop 'ps-sym :x))
              (found-after  (get 'ps-sym :x 'missing)))
          (list found-before removed found-after))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(42 T MISSING)");
}

#[test]
fn remprop_returns_nil_when_absent() {
    let mut s = fresh_session_with_symbols();
    let out = s.eval("(remprop 'absent-sym :nothing)").unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

#[test]
fn putprop_sets_property() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (putprop 'pp-sym 99 :score)
        (get 'pp-sym :score)
    ";
    assert_eq!(s.eval(prog).unwrap(), "99");
}

#[test]
fn symbol_plist_and_setf_symbol_plist() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (setf (symbol-plist 'sp-sym) '(:a 1 :b 2))
        (get 'sp-sym :b)
    ";
    assert_eq!(s.eval(prog).unwrap(), "2");
}

// ── prog1 / prog2 ─────────────────────────────────────────────────────

#[test]
fn prog1_returns_first_form_value() {
    let mut s = fresh_session_with_symbols();
    assert_eq!(
        s.eval("(prog1 (+ 1 2) (+ 3 4) (+ 5 6))").unwrap(),
        "3"
    );
}

#[test]
fn prog2_returns_second_form_value() {
    let mut s = fresh_session_with_symbols();
    assert_eq!(
        s.eval("(prog2 (+ 1 2) (+ 3 4) (+ 5 6))").unwrap(),
        "7"
    );
}

// ── copy-symbol ───────────────────────────────────────────────────────

#[test]
fn copy_symbol_creates_uninterned_copy() {
    let mut s = fresh_session_with_symbols();
    // NCL's make-symbol appends a counter to ensure uniqueness, so the
    // copy's name starts with "HELLO" but is not identical.  The copy
    // must be a different object from the interned symbol.
    let prog = r#"
        (let ((new (copy-symbol 'hello)))
          (list (symbolp new)
                (> (length (symbol-name new)) 0)
                (not (eq new 'hello))))
    "#;
    assert_eq!(s.eval(prog).unwrap(), "(T T T)");
}

#[test]
fn copy_symbol_with_props_copies_value() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (defparameter *orig* 42)
        (let ((c (copy-symbol '*orig* t)))
          (symbol-value c))
    ";
    assert_eq!(s.eval(prog).unwrap(), "42");
}

// ── assert ────────────────────────────────────────────────────────────

#[test]
fn assert_passes_when_true() {
    let mut s = fresh_session_with_symbols();
    // Should not signal an error; returns nil (the progn result)
    let out = s.eval("(assert (= 1 1))").unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

// ── check-type ────────────────────────────────────────────────────────

#[test]
fn check_type_passes_on_correct_type() {
    let mut s = fresh_session_with_symbols();
    let out = s.eval("(let ((x 5)) (check-type x integer))").unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

// ── remf ──────────────────────────────────────────────────────────────

#[test]
fn remf_removes_from_plist_place() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (let ((plist '(:a 1 :b 2 :c 3)))
          (remf plist :b)
          plist)
    ";
    // :b and its value 2 are gone; order of remaining entries preserved
    assert_eq!(s.eval(prog).unwrap(), "(:A 1 :C 3)");
}

#[test]
fn remf_returns_nil_when_absent() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (let ((plist '(:a 1)))
          (remf plist :z))
    ";
    let out = s.eval(prog).unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

// ── with-gensyms ──────────────────────────────────────────────────────

#[test]
fn with_gensyms_binds_fresh_symbols() {
    let mut s = fresh_session_with_symbols();
    // Each binding should be a fresh, uninterned symbol.
    let prog = "
        (with-gensyms (a b)
          (list (symbolp a)
                (symbolp b)
                (not (eq a b))
                (not (eq a 'a))))   ; definitely not the interned symbol A
    ";
    assert_eq!(s.eval(prog).unwrap(), "(T T T T)");
}

#[test]
fn with_gensyms_used_in_macro_definition() {
    let mut s = fresh_session_with_symbols();
    // Idiomatic usage: define a swap macro hygienically.
    let prog = "
        (defmacro swap! (a b)
          (with-gensyms (ta)
            `(let ((,ta ,a))
               (setf ,a ,b)
               (setf ,b ,ta))))
        (let ((x 1) (y 2))
          (swap! x y)
          (list x y))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(2 1)");
}

// ── once-only ─────────────────────────────────────────────────────────

#[test]
fn once_only_wraps_with_let_binding() {
    let mut s = fresh_session_with_symbols();
    // once-only should produce a (let ((G expr)) ...) wrapper so the
    // expression is evaluated exactly once regardless of use count.
    let prog = "
        (defmacro my-square (x)
          (once-only (x)
            `(* ,x ,x)))

        ;; Use an explicit counter to verify single evaluation.
        (let ((count 0))
          (my-square (progn (setq count (+ count 1)) 3))   ; incf equivalent
          count)
    ";
    assert_eq!(s.eval(prog).unwrap(), "1");
}

#[test]
fn once_only_correct_result_value() {
    let mut s = fresh_session_with_symbols();
    let prog = "
        (defmacro my-square (x)
          (once-only (x)
            `(* ,x ,x)))
        (my-square 5)
    ";
    assert_eq!(s.eval(prog).unwrap(), "25");
}

#[test]
fn once_only_multiple_vars() {
    let mut s = fresh_session_with_symbols();
    // Two arguments — each should be evaluated once.
    let prog = "
        (defmacro my-between (lo x hi)
          (once-only (lo x hi)
            `(and (<= ,lo ,x) (<= ,x ,hi))))
        (my-between 1 5 10)
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn once_only_side_effect_not_duplicated() {
    let mut s = fresh_session_with_symbols();
    // Counter updated by setq: wrap in a closure so we can observe
    // how many times the form is evaluated without needing incf/pop.
    let prog = "
        (defmacro my-square (x)
          (once-only (x)
            `(* ,x ,x)))

        ;; Track evaluation count with a global counter.
        (defparameter *eval-count* 0)
        (defun bump-and-return (n)
          (setq *eval-count* (+ *eval-count* 1))
          n)
        (my-square (bump-and-return 7))   ; should call bump once, return 49
        *eval-count*
    ";
    // bump-and-return called exactly once
    assert_eq!(s.eval(prog).unwrap(), "1");
}
