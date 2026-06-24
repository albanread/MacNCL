//! Phase 3 coverage for `docs/WINDOWS_FFI.md`: the `%ffi-call`
//! kernel. Exercises real Win32 calls — KERNEL32 functions that
//! work from any thread, plus a few USER32 calls that are also
//! thread-safe (GetSystemMetrics) or self-contained (LoadCursorW).
//!
//! Like windows_flag.rs, each test shells out to a fresh `ncl`
//! subprocess so the process-global FFI cache state is clean.

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

// ─── KERNEL32 no-arg functions ────────────────────────────────────

#[test]
fn ffi_get_tick_count_returns_positive_u32() {
    let (stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"GetTickCount\" () :u32)",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let first_line = stdout.lines().next().unwrap_or("");
    let n: i64 = first_line.parse()
        .unwrap_or_else(|_| panic!("expected an integer, got: {stdout:?}"));
    assert!(n > 0, "tick count should be positive, got {n}");
}

#[test]
fn ffi_get_current_process_id() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"GetCurrentProcessId\" () :u32)",
    ]);
    assert_eq!(code, 0);
    let pid: i64 = stdout.lines().next().unwrap().parse().unwrap();
    assert!(pid > 0);
}

#[test]
fn ffi_get_current_thread_id() {
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"GetCurrentThreadId\" () :u32)",
    ]);
    assert_eq!(code, 0);
    let tid: i64 = stdout.lines().next().unwrap().parse().unwrap();
    assert!(tid > 0);
}

// ─── KERNEL32 with int args ───────────────────────────────────────

#[test]
fn ffi_set_last_error_void_return() {
    // SetLastError is a void function with one DWORD arg. We don't
    // pair it with GetLastError here because intermediate Lisp ops
    // (format / allocations / GC) call other Win32 APIs that can
    // clobber the last-error slot. This test only confirms the
    // call itself completes without crashing.
    let (_stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"SetLastError\" (list :u32) :void 42)",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
}

// ─── USER32 thread-safe functions ─────────────────────────────────

#[test]
fn ffi_get_system_metrics() {
    // GetSystemMetrics(0) = screen width in pixels; (1) = height.
    // Both must be positive on any real Windows install. Note the
    // trailing `~%` — without it `format` returns NIL which prints
    // as `nil` directly after the numbers, breaking the parse.
    let (stdout, _stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(format t \"~A ~A~%\"
           (%ffi-call \"USER32.dll\" \"GetSystemMetrics\" (list :i32) :i32 0)
           (%ffi-call \"USER32.dll\" \"GetSystemMetrics\" (list :i32) :i32 1))",
    ]);
    assert_eq!(code, 0);
    let line = stdout.lines().next().unwrap();
    let parts: Vec<&str> = line.split_whitespace().collect();
    assert!(parts.len() >= 2, "expected two numbers, got: {line:?}");
    let w: i64 = parts[0].parse().unwrap();
    let h: i64 = parts[1].parse().unwrap();
    assert!(w > 0 && h > 0,
            "screen metrics should be positive, got w={w} h={h}");
}

// ─── String marshalling ───────────────────────────────────────────

#[test]
fn ffi_output_debug_string_w_accepts_wstr() {
    // OutputDebugStringW is a void function that sends a string to
    // the attached debugger. From a test it does nothing observable
    // beyond not crashing — which is exactly what we want: it
    // confirms the :wstr UTF-16 marshalling reaches a real Win32
    // function call without ABI breakage.
    let (_stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"OutputDebugStringW\"
                    (list :wstr) :void \"hello from NCL FFI\")",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
}

// ─── Error handling ───────────────────────────────────────────────

#[test]
fn ffi_unknown_function_clean_error() {
    let (_stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"ThisDoesNotExist\" () :u32)",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("ThisDoesNotExist") || stderr.contains("null"),
            "expected error mentioning the function, got: {stderr:?}");
}

#[test]
fn ffi_unknown_type_tag_clean_error() {
    let (_stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        "(%ffi-call \"KERNEL32.dll\" \"GetTickCount\" () :nonsense)",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.contains("nonsense") || stderr.to_lowercase().contains("unknown"),
            "expected error mentioning the bad tag, got: {stderr:?}");
}

#[test]
fn ffi_wrong_arg_count_clean_error() {
    let (_stdout, stderr, code) = run_ncl(&[
        "--lean", "-e",
        // GetSystemMetrics takes 1 arg but we pass 0:
        "(%ffi-call \"USER32.dll\" \"GetSystemMetrics\" (list :i32) :i32)",
    ]);
    assert_ne!(code, 0);
    assert!(stderr.to_lowercase().contains("expects") || stderr.contains("args"),
            "expected arg-count error, got: {stderr:?}");
}
