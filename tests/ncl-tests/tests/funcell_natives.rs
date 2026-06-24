//! Coverage for the function-cell bindings of numeric comparison
//! operators and LENGTH. Before this slice, `(funcall #'< 1 2)`
//! failed with `undefined function: <` because the compiler
//! lowered `(< a b)` as a special form and never installed a
//! callable in the symbol's function cell. The same applied to
//! `>`, `<=`, `>=`, `=`, `/=`, and `length`. `/=` had no binding
//! at all — neither special-form nor function-cell.
//!
//! These tests pin down that the funcall path now works for every
//! one of those names, and that the direct-call special-form
//! lowering is unchanged (no regression in the fast path).

use ncl_compiler::Session;
use ncl_tests::TestSession;

fn fresh() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    TestSession::with_thread_name(s)
}

#[test]
fn funcall_of_comparison_returns_t_or_nil() {
    let mut s = fresh();
    assert_eq!(s.eval("(funcall #'< 1 2)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'< 2 1)").unwrap(), "nil");
    assert_eq!(s.eval("(funcall #'> 2 1)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'<= 3 3)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'>= 3 4)").unwrap(), "nil");
    assert_eq!(s.eval("(funcall #'= 7 7)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'= 7 8)").unwrap(), "nil");
}

#[test]
fn funcall_of_length_walks_strings_and_lists() {
    let mut s = fresh();
    assert_eq!(s.eval("(funcall #'length '(a b c d))").unwrap(), "4");
    assert_eq!(s.eval("(funcall #'length \"hello\")").unwrap(), "5");
    assert_eq!(s.eval("(funcall #'length nil)").unwrap(), "0");
}

#[test]
fn slash_eq_works_both_directly_and_via_funcall() {
    let mut s = fresh();
    // Direct call — falls through generic call path since /= isn't
    // a special form. Now it resolves to the shim instead of
    // signalling "undefined function: /=".
    assert_eq!(s.eval("(/= 1 2)").unwrap(), "T");
    assert_eq!(s.eval("(/= 5 5)").unwrap(), "nil");
    // Through funcall.
    assert_eq!(s.eval("(funcall #'/= 1 2)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'/= 5 5)").unwrap(), "nil");
}

#[test]
fn comparison_funcells_span_the_numeric_tower() {
    let mut s = fresh();
    // The shims go through ncl_cmp_full, which crosses fixnums /
    // bignums / ratios / floats. So `(< 1 1.5)` is T even though
    // 1 is fixnum and 1.5 is float.
    assert_eq!(s.eval("(funcall #'< 1 1.5)").unwrap(), "T");
    assert_eq!(s.eval("(funcall #'= 1 1.0)").unwrap(), "T");
    // ... and `(/= 1 1.0)` is NIL — numeric equality, not EQL.
    assert_eq!(s.eval("(funcall #'/= 1 1.0)").unwrap(), "nil");
}

#[test]
fn higher_order_use_of_comparison_operators() {
    let mut s = fresh();
    // The payoff: pass #'< to a higher-order combinator without
    // a wrapper. SORT (in core.lisp) takes its comparator as a
    // function value, so this directly exercises the funcall
    // shim from inside the merge step.
    assert_eq!(
        s.eval("(sort (list 3 1 4 1 5 9 2 6) #'<)").unwrap(),
        "(1 1 2 3 4 5 6 9)",
    );
    assert_eq!(
        s.eval("(sort (list 3 1 4 1 5 9 2 6) #'>)").unwrap(),
        "(9 6 5 4 3 2 1 1)",
    );
}

#[test]
fn direct_calls_still_use_special_form_lowering() {
    // The funcall path now has shims, but the direct-call path
    // should still go through the compiler's binary_op lowering
    // (no regression in the fast path). Simplest probe: a tight
    // arithmetic-comparison loop still works exactly as before.
    let mut s = fresh();
    let prog = "
        (defun sum-up-to (n)
          (let ((i 0) (s 0))
            (loop
              (when (>= i n) (return s))
              (setq s (+ s i))
              (setq i (+ i 1)))))
        (sum-up-to 100)
    ";
    assert_eq!(s.eval(prog).unwrap(), "4950");
}
