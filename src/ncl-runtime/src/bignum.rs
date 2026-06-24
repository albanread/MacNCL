//! Arbitrary-precision integers — Tier 1.D.
//!
//! Storage: a heap object tagged `Tag::Vector` with header type
//! `HeapType::Bignum`. Layout:
//!
//!   cell 0: HeapHeader { ty: Bignum, length_cells: 4 + n_limbs }
//!   cell 1: marker — the symbol `%BIGNUM` (printer + typep recogniser)
//!   cell 2: sign — fixnum +1 or -1
//!   cell 3: n_limbs — fixnum
//!   cell 4: reserved (cached fixnum-equivalent / hash)
//!   cell 5..5+n_limbs: raw u64 limbs, little-endian magnitude
//!
//! Arithmetic kernel: we delegate to `num_bigint::BigInt`. On every
//! operation we marshal limbs in (one allocation per operand), do
//! the arithmetic, marshal the result back into a fresh GC heap
//! object. No persistent Rust-side allocations means the GC owns
//! 100% of bignum memory and never needs finalisers.
//!
//! Normalisation: every result is run through `bigint_to_word`
//! which demotes to fixnum if the magnitude fits in 61-bit signed.

use crate::heap::{HeapHeader, HeapType};
use crate::mutator::MutatorState;
use crate::word::{Tag, Word, FIXNUM_MAX, FIXNUM_MIN};
use num_bigint::{BigInt, Sign};
use num_traits::{Signed, Zero};

/// Cells before the limb data: header + marker + sign + n_limbs + reserved.
pub const BIGNUM_HEADER_CELLS: usize = 5;

/// Allocate a bignum heap object holding the given little-endian
/// magnitude limbs and sign. NEVER call this directly with a value
/// that fits in fixnum range — go through `bigint_to_word`, which
/// demotes correctly.
fn alloc_bignum_raw(m: &mut MutatorState, sign: i8, limbs: &[u64]) -> Word {
    debug_assert!(sign == 1 || sign == -1);
    debug_assert!(!limbs.is_empty());
    debug_assert!(*limbs.last().unwrap() != 0);

    let total_cells = BIGNUM_HEADER_CELLS + limbs.len();
    let bignum_marker = m.coord().marker_bignum();

    // alloc_vector_with_header writes the heap header and returns a
    // Vector-tagged Word; we then fill in the rest of the cells.
    let w = m.alloc_typed_vector(HeapType::Bignum, total_cells as u32);
    let p = w.as_mut_ptr::<u64>(Tag::Vector).expect("just-allocated vector");

    unsafe {
        // p[0] = header (already written by alloc_typed_vector)
        *p.add(1) = bignum_marker.raw();
        *p.add(2) = Word::fixnum(sign as i64).raw();
        *p.add(3) = Word::fixnum(limbs.len() as i64).raw();
        *p.add(4) = Word::NIL.raw(); // reserved
        for (i, &limb) in limbs.iter().enumerate() {
            *p.add(BIGNUM_HEADER_CELLS + i) = limb;
        }
    }
    w
}

/// True iff WORD is a bignum heap object.
pub fn is_bignum(w: Word) -> bool {
    if w.tag() != Tag::Vector {
        return false;
    }
    let p = match w.as_ptr::<u64>(Tag::Vector) {
        Some(p) => p,
        None => return false,
    };
    let header = HeapHeader::from_raw(unsafe { *p });
    header.ty() == HeapType::Bignum
}

/// True iff WORD is any kind of integer — fixnum or bignum.
pub fn is_integer(w: Word) -> bool {
    w.tag() == Tag::Fixnum || is_bignum(w)
}

/// Read a bignum's contents into a `BigInt`. Caller must have
/// already verified `is_bignum(w)`.
pub fn bignum_to_bigint(w: Word) -> BigInt {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("bignum is a vector");
    let sign_word = Word::from_raw(unsafe { *p.add(2) });
    let n_limbs_word = Word::from_raw(unsafe { *p.add(3) });
    let sign = sign_word.as_fixnum().expect("bignum sign is fixnum") as i8;
    let n_limbs = n_limbs_word.as_fixnum().expect("bignum n_limbs is fixnum") as usize;
    // num-bigint's BigUint::new accepts u32 little-endian limbs. Split
    // each u64 heap limb into its two u32 halves directly into the result
    // vec — no intermediate Vec<u64> copy of the limbs.
    let mut u32_limbs = Vec::with_capacity(n_limbs * 2);
    for i in 0..n_limbs {
        let limb = unsafe { *p.add(BIGNUM_HEADER_CELLS + i) };
        u32_limbs.push((limb & 0xFFFF_FFFF) as u32);
        u32_limbs.push((limb >> 32) as u32);
    }
    let mag = num_bigint::BigUint::new(u32_limbs);
    let s = if sign > 0 { Sign::Plus } else { Sign::Minus };
    if mag.is_zero() {
        BigInt::from(0)
    } else {
        BigInt::from_biguint(s, mag)
    }
}

/// Read any integer Word (fixnum or bignum) into a `BigInt`.
pub fn integer_to_bigint(w: Word) -> Option<BigInt> {
    if let Some(n) = w.as_fixnum() {
        return Some(BigInt::from(n));
    }
    if is_bignum(w) {
        return Some(bignum_to_bigint(w));
    }
    None
}

/// Convert a BigInt back to a Word, demoting to fixnum if the
/// magnitude fits. Allocates only if a real bignum is required.
pub fn bigint_to_word(m: &mut MutatorState, n: &BigInt) -> Word {
    // Fast path: try as i64. num-bigint's `to_i64` covers anything
    // within `i64::MIN..=i64::MAX`. We further restrict to fixnum
    // range (61-bit signed).
    if let Some(small) = num_traits::ToPrimitive::to_i64(n) {
        if small >= FIXNUM_MIN && small <= FIXNUM_MAX {
            return Word::fixnum(small);
        }
    }
    let sign: i8 = if n.is_negative() { -1 } else { 1 };
    let mag = n.magnitude();
    // Convert to u64 little-endian limbs.
    let u32_limbs: Vec<u32> = mag.to_u32_digits();
    let mut u64_limbs: Vec<u64> = Vec::with_capacity((u32_limbs.len() + 1) / 2);
    let mut i = 0;
    while i < u32_limbs.len() {
        let lo = u32_limbs[i] as u64;
        let hi = if i + 1 < u32_limbs.len() {
            u32_limbs[i + 1] as u64
        } else {
            0
        };
        u64_limbs.push((hi << 32) | lo);
        i += 2;
    }
    // Trim any trailing-zero limb introduced by the u32→u64 pack
    // (only happens when u32_limbs has odd length AND the high half
    // was zero — impossible since num-bigint's to_u32_digits doesn't
    // emit a leading zero. Defensive trim anyway.)
    while u64_limbs.len() > 1 && *u64_limbs.last().unwrap() == 0 {
        u64_limbs.pop();
    }
    if u64_limbs.is_empty() {
        return Word::fixnum(0);
    }
    alloc_bignum_raw(m, sign, &u64_limbs)
}

// ─── ABI helpers — promotion at fixnum overflow ────────────────────────────

/// `(+ a b)` slow path. Called from the JIT when the inline fixnum
/// add overflowed, OR when either operand is a bignum.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_add_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = match integer_to_bigint(a) {
        Some(n) => n,
        None => panic!("+: non-integer operand: {a:?}"),
    };
    let nb = match integer_to_bigint(b) {
        Some(n) => n,
        None => panic!("+: non-integer operand: {b:?}"),
    };
    bigint_to_word(m, &(na + nb)).raw()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_sub_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a).unwrap_or_else(|| panic!("-: non-integer: {a:?}"));
    let nb = integer_to_bigint(b).unwrap_or_else(|| panic!("-: non-integer: {b:?}"));
    bigint_to_word(m, &(na - nb)).raw()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_mul_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a).unwrap_or_else(|| panic!("*: non-integer: {a:?}"));
    let nb = integer_to_bigint(b).unwrap_or_else(|| panic!("*: non-integer: {b:?}"));
    bigint_to_word(m, &(na * nb)).raw()
}

/// Cross-type integer comparison. Returns -1, 0, or +1 (as i64).
/// Called when the inline both-fixnum compare path can't handle
/// the operand types.
///
/// Permissive on non-integer arguments: pointer-bits comparison
/// (raw word value, signed) so e.g. `(eq sym1 sym2)`-style tests
/// via `=` still work the way they did under the pre-bignum
/// inline-compare. Strict integer-only comparison would break a
/// lot of pre-existing library code that uses `=` opportunistically
/// on tagged words.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_cmp_int(
    a_raw: u64,
    b_raw: u64,
) -> i64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a);
    let nb = integer_to_bigint(b);
    use std::cmp::Ordering::*;
    match (na, nb) {
        (Some(na), Some(nb)) => match na.cmp(&nb) {
            Less => -1,
            Equal => 0,
            Greater => 1,
        },
        _ => {
            // Fallback: signed compare of raw words. Matches the
            // pre-bignum inline behaviour. The only reason this
            // path fires today is that `(= sym1 sym2)` and similar
            // shapes show up in library code; treating them like
            // raw-word eq via cmp keeps the legacy idiom working.
            let ai = a_raw as i64;
            let bi = b_raw as i64;
            if ai < bi { -1 } else if ai > bi { 1 } else { 0 }
        }
    }
}

/// Render a bignum as a base-10 decimal string. Used by the
/// printer.
pub fn bignum_to_decimal(w: Word) -> String {
    bignum_to_bigint(w).to_str_radix(10)
}

/// Allocate a bignum in the STATIC area from a decimal-string
/// representation. Used by the compiler to embed literal bignum
/// constants — same lifetime as string literals.
///
/// Returns `None` if the string can't be parsed as an integer or
/// the static area is exhausted.
pub fn alloc_bignum_in_static(
    static_area: &crate::static_area::StaticArea,
    coord: &crate::mutator::GcCoordinator,
    decimal: &str,
) -> Option<Word> {
    use num_traits::{Signed, ToPrimitive};
    let n: BigInt = decimal.parse().ok()?;

    // If it fits in fixnum, just return that — no static alloc needed.
    if let Some(small) = n.to_i64() {
        if small >= FIXNUM_MIN && small <= FIXNUM_MAX {
            return Some(Word::fixnum(small));
        }
    }

    let sign: i8 = if n.is_negative() { -1 } else { 1 };
    let mag = n.magnitude();
    let u32_limbs: Vec<u32> = mag.to_u32_digits();
    let mut u64_limbs: Vec<u64> = Vec::with_capacity((u32_limbs.len() + 1) / 2);
    let mut i = 0;
    while i < u32_limbs.len() {
        let lo = u32_limbs[i] as u64;
        let hi = if i + 1 < u32_limbs.len() {
            u32_limbs[i + 1] as u64
        } else {
            0
        };
        u64_limbs.push((hi << 32) | lo);
        i += 2;
    }
    while u64_limbs.len() > 1 && *u64_limbs.last().unwrap() == 0 {
        u64_limbs.pop();
    }
    if u64_limbs.is_empty() {
        return Some(Word::fixnum(0));
    }

    let total_cells = BIGNUM_HEADER_CELLS + u64_limbs.len();
    let header_ptr = static_area.try_alloc_with_header(
        HeapType::Bignum,
        total_cells as u32,
    )?;
    // The header lives at the allocation's first cell. The Word
    // tag bits get added by Word::from_ptr.
    let p = header_ptr.as_ptr() as *mut u64;
    let marker = coord.intern("%BIGNUM");
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = Word::fixnum(sign as i64).raw();
        *p.add(3) = Word::fixnum(u64_limbs.len() as i64).raw();
        *p.add(4) = Word::NIL.raw();
        for (i, &limb) in u64_limbs.iter().enumerate() {
            *p.add(BIGNUM_HEADER_CELLS + i) = limb;
        }
    }
    Some(Word::from_ptr(p as *const u8, Tag::Vector))
}

// ─── D.2: division, remainder, gcd, expt ────────────────────────────────

/// Truncate (round-toward-zero) integer division. Returns the
/// quotient as a Word. Called by the JIT slow path when either
/// operand is a bignum.
///
/// Signals a Lisp error on division by zero (via the error_shim
/// pathway) — the JIT slow path will then unwind through the
/// abort-pending mechanism.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_truncate_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a)
        .unwrap_or_else(|| panic!("truncate: non-integer: {a:?}"));
    let nb = integer_to_bigint(b)
        .unwrap_or_else(|| panic!("truncate: non-integer: {b:?}"));
    if nb.is_zero() {
        return crate::abi::signal_condition_string(mutator, "division by zero");
    }
    // num-bigint::BigInt::Div is truncated-toward-zero, matching
    // CL's truncate semantics.
    bigint_to_word(m, &(na / nb)).raw()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_rem_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a)
        .unwrap_or_else(|| panic!("rem: non-integer: {a:?}"));
    let nb = integer_to_bigint(b)
        .unwrap_or_else(|| panic!("rem: non-integer: {b:?}"));
    if nb.is_zero() {
        return crate::abi::signal_condition_string(mutator, "division by zero");
    }
    bigint_to_word(m, &(na % nb)).raw()
}

/// Floor division — rounds toward negative infinity. Returns
/// quotient.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_floor_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    use num_integer::Integer;
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a)
        .unwrap_or_else(|| panic!("floor: non-integer: {a:?}"));
    let nb = integer_to_bigint(b)
        .unwrap_or_else(|| panic!("floor: non-integer: {b:?}"));
    if nb.is_zero() {
        return crate::abi::signal_condition_string(mutator, "division by zero");
    }
    bigint_to_word(m, &na.div_floor(&nb)).raw()
}

/// Floor remainder (mod). Always has the same sign as the divisor.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_mod_promote(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    use num_integer::Integer;
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    let na = integer_to_bigint(a)
        .unwrap_or_else(|| panic!("mod: non-integer: {a:?}"));
    let nb = integer_to_bigint(b)
        .unwrap_or_else(|| panic!("mod: non-integer: {b:?}"));
    if nb.is_zero() {
        return crate::abi::signal_condition_string(mutator, "division by zero");
    }
    bigint_to_word(m, &na.mod_floor(&nb)).raw()
}

/// 2-operand JIT slow-path helpers for the inline bitwise ops. The JIT
/// emits the both-fixnum fast path inline (a raw `and`/`or`/`xor` on the
/// tagged words — tag bits are 000 for fixnums, so the result is
/// correctly tagged) and calls these only when an operand is a bignum.
/// ABI mirrors `ncl_add_promote`: `(mutator, a, b) -> Word`.
fn bit_promote_2(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
    name: &str,
    op: impl Fn(BigInt, BigInt) -> BigInt,
) -> u64 {
    let na = match integer_to_bigint(Word::from_raw(a_raw)) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, &format!("{name}: non-integer argument"),
        ),
    };
    let nb = match integer_to_bigint(Word::from_raw(b_raw)) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, &format!("{name}: non-integer argument"),
        ),
    };
    let m = unsafe { &mut *mutator };
    bigint_to_word(m, &op(na, nb)).raw()
}

/// `(ash n shift)` slow path: 2-operand promote ABI. The JIT inlines the
/// common small-left-shift fixnum case; this handles bignum `n`, large
/// or negative shifts, and fixnum-overflow-to-bignum. Mirrors
/// `ash_shim`'s bigint logic.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_ash_promote(mutator: *mut MutatorState, n_raw: u64, shift_raw: u64) -> u64 {
    use num_traits::{Pow, Signed};
    use num_integer::Integer;
    let n = match integer_to_bigint(Word::from_raw(n_raw)) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "ash: first arg must be an integer"),
    };
    let shift = match Word::from_raw(shift_raw).as_fixnum() {
        Some(s) => s,
        None => return crate::abi::signal_condition_string(
            mutator, "ash: shift count must be a fixnum"),
    };
    let result = if shift >= 0 {
        if shift > u32::MAX as i64 {
            return crate::abi::signal_condition_string(mutator, "ash: shift count too large");
        }
        n << (shift as usize)
    } else {
        let s = (-shift) as usize;
        if n.is_negative() {
            let divisor: BigInt = BigInt::from(2).pow(s as u32);
            n.div_floor(&divisor)
        } else {
            n >> s
        }
    };
    let m = unsafe { &mut *mutator };
    bigint_to_word(m, &result).raw()
}

#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_logand_promote(m: *mut MutatorState, a: u64, b: u64) -> u64 {
    bit_promote_2(m, a, b, "logand", |x, y| x & y)
}
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_logior_promote(m: *mut MutatorState, a: u64, b: u64) -> u64 {
    bit_promote_2(m, a, b, "logior", |x, y| x | y)
}
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_logxor_promote(m: *mut MutatorState, a: u64, b: u64) -> u64 {
    bit_promote_2(m, a, b, "logxor", |x, y| x ^ y)
}

// ─── Lisp-callable math shims (registered via install_native) ────────────

/// `(truncate a b)` — integer truncating division. Quotient rounds
/// toward zero. Both operands must be integers (fixnum or bignum);
/// non-int operands signal a condition. The polymorphic wrapper in
/// `Lisp/Library/numbers.lisp` dispatches floats and ratios before
/// this shim sees them.
///
/// Returns single value (the quotient). The Lisp wrapper computes
/// the remainder via `(- a (* q b))` and packages both with `values`.
pub extern "C-unwind" fn truncate_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return crate::abi::signal_condition_string(
            mutator, "truncate: expected 2 args",
        );
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    // ncl_truncate_promote already accepts fixnum + bignum and
    // signals "division by zero" cleanly.
    ncl_truncate_promote(mutator, a, b)
}

/// `(rem a b)` — integer remainder paired with TRUNCATE (sign of
/// the dividend; LLVM srem semantics). Integer-only; the
/// polymorphic dispatcher lives in `Lisp/Library/numbers.lisp`.
pub extern "C-unwind" fn rem_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return crate::abi::signal_condition_string(
            mutator, "rem: expected 2 args",
        );
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    ncl_rem_promote(mutator, a, b)
}

/// `(gcd a b)` — non-negative greatest common divisor. Returns a
/// fixnum or bignum.
pub extern "C-unwind" fn gcd_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_integer::Integer;
    if n_args != 2 {
        return crate::abi::signal_condition_string(
            mutator, "gcd: expected 2 args",
        );
    }
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(unsafe { *args });
    let b = Word::from_raw(unsafe { *args.add(1) });
    let na = match integer_to_bigint(a) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "gcd: first arg is not an integer",
        ),
    };
    let nb = match integer_to_bigint(b) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "gcd: second arg is not an integer",
        ),
    };
    bigint_to_word(m, &na.gcd(&nb)).raw()
}

/// `(lcm a b)` — least common multiple.
pub extern "C-unwind" fn lcm_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_integer::Integer;
    if n_args != 2 {
        return crate::abi::signal_condition_string(mutator, "lcm: expected 2 args");
    }
    let m = unsafe { &mut *mutator };
    let a = Word::from_raw(unsafe { *args });
    let b = Word::from_raw(unsafe { *args.add(1) });
    let na = match integer_to_bigint(a) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "lcm: first arg is not an integer",
        ),
    };
    let nb = match integer_to_bigint(b) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "lcm: second arg is not an integer",
        ),
    };
    bigint_to_word(m, &na.lcm(&nb)).raw()
}

/// `(expt base power)` — integer base, non-negative integer power.
/// Returns a fixnum or bignum. Errors on negative powers (those
/// would need ratios).
pub extern "C-unwind" fn expt_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_traits::{ToPrimitive, Pow};
    if n_args != 2 {
        return crate::abi::signal_condition_string(mutator, "expt: expected 2 args");
    }
    let m = unsafe { &mut *mutator };
    let base_w = Word::from_raw(unsafe { *args });
    let exp_w = Word::from_raw(unsafe { *args.add(1) });
    // Float contagion: if either operand is a float, the result is a
    // float (CL: expt of a float, or to a float power, yields a
    // float). Delegate to the float path, which uses powf and also
    // handles non-integer powers — e.g. (expt 2.0 3), (expt 1.5 2),
    // (expt 2.0 0.5). The integer/ratio exact path below is reserved
    // for rational base AND integer power.
    if crate::float::is_float(base_w) || crate::float::is_float(exp_w) {
        return crate::float::expt_float_shim(mutator, _env, args, n_args);
    }
    let base = match integer_to_bigint(base_w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "expt: base must be an integer",
        ),
    };
    // Exponent must be a fixnum whose magnitude fits in u32
    // (BigInt::pow takes u32). 2^32 is more headroom than any
    // reasonable Lisp program will use — a million-digit result
    // happens at exponent ~17000.
    let exp = match exp_w.as_fixnum() {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "expt: exponent must be a fixnum",
        ),
    };
    let abs_exp = exp.unsigned_abs();
    if abs_exp > u32::MAX as u64 {
        return crate::abi::signal_condition_string(
            mutator, "expt: exponent too large",
        );
    }
    let pow = base.pow(abs_exp as u32);
    if exp >= 0 {
        return bigint_to_word(m, &pow).raw();
    }
    // Negative exponent on an integer base: (expt b n) for n<0 is the
    // exact rational 1 / b^|n|. b^|n| == 0 only when b == 0, which is a
    // division by zero. BigRational::new reduces to lowest terms and
    // normalises the sign onto the numerator, and bigrational_to_word
    // collapses a unit denominator back to an integer — so (expt -2 -3)
    // => -1/8, (expt 1 -5) => 1, (expt 2 -3) => 1/8 all fall out.
    if pow.is_zero() {
        return crate::abi::signal_condition_string(
            mutator, "expt: zero to a negative power (division by zero)",
        );
    }
    let q = num_rational::BigRational::new(BigInt::from(1), pow);
    crate::ratio::bigrational_to_word(m, &q).raw()
}

/// `(abs n)` — magnitude. Integer-only for now.
pub extern "C-unwind" fn abs_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "abs: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    let n = match integer_to_bigint(w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "abs: argument is not an integer",
        ),
    };
    bigint_to_word(m, &n.abs()).raw()
}

/// `(isqrt n)` — integer square root (largest k with k*k <= n).
/// Errors on negative input.
pub extern "C-unwind" fn isqrt_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_integer::Roots;
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "isqrt: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    let n = match integer_to_bigint(w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "isqrt: argument is not an integer",
        ),
    };
    if n.is_negative() {
        return crate::abi::signal_condition_string(
            mutator, "isqrt: argument is negative",
        );
    }
    bigint_to_word(m, &n.sqrt()).raw()
}

// ─── D.3: bit operations ────────────────────────────────────────────────

/// `(ash int shift)` — arithmetic shift. Positive SHIFT shifts
/// left; negative shifts right (sign-extending). CL semantics:
/// (ash 5 2) => 20, (ash 20 -2) => 5, (ash -1 1) => -2.
pub extern "C-unwind" fn ash_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_traits::Signed;
    if n_args != 2 {
        return crate::abi::signal_condition_string(mutator, "ash: expected 2 args");
    }
    let m = unsafe { &mut *mutator };
    let int_w = Word::from_raw(unsafe { *args });
    let shift_w = Word::from_raw(unsafe { *args.add(1) });
    let n = match integer_to_bigint(int_w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "ash: first arg must be an integer",
        ),
    };
    let shift = match shift_w.as_fixnum() {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "ash: shift count must be a fixnum",
        ),
    };
    let result = if shift >= 0 {
        if shift > u32::MAX as i64 {
            return crate::abi::signal_condition_string(
                mutator, "ash: shift count too large",
            );
        }
        n << (shift as usize)
    } else {
        let s = (-shift) as usize;
        // num-bigint's >> rounds toward minus infinity for
        // negative values, matching CL's ash.
        if n.is_negative() {
            // num-bigint's >> on negative might truncate toward
            // zero instead. Use the floor-shift identity:
            // (ash n -k) = (floor n (expt 2 k)).
            use num_traits::Pow;
            use num_integer::Integer;
            let two: BigInt = BigInt::from(2);
            let divisor = two.pow(s as u32);
            n.div_floor(&divisor)
        } else {
            n >> s
        }
    };
    bigint_to_word(m, &result).raw()
}

/// `(logand a b)` — bitwise AND.
pub extern "C-unwind" fn logand_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    bit_op_2(mutator, args, n_args, "logand", |a, b| a & b)
}

/// `(logior a b)` — bitwise OR.
pub extern "C-unwind" fn logior_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    bit_op_2(mutator, args, n_args, "logior", |a, b| a | b)
}

/// `(logxor a b)` — bitwise XOR.
pub extern "C-unwind" fn logxor_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    bit_op_2(mutator, args, n_args, "logxor", |a, b| a ^ b)
}

fn bit_op_2(
    mutator: *mut MutatorState,
    args: *const u64,
    n_args: u64,
    name: &str,
    op: impl Fn(BigInt, BigInt) -> BigInt,
) -> u64 {
    // CL semantics: (logand)/(logior)/(logxor) accept any arity.
    // (logior) → 0, (logior x) → x, (logior a b c ...) → fold.
    // The identity differs per op:
    //   logand: -1 (all bits set), logior: 0, logxor: 0.
    let identity: BigInt = match name {
        "logand" => -BigInt::from(1),
        _ => BigInt::from(0),
    };
    let m = unsafe { &mut *mutator };
    if n_args == 0 {
        return bigint_to_word(m, &identity).raw();
    }
    let mut acc = match integer_to_bigint(Word::from_raw(unsafe { *args })) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, &format!("{name}: arg 0 is not an integer"),
        ),
    };
    for i in 1..n_args {
        let next = match integer_to_bigint(Word::from_raw(unsafe { *args.add(i as usize) })) {
            Some(n) => n,
            None => return crate::abi::signal_condition_string(
                mutator, &format!("{name}: arg {i} is not an integer"),
            ),
        };
        acc = op(acc, next);
    }
    bigint_to_word(m, &acc).raw()
}

/// `(lognot n)` — bitwise complement (-n - 1 in two's complement).
pub extern "C-unwind" fn lognot_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "lognot: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    let n = match integer_to_bigint(w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "lognot: argument is not an integer",
        ),
    };
    bigint_to_word(m, &!n).raw()
}

/// `(integer-length n)` — number of bits needed to represent
/// (abs n), excluding sign. (integer-length 0) = 0,
/// (integer-length 255) = 8, (integer-length -1) = 0.
pub extern "C-unwind" fn integer_length_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "integer-length: expected 1 arg",
        );
    }
    let w = Word::from_raw(unsafe { *args });
    let n = match integer_to_bigint(w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "integer-length: argument is not an integer",
        ),
    };
    // CL: (integer-length n) is the number of bits in n's two's-
    // complement representation, not counting leading zeros (or
    // leading ones for negative). For non-negative n,
    // num-bigint's `bits()` matches. For negative n, CL says
    // (integer-length -1) = 0, (integer-length -256) = 8 — i.e.,
    // (integer-length n) = (integer-length (lognot n)).
    let m = if n.sign() == num_bigint::Sign::Minus {
        (!n).bits()
    } else {
        n.bits()
    };
    Word::fixnum(m as i64).raw()
}

/// `(logbitp index n)` — T iff bit INDEX of N (with infinite
/// two's-complement sign extension) is 1.
pub extern "C-unwind" fn logbitp_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_traits::Signed;
    if n_args != 2 {
        return crate::abi::signal_condition_string(
            mutator, "logbitp: expected 2 args",
        );
    }
    let idx_w = Word::from_raw(unsafe { *args });
    let n_w = Word::from_raw(unsafe { *args.add(1) });
    let idx = match idx_w.as_fixnum() {
        Some(n) if n >= 0 => n as u64,
        _ => return crate::abi::signal_condition_string(
            mutator, "logbitp: index must be a non-negative fixnum",
        ),
    };
    let n = match integer_to_bigint(n_w) {
        Some(n) => n,
        None => return crate::abi::signal_condition_string(
            mutator, "logbitp: second arg is not an integer",
        ),
    };
    // For n >= 0: test bit i of n.
    // For n  < 0: in CL's notional two's complement, the bit is
    //             the complement of bit i of (- -n 1).
    let bit_set = if n.is_negative() {
        let abs_minus_one = (-&n) - BigInt::from(1);
        !abs_minus_one.bit(idx)
    } else {
        n.bit(idx)
    };
    if bit_set { Word::T.raw() } else { Word::NIL.raw() }
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
    fn fixnum_in_fixnum_out_for_small_bigints() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let n = BigInt::from(42i64);
        let w = bigint_to_word(&mut m, &n);
        assert_eq!(w.as_fixnum(), Some(42));
        assert!(!is_bignum(w));
    }

    #[test]
    fn round_trip_through_bignum() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let n: BigInt = "12345678901234567890123456789012345".parse().unwrap();
        let w = bigint_to_word(&mut m, &n);
        assert!(is_bignum(w));
        let back = bignum_to_bigint(w);
        assert_eq!(back, n);
    }

    #[test]
    fn negative_bignum_round_trip() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let n: BigInt = "-99999999999999999999999999999999999".parse().unwrap();
        let w = bigint_to_word(&mut m, &n);
        assert!(is_bignum(w));
        assert_eq!(bignum_to_bigint(w), n);
    }
}
