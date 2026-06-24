//! Regression: nested-lambda closure capture must thread the
//! captured variable through *every* intermediate lambda, not
//! just the immediate parent.
//!
//! The bug was a one-level reconcile inside `lower_lambda` after
//! a nested lambda finished lowering. `find_or_capture` walked
//! the clone-chain to grab `x` from the outermost defun, adding
//! capture entries at each clone level along the way. But the
//! reconcile back-copied entries only from the IMMEDIATE
//! `inner_env.capture_parent` — a separate clone of the
//! enclosing lambda's env that was never touched by the inner
//! walk. Each intermediate lambda's "real" env therefore lost
//! the capture, emitted an empty captures list, allocated an
//! empty env at runtime, and the next-level-down lambda crashed
//! reading off the end of that env.
//!
//! Visible symptom: 3 lambdas deep around a defun parameter
//! worked; 4 lambdas deep segfaulted. Auto-block adds one
//! implicit lambda to every defun body, so all real CLOS-using
//! code with a 3-deep capture pattern hit this path. The fix in
//! `src/ncl-compiler/src/lower.rs` walks the clone-chain
//! recursively during reconcile.
//!
//! These tests pin the depth-1 through depth-5 cases.

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn fresh() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

#[test]
fn capture_through_one_lambda_layer() {
    // Baseline. Always worked, kept here for completeness.
    let mut s = fresh();
    assert_eq!(
        s.eval("(defun mk (x) (lambda (y) (+ x y))) (funcall (mk 10) 5)").unwrap(),
        "15",
    );
}

#[test]
fn capture_through_two_lambda_layers() {
    let mut s = fresh();
    // mk → outer → inner; inner uses x from mk.
    assert_eq!(
        s.eval(
            "(defun mk (x) (lambda () (lambda (y) (+ x y))))
             (funcall (funcall (mk 10)) 5)"
        )
        .unwrap(),
        "15",
    );
}

#[test]
fn capture_through_three_lambda_layers() {
    let mut s = fresh();
    // Previously the boundary case — with the auto-block
    // re-enabled this is now a routine 4-level reach (auto-block
    // adds its own implicit lambda).
    assert_eq!(
        s.eval(
            "(defun mk (x) (lambda () (lambda () (lambda (y) (+ x y)))))
             (funcall (funcall (funcall (mk 10))) 5)"
        )
        .unwrap(),
        "15",
    );
}

#[test]
fn capture_through_four_lambda_layers() {
    let mut s = fresh();
    // 5-deep chain (defun + 4 lambdas). Exercises three rounds
    // of recursive reconcile.
    assert_eq!(
        s.eval(
            "(defun mk (x) (lambda () (lambda () (lambda () (lambda (y) (+ x y))))))
             (funcall (funcall (funcall (funcall (mk 10)))) 5)"
        )
        .unwrap(),
        "15",
    );
}

#[test]
fn capture_through_native_block_chain() {
    let mut s = fresh();
    // The exact shape auto-block synthesises: a (block name body)
    // around a lambda-returning-lambda body. Without the fix this
    // segfaulted because the BLOCK macro expands to a
    // `(%native-block 'name (lambda () body))`, adding one more
    // lambda level between the defun's params and the inner
    // captures.
    assert_eq!(
        s.eval(
            "(defun mk (x)
               (block mk
                 (lambda ()
                   (lambda (y) (+ x y)))))
             (funcall (funcall (mk 10)) 5)"
        )
        .unwrap(),
        "15",
    );
}

#[test]
fn capture_multiple_vars_through_deep_chain() {
    let mut s = fresh();
    // Two outer-scope vars captured by an inner lambda at depth 3.
    // The recursive reconcile must thread BOTH `x` and `y` through
    // every level.
    assert_eq!(
        s.eval(
            "(defun mk (x y)
               (lambda ()
                 (lambda ()
                   (lambda (z) (+ x y z)))))
             (funcall (funcall (funcall (mk 10 100))) 1)"
        )
        .unwrap(),
        "111",
    );
}

#[test]
fn return_from_inside_defun_works_without_explicit_block() {
    // Auto-block payoff: this is the canonical CL idiom that
    // previously needed an explicit (block name …) wrap. Used
    // throughout the Corman stdlib.
    let mut s = fresh();
    assert_eq!(
        s.eval(
            "(defun first-positive (xs)
               (dolist (x xs)
                 (when (> x 0) (return-from first-positive x)))
               nil)
             (list (first-positive '(-1 -2 3 -4 5))
                   (first-positive '(-1 -2 -3))
                   (first-positive nil))"
        )
        .unwrap(),
        "(3 nil nil)",
    );
}

#[test]
fn return_from_inside_defun_with_args() {
    // Variant: the function takes args and the early return
    // reaches them.
    let mut s = fresh();
    assert_eq!(
        s.eval(
            "(defun classify (n)
               (when (zerop n) (return-from classify :zero))
               (when (< n 0)   (return-from classify :negative))
               :positive)
             (list (classify 0) (classify -5) (classify 7))"
        )
        .unwrap(),
        "(:ZERO :NEGATIVE :POSITIVE)",
    );
}
