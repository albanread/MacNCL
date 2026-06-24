# Build node 2 — from arithmetic Lisp to a Lisp

*Written 2026-05-10, after commit `5cebcec`. 29 commits on `main`.*

Node 1 ([build_node_1.md](build_node_1.md)) ended with NCL
evaluating recursive arithmetic and conditional list-walking
programs. It could do `fact`, `length`, `sum-list` — but every
function had a single-form body, every reference to a name had to
be a parameter or `T`, every conditional was nested `if`, and there
were no first-class functions.

Node 2 took us from there to:

```
$ ncl --eval "(defun compose (f g)
                (lambda (x) (funcall f (funcall g x))))
              (defun map-list (f lst)
                (if (null lst) nil
                    (cons (funcall f (car lst))
                          (map-list f (cdr lst)))))
              (map-list (compose (lambda (x) (* x x))
                                 (lambda (x) (+ x 1)))
                        '(1 2 3 4 5))"
(4 9 16 25 36)
```

That's a real Lisp. Higher-order functions, closures, `compose`,
list mapping, lambda literals, quoted list constants. Compiled
through LLVM, with proper lexical scope, atomic function cells,
and the multi-threaded GC sitting underneath.

This document captures the path.

## The plan that emerged

When node 1 closed, the list of "missing for a believable Lisp"
read:

```
1.  defun, recursive functions   ← already done in node 1's last
                                    commit, technically
2.  let, progn, implicit progn
3.  Numeric comparisons + type predicates
4.  not, and, or, cond
5.  list, quoted symbols, quoted lists
6.  defparameter, setq
7.  lambda, closures, funcall   ← the heavy lift
8.  equal, string, more stdlib
```

We worked them in roughly that order, in seven commits. Each was a
small, testable step; closures (the seventh) was much larger than
the rest combined.

## The arc

### `let`, `progn`, implicit progn (commit `4ee69cb`)

The first real lexical scope. `let` bindings are added to the
`LocalEnv` with a new `Binding::Local(usize)` kind, looked up in
the same way as `Param`. The emitter manages a `Vec<IntValue>` for
local SSA values, pushed during a let scope and truncated on exit.

Implicit progn arrived together: defun bodies and let bodies that
have multiple forms get wrapped in `Expr::Progn` automatically.

```lisp
(defun hypot-sq (a b)
  (let ((aa (* a a))
        (bb (* b b)))
    (+ aa bb)))
```

This was the "if I had to write one Lisp program right now, I'd
need this" commit.

### Numeric comparisons + type predicates (commit `79f83d3`)

`<`, `>`, `<=`, `>=`, `=`, `null`, `consp`, `atom`, `listp`. All
trivial in retrospect — every comparison is a `build_int_compare`
plus a select between `Word::T` and `Word::NIL`; type predicates
are tag-bit checks.

The fixnum-tag trick paid off again here: signed comparison on
tagged Words is correct because shifting both operands left by 3
preserves ordering.

The unlock: **fibonacci**. After this commit, a recursive `fib`
function works.

```
$ ncl --eval "(defun fib (n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2)))))
              (fib 15)"
610
```

### `not`, `and`, `or`, `cond` (commit `bfb3ccd`)

Pure source-level desugaring. No new IR, no LLVM emit changes.
Each form rewrites to existing primitives:

```
(not x)       ≡ (if x nil t)
(and a b c)   ≡ (if a (if b c nil) nil)
(or a b)      ≡ (let ((tmp a)) (if tmp tmp b))   ; let avoids
                                                  ; double-evaluating
(cond ((t1 b1) (t2 b2) ...)
              ≡ (if t1 b1 (if t2 b2 ...))
```

The `or` form was the trickiest because each argument must be
evaluated at most once. The classic fix — bind the test result to
a let-local and reference it twice — is built right into the
desugaring.

After this, multi-branch logic is finally readable. `cond` is a
real upgrade over nested `if`.

### `list`, quoted symbols, quoted lists (commit `52ded4e`)

The first commit that interleaves runtime and compile-time
allocation: `(list 1 2 3)` is just `cons` chaining (runtime);
`'(1 2 3)` allocates the cons chain in the **static** area at
compile time, embedding a constant Word in the JIT'd code.

Static cons-cell allocation joined `try_alloc_with_header` in
`StaticArea`. Quoted list literals live forever; two references to
`'(1 2 3)` aren't `eq` because each `quote` form allocates its own
chain (sharing is a future optimisation).

Symbol names: quoted-symbol references print as their names, not
as `<symbol>`. Got there with a process-global `sym_names`
registry — a workaround until proper string allocation lands. The
intern function updates the registry; the printer reads from it.

```
$ ncl --eval "(defun classify (n)
                (cond ((< n 0) 'negative)
                      ((= n 0) 'zero)
                      (t 'positive)))
              (list (classify -3) (classify 0) (classify 5))"
(NEGATIVE ZERO POSITIVE)
```

### `defparameter`, `setq`, global value cells (commit `475f9b2`)

The other half of the Symbol's `AtomicU64` cell pair, finally
exercised. `defun` swaps the function cell; `defparameter`/`setq`
swap the value cell.

Two new IR variants: `Expr::LoadGlobal(u64)` and
`Expr::StoreGlobal { sym_word, value }`. Two new ABI helpers:
`ncl_load_value` (acquire load, panics on unbound) and
`ncl_store_value` (release store).

Bare symbols in expression position now lower to `LoadGlobal`
rather than erroring, with the runtime deciding bound-or-not. The
condition system that turns "unbound variable" into a proper Lisp
error still doesn't exist — for now, it's a process-terminating
panic. We accept it.

The unlock: **state**. Counters, accumulators, anything that lives
across function calls.

```
$ ncl --eval "(defparameter *counter* 0)
              (defun bump () (setq *counter* (+ *counter* 1)))
              (bump) (bump) (bump)
              *counter*"
3
```

### Lambda, closures, funcall (commit `5cebcec`) — the big one

The architectural change. Function objects grow from 4 cells to 5
— gain an `env` field. Calling convention shifts from `(mutator,
args, n_args)` to `(mutator, env, args, n_args)`. Every JIT'd
function takes the same shape; defun'd functions get `env=NIL`,
closures get a `Vector` of captured values.

The compiler does **lazy capture**: `LocalEnv` for a lambda body
has a `capture_parent` reference. When a name doesn't resolve in
the inner env, the resolver tries the parent; on hit, it adds a
new `ClosureRef(idx)` binding to the inner env and records the
outer-scope expression to evaluate at lambda-construction time.
Only names actually referenced get captured.

`(make-adder 5)` evaluates as:

1. `make-adder` is called with `n=5`, runs JIT'd code.
2. The body is a `Lambda` expression with body `(+ x n)` and one
   capture: the outer scope's `Param(0)` (i.e. `n`).
3. Emitting the `Lambda` Expr at runtime:
   - Builds a stack array `[5]` (the capture values)
   - Calls `ncl_make_closure(mutator, code_addr, arity=1, captures=&arr, n_caps=1)`
   - `ncl_make_closure` allocates a `Vector` in young with `[5]`,
     allocates a `Function` in static with `env = the Vector`,
     marks the static card (because static→young).
4. The Function-tagged Word flows out as `make-adder`'s return.

`(funcall (make-adder 5) 10)`:

1. Evaluates `(make-adder 5)` (returns the Function above).
2. Evaluates `10`.
3. Calls `ncl_funcall(mutator, fn_word, &[10], 1)`.
4. `ncl_funcall` extracts `env` and `code_ptr` from the Function,
   calls `code_ptr(mutator, env, args, n_args)`.
5. The code's body reads `Param(0)` (= 10 from args) and
   `ClosureRef(0)` (= 5 from env), adds, returns 15.

Every piece of the GC machinery comes alive in this commit:
- The Function object's `env` cell, written once at closure
  creation, is the only static→young pointer; the card barrier
  fires for it.
- The Vector in young is a real heap object scanned during minor
  GC.
- The atomic function cell continues to be the redefinition path
  (still works).

Eleven new tests including `compose`, `map-list`, `filter`,
nested closures, multi-capture closures, and `apply-n` (a function
that applies its function argument N times via tail-style
recursion).

## What's working at end of node 2

```
$ ncl --eval "(defun double (x) (+ x x))
              (defun compose (f g) (lambda (x) (funcall f (funcall g x))))
              (defun map-list (f lst)
                (if (null lst) nil
                    (cons (funcall f (car lst)) (map-list f (cdr lst)))))
              (defun filter (pred lst)
                (cond ((null lst) nil)
                      ((funcall pred (car lst))
                       (cons (car lst) (filter pred (cdr lst))))
                      (t (filter pred (cdr lst)))))
              (defun length-of (lst)
                (if (null lst) 0 (+ 1 (length-of (cdr lst)))))
              (defparameter *xs* '(1 2 3 4 5 6 7 8 9 10))
              (length-of (filter (lambda (x) (> x 5))
                                 (map-list double *xs*)))"
8
```

That's user-defined `compose`/`map-list`/`filter`/`length-of`,
acting on a `defparameter`'d list of integers, doing real
higher-order functional work. The result `8` is correct: doubling
`(1..10)` gives `(2 4 6 8 10 12 14 16 18 20)`, of which 8 are
greater than 5.

**Test counts:**
- ncl-runtime: 106 (unchanged from node 1 — the GC stays put)
- ncl-reader: 66 (unchanged)
- ncl-compiler: 97 (was 43 at end of node 1 — the language grew)
- ncl-llvm: 16 (closure-aware emit landed here)
- ncl-ir: 2
- ncl-tests / ncl-corman-demos: 3

Total: ~290 tests, all green.

## What we deferred

A handful of items that come up but aren't yet wired:

1. **Mutable lexical bindings.** `(let ((n 0)) (lambda () (setq n (+ n 1)) n))`
   doesn't work — the `setq` of a let-local errors. Real CL
   compiles closed-over mutable state by promoting the variable
   to a heap cell. We can do this when needed.
2. **Strings as first-class values.** Reader produces them, but
   the compiler doesn't lower them, the runtime can't allocate
   them, the printer can't print them. The path is obvious
   (parallel to what we did for cons in static); it's just work.
3. **`equal`** — structural cons-tree equality. ~30 lines once we
   have it as a builtin or written in user-Lisp.
4. **`format`** — formatted output. Pretty essential for any
   program with user-visible behavior.
5. **Sharing of `'(1 2 3)` literals.** Each `quote` form allocates
   fresh. SBCL etc. share. Optimisation, not correctness.
6. **Condition system.** Unbound variables and undefined functions
   panic across the FFI boundary. Real CL raises conditions you
   can catch.
7. **Sequence stdlib in user-Lisp.** `length`, `reverse`,
   `append`, `mapcar`, `member`, `assoc`. All easy now that
   closures exist.

The next natural arc — node 3 — is probably "fill in the
language" rather than another architectural shift: strings,
`equal`, the basic sequence library, `format`. After that, we'd
be at "small Corman demos start running."

## Decisions worth re-reading later

In addition to the ten from node 1:

11. **Function layout grew, didn't fragment.** When closures
    landed, we extended Function to 5 cells rather than splitting
    closure-Function from defun-Function. Result: one calling
    convention, one dispatch path, env=NIL for non-closures. The
    cost (one extra cell per defun'd function) is rounding error.

12. **Lazy capture.** A lambda only captures names it actually
    references. `(lambda (x) (+ x 1))` captures nothing even
    inside a function with twenty other locals in scope. The
    `find_or_capture` resolver looks up names, falls through to
    the parent on miss, captures only on hit. Each capture
    expression is recorded once.

13. **Boolean operators desugar at lower time.** No `Expr::And`
    or `Expr::Or` variants. They rewrite to `if`+`let` directly
    in `lower`. Keeps the IR small; the LLVM emitter never sees
    `and`. Same approach for `cond`. The simplicity rule paying
    off.

14. **Quoted compound data lives in static.** Allocated once at
    compile time, never moved, never collected. The Vector for a
    closure env, by contrast, lives in young. Two different
    storage stories for two different lifetime profiles.

15. **The "process-global registry" pattern.** When something
    needs to be looked up across the runtime/printer boundary
    (currently: symbol names), a small `OnceLock<Mutex<HashMap>>`
    is the v1 answer. Not pretty; works; replaceable when the
    proper structure (string-in-static + symbol's name field)
    arrives.

## Where it leaves us

NCL is now a Lisp — a small one, but a real one. You
can write `compose`, `map-list`, `filter`, `length`, `member`,
`reverse` (almost), and have them work. The architecture has
held: every commit since the GC build has been "add a feature,"
not "redesign a subsystem."

Node 3 will start with strings and `equal`, then build a small
stdlib in user-Lisp, then probably a Corman demo. The walking
boots are still on.
