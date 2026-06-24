//! Tests for DECLARE SPECIAL / dynamic variable binding.
//!
//! Covers:
//!   - defparameter proclaims a symbol globally special
//!   - (let ((*var* new)) ...) dynamically rebinds the value cell
//!   - Inner functions see the rebound value (dynamic scope)
//!   - On let exit the old value is restored
//!   - (declare (special x)) in a let body makes x dynamically scoped
//!   - (proclaim '(special *x*)) at top level proclaims special
//!   - (locally (declare ...) body) strips the declare
//!   - Nested dynamic bindings restore in LIFO order

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn sess() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

// ── defparameter makes the variable globally special ──────────────────────

#[test]
fn defparameter_establishes_global_special() {
    let mut s = sess();
    assert_eq!(
        s.eval(
            "(defparameter *counter* 0)
             (defun bump () (setq *counter* (+ *counter* 1)))
             (bump) (bump)
             *counter*"
        )
        .unwrap(),
        "2"
    );
}

// ── let rebinds a special variable for the duration of the body ───────────

#[test]
fn let_rebinds_defparameter_special() {
    let mut s = sess();
    assert_eq!(
        s.eval(
            "(defparameter *x* 10)
             (let ((*x* 99))
               *x*)"
        )
        .unwrap(),
        "99"
    );
}

#[test]
fn let_restores_special_after_body() {
    let mut s = sess();
    s.eval("(defparameter *x* 10)").unwrap();
    s.eval("(let ((*x* 99)) *x*)").unwrap();
    assert_eq!(s.eval("*x*").unwrap(), "10");
}

// ── inner function sees the dynamically rebound value (dynamic scope) ─────

#[test]
fn dynamic_scope_visible_to_called_function() {
    let mut s = sess();
    s.eval(
        "(defparameter *greeting* \"hello\")
         (defun get-greeting () *greeting*)"
    )
    .unwrap();
    assert_eq!(
        s.eval("(let ((*greeting* \"hi\")) (get-greeting))").unwrap(),
        "\"hi\""
    );
}

#[test]
fn dynamic_scope_restored_after_let() {
    let mut s = sess();
    s.eval(
        "(defparameter *greeting* \"hello\")
         (defun get-greeting () *greeting*)"
    )
    .unwrap();
    s.eval("(let ((*greeting* \"hi\")) nil)").unwrap();
    assert_eq!(s.eval("(get-greeting)").unwrap(), "\"hello\"");
}

// ── setq inside dynamic let writes through to the value cell ─────────────

#[test]
fn setq_inside_dynamic_let_updates_value_cell() {
    let mut s = sess();
    s.eval("(defparameter *x* 1)").unwrap();
    assert_eq!(
        s.eval("(let ((*x* 5)) (setq *x* 42) *x*)").unwrap(),
        "42"
    );
}

#[test]
fn setq_inside_dynamic_let_does_not_affect_outer() {
    let mut s = sess();
    s.eval("(defparameter *x* 1)").unwrap();
    s.eval("(let ((*x* 5)) (setq *x* 42))").unwrap();
    assert_eq!(s.eval("*x*").unwrap(), "1");
}

// ── nested dynamic bindings ───────────────────────────────────────────────

#[test]
fn nested_let_rebinds_stack() {
    let mut s = sess();
    s.eval("(defparameter *depth* 0)").unwrap();
    assert_eq!(
        s.eval("(let ((*depth* 1)) (let ((*depth* 2)) *depth*))").unwrap(),
        "2"
    );
}

#[test]
fn nested_let_restores_outer_rebind() {
    let mut s = sess();
    s.eval("(defparameter *depth* 0)").unwrap();
    assert_eq!(
        s.eval("(let ((*depth* 1)) (let ((*depth* 2)) nil) *depth*)").unwrap(),
        "1"
    );
}

// ── mixed lexical and special bindings in one let ─────────────────────────

#[test]
fn mixed_lexical_and_special_in_one_let() {
    let mut s = sess();
    s.eval("(defparameter *sp* 100)").unwrap();
    assert_eq!(
        s.eval("(let ((lex 1) (*sp* 200) (lex2 3)) (+ lex lex2 *sp*))").unwrap(),
        "204"
    );
}

#[test]
fn mixed_let_restores_special_leaves_lexical() {
    let mut s = sess();
    s.eval("(defparameter *sp* 100)").unwrap();
    s.eval("(let ((lex 1) (*sp* 200) (lex2 3)) nil)").unwrap();
    assert_eq!(s.eval("*sp*").unwrap(), "100");
}

// ── (declare (special ...)) inside a let ─────────────────────────────────

#[test]
fn declare_special_in_let_makes_binding_dynamic() {
    let mut s = sess();
    // x is NOT globally special; the declare makes it dynamic within
    // this let. An inner function reading x uses the value cell.
    s.eval(
        "(defvar x 0)
         (defun read-x () x)"
    )
    .unwrap();
    assert_eq!(
        s.eval(
            "(let ((x 77))
               (declare (special x))
               (read-x))"
        )
        .unwrap(),
        "77"
    );
}

// ── (locally (declare ...) body) strips the declare cleanly ──────────────

#[test]
fn locally_with_declare_evaluates_body() {
    let mut s = sess();
    assert_eq!(
        s.eval("(locally (declare (special ignored-here)) (+ 1 2))").unwrap(),
        "3"
    );
}

// ── (defvar name) without initial value proclaims special ────────────────

#[test]
fn defvar_proclaims_special() {
    let mut s = sess();
    // defvar with value marks as special; let should rebind via value cell
    s.eval("(defvar *bare* 0)").unwrap();
    assert_eq!(
        s.eval("(let ((*bare* 99)) *bare*)").unwrap(),
        "99"
    );
    assert_eq!(s.eval("*bare*").unwrap(), "0");
}
