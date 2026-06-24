//! Coverage for `Lisp/Library/trees.lisp` — the CL tree-walking
//! surface ported from Roger Corman's Sys/trees.lisp,
//! Sys/lists.lisp, and Sys/misc.lisp.
//!
//! Each test loads the module into a fresh stdlib session, then
//! exercises a particular operator against a deliberately mixed
//! tree (atoms, sub-conses, dotted tails) so the recursion shape
//! is exposed.

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

fn fresh_session_with_trees() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots with stdlib");
    s.activate();
    let path = library_path("trees.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/trees.lisp loads");
    TestSession::with_thread_name(s)
}

#[test]
fn subst_replaces_matching_atoms_throughout_a_tree() {
    let mut s = fresh_session_with_trees();
    // Replace every 'X with 'Y in a nested tree, preserving shape.
    assert_eq!(
        s.eval("(subst 'y 'x '(a x (b x c) (x . d)))").unwrap(),
        "(A Y (B Y C) (Y . D))",
    );
}

#[test]
fn subst_leaves_unmatched_tree_untouched() {
    let mut s = fresh_session_with_trees();
    assert_eq!(
        s.eval("(subst 'z 'q '(a b (c (d) e)))").unwrap(),
        "(A B (C (D) E))",
    );
}

#[test]
fn subst_with_key_consults_the_key_function() {
    let mut s = fresh_session_with_trees();
    // :KEY is applied to *every* node SUBST visits, including
    // atom leaves; the user is responsible for supplying a key
    // function that handles atoms. We use a guarded key —
    // (car n) for conses, n itself for atoms — so the walk
    // terminates cleanly when it hits NIL or a number.
    let prog = "
        (defparameter *tr* '((a 1) (target 2) (b (target 3))))
        (subst '(replaced 0) 'target *tr*
               :key (lambda (n) (if (consp n) (car n) n)))
    ";
    assert_eq!(
        s.eval(prog).unwrap(),
        "((A 1) (REPLACED 0) (B (REPLACED 0)))",
    );
}

#[test]
fn subst_with_test_equal_matches_string_leaves() {
    let mut s = fresh_session_with_trees();
    // The default :TEST is #'eql, which never says two distinct
    // string instances are equal. Opting into #'equal makes
    // string-content substitution work.
    assert_eq!(
        s.eval(
            r#"(subst 'changed "target" '(a "target" (b "target" c)) :test #'equal)"#
        )
        .unwrap(),
        "(A CHANGED (B CHANGED C))",
    );
}

#[test]
fn subst_if_uses_predicate_match() {
    let mut s = fresh_session_with_trees();
    // Replace every NUMBER in the tree with 0. Uses #'numberp as
    // the predicate; recursion preserves cons structure.
    assert_eq!(
        s.eval("(subst-if 0 #'numberp '(a 1 (b 2 c) (3 . d)))").unwrap(),
        "(A 0 (B 0 C) (0 . D))",
    );
}

#[test]
fn subst_if_not_inverts_predicate() {
    let mut s = fresh_session_with_trees();
    // Replace every node that is NOT a cons or a number with X.
    // In this tree only the symbol 'a survives the negation.
    assert_eq!(
        s.eval("(subst-if-not 'x #'numberp '(1 a 2))").unwrap(),
        "X",
    );
}

#[test]
fn nsubst_mutates_in_place_and_returns_root() {
    let mut s = fresh_session_with_trees();
    // nsubst rewrites the existing conses; we ensure that the
    // bound variable's root still equals the returned root and
    // both reflect the substitutions.
    let prog = "
        (defparameter *t* (list 'a 'x (list 'b 'x 'c)))
        (defparameter *r* (nsubst 'y 'x *t*))
        (list *t* *r*)
    ";
    assert_eq!(
        s.eval(prog).unwrap(),
        "((A Y (B Y C)) (A Y (B Y C)))",
    );
}

#[test]
fn sublis_substitutes_via_alist() {
    let mut s = fresh_session_with_trees();
    // Two substitutions in one walk via an alist of pairs.
    assert_eq!(
        s.eval("(sublis '((a . 1) (b . 2)) '(a (b a) (c b)))").unwrap(),
        "(1 (2 1) (C 2))",
    );
}

#[test]
fn sublis_with_no_match_returns_eql_tree() {
    let mut s = fresh_session_with_trees();
    // No keys match → result must structurally equal the input.
    assert_eq!(
        s.eval("(equal (sublis '((q . r)) '(a b (c d))) '(a b (c d)))").unwrap(),
        "T",
    );
}

#[test]
fn tree_equal_compares_shape_and_leaves() {
    let mut s = fresh_session_with_trees();
    // Same shape, same leaves under eql: T.
    assert_eq!(
        s.eval("(tree-equal '(a (b 1) c) '(a (b 1) c))").unwrap(),
        "T",
    );
    // Same shape, leaf mismatch: NIL.
    assert_eq!(
        s.eval("(tree-equal '(a (b 1) c) '(a (b 2) c))").unwrap(),
        "nil",
    );
    // Different shapes (one atom, one cons in a leaf position): NIL.
    assert_eq!(
        s.eval("(tree-equal '(a (b 1)) '(a b 1))").unwrap(),
        "nil",
    );
}

#[test]
fn tree_equal_honours_custom_test() {
    let mut s = fresh_session_with_trees();
    // EQL says "a" /= "a" (distinct string instances), so tree-equal
    // with default test sees a leaf mismatch.
    assert_eq!(
        s.eval(r#"(tree-equal '("a" "b") '("a" "b"))"#).unwrap(),
        "nil",
    );
    // EQUAL compares string contents — same trees now equal.
    assert_eq!(
        s.eval(r#"(tree-equal '("a" "b") '("a" "b") :test #'equal)"#).unwrap(),
        "T",
    );
}

#[test]
fn copy_tree_returns_a_fresh_cons_skeleton() {
    let mut s = fresh_session_with_trees();
    // The copy is EQUAL to the original …
    let prog = "
        (defparameter *t* '(a (b c) (d (e f))))
        (defparameter *c* (copy-tree *t*))
        (list (equal *t* *c*) (eq *t* *c*))
    ";
    // … but not EQ to it: every cons is freshly allocated.
    assert_eq!(s.eval(prog).unwrap(), "(T nil)");
}

#[test]
fn copy_tree_shares_atom_leaves() {
    // Atoms (symbols, fixnums, strings) are shared — only conses
    // are copied. A modification through the atom would (by
    // identity) be visible in both trees, but since these atoms
    // are immutable we just check the structural part.
    let mut s = fresh_session_with_trees();
    assert_eq!(
        s.eval("(equal (copy-tree '(1 2 (3 (4 5)) 6)) '(1 2 (3 (4 5)) 6))")
            .unwrap(),
        "T",
    );
}

#[test]
fn revappend_concatenates_reversed_list_to_tail() {
    let mut s = fresh_session_with_trees();
    // Direct definition: (revappend '(1 2 3) '(4 5)) = (3 2 1 4 5).
    assert_eq!(
        s.eval("(revappend '(1 2 3) '(4 5))").unwrap(),
        "(3 2 1 4 5)",
    );
    // Empty source list — returns the tail unchanged.
    assert_eq!(
        s.eval("(revappend nil '(4 5))").unwrap(),
        "(4 5)",
    );
    // Empty tail — returns just the reverse.
    assert_eq!(
        s.eval("(revappend '(1 2 3) nil)").unwrap(),
        "(3 2 1)",
    );
}
