//! Complex numbers — Tier 2.C.
//!
//! Storage: heap object tagged `Tag::Vector`, header type
//! `HeapType::Complex`. Layout:
//!
//!   cell 0: HeapHeader { ty: Complex, length_cells: 3 }
//!   cell 1: %COMPLEX marker symbol
//!   cell 2: real part      (Word — any real-number subtype)
//!   cell 3: imaginary part (Word — any real-number subtype)
//!
//! Both parts are arbitrary Words (fixnum / bignum / ratio / float),
//! so the GC scans them as live pointers. Canonicalisation:
//!
//!   * If the imaginary part is exactly zero (a real zero of any
//!     subtype) and the real part is rational, return the real
//!     part — `(complex 1 0) → 1`, but `(complex 1.0 0.0)` stays
//!     `#C(1.0 0.0)` because CL specifies that mixed-type ops
//!     coerce both parts to the wider type.
//!   * Both parts share the same "contagion class" after construction:
//!     if either is a float, both get coerced to float.
//!
//! Arithmetic kernel: we do it ourselves component-wise via the
//! existing full-lattice helpers (ncl_add_full, ncl_mul_full,
//! ncl_div_full). Each component keeps its own exact type — a
//! rational complex stays rational. The component identities:
//!
//!   (a+bi) + (c+di) = (a+c) + (b+d)i
//!   (a+bi) − (c+di) = (a−c) + (b−d)i
//!   (a+bi) × (c+di) = (ac−bd) + (ad+bc)i
//!   (a+bi) / (c+di) = ((ac+bd) + (bc−ad)i) / (c²+d²)
//!
//! `num-complex` is on the dep list for `Complex<f64>` transcendentals
//! (sqrt of a complex, etc.) — but the basic arithmetic stays
//! exact, never going through float intermediates.

use crate::float::is_float;
use crate::heap::{HeapHeader, HeapType};
use crate::mutator::MutatorState;
use crate::word::{Tag, Word};
use num_complex::Complex64;

/// Cells in a complex heap object: marker + real + imag.
pub const COMPLEX_PAYLOAD_CELLS: u32 = 3;

/// True iff WORD is a heap-allocated complex.
pub fn is_complex(w: Word) -> bool {
    if w.tag() != Tag::Vector {
        return false;
    }
    let p = match w.as_ptr::<u64>(Tag::Vector) {
        Some(p) => p,
        None => return false,
    };
    let header = HeapHeader::from_raw(unsafe { *p });
    header.ty() == HeapType::Complex
}

/// True iff WORD is any number: integer, ratio, float, OR complex.
pub fn is_number(w: Word) -> bool {
    crate::float::is_real(w) || crate::ratio::is_ratio(w) || is_complex(w)
}

/// Read a complex's real part.
pub fn complex_real(w: Word) -> Word {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("complex is a vector");
    Word::from_raw(unsafe { *p.add(2) })
}

/// Read a complex's imaginary part.
pub fn complex_imag(w: Word) -> Word {
    let p = w.as_ptr::<u64>(Tag::Vector).expect("complex is a vector");
    Word::from_raw(unsafe { *p.add(3) })
}

/// True iff WORD is a numeric zero of any real subtype.
fn is_real_zero(w: Word) -> bool {
    if let Some(n) = w.as_fixnum() {
        return n == 0;
    }
    if crate::bignum::is_bignum(w) {
        if let Some(n) = crate::bignum::integer_to_bigint(w) {
            return num_traits::Zero::is_zero(&n);
        }
    }
    if is_float(w) {
        return crate::float::float_value(w) == 0.0;
    }
    if crate::ratio::is_ratio(w) {
        if let Some(q) = crate::ratio::rational_to_bigrational(w) {
            return num_traits::Zero::is_zero(&q);
        }
    }
    false
}

/// CL "contagion": if EITHER part is a float, both parts get
/// coerced to float. Otherwise leave both parts as-is (rational).
fn coerce_to_common_type(m: &mut MutatorState, a: Word, b: Word) -> (Word, Word) {
    if is_float(a) || is_float(b) {
        let fa = crate::float::to_f64(a).unwrap_or(0.0);
        let fb = crate::float::to_f64(b).unwrap_or(0.0);
        (crate::float::alloc_float(m, fa), crate::float::alloc_float(m, fb))
    } else {
        (a, b)
    }
}

/// Construct a canonical complex Word from real and imaginary
/// parts. If the imaginary part is an EXACT (rational) zero and
/// the real part is also rational, demote to the real part. If
/// either part is a float, both get float-coerced.
pub fn make_complex(m: &mut MutatorState, re: Word, im: Word) -> Word {
    // Float-contagion first.
    let (re, im) = coerce_to_common_type(m, re, im);
    // Rational-zero imaginary part → demote IF the real part is
    // also rational. (CL keeps `#C(1.0 0.0)` as a complex but
    // collapses `#C(1 0)` to 1.)
    if !is_float(re) && !is_float(im) && is_real_zero(im) {
        return re;
    }
    alloc_complex_raw(m, re, im)
}

fn alloc_complex_raw(m: &mut MutatorState, re: Word, im: Word) -> Word {
    let marker = m.coord().marker_complex();
    let w = m.alloc_typed_vector(HeapType::Complex, COMPLEX_PAYLOAD_CELLS);
    let p = w.as_mut_ptr::<u64>(Tag::Vector).expect("just-allocated vector");
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = re.raw();
        *p.add(3) = im.raw();
    }
    w
}

/// Allocate a complex in the STATIC area. Used by the reader's
/// `#C(re im)` literal.
pub fn alloc_complex_in_static(
    static_area: &crate::static_area::StaticArea,
    coord: &crate::mutator::GcCoordinator,
    re: Word,
    im: Word,
) -> Option<Word> {
    let marker = coord.intern("%COMPLEX");
    let header_ptr =
        static_area.try_alloc_with_header(HeapType::Complex, COMPLEX_PAYLOAD_CELLS)?;
    let p = header_ptr.as_ptr() as *mut u64;
    unsafe {
        *p.add(1) = marker.raw();
        *p.add(2) = re.raw();
        *p.add(3) = im.raw();
    }
    Some(Word::from_ptr(p as *const u8, Tag::Vector))
}

/// Render a complex as "#C(re im)".
pub fn complex_to_string(w: Word) -> String {
    let re = complex_real(w);
    let im = complex_imag(w);
    let re_s = crate::printer::format_word_aesthetic(re);
    let im_s = crate::printer::format_word_aesthetic(im);
    format!("#C({} {})", re_s, im_s)
}

// ─── Arithmetic kernel — component-wise via the full lattice ──────────────
//
// We re-enter the JIT's slow paths (`ncl_*_full`) for each
// component so the rational / integer / float subtypes flow
// through without losing exactness.

#[inline]
fn add(m: *mut MutatorState, a: Word, b: Word) -> Word {
    Word::from_raw(crate::ratio::ncl_add_full(m, a.raw(), b.raw()))
}
#[inline]
fn sub(m: *mut MutatorState, a: Word, b: Word) -> Word {
    Word::from_raw(crate::ratio::ncl_sub_full(m, a.raw(), b.raw()))
}
#[inline]
fn mul(m: *mut MutatorState, a: Word, b: Word) -> Word {
    Word::from_raw(crate::ratio::ncl_mul_full(m, a.raw(), b.raw()))
}
#[inline]
fn div(m: *mut MutatorState, a: Word, b: Word) -> Word {
    Word::from_raw(crate::ratio::ncl_div_full(m, a.raw(), b.raw()))
}

/// Split a Word into (real, imag) parts. Reals become (real, 0);
/// complex values are returned as-is.
fn parts(w: Word) -> (Word, Word) {
    if is_complex(w) {
        (complex_real(w), complex_imag(w))
    } else {
        (w, Word::fixnum(0))
    }
}

/// `(a+bi) + (c+di) = (a+c) + (b+d)i`. The `make_complex` call
/// auto-demotes when the imag part is an exact zero — so adding
/// `1 + 0i` to `2 + 0i` returns the real `3`.
pub extern "C-unwind" fn ncl_add_complex(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if !is_complex(a) && !is_complex(b) {
        return crate::ratio::ncl_add_full(mutator, a_raw, b_raw);
    }
    let m = unsafe { &mut *mutator };
    let (ar, ai) = parts(a);
    let (br, bi) = parts(b);
    let re = add(mutator, ar, br);
    let im = add(mutator, ai, bi);
    make_complex(m, re, im).raw()
}

pub extern "C-unwind" fn ncl_sub_complex(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if !is_complex(a) && !is_complex(b) {
        return crate::ratio::ncl_sub_full(mutator, a_raw, b_raw);
    }
    let m = unsafe { &mut *mutator };
    let (ar, ai) = parts(a);
    let (br, bi) = parts(b);
    let re = sub(mutator, ar, br);
    let im = sub(mutator, ai, bi);
    make_complex(m, re, im).raw()
}

/// `(a+bi)(c+di) = (ac − bd) + (ad + bc)i`.
pub extern "C-unwind" fn ncl_mul_complex(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if !is_complex(a) && !is_complex(b) {
        return crate::ratio::ncl_mul_full(mutator, a_raw, b_raw);
    }
    let m = unsafe { &mut *mutator };
    let (ar, ai) = parts(a);
    let (br, bi) = parts(b);
    let ac = mul(mutator, ar, br);
    let bd = mul(mutator, ai, bi);
    let ad = mul(mutator, ar, bi);
    let bc = mul(mutator, ai, br);
    let re = sub(mutator, ac, bd);
    let im = add(mutator, ad, bc);
    make_complex(m, re, im).raw()
}

/// `(a+bi) / (c+di) = ((ac + bd) + (bc − ad)i) / (c² + d²)`.
/// Signals on division by exact zero (c² + d² == 0 in rational).
pub extern "C-unwind" fn ncl_div_complex(
    mutator: *mut MutatorState,
    a_raw: u64,
    b_raw: u64,
) -> u64 {
    let a = Word::from_raw(a_raw);
    let b = Word::from_raw(b_raw);
    if !is_complex(a) && !is_complex(b) {
        return crate::ratio::ncl_div_full(mutator, a_raw, b_raw);
    }
    let m = unsafe { &mut *mutator };
    let (ar, ai) = parts(a);
    let (br, bi) = parts(b);
    let denom = add(mutator, mul(mutator, br, br), mul(mutator, bi, bi));
    if is_real_zero(denom) {
        return crate::abi::signal_condition_string(mutator, "/: division by zero");
    }
    let re_num = add(mutator, mul(mutator, ar, br), mul(mutator, ai, bi));
    let im_num = sub(mutator, mul(mutator, ai, br), mul(mutator, ar, bi));
    let re = div(mutator, re_num, denom);
    let im = div(mutator, im_num, denom);
    make_complex(m, re, im).raw()
}

// ─── Lisp-callable shims ────────────────────────────────────────────────────

/// `(complex re [im])` — constructor. With one arg, returns a real
/// number (CL: `(complex x)` = `x` for real x). With two args, builds
/// the canonical complex; auto-demotes when im is exact zero and re
/// is rational.
pub extern "C-unwind" fn complex_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    match n_args {
        1 => {
            let w = Word::from_raw(unsafe { *args });
            if !crate::float::is_real(w) && !crate::ratio::is_ratio(w) {
                return crate::abi::signal_condition_string(
                    mutator, "complex: argument is not a real number",
                );
            }
            w.raw()
        }
        2 => {
            let re = Word::from_raw(unsafe { *args });
            let im = Word::from_raw(unsafe { *args.add(1) });
            if (!crate::float::is_real(re) && !crate::ratio::is_ratio(re))
                || (!crate::float::is_real(im) && !crate::ratio::is_ratio(im))
            {
                return crate::abi::signal_condition_string(
                    mutator, "complex: both arguments must be real numbers",
                );
            }
            make_complex(m, re, im).raw()
        }
        _ => crate::abi::signal_condition_string(
            mutator, "complex: expected 1 or 2 args",
        ),
    }
}

pub extern "C-unwind" fn complexp_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let w = Word::from_raw(unsafe { *args });
    if is_complex(w) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn numberp_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let w = Word::from_raw(unsafe { *args });
    if is_number(w) { Word::T.raw() } else { Word::NIL.raw() }
}

pub extern "C-unwind" fn realpart_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "realpart: expected 1 arg");
    }
    let w = Word::from_raw(unsafe { *args });
    if is_complex(w) {
        return complex_real(w).raw();
    }
    // Real number → itself is its own real part.
    if crate::float::is_real(w) || crate::ratio::is_ratio(w) {
        return w.raw();
    }
    crate::abi::signal_condition_string(mutator, "realpart: not a number")
}

pub extern "C-unwind" fn imagpart_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "imagpart: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    if is_complex(w) {
        return complex_imag(w).raw();
    }
    // Real number → imaginary part is 0 (or 0.0 if the real is a float).
    if crate::float::is_float(w) {
        return crate::float::alloc_float(m, 0.0).raw();
    }
    if crate::float::is_real(w) || crate::ratio::is_ratio(w) {
        return Word::fixnum(0).raw();
    }
    crate::abi::signal_condition_string(mutator, "imagpart: not a number")
}

/// `(abs c)` — magnitude. Dispatches by type:
///   integer  → integer (via num-bigint)
///   ratio    → ratio   (num-rational)
///   float    → float   (f64::abs)
///   complex  → float   (sqrt(re² + im²) via Complex64::norm)
pub extern "C-unwind" fn abs_complex_shim(
    mutator: *mut MutatorState,
    env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    use num_traits::Signed;
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "abs: expected 1 arg");
    }
    let w = Word::from_raw(unsafe { *args });
    let m = unsafe { &mut *mutator };
    if is_complex(w) {
        let re = crate::float::to_f64(complex_real(w)).unwrap_or(0.0);
        let im = crate::float::to_f64(complex_imag(w)).unwrap_or(0.0);
        return crate::float::alloc_float(m, Complex64::new(re, im).norm()).raw();
    }
    if crate::bignum::is_integer(w) {
        return crate::bignum::abs_shim(mutator, env, args, n_args);
    }
    if crate::ratio::is_ratio(w) {
        let q = crate::ratio::ratio_to_bigrational(w);
        return crate::ratio::bigrational_to_word(m, &q.abs()).raw();
    }
    if crate::float::is_float(w) {
        return crate::float::alloc_float(m, crate::float::float_value(w).abs()).raw();
    }
    crate::abi::signal_condition_string(mutator, "abs: argument is not a number")
}

/// `(conjugate c)` — flip the sign of the imaginary part. Reals
/// returned as-is.
pub extern "C-unwind" fn conjugate_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(
            mutator, "conjugate: expected 1 arg",
        );
    }
    let w = Word::from_raw(unsafe { *args });
    if !is_complex(w) {
        if crate::float::is_real(w) || crate::ratio::is_ratio(w) {
            return w.raw();
        }
        return crate::abi::signal_condition_string(mutator, "conjugate: not a number");
    }
    let m = unsafe { &mut *mutator };
    let re = complex_real(w);
    let im = complex_imag(w);
    let neg_im = sub(mutator, Word::fixnum(0), im);
    make_complex(m, re, neg_im).raw()
}

/// `(sqrt x)` — square root. Negative reals and complex inputs
/// return complex; non-negative reals stay real. Overrides the
/// float-only sqrt_shim from float.rs.
pub extern "C-unwind" fn sqrt_complex_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "sqrt: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    if is_complex(w) {
        let re = crate::float::to_f64(complex_real(w)).unwrap_or(0.0);
        let im = crate::float::to_f64(complex_imag(w)).unwrap_or(0.0);
        let r = Complex64::new(re, im).sqrt();
        let re_w = crate::float::alloc_float(m, r.re);
        let im_w = crate::float::alloc_float(m, r.im);
        return alloc_complex_raw(m, re_w, im_w).raw();
    }
    // Real arg. If negative, lift to complex.
    let f = match crate::float::to_f64(w) {
        Some(f) => f,
        None => return crate::abi::signal_condition_string(
            mutator, "sqrt: argument is not a number",
        ),
    };
    if f < 0.0 {
        let r = Complex64::new(f, 0.0).sqrt();
        let re_w = crate::float::alloc_float(m, r.re);
        let im_w = crate::float::alloc_float(m, r.im);
        return alloc_complex_raw(m, re_w, im_w).raw();
    }
    crate::float::alloc_float(m, f.sqrt()).raw()
}

/// Macro generating a unary transcendental shim that lifts to
/// complex when the real method would return NaN.
///
/// For each function: try the real f64 method first. If the result
/// is NaN (or the input is complex), use num-complex's Complex64
/// version. Result type is complex if a complex result was needed,
/// else float.
macro_rules! unary_lifted_shim {
    ($name:ident, $real:ident, $complex:ident, $err:literal) => {
        pub extern "C-unwind" fn $name(
            mutator: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            if n_args != 1 {
                return crate::abi::signal_condition_string(
                    mutator, concat!($err, ": expected 1 arg"),
                );
            }
            let m = unsafe { &mut *mutator };
            let w = Word::from_raw(unsafe { *args });
            if is_complex(w) {
                let re = crate::float::to_f64(complex_real(w)).unwrap_or(0.0);
                let im = crate::float::to_f64(complex_imag(w)).unwrap_or(0.0);
                let r = Complex64::new(re, im).$complex();
                let re_w = crate::float::alloc_float(m, r.re);
                let im_w = crate::float::alloc_float(m, r.im);
                return alloc_complex_raw(m, re_w, im_w).raw();
            }
            let f = match crate::float::to_f64(w) {
                Some(f) => f,
                None => return crate::abi::signal_condition_string(
                    mutator, concat!($err, ": argument is not a number"),
                ),
            };
            let r = f.$real();
            if r.is_nan() && !f.is_nan() {
                // Real method failed (negative arg into log, etc.).
                // Lift to complex and retry.
                let rc = Complex64::new(f, 0.0).$complex();
                let re_w = crate::float::alloc_float(m, rc.re);
                let im_w = crate::float::alloc_float(m, rc.im);
                return alloc_complex_raw(m, re_w, im_w).raw();
            }
            crate::float::alloc_float(m, r).raw()
        }
    };
}

unary_lifted_shim!(log_complex_shim,  ln,   ln,   "log");
unary_lifted_shim!(exp_complex_shim,  exp,  exp,  "exp");
unary_lifted_shim!(sin_complex_shim,  sin,  sin,  "sin");
unary_lifted_shim!(cos_complex_shim,  cos,  cos,  "cos");
unary_lifted_shim!(tan_complex_shim,  tan,  tan,  "tan");
unary_lifted_shim!(asin_complex_shim, asin, asin, "asin");
unary_lifted_shim!(acos_complex_shim, acos, acos, "acos");
unary_lifted_shim!(atan_complex_shim, atan, atan, "atan");
unary_lifted_shim!(sinh_complex_shim, sinh, sinh, "sinh");
unary_lifted_shim!(cosh_complex_shim, cosh, cosh, "cosh");
unary_lifted_shim!(tanh_complex_shim, tanh, tanh, "tanh");

/// `(log x [base])` — natural log if base omitted, log_base(x)
/// otherwise. Complex-aware for x; base must be real (CL spec).
pub extern "C-unwind" fn log_complex_base_shim(
    mutator: *mut MutatorState,
    env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    match n_args {
        1 => log_complex_shim(mutator, env, args, 1),
        2 => {
            let x = Word::from_raw(unsafe { *args });
            let b = Word::from_raw(unsafe { *args.add(1) });
            let fb = match crate::float::to_f64(b) {
                Some(f) => f,
                None => return crate::abi::signal_condition_string(
                    mutator, "log: base is not a real number",
                ),
            };
            if is_complex(x) {
                let re = crate::float::to_f64(complex_real(x)).unwrap_or(0.0);
                let im = crate::float::to_f64(complex_imag(x)).unwrap_or(0.0);
                let r = Complex64::new(re, im).log(fb);
                let re_w = crate::float::alloc_float(m, r.re);
                let im_w = crate::float::alloc_float(m, r.im);
                return alloc_complex_raw(m, re_w, im_w).raw();
            }
            let fx = match crate::float::to_f64(x) {
                Some(f) => f,
                None => return crate::abi::signal_condition_string(
                    mutator, "log: argument is not a number",
                ),
            };
            let r = fx.log(fb);
            if r.is_nan() && !fx.is_nan() && !fb.is_nan() {
                let rc = Complex64::new(fx, 0.0).log(fb);
                let re_w = crate::float::alloc_float(m, rc.re);
                let im_w = crate::float::alloc_float(m, rc.im);
                return alloc_complex_raw(m, re_w, im_w).raw();
            }
            crate::float::alloc_float(m, r).raw()
        }
        _ => crate::abi::signal_condition_string(mutator, "log: expected 1 or 2 args"),
    }
}

/// `(phase c)` — argument / angle of a complex number, in radians.
/// Always returns a float.
pub extern "C-unwind" fn phase_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return crate::abi::signal_condition_string(mutator, "phase: expected 1 arg");
    }
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(unsafe { *args });
    let (re, im) = if is_complex(w) {
        (complex_real(w), complex_imag(w))
    } else {
        (w, Word::fixnum(0))
    };
    let fr = crate::float::to_f64(re).unwrap_or(f64::NAN);
    let fi = crate::float::to_f64(im).unwrap_or(f64::NAN);
    crate::float::alloc_float(m, fi.atan2(fr)).raw()
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
    fn integer_zero_demotes() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // (complex 5 0) → 5
        let w = make_complex(&mut m, Word::fixnum(5), Word::fixnum(0));
        assert!(!is_complex(w));
        assert_eq!(w.as_fixnum(), Some(5));
    }

    #[test]
    fn complex_round_trip() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // (complex 3 4) → #C(3 4)
        let w = make_complex(&mut m, Word::fixnum(3), Word::fixnum(4));
        assert!(is_complex(w));
        assert_eq!(complex_real(w).as_fixnum(), Some(3));
        assert_eq!(complex_imag(w).as_fixnum(), Some(4));
        assert_eq!(complex_to_string(w), "#C(3 4)");
    }

    #[test]
    fn float_contagion() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        // (complex 1 2.0) — both parts become float.
        let f2 = crate::float::alloc_float(&mut m, 2.0);
        let w = make_complex(&mut m, Word::fixnum(1), f2);
        assert!(is_complex(w));
        assert!(is_float(complex_real(w)));
        assert!(is_float(complex_imag(w)));
    }
}
