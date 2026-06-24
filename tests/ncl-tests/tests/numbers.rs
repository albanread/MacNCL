//! Coverage for `Lisp/Library/numbers.lisp` — polymorphic
//! FLOOR / CEILING / ROUND / TRUNCATE / MOD / REM with two-value
//! returns, optional-divisor forms, and integer / ratio / float
//! dispatch.
//!
//! The test session loads `numbers.lisp` explicitly so these tests
//! remain self-contained (no dependency on init.lisp load order).

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

fn fresh_session_with_numbers() -> TestSession {
    let mut s = Session::with_stdlib().expect("session boots");
    s.activate();
    let path = library_path("numbers.lisp");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read numbers.lisp: {e}"));
    s.eval(&src).unwrap_or_else(|e| panic!("load numbers.lisp: {e}"));
    TestSession::with_thread_name(s)
}

// ── TRUNCATE ─────────────────────────────────────────────────────────

#[test]
fn truncate_int_int_quotient() {
    let mut s = fresh_session_with_numbers();
    // Single-value context → first value (quotient).
    assert_eq!(s.eval("(truncate 10 3)").unwrap(),  "3");
    assert_eq!(s.eval("(truncate -7 2)").unwrap(),  "-3");
    assert_eq!(s.eval("(truncate 7 -2)").unwrap(),  "-3");
    assert_eq!(s.eval("(truncate -7 -2)").unwrap(), "3");
    assert_eq!(s.eval("(truncate 6 3)").unwrap(),   "2");
}

#[test]
fn truncate_int_int_two_values() {
    let mut s = fresh_session_with_numbers();
    // (multiple-value-list …) captures both values.
    assert_eq!(
        s.eval("(multiple-value-list (truncate 10 3))").unwrap(),
        "(3 1)"
    );
    assert_eq!(
        s.eval("(multiple-value-list (truncate -7 2))").unwrap(),
        "(-3 -1)"
    );
    assert_eq!(
        s.eval("(multiple-value-list (truncate 6 3))").unwrap(),
        "(2 0)"
    );
}

#[test]
fn truncate_unary_form() {
    // (truncate a) ≡ (truncate a 1) — rounds toward zero.
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(truncate 7)").unwrap(),  "7");
    assert_eq!(s.eval("(truncate -7)").unwrap(), "-7");
}

#[test]
fn truncate_invariant() {
    // (= a (+ (* q b) r)) for all integer a, b.
    let mut s = fresh_session_with_numbers();
    s.eval("(defun ok (a b)
              (multiple-value-bind (q r) (truncate a b)
                (= a (+ (* q b) r))))").unwrap();
    for pair in ["(ok 17 5)", "(ok -17 5)", "(ok 17 -5)", "(ok -17 -5)", "(ok 0 3)"] {
        assert_eq!(s.eval(pair).unwrap(), "T", "invariant failed for {pair}");
    }
}

#[test]
fn truncate_float_path() {
    let mut s = fresh_session_with_numbers();
    // (truncate 3.7) → quotient 3, remainder ≈ 0.7.
    assert_eq!(s.eval("(truncate 3.7)").unwrap(), "3");
    // Both values.
    let out = s.eval("(multiple-value-list (truncate 3.7))").unwrap();
    assert!(out.starts_with("(3 "), "got: {out}");
    // Negative float.
    assert_eq!(s.eval("(truncate -3.7)").unwrap(), "-3");
}

#[test]
fn truncate_ratio_path() {
    let mut s = fresh_session_with_numbers();
    // (truncate 7/3) → 2, remainder 1/3.
    assert_eq!(s.eval("(truncate 7/3)").unwrap(), "2");
    assert_eq!(
        s.eval("(multiple-value-list (truncate 7/3))").unwrap(),
        "(2 1/3)"
    );
    // Exact ratio.
    assert_eq!(
        s.eval("(multiple-value-list (truncate 6/3))").unwrap(),
        "(2 0)"
    );
    // Negative.
    assert_eq!(s.eval("(truncate -7/3)").unwrap(), "-2");
}

// ── FLOOR ────────────────────────────────────────────────────────────

#[test]
fn floor_int_int() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(floor 10 3)").unwrap(),  "3");
    // floor(-7, 2): true quotient = -3.5 → floor = -4.
    assert_eq!(s.eval("(floor -7 2)").unwrap(),  "-4");
    // floor(7, -2) = -4 as well.
    assert_eq!(s.eval("(floor 7 -2)").unwrap(),  "-4");
    assert_eq!(s.eval("(floor -7 -2)").unwrap(), "3");
    // Exact.
    assert_eq!(s.eval("(floor 6 3)").unwrap(),   "2");
}

#[test]
fn floor_two_values() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(
        s.eval("(multiple-value-list (floor 10 3))").unwrap(),
        "(3 1)"
    );
    assert_eq!(
        s.eval("(multiple-value-list (floor -7 2))").unwrap(),
        "(-4 1)"
    );
}

#[test]
fn floor_float_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(floor 3.7)").unwrap(), "3");
    assert_eq!(s.eval("(floor -3.7)").unwrap(), "-4");
}

#[test]
fn floor_ratio_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(floor 7/3)").unwrap(), "2");
    assert_eq!(s.eval("(floor -7/3)").unwrap(), "-3");
    assert_eq!(
        s.eval("(multiple-value-list (floor -7/3))").unwrap(),
        "(-3 2/3)"
    );
}

// ── CEILING ──────────────────────────────────────────────────────────

#[test]
fn ceiling_int_int() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(ceiling 10 3)").unwrap(),  "4");
    assert_eq!(s.eval("(ceiling -7 2)").unwrap(),  "-3");
    assert_eq!(s.eval("(ceiling 6 3)").unwrap(),   "2");
}

#[test]
fn ceiling_two_values() {
    let mut s = fresh_session_with_numbers();
    // (ceiling 10 3) = 4, rem = 10 - 4*3 = -2
    assert_eq!(
        s.eval("(multiple-value-list (ceiling 10 3))").unwrap(),
        "(4 -2)"
    );
}

#[test]
fn ceiling_float_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(ceiling 3.2)").unwrap(), "4");
    assert_eq!(s.eval("(ceiling -3.7)").unwrap(), "-3");
}

#[test]
fn ceiling_ratio_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(ceiling 7/3)").unwrap(), "3");
    assert_eq!(s.eval("(ceiling -7/3)").unwrap(), "-2");
}

// ── ROUND ────────────────────────────────────────────────────────────

#[test]
fn round_basic() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(round 7 2)").unwrap(),  "4"); // 3.5 → even = 4
    assert_eq!(s.eval("(round 5 2)").unwrap(),  "2"); // 2.5 → even = 2
    assert_eq!(s.eval("(round 3 2)").unwrap(),  "2"); // 1.5 → even = 2
    assert_eq!(s.eval("(round 10 3)").unwrap(), "3"); // 3.33 → 3
    assert_eq!(s.eval("(round 11 3)").unwrap(), "4"); // 3.67 → 4
}

#[test]
fn round_half_to_even() {
    // The canonical tie-breaking cases: n.5 → nearest even.
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(round 1 2)").unwrap(), "0"); // 0.5 → 0 (even)
    assert_eq!(s.eval("(round 3 2)").unwrap(), "2"); // 1.5 → 2 (even)
    assert_eq!(s.eval("(round 5 2)").unwrap(), "2"); // 2.5 → 2 (even)
    assert_eq!(s.eval("(round 7 2)").unwrap(), "4"); // 3.5 → 4 (even)
}

#[test]
fn round_two_values() {
    let mut s = fresh_session_with_numbers();
    // (round 7 2) = 4 (3.5 ties-even), rem = 7 - 4*2 = -1.
    assert_eq!(
        s.eval("(multiple-value-list (round 7 2))").unwrap(),
        "(4 -1)"
    );
}

#[test]
fn round_float_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(round 3.4)").unwrap(), "3");
    assert_eq!(s.eval("(round 3.6)").unwrap(), "4");
    // 2.5 → banker's round to even = 2.
    assert_eq!(s.eval("(round 2.5)").unwrap(), "2");
}

#[test]
fn round_ratio_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(round 7/3)").unwrap(), "2");
    // 5/2 = 2.5 → even → 2.
    assert_eq!(s.eval("(round 5/2)").unwrap(), "2");
    // 7/2 = 3.5 → even → 4.
    assert_eq!(s.eval("(round 7/2)").unwrap(), "4");
}

// ── MOD ──────────────────────────────────────────────────────────────

#[test]
fn mod_integer_sign_of_divisor() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(mod 10 3)").unwrap(),   "1");
    assert_eq!(s.eval("(mod -7  2)").unwrap(),  "1"); // sign of 2
    assert_eq!(s.eval("(mod  7 -2)").unwrap(), "-1"); // sign of -2
    assert_eq!(s.eval("(mod -7 -2)").unwrap(), "-1");
    assert_eq!(s.eval("(mod  6  3)").unwrap(),  "0");
}

#[test]
fn mod_float_path() {
    let mut s = fresh_session_with_numbers();
    // (mod 10.0 3.0) ≈ 1.0.
    let out = s.eval("(mod 10.0 3.0)").unwrap();
    assert!(out.starts_with("1."), "got: {out}");
}

#[test]
fn mod_ratio_path() {
    let mut s = fresh_session_with_numbers();
    // (mod 7/3 1) = 1/3 (floor(7/3)=2; 7/3 - 2 = 1/3)
    assert_eq!(s.eval("(mod 7/3 1)").unwrap(), "1/3");
}

// ── REM ──────────────────────────────────────────────────────────────

#[test]
fn rem_integer_sign_of_dividend() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(rem 10 3)").unwrap(),   "1");
    assert_eq!(s.eval("(rem -7  2)").unwrap(), "-1"); // sign of -7
    assert_eq!(s.eval("(rem  7 -2)").unwrap(),  "1"); // sign of 7
    assert_eq!(s.eval("(rem -7 -2)").unwrap(), "-1");
    assert_eq!(s.eval("(rem  6  3)").unwrap(),  "0");
}

#[test]
fn rem_float_path() {
    let mut s = fresh_session_with_numbers();
    let out = s.eval("(rem 10.0 3.0)").unwrap();
    assert!(out.starts_with("1."), "got: {out}");
}

#[test]
fn rem_ratio_path() {
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(rem 7/3 1)").unwrap(), "1/3");
    assert_eq!(s.eval("(rem -7/3 1)").unwrap(), "-1/3");
}

// ── Regression: core callers still work after redefinition ───────────

#[test]
fn evenp_oddp_still_work() {
    // evenp / oddp use (rem n 2) internally — must not break.
    let mut s = fresh_session_with_numbers();
    assert_eq!(s.eval("(evenp 4)").unwrap(), "T");
    assert_eq!(s.eval("(oddp 7)").unwrap(),  "T");
    assert_eq!(s.eval("(evenp 3)").unwrap(), "nil");
}

#[test]
fn bignum_truncate_and_rem() {
    // Ensure the shims reach into bignum territory.
    let mut s = fresh_session_with_numbers();
    let big = "100000000000000000000"; // > i64
    assert_eq!(
        s.eval(&format!("(truncate {big} 3)")).unwrap(),
        "33333333333333333333"
    );
    assert_eq!(
        s.eval(&format!("(rem {big} 3)")).unwrap(),
        "1"
    );
}

// ── FFLOOR / FCEILING / FTRUNCATE / FROUND ──────────────────────────

#[test]
fn ffloor_returns_float_quotient() {
    let mut s = fresh_session_with_numbers();
    // Quotient must be a float; remainder is the natural type (integer here).
    let out = s.eval("(multiple-value-list (ffloor 7 2))").unwrap();
    assert_eq!(out, "(3.0 1)");
}

#[test]
fn fceiling_returns_float_quotient() {
    let mut s = fresh_session_with_numbers();
    let out = s.eval("(multiple-value-list (fceiling 7 2))").unwrap();
    assert_eq!(out, "(4.0 -1)");
}

#[test]
fn ftruncate_returns_float_quotient() {
    let mut s = fresh_session_with_numbers();
    let out = s.eval("(multiple-value-list (ftruncate -7 2))").unwrap();
    assert_eq!(out, "(-3.0 -1)");
}

#[test]
fn fround_returns_float_quotient() {
    let mut s = fresh_session_with_numbers();
    // (round 7 2) = 4 (banker's rounding: 3.5 → even = 4)
    let out = s.eval("(multiple-value-list (fround 7 2))").unwrap();
    assert_eq!(out, "(4.0 -1)");
}

#[test]
fn ffloor_float_input() {
    let mut s = fresh_session_with_numbers();
    // (ffloor 3.7) — single-arg form, divisor defaults to 1.
    let out = s.eval("(multiple-value-list (ffloor 3.7))").unwrap();
    // quotient = 3.0, remainder = 3.7 - 3.0*1 = 0.7
    assert!(out.starts_with("(3.0 0."), "got: {out}");
}

#[test]
fn fceiling_and_ftruncate() {
    let mut s = fresh_session_with_numbers();
    // (fceiling -3.2) — quotient rounds toward +inf → -3.0, rem = -3.2 + 3.0 = -0.2
    let fc = s.eval("(car (multiple-value-list (fceiling -3.2)))").unwrap();
    assert_eq!(fc, "-3.0");
    // (ftruncate -7.5 2) — truncate-float(-7.5/2) = truncate-float(-3.75) = -3.0
    let ft = s.eval("(car (multiple-value-list (ftruncate -7.5 2)))").unwrap();
    assert_eq!(ft, "-3.0");
}

// ── Inverse hyperbolic functions ─────────────────────────────────────

#[test]
fn inverse_hyperbolic_roundtrip() {
    let mut s = fresh_session_with_numbers();
    // asinh(sinh(x)) ≈ x  for x in a safe range.
    s.eval("(defun approx= (a b) (< (abs (- a b)) 0.0001))").unwrap();
    assert_eq!(s.eval("(approx= (asinh (sinh 1.5)) 1.5)").unwrap(), "T");
    assert_eq!(s.eval("(approx= (acosh (cosh 2.0)) 2.0)").unwrap(), "T");
    assert_eq!(s.eval("(approx= (atanh (tanh 0.5)) 0.5)").unwrap(), "T");
}
