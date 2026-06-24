# NCL Test-and-Improve Journal

A running log of testing, debugging, and improvement work on
NewCormanLisp. Newest entries at the top.

---

## Session 2 — CL compliance sweep (2026-06-03)

### Method

Ran the **Corman ANSI hyperspec-examples suite** (`E:/CL/cormanlisp/test/
ansi-chapter-{2..8}.lisp`, ~225 CLHS-example assertions) via
`demos/ansi-runner.lisp` for a baseline, then cross-checked every
suspected failure with a **direct probe** of the live binary — the suite
has false failures (harness/`test-equalp` artifacts: e.g. it reported
`(typep 12 'integer)` and `(coerce '(a b c) 'vector)` failing, but both
are correct when run directly) and the runner aborts a whole chapter on
the first compile error, so its raw tally undercounts.

Baseline: **174 pass / 25 fail / 26 err**, with chapters 3, 5, 6, 8
aborting mid-load. Direct probes were the source of truth throughout.

### Fixes landed (7 commits, each verified; no GC regressions)

| commit | area | what |
|--------|------|------|
| `9fa9025` | types/io | `(typep x t)`→T, `(typep x nil)`→NIL; `compiled-function-p`; `force-output`/`finish-output`/`clear-output` (the last unblocked the ANSI runner) |
| `0f159f3` | loop | leading numeric bound w/o FROM (`for i below n`), `upto`, `accumulate … into VAR` |
| `b5eae37` | numbers | `pi`, `most-positive/negative-fixnum`, double/single-float limit + epsilon constants (none existed) |
| `f819338` | macros | `macro-function`, `macroexpand`/`-1`, `special-operator-p` (all were undefined); `fboundp` fixed to see macros, special operators, and compiler-baked macros |
| `e095ddb` | loop | destructuring `for (a b) in/on` (nested + dotted) |
| `cbab7b8` | loop | `for VAR being the hash-keys/hash-values of HT [using …]` |
| `85a78d7` | loop | parallel `and` in for-/with-clauses |

Only one (`f819338`) touched Rust (abi.rs shims) and needed a rebuild; it
was gated on 208 runtime+GC tests + alloc-test. The rest are Library-only
(`.lisp`, disk-loaded — no rebuild), each verified by direct probe + a
regression sweep + the full smoke test.

Suite after fixes: **180 pass / 23 fail / 22 err** — but this *undercounts*
the real gain: chapters 3 and 6 (the LOOP chapter) still abort on
not-yet-implemented LOOP clauses, so the ~5 LOOP features added only show
up via direct probes, not the chapter tally.

### Remaining gaps — prioritized backlog (verified, with a plan)

Tractable, no Rust (Library `.lisp`):
- **LOOP `if/else` conditional accumulation** and **`loop-finish`** — the
  two clauses still blocking ANSI ch6. Both need accumulator-model rework
  (shared accumulator across then/else branches; a finally-aware
  non-local exit that isn't rewritten to `return-from`). Medium risk.
- **`(setf (getf place ind) val)`** — needs a real setf-expander that can
  update the place (insert at front when absent). Common.

Needs Rust:
- **`read` / `read-char` / `peek-char` from a stream**, and
  **`read-from-string` second value (end position)** — currently returns
  NIL. Fixing the position also enables a *resilient* ANSI runner
  (form-by-form load) for a true, uncapped compliance number.
- **`coerce`** to `character` / float types / `complex` (several targets
  signal errors).
- **`multiple-value-call`** (special form / compiler support).
- reader: **`#S`** struct literals (blocks ANSI ch8); large-integer
  literal overflow (reader tags `2^60` as fixnum though arithmetic
  correctly promotes).

Larger features:
- **`define-modify-macro`** (blocks ANSI ch5 — "non-symbol head BABBIT")
  and **nested backquote**.
- `defmethod` lambda-lists with explicit keyword-var forms `((:k v) d)`.
- `subtypep` on `integer` ranges; `change-class`; fill-pointers /
  `vector-push-extend` / adjustable arrays; `~R` number spelling.

### Now confirmed WORKING (was flagged missing in the May review)

`typep` on all the common types, `eval`, `read-from-string`,
`equal`/`equalp` hash tables honoring `:test`, vector sequences,
`macroexpand`, `fboundp` on macros/special-ops, `(expt _ -n)` → ratio,
`pi` & numeric limits, and a much larger LOOP subset (below/to/upto,
into, destructuring, hash-keys, parallel `and`).

---

## Session 1 summary (2026-06-03)

Five commits, each verified before landing; GC reliability re-checked
after every GC-touching change (per the user's standing instruction).

| commit    | what | verification |
|-----------|------|--------------|
| `6c56628` | **blocker**: stdlib bootstrap stack-overflow (hash-bucket → polymorphic-mod recursion cycle). The whole CLI was unusable on any non-`--lean` start. | stdlib loads; CLOS/conditions/LOOP/vectors/Prolog all run |
| `436f12c` | **perf**: precise-root stack `Mutex`→`RefCell` | ~3.9× call-heavy; 208 unit + full GC stress green |
| `1374340` | **perf**: inline symbol-call dispatch fast path | ~11% (interleaved A/B); GC stress + SEH green |
| `abda652` | **compat**: `alphanumericp` Unicode (un-shadow native shim) | red integration test → green |
| `6ccbdb1` | **compat**: `(expt <int> <neg>)` → exact rational | 208 unit + GC sanity + ratio-pressure |

Methodology note worth keeping: the inline-dispatch decision was nearly
wrong because the first A/B compared numbers taken ~15 min apart in a
build-heavy session — CPU-frequency drift made it look 1.7× *slower*. An
interleaved A/B (binaries run alternately) corrected it to ~11 % faster.
Never compare perf numbers across different points in a busy session.

Per-change verification done before each landed:
- `ncl-runtime` unit tests (208, incl. mutator/heap/ratio/bignum) — green
  after every commit that touched the crate.
- GC semantic gate after each GC-touching change: conservative-pin
  `alloc-test` (exact 32000000), `stress` (16-thread), `gc-watch`
  (0 pin leaks), plus `seh-unwind` for the dispatch change.
- `ncl-tests::characters` (incl. the formerly-red Unicode test) — green.

A final *cumulative* `cargo test -p ncl-tests -p ncl-corman-demos` run was
launched as a belt-and-suspenders check; it rebuilds ~30 Windows-linked
test binaries and runs the JIT demo corpus, so it is very slow. Since
every change above is independently verified and the working tree is
clean + committed, the session's correctness does not hinge on it.

Details for each below.

---

## 2026-06-03 — Session 1

### Orientation

Started from a working tree with two uncommitted, GC-touching changes:

- `src/ncl-runtime/src/mutator.rs` — the per-thread precise-root stack
  changed from `Mutex<Vec<Word>>` to `RefCell<Vec<Word>>`. Reasoning in
  the diff: the root stack is only touched by its owning thread (each
  `MutatorState` is `!Send`); the collector reads peers' roots through
  raw slices they publish while parked inside `poll_safepoint`, not
  through their `RefCell`s. So a non-atomic borrow (~1 ns) replaces a
  mutex lock (~25 ns) on the per-call hot path. Claimed win: fib(30)
  184 ms → 20 ms with rooting disabled, i.e. the lock was ~90 % of
  per-call cost.
- `src/ncl-llvm/src/lib.rs` — "Lever 2a": inline the symbol-call
  dispatch in IR (load function cell, tag-check, indirect-call the
  fast path; fall back to `ncl_call` only for unbound / not-a-function)
  plus an `NCL_NO_ROOTS` diagnostic and a gated-off `optimize_module`
  harness.

**Verdict on the uncommitted changes:** I audited the RefCell change
against the threading model and it is sound. I audited the inline
dispatch against `ncl_call` (abi.rs) and the offsets/ABI match. All 208
`ncl-runtime` unit tests pass with both changes in place.

### BLOCKER found: stdlib bootstrap stack-overflows

`./target/release/ncl.exe -e '(+ 1 2)'` → **STACK_OVERFLOW**. So does
`--repl`, `-c`, and every non-`--lean` entry. `--lean -e '(+ 1 2)'`
returns `3`. So the crash is in **stdlib bootstrap**, and it reproduces
on the committed HEAD too (stashed the perf changes and rebuilt to
confirm) — *not* caused by the uncommitted work.

Bisected the `init.lisp` require sequence: crash hits when loading
`hash-tables` (after `numbers`). The crash backtrace is a tight cycle:

```
%HT-BUCKET-INDEX → GETHASH → %NEW-TYPEP → TYPEP → FLOATP → MOD → %HT-BUCKET-INDEX → …
```

Root cause: `Library/numbers.lisp` (commit a25a5ab) redefines `MOD`/`REM`
to be **polymorphic** — they call `(floatp a)` to dispatch. `FLOATP`
is `(typep x 'float)`; `%NEW-TYPEP` (types.lisp) looks the type symbol
up with `(gethash head *type-expanders*)`; and `GETHASH`'s bucket index
is `(mod hash nbuckets)` — which now resolves to the polymorphic `MOD`.
That closes an infinite cycle. types.lisp even carries a "CRITICAL
ARCHITECTURE NOTE" about the `symbolp→typep` version of this loop, but
the `mod→floatp` path slipped through because `MOD` only becomes
dangerous once numbers.lisp is layered on top.

### Fix

`%ht-bucket-index` (core.lisp) now uses the **native integer `REM`**,
captured into `%ht-native-rem` via `(symbol-function 'rem)` *before*
numbers.lisp can redefine it. The hash is a non-negative fixnum
(`%word-hash` is documented non-negative) and nbuckets is positive, so
`rem ≡ mod` here. This breaks the cycle at its only closure point and
also takes every `gethash` off the polymorphic-dispatch path (a perf
win in the hash hot loop).

Pure-Lisp change to `core.lisp` (embedded via `include_str!`, so it
needs a rebuild); does not touch GC or compiler code.

Committed as `6c56628`.

### Post-fix verification

**Broad Lisp smoke (full CLI binary):** CLOS multiple dispatch +
`call-next-method`, `handler-case`, extended `LOOP`, vector sequences
(`reverse`/`some`/`every` — all *segfaulted* in the May review), bignums,
ratios, `string-upcase`, `equal`-keyed hash tables, and the Prolog
Zebra-puzzle demo (correct answer: Norwegian drinks water, Japanese owns
the zebra) — all run.

**GC reliability suite (all green):**
- `cargo test -p ncl-runtime` — 208/208 (26 mutator + 21 heap GC tests).
- `conservative-pin/alloc-test.lisp` — 8 threads × 2000, exact value
  `32000000` (no lost increments under concurrent minor GC).
- `conservative-pin/stress.lisp` — 16 threads × 1500 heavy mixed alloc,
  all joined cleanly.
- `conservative-pin/gc-watch.lisp` — 8 workers, 5 s, 62 M iterations,
  3 minor GCs, 803 MB promoted, 0 pinned/residual (no pin leaks).
- `seh-unwind/{exit,deep-panic,preempt,preempt-full}-test.lisp` — all
  print DONE, exit 0; deep-panic unwinds 1000 JIT frames cleanly.
  `panic-test.lisp` now exits 2 (clean driver-level catch), not the
  README's documented 101 — the unwind itself is healthy (no
  `__fastfail`); the exit code changed with the "unhandled conditions
  exit cleanly" commit (a40ccee). README is stale, not the code.

### Integration suite: 1 pre-existing failure (not from this fix)

`ncl-tests::characters::predicates_reach_beyond_ascii` fails:
`(alphanumericp (code-char #x0969))` (Devanagari digit ३) returns `nil`,
test expects `T`. Root cause: `core.lisp:2021` shadows the Unicode-aware
native `alphanumericp_shim` (`is_alphanumeric`) with the spec-literal
`(or (alpha-char-p c) (digit-char-p c))`; `digit-char-p` is ASCII
radix-based, so a non-ASCII decimal digit yields `nil`. Identical at
HEAD~1, and there is no call path from the predicate to `%ht-bucket-index`,
so the hash fix did not cause it. Candidate compat fix (see below).

### Perf: precise-root stack `Mutex` → `RefCell`

Re-evaluated the stashed perf WIP now that the stdlib loads. Adopted the
first half: `MutatorState.roots` changes from `Mutex<Vec<Word>>` to
`RefCell<Vec<Word>>`.

Soundness: `MutatorState` is `!Send` — each Lisp thread owns its own, and
the only borrower of a given root stack is its owning thread. When a peer
drives a stop-the-world minor GC, every other mutator is parked *inside*
`poll_safepoint`, holding its own `borrow_mut` guard across the call; the
collector visits peers' roots through the raw slices they published to
newgc-core while parked, never through their `RefCell`s. No cross-thread
`RefCell` access, no reentrant borrow (the collecting thread runs no Lisp
code mid-collection). So the mutex was pure overhead on the hot path.

Per the GC_LESSONS warning ("Rust tests pass, GC still wrong"), I gated
this on the *semantic* tests, not just the 208 unit tests:
- alloc-test 8×2000 exact value `32000000`; stress 16×1500 clean;
  gc-watch 8-worker 5 s, 0 pinned / 0 residual. GC reliability identical
  to baseline.

Speedup (this machine, release):

| bench         | baseline | RefCell | speedup |
|---------------|----------|---------|---------|
| fib 30        | 0.189 s  | 0.049 s | 3.9×    |
| fib 32        | 0.502 s  | 0.128 s | 3.9×    |
| tak 24 16 8   | 0.300 s  | 0.060 s | 5.0×    |
| ack 3 7       | 0.042 s  | 0.010 s | 4.2×    |

The push/pop happens once per live variable around *every* call, so the
~25 ns mutex lock dominated; the ~1 ns non-atomic borrow erases it.

### Perf: inline call dispatch ("Lever 2a") — kept, ~11%

The second half of the stashed WIP inlines the symbol-call dispatch into
the JIT'd IR: load the function cell, tag-check, and indirect-call the
bound-function fast path directly, falling back to `ncl_call` only for
unbound / not-a-function. Saves one Rust call per Lisp call.

**Correct and GC-safe** — stdlib + CLOS + conditions + Prolog all right,
and the full GC suite passed on this exact binary (alloc-test exact
32000000, stress 16-thread clean, gc-watch 0 pin leaks, all four
seh-unwind tests exit 0). GC-safety reasoning: no safepoint between the
code/env loads and the indirect call, and the callee roots its env param
exactly as the `ncl_call` path does.

**Methodology note — I almost rejected this on bad data.** A first pass
compared "RefCell" numbers from early in the session (fib 30 ≈ 0.049 s)
against "inline" numbers measured ~15 min and several heavy LLVM builds
later (≈ 0.085 s) and concluded it was 1.7× *slower*. That was a
**machine-state drift confound** — re-measuring the *identical* committed
RefCell binary at the later time also gave ≈ 0.093 s, i.e. the machine had
slowed ~1.9× (CPU freq/thermal), not the code. Lesson: never compare
benchmark numbers taken at different points in a build-heavy session.

Redid it as an **interleaved A/B** — saved both binaries, ran them
alternately (R,I,R,I…) so drift hits both equally, 7 rounds of fib 30:

| stat        | refcell | inline |
|-------------|---------|--------|
| mean (s)    | 0.094   | 0.084  |
| rounds won  | 1       | 6      |

~11 % faster, reproducible (one noisy outlier round). Modest next to the
RefCell 3.9×, but real and on the core dispatch path. Kept.

(Microbench caveat: fib/tak are pure-call, no allocation; allocation-heavy
real code spends a smaller fraction in call dispatch, so expect <11 % there.)

Committed as `1374340`.

### Compat: ALPHANUMERICP made Unicode-aware

Fixed the pre-existing red test from above. `core.lisp` was redefining
`alphanumericp` as `(or (alpha-char-p c) (digit-char-p c))`, *shadowing*
the registered native shim (Rust `char::is_alphanumeric`). Because
`digit-char-p` is radix-based (ASCII 0-9 / A-Z), the wrapper narrowed the
predicate so non-ASCII decimal digits answered NIL. Deleted the wrapper
(left a comment so it isn't re-added); the Unicode-aware native shim now
serves. `(alphanumericp (code-char #x0969))` (Devanagari ३) → T, while
`#\A`/`#\5` → T and `#\!` → NIL still hold.

### CL gap inventory (probed, current binary) — for future rounds

Still missing / wrong (none crash the process — all signal Lisp errors):
- `(expt <int> <neg>)` → "negative exponent not yet supported"; should be
  a ratio (`(expt 2 -3)` = 1/8). Native shim, needs ratio construction.
- `multiple-value-call` — undefined (needs compiler/special-form support).
- `(coerce 1 'double-float)` / `'single-float` → SIMPLE-ERROR.
- `parse-integer` ignores `:junk-allowed` / `:start` / `:end` / `:radix`
  (native, fixed-arity-1).
- `vector-push-extend` / fill pointers / adjustable arrays — undefined.
- multidimensional arrays — `(make-array '(2 3))` signals cleanly now (was
  a panic in the May review); `aref` is hard-wired to arity 2 (compile
  error on `(aref a i j)`).
- `~R` doesn't spell numbers (`(format nil "~R" 4)` → "4").

Lots now works that the May review flagged: vector sequences
(reverse/nreverse/some/every/count/reduce/sort), `read-from-string`,
`eval`, `equal`/`equalp` hash tables honoring `:test`, `~(~A~)` case
conversion, complex arithmetic.

### Compat: (expt <int> <neg-int>) now returns an exact rational

`expt_shim` (bignum.rs) signalled "negative exponent not yet supported"
for any negative integer exponent. Implemented it: for `n < 0`,
`(expt b n)` = `1 / b^|n|`, built with `BigRational::new(1, b^|n|)` and
collapsed through `ratio::bigrational_to_word` (which reduces a unit
denominator back to an integer). `BigRational::new` reduces to lowest
terms and normalises the sign onto the numerator, so every case is exact:

  (expt 2 -3) => 1/8     (expt -2 -3) => -1/8   (expt 10 -2) => 1/100
  (expt 1 -5) => 1       (expt 4 -1)  => 1/4    (expt 0 -2)  => clean error
  (expt 2 10) => 1024 (positive path unchanged)

Touches the numeric tower and allocates a ratio on the GC heap (same
allocation API the `/` operator already uses), so re-verified: 208/208
runtime tests, alloc-test exact 32000000, and a 50 000-iteration ratio
allocation loop runs clean under GC pressure. (Float-base `expt` —
`EXPT-FLOAT` exists but isn't wired into `EXPT` — remains a separate gap.)

