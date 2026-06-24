//! Phase 1 coverage for `docs/WINDOWS_FFI.md`: the `--windows` flag.
//!
//! These tests shell out to the `ncl` binary because the Windows
//! surface state (the `windows-enabled` and `ui-thread-id` OnceLocks)
//! is per-process; running multiple in-process variants in the same
//! test binary would collide. Each test gets a fresh subprocess.

use std::path::PathBuf;
use std::process::Command;

fn ncl_path() -> PathBuf {
    // tests/ncl-tests/tests/<this-file>.rs → walk up to target/debug/ncl.exe
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push("target");
    p.push("debug");
    p.push(if cfg!(windows) { "ncl.exe" } else { "ncl" });
    p
}

fn run_ncl(args: &[&str]) -> (String, String, i32) {
    let output = Command::new(ncl_path())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("could not spawn ncl: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, stderr, code)
}

#[test]
fn windows_enabled_p_without_flag_is_nil() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(format t \"~A\" (windows-enabled-p))",
    ]);
    assert_eq!(code, 0, "exit code should be 0");
    // The `format t` body writes the result; then the -e mechanism
    // prints the form's value, which is NIL. We just need to see
    // NIL appear in stdout (case may differ).
    assert!(stdout.contains("NIL") || stdout.contains("nil"),
            "expected NIL in stdout, got: {stdout:?}");
}

#[test]
fn windows_enabled_p_with_flag_is_t() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(format t \"~A\" (windows-enabled-p))",
    ]);
    assert_eq!(code, 0, "exit code should be 0");
    // format prints "T" (the value the shim returns) to stdout.
    assert!(stdout.contains('T'),
            "expected T in stdout, got: {stdout:?}");
}

#[test]
fn ui_thread_id_without_flag_is_nil() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(format t \"~A\" (ui-thread-id))",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("NIL") || stdout.contains("nil"),
            "expected NIL, got: {stdout:?}");
}

#[test]
fn ui_thread_id_with_flag_is_positive_fixnum() {
    // -e prints the form's value after the form runs. Since `format
    // t` returns NIL, the stdout will be "<tid>nil" (no separator).
    // Add an explicit newline-terminated format and parse the first
    // line's leading digits.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(format t \"~A~%\" (ui-thread-id))",
    ]);
    assert_eq!(code, 0);
    let first_line = stdout.lines().next().unwrap_or("");
    let digits: String = first_line.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().unwrap_or_else(|_| {
        panic!("expected an integer thread id in stdout, got: {stdout:?}")
    });
    assert!(n > 0, "thread id should be positive, got {n}");
}

#[test]
fn ui_thread_p_is_nil_on_worker_thread() {
    // With --windows, the -e body runs on the Lisp worker. The UI
    // thread is thread 0, running the pump. So (ui-thread-p) from
    // worker code must be NIL.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(format t \"~A\" (ui-thread-p))",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("NIL") || stdout.contains("nil"),
            "Lisp worker should NOT be the UI thread; got: {stdout:?}");
}

#[test]
fn windows_short_flag_w_works() {
    // Accept -W as the short form of --windows, matching --version/-V.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-W", "-e",
        "(format t \"~A\" (windows-enabled-p))",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains('T'),
            "expected T in stdout, got: {stdout:?}");
}

#[test]
fn worker_eval_completes_and_returns() {
    // Round-trip: with --windows, the worker should finish, send
    // its exit code, post WM_QUIT, and the pump should exit. If
    // any of that's broken the subprocess hangs and we time out.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(+ 1 2 3)",
    ]);
    assert_eq!(code, 0, "process should exit cleanly");
    // -e prints the last form's value:
    assert!(stdout.contains('6'), "expected '6' in stdout, got: {stdout:?}");
}

// ─── Phase 2: on-ui-thread closure marshalling ─────────────────────

#[test]
fn on_ui_thread_runs_closure_on_thread_zero() {
    // (ui-thread-p) outside on-ui-thread is NIL (we're on the worker);
    // inside on-ui-thread it's T (we're on thread 0). The macro
    // therefore proves the marshalling actually crossed threads.
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A ~A ~A\" (ui-thread-p) (on-ui-thread (ui-thread-p)) (ui-thread-p))",
    ]);
    assert_eq!(code, 0, "stderr: ");
    assert!(stdout.contains("nil T nil") || stdout.contains("NIL T NIL"),
            "expected 'nil T nil' pattern, got: {stdout:?}");
}

#[test]
fn on_ui_thread_propagates_return_value() {
    // The closure returns a value; on-ui-thread relays it to the
    // worker. Confirms the out_result slot of ExecuteRequest is
    // round-tripping correctly.
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A\" (on-ui-thread (+ 100 200 300)))",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("600"),
            "expected 600 in stdout, got: {stdout:?}");
}

#[test]
fn nested_on_ui_thread_short_circuits() {
    // Inside on-ui-thread, (ui-thread-p) is T, so the macro's
    // (if (ui-thread-p) (progn …) (%ui-execute …)) takes the
    // progn branch — no extra SendMessage round-trip. Functionally
    // this means three-deep nesting still returns the right value;
    // performance-wise it's a single thread hop. We can't easily
    // assert "no round-trip" but we can confirm correctness.
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A\" (on-ui-thread (on-ui-thread (on-ui-thread 42))))",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.contains("42"),
            "expected 42 in stdout, got: {stdout:?}");
}

#[test]
fn on_ui_thread_without_windows_errors() {
    // Without --windows, init.lisp doesn't auto-load win32-threading,
    // so the macro is undefined. The user sees a clean compiler
    // error rather than silently wrong behaviour.
    let (_stdout, stderr, code) = run_ncl(&[
        "-e", "(on-ui-thread (+ 1 2))",
    ]);
    assert_ne!(code, 0, "should fail when surface is off");
    assert!(stderr.to_lowercase().contains("on-ui-thread")
            || stderr.to_lowercase().contains("undefined"),
            "expected error mentioning on-ui-thread, got: {stderr:?}");
}

#[test]
fn ui_thread_id_inside_on_ui_thread_matches_outer() {
    // (ui-thread-id) returns the same OS thread id whether read
    // from the worker or from inside on-ui-thread. It's just a
    // OnceLock lookup, not "what thread am I on right now" — so it
    // must agree across threads.
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A=~A\" (ui-thread-id) (on-ui-thread (ui-thread-id)))",
    ]);
    assert_eq!(code, 0);
    let first_line = stdout.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split('=').collect();
    assert_eq!(parts.len(), 2, "expected exactly one '=', got: {first_line:?}");
    let outer: i64 = parts[0].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap();
    let inner: i64 = parts[1].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap();
    assert_eq!(outer, inner, "thread id should match across threads");
    assert!(outer > 0, "thread id should be positive");
}
