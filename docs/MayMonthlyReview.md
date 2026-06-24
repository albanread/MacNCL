# NCL Monthly Review — May 2026

*A high-level review of the NewCormanLisp compiler and an honest
assessment of Common Lisp coverage. Findings about runtime behavior
were produced by **probing the live `nclterm.exe`**, not by reading
source — and every severe claim below was independently re-verified
against the running binary. That distinction matters: NCL's source
surface materially overstates its working surface (see "The meta-
pattern").*

Scope reviewed:
- `src/ncl-reader/` (~2,370 lines) — reader/lexer
- `src/ncl-compiler/` (~9,040 lines: `lib.rs` 5,893 · `lower.rs` 2,581 · `macroexpand.rs` 566)
- `src/ncl-ir/` (~350 lines) — the NCL IR
- `src/ncl-llvm/` (~3,900 lines) — NCL IR → LLVM IR + JIT
- `Lisp/core.lisp` (1,711) · `Lisp/clos.lisp` (2,236)
- `release/.../Library/*.lisp` (~9,700, incl. `xp.lisp` 2,548, `loop.lisp` 580, `sequences.lisp` 745)

---

## Part 1 — Compiler code review

The pipeline `reader → Value → NCL IR → LLVM IR → JIT` is a real,
mostly-clean layering. `ncl-ir` earns its place as a narrow typed IR
(~50 deliberate variants, each a builder). The bulk of the real
lowering lives in `lower.rs`, not `lib.rs`.

### Top 5 risks (ranked)

**1. No tail-call optimization.** There is no `musttail`, no
self-call→loop transform anywhere in `ncl-llvm`. Combined with a
per-call shadow-stack push/pop, idiomatic recursive Lisp grows the
*native* stack and overflows. (This is the same root cause that forced
`nclterm.exe`'s 8 MB worker stack — a band-aid over the real gap.)
Highest-impact correctness/robustness issue. Tail recursion is
idiomatic CL iteration; today it is unsafe past a few thousand frames.

**2. Multiple-values instrumentation is silently wrong at the
native/Lisp boundary.** `instrument_tail_for_mv` (`lower.rs:2248`)
decides whether to collapse a tail call's value list by reading the
callee's function cell *at compile time* (`call_is_lisp_compiled`,
`lower.rs:2298`). If a symbol is a native shim now but a Lisp function
later (or the reverse), the decision is stale and secondary values
vanish with **no error**. The code itself documents that the
`Funcall` case "can produce a wrong secondary value… not a crash."
Silent-wrong is worse than a crash. This is the exact mechanism that
made `floor` drop its remainder until it was fixed this month.

**3. Diagnostics carry no source location.** The reader produces spans
(`ReaderError.span`), but they are discarded at the `EvalError`
boundary — `lib.rs:180` does `EvalError::Read(format!("{:?}", e.kind))`
and `EvalError::Display` renders compile errors as raw `{e:?}` Debug
output. Both genuine "unimplemented" and ordinary user typos surface
as `NotImplemented("...")` strings with no line number. The single
biggest day-to-day friction, for users and for the team.

**4. The special-form / library line is drawn by expedience.**
`lower_call_in_mut` (`lower.rs:550-851`) is one ~300-line `match` that
hard-wires `car`, `cdr`, `length`, `+`, `aref`, `list` *alongside*
`if`/`let`/`quote`. Consequences: a user **cannot redefine** core
functions (the compiler intercepts before the function cell is read);
making `truncate`/`rem` overridable already required surgery to demote
them to shims (see the comment at `lower.rs:553`). Separately, the
**`let*`-defined-later ordering trap is systemic**: a macro used
before its `defmacro` form is reached silently lowers as a *function
call* — no warning, wrong code. There is no forward-reference
detection.

**5. `emit_expr` is a ~1,055-line single match** (`ncl-llvm/src/lib.rs:1871`)
where every IR variant's codegen *and* its GC-root discipline live
together. One omitted `push_root` around an allocating call is a
use-after-GC, and that safety property is entirely manual and
unchecked. The `emit_safepoint_wrap` helper that enforces it is even
(stale-ly) annotated `#[allow(dead_code)]` despite being load-bearing.

### Macros and codegen

- **Macroexpansion** (`macroexpand.rs:231`) is a hand-written recursive
  walker; macros are JIT-compiled Lisp functions invoked at compile
  time via `transmute` to a fn-ptr. It is **not hygienic** (no
  gensym/rename, no `&environment`), and **nested backquote is
  unsupported** (`macroexpand.rs:429`).
- **Calling convention**: uniform `ncl_call(mutator, sym, args_ptr, n)`
  indirection for every call regardless of arity — simple, but every
  call is an indirect runtime dispatch with an args-buffer `alloca`;
  no direct/static dispatch even for known callees.
- **GC roots**: precise shadow-stack (`push_root`/`pop_root` around
  every allocating call, re-reading moved pointers). Correct model for
  the moving collector, but manual and O(live-vars) per call.
- **Boxing**: fixnums tagged inline (`<<3`), arithmetic via
  `*.with.overflow` intrinsics with bignum-promote slow paths. Solid.
  Tag-bit knowledge is open-coded across all four crates — changing the
  tag scheme touches everything.

**Bottom line:** a coherent, honestly-scoped small-language
implementation with a genuinely clean IR and a *correct* (if costly)
precise-rooting strategy. The debt concentrates in (1) absence of TCO,
(2) the fragile silently-failing MV instrumentation, and (3) location-
free stringly-typed diagnostics. None are architectural dead-ends, but
#1 and #2 are correctness issues users will hit.

---

## Part 2 — Language coverage (what works well)

Roughly **75–80% of "everyday CL" works on lists and scalars.** Three
areas are genuinely impressive:

- **CLOS / MOP** — `clos.lisp` is a faithful 2,236-line Closette:
  classes, class precedence lists, effective slots,
  `defgeneric`/`defmethod`, inheritance + specificity,
  `:before`/`:after`/`:around`, `call-next-method`, **multiple
  dispatch**, `print-object`, eql-specializers. Verified working,
  including `(combo 3 "z") → (3 "z")` multi-dispatch.
- **Numeric tower** — real bignums (`(expt 2 100)` prints in full),
  exact ratios (`1/3`, `(* 1/2 2/3)`), complex (`(sqrt -1) → #C(0.0 1.0)`),
  transcendentals, `gcd`/`isqrt`/`ash`/`logand`. Native and solid.
- **XP pretty-printer** — `xp.lisp` is a 2,548-line Waters XP port,
  demand-loaded.

Also solid and verified: `defmacro`, `destructuring-bind` (nested +
`&rest`), `flet`/`labels`, `do`/`dolist`/`dotimes`, `case`/`typecase`,
generalized `setf` (`car`/`nth`), `handler-case`/`ignore-errors`,
`loop` (common clauses), the common `format` directives
(`~A ~S ~D ~B ~X ~O ~,2F ~E ~5,'0D ~{~} ~:P ~[~] ~*`), strings/chars/
symbols (`intern`, `string<`, `char-code`, `gensym`), hash tables
(eq/eql), `compile`, `multiple-value-bind`, `nth-value`,
`with-output-to-string`.

---

## Part 3 — Essential gaps (every item verified against the running binary)

### Process-crashing (memory-safety / DoS) — fix first

- **`(reverse #(1 2 3))` → ACCESS_VIOLATION segfault** in `%REVAPPEND`.
  The sequence library is **list-only**; `reverse`/`nreverse` walk
  vector storage as cons cells. Same root cause makes
  **`(some #'evenp #(1 2 3))` panic** (`bignum.rs:360 rem: non-integer: Cons`)
  and **`(every #'numberp #(1 2 3))` return `nil`** (silently wrong;
  should be `T`). The "generic sequence protocol" is generic in name
  only. `(reverse (list 1 2 3))` is correct — lists are fine.
- **`(make-array '(2 3))` → Rust panic** (`abi.rs:2776`). Multidim
  arrays crash; only 1-D works.
- **`(format nil "~(~A~)" "HI")` → panic** (`format.rs:517`) — the
  case-conversion directive is unimplemented as a *panic*, not a Lisp
  error.

### Blockers (can't write normal programs without them)

- **The printer entry points are undefined**: `print`, `princ`,
  `prin1`, `write`, `prin1-to-string`, `princ-to-string`,
  `write-to-string`, `write-string`, `write-char`. The machinery exists
  (`format` works) but the standard names every tutorial uses are not
  wired. *Low effort, huge impact — mostly one-line `format` wrappers.*
- **`eval`, `read`, `read-from-string` are undefined.** No way to turn a
  string into evaluated Lisp at runtime — cripples REPL / loader /
  serialization.
- **`equal` / `equalp` hash tables don't honor `:test`.**
  `(let ((h (make-hash-table :test 'equal))) (setf (gethash "k" h) 99) (gethash "k" h))`
  → `nil`. `hash-table-test` reports `EQUAL`, but lookups use `eq`/`eql`.
  String-keyed tables silently fail.

### Major

- **`define-condition` is non-functional** (`(define-condition my-err (error) ())`
  → `unbound variable: MY-ERR`) → custom conditions impossible. **Restart
  resolution by name is broken** (`(invoke-restart 'r)` → `CONTROL-ERROR`
  even when `compute-restarts` shows `r`). Conditions don't carry their
  message to `~A`: `(error "boom")` prints `#<SIMPLE-ERROR>`. Arithmetic
  errors signal generic `SIMPLE-ERROR`, not `DIVISION-BY-ZERO`.
- **`tagbody`/`go` broken** — `(tagbody top … (go top))` →
  `unbound variable: TOP` (the tag is read as a variable). `do`/`dolist`/
  `loop` are separate code paths and work.
- **`defstruct` partial** — generates `make-X`, accessors, and the
  predicate, but **no `copy-X`**, **no BOA `(:constructor mk (a b))`**,
  **no `(:include …)`** inheritance.
- **No packages** — `defpackage`/`in-package`/`make-package`/`export`
  absent, `*package*` unbound (flat single namespace by design).
- **`macroexpand` / `macroexpand-1` / `macro-function` undefined** — no
  macro introspection.
- **`multiple-value-call`, `parse-integer` undefined**; fill pointers /
  `vector-push-extend` / `vector-push` / `adjust-array` / `array-rank` /
  `array-dimension` undefined (adjustable arrays are half-wired —
  `make-array` accepts the keywords but nothing consumes them).

### Minor

- `~R` / `~:R` don't spell numbers (`(format nil "~R" 4)` → `"4"`).
- `*standard-output*`, `*print-base*`, `*read-base*`, `*print-circle*`
  unbound — can't rebind printer/reader behavior.
- `(coerce 1 'double-float)` / `'single-float` error; `'float`/`'vector`/
  `'list` work.
- `loop` gaps: `for x below N` without `from` errors; destructuring
  `for (a b) in …` → `NotImplemented`; `for k being the hash-keys of …`
  errors.
- `&aux` lambda-list keyword unsupported; supplied-p flags unsupported;
  immediate `((lambda (&optional …) …) args)` is a compile error.
- Reader: `#.` (read-eval), `#A`, `#S`, `#P` unsupported (explicit
  errors in `parser.rs`); nested backquote unsupported.
- Many failures surface as raw Rust panics / `NotImplemented(...)` /
  ACCESS_VIOLATION dumps rather than Lisp conditions — leaks internals
  and can crash the process.

---

## The meta-pattern

The single most important finding: **NCL's source surface materially
overstates its working surface.** Reading the source finds a condition
class tree, "generic" sequence functions, `make-array` accepting
`:adjustable`, a `floor` returning two values. At runtime the
conditions don't define, the sequences crash on vectors, the arrays
are 1-D, and (until this month) `floor` returned one value. A
source-reading inventory and an empirical probe of this codebase
disagree on a dozen severe points, and the empirical probe is right
every time.

**Consequence for planning:** the roadmap must be driven by *probing*,
not by grepping `defun`. A feature is "done" when a probe confirms it,
not when its definition exists.

---

## Suggested fix order (highest leverage first)

1. **Wire the printer names** (`print` / `princ` / `prin1` / `write*` +
   the `-to-string` variants) — a dozen `format` wrappers; unblocks
   essentially every tutorial and example. Hours of work.
2. **Make the sequence library vector-aware** — kill the
   `reverse`/`some`/`every` segfault-and-wrong-answer class. Memory
   safety, and "generic sequences" is a headline claim that's currently
   false on vectors.
3. **`eval` + `read-from-string`** — a runtime entry into the compile
   path; unblocks REPL/loader/serialization.
4. **`equal`/`equalp` hash-table dispatch** — honor `:test` so
   string-keyed tables work.
5. **Compiler: TCO** (self-tail-call → loop at minimum) — removes the
   stack-overflow class and retires the 8 MB-stack band-aid.
6. **Compiler: thread reader spans into `EvalError`** — line numbers in
   errors.
7. Then `define-condition` / restarts, `defstruct` options (`copy-`,
   BOA, `:include`), multidimensional / adjustable arrays, packages.

Items 1 and 4 are quick, high-impact wins. Item 2 is the one to
prioritize on memory-safety grounds — it's an unprivileged segfault on
a one-line program.

---

*Review compiled from three parallel deep-dives (compiler internals;
source coverage inventory; empirical runtime probing) plus direct
verification of all process-crashing and blocker-tier findings against
`release/NewCormanLisp/nclterm.exe`.*
