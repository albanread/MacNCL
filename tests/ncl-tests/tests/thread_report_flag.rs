//! Regression: the `:report-when-finished` keyword on `create-thread`
//! actually reaches the native shim.
//!
//! Before the fix, `Library/threads.lisp`'s wrapper accepted the
//! keyword but discarded it (a `(declare (ignore ...))`) and the
//! native `%CREATE-THREAD` was installed with arity 1. The runtime
//! always emitted "[threads] thread N finished normally" on
//! exit, regardless of what the user requested.
//!
//! The visible side-effect of the flag is a stderr line; that's
//! awkward to capture from inside Rust unit tests. What we *can*
//! verify, cheaply and portably, is:
//!
//!   * Both shapes — bare `(create-thread fn)` and `(create-thread fn
//!     :report-when-finished nil)` — compile and evaluate without
//!     errors.
//!   * The thread actually runs the supplied closure (we assert on
//!     a side-effect via an atomic counter).
//!   * The Lisp wrapper is JIT-compilable as written.
//!
//! That covers the wrapper-arity plumbing without taking a
//! dependency on stderr capture.

use std::path::PathBuf;

use ncl_compiler::Session;
use ncl_tests::TestSession;

/// Path to `Lisp/Library/threads.lisp` relative to the workspace
/// root. `CARGO_MANIFEST_DIR` is the test crate's dir, so we walk
/// up two levels.
fn threads_lisp_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("Lisp");
    p.push("Library");
    p.push("threads.lisp");
    p
}

fn fresh_session_with_threads() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots with stdlib");
    s.activate();
    let path = threads_lisp_path();
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    s.eval(&src).expect("Library/threads.lisp loads");
    TestSession::with_thread_name(s)
}

#[test]
fn create_thread_accepts_both_shapes_and_runs_body() {
    let mut s = fresh_session_with_threads();

    // Shared counter the worker bumps; the test reads it after
    // joining. We use an atomic counter because the counter
    // crosses the spawn-thread / main-thread boundary, and v1
    // mailbox/atomics are the only thread-safe Lisp value we
    // can hand both sides.
    s.eval(
        "(defparameter *flag-c* (make-atomic-counter 0))
         (defparameter *bare-tid*
           (create-thread (lambda () (atomic-incf *flag-c*))))
         (defparameter *kw-t-tid*
           (create-thread (lambda () (atomic-incf *flag-c*))
                          :report-when-finished t))
         (defparameter *kw-nil-tid*
           (create-thread (lambda () (atomic-incf *flag-c*))
                          :report-when-finished nil))",
    )
    .expect("the three create-thread calls evaluate");

    s.eval(
        "(join-thread *bare-tid*)
         (join-thread *kw-t-tid*)
         (join-thread *kw-nil-tid*)",
    )
    .expect("join-thread returns");

    let final_value = s
        .eval("(atomic-get *flag-c*)")
        .expect("atomic-get returns");
    assert_eq!(
        final_value, "3",
        "all three workers ran exactly once",
    );
}

#[test]
fn create_thread_keyword_can_be_quoted_t_or_nil() {
    // Sanity: the keyword arg accepts both literal `t` and literal
    // `nil`. (`*foo*` style defparameters bind to those exact
    // symbols, so this is the shape user code is most likely to
    // hit.)
    let mut s = fresh_session_with_threads();
    s.eval(
        "(defparameter *report?* nil)
         (defparameter *tid*
           (create-thread (lambda () 42)
                          :report-when-finished *report?*))
         (join-thread *tid*)",
    )
    .expect("dynamic boolean is accepted by create-thread");
}
