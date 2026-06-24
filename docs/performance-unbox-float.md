# Unboxed Float Arithmetic — Plan

*Status: proposed. Author: perf investigation 2026-06-10. Owner: TBD.*

The single biggest remaining numeric lever in NCL. This plan tracks float
values as native `f64` in registers through JIT codegen and boxes them
only when they escape, eliminating the per-operation heap allocation +
generic-dispatch tower that currently dominates all float-heavy code.

---

## 1. Why — the validated ceiling

NCL floats are **heap-boxed**: every `f64` is a 3-cell `HeapType::Float`
object (`Tag::Vector` pointer; header at cell 0, `%FLOAT` marker at cell
1, raw `f64` bits at cell 2 — see `src/ncl-runtime/src/float.rs`). The JIT
(`emit_overflow_op`, `emit_cmp`, `emit_div_op` in `src/ncl-llvm/src/lib.rs`)
inlines arithmetic
**only when both operands are fixnums** (`(lhs|rhs)&7 == 0`). Floats have
a non-zero tag, so every float `*`/`+`/`-`/`/`/`>` falls to a generic
runtime call tower:

```
emit_overflow_op slow-path  →  ncl_mul_complex  →  ncl_mul_full
                            →  ncl_mul_float     →  alloc_float (heap box)
```

Each float op therefore costs: a non-inlinable call, repeated tag
decoding down the tower, **and a fresh 24-byte heap allocation** for the
result — all immediately dead, feeding GC churn.

### Measured (480×360, max-iter 100 Mandelbrot render)

| Variant | Time | vs baseline |
|---|---|---|
| Baseline — boxed-float Lisp inner loop | **1714 ms** | — |
| **Ceiling** — Lisp render loop + per-pixel call, **native `f64` inner loop** | **21 ms** | **~82×** |
| Floor — entire render native (no Lisp loop) | 7 ms | 245× |

(Pixel sums identical across all three; measured via a throwaway native
`mandel-iter` shim, since reverted.) The inner-loop boxing/dispatch is
**~99 % of render time**; the per-pixel Lisp call + boxed coordinate math
that unboxing would *keep* is only ~14 ms. Conclusion: unboxing just the
inner-loop arithmetic captures essentially the entire win.

### What we already ruled out

- **A fixed-point (pure-fixnum) rewrite** was only **1.4×** faster — a red
  herring, because `ash`/shift is itself an un-inlined shim call. Not a
  ceiling on the float work.
- **An inline float fast-path that still reboxes** (`is_float` check →
  unbox → native `fmul` → `ncl_box_float`) was *implemented, correct, and
  ~1.8× **slower*** than the generic tower, then reverted. Lesson: a
  per-op rebox does not help — **the cost is the per-op allocation, not
  the dispatch.** The value must stay unboxed across the whole expression
  and loop, never round-tripping through the heap. This plan is built
  around that lesson.

---

## 2. Design

### 2.1 The representation abstraction

Today `emit_expr(...) -> Result<IntValue<'ctx>, String>` — every
expression yields one tagged `Word` (an `i64`). We give each SSA value a
**representation**:

```rust
enum Repr<'ctx> {
    /// A tagged NCL Word (i64). The universal representation; what
    /// everything is today.
    Word(IntValue<'ctx>),
    /// An unboxed IEEE-754 double in a register. Never a GC root.
    F64(FloatValue<'ctx>),
}
```

`emit_expr` returns `Repr`. Two boundary coercions mediate everything:

- `coerce_to_word(Repr) -> IntValue` — if `F64`, box via `ncl_box_float`
  (re-add the helper from the reverted #2 spike); if `Word`, identity.
- `coerce_to_f64(Repr) -> FloatValue` — if `F64`, identity; if `Word`,
  emit the inline unbox **guarded by a type check** (or call a coercion
  helper for the int→float / error cases).

**Invariant:** a `Word` produced by `coerce_to_word(F64 x)` is a freshly
heap-allocated float and must be rooted like any allocation if it can be
live across a later GC (the existing safepoint-wrap machinery handles
this — boxing happens at an escape, which is exactly where a Word is
needed and rooted).

### 2.2 Where float-typedness comes from

The compiler may represent a value as `F64` only when it can **prove**
it is a float. Sources, in priority order:

1. **Float literals** — `0.0`, `4.0`, `2.0d0` → `Repr::F64(const)`.
2. **Float-producing ops** — `(f64 OP f64)` for `+ - * /`; `sqrt`/`sin`/…
   results (once we add unboxed-return shims, Sprint 5; until then their
   results are `Word` and re-unboxed on use).
3. **Coercion forms** — `(float x)`, `(coerce x 'double-float)`.
4. **Declarations** — `(declare (double-float v))` / `(declare (type
   double-float v))` on locals and params.
5. **Local inference** — a `let`/`setq` variable whose initialiser is
   `F64` and whose every `setq` is `F64` is itself `F64` (intra-procedural;
   the loop-carried `zx`/`zy` case).

Anything not provably float stays `Word` (current behaviour). **No
speculation in the early sprints** — unproven ⇒ boxed ⇒ identical to
today. This is what keeps each sprint a strict superset (no regressions).

### 2.3 Escape / boundary rules (when an `F64` must be boxed)

`coerce_to_word` is inserted wherever an `F64` flows into a Word context:

- stored into a cons / vector / struct / array slot;
- returned from the function (until Sprint 5's unboxed-return convention);
- passed as an argument to a general (non-float-specialised) call;
- bound to, or `setq`'d into, a variable whose chosen representation is
  `Word`;
- used by a Word-only primitive (`eq`, `consp`, `print`, hash, generic
  `=` against a non-float, …);
- merged at a control-flow join (`if`/`cond`/`loop` phi) with a `Word`
  sibling — unify to `Word` (box the `F64` arm). If both arms are `F64`,
  the join stays `F64`.

### 2.4 Variable representation is fixed per scope

A variable cannot be `F64` on one read and `Word` on another — the loop
phi must have a single type. So **representation is decided before
emission** by a per-function pre-pass (`assign_reprs`) over the body:

1. Seed: declared floats, and locals whose initialiser is statically
   `F64`.
2. Propagate through `setq`: if any assignment to `v` is non-`F64` (or
   `v` escapes to a Word context that forces Word), demote `v` to `Word`
   for its whole scope.
3. Fixed-point until stable. Conservative: any doubt ⇒ `Word`.

`F64` locals are emitted as an `f64` `alloca` (which `mem2reg`/SROA
promotes to SSA + phis at `-O2`) or directly as phi-threaded SSA. The
`alloca` route is simplest and optimises cleanly — mirror how mutable
boxed locals are already handled.

**Root-tracking boundary (must be explicit).** The existing codegen
threads Word locals/params as a `Vec<IntValue>` that the safepoint wrap
re-roots and pop-reloads after a GC (`merge_locals_params_at_join`,
`emit_safepoint_wrap` in `src/ncl-llvm/src/lib.rs`). An `F64` `alloca`
must live **outside** that vector: it is not a GC root, and it must
**not** be reloaded post-GC (a stack-slot `f64` does not move). This is
why the `alloca` route is attractive — it sidesteps the root machinery
entirely. But the converse is a corruption-class hazard: if a future
change ever threads an `F64` slot through the Word `locals`/`params`
vectors, the wrap will treat raw `f64` bits as a heap pointer to root and
forward. Keep `F64` slots in a separate side-table; never in the Word
root set.

### 2.5 Mixed int/float coercion

`(+ fixnum-expr f64-expr)` and friends: when one side is `F64` and the
other is a **statically-known fixnum** (literal, or a declared/inferred
fixnum), convert and do the native op. When the other side's type is
unknown, **box and go generic** (today's path) — correct, just not
accelerated. The render's `(+ -2.5 (* px xs))` (fixnum `px` × float `xs`)
is the motivating case.

**Order of operations matters:** a tagged fixnum is `n << 3`, so the
conversion is **`ashr 3` (untag) *then* `sitofp i64→double`** — not
`sitofp` of the tagged word, which would compute `8 × n`. The canonical
off-by-8× bug; the `float-arith` suite's mixed cases must catch it.

### 2.6 Safety

`(declare (double-float cx))` is a promise. Default (`safety >= 1`):
unbox the param **with a runtime check** that signals a `type-error` if a
non-float is passed (one branch at function entry, negligible). Under
`(optimize (safety 0))`: unchecked unbox. This matches CL semantics and
keeps the default safe.

### 2.6a NaN consistency between the unboxed and boxed paths

A subtlety the "strict superset" claim depends on: the *same* source
comparison must give the *same* answer whether the compiler proved
float-typedness (native `fcmp`) or not (boxed `ncl_cmp_real`). The boxed
path today collapses NaN to "equal" (`ncl_cmp_real` returns `0` for
unordered — `src/ncl-runtime/src/float.rs`), whereas IEEE `fcmp OEQ`
yields the correct `(= nan nan) ⇒ NIL`. So an `F64`-typed comparison and
its boxed twin would **disagree on NaN**. Resolve this when the native
path lands (Sprint 1): preferably fix *both* to IEEE-ordered semantics;
at minimum the `float-arith` suite must assert the two paths agree.
Silent divergence here is a "same code, different answer depending on
inference" trap.

### 2.7 Explicitly out of scope

- **NaN-boxing** floats as immediates — would remove boxing globally but
  is a sweeping change to the entire `Word` representation (NCL uses
  3-tag-bit fixnums today). Rejected; not worth the blast radius.
- **`single-float`** as a distinct representation — NCL floats are
  doubles. Treat `single-float` declarations as `double-float` for now
  (note in `coerce`); a separate `f32` repr is a later, low-value add.
  This is a real semantic deviation (`typep`, `coerce`, print
  round-trip), not just a perf simplification — before relying on it,
  confirm it does not move the ANSI `490/56/85` baseline, and if any of
  the 56 known failures are already `single-float`-related, note that so
  the gate isn't misread as a regression.

---

## 3. Sprints

Each sprint is independently shippable, lands behind the existing
build/test gates, and **must not regress** the ANSI suite (held at
490 / 56 / 85) or the gauntlet (`ALL-PASS`). The ordering front-loads
de-risking (Sprint 0 is a behaviour-identical refactor) and back-loads
the payoff (Sprint 3 hits the Mandelbrot ceiling), mirroring the
root-stack work (prove the plumbing first, then switch the optimisation
on).

### Sprint 0 — `Repr` scaffolding (behaviour-identical)

**Goal:** thread `Repr` through codegen with *every* value still a
`Word`. No optimisation yet — pure refactor, provably a no-op.

- Define `Repr` + `coerce_to_word` / `coerce_to_f64`.
- Change `emit_expr` (and the helpers it calls) to return `Repr`,
  wrapping every current result as `Repr::Word(..)`; callers
  `coerce_to_word` at use sites.
- Re-add `ncl_box_float(mutator, f64) -> Word` (float.rs) + JIT helper
  binding (from the reverted #2 spike) — unused until Sprint 1, but lands
  the plumbing + the `jit_float_layout_contract` unit test that locks
  `(w&7)==3 && (*(w&!7)&31)==7`, f64 at byte 16.
- **Gate:** ANSI unchanged, gauntlet green, `ncl-runtime` unit tests
  green. Diff is large but mechanical; reviewable as "no behavioural
  change."

### Sprint 1 — unboxed float *expressions* (no variables)

**Goal:** a float expression tree with no variable involvement computes
in registers and boxes once at the end.

- Float literal → `Repr::F64(const_float)` (instead of a static-area
  boxed Word load — today `lower.rs` boxes the literal via
  `alloc_float_in_static` and emits `Expr::Word(ptr)`).
- **Constant-fold invariant (avoids a regression):** `coerce_to_word` of
  a **compile-time-constant** `F64` must fold *back* to the static-area
  box (the existing `alloc_float_in_static` path → `Expr::Word`), **not**
  emit a fresh young-heap `ncl_box_float`. Otherwise a literal that flows
  into a Word sink (`(list 0.0)`, `(print 3.14)`, `(setq *x* 1.5)`,
  return, generic call) turns a free, shared static-constant load into a
  per-evaluation young-heap allocation — GC churn that passes the
  correctness gate while silently pessimising. Only **runtime-computed**
  `F64` values take the `ncl_box_float` path.
- `emit_overflow_op` / `emit_div_op`: when **both** operands are
  `Repr::F64`, emit native `fadd`/`fsub`/`fmul`/`fdiv` → `Repr::F64`. Else
  unchanged (box + generic).
- `emit_cmp`: both `F64` → `fcmp` → bool (no box, no `ncl_cmp_full`).
  Settle the NaN-consistency question here (§2.6a): the native `fcmp`
  result must agree with the boxed `ncl_cmp_real` path for the same
  source.
- Mixed `F64` + static-fixnum → untag (`ashr 3`) + `sitofp` + native
  (§2.5).
- **Gate:** ANSI + gauntlet; new `float-arith` correctness suite
  (contagion, chained `(+ (* 2.0 3.0) 1.0)`, NaN/inf/-0.0, mixed, and the
  native-vs-boxed agreement assertion from §2.6a). Bench: a
  pure-float-expression microbench improves; `mb-iter` does **not** yet
  (its values live in variables — Sprint 2).

### Sprint 2 — float-typed locals (`let` / `setq`)

**Goal:** `zx`/`zy`/`zx2`/`zy2` stay unboxed across the loop.

- `assign_reprs` pre-pass (§2.4): declarations + local inference decide
  each variable's repr.
- **Sequence the two seeds: declared locals first, inference second.**
  Honouring `(declare (double-float zx zy zx2 zy2))` on locals is
  strictly simpler than the fixed-point inference of §2.2 item 5, and the
  Sprint-3 demo adds those declarations anyway — so the headline
  Mandelbrot win can ride entirely on declared locals, keeping the
  inference pass off the critical path. Land declared-locals (2a), gate,
  then add inference (2b).
- Emit `F64` locals as `f64` allocas (in a side-table, **not** the Word
  root vector — §2.4); `let`-init and `setq` store `f64` (no box) when
  both var and value are `F64`; reads yield `Repr::F64`.
- Loop-carried consistency: the pre-pass guarantees a single repr per
  variable, so the loop-header phi is well-typed.
- **Gate:** ANSI + gauntlet + `float-arith` suite extended with
  loop-carried float accumulators. `mb-iter` now unboxes its locals — but
  `cx`/`cy` are params (still boxed), so it isn't at the ceiling yet;
  expect a partial win.

### Sprint 3 — float *params* via declaration (**the payoff**)

**Goal:** `(defun mb-iter (cx cy max) (declare (double-float cx cy)) …)`
unboxes end-to-end and hits the measured ceiling.

- Parse + honour `(declare (double-float …))` / `(declare (type
  double-float …))` on parameters. The scaffolding exists —
  `strip_declares` / `match_declare` in `src/ncl-compiler/src/lower.rs`
  already collect declspecs, but only `special` is honoured today; this
  extends that to type declares (in `src/ncl-compiler/src/lib.rs` +
  `lower.rs`).
- Prologue: unbox declared float params to `f64` (checked per §2.6). Once
  unboxed to a register, the incoming boxed argument may go dead — it
  need not be rooted past the prologue (the conservative stack pin
  already kept it live up to the unbox).
- **Gate:** ANSI + gauntlet + the **Mandelbrot bench as the headline
  metric**. Judge the sprint by *approaching* the ceiling, **not** by
  hitting 21 ms on the nose: the 21 ms was a native Rust shim, whereas
  Sprint 3 keeps the inner loop as JIT'd Lisp at `-O2`, which won't fully
  match `rustc -O3`. Realistic target is tens-of-× over 1714 ms (tens of
  ms), not 21 ms. Add `mandel` (already in `bench/bench.lisp`)
  before/after to the perf memo.
- Update `Lisp/demos/mandelbrot.lisp` with the declarations so the
  shipped demo is fast.

### Sprint 4 — inference / guarded speculation for undeclared params (stretch)

**Goal:** undeclared float-heavy functions get unboxed too.

- If a param is used only in float contexts, speculatively unbox **with a
  deopt guard**: a fast entry check `is_float(p)`; on the rare miss,
  branch to a boxed slow clone of the body (or fall back to per-op
  generic). Higher complexity/risk — keep behind a clear win and full
  tests. Optional; declarations already cover the important cases.

### Sprint 5 — unboxed float *return/call* convention (stretch)

**Goal:** float-returning helpers (`sqrt`, user fns declared
`(values double-float)`) avoid box-at-return + unbox-at-caller.

- A secondary calling convention returning a raw `f64` for functions
  whose return type is a declared float; native float shims (`sqrt`/`sin`/
  …) gain unboxed-return variants the JIT calls directly.
- Only worthwhile if float-returning calls show up hot after Sprints 1–3.

---

## 4. Testing & metrics

- **Non-negotiable gate every sprint:** ANSI suite `490/56/85` unchanged
  (`demos/ansi-runner.lisp`); gauntlet `ALL-PASS`; `ncl-runtime` unit
  tests green. Run the **console** build (`cargo build --release` without
  `--features gui-app`, or `nclterm.exe`) — the gui-app build has no
  console stdout.
- **Float correctness suite** (new, `bench/` or a test file): float·float
  and contagion for `+ - * / < > <= >= =`; chained expressions;
  loop-carried accumulators; declared + inferred + undeclared paths;
  edge cases NaN / ±inf / -0.0 / fixnum-overflow-to-float; `eql`/`=`
  identity; printing round-trip. A wrong unbox/box surfaces here first.
- **`jit_float_layout_contract`** unit test (Sprint 0) locks the memory
  layout the JIT hardcodes.
- **Headline benchmark:** the Mandelbrot render — judged by *approaching*
  the ~21 ms ceiling, not hitting it (the ceiling was a native shim;
  JIT'd Lisp at `-O2` lands at tens of ms, see Sprint 3) — plus a
  pure-float-arith kernel and a float dot-product added to
  `bench/bench.lisp`. Record before/after in the perf memo each sprint.
- **GC pressure check:** `gc-stats` `:minor-gcs` / `:total-minor-pause-us`
  before/after — unboxed floats should slash float-workload minors toward
  zero (they stop allocating).

## 5. Risks

- **Partial unboxing collapses the win** (the #2 lesson). If a value
  boxes even once per loop iteration, the per-op alloc returns and the
  speedup evaporates. Mitigation: `assign_reprs` must *prove* loop-carried
  `F64` consistency; add an assertion/log when a "float-looking" loop var
  is demoted to `Word`, so silent pessimisation is visible.
- **Representation mismatch at joins** — a value `F64` on one branch and
  `Word` on another. Rule: unify to `Word`. Must be applied at *every*
  `if`/`cond`/`case`/`loop`/`and`/`or` join; a missed join = a type-confused
  phi = miscompile. Enumerate joins exhaustively; the float-correctness
  suite must cover branchy float code.
- **Boundary completeness** — every escape in §2.3 must box. A missed
  escape hands a raw `f64` bit-pattern to code expecting a tagged Word →
  silent corruption. This is corruption-class; treat like the GC work.
- **Safety semantics** — declared-but-violated types. Default to checked
  unboxing; only `(optimize (safety 0))` is unchecked.
- **GC interaction** — unboxed `f64` in registers are *not* roots (good,
  fewer roots), but a *boxed* float live across a GC must be rooted; the
  box happens at an escape where a Word is needed and the existing
  safepoint-wrap already roots it. Verify with a float-allocation-heavy
  GC-stress (mirror the cons cases). **The inverse hazard:** an `F64`
  `alloca`/slot must never enter the Word `locals`/`params` root vectors
  (§2.4) — if it does, the safepoint wrap will treat raw `f64` bits as a
  heap pointer and try to forward them. Corruption-class; keep `F64`
  slots in a separate side-table.
- **Native/boxed path divergence** — the same source op can take a native
  (`fadd`/`fcmp`) or a boxed (`ncl_*_float`/`ncl_cmp_real`) path depending
  only on whether the compiler proved float-typedness. These must agree
  bit-for-bit on results, NaN ordering, and `-0.0` (§2.6a). A divergence
  is a "works differently after I added a declaration" bug — the
  `float-arith` suite must assert agreement, not just per-path
  correctness.

## 6. Touch points (current code)

- `src/ncl-llvm/src/lib.rs`: `emit_expr` signature → `Repr` (~64 call
  sites in this file); `emit_overflow_op`, `emit_cmp`, `emit_div_op`
  (native FP paths); helper decl/bind for `ncl_box_float`; the boundary
  coercions; the function prologue / param binding;
  `merge_locals_params_at_join` / `emit_safepoint_wrap` (keep `F64` slots
  out of the Word root vectors — §2.4).
- `src/ncl-compiler/src/lib.rs`, `src/ncl-compiler/src/lower.rs`:
  `(declare …)` parsing (extend `strip_declares` / `match_declare`, which
  today honour only `special`); float-literal lowering currently boxes
  via `alloc_float_in_static` (§ Sprint 1 constant-fold invariant);
  `assign_reprs` pre-pass, param-repr at lowering.
- `src/ncl-ir/src/lib.rs` / `Expr`: may need per-binding type annotations
  carried from the declare pass to codegen.
- `src/ncl-runtime/src/float.rs`: re-add `ncl_box_float`; the
  `jit_float_layout_contract` test; (Sprint 5) unboxed-return shims.
- `Lisp/demos/mandelbrot.lisp`: add declarations (Sprint 3).
- `bench/bench.lisp`: float kernels.

## 6.5 Implementation findings (Sprints 0–3 landed)

Sprints 0, 1, 3, and 2 are implemented and committed on `perf/unbox-float`
(behaviour-identical scaffolding; unboxed float **expressions**; unboxed
declared float **params**; unboxed declared float **locals**). Gates held
throughout: ANSI 490/56/85, gauntlet `ALL-PASS` (+41 new float checks),
`ncl-runtime` units green. Two findings materially change §2.4 / §3:

1. **Mutable locals are heap-boxed cons cells, not allocas.** A `setq`
   target lowers to `Binding::LocalCell` — a 1-cell cons, read via `car`,
   written via `set-car`. So "emit F64 locals as f64 allocas" is a *new
   storage class*, not a tweak. Implemented: declared float locals get a
   dedicated f64 stack slot (`Binding::LocalF64` + `Expr::F64Local{Read,
   Store}`); immutable always, mutable only when uncaptured (see below).

2. **`(loop …)` expands to `(%native-loop (lambda () …))` — a closure.**
   This is the decisive one. Loop-carried mutable locals are *captured*
   by that lambda, so they must stay boxed (an f64 stack slot can't be
   shared by reference with a separately-compiled lambda). The
   Mandelbrot inner loop (`zx`/`zy`) is exactly this case. Measured:

   | kernel | undeclared | declared | result |
   |---|---|---|---|
   | float-param `dist2` swept in a loop | 405 ms | **232 ms** | **1.75×** |
   | Mandelbrot grid (loop-carried `zx`/`zy`) | 41 ms | 44 ms | wash |

   The param/expression wins are real (1.75× on float-call-heavy code).
   But the **82× Mandelbrot ceiling is unreachable while loop carries
   box** — declaring the loop vars is a wash (boundary unbox/rebox
   cancels the native-arithmetic savings).

   **Route to the ceiling (DONE — `fast-loop`).** Rather than f64 phis in
   `TailLoop`, the fix was a new **inline loop construct** that doesn't
   capture: `(fast-loop TEST RESULT BODY…)` emits a real back-edge in the
   same function (preheader → header-phis → test → body → back-edge /
   exit→result). Loop variables live in the enclosing `let`; a declared
   `double-float` loop var sits in an f64 stack slot *outside* the
   locals/params root vectors, so it persists across the back-edge with
   **zero boxing**. The only loop-header phis are for the Word
   locals/params slots the safepoint wrap may reload mid-iteration
   (mirrors the `TailLoop` phi pattern). A statement-position
   `(setq float v)` is emitted through an effect-context emitter
   (`emit_for_effect`, scoped to the fast-loop body) so it doesn't box a
   discarded result.

   **Measured (480×360 Mandelbrot, max-iter 100), same pixel sum 3719029:**

   | variant | ms/grid | vs C |
   |---|---|---|
   | C / Rust, LLVM **-O2** (apples-to-apples) | ~6.9 | 1× |
   | NCL `(loop …)` lambda (boxes carries) | 10836 | ~1570× |
   | NCL `fast-loop`, declared floats | **~60** | **~9×** |

   So `fast-loop` took the Mandelbrot inner loop from ~1570× off the
   native -O2 floor to ~9× — the unboxed-float work now pays off. The
   remaining ~9× is per-call/funcall overhead, the integer counter's cons
   cell, GC safepoint polls, and JIT-O2 vs the ahead-of-time build. A
   self-tail-recursive form (or `TailLoop` f64 phis) could close more, but
   `fast-loop` already realizes the headline.

   Note (test harness): the ANSI summary count flickers 56↔57 across
   runs. The culprit is a pre-existing inherently-random ANSI test —
   `(< (zap 5 3) 3)` reduces to `(< (random 4) 3)`, false 1/4 of the
   time — not any of this work. The 55 *deterministic* failures are
   stable.

3. **Auto-detection (DONE — no syntax change).** The compiler now
   recognises when a simple `(loop …)` — which macroexpands to
   `(%native-loop (lambda () body))` — can be lowered inline, and does so
   automatically, giving the `fast-loop` win to ordinary code.

   **Conservative gate (`native_loop_inlinable`):** inline only when the
   body calls no Lisp-compiled (user) function and creates no
   closure / nested loop / non-local-control form — only special forms
   and primitive operators that lower to dedicated `Expr` nodes
   (Add/Mul/Lt/Car/…) or native shims. That makes every exit a *lexical*
   `(return)` (→ a direct break) and rules out a *dynamic* `%loop-return`
   or a signal/`return-from` unwind that the inline form can't model
   (it has no lambda boundary for the per-call `ABORT_PENDING`
   early-return to land on). Anything unproven stays on `%native-loop`.
   The *same* predicate gates the unbox/box decision, so they can't
   disagree. Scope: only the simple `(loop …)` path — `dotimes` / `do` /
   extended-`LOOP` use other expansions and are untouched.

   Measured: a plain `(loop …)` Mandelbrot (no `fast-loop`, no special
   syntax — just `(declare (double-float …))`) now runs at **60 ms**,
   identical to explicit `fast-loop`, vs 10836 ms boxed. The float work
   finally pays off in idiomatic code, automatically.

## 7. Prior art

The SBCL/CMUCL "representation selection" model: primitive `f64`
representation, box/unbox at boundaries chosen by type derivation, driven
by declarations + flow inference. NCL's version is deliberately smaller
(one unboxed repr, `f64`; conservative inference; declarations do the
heavy lifting first). See also `docs/GC_PRECISE_ROOTS_PLAN.md` for the
staged-de-risking style this plan follows.
