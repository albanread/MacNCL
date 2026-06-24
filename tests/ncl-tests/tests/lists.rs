//! Coverage for `Lisp/Library/lists.lisp` — the CL list-mapping
//! and alist-helper surface ported from Corman's Sys/lists.lisp.
//! Exercises every operator end-to-end through the JIT.

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

fn fresh_session_with_lists() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots with stdlib");
    s.activate();
    let path = library_path("lists.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/lists.lisp loads");
    TestSession::with_thread_name(s)
}

// ── Map family ─────────────────────────────────────────────────────────

#[test]
fn maplist_walks_cdr_chains_and_collects() {
    let mut s = fresh_session_with_lists();
    // (maplist #'identity '(a b c)) gives the chain of cdrs.
    assert_eq!(
        s.eval("(maplist #'identity '(a b c))").unwrap(),
        "((A B C) (B C) (C))",
    );
    // The function sees the *tail*, not the head — counting cars
    // from each tail gives a strictly decreasing series.
    assert_eq!(
        s.eval("(maplist #'length '(a b c d e))").unwrap(),
        "(5 4 3 2 1)",
    );
}

#[test]
fn mapl_walks_for_effect_and_returns_the_original_list() {
    let mut s = fresh_session_with_lists();
    // MAPL is called for effect; its return value is LST.
    let prog = "
        (defparameter *tails-seen* nil)
        (mapl (lambda (tail) (push tail *tails-seen*)) '(a b c))
        (list (length *tails-seen*) *tails-seen*)
    ";
    assert_eq!(
        s.eval(prog).unwrap(),
        "(3 ((C) (B C) (A B C)))",
    );
}

#[test]
fn mapcan_nconcs_per_element_results() {
    let mut s = fresh_session_with_lists();
    // FN returns a list per element; MAPCAN concatenates.
    assert_eq!(
        s.eval("(mapcan (lambda (x) (list x x)) '(1 2 3))").unwrap(),
        "(1 1 2 2 3 3)",
    );
    // FN can return NIL to contribute nothing — useful for
    // filter-and-flatten.
    assert_eq!(
        s.eval("(mapcan (lambda (x) (if (oddp x) (list x) nil)) '(1 2 3 4 5))")
            .unwrap(),
        "(1 3 5)",
    );
}

#[test]
fn mapcon_nconcs_per_tail_results() {
    let mut s = fresh_session_with_lists();
    // FN returns (cars-from-tail). MAPCON concatenates all of them.
    assert_eq!(
        s.eval(
            "(mapcon (lambda (tail) (list (length tail))) '(a b c))"
        )
        .unwrap(),
        "(3 2 1)",
    );
}

// ── Alist helpers ─────────────────────────────────────────────────────

#[test]
fn acons_prepends_a_single_pair() {
    let mut s = fresh_session_with_lists();
    assert_eq!(
        s.eval("(acons 'a 1 '((b . 2) (c . 3)))").unwrap(),
        "((A . 1) (B . 2) (C . 3))",
    );
    // ACONS onto NIL gives a one-pair alist.
    assert_eq!(
        s.eval("(acons 'x 42 nil)").unwrap(),
        "((X . 42))",
    );
}

#[test]
fn pairlis_zips_keys_and_values_into_an_alist() {
    let mut s = fresh_session_with_lists();
    // Two equal-length lists; no tail.
    assert_eq!(
        s.eval("(pairlis '(a b c) '(1 2 3))").unwrap(),
        "((A . 1) (B . 2) (C . 3))",
    );
    // Mismatched lengths — pairs up to the shorter.
    assert_eq!(
        s.eval("(pairlis '(a b c) '(1 2))").unwrap(),
        "((A . 1) (B . 2))",
    );
    // With a tail alist, new pairs are prepended.
    assert_eq!(
        s.eval("(pairlis '(x) '(99) '((a . 1) (b . 2)))").unwrap(),
        "((X . 99) (A . 1) (B . 2))",
    );
}

// ── Tail-sharing predicates ───────────────────────────────────────────

#[test]
fn tailp_recognises_structural_suffixes() {
    let mut s = fresh_session_with_lists();
    let prog = "
        (defparameter *xs* (list 'a 'b 'c 'd))
        (defparameter *tail* (cddr *xs*))   ; the cons containing 'c
        (tailp *tail* *xs*)
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
    // Same content, different identity: NIL.
    assert_eq!(
        s.eval("(tailp '(c d) '(a b c d))").unwrap(),
        "nil",
    );
    // NIL is the tail of every proper list.
    assert_eq!(
        s.eval("(tailp nil '(a b))").unwrap(),
        "T",
    );
}

#[test]
fn ldiff_returns_prefix_before_eq_object() {
    let mut s = fresh_session_with_lists();
    let prog = "
        (defparameter *xs* (list 'a 'b 'c 'd))
        (defparameter *tail* (cddr *xs*))
        (ldiff *xs* *tail*)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(A B)");
    // OBJECT not a tail → full copy of LST.
    assert_eq!(
        s.eval("(ldiff '(a b c) 'q)").unwrap(),
        "(A B C)",
    );
    // LDIFF of NIL → NIL.
    assert_eq!(s.eval("(ldiff nil 'q)").unwrap(), "nil");
}

// ── Variadic forms (walk N lists in parallel) ─────────────────────────

#[test]
fn maplist_variadic_walks_n_lists_in_parallel() {
    let mut s = fresh_session_with_lists();
    // FN receives one tail per list; we cons (car a) onto (car b)
    // at each step to make the parallel walk visible.
    assert_eq!(
        s.eval(
            "(maplist (lambda (a b) (cons (car a) (car b)))
                      '(1 2 3) '(x y z))"
        )
        .unwrap(),
        "((1 . X) (2 . Y) (3 . Z))",
    );
    // Mismatched lengths: stops at the shortest.
    assert_eq!(
        s.eval(
            "(maplist (lambda (a b) (cons (car a) (car b)))
                      '(1 2 3 4 5) '(x y))"
        )
        .unwrap(),
        "((1 . X) (2 . Y))",
    );
}

#[test]
fn mapl_variadic_returns_first_list() {
    let mut s = fresh_session_with_lists();
    let prog = "
        (defparameter *seen* nil)
        (defparameter *r*
          (mapl (lambda (a b) (push (cons (car a) (car b)) *seen*))
                '(1 2 3) '(x y z)))
        (list *r* (nreverse *seen*))
    ";
    // First return is LIST (the first input, unchanged); second
    // shows the side-effect captured the parallel walk.
    assert_eq!(
        s.eval(prog).unwrap(),
        "((1 2 3) ((1 . X) (2 . Y) (3 . Z)))",
    );
}

#[test]
fn mapcan_variadic_nconcs_per_tuple() {
    let mut s = fresh_session_with_lists();
    // Each FN call receives one element per list and returns a list;
    // results are nconc'd.
    assert_eq!(
        s.eval(
            "(mapcan (lambda (a b) (list a b))
                     '(1 2 3) '(10 20 30))"
        )
        .unwrap(),
        "(1 10 2 20 3 30)",
    );
}

#[test]
fn mapcon_variadic_nconcs_tail_results() {
    let mut s = fresh_session_with_lists();
    assert_eq!(
        s.eval(
            "(mapcon (lambda (a b) (list (car a) (car b)))
                     '(1 2 3) '(x y z))"
        )
        .unwrap(),
        "(1 X 2 Y 3 Z)",
    );
}
