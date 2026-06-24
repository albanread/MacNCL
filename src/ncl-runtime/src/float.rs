//! IEEE 754 double-precision floats — Tier 2.A.
//!
//! Storage: a heap object tagged `Tag::Vector` with header type
//! `HeapType::Float`. Layout:
//!
//!   cell 0: HeapHeader { ty: Float, length_cells: 2 }
//!   cell 1: %FLOAT marker symbol
//!   cell 2: raw f64 bits (transmute)
//!
//! Arithmetic kernel: Rust's native f64 (which delegates to the
//! host FPU). For ops the host doesn't supply we use Rust's
//! `f64::sin/cos/log/exp/sqrt/...` — those go to libm on
//! reasonable platforms.
//!
//! Printing: ryu — shortest-round-trip decimal output, the
//! "nightmare algorithm" replacement (Adams 2018; sidesteps
//! Dragon4 / Grisu).

use crate::bignum::integer_to_bigint;
use crate::heap::{HeapHeader, HeapType};
use crate::mutator::MutatorState;
use crate::word::{Tag, Word};
use num_traits::ToPrimitive;

/// Cells in a float heap object: header + marker + f64 bits.
pub const FLOAT_PAYLOAD_CELLS: u32 = 2;

/// Allocate a float on the young heap.
pub fn alloc_float(m: &mut MutatorState, value: f64) -> Word {
    let marker = m.coord().marker_float();
    let w = m.alloc_typed_vector(HeapType::Float, FLOAT_PAYLOAD_CELLS);
    let p = w.as_mut_ptr::<u64>(Tag::Vector).expect("just-allocated vector");
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = value.to_bits();
    }
    w
}

/// Allocate a float in the STATIC area. Used by the compiler for
/// embedded literal float constants.
pub fn alloc_float_in_static(
    static_area: &crate::static_area::StaticArea,
    coord: &crate::mutator::GcCoordinator,
    value: f64,
) -> Option<Word> {
    let header_ptr =
        static_area.try_alloc_with_header(HeapType::Float, FLOAT_PAYLOAD_CELLS)?;
    let p = header_ptr.as_ptr() as *mut u64;
    let marker = coord.intern("%FLOAT");
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = value.to_bits();
    }
    Some(Word::from_ptr(p as *const u8, Tag::Vector))
}

/// Box a raw `f64` into a heap `Float` Word. The JIT's
/// `coerce_to_word(F64)` boundary calls this at an *escape* — when an
/// unboxed double must become a tagged Word (stored into a cons/vector,
/// returned, passed to a generic call, …). The result is a young-heap
/// allocation; it is kept alive across any later GC by the conservative
/// stack pin while it lives in a JIT register, exactly like every other
/// freshly-allocated intermediate. See `docs/performance-unbox-float.md`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_box_float(mutator: *mut MutatorState, value: f64) -> u64 {
    let m = unsafe { &mut *mutator };
    alloc_float(m, value).raw()
}

/// Checked slow path for the JIT's `coerce_to_f64(Word)` boundary. The
/// JIT inlines the fast path (the Word *is* a heap `Float`, read cell 2)
/// and calls this ONLY when the Word is not a float — e.g. a
/// `(declare (double-float x))` parameter that received a non-float at
/// runtime. Coerces any real (fixnum / bignum / ratio → nearest double)
/// and signals a condition for a non-real, instead of dereferencing a
/// non-pointer (which segfaults — that was the bug). On the signalled
/// path it returns NaN; the pending condition is taken at the caller's
/// next abort check.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_unbox_float_checked(mutator: *mut MutatorState, w_raw: u64) -> f64 {
    match to_f64(Word::from_raw(w_raw)) {
        Some(f) => f,
        None => {
            crate::abi::signal_condition_string(
                mutator,
                "double-float-declared value is not a real number",
            );
            f64::NAN
        }
    }
}

/// True iff WORD is a heap-allocated float.
pub fn is_float(w: Word) -> bool {
    if w.tag() != Tag::Vector {
        return false;
    }
    let p = match w.as_ptr::<u64>(Tag::Vector) {
        Some(p) => p,
        None => return false,
    };
    let header = HeapHeader::from_raw(unsafe { *p });
    header.ty() == HeapType::Float
}

/// True iff WORD is any kind of real number: fixnum, bignum, or float.
pub fn is_real(w: Word) -> bool {
    crate::bignum::is_integer(w) || is_float(w)
}

/// Read the raw f64 value from a heap-allocated float. Caller must
/// have verified `is_float(w)`.
pub fn float_value(w: Word) -> f64 {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("float is a vector");
    f64::from_bits(unsafe { *p.add(2) })
}

/// Convert any real-number Word to f64. Returns None for non-real
/// Words. Fixnums and bignums round to nearest f64 — same behaviour
/// as `as f64` casts. Ratios are converted via num-bigint's BigRational
/// to_f64 — rounds to nearest representable double.
pub fn to_f64(w: Word) -> Option<f64> {
    if is_float(w) {
        return Some(float_value(w));
    }
    if let Some(n) = w.as_fixnum() {
        return Some(n as f64);
    }
    if crate::bignum::is_bignum(w) {
        return integer_to_bigint(w).and_then(|n| n.to_f64());
    }
    if crate::ratio::is_ratio(w) {
        return crate::ratio::ratio_to_bigrational(w).to_f64();
    }
    None
}

/// Format a float as the shortest decimal string that round-trips
/// through f64 parsing. Used by the printer's ~A / ~S / ~F paths.
pub fn float_to_string(f: f64) -> String {
    // ryu's special-case handling: NaN, infinity. We render in
    // CL-style for those.
    if f.is_nan() {
        return "#<NaN>".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "#<infinity>".to_string()
        } else {
            "#<-infinity>".to_string()
        };
    }
    // ryu prints "0.0" and "-0.0" with a trailing ".0" already.
    let mut buf = ryu::Buffer::new();
    let s = buf.format(f).to_string();
    // ryu may print "1e10" for very large; we want CL's "1.0e10"
    // style (digit before the decimal point + at least one after).
    // For simple cases ryu already emits "1.0" not "1." so we're
    // mostly fine; the exponent normalisation is the only fixup.
    // Leave as-is for now — CL doesn't require any particular
    // exponent style.
    s
}

// ─── ABI helpers — promotion at type mismatch ─────────────────────────────

/// `(+ a b)` slow path that knows about floats. Called when the
/// JIT-inlined fixnum fast path can't handle the operand types
/// AND at least one operand is a float (otherwise the bignum
/// path in bignum.rs handles it).
///
/// If neither operand is real, falls through to bignum's add
/// promote, which signals on non-integers.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_add_float(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let m = unsafe { &mut *mutator };
    match (to_f64(a), to_f64(b)) {
        (Some(fa), Some(fb)) => {
            // If both are exact integers and neither is a float, the
            // bignum path is correct (preserves exactness). Only
            // route through float when at least one IS a float.
            if !is_float(a) && !is_float(b) {
                return crate::bignum::ncl_add_promote(mutator, a_raw, b_raw);
            }
            alloc_float(m, fa + fb).raw()
        }
        _ => crate::bignum::ncl_add_promote(mutator, a_raw, b_raw),
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_sub_float(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let m = unsafe { &mut *mutator };
    match (to_f64(a), to_f64(b)) {
        (Some(fa), Some(fb)) => {
            if !is_float(a) && !is_float(b) {
                return crate::bignum::ncl_sub_promote(mutator, a_raw, b_raw);
            }
            alloc_float(m, fa - fb).raw()
        }
        _ => crate::bignum::ncl_sub_promote(mutator, a_raw, b_raw),
    }
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_mul_float(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let m = unsafe { &mut *mutator };
    match (to_f64(a), to_f64(b)) {
        (Some(fa), Some(fb)) => {
            if !is_float(a) && !is_float(b) {
                return crate::bignum::ncl_mul_promote(mutator, a_raw, b_raw);
            }
            alloc_float(m, fa * fb).raw()
        }
        _ => crate::bignum::ncl_mul_promote(mutator, a_raw, b_raw),
    }
}

/// `(/ a b)` — true division. With at least one float operand,
/// returns a float quotient. With two integers, returns the
/// truncated-toward-zero quotient as an integer (matching `truncate`
/// for now — proper ratios land with Tier 2.B).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_div_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let m = unsafe { &mut *mutator };
    let fb = to_f64(b);
    if fb == Some(0.0) && !is_float(b) {
        // integer 0
        return crate::abi::signal_condition_string(mutator, "division by zero");
    }
    match (to_f64(a), fb) {
        (Some(fa), Some(fb)) => {
            // If both are exact integers, do exact integer division
            // via the bignum path.
            if !is_float(a) && !is_float(b) {
                return crate::bignum::ncl_truncate_promote(mutator, a_raw, b_raw);
            }
            if fb == 0.0 {
                return crate::abi::signal_condition_string(mutator, "division by zero");
            }
            alloc_float(m, fa / fb).raw()
        }
        _ => crate::abi::signal_condition_string(mutator, "/: non-numeric argument"),
    }
}

/// Cross-type real comparison. Returns -1, 0, or +1 as i64. Used by
/// the bignum-aware `ncl_cmp_int` when either operand is a float;
/// for integer-only inputs the bignum path handles it directly.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_cmp_real(a_raw: u64, b_raw: u64) -> i64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    // Both floats — direct compare. (Total order, NaN handling
    // via partial_cmp + Ordering::Greater fallback so NaN sorts to
    // the end. CL doesn't define NaN ordering, but we have to
    // pick *something*.)
    if is_float(a) && is_float(b) {
        let fa = float_value(a);
        let fb = float_value(b);
        return match fa.partial_cmp(&fb) {
            Some(std::cmp::Ordering::Less) => -1,
            Some(std::cmp::Ordering::Greater) => 1,
            Some(std::cmp::Ordering::Equal) => 0,
            None => 0, // NaN — treat as equal for now
        };
    }
    // Mixed or both integers: coerce to f64 for comparison if at
    // least one is a float, otherwise defer to integer cmp.
    if is_float(a) || is_float(b) {
        let fa = to_f64(a).unwrap_or(f64::NAN);
        let fb = to_f64(b).unwrap_or(f64::NAN);
        return match fa.partial_cmp(&fb) {
            Some(std::cmp::Ordering::Less) => -1,
            Some(std::cmp::Ordering::Greater) => 1,
            _ => 0,
        };
    }
    crate::bignum::ncl_cmp_int(a_raw, b_raw)
}

/// True iff comparing A and B is IEEE-*unordered* — at least one operand
/// is a float NaN. CL's number comparisons (`<`, `>`, `<=`, `>=`, `=`)
/// must all return NIL when an operand is NaN, and `/=` must return T.
///
/// `ncl_cmp_real`/`ncl_cmp_full` collapse the unordered case to 0
/// ("equal") because they return a 3-way ordering, and `eql`/`equal`
/// rely on that tower compare. So we detect the unordered case
/// *separately* at the comparison-predicate sites (the abi.rs shims and
/// the JIT's `ncl_num_cmp`) rather than perturbing the shared compare.
pub fn cmp_unordered(a: Word, b: Word) -> bool {
    (is_float(a) && float_value(a).is_nan()) || (is_float(b) && float_value(b).is_nan())
}

/// Numeric-tower comparison for the JIT's inlined `<`/`>`/`<=`/`>=`/`=`
/// slow path. Identical to `ncl_cmp_full` for ordered operands; returns
/// the sentinel `i64::MIN` when the comparison is IEEE-unordered (a NaN
/// operand). The JIT maps that sentinel to "false" for every ordered
/// predicate (see `emit_cmp_vals`). `eql`/`equal`/`equalp` keep calling
/// `ncl_cmp_full` directly and are unaffected.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_num_cmp(a_raw: u64, b_raw: u64) -> i64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if cmp_unordered(a, b) {
        return i64::MIN;
    }
    crate::ratio::ncl_cmp_full(a_raw, b_raw)
}

// ─── Lisp-callable shims ─────────────────────────────────────────────────

/// `(/ a b)` — Lisp-callable shim wrapping the full-lattice
/// division. Integer / integer that doesn't divide evenly returns
/// a ratio (Tier 2.B); float-anywhere returns float; mixed
/// integer/ratio returns a ratio (or integer if simplified).
pub extern "C-unwind" fn div_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return crate::abi::signal_condition_string(mutator, "/: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    crate::complex::ncl_div_complex(mutator, a, b)
}

/// `(floatp x)` — T iff X is a float.
pub extern "C-unwind" fn floatp_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let w = Word::from_raw(unsafe { *args });
    if is_float(w) { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(float x)` — coerce a number to a float. If x is already a
/// float, returned as-is. Integer arguments get the standard f64
/// rounding.
pub extern "C-unwind" fn float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "float: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    match to_f64(w) {
        Some(f) => alloc_float(m, f).raw(),
        None => crate::abi::signal_condition_string(
            mutator, "float: argument is not a real number",
        ),
    }
}

/// Generate a unary float shim wrapping a standard f64 method.
macro_rules! unary_float_shim {
    ($name:ident, $method:ident, $err_name:literal) => {
        pub extern "C-unwind" fn $name(
            mutator: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            if n_args != 1 {
                return crate::abi::signal_condition_string(
                    mutator, concat!($err_name, ": expected 1 arg"),
                );
            }
            let m = unsafe { &mut *mutator };
            let w = Word::from_raw(unsafe { *args });
            match to_f64(w) {
                Some(f) => alloc_float(m, f.$method()).raw(),
                None => crate::abi::signal_condition_string(
                    mutator,
                    concat!($err_name, ": argument is not a real number"),
                ),
            }
        }
    };
}

unary_float_shim!(sqrt_shim,  sqrt,  "sqrt");
unary_float_shim!(sin_shim,   sin,   "sin");
unary_float_shim!(cos_shim,   cos,   "cos");
unary_float_shim!(tan_shim,   tan,   "tan");
unary_float_shim!(asin_shim,  asin,  "asin");
unary_float_shim!(acos_shim,  acos,  "acos");
unary_float_shim!(atan_shim,  atan,  "atan");
unary_float_shim!(sinh_shim,  sinh,  "sinh");
unary_float_shim!(cosh_shim,  cosh,  "cosh");
unary_float_shim!(tanh_shim,  tanh,  "tanh");
unary_float_shim!(exp_shim,   exp,   "exp");
unary_float_shim!(log_shim,   ln,    "log");      // CL `log` is natural log by default

/// `(log x [base])` — natural log if base omitted, log_base(x) otherwise.
pub extern "C-unwind" fn log_base_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    match n_args {
        1 => log_shim(mutator, 0, args, 1),
        2 => {
            let x = Word::from_raw(unsafe { *args });
            let b = Word::from_raw(unsafe { *args.add(1) });
            match (to_f64(x), to_f64(b)) {
                (Some(fx), Some(fb)) => alloc_float(m, fx.log(fb)).raw(),
                _ => crate::abi::signal_condition_string(
                    mutator, "log: non-numeric argument",
                ),
            }
        }
        _ => crate::abi::signal_condition_string(mutator, "log: expected 1 or 2 args"),
    }
}

/// `(atan y [x])` — atan(y) if x omitted, atan2(y, x) otherwise.
pub extern "C-unwind" fn atan_base_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    match n_args {
        1 => atan_shim(mutator, 0, args, 1),
        2 => {
            let y = Word::from_raw(unsafe { *args });
            let x = Word::from_raw(unsafe { *args.add(1) });
            match (to_f64(y), to_f64(x)) {
                (Some(fy), Some(fx)) => alloc_float(m, fy.atan2(fx)).raw(),
                _ => crate::abi::signal_condition_string(
                    mutator, "atan: non-numeric argument",
                ),
            }
        }
        _ => crate::abi::signal_condition_string(mutator, "atan: expected 1 or 2 args"),
    }
}

/// `(expt-float base power)` — float exponentiation. Distinct from
/// integer expt — this one handles non-integer powers and float bases.
pub extern "C-unwind" fn expt_float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return crate::abi::signal_condition_string(
            mutator, "expt-float: expected 2 args",
        );
    }
    let m = unsafe { &mut *mutator };
    let base = Word::from_raw(unsafe { *args });
    let pow = Word::from_raw(unsafe { *args.add(1) });
    match (to_f64(base), to_f64(pow)) {
        (Some(fb), Some(fp)) => alloc_float(m, fb.powf(fp)).raw(),
        _ => crate::abi::signal_condition_string(
            mutator, "expt-float: non-numeric argument",
        ),
    }
}

/// `(truncate-float x)` — round-toward-zero, returning an integer.
/// `(floor-float x)` — round toward -inf.
/// `(ceiling-float x)` — round toward +inf.
/// `(round-float x)` — round to nearest, ties to even.
///
/// These each take a single float and return an integer (fixnum or
/// bignum if the result is out of fixnum range).
pub extern "C-unwind" fn truncate_float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "truncate-float: expected 1 arg",
        );
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    match to_f64(w) {
        Some(f) => float_to_integer_word(m, f.trunc()),
        None => crate::abi::signal_condition_string(
            mutator, "truncate-float: not a real",
        ),
    }
}

pub extern "C-unwind" fn floor_float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "floor-float: expected 1 arg",
        );
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    match to_f64(w) {
        Some(f) => float_to_integer_word(m, f.floor()),
        None => crate::abi::signal_condition_string(
            mutator, "floor-float: not a real",
        ),
    }
}

pub extern "C-unwind" fn ceiling_float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "ceiling-float: expected 1 arg",
        );
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    match to_f64(w) {
        Some(f) => float_to_integer_word(m, f.ceil()),
        None => crate::abi::signal_condition_string(
            mutator, "ceiling-float: not a real",
        ),
    }
}

pub extern "C-unwind" fn round_float_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "round-float: expected 1 arg",
        );
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    match to_f64(w) {
        Some(f) => {
            // f64::round rounds half AWAY from zero. CL spec wants
            // round-half-to-even. Use round_ties_even (stable).
            float_to_integer_word(m, f.round_ties_even())
        }
        None => crate::abi::signal_condition_string(
            mutator, "round-float: not a real",
        ),
    }
}

/// Convert a possibly-integer-valued f64 to a Word (fixnum or bignum).
/// Used by the floor/ceiling/round/truncate shims.
fn float_to_integer_word(m: &mut MutatorState, f: f64) -> u64 {
    use num_bigint::BigInt;
    if !f.is_finite() {
        // Signal infinity/NaN as bignum-impossible.
        return crate::bignum::bigint_to_word(m, &BigInt::from(0)).raw();
    }
    // Try i64 first; fall back to BigInt for very large floats.
    if f >= i64::MIN as f64 && f <= i64::MAX as f64 {
        let n = f as i64;
        return crate::bignum::bigint_to_word(m, &BigInt::from(n)).raw();
    }
    // Out of i64 range — convert via BigInt::from_f64.
    use num_traits::FromPrimitive;
    let bi = BigInt::from_f64(f).unwrap_or_else(|| BigInt::from(0));
    crate::bignum::bigint_to_word(m, &bi).raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{GcConfig, GcCoordinator};

    fn small_config() -> GcConfig {
        GcConfig {
            young_bytes: 64 * 1024,
            old_bytes: 64 * 1024,
            static_bytes: 16 * 1024,
            tlab_cells: 256,
        }
    }

    #[test]
    fn round_trip_float() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let w = alloc_float(&mut m, 3.14);
        assert!(is_float(w));
        assert_eq!(float_value(w), 3.14);
    }

    #[test]
    fn ryu_prints_short() {
        assert_eq!(float_to_string(0.0), "0.0");
        assert_eq!(float_to_string(-0.0), "-0.0");
        assert_eq!(float_to_string(3.14), "3.14");
        assert_eq!(float_to_string(1.5), "1.5");
        // ryu picks shortest representation
        assert_eq!(float_to_string(0.1 + 0.2), "0.30000000000000004");
    }

    /// Locks the exact heap layout the JIT hardcodes when it inlines
    /// float unbox/box (`coerce_to_f64` / `coerce_to_word` in
    /// `ncl-llvm`). If any of these constants move, the JIT codegen must
    /// change in lockstep. See `docs/performance-unbox-float.md` §2.1.
    #[test]
    fn jit_float_layout_contract() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let w = alloc_float(&mut m, 1.5);

        // (1) A float Word carries Tag::Vector == 0b011 == 3.
        assert_eq!(w.raw() & 7, 3, "float Word tag must be Tag::Vector (3)");

        // (2) Clearing the tag bits yields the heap base (cell 0 = the
        //     header). HeapType::Float == 7 lives in the header's low 5
        //     bits (TYPE_SHIFT = 0, TYPE_MASK = 31).
        let base = (w.raw() & !7u64) as *const u64;
        let header = unsafe { *base };
        assert_eq!(header & 31, 7, "header low 5 bits must be HeapType::Float (7)");

        // (3) The raw f64 bits live at cell 2 == byte 16.
        let bits = unsafe { *base.add(2) };
        assert_eq!(f64::from_bits(bits), 1.5, "f64 payload at cell 2 (byte 16)");

        // (4) ncl_box_float produces the identical layout.
        let b = ncl_box_float(&mut m as *mut MutatorState, -0.0);
        assert_eq!(b & 7, 3);
        let bbase = (b & !7u64) as *const u64;
        assert_eq!(unsafe { *bbase } & 31, 7);
        let bbits = unsafe { *bbase.add(2) };
        // -0.0 round-trips bit-exactly (sign bit preserved).
        assert!(f64::from_bits(bbits) == 0.0 && f64::from_bits(bbits).is_sign_negative());
    }

    #[test]
    fn coerces_integers() {
        let coord = GcCoordinator::new(small_config());
        let _m = coord.register_mutator();
        assert_eq!(to_f64(Word::fixnum(42)), Some(42.0));
        assert_eq!(to_f64(Word::fixnum(-7)), Some(-7.0));
    }
}
