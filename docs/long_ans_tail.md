# The Long ANSI Tail — status

Status of NCL against the Corman ANSI hyperspec-examples suite
(`demos/ansi-runner.lisp`, loading `E:/CL/cormanlisp/test/ansi-chapter-{2..8}.lisp`).

## TL;DR

| | passing | total run | suite outcome |
|---|---|---|---|
| **Start of this work** | ~192 | (partial) | **crashed mid-chapter-5** — ch6/7/8 never ran |
| **Now** | **490** | **631** | **completes cleanly, all chapters 2–8** |

The headline isn't the +298 passes; it's that the suite *finishes* without
an `ACCESS_VIOLATION` / worker panic. A whole family of "reads past the
arguments into garbage → crash" bugs was converted into clean, catchable
conditions, which is what let chapters 5–8 run at all.

Run it:

```
./target/release/ncl.exe --load demos/ansi-runner.lisp
```

(One process, full — *not* `--lean`. The full environment loads
`Lisp/Library/loop.lisp`, the real extended LOOP, and the rest of the
Library. `--lean` uses a different, smaller LOOP in `core.lisp` and is
not what the suite exercises.)

## The arc: crashes → robustness → coverage

All on branch `feat/macro-environment` (pushed to `origin`,
`albanread/NewCL`). Ten commits, in order:

| commit | area | what / why |
|---|---|---|
| `23e5949` | macros | `macrolet`, `symbol-macrolet`, `&whole`/`&environment` parsing; RAII-guarded macro env; loop-safe `setq`→`setf` rewrite; control-flow fixes (return-from no-value, no-block→condition, `block nil` consumes `(return)`) |
| `f1796ca` | core | `complement` made variadic (was unary → called its arg fn with too few args → crash) |
| `d2d90ee` | abi | **systemic**: `ncl_call`/`funcall`/`apply` reject an under-supplied call with a catchable condition instead of reading past the arg array. Unblocked the whole ch5 tail (192→442) |
| `06fb885` | equality | `eq` = identity, `eql` = identity-or-(same-type-and-value), `equal` delegates to `eql` for atoms (they had all compiled to a numeric compare) |
| `2a9aef1` | core | `constantp` + a `defconstant` constant registry (was undefined — 17 errors) |
| `7cfcee3` | loop | numeric `for` keywords (`from`/`to`/`by`/`downto`/…) accepted in any order; `by` before `from` had aborted ch6 loading. +`upfrom`/`downfrom` |
| `35280c6` | expt | float contagion — a float base or power yields a float (`(expt 1.5 2)`, `(expt 2.0 0.5)` had errored) |
| `6d61f20` | coerce | `character` / `complex` / float-subtype target types |
| `bcf8300` | = | equality on complex numbers (so `equalp` on complex, since equalp compares numbers with `=`) |
| `f8baa3f` | &environment | a real (minimal) lexical macro env: `macro-function`/`macroexpand` with a non-nil `&environment` see macrolet-local macros via a runtime↔compiler bridge hook |

### The systemic crash class

The recurring root cause behind the ch5 crashes: **a function/macro/expander
called with fewer arguments than its required parameters reads past the
supplied `args` array into uninitialised memory** — a stale stack slot or a
code pointer — and dereferences it. On Windows that surfaces as
`0xC0000005` (access violation) or `0xC0000409` (abort), indistinguishable
at a glance from a stack overflow.

Each specific offender was fixed (`complement`, `return-from` with no value,
`&environment`/`&whole` mis-parsed as required params), and then the class
was closed at the dynamic call paths with the `d2d90ee` arity guard. The one
residual hole is a *direct, literal* under-supply through the inline
fast-path codegen (e.g. writing `(member 'a)`); the suite doesn't hit it, and
closing it would mean a callee-side prologue check in `ncl-llvm`.

## Current breakdown (490 / 56 fail / 85 error / 631)

The 56 failures + 85 errors fall into three buckets.

### A. Harness artifacts — NOT NCL bugs (≈ the bulk of the errors)

The Corman `dotests` harness compiles **each test form in isolation**
(`(check-one 'expr (lambda () expr) 'expected)`), so state from one form is
invisible to the next. These all look like NCL failures but are not:

- `unbound variable: B, Y, THINGN, OBJN, INTEGER, FIXNUM, *V*, *THINGS*,
  PROSP, REG, S` — a `setq`/`defvar`/`the`/`defparameter` in one form,
  referenced from the next.
- `undefined function: MACN, MLETS, MY-MACRO, EXPAND, SAMPLE-FUNCTION,
  DECLARE-EG, MACHOOK` — a `defmacro`/`defun` in one form, called from the
  next.
- `return-from … TEMP` — a nested `defun` (the harness wraps it in a lambda)
  that gets no implicit block.
- Most of the `macroexpand` / `macroexpand-1` `&environment` failures need a
  cross-form `defmacro alpha`/`expand` and so can't pass under this harness
  even though `&environment` itself now works (see `f8baa3f`).

Fixing these would mean rewriting the harness to share one environment
across a chapter — a test-suite change, not a language change.

### B. Niche features — real but narrow (itemized, low ROI)

- `function-lambda-expression`, `compiler-macro-function` /
  `define-compiler-macro`
- `documentation`, one-arg `(compile 'foo)`, `(setf (symbol-value …))`,
  `*macroexpand-hook*`
- `call-arguments-limit` / `lambda-parameters-limit` (trivial constants)
- CLOS introspection: `find-method`, `function-keywords`, `change-class`,
  `make-load-form`

### C. Medium / deep — the remaining substantive work

- **`&environment`, full**: the bridge handles `macro-function` against the
  live lexical env. A fuller version would snapshot the env so it's valid
  after expansion, and make `macroexpand` env-aware end to end. Most ANSI
  tests for it are blocked by (A) regardless.
- **`LOOP WITH` edge cases**: parallel `and` with inter-dependencies
  (`with a = 1 and b = (+ a 2)`), and destructuring `with` + `of-type` + no
  init.
- **`subtypep`** on compound/range types (`(integer 1 3)` ⊑ `(integer 1 4)`).
- **`type-of` vocabulary**: many are *valid* differences — NCL's 61-bit
  fixnums vs Corman's narrower ones make `(type-of (expt 2 40))` legitimately
  differ; these are harness-vocabulary mismatches, not bugs.
- **structures / CLOS**: the `#S(...)` struct reader/printer, `make-array`
  with `:fill-pointer`, and nested `with-slots` / `with-accessors`.
- **`coerce … 'complex`** computes the correct `#C(x 0.0)` but the ch4 tests
  still differ under the `#C(...)` / `4.5s0` short-float reader path — a
  narrow reader edge.

## Notes for whoever picks this up

- **Two LOOPs.** The suite (full env) uses `Lisp/Library/loop.lisp`. Editing
  `core.lisp`'s `%loop` only affects `--lean`. Library `.lisp` files load
  from disk at runtime → no rebuild needed to test changes there; `core.lisp`
  is `include_str!`'d into the compiler → rebuild required.
- **Where things live.** Macro env + macrolet/symbol-macrolet:
  `src/ncl-compiler/src/macroexpand.rs`. Calling convention + equality +
  macro-function bridge: `src/ncl-runtime/src/abi.rs`. Numeric tower:
  `bignum.rs` / `ratio.rs` / `float.rs` / `complex.rs`. Codegen (eq/cmp):
  `src/ncl-llvm/src/lib.rs`. Lambda-list parsing + `compile_function_raw`:
  `src/ncl-compiler/src/lib.rs`.
- **Measuring the tail.** To rank what to fix next, extract and group the
  `ERROR in …:` messages from a suite run — a single missing function can
  account for a dozen errors (that's how `constantp` (17) and the `loop`
  chapter-6 unblock were found).
