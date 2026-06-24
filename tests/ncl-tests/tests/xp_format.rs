//! Does FORMAT actually work after the XP override? Smoke-check
//! the basic CL directives against (format nil "...") output.

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
    for name in [
        "streams.lisp", "conditions.lisp", "loop.lisp",
        "sequences.lisp", "trees.lisp", "characters.lisp",
        "lists.lisp", "places.lisp", "numbers.lisp", "xp.lisp",
    ] {
        let path = library_path(name);
        let src = std::fs::read_to_string(&path).unwrap();
        s.eval(&src).unwrap_or_else(|e| panic!("load {name}: {e}"));
    }
    TestSession::with_thread_name(s)
}

#[test]
fn format_nil_returns_string() {
    let mut s = fresh_session_with_xp();
    let r = s.eval(r#"(format nil "hello")"#).unwrap();
    eprintln!("format nil 'hello' returned: {r}");
}

#[test]
fn format_basic_directives_a_and_s_and_d() {
    let mut s = fresh_session_with_xp();
    eprintln!("~A 42:    {:?}", s.eval(r#"(format nil "~A" 42)"#));
    eprintln!("~S \"hi\":  {:?}", s.eval(r#"(format nil "~S" "hi")"#));
    eprintln!("~D 7:     {:?}", s.eval(r#"(format nil "~D" 7)"#));
    eprintln!("~A list:  {:?}", s.eval(r#"(format nil "~A" '(1 2 3))"#));
}

#[test]
fn write_to_string_basic() {
    let mut s = fresh_session_with_xp();
    eprintln!("write-to-string 42:   {:?}", s.eval("(write-to-string 42)"));
    eprintln!("prin1-to-string 'foo: {:?}", s.eval("(prin1-to-string 'foo)"));
    eprintln!("princ-to-string 'foo: {:?}", s.eval("(princ-to-string 'foo)"));
}

#[test]
fn pprint_to_string() {
    let mut s = fresh_session_with_xp();
    // pprint to a string-output-stream
    let r = s.eval(
        r#"(let ((s (make-string-output-stream)))
             (pprint '(let ((x 1) (y 2)) (+ x y)) s)
             (get-output-stream-string s))"#,
    );
    eprintln!("pprint short: {:?}", r);
}

#[test]
fn pprint_fill_basic() {
    // pprint-fill on a short list. (After the unwind-protect /
    // catch+throw fix, the closing paren is emitted correctly.)
    let mut s = fresh_session_with_xp();
    let r = s.eval(
        r#"(with-output-to-string (out)
             (pprint-fill out '(1 2 3 4 5)))"#,
    ).unwrap();
    assert_eq!(r, "\"(1 2 3 4 5)\"");
}

#[test]
#[ignore = "user-level (formatter \"...\") expansion runs out of NCL static area for closure allocation — XP loads ~50 default-dispatch printers that already consume most of the static area at library-load time. Either NCL needs a bigger static area, heap-allocated closures, or formatter rewriting to share lambdas."]
fn formatter_macro_expands() {
    let mut s = fresh_session_with_xp();
    let r = s.eval(
        r#"(with-output-to-string (out)
             (funcall (formatter "~A + ~A = ~A") out 2 3 5))"#,
    ).unwrap();
    assert_eq!(r, "\"2 + 3 = 5\"");
}
