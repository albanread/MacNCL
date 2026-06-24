//! Exact rationals — Tier 2.B.
//!
//! Storage: heap object tagged `Tag::Vector`, header type
//! `HeapType::Ratio`. Layout:
//!
//!   cell 0: HeapHeader { ty: Ratio, length_cells: 3 }
//!   cell 1: %RATIO marker symbol
//!   cell 2: numerator   (Word — fixnum or bignum, sign-carrier)
//!   cell 3: denominator (Word — fixnum or bignum, > 1 always)
//!
//! Both num and den are Words, so the GC scan path treats them as
//! ordinary live pointers — no special opaque-cell handling needed
//! (unlike bignum limbs or float bits).
//!
//! Arithmetic kernel: num-rational's `BigRational = Ratio<BigInt>`
//! handles add / sub / mul / div / gcd-reduction. We marshal
//! through `(num, den) → BigRational`, op, `BigRational → (num, den)`,
//! demote to integer if den == 1.
//!
//! Promotion lattice (Tier 2 numeric tower):
//!
//!   integer + integer       → integer
//!   integer + ratio         → rational  (ratio, may simplify to integer)
//!   ratio + ratio           → rational
//!   integer / integer       → rational  (exact via this module)
//!   anything + float        → float
//!
//! This module's *_promote functions are the runtime slow path
//! called by the JIT-lowered arithmetic when either operand is
//! a ratio, OR when integer division produces a non-integer.

use crate::bignum::{bigint_to_word, integer_to_bigint};
use crate::heap::{HeapHeader, HeapType};
use crate::mutator::MutatorState;
use crate::word::{Tag, Word};
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};

/// Cells in a ratio heap object: marker + numerator + denominator.
pub const RATIO_PAYLOAD_CELLS: u32 = 3;

/// True iff WORD is a heap-allocated ratio.
pub fn is_ratio(w: Word) -> bool {
    if w.tag() != Tag::Vector {
        return false;
    }
    let p = match w.as_ptr::<u64>(Tag::Vector) {
        Some(p) => p,
        None => return false,
    };
    let header = HeapHeader::from_raw(unsafe { *p });
    header.ty() == HeapType::Ratio
}

/// True iff WORD is any rational — integer or ratio.
pub fn is_rational(w: Word) -> bool {
    crate::bignum::is_integer(w) || is_ratio(w)
}

/// Read a ratio's numerator + denominator into a `BigRational`.
/// Caller must have verified `is_ratio(w)`.
pub fn ratio_to_bigrational(w: Word) -> BigRational {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("ratio is a vector");
    let num_w = Word::from_raw(unsafe { *p.add(2) });
    let den_w = Word::from_raw(unsafe { *p.add(3) });
    let num = integer_to_bigint(num_w).expect("ratio numerator must be an integer");
    let den = integer_to_bigint(den_w).expect("ratio denominator must be an integer");
    BigRational::new_raw(num, den) // already simplified — no recompute
}

/// Read any rational Word (integer or ratio) into a BigRational.
pub fn rational_to_bigrational(w: Word) -> Option<BigRational> {
    if is_ratio(w) {
        return Some(ratio_to_bigrational(w));
    }
    integer_to_bigint(w).map(|n| BigRational::from_integer(n))
}

/// Convert a BigRational back to a Word. If the denominator
/// simplifies to 1, returns an integer (fixnum or bignum) via
/// `bigint_to_word`. Otherwise allocates a fresh ratio heap object.
///
/// num-rational guarantees `new(num, den)` simplifies; we still
/// double-check `is_integer()` before allocating a ratio.
pub fn bigrational_to_word(m: &mut MutatorState, q: &BigRational) -> Word {
    if q.is_integer() {
        return bigint_to_word(m, q.numer());
    }
    alloc_ratio_raw(m, q.numer(), q.denom())
}

/// Allocate a fresh ratio heap object. Caller must have verified
/// that num and den are coprime AND den > 1 — go through
/// `bigrational_to_word` to be safe.
fn alloc_ratio_raw(m: &mut MutatorState, num: &BigInt, den: &BigInt) -> Word {
    let num_w = bigint_to_word(m, num);
    let den_w = bigint_to_word(m, den);
    let marker = m.coord().marker_ratio();
    let w = m.alloc_typed_vector(HeapType::Ratio, RATIO_PAYLOAD_CELLS);
    let p = w.as_mut_ptr::<u64>(Tag::Vector).expect("just-allocated vector");
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = num_w.raw();
        *p.add(3) = den_w.raw();
    }
    w
}

/// Allocate a ratio in the STATIC area. Used by the compiler for
/// embedded literal ratio constants (e.g. `3/4` in user source).
pub fn alloc_ratio_in_static(
    static_area: &crate::static_area::StaticArea,
    coord: &crate::mutator::GcCoordinator,
    num_str: &str,
    den_str: &str,
) -> Option<Word> {
    let num: BigInt = num_str.parse().ok()?;
    let den: BigInt = den_str.parse().ok()?;
    if den.is_zero() {
        return None;
    }
    // Simplify at compile time.
    let q = BigRational::new(num, den);
    if q.is_integer() {
        // Integer literal masquerading as a ratio — use the
        // bignum static-area allocator. (3/1 reads as 3.)
        return Some(
            crate::bignum::alloc_bignum_in_static(
                static_area, coord, &q.numer().to_string(),
            )
            .unwrap_or(Word::fixnum(
                num_traits::ToPrimitive::to_i64(q.numer()).unwrap_or(0),
            )),
        );
    }
    // True ratio. Allocate the numerator and denominator in
    // static (their lifetimes match the literal's).
    let num_w = crate::bignum::alloc_bignum_in_static(
        static_area, coord, &q.numer().to_string(),
    )?;
    let den_w = crate::bignum::alloc_bignum_in_static(
        static_area, coord, &q.denom().to_string(),
    )?;
    let marker = coord.intern("%RATIO");
    let header_ptr =
        static_area.try_alloc_with_header(HeapType::Ratio, RATIO_PAYLOAD_CELLS)?;
    let p = header_ptr.as_ptr() as *mut u64;
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = num_w.raw();
        *p.add(3) = den_w.raw();
    }
    Some(Word::from_ptr(p as *const u8, Tag::Vector))
}

/// Render a ratio as "num/den". Used by the printer.
pub fn ratio_to_string(w: Word) -> String {
    let q = ratio_to_bigrational(w);
    format!("{}/{}", q.numer(), q.denom())
}

/// Accessors for Lisp-level `numerator` / `denominator`.
pub fn ratio_numerator(w: Word) -> Word {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("ratio is a vector");
    Word::from_raw(unsafe { *p.add(2) })
}

pub fn ratio_denominator(w: Word) -> Word {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("ratio is a vector");
    Word::from_raw(unsafe { *p.add(3) })
}

// ─── ABI helpers — the full numeric-tower promote path ──────────────────────
//
// These replace the previous {float,bignum}::ncl_*_promote pairs on
// the JIT's slow path. The full lattice:
//
//   any operand is float       → coerce both to f64, return float
//   any operand is ratio       → build BigRationals, op via num-rational,
//                                 return ratio (or integer if simplified)
//   both operands are integer  → delegate to bignum::ncl_*_promote
//
// We keep the {float,bignum} versions around so internal callers
// (and the `float` shim path) can still use them, but the LLVM
// register_runtime_helpers now points the slow paths here.

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_add_full(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if crate::float::is_float(a) || crate::float::is_float(b) {
        return crate::float::ncl_add_float(mutator, a_raw, b_raw);
    }
    if is_ratio(a) || is_ratio(b) {
        let m = unsafe { &mut *mutator };
        let qa = match rational_to_bigrational(a) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "+: non-numeric argument",
            ),
        };
        let qb = match rational_to_bigrational(b) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "+: non-numeric argument",
            ),
        };
        return bigrational_to_word(m, &(qa + qb)).raw();
    }
    crate::bignum::ncl_add_promote(mutator, a_raw, b_raw)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_sub_full(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if crate::float::is_float(a) || crate::float::is_float(b) {
        return crate::float::ncl_sub_float(mutator, a_raw, b_raw);
    }
    if is_ratio(a) || is_ratio(b) {
        let m = unsafe { &mut *mutator };
        let qa = match rational_to_bigrational(a) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "-: non-numeric argument",
            ),
        };
        let qb = match rational_to_bigrational(b) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "-: non-numeric argument",
            ),
        };
        return bigrational_to_word(m, &(qa - qb)).raw();
    }
    crate::bignum::ncl_sub_promote(mutator, a_raw, b_raw)
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_mul_full(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if crate::float::is_float(a) || crate::float::is_float(b) {
        return crate::float::ncl_mul_float(mutator, a_raw, b_raw);
    }
    if is_ratio(a) || is_ratio(b) {
        let m = unsafe { &mut *mutator };
        let qa = match rational_to_bigrational(a) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "*: non-numeric argument",
            ),
        };
        let qb = match rational_to_bigrational(b) {
            Some(q) => q,
            None => return crate::abi::signal_condition_string(
                mutator, "*: non-numeric argument",
            ),
        };
        return bigrational_to_word(m, &(qa * qb)).raw();
    }
    crate::bignum::ncl_mul_promote(mutator, a_raw, b_raw)
}

/// `(/ a b)` — true division across the numeric tower.
///   float-anywhere → float
///   integer / integer (exact)    → integer
///   integer / integer (inexact)  → ratio
///   ratio / anything             → ratio (or integer if simplified)
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_div_full(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let m = unsafe { &mut *mutator };
    if crate::float::is_float(a) || crate::float::is_float(b) {
        return crate::float::ncl_div_promote(mutator, a_raw, b_raw);
    }
    let qa = match rational_to_bigrational(a) {
        Some(q) => q,
        None => return crate::abi::signal_condition_string(
            mutator, "/: non-numeric argument",
        ),
    };
    let qb = match rational_to_bigrational(b) {
        Some(q) => q,
        None => return crate::abi::signal_condition_string(
            mutator, "/: non-numeric argument",
        ),
    };
    if qb.is_zero() {
        return crate::abi::signal_condition_string(mutator, "/: division by zero");
    }
    bigrational_to_word(m, &(qa / qb)).raw()
}

/// Decompose any number into (realpart, imagpart) as raw Words. A
/// real has itself as the realpart and fixnum 0 as the imaginary
/// part; a complex returns its stored components.
fn complex_parts(w: Word) -> (u64, u64) {
    if crate::complex::is_complex(w) {
        (crate::complex::complex_real(w).raw(), crate::complex::complex_imag(w).raw())
    } else {
        (w.raw(), Word::fixnum(0).raw())
    }
}

/// Heap-numeric type of W: `Some(Float | Bignum | Ratio | Complex)`
/// when W is a boxed number, `None` otherwise (fixnums included —
/// they're immediate). One tag decode + at most one header load.
///
/// The hot comparison/eql paths dispatch on this instead of
/// chaining `is_float` / `is_bignum` / `is_ratio` / `is_complex`,
/// each of which re-decoded the tag AND re-loaded the heap header.
#[inline]
pub(crate) fn heap_numeric_type(w: Word) -> Option<HeapType> {
    if w.tag() != Tag::Vector {
        return None;
    }
    let p = w.as_ptr::<u64>(Tag::Vector)?;
    match HeapHeader::from_raw(unsafe { *p }).ty() {
        t @ (HeapType::Float
        | HeapType::Bignum
        | HeapType::Ratio
        | HeapType::Complex) => Some(t),
        _ => None,
    }
}

/// Cross-type comparison spanning the full numeric tower. Returns
/// -1, 0, or +1 as i64.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_cmp_full(a_raw: u64, b_raw: u64) -> i64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    cmp_full_typed(a, b, heap_numeric_type(a), heap_numeric_type(b))
}

/// `ncl_cmp_full` body with the operands' heap-numeric types
/// pre-decoded, so callers that already know them (`eql_values`)
/// don't pay a second round of tag/header reads.
pub(crate) fn cmp_full_typed(
    a: Word,
    b: Word,
    ha: Option<HeapType>,
    hb: Option<HeapType>,
) -> i64 {
    let a_raw = a.raw();
    let b_raw = b.raw();
    // Complex numbers are not ordered — only (in)equality is defined.
    // Return 0 iff the real AND imaginary parts both compare equal,
    // else a nonzero sentinel. This makes `=` / `/=` correct on complex
    // (and on complex-vs-real, where the real has a zero imaginary
    // part). `<` / `>` on a complex are undefined in CL; we do not make
    // them meaningful here.
    if ha == Some(HeapType::Complex) || hb == Some(HeapType::Complex) {
        let (ar, ai) = complex_parts(a);
        let (br, bi) = complex_parts(b);
        return if ncl_cmp_full(ar, br) == 0 && ncl_cmp_full(ai, bi) == 0 {
            0
        } else {
            1
        };
    }
    if ha == Some(HeapType::Float) || hb == Some(HeapType::Float) {
        return crate::float::ncl_cmp_real(a_raw, b_raw);
    }
    // Both rational — compare via BigRational. Cheaper than going
    // through f64 (no rounding) and exact.
    let qa = match rational_to_bigrational(a) {
        Some(q) => q,
        None => return crate::float::ncl_cmp_real(a_raw, b_raw),
    };
    let qb = match rational_to_bigrational(b) {
        Some(q) => q,
        None => return crate::float::ncl_cmp_real(a_raw, b_raw),
    };
    use std::cmp::Ordering::*;
    match qa.cmp(&qb) {
        Less => -1,
        Greater => 1,
        Equal => 0,
    }
}

// ─── Lisp-callable shims ────────────────────────────────────────────────────

pub extern "C-unwind" fn ratiop_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let w = Word::from_raw(unsafe { *args });
    if is_ratio(w) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn rationalp_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let w = Word::from_raw(unsafe { *args });
    if is_rational(w) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn numerator_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "numerator: expected 1 arg");
    }
    let w = Word::from_raw(unsafe { *args });
    if is_ratio(w) {
        return ratio_numerator(w).raw();
    }
    if crate::bignum::is_integer(w) {
        // Integer numerator is the integer itself.
        return w.raw();
    }
    crate::abi::signal_condition_string(mutator, "numerator: not a rational")
}

pub extern "C-unwind" fn denominator_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "denominator: expected 1 arg",
        );
    }
    let w = Word::from_raw(unsafe { *args });
    if is_ratio(w) {
        return ratio_denominator(w).raw();
    }
    if crate::bignum::is_integer(w) {
        // Integer denominator is 1.
        return Word::fixnum(1).raw();
    }
    crate::abi::signal_condition_string(mutator, "denominator: not a rational")
}

/// `(rational x)` — coerce a real to a rational. Float input gets
/// converted exactly (no decimal-rounding loss; the resulting ratio
/// can have a very large denominator). Integer/ratio pass through.
pub extern "C-unwind" fn rational_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_traits::FromPrimitive;
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "rational: expected 1 arg",
        );
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    if is_rational(w) {
        return w.raw();
    }
    if crate::float::is_float(w) {
        let f = crate::float::float_value(w);
        if !f.is_finite() {
            return crate::abi::signal_condition_string(
                mutator, "rational: non-finite float",
            );
        }
        let q = BigRational::from_f64(f).unwrap_or_else(|| BigRational::zero());
        return bigrational_to_word(m, &q).raw();
    }
    crate::abi::signal_condition_string(mutator, "rational: not a real number")
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
    fn integer_demotion() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // 6/3 → 2 (integer)
        let q = BigRational::new(BigInt::from(6), BigInt::from(3));
        let w = bigrational_to_word(&mut m, &q);
        assert!(!is_ratio(w));
        assert_eq!(w.as_fixnum(), Some(2));
    }

    #[test]
    fn ratio_round_trip() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let q = BigRational::new(BigInt::from(22), BigInt::from(7));
        let w = bigrational_to_word(&mut m, &q);
        assert!(is_ratio(w));
        assert_eq!(ratio_to_bigrational(w), q);
        assert_eq!(ratio_to_string(w), "22/7");
    }

    #[test]
    fn negative_ratio_normalises_sign() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // -1/2 stored as numerator=-1, den=+1 ... wait: BigRational
        // normalises sign to numerator.
        let q = BigRational::new(BigInt::from(-1), BigInt::from(2));
        let w = bigrational_to_word(&mut m, &q);
        assert!(is_ratio(w));
        assert_eq!(ratio_to_string(w), "-1/2");
    }
}
