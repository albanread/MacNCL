//! Tests for UNWIND-PROTECT.
//!
//! Covers:
//!   - Normal exit: cleanup runs, protected value is returned
//!   - Cleanup return value is discarded (protected value wins)
//!   - Nested unwind-protect: inner and outer cleanup both run
//!   - setq inside protected: dynamic binding + cleanup
//!   - Empty cleanup: no body forms in cleanup is legal
//!   - Multiple cleanup forms: all run in order

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn sess() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

// ── Normal path: cleanup runs and protected value is returned ─────────────

#[test]
fn unwind_protect_returns_protected_value() {
    let mut s = sess();
    assert_eq!(
        s.eval("(unwind-protect 42 nil)").unwrap(),
        "42"
    );
}

#[test]
fn unwind_protect_cleanup_runs_on_normal_exit() {
    let mut s = sess();
    s.eval("(defparameter *ran* nil)").unwrap();
    s.eval("(unwind-protect 1 (setq *ran* t))").unwrap();
    assert_eq!(s.eval("*ran*").unwrap(), "T");
}

// ── Cleanup return value is discarded ─────────────────────────────────────

#[test]
fn cleanup_return_value_discarded() {
    let mut s = sess();
    // The cleanup returns 99, but unwind-protect should return
    // the protected form's value (42).
    assert_eq!(
        s.eval("(unwind-protect 42 99)").unwrap(),
        "42"
    );
}

// ── Multiple cleanup forms ────────────────────────────────────────────────

#[test]
fn multiple_cleanup_forms_all_run() {
    let mut s = sess();
    s.eval("(defparameter *log* nil)").unwrap();
    s.eval(
        "(unwind-protect
           (progn 1)
           (setq *log* (cons 1 *log*))
           (setq *log* (cons 2 *log*)))"
    ).unwrap();
    // Both cleanup forms ran; log is (2 1) — last pushed first
    assert_eq!(s.eval("*log*").unwrap(), "(2 1)");
}

// ── Nested unwind-protect ─────────────────────────────────────────────────

#[test]
fn nested_unwind_protect_both_cleanups_run() {
    let mut s = sess();
    s.eval("(defparameter *inner-ran* nil)").unwrap();
    s.eval("(defparameter *outer-ran* nil)").unwrap();
    s.eval(
        "(unwind-protect
           (unwind-protect
             42
             (setq *inner-ran* t))
           (setq *outer-ran* t))"
    ).unwrap();
    assert_eq!(s.eval("*inner-ran*").unwrap(), "T");
    assert_eq!(s.eval("*outer-ran*").unwrap(), "T");
}

#[test]
fn nested_unwind_protect_returns_innermost_value() {
    let mut s = sess();
    assert_eq!(
        s.eval(
            "(unwind-protect
               (unwind-protect
                 7
                 nil)
               nil)"
        ).unwrap(),
        "7"
    );
}

// ── Dynamic binding + unwind-protect ─────────────────────────────────────

#[test]
fn dynamic_bind_restored_after_unwind_protect() {
    let mut s = sess();
    s.eval("(defparameter *x* 1)").unwrap();
    // Inside the let, *x* is dynamically rebound to 99.
    // The cleanup sees 99 (dynamic scope is still active during cleanup).
    // After the whole form, *x* is restored to 1.
    s.eval(
        "(let ((*x* 99))
           (unwind-protect
             *x*
             nil))"
    ).unwrap();
    // After let exits, *x* is back to 1.
    assert_eq!(s.eval("*x*").unwrap(), "1");
}

#[test]
fn cleanup_sees_dynamic_binding() {
    let mut s = sess();
    s.eval("(defparameter *x* 1)").unwrap();
    s.eval("(defparameter *seen* nil)").unwrap();
    s.eval(
        "(let ((*x* 42))
           (unwind-protect
             nil
             (setq *seen* *x*)))"
    ).unwrap();
    // Cleanup ran while *x* was still dynamically bound to 42.
    assert_eq!(s.eval("*seen*").unwrap(), "42");
}

// ── Empty cleanup ─────────────────────────────────────────────────────────

#[test]
fn unwind_protect_no_cleanup_forms() {
    let mut s = sess();
    // (unwind-protect form) with no cleanup is legal; returns form's value.
    assert_eq!(
        s.eval("(unwind-protect 77)").unwrap(),
        "77"
    );
}
