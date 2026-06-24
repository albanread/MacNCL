//! Coverage for extensions to `Lisp/Library/sequences.lisp`:
//! `delete-duplicates`, `nsubstitute` family, and `(setf subseq)`.

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

fn load(s: &mut Session, name: &str) {
    let path = library_path(name);
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
}

fn fresh_session() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    load(&mut s, "trees.lisp");
    load(&mut s, "symbols.lisp");  // destructuring-bind
    load(&mut s, "lists.lisp");
    load(&mut s, "places.lisp");   // defsetf
    load(&mut s, "sequences.lisp");
    TestSession::with_thread_name(s)
}

// ── delete-duplicates ─────────────────────────────────────────────────────

#[test]
fn delete_duplicates_removes_later_copies() {
    let mut s = fresh_session();
    assert_eq!(
        s.eval("(delete-duplicates '(1 2 3 2 1))").unwrap(),
        "(3 2 1)"
    );
}

#[test]
fn delete_duplicates_empty_list() {
    let mut s = fresh_session();
    let r = s.eval("(delete-duplicates '())").unwrap();
    assert!(r.eq_ignore_ascii_case("nil"), "got: {r}");
}

#[test]
fn delete_duplicates_no_dups() {
    let mut s = fresh_session();
    assert_eq!(s.eval("(delete-duplicates '(1 2 3))").unwrap(), "(1 2 3)");
}

#[test]
fn delete_duplicates_custom_test() {
    let mut s = fresh_session();
    // Using string= as the test — case-sensitive comparison.
    let prog = r#"(delete-duplicates '("a" "b" "a") :test #'equal)"#;
    assert_eq!(s.eval(prog).unwrap(), "(\"b\" \"a\")");
}

// ── nsubstitute ───────────────────────────────────────────────────────────

#[test]
fn nsubstitute_replaces_all() {
    let mut s = fresh_session();
    // In-place: replace all 1s with 99.
    assert_eq!(
        s.eval("(nsubstitute 99 1 (list 1 2 1 3 1))").unwrap(),
        "(99 2 99 3 99)"
    );
}

#[test]
fn nsubstitute_returns_sequence() {
    let mut s = fresh_session();
    // Return value is the (possibly modified) sequence.
    let prog = "
        (let ((v (list 'a 'b 'a 'c)))
          (eq v (nsubstitute 'x 'a v)))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn nsubstitute_count_limits_replacements() {
    let mut s = fresh_session();
    assert_eq!(
        s.eval("(nsubstitute 99 1 (list 1 1 1 1) :count 2)").unwrap(),
        "(99 99 1 1)"
    );
}

#[test]
fn nsubstitute_from_end_replaces_last() {
    let mut s = fresh_session();
    // :from-end replaces the last occurrences first.
    assert_eq!(
        s.eval("(nsubstitute 99 1 (list 1 1 1 1) :count 2 :from-end t)").unwrap(),
        "(1 1 99 99)"
    );
}

#[test]
fn nsubstitute_if_replaces_evens() {
    let mut s = fresh_session();
    let prog = "
        (nsubstitute-if 0 #'evenp (list 1 2 3 4 5 6))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 0 3 0 5 0)");
}

#[test]
fn nsubstitute_if_not_replaces_odds() {
    let mut s = fresh_session();
    let prog = "
        (nsubstitute-if-not 0 #'evenp (list 1 2 3 4 5 6))
    ";
    assert_eq!(s.eval(prog).unwrap(), "(0 2 0 4 0 6)");
}

// ── (setf subseq) ─────────────────────────────────────────────────────────

#[test]
fn setf_subseq_direct_call() {
    // Test %setf-subseq directly before testing via (setf (subseq ...))
    let mut s = fresh_session();
    let prog = "
        (let ((v (list 1 2 3 4 5)))
          (%setf-subseq '(10 20 30) v 1 4)
          v)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 10 20 30 5)");
}

#[test]
fn setf_subseq_copies_into_list() {
    let mut s = fresh_session();
    let prog = "
        (let ((v (list 1 2 3 4 5)))
          (setf (subseq v 1 4) '(10 20 30))
          v)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 10 20 30 5)");
}

#[test]
fn setf_subseq_shorter_source() {
    let mut s = fresh_session();
    // Source shorter than the slice — only fills what it can.
    let prog = "
        (let ((v (list 1 2 3 4 5)))
          (setf (subseq v 1 4) '(10 20))
          v)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(1 10 20 4 5)");
}

#[test]
fn setf_subseq_full_replace() {
    let mut s = fresh_session();
    let prog = "
        (let ((v (list 'a 'b 'c)))
          (setf (subseq v 0) '(x y z))
          v)
    ";
    assert_eq!(s.eval(prog).unwrap(), "(X Y Z)");
}
