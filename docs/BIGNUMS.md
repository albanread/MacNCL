# Bignums — Tier 1.D design + staging plan

> ⚠ **Scope note — superseded.** This is the Tier 1.D plan; it
> deliberately defers floats, ratios, and complex numbers. All three have
> since been implemented: floats (`src/ncl-runtime/src/float.rs`, ryū
> printing), exact rationals (`ratio.rs`, Tier 2.B), and complex numbers
> (`complex.rs`, Tier 2.C). Do not cite this doc's "not in this tier" list
> as the current state of the numeric tower.

This is the planning doc for Tier 1.D (the last piece of Tier 1).
Read first; commits implement against this plan.

## Why now

Tier 1.A/B/C (streams + FORMAT + conditions) shipped. Without
bignums, the language is still "demo Lisp" — you can't run real
combinatorics, can't compute `(factorial 30)`, can't read large
integer literals. Corman shipped a full numeric tower; we want
at least the integer half (fixnum × bignum) before anything else.

Floats / ratios / complex are deliberately deferred — they each
need their own design pass and don't block credibility the way
integer overflow does.

## Scope (and what's NOT in scope)

In:

- Arbitrary-precision integers (signed)
- Transparent overflow promotion from fixnum to bignum at `+`,
  `-`, `*`
- Comparison operators (`<`, `>`, `<=`, `>=`, `=`) across fixnum
  and bignum
- Division: `truncate`, `floor`, `ceiling`, `round`, `mod`, `rem`
  (each returning the integer with appropriate rounding)
- Bit ops: `logand`, `logior`, `logxor`, `lognot`, `ash`,
  `logbitp`, `integer-length`
- Math: `gcd`, `lcm`, `abs`, `isqrt`, `expt` (integer base +
  non-negative integer exponent — fractional exponents wait on
  floats)
- Reader: parse `1234567890123456789012345678901234567890` as a
  bignum literal. Plus radix prefixes (`#x`, `#o`, `#b`, `#nR`)
- Printer: render a bignum in decimal (and arbitrary radix via
  `~R` once it picks up bignum support)

Not in this tier:

- Floats / `ratio` / `complex` types
- `float`, `rational`, `coerce` between numeric subtypes
- Floating point printing (Dragon4, Grisu)
- `pretty.lisp`-style adjustable precision

## Representation

CLOS instances showed us a clean pattern: 4-cell Vector with a
sentinel symbol in slot 0 lets the printer / `typep` distinguish
without a new tag. We use the same trick, but the cells after the
marker carry raw u64 limbs that the GC must NOT trace as Words.

That's why `HeapType::FfiBlock` exists in `heap.rs` — opaque
cells the GC scans for liveness but doesn't follow as pointers.
We add a parallel `HeapType::Bignum`.

Layout of a bignum heap object (5 + n_limbs cells; `n_limbs >= 1`):

| Cell | Contents |
|------|----------|
| 0 | `HeapHeader { ty: Bignum, length_cells: 4 + n_limbs }` |
| 1 | `%BIGNUM` marker symbol (so printer can identify it cheaply) |
| 2 | sign — fixnum +1 or -1 |
| 3 | n_limbs — fixnum |
| 4 | reserved (future: cached fixnum-equivalent / hash) |
| 5..5+n_limbs | raw `u64` limbs, little-endian |

A bignum is tagged with `Tag::Vector` (so existing
`as_ptr::<u64>(Tag::Vector)` works), but the `HeapType::Bignum`
header tells the GC scan loop to skip limb cells.

### Normalisation

Every arithmetic operation ends in normalisation:

1. Drop trailing zero limbs (so `n_limbs` is minimal)
2. If `n_limbs == 0`, return fixnum 0
3. If `n_limbs == 1` and `sign * limb[0]` fits in fixnum range,
   demote to fixnum
4. Otherwise return the bignum Word

`(typep x 'integer)` is `(or (fixnump x) (bignump x))`.
`(typep x 'fixnum)` is just `Tag::Fixnum`.

## Stages

### D.1 — foundation, additive + comparison (this commit's plan)

1. `HeapType::Bignum` variant in `heap.rs`
2. GC scan: treat Bignum cells past header+marker+sign+n_limbs+
   reserved as opaque (don't trace)
3. `bignum.rs` module:
   - `alloc_bignum(m, sign: i8, limbs: &[u64]) -> Word`
   - `BignumView` accessor (sign, limbs as `&[u64]`)
   - `normalise(m, sign, limbs)` — drops zero-limbs, demotes to
     fixnum if range allows
   - `add_unsigned(a: &[u64], b: &[u64]) -> Vec<u64>`
   - `sub_unsigned(a: &[u64], b: &[u64]) -> Vec<u64>` (a >= b)
   - `cmp_unsigned(a, b) -> Ordering`
   - `from_i64(n)` — fixnum-friendly limb builder for promotion
4. ABI helpers exposed to JIT:
   - `ncl_add_promote(a, b)` — handles fixnum+fixnum overflow
     and bignum operands. Returns Word.
   - `ncl_sub_promote(a, b)`
   - `ncl_mul_promote(a, b)`
   - `ncl_cmp_int(a, b) -> i32` (-1/0/+1)
5. LLVM lowering changes:
   - `Expr::Add` emits `llvm.sadd.with.overflow.i64` of the tagged
     operands. If no overflow flag, return the sum word directly
     (still tagged correctly). On overflow, branch to a slow path
     that calls `ncl_add_promote` with the original operand words.
   - `Expr::Sub` / `Expr::Mul` analogous.
   - `Expr::Lt`/`Gt`/`Le`/`Ge`/`NumEq`: keep inline fast path
     for the both-fixnum case; on either being a bignum, call
     `ncl_cmp_int`.
6. Printer: `format_word` / `format_word_aesthetic` detect
   bignum (vector + HeapType::Bignum) and render as decimal via
   `bignum_to_decimal` (schoolbook long division by 10^18 chunk).
7. Lisp surface:
   - `bignump` predicate
   - `integer-length` (basic)
   - `(typep x 'integer)` extension

Tests: `(factorial 25)` returns `15511210043330985984000000`.

### D.2 — multiplication, division, gcd

1. `mul_unsigned` (schoolbook O(n²); Karatsuba later)
2. `divmod_unsigned`:
   - Knuth Algorithm D (single-limb fast path; multi-limb slow
     path)
   - Returns (quotient, remainder)
3. `Expr::Truncate`/`Expr::Rem` updated to call promoting
   helpers and pick up bignum operands
4. `gcd` / `lcm` (binary GCD on bignums)
5. `expt` integer^integer (square-and-multiply)
6. Lisp: `floor`, `ceiling`, `round`, `mod`, `truncate`, `rem`

### D.3 — bit operations + reader

1. `ash` (arithmetic shift, positive = left, negative = right)
2. `logand` / `logior` / `logxor` / `lognot`
3. `logbitp`, `integer-length`, `logcount`
4. Reader: integer literals over fixnum range parse to bignum.
   Radix prefixes `#x`, `#o`, `#b`, `#nR`.
5. `~R` directive in FORMAT picks up bignum support

### D.4 — performance polish (optional)

Only if measured slow:
- Karatsuba multiplication above some threshold
- Newton-Raphson square root
- Inline single-limb fast path for ops where both args fit

## What this WON'T do

- No `num-bigint` crate dependency. The whole point is the GC
  must own bignum heap memory. Importing a Rust bignum lib means
  every bignum allocates outside our heap and we leak (or
  reference-count) on the side. Roll our own.
- No Karatsuba/Toom/FFT in D.1/D.2. Schoolbook is fine for
  numbers up to ~thousands of digits, which covers every demo we
  care about.
- No `bignum-fits-in-fixnum?` short-circuit on the fast `+ - *`
  path. The overflow-check intrinsic is fast enough; adding a
  range-check first is premature.

## Risks / open questions

1. **GC tenuring of bignums**: bignums are typically created at
   arithmetic sites and may immediately become garbage (an
   intermediate in a long calculation). The young-gen collector
   should handle this fine, but the `HeapType::Bignum` scan path
   needs to be carefully written so a half-built bignum during
   allocation can't be observed by a stop-the-world from another
   thread mid-construction. Same constraint as cons cells today.

2. **MCJIT and the overflow intrinsic**: `llvm.sadd.with.overflow`
   on Windows MCJIT needs to be confirmed. Worst case we emit
   manual overflow detection (sign bits of operands + sum).

3. **Fixnum boundary semantics**: our fixnum tag is `0b000` and
   the value is `n << 3`. After `(a<<3) + (b<<3) = (a+b)<<3`, the
   overflow is the same as a 61-bit add. The intrinsic on the
   tagged i64 gives 64-bit overflow, which fires three steps
   earlier than the actual fixnum-range overflow. Either:
   - shift right BEFORE the add and re-shift after (loses bit
     for sub on `i64::MIN`)
   - check intrinsic result against `FIXNUM_MAX << 3` / `FIXNUM_MIN << 3`
   - Just promote on any 61-bit overflow; the extra 3 bits of
     slack don't hurt anyone. **Pick this.**

4. **Comparisons with one bignum + one fixnum**: must compare by
   magnitude + sign without converting the fixnum to a bignum
   first (allocation in hot comparison path is wasteful). The
   bignum side always wins by magnitude unless `n_limbs == 1`
   and `sign * limb[0]` fits in fixnum range — but normalisation
   guarantees that case never reaches comparison (it'd have been
   demoted). So bignum-vs-fixnum always: bignum has the larger
   magnitude.

## Test coverage to add

- `(factorial 25)` → exact answer
- `(* (factorial 12) (factorial 12))` → exact answer
- `(+ most-positive-fixnum most-positive-fixnum)` → 2x and the
  type is bignum
- `(- 0 most-positive-fixnum)` → still fixnum
- `(- most-negative-fixnum 1)` → bignum
- Round-trip: `(format nil "~A" big)` → reader can parse it back
  (once D.3 lands)
- All arithmetic identities (`a + (- a) = 0`, `(a * b) / b = a`,
  etc.) hold across fixnum/bignum boundaries

## Where the test suite hurts

The `nested_handler_case_inner_catches` LLVM relocation issue
hit earlier (`STATUS_STACK_BUFFER_OVERRUN` when running the full
test suite) suggests MCJIT is sensitive to compiled code volume.
Adding bignum runtime helpers adds more code. Watch for it.
