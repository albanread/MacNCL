//! Phase 4 coverage for `docs/WINDOWS_FFI.md`: the metadata pack
//! and the `(win32 …)` / `(defwin32 …)` surface.

use std::path::PathBuf;
use std::process::Command;

fn ncl_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); p.pop();
    p.push("target"); p.push("debug");
    p.push(if cfg!(windows) { "ncl.exe" } else { "ncl" });
    p
}

fn run_ncl(args: &[&str]) -> (String, String, i32) {
    let output = Command::new(ncl_path())
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("could not spawn ncl: {e}"));
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

// ─── %win32-lookup primitive ──────────────────────────────────────

#[test]
fn lookup_returns_plist_for_known_function() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(%win32-lookup \"MessageBoxW\")",
    ]);
    assert_eq!(code, 0);
    // Should contain the basics: DLL, args, ret, sle, route
    assert!(stdout.contains("USER32"),  "missing USER32: {stdout}");
    assert!(stdout.contains(":HANDLE"), "missing :HANDLE: {stdout}");
    assert!(stdout.contains(":WSTR"),   "missing :WSTR:  {stdout}");
    assert!(stdout.contains(":UI"),     "missing :UI route: {stdout}");
}

#[test]
fn lookup_returns_nil_for_unknown_function() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "--windows", "-e",
        "(%win32-lookup \"ThisDoesNotExist\")",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.trim().to_lowercase().starts_with("nil"),
            "expected NIL, got: {stdout:?}");
}

#[test]
fn lookup_without_windows_returns_nil() {
    // Without --windows the pack isn't loaded, so every lookup is
    // NIL. Phase 4 deliberately doesn't error here — the caller
    // can decide.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%win32-lookup \"GetTickCount\")",
    ]);
    assert_eq!(code, 0);
    assert!(stdout.trim().to_lowercase().starts_with("nil"),
            "expected NIL, got: {stdout:?}");
}

// ─── (win32 …) cold path ──────────────────────────────────────────

#[test]
fn win32_get_tick_count() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A\" (win32 \"GetTickCount\"))",
    ]);
    assert_eq!(code, 0);
    let s = stdout.lines().next().unwrap_or("");
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u64 = digits.parse()
        .unwrap_or_else(|_| panic!("expected an integer, got: {stdout:?}"));
    assert!(n > 0);
}

#[test]
fn win32_get_system_metrics_two_calls() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(format t \"~A,~A\" (win32 \"GetSystemMetrics\" 0) (win32 \"GetSystemMetrics\" 1))",
    ]);
    assert_eq!(code, 0);
    let parts: Vec<&str> = stdout.lines().next().unwrap().split(',').collect();
    let w: i64 = parts[0].parse().unwrap();
    let h: i64 = parts[1].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap();
    assert!(w > 0 && h > 0);
}

#[test]
fn win32_unknown_name_errors() {
    let (_stdout, stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(win32 \"ThisDoesNotExist\")",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("ThisDoesNotExist") || stderr.contains("metadata"),
            "expected error mentioning the bad name, got: {stderr:?}");
}

// ─── (defwin32 …) hot path ────────────────────────────────────────

#[test]
fn defwin32_generates_callable_function() {
    let (stdout, stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(progn (defwin32 my-tick \"GetTickCount\")
                (format t \"~A\" (my-tick)))",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let s = stdout.lines().next().unwrap();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u64 = digits.parse().unwrap();
    assert!(n > 0);
}

#[test]
fn defwin32_with_args() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(progn (defwin32 metrics \"GetSystemMetrics\")
                (format t \"~A,~A\" (metrics 0) (metrics 1)))",
    ]);
    assert_eq!(code, 0);
    let parts: Vec<&str> = stdout.lines().next().unwrap().split(',').collect();
    let w: i64 = parts[0].parse().unwrap();
    let h: i64 = parts[1].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap();
    assert!(w > 0 && h > 0);
}

#[test]
fn defwin32_unknown_name_errors_at_macro_expansion() {
    // Macro-time lookup; should error during compile, not at call.
    let (_stdout, stderr, code) = run_ncl(&[
        "--windows", "-e",
        "(defwin32 nope \"NotAFunction\")",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("NotAFunction") || stderr.contains("metadata"),
            "expected macro-time error, got: {stderr:?}");
}
