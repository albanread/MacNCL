//! Coverage for the variadic shapes of MAPCAR / MAPC / EVERY /
//! SOME — the four mapping operators in core.lisp.
//!
//! Each operator walks N lists in parallel and stops at the shortest
//! input. EVERY and SOME short-circuit on the first negative /
//! positive answer respectively. MAPC returns its first list
//! (called for effect); the others build a result list.
//!
//! Before this slice every member of the family accepted exactly
//! one input list; this file pins down the multi-list behaviour now
//! that `do` / closure-capture / auto-block all land cleanly.

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn fresh() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

// ── MAPCAR ────────────────────────────────────────────────────────────

#[test]
fn mapcar_unary_is_unchanged() {
    let mut s = fresh();
    assert_eq!(
        s.eval("(mapcar (lambda (x) (* x x)) '(1 2 3))").unwrap(),
        "(1 4 9)",
    );
    assert_eq!(s.eval("(mapcar #'car '((1 a) (2 b) (3 c)))").unwrap(), "(1 2 3)");
}

#[test]
fn mapcar_binary_zips_two_lists() {
    let mut s = fresh();
    // The classic zip-with-sum.
    assert_eq!(
        s.eval("(mapcar (lambda (a b) (+ a b)) '(1 2 3) '(10 20 30))")
            .unwrap(),
        "(11 22 33)",
    );
    // Stops at the shortest input (here, the second list).
    assert_eq!(
        s.eval("(mapcar (lambda (a b) (list a b)) '(1 2 3 4) '(a b))")
            .unwrap(),
        "((1 A) (2 B))",
    );
}

#[test]
fn mapcar_triadic_zips_three_lists() {
    let mut s = fresh();
    // FN receives one element per list.
    assert_eq!(
        s.eval(
            "(mapcar (lambda (a b c) (list a b c))
                     '(1 2 3) '(a b c) '(:x :y :z))"
        )
        .unwrap(),
        "((1 A :X) (2 B :Y) (3 C :Z))",
    );
    // Empty list in the mix → empty result.
    assert_eq!(
        s.eval(
            "(mapcar (lambda (a b c) (list a b c))
                     '(1 2 3) '() '(:x :y :z))"
        )
        .unwrap(),
        "nil",
    );
}

// ── MAPC (called for effect, returns first list) ─────────────────────

#[test]
fn mapc_unary_returns_input_list() {
    let mut s = fresh();
    let prog = "
        (defparameter *side* nil)
        (mapc (lambda (x) (push x *side*)) '(a b c))
    ";
    // mapc returns the first input list, unchanged.
    assert_eq!(s.eval(prog).unwrap(), "(A B C)");
    // The push happened in walk order; check via reverse.
    assert_eq!(s.eval("(reverse *side*)").unwrap(), "(A B C)");
}

#[test]
fn mapc_variadic_walks_parallel_lists_returns_first() {
    let mut s = fresh();
    let prog = "
        (defparameter *seen* nil)
        (defparameter *r*
          (mapc (lambda (a b) (push (cons a b) *seen*))
                '(1 2 3) '(:x :y :z)))
        (list *r* (reverse *seen*))
    ";
    assert_eq!(
        s.eval(prog).unwrap(),
        "((1 2 3) ((1 . :X) (2 . :Y) (3 . :Z)))",
    );
}

// ── EVERY ────────────────────────────────────────────────────────────

#[test]
fn every_unary_returns_t_for_all_passing() {
    let mut s = fresh();
    assert_eq!(
        s.eval("(every (lambda (x) (> x 0)) '(1 2 3))").unwrap(),
        "T",
    );
    assert_eq!(
        s.eval("(every (lambda (x) (> x 0)) '(1 -2 3))").unwrap(),
        "nil",
    );
    // Empty input is vacuously T.
    assert_eq!(s.eval("(every (lambda (x) nil) nil)").unwrap(), "T");
}

#[test]
fn every_binary_walks_pairwise_with_predicate() {
    let mut s = fresh();
    // (every #'< '(1 2 3) '(2 3 4)) — every pair (a < b)? T.
    assert_eq!(
        s.eval("(every #'< '(1 2 3) '(2 3 4))").unwrap(),
        "T",
    );
    // (every #'< '(1 2 3) '(2 1 4)) — middle pair (2 < 1) fails.
    assert_eq!(
        s.eval("(every #'< '(1 2 3) '(2 1 4))").unwrap(),
        "nil",
    );
}

#[test]
fn every_short_circuits_on_first_nil() {
    let mut s = fresh();
    // Trace the predicate calls — only the first should run.
    let prog = "
        (defparameter *calls* 0)
        (defparameter *r*
          (every (lambda (x)
                   (setq *calls* (+ *calls* 1))
                   (> x 5))
                 '(1 2 3 4 5)))
        (list *r* *calls*)
    ";
    // First call: x=1; predicate returns NIL; every stops.
    assert_eq!(s.eval(prog).unwrap(), "(nil 1)");
}

// ── SOME ─────────────────────────────────────────────────────────────

#[test]
fn some_unary_returns_first_nonnil() {
    let mut s = fresh();
    // Returns the actual predicate result, not just T.
    assert_eq!(
        s.eval("(some (lambda (x) (if (> x 5) (list :big x) nil)) '(1 7 3 9))")
            .unwrap(),
        "(:BIG 7)",
    );
    // No hit → NIL.
    assert_eq!(
        s.eval("(some (lambda (x) (> x 100)) '(1 2 3))").unwrap(),
        "nil",
    );
}

#[test]
fn some_binary_pairwise_first_hit() {
    let mut s = fresh();
    // Find a > b in two parallel lists. (1>0)? NIL; (2>5)? NIL; (3>5)? NIL — but wait,
    // (1>0) is T. So this should hit immediately.
    assert_eq!(
        s.eval("(some #'> '(1 2 3) '(0 5 5))").unwrap(),
        "T",
    );
    // All NIL.
    assert_eq!(
        s.eval("(some #'> '(1 2 3) '(10 20 30))").unwrap(),
        "nil",
    );
}

#[test]
fn some_short_circuits_on_first_hit() {
    let mut s = fresh();
    let prog = "
        (defparameter *calls* 0)
        (defparameter *r*
          (some (lambda (x)
                  (setq *calls* (+ *calls* 1))
                  (if (= x 2) :found nil))
                '(1 2 3 4 5)))
        (list *r* *calls*)
    ";
    // First call: x=1 → NIL.  Second: x=2 → :FOUND. Stops.
    assert_eq!(s.eval(prog).unwrap(), "(:FOUND 2)");
}

// ── Mixed-arity higher-order usage ────────────────────────────────────

#[test]
fn variadic_mapcar_passes_funcell_natives() {
    let mut s = fresh();
    // The mapcar fast-path goes through funcall on #'+, which now
    // resolves to its native shim (see commit 5d15593).
    assert_eq!(
        s.eval("(mapcar #'+ '(1 2 3) '(10 20 30))").unwrap(),
        "(11 22 33)",
    );
    // Triadic.
    assert_eq!(
        s.eval("(mapcar #'+ '(1 2 3) '(10 20 30) '(100 200 300))")
            .unwrap(),
        "(111 222 333)",
    );
}
