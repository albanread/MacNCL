//! Coverage for the DO and DO* macros in core.lisp.
//!
//! ANSI CL's general iterative construct, parametrised over
//! parallel vs sequential init/step semantics. Common Lisp code
//! ported from other systems leans on it heavily — variadic
//! mapping operators, multi-list walks, search-and-extract idioms
//! all use DO at the bottom.
//!
//! These tests pin down every part of the contract:
//!
//!   * single-variable counting do
//!   * multi-variable parallel step (the swap idiom)
//!   * no-step bindings (variable kept constant across iterations)
//!   * multi-form result clause
//!   * (return val) inside body exits the implicit (block nil …)
//!   * DO* sequential init/step semantics differ from DO

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn fresh() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

// ── DO ────────────────────────────────────────────────────────────────

#[test]
fn do_counting_loop() {
    let mut s = fresh();
    // Classic accumulator: sum 0..9 = 45.
    assert_eq!(
        s.eval(
            "(do ((i 0 (+ i 1))
                  (sum 0 (+ sum i)))
                 ((= i 10) sum))"
        )
        .unwrap(),
        "45",
    );
}

#[test]
fn do_parallel_step_swap_idiom() {
    let mut s = fresh();
    // (do ((a 0 b) (b 1 (+ a b))) ((> a 100) a))
    // Fibonacci-style walk: a steps to old-b, b steps to old-a+old-b.
    // Both steps see the OLD values — parallel update.
    //   a=0 b=1  → a=1  b=1
    //   a=1 b=1  → a=1  b=2
    //   a=1 b=2  → a=2  b=3
    //   a=2 b=3  → a=3  b=5
    //   …
    // First a > 100 is 144 (a after b became 144).
    assert_eq!(
        s.eval(
            "(do ((a 0 b)
                  (b 1 (+ a b)))
                 ((> a 100) a))"
        )
        .unwrap(),
        "144",
    );
}

#[test]
fn do_no_step_binding_is_loop_constant() {
    let mut s = fresh();
    // `(bound 10)` has no STEP form, so BOUND stays at 10 throughout.
    assert_eq!(
        s.eval(
            "(do ((bound 10)
                  (i 0 (+ i 1)))
                 ((>= i bound) :done))"
        )
        .unwrap(),
        ":DONE",
    );
}

#[test]
fn do_result_clause_is_an_implicit_progn() {
    let mut s = fresh();
    // Multi-form result; all evaluated, value of the last is returned.
    let prog = "
        (defparameter *side-effects* nil)
        (do ((i 0 (+ i 1)))
            ((= i 3)
             (push :a *side-effects*)
             (push :b *side-effects*)
             *side-effects*))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(:B :A)");
}

#[test]
fn do_body_can_return_early_via_block_nil() {
    let mut s = fresh();
    // `(return x)` inside the body exits the implicit (block nil …).
    assert_eq!(
        s.eval(
            "(do ((i 0 (+ i 1)))
                 ((> i 100) :overflow)
               (when (= i 7) (return :found-7)))"
        )
        .unwrap(),
        ":FOUND-7",
    );
}

#[test]
fn do_with_no_bindings_loops_until_test() {
    let mut s = fresh();
    // Empty bindings list — the loop runs until something inside
    // sets state that flips the end-test, or `return` exits.
    let prog = "
        (defparameter *counter* 0)
        (do ()
            ((>= *counter* 5) *counter*)
          (setq *counter* (+ *counter* 1)))
    ";
    assert_eq!(s.eval(prog).unwrap(), "5");
}

// ── DO* ───────────────────────────────────────────────────────────────

#[test]
fn do_star_sequential_init() {
    let mut s = fresh();
    // DO*'s init is let*-shaped: J's init form sees I's binding.
    assert_eq!(
        s.eval(
            "(do* ((i 5)
                   (j (+ i 1)))
                  ((> i 0) (list i j)))"
        )
        .unwrap(),
        "(5 6)",
    );
}

#[test]
fn do_star_sequential_step_sees_updated_values() {
    let mut s = fresh();
    // Each step expression is evaluated after the previous one has
    // assigned — so J in pass 2 sees the NEW I.
    //   pass 0: i=0 j=0
    //   pass 1: i=1; then j=(+ j 2) but with updated semantics —
    //           DO*'s step is let*-style: i steps first to 1, then
    //           j's step `(+ j 2)` runs (still uses old j=0+2=2 …)
    // Actually for DO*, the STEP forms run sequentially; each step
    // assigns BEFORE the next is evaluated. So J's step `(+ j 2)`
    // sees the freshly-assigned I but the old J. Confirm.
    assert_eq!(
        s.eval(
            "(do* ((i 0 (+ i 1))
                   (j 0 (+ i j)))
                  ((>= i 3) (list i j)))"
        )
        .unwrap(),
        // pass 0:  i=0  j=0
        // step 0: i := (+ 0 1) = 1
        //         j := (+ i j) where i is now 1, j is 0  → 1
        // pass 1: i=1 j=1; end-test 1>=3? no
        // step 1: i := (+ 1 1) = 2
        //         j := (+ i j) where i=2, j=1 → 3
        // pass 2: i=2 j=3; end-test no
        // step 2: i := (+ 2 1) = 3
        //         j := (+ i j) where i=3, j=3 → 6
        // pass 3: i=3 j=6; end-test 3>=3? yes → (list 3 6)
        "(3 6)",
    );
}

#[test]
fn do_star_no_step_bindings_stay_constant() {
    let mut s = fresh();
    // Same as DO but verifying DO* also tolerates no-step bindings.
    assert_eq!(
        s.eval(
            "(do* ((k 7)
                   (i 0 (+ i 1)))
                  ((>= i 3) k))"
        )
        .unwrap(),
        "7",
    );
}

// ── Interaction with the rest of the language ────────────────────────

#[test]
fn do_nests_inside_a_function() {
    let mut s = fresh();
    let prog = "
        (defun sum-up-to (n)
          (do ((i 0 (+ i 1))
               (s 0 (+ s i)))
              ((> i n) s)))
        (list (sum-up-to 5) (sum-up-to 100))
    ";
    // sum 0..5 = 15; sum 0..100 = 5050.
    assert_eq!(s.eval(prog).unwrap(), "(15 5050)");
}

#[test]
fn do_walks_a_list_via_cdr_stepping() {
    let mut s = fresh();
    // Common idiom: walk a list with `(rest (cdr rest))` and
    // accumulate. Exits when REST is null.
    assert_eq!(
        s.eval(
            "(do ((rest '(1 2 3 4 5) (cdr rest))
                  (sum 0 (+ sum (car rest))))
                 ((null rest) sum))"
        )
        .unwrap(),
        "15",
    );
}
