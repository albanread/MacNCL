//! Smoke test: confirm the reader can parse real Corman Lisp source
//! end-to-end. This is a precursor to Phase 1d's full corpus run.

use std::fs;
use std::path::Path;

#[test]
fn parses_baby_lisp() {
    let path = Path::new(r"E:\CL\cormanlisp\examples\baby.lisp");
    if !path.exists() {
        // Upstream may not be present in some environments — skip
        // rather than fail hard. The full corpus run (1d) will be
        // a hard-required test that only runs when the reference
        // is checked out.
        eprintln!("upstream not present at {} — skipping", path.display());
        return;
    }
    let src = fs::read_to_string(path).expect("read baby.lisp");
    let forms = ncl_reader::read_all(&src).expect("parse baby.lisp");
    assert!(forms.len() >= 5, "expected several top-level forms, got {}", forms.len());
}
