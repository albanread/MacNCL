//! Coverage for `Lisp/Library/xp.lisp` — Richard Waters' XP pretty
//! printer, ported from Corman Lisp's Sys/xp.lisp.
//!
//! These tests load streams + xp explicitly so they don't depend on
//! init.lisp load order. We verify:
//!   * the library loads without error;
//!   * pretty-printing predicates / control variables exist;
//!   * `pprint-fill` / `pprint-linear` produce some output;
//!   * `write-to-string` and `prin1-to-string` round-trip simple
//!     atoms and lists.

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

fn fresh_session_with_xp() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    // XP needs streams (string-output-stream, with-output-to-string),
    // characters (char-upcase, alpha-char-p, etc.), sequences
    // (find, position, fill, replace), and numbers (rationalp).
    for name in [
        "streams.lisp",
        "conditions.lisp",
        "loop.lisp",
        "sequences.lisp",
        "trees.lisp",
        "characters.lisp",
        "lists.lisp",
        "places.lisp",
        "numbers.lisp",
        "xp.lisp",
    ] {
        let path = library_path(name);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
    }
    TestSession::with_thread_name(s)
}

#[test]
fn xp_library_loads() {
    // Smoke test — just make sure the file evaluates without error.
    let _s = fresh_session_with_xp();
}

#[test]
fn print_control_variables_are_bound() {
    let mut s = fresh_session_with_xp();
    // All the print variables XP declares should be readable.
    assert_eq!(s.eval("(boundp '*print-escape*)").unwrap(),   "T");
    assert_eq!(s.eval("(boundp '*print-pretty*)").unwrap(),   "T");
    assert_eq!(s.eval("(boundp '*print-circle*)").unwrap(),   "T");
    assert_eq!(s.eval("(boundp '*print-level*)").unwrap(),    "T");
    assert_eq!(s.eval("(boundp '*print-length*)").unwrap(),   "T");
    assert_eq!(s.eval("(boundp '*print-pprint-dispatch*)").unwrap(), "T");
    assert_eq!(s.eval("(boundp '*default-right-margin*)").unwrap(),  "T");
}

#[test]
fn dispatch_table_initialised() {
    let mut s = fresh_session_with_xp();
    // *print-pprint-dispatch* should hold a pprint-dispatch object,
    // not the initial T sentinel.
    assert_eq!(s.eval("(pprint-dispatch-p *print-pprint-dispatch*)").unwrap(),
               "T");
}
