//! Coverage for `Lisp/Library/places.lisp` — the standard
//! `(setf accessor)` declarations and the short-form DEFSETF
//! macro.
//!
//! There are two pieces that have to line up:
//!
//!   1. `(defun (setf NAME) (val args…) …)` is accepted by the
//!      compiler. The function-name slot can now be either a
//!      symbol or a `(setf SYMBOL)` cons; the latter mangles to
//!      `%SETF-SYMBOL` for the generic-setf-fallback to find.
//!
//!   2. Library/places.lisp registers the standard accessors
//!      (FIRST, REST, NTH, the CXR family, LAST) so user code
//!      can write `(setf (first xs) 99)` instead of
//!      `(setf (car xs) 99)`.
//!
//!   3. DEFSETF lets a user register the inverse of any access
//!      function: `(defsetf place setter)` arranges for
//!      `(setf (place args…) val)` to call `(setter args… val)`.

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

fn fresh_session_with_places() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    let path = library_path("places.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/places.lisp loads");
    TestSession::with_thread_name(s)
}

// ── (defun (setf NAME) …) — the compiler-side enabler ─────────────────

#[test]
fn defun_setf_name_compiles_and_dispatches() {
    // No Library load — just verify the compiler accepts the
    // `(setf NAME)` shape on its own. User defines an accessor +
    // its setter; setf-fallback finds the setter.
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    let prog = "
        (defun my-foo (obj) (car obj))
        (defun (setf my-foo) (val obj) (setf (car obj) val))
        (defparameter *xs* (list 1 2 3))
        (setf (my-foo *xs*) 99)
        *xs*
    ";
    assert_eq!(s.eval(prog).unwrap(), "(99 2 3)");
}

#[test]
fn defun_setf_supports_multiple_args() {
    // Verify the compiler accepts `(setf NAME)` defuns with more
    // than one place-arg. Stays inside core (no places.lisp
    // dependency) by using only setf-(car) under the hood.
    let mut s = TestSession::with_thread_name(
        Session::with_stdlib().expect("session boots"),
    );
    s.activate();
    let prog = "
        ;; Two-arg accessor: head of a numbered ALIST entry.
        (defun nth-pair (n alist) (nth n alist))
        (defun (setf nth-pair) (val n alist)
          (setf (car (nthcdr n alist)) val))
        (defparameter *a* (list 'a 'b 'c 'd))
        (setf (nth-pair 1 *a*) :HIT)
        *a*
    ";
    assert_eq!(s.eval(prog).unwrap(), "(A :HIT C D)");
}

// ── Named cons accessors ──────────────────────────────────────────────

#[test]
fn setf_first_through_fourth_each_set_the_named_position() {
    let mut s = fresh_session_with_places();
    let prog = "
        (let ((xs (list 1 2 3 4 5)))
          (setf (first  xs) 'a)
          (setf (second xs) 'b)
          (setf (third  xs) 'c)
          (setf (fourth xs) 'd)
          xs)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(A B C D 5)");
}

#[test]
fn setf_rest_replaces_the_tail() {
    let mut s = fresh_session_with_places();
    assert_eq!(
        s.eval(
            "(let ((xs (list 1 2 3)))
               (setf (rest xs) '(20 30 40))
               xs)"
        )
        .unwrap(),
        "(1 20 30 40)",
    );
}

#[test]
fn setf_cxr_family_walks_through_nested_lists() {
    let mut s = fresh_session_with_places();
    // (setf (caddr xs) v) replaces the third element. Same as
    // (setf (third xs) v) but through the cdr-chain notation.
    let prog = "
        (let ((xs (list 1 2 3 4 5)))
          (setf (caddr xs) :third)
          xs)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 2 :THIRD 4 5)");
    // (setf (cadar xs) v) — second element of the first sublist.
    let prog = "
        (let ((xs (list (list 'a 'b 'c) (list 'd 'e 'f))))
          (setf (cadar xs) :Z)
          xs)
    ";
    assert_eq!(s.eval(prog).unwrap(), "((A :Z C) (D E F))");
}

// ── (setf nth N list) ────────────────────────────────────────────────

#[test]
fn setf_nth_replaces_indexed_element() {
    let mut s = fresh_session_with_places();
    assert_eq!(
        s.eval(
            "(let ((xs (list 'a 'b 'c 'd 'e)))
               (setf (nth 0 xs) :zero)
               (setf (nth 2 xs) :two)
               (setf (nth 4 xs) :four)
               xs)"
        )
        .unwrap(),
        "(:ZERO B :TWO D :FOUR)",
    );
}

// ── DEFSETF (short form) ─────────────────────────────────────────────

#[test]
fn defsetf_short_form_installs_inverse() {
    let mut s = fresh_session_with_places();
    let prog = "
        ;; A toy place: middle of a 3-list.
        (defun mid (xs) (second xs))
        (defun set-mid (xs val) (setf (second xs) val))
        (defsetf mid set-mid)

        (defparameter *xs* (list 'a 'b 'c))
        (setf (mid *xs*) :HIT)
        *xs*
    ";
    assert_eq!(s.eval(prog).unwrap(), "(A :HIT C)");
}

#[test]
fn defsetf_passes_multiple_place_args() {
    let mut s = fresh_session_with_places();
    let prog = "
        ;; Two-arg place. (cell row grid) reads the row-th element;
        ;; setter takes both args + val and stores.
        (defun cell (row grid) (nth row grid))
        (defun set-cell (row grid val) (setf (nth row grid) val))
        (defsetf cell set-cell)

        (defparameter *g* (list 'a 'b 'c 'd))
        (setf (cell 2 *g*) :TWO)
        *g*
    ";
    assert_eq!(s.eval(prog).unwrap(), "(A B :TWO D)");
}

// ── PSETQ ────────────────────────────────────────────────────────────

#[test]
fn psetq_swaps_two_variables() {
    let mut s = fresh_session_with_places();
    // Classic swap idiom: both old values are captured before assignment.
    let prog = "
        (let ((a 1) (b 2))
          (psetq a b  b a)
          (list a b))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(2 1)");
}

#[test]
fn psetq_evaluates_rhs_before_assignment() {
    let mut s = fresh_session_with_places();
    // a and b both start at 10; after (psetq a (+ a 1) b (+ a 2)),
    // a=11 and b=12 (using OLD value of a=10 for both rhs expressions).
    let prog = "
        (let ((a 10) (b 10))
          (psetq a (+ a 1)
                 b (+ a 2))
          (list a b))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(11 12)");
}

#[test]
fn psetq_returns_nil() {
    let mut s = fresh_session_with_places();
    // psetq returns nil (printed lowercase in NCL)
    let out = s.eval("(let ((x 1)) (psetq x 2))").unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

// ── PSETF ────────────────────────────────────────────────────────────

#[test]
fn psetf_swaps_via_setf_places() {
    let mut s = fresh_session_with_places();
    let prog = "
        (let ((xs (list 1 2 3)))
          (psetf (first xs) (second xs)
                 (second xs) (first xs))
          xs)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(2 1 3)");
}

// ── GET-SETF-EXPANSION ───────────────────────────────────────────────

#[test]
fn get_setf_expansion_bare_symbol() {
    let mut s = fresh_session_with_places();
    // For a bare symbol, vars and vals are nil, stores has one element,
    // writer is a setq, reader is the symbol itself.
    let prog = "
        (multiple-value-bind (vars vals stores writer reader)
            (get-setf-expansion 'x)
          (list (null vars) (null vals) (= 1 (length stores))
                (car (cdr writer))    ; variable being set (x)
                reader))
    ";
    // vars=nil→T, vals=nil→T, stores has 1 element→T, writer assigns x→X, reader=X
    assert_eq!(s.eval(prog).unwrap(), "(T T T X X)");
}

#[test]
fn get_setf_expansion_accessor_form() {
    let mut s = fresh_session_with_places();
    // For (car xs): vars has 1 gensym, vals=(xs), stores has 1,
    // reader is (car <gensym>), writer is (setf (car <gensym>) <store>).
    let prog = "
        (multiple-value-bind (vars vals stores writer reader)
            (get-setf-expansion '(car xs))
          (list (= 1 (length vars))   ; one temp var
                (equal vals '(xs))    ; val is xs
                (= 1 (length stores)) ; one store var
                (car writer)          ; writer starts with setf
                (car reader)          ; reader starts with car
                ))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(T T T SETF CAR)");
}

// ── DEFINE-MODIFY-MACRO ──────────────────────────────────────────────

#[test]
fn define_modify_macro_basic() {
    let mut s = fresh_session_with_places();
    // APPENDF: (appendf place more) ≡ (setf place (append place more))
    let prog = "
        (define-modify-macro appendf (&rest more) append
          \"Append MORE to the list at PLACE.\")
        (defparameter *lst* '(1 2 3))
        (appendf *lst* '(4 5 6))
        *lst*
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 2 3 4 5 6)");
}

#[test]
fn define_modify_macro_with_optional_arg() {
    let mut s = fresh_session_with_places();
    // MULTF: multiply place by factor (default 2).
    let prog = "
        (define-modify-macro multf (&optional (factor 2)) *)
        (let ((x 5))
          (multf x 3)
          x)
    ";
    assert_eq!(s.eval(prog).unwrap(), "15");
}

#[test]
fn define_modify_macro_incf_like() {
    let mut s = fresh_session_with_places();
    // Re-implement INCF using define-modify-macro to verify the machinery.
    let prog = "
        (define-modify-macro my-incf (&optional (delta 1)) +)
        (let ((n 10))
          (my-incf n)
          (my-incf n 5)
          n)
    ";
    assert_eq!(s.eval(prog).unwrap(), "16");
}
