# Build node 1 — empty folder to a working Lisp evaluator

*Written 2026-05-10, after commit `af3582e`. 21 commits on `main`.*

This document captures the path we walked to take NCL from
nothing to a Lisp that evaluates real expressions:

```
$ ncl --eval "(if (eq (+ 1 2) 3) (* 100 100) 0)"
10000
$ ncl --eval "(car (cdr (cons 1 (cons 2 (cons 3 nil)))))"
2
```

It's deliberately a build journal, not a reference manual. The
manifesto and `docs/GC.md` cover *what* the system is. This file
covers *how we got here* — the order, the decisions, and the
reasoning at each step.

## Starting point

Three things existed at the start:

1. **An empty `E:\CL\` folder** — the workspace.
2. **`E:\CL\cormanlisp\`** — a clone of
   [sharplispers/cormanlisp](https://github.com/sharplispers/cormanlisp),
   the maintained fork of the original Corman Lisp (Win32 x86 32-bit,
   MFC IDE, hand-written assembly kernel). This is the *source* of
   our compatibility commitment, not the *target* of our
   implementation.
3. **`E:\NewCP\NewCP\`** — the user's sibling project: a JIT-first
   Component Pascal compiler in Rust. Already had a working LLVM
   pipeline, an `iGui` Direct2D-based GUI shim, a generational
   loader with retirement-and-quiescent-epoch, and a multi-threaded
   runtime. Used as an architectural reference, not a template.

## The original four-phase plan

Phase 0: workspace skeleton.
Phase 1: reader (s-expression parser).
Phase 2: LLVM JIT bring-up.
Phase 3: first end-to-end form `(+ 1 2)` → `3`.

This plan grew. The GC turned into its own 9-step build inserted
between Phase 2 and Phase 3, and a multi-threading design constraint
landed mid-stream, which extended the GC plan further. Two more
mini-phases (cons/car/cdr and eq/if/quote) followed Phase 3 to get
to the state captured here.

## The progression

### Phase 0 — workspace skeleton (1 commit)

A 10-crate Cargo workspace: 8 source crates (`ncl-driver`,
`ncl-reader`, `ncl-ir`, `ncl-compiler`, `ncl-llvm`, `ncl-runtime`,
`ncl-loader`, `ncl-cl`) and 2 test crates (`ncl-tests`,
`ncl-corman-demos`). Edition 2024, resolver 3,
`unsafe_op_in_unsafe_fn = "deny"` inherited by every crate.
`.gitattributes` pins LF-only line endings — important for the
planned Mac port.

The `ncl` binary from day 0:

```
$ ncl --version
NCL 0.0.0
```

### Phase 1 — reader (4 commits)

Built incrementally as 1a/1b/1c/1d:

**1a (value type, packages, features).** Added the in-memory
representation: `Value` enum (Nil, Cons, Symbol, Fixnum, Float,
Char, String, Vector, FfiBlock), `Symbol` and `Package` with `:use`
chain and Internal/External/Inherited visibility, a `Universe`
singleton bootstrapping the Corman-faithful package set
(`COMMON-LISP` with nicknames `CL`/`LISP`, `CORMANLISP` with
`CCL`/`PL`, `COMMON-LISP-USER` with `CL-USER`/`USER`, `KEYWORD`).
`*features*` populated per the rule **"claim a feature only if
claiming it leads demo code to working code"** — so `:cormanlisp`,
`:64-bit`, `:x86-64` are claimed but `:x86` and `:32-bit` are not.

**1b (tokenizer).** ANSI-CL syntax tables for the default readtable,
plus the Corman-specific `#!…!#` block reader (FFI declarations).
Standard-CL token shapes: parens, quote/backquote/comma family,
strings, character literals, atoms with single-escape `\` and
multi-escape `|…|`, sharp dispatch, `#=`/`##` circular markers.
Numbers are NOT parsed in the tokenizer — atoms come out as raw
text and the parser tries-parse-number-then-falls-back-to-symbol.
This keeps numeric syntax case-insensitive independently of
readtable case.

**1c (parser).** Token stream → `Value`s, with full
ANSI-CL behavior: readtable case (`Upcase`/`Downcase`/`Preserve`/
`Invert`), package qualifiers (`pkg:name`, `pkg::name`, `:keyword`,
`#:uninterned`), quote-family expansion, sharp dispatch (`#'`,
`#:`, `#(`, `#+`, `#-`, radix), feature-expression evaluation
(`AND`/`OR`/`NOT`), dotted pairs, `NIL` normalization. Hard ones
that are deferred (`#=`/`##` circular, `#A`/`#S`/`#P`/`#C`, `#.`
read-time eval) error with helpful "not supported in Phase 1c"
messages.

**1d (corpus run).** The acid test: parse every `.lisp` file under
`cormanlisp/examples/`. Result: **38/38 demos parse cleanly.** Two
real bugs surfaced and were fixed:

1. Missing standard Corman packages (`C-TYPES`/`CT`, `WIN32`/`WIN`,
   `SYS`) — pre-registered in the universe.
2. The `#+`/`#-` discard path was reading the to-be-discarded form
   through the value-producing pipeline, which enforced package
   existence on symbols inside the discarded form. Per CL spec,
   `*read-suppress*` says discard should be a structural skip.
   Implemented as a token-level `skip_form` walker.

A stretch run on `cormanlisp/Sys/` (Corman's own implementation
sources, 109 files) reaches 78/109 (72%) — most failures are
self-bootstrap order issues, not reader bugs.

### A mid-stream decision: FFI is a feature, not a foundation

Before Phase 2, the user clarified an architectural rule that
sharpened the manifesto: **FFI is a fully supported user-facing
capability, but our own implementation never uses FFI.** `cl:open`
uses `std::fs`, not `CreateFileW`. `cl:make-thread` uses
`std::thread`, not `CreateThread`. The stdlib is Rust-backed; the
FFI sits beside it for user code to reach.

Why: relying on FFI internally would force every primitive to
carry a "what's the Cocoa equivalent?" question into the Mac port,
and would leak the iGui thread-boundary discipline into our stream/
time/thread code. Clean separation pays off later.

Concrete consequence: Corman's `Sys/` source tree is *not* ported.
It's heavily FFI-dependent because Corman implements its stdlib
through Win32. We re-implement the ANSI surface natively in Rust;
the demos see the same Lisp-level API.

### Phase 2 — LLVM JIT bring-up (1 commit)

Hand-built a trivial LLVM module containing
`fn three() -> i64 { 3 }`, JIT'd it, called it from Rust, asserted
the result. No reader, no compiler — just proving the toolchain is
alive on this machine. inkwell 0.9.0 + llvm-sys 221.0.1 + LLVM 22.1.4
+ Windows MSVC, end-to-end.

The `.cargo/config.toml` set `LLVM_SYS_221_PREFIX` to a workstation-
local install. A `jit_add(a, b)` smoke test followed, exercising
argument passing through the calling convention.

### The GC design conversation

Phase 3 was supposed to come next. It didn't, because the user
made two interventions:

**"Adding the GC later feels like a mistake to me."** The original
plan was bump-allocator-now, GC-later. The user was right: bolting
a GC onto a system designed without one means rewriting most of
the runtime — tagged-value layout, heap shape, root tracking, write
barriers all need to be GC-aware from day one.

**"Look at the Corman Lisp 32-bit GC for inspiration."** The original
`Gc.cpp` is 4632 lines: generational copying, forwarding pointers,
per-page dirty tracking, 3-bit pointer tagging, write barriers via
Win32 page protection (off by default for stability), conservative
stack scanning with Lisp-region sentinels. We surveyed it via an
Explore agent and pulled out:

- **Adopt:** generational copying, forwarding pointers,
  headerless cons cells, per-card dirty tracking, atomic function
  cells.
- **Leave behind:** hand-written x86 assembly, Win32 page-
  protection as the primary write barrier, 32-bit pointer math,
  conservative-only scanning.

We pinned the design in `docs/GC.md` *before* writing any GC code.
Build order locked in: Word → Header → Cheney copy → Young+Old →
TLAB+stop-the-world → cards → static → atomic Symbol → stack maps.

### A second mid-stream intervention: multi-threaded as a constraint

While planning step 5, the user asked the load-bearing question:
**"Are we sure Corman GC assumes a single Lisp thread?"** It
doesn't. Verified in upstream `Gc.cpp`:
`EnterGCCriticalSection`/`LeaveGCCriticalSection` wrap every
allocation, `TlsGetValue(Thread_Index)` for per-thread state,
`checkThreadStackRoots` iterates a global thread-record list with
suspend-and-scan.

The user's verdict: "treat Corman GC fully multi-threaded as a
design constraint. Lisp threads can start threads. My 4-year-old PC
has 20 cores."

This redesigned the GC plan mid-build. The single-thread design
became multi-threaded:

- TLABs (thread-local allocation buffers) per Lisp thread instead
  of a global lock on every cons.
- Cooperative stop-the-world via a flag and a condvar, not OS
  thread suspension. Mac-portable from day one.
- AtomicU64 symbol cells (already designed in for redefinition).

The build order grew by one step (step 5 — MutatorState + TLAB +
stop-the-world coordinator) and the `docs/GC.md` headline was
rewritten.

### GC steps 1-9 (9 commits)

In order:

| Step | What |
|---|---|
| 1 | `Word` newtype, 3-bit tag table, immediate encodings (T, char, unbound). 64-bit tagged pointer. |
| 2 | `HeapHeader` (5-bit type + 24-bit length + 8-bit GC flags + 27 reserved). `Semispace` with bump allocator. Cons cells are headerless (16 bytes, two raw Words). |
| 3 | Cheney-style copying GC with forwarding pointers. `Heap` owns `from`/`to` semispaces. Cheney's two-pointer scan; queue-of-copied-objects to disambiguate cons (no header) from header'd objects in to-space. |
| 4 | Young + Old generations. `OldGen` with two semispaces that swap on full GC. `collect_minor` (young → old.live) and `collect_full` (young + old.live → old.scratch + swap). Bug found and fixed: `swap_and_reset_scratch` had reset-then-swap order, discarding survivors. |
| 5 | The big multi-threading commit. `MutatorState` per Lisp thread holds a TLAB (~512 KB slice of young). `GcCoordinator` shared via `Arc` owns the heap behind a `Mutex`, plus a `stop_requested` `AtomicBool` and a parked-state `Mutex`+`Condvar`. Mutators allocate lock-free in their TLAB; refill takes the heap lock; trigger thread sets stop, waits for all-others-parked, runs GC, signals. Tests with 4 OS threads concurrently allocating under GC pressure. |
| 6 | Software card-marking write barrier. `CardTable` of `AtomicU8` (one byte per 512 bytes of old). Lock-free `mark_card(addr)` on `MutatorState` — one atomic load (`live_base`) + one branch + one atomic byte store, no mutex. Minor GC scans only dirty cards. Negative test included: forgetting to mark a card means the young pointer is lost on the next GC. |
| 7 | Static area: pinned, never-moved. `try_alloc_cells` is lock-free CAS bump. Has its own card table. `mark_card` extended to route writes by address (old → old card table; static → static card table; elsewhere → no-op). Minor GC scans dirty static cards too. |
| 8 | GC-managed `Symbol` allocated in static, with the layout the redefinition design called for: 8 cells, with `value`/`function`/`jump_cache` as `AtomicU64`. `set_symbol_function` is one `store-release` + one byte store to the static card. Two writers + one reader test verifies no torn reads, both writes seen. |
| 9 | Stack-map scaffolding for precise root walking via LLVM `gc.statepoint`. Data shapes (`LiveSlot` `FpOffset`/`SavedRegister`, `StackMapEntry`, `ParkedFrame`), the walker function, tests against manually-constructed stack maps. The compiler-side emission lands later; until then, the explicit `push_root`/`pop_root` API on `MutatorState` is the working contract. |

### Phase 3 — first end-to-end form (1 commit)

Back to Lisp. The bridge: tiny IR (`Expr::Const(i64)`, `Add`/`Sub`/
`Mul`) in `ncl-ir`; lowering pass `Value → Expr` in
`ncl-compiler`; LLVM emission of `Expr` to `entry() -> i64` in
`ncl-llvm`'s new `jit_eval`; driver `--eval` flag.

```
$ ncl --eval "(+ 1 2)"
3
```

Variadic arithmetic folds left: `(+ 1 2 3)` → `Add(Add(1, 2), 3)`.
Nullary uses identity (`(+) → 0`, `(*) → 1`). Unary minus is
`0 - x`.

The GC machinery built in steps 1-9 is idle for this commit:
fixnums are immediate, no allocation. That changes next.

### Cons, car, cdr (1 commit)

The first allocating forms. The big architectural shift: switch
the JIT's value representation from raw `i64` to **tagged Word**,
thread a `*mut MutatorState` through the entry function so JIT'd
code can call back into the runtime, expose
`extern "C" fn ncl_alloc_cons` for the JIT to call.

The fixnum-tag trick paid off: switching `Const(n)` from `n` to
`n << 3` left `Add`/`Sub` unchanged because
`(a << 3) + (b << 3) = (a + b) << 3`. Only `Mul` needed an
`ashr 3` on one operand. Hot arithmetic stays cheap.

`Word::NIL` was a real bug: bit pattern 0 collided with
`Word::fixnum(0)`, which would have made `(eq nil 0)` true — wrong
by CL semantics. Caught it via the printer test
(`Word::fixnum(0)` printed as `nil`). Fixed by giving nil its own
immediate sub-tag (3). `(eq x nil)` is still one compare, just
against a different constant.

Added a Word printer (`format_word`) that walks heap pointers and
produces CL-style printed representations: integers, `nil`, `T`,
chars, dotted cons, proper lists.

### eq, if, quote (1 commit)

The first conditional. `Expr::Eq(a, b)` emits `build_int_compare
EQ` + `build_select` between `Word::T` and `Word::NIL`.
`Expr::If(c, t, e)` builds three blocks (then/else/merge), emits
a conditional branch on `(cond != Word::NIL)`, evaluates each arm
in its own block, joins via phi.

Subtlety: the phi node's "incoming block" for each arm is captured
*after* the arm emits its body, not at branch creation, so nested
control flow inside an arm doesn't break the phi.

`(quote x)` is supported for fixnums, `nil`, and `T` (the kinds
that map to existing IR variants). Quoted symbols other than `T`,
quoted lists, and quoted strings need either symbol resolution or
compile-time heap allocation, both of which arrive with the next
phase.

Drive-by fix: `Symbol`'s default `Debug` impl recursed through
`home → Package → all symbols → their packages → ...`, producing
a multi-megabyte dump on any quoted-symbol error. Replaced with
`Symbol(PKG::NAME)`. Caught when running `ncl --eval "(if t 'yes 'no)"`.

## Architecture as it stands

```
                 ncl-driver
                     │
                     ▼
                 ncl-compiler  ──► ncl-llvm  ──► inkwell + LLVM 22.1
                     │                │
                     ▼                ▼
                 ncl-ir         (calls back into ncl-runtime
                     │             via ncl_alloc_cons C ABI)
                     ▼
                 ncl-reader  ──► ncl-runtime
                                     │
                                     ▼
                            heap, Word, mutator,
                            static_area, gc_symbol,
                            stack_map, abi, printer
```

`ncl-runtime` is the centre of gravity: every other crate depends
on it. It contains zero FFI imports — that discipline holds.
`ncl-llvm` is the only crate that imports `inkwell` or `llvm-sys`.

**Value representation:** 64-bit tagged Word, low 3 bits classify.
Fixnum is tag 000 (so arithmetic is native i64). Cons is tag 001
(headerless, two raw Words). Forward is tag 111 (so a stale slot
is one mask-and-compare during GC). NIL is a unique immediate
(sub-tag 3) distinct from fixnum 0.

**GC:** generational copying, two generations + pinned static.
TLABs per Lisp thread. Cooperative stop-the-world. Software card-
marking write barrier. Symbol's function/value cells are
`AtomicU64`. Stack-map walker exists; integration with JIT'd code
waits for the compiler to emit `gc.statepoint`.

**JIT pipeline:** read source → reader produces `Value` →
compiler lowers to typed `Expr` → emitter walks `Expr` and builds
LLVM IR → JIT compiles to machine code → `entry(mutator_ptr)`
runs and returns a tagged Word → printer walks the result.

## What works at end of node 1

```
$ ncl --eval "(+ 1 2)"                                    →  3
$ ncl --eval "(* 1 2 3 4 5)"                              →  120
$ ncl --eval "(- 100 1 2 3 4)"                            →  90
$ ncl --eval "(cons 1 2)"                                 →  (1 . 2)
$ ncl --eval "(cons 1 (cons 2 (cons 3 nil)))"             →  (1 2 3)
$ ncl --eval "(car (cdr (cons 1 (cons 2 (cons 3 nil)))))" →  2
$ ncl --eval "(eq 1 1)"                                   →  T
$ ncl --eval "(eq (cons 1 2) (cons 1 2))"                 →  nil
$ ncl --eval "(if (eq 1 1) 7 8)"                          →  7
$ ncl --eval "(if (eq (+ 1 2) 3) (* 100 100) 0)"          →  10000
$ ncl --eval "(if (eq 1 1) (cons 1 2) (cons 3 4))"        →  (1 . 2)
$ ncl --eval "'42"                                        →  42
$ ncl --eval "'t"                                         →  T
```

**Test counts at end of node 1:**
- ncl-runtime: 105 tests (Word, heap, mutator, gc_symbol, static_area,
  stack_map, abi, printer, symbol, universe, value).
- ncl-reader: 66 tests.
- ncl-compiler: 43 tests.
- ncl-llvm: 16 tests.
- ncl-ir: 2 tests.
- ncl-tests/ncl-corman-demos: 3 tests including the smoke test
  parsing `cormanlisp/examples/baby.lisp` and the corpus runners.

Total: ~235 tests, all green.

## Decisions worth re-reading later

A short list of the calls we made along the way that were either
surprising at the time or load-bearing for the future. Each is
documented in more detail in the manifesto, the GC design doc, or
the relevant memory file.

1. **Run Corman demos, do not port Corman's implementation.** Demo
   programs are the compatibility surface. Corman's `Sys/` source
   tree (FFI-heavy) is not ported.

2. **Multi-threaded as a design constraint.** TLABs, cooperative
   stop-the-world, atomic symbol cells. Modern hardware demands it.

3. **Static area is pinned; image is not.** Source on disk is the
   only persistence; the running image is rebuilt from source on
   every launch. The static area holds JIT'd code and interned
   constants, never dies, never moves, never serializes.

4. **Headerless cons cells.** 16 bytes per cons, no header. Cons
   dominates the heap; the savings are real. The GC scanner uses a
   parallel queue to disambiguate cons from header'd objects in
   to-space.

5. **Atomic function cell for `defun`.** `set_symbol_function` is
   one `store-release` + one card-mark byte. Hot redefinition under
   multi-threading is correct without locks.

6. **Tagged-Word representation, fixnum tag 000.** Arithmetic stays
   native i64 because shifting both operands left by 3 is a no-op
   for add/sub. Mul untags one operand.

7. **NIL as a unique immediate, not bit pattern 0.** Required for
   `(eq nil 0)` to be false. Caught by the printer test.

8. **Cooperative parking, not preemptive suspension.** Mac-portable
   from day one, simpler than `SuspendThread` / signals.

9. **Stack maps via LLVM `gc.statepoint`.** Scaffolding lands now;
   integration when the compiler arrives. Until then,
   `push_root`/`pop_root` is the explicit contract.

10. **The compiler is ours.** Faithful-tribute compatibility is at
    the language and demo level. Corman's compiler internals are
    not the spec; we redesign freely.

## What's next

The end of node 1 is also the threshold of "Lisp" as an
expression language. The next horizon is **first-class
functions** — lambda, defun, the function-cell dispatch path,
multi-form programs, lexical scope. After those, a recursive
factorial or Fibonacci becomes runnable, and a Corman demo file
plausibly executes.

Node 2 will start there.
