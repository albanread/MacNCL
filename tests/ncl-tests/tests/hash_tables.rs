//! Coverage for `Lisp/Library/hash-tables.lisp` —
//! `with-hash-table-iterator` and `sxhash`.

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
    // Load in dependency order so all helpers are available.
    load(&mut s, "streams.lisp");
    load(&mut s, "conditions.lisp");
    load(&mut s, "trees.lisp");
    load(&mut s, "characters.lisp");  // make-string, char utilities
    load(&mut s, "symbols.lisp");     // ignore-errors (used by sxhash)
    load(&mut s, "lists.lisp");
    load(&mut s, "places.lisp");
    load(&mut s, "hash-tables.lisp");
    TestSession::with_thread_name(s)
}

// ── with-hash-table-iterator ─────────────────────────────────────────

#[test]
fn whi_visits_all_entries() {
    let mut s = fresh_session();
    let prog = "
        (let ((ht (make-hash-table :test 'eql))
              (sum 0))
          (setf (gethash :a ht) 1)
          (setf (gethash :b ht) 2)
          (setf (gethash :c ht) 3)
          (with-hash-table-iterator (next ht)
            (loop
              (multiple-value-bind (more? k v) (next)
                (declare (ignore k))
                (unless more? (return))
                (setq sum (+ sum v)))))
          sum)
    ";
    assert_eq!(s.eval(prog).unwrap(), "6");
}

#[test]
fn whi_returns_nil_on_empty_table() {
    let mut s = fresh_session();
    let prog = "
        (let ((ht (make-hash-table)))
          (with-hash-table-iterator (next ht)
            (multiple-value-bind (more? k v) (next)
              (declare (ignore k v))
              more?)))
    ";
    let out = s.eval(prog).unwrap();
    assert!(out.eq_ignore_ascii_case("nil"), "got: {out}");
}

#[test]
fn whi_key_value_roundtrip() {
    let mut s = fresh_session();
    // Store one entry; verify the exact key and value come back.
    let prog = "
        (let ((ht (make-hash-table :test 'equal)))
          (setf (gethash \"hello\" ht) 42)
          (with-hash-table-iterator (next ht)
            (multiple-value-bind (more? k v) (next)
              (and more? (equal k \"hello\") (= v 42)))))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn whi_exhausts_correctly() {
    let mut s = fresh_session();
    // Two entries: after reading both, next call returns nil.
    let prog = "
        (let ((ht (make-hash-table))
              (count 0))
          (setf (gethash 'x ht) 10)
          (setf (gethash 'y ht) 20)
          (with-hash-table-iterator (next ht)
            (loop
              (multiple-value-bind (more? k v) (next)
                (declare (ignore k v))
                (unless more? (return))
                (setq count (+ count 1)))))
          count)
    ";
    assert_eq!(s.eval(prog).unwrap(), "2");
}

// ── sxhash ────────────────────────────────────────────────────────────

#[test]
fn sxhash_is_nonneg_fixnum() {
    let mut s = fresh_session();
    // sxhash must return a non-negative integer.
    let prog = "
        (and (>= (sxhash 'hello)   0)
             (>= (sxhash 42)       0)
             (>= (sxhash \"str\")  0)
             (>= (sxhash '(1 2 3)) 0)
             (>= (sxhash nil)      0))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn sxhash_equal_objects_same_hash() {
    let mut s = fresh_session();
    // The fundamental requirement: (equal x y) => (= (sxhash x) (sxhash y)).
    // Two independently consed lists with the same structure must hash the same.
    let prog = "
        (= (sxhash (list 1 2 3))
           (sxhash (list 1 2 3)))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn sxhash_list_equal_same_hash() {
    let mut s = fresh_session();
    let prog = "
        (= (sxhash '(1 2 3))
           (sxhash (list 1 2 3)))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}

#[test]
fn sxhash_nil_is_zero() {
    let mut s = fresh_session();
    assert_eq!(s.eval("(sxhash nil)").unwrap(), "0");
}

#[test]
fn sxhash_stable_across_calls() {
    let mut s = fresh_session();
    // Same object hashes consistently.
    let prog = "
        (let ((x '(a b c)))
          (= (sxhash x) (sxhash x)))
    ";
    assert_eq!(s.eval(prog).unwrap(), "T");
}
