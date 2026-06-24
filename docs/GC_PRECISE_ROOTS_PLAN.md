# GC Precise Roots — implementation plan

> ⚠ **Largely implemented — historical plan.** This doc's premise
> ("NCL's JIT doesn't call `push_root` — that's the gap") is no longer
> true. Option B (explicit push/pop roots) landed: the JIT emits
> `ncl_push_root`/`ncl_pop_root` via `emit_safepoint_wrap`
> (`src/ncl-llvm/src/lib.rs`) around every GC-triggering call site, and
> `ncl_push_root`/`ncl_pop_root` are `extern "C-unwind"` entry points in
> `src/ncl-runtime/src/abi.rs`. Read this as the design rationale, not as
> outstanding work. Option A (LLVM `gc.statepoint`) remains a possible
> future optimisation.

*Written 2026-05-16 after `GC_CHUNKED_INVARIANTS.md` established that
no amount of guard-adding in `evac.rs` can make invariant I-6 hold
by construction. The chunked design is correct given precise roots;
all the iterative crashes were the JIT failing to report which
in-flight Lisp values are live across a call.*

This doc is the work breakdown. Once it's approved, the next code
change starts here, not in `evac.rs`.

---

## 1. Why precise roots, not more guards

`docs/GC_CHUNKED_INVARIANTS.md` §3.B traces every Life crash to the
same shape:

> A `Word` lives in some location the GC didn't visit, points at a
> cell that has been recycled, and the mutator dereferences it.

The two ways to make that impossible (§4 of that doc):

- **Zero-on-recycle.** Cheap. Already landed. Reduces the failure
  to "stale `Word` reads `Fixnum 0`," and the JIT's `(cdr X)`
  inlining now faults at `0x08` instead of `0x40`. The JIT's only
  type check on `(cdr X)` is `X != NIL` — `Fixnum 0` is not `NIL`,
  so the deref still happens. The remaining failure mode would
  require **every JIT-inlined cell op to type-check Fixnum vs cons**
  — a wider change than precise roots, and still no help for the
  underlying issue (stale Words shouldn't exist).
- **Precise roots.** Every live `Word` is registered as a root, so
  Phase 2 visits and rewrites it. Stale Words become impossible.

NCL's `MutatorState::push_root` / `pop_root` API has been there
since Phase 1. The JIT doesn't call it. That's the gap.

---

## 2. Two approaches

| | Option A: LLVM `gc.statepoint` | Option B: explicit `push_root` / `pop_root` |
|---|---|---|
| **Mechanism** | LLVM IR intrinsics; LLVM emits stackmaps for the runtime to read at GC time | JIT emits direct calls to `ncl_push_root` / `ncl_pop_root` around every helper call |
| **Where roots live** | LLVM-managed stack slots, located via the stackmap PC → slot table | The mutator's `roots: Vec<Word>` |
| **Runtime cost per call** | One safepoint poll (existing `abort_pending` check), no per-root store | One memory store per live root before each call, one load after |
| **Compiler work** | Plug in a `GCStrategy`, wrap every Lisp pointer in `gc.statepoint`/`gc.relocate`, parse stackmap section | Track live `IntValue`s in `locals`, emit push/pop around each `build_call`, reload locals from popped roots |
| **Runtime work** | Stackmap parser + per-cycle frame walk into `MutatorHandle` (the shape `stack_map.rs` already sketches) | Already implemented — `MutatorState` exposes `push_root`/`pop_root`/`roots` |
| **Failure mode if wrong** | Subtle: a missing relocate keeps a stale pointer in a register; needs stackmap audit | Loud: the locals Vec is out-of-sync and JIT'd code reads the wrong slot |
| **Estimated effort** | 5–10 days; non-trivial LLVM interaction | 2–3 days; straightforward IR emission change |

NCL's `the_seg_windup.md` notes that the `abort_pending` flag-check
after every call was the predecessor of this work — the original
plan was to ditch it once SEH unwind landed. Precise roots is the
*other* thing the per-call site needs.

**Recommendation: Option B.** Reasons:

1. It's transparent: every push/pop is in the emitted IR; a
   developer can `NCL_DUMP_IR=1` and see exactly which locals get
   rooted per call site. The crash traces we just built suddenly
   become diagnostically excellent again.
2. `stack_map.rs` is plumbed but not wired; Option A requires
   actually implementing the LLVM stackmap parser, which is
   ~hundreds of lines on top of the codegen change.
3. The ABI tax Option B imposes (one store + one load per call)
   is small. Life makes a few million calls; total cost ≪ the GC
   time we've already been spending.
4. Option A can land *later* as an optimisation, replacing Option
   B's IR with the intrinsic-based shape. The runtime side
   (`PageEvacuator::visit` over `mutator.roots`) doesn't change.

---

## 3. The exact IR change (Option B)

Today, a Lisp `(foo X Y)` call lowers (roughly) to:

```llvm
%X = ...                 ; SSA value with Tag::Cons (or whatever) low bits
%Y = ...
%args = alloca [2 x i64]
store i64 %X, ptr %args, ...
store i64 %Y, ptr %args+8, ...
%result = call i64 @ncl_call(ptr %mutator, i64 %foo_fn, ptr %args, i64 2)
%abort = call i32 @ncl_abort_pending()
br i1 %abort, ...
```

After Option B, every `build_call` that can trigger GC is wrapped:

```llvm
;-------- pre-call: root any live Lisp pointer --------
%L0 = ...                              ; existing locals
%L1 = ...
%X  = ...
%Y  = ...

call void @ncl_push_root(ptr %mutator, i64 %L0)
call void @ncl_push_root(ptr %mutator, i64 %L1)
call void @ncl_push_root(ptr %mutator, i64 %X)
call void @ncl_push_root(ptr %mutator, i64 %Y)

;-------- the call --------
%args = alloca [2 x i64]
store i64 %X, ptr %args, ...
store i64 %Y, ptr %args+8, ...
%result = call i64 @ncl_call(ptr %mutator, i64 %foo_fn, ptr %args, i64 2)

;-------- post-call: reload + pop in reverse order --------
%Y2  = call i64 @ncl_pop_root(ptr %mutator)
%X2  = call i64 @ncl_pop_root(ptr %mutator)
%L1_post = call i64 @ncl_pop_root(ptr %mutator)
%L0_post = call i64 @ncl_pop_root(ptr %mutator)

;-------- existing abort-pending check --------
%abort = call i32 @ncl_abort_pending()
br i1 %abort, ...

;-------- subsequent uses of L0/L1/X/Y must use the _post versions --------
```

Implementation in `ncl-llvm/src/lib.rs`:

- Add `ncl_push_root` / `ncl_pop_root` to the runtime helper table
  (`Helpers` struct + the `declare_runtime_helpers` block).
- Add a helper `emit_safepoint_wrap(builder, function, helpers,
  locals: &mut Vec<IntValue>, call: impl FnOnce(...) -> Result<...>)`
  that:
  1. Pushes every entry in `locals` via `ncl_push_root` (in order).
  2. Runs the caller-supplied `call` closure (which emits the
     actual `build_call`).
  3. Pops each root into a fresh `IntValue` and **replaces the
     corresponding entry in `locals`**.
  4. Returns the call's result.
- Wrap **every** `build_call` in `emit_expr` that targets a Lisp
  function (`ncl_call`, `ncl_funcall`, `ncl_apply`,
  `ncl_make_closure`) or any runtime helper that can trigger GC
  (anything that allocates: `ncl_alloc_cons`, `ncl_load_value` for
  unbound globals, `ncl_add_promote`, `ncl_build_rest_list`,
  `ncl_make_closure`, etc.).
- The Lisp function's incoming params (currently loaded via
  `Expr::Param(idx)` from the args pointer) are *also* Lisp values
  and must enter `locals` at function entry so they get rooted at
  the first safepoint.

### What does NOT need wrapping

- Calls to `ncl_abort_pending`, `ncl_set_mv_single`,
  `ncl_set_mv_many`, `llvm.sadd.with.overflow.i64`,
  `ncl_string_eq`, `ncl_equal` — these don't allocate. Helpers
  table comment can mark them as "non-GC."
- The terminating `build_return` — no further uses of `locals`
  after a return; nothing to root.

### How `locals` gets reloaded

The current `emit_expr` walks the AST recursively. When a child
expression contains a call, the parent's `locals` (its captured
values) need to be re-loaded after the call returns. That's the
*replace the entry in `locals`* step above.

For values that are *not* in `locals` but live in SSA across a
call (intermediate results held in registers), we have to either:

- Convert them to allocas at the start of any expression that
  contains a nested call, OR
- Spill them to `locals` before descending into the child, then
  reload after.

The second is simpler: before walking a child expression that may
emit a call, push every intermediate-result `IntValue` onto
`locals`. After the recursive `emit_expr` returns, pop the count
we added.

### Walker change estimate

In `emit_expr`, every `Expr` arm that:

- Computes a sub-expression *before* a call (e.g. `Expr::Funcall`,
  `Expr::Apply`, `Expr::If` with calls in either branch), and
- Uses those sub-expression results *after* the call,

needs the spill-into-locals dance. About 8–10 arms.

---

## 4. The runtime side

This is the easy half — it's already there.

`PageEvacuator::visit` is what gets called on every entry in
`mutator.roots` during Phase 2 of every chunk. Today the
production `visit_roots` closure (in `mutator.rs::do_minor_gc`)
walks `my_handle.roots` and every other parked mutator's
`roots`. So roots-the-Vec is already in the precise-mark + Phase-2
walk paths.

The only runtime change needed:

- Currently `MutatorState::push_root(w) -> usize` and
  `pop_root() -> Option<Word>` exist as Rust methods. They need a
  C ABI entry point (`ncl_push_root` / `ncl_pop_root` with
  `extern "C-unwind"`) for the JIT to call. Trivial.

---

## 5. Conservative-pin coverage after precise roots land

Once every Lisp pointer is rooted, conservative pinning's job
shrinks. Specifically:

- **Conservative scan can be removed** for the trigger thread —
  the JIT already roots everything live at the safepoint where it
  called the runtime helper.
- **For parked mutators**, conservative scan still matters until
  *those* mutators' JIT'd code is recompiled with safepoint
  wrappers too. So keep it on for now.

For NCL today, multi-threaded Lisp is exercised but life.lisp is
single-threaded. Single-thread Life with safepoint wrappers should
not need conservative scan at all.

We don't need to *remove* conservative pinning when this lands —
just stop relying on it. The current pin → extend-mark code can
stay as belt-and-suspenders. The G1 pin we recently added becomes
redundant (G1 refs are already rooted at the safepoint) but
shouldn't break anything.

---

## 6. Order of work, with tests

Each step ends with a checkpoint that can be verified before
moving on. **No re-running Life until step 6.** Avoid the
iterative-crash pattern.

| Step | What | Verification |
|---|---|---|
| 1 | Add `ncl_push_root` / `ncl_pop_root` C-ABI entry points; register in `Helpers` declarations | `cargo test` still 99/99 |
| 2 | Implement `emit_safepoint_wrap` helper (no callers yet) | `cargo test` still 99/99; visual review of the helper for correctness |
| 3 | Wrap `Expr::Funcall` (the most common path). Reload `locals` correctly | Add a unit test that pushes a root before a call, allocates many objects via the JIT'd call, asserts the popped value points at the new dest |
| 4 | Wrap `Expr::Apply`, `Expr::Call` (direct), `Expr::Lambda`'s `make_closure` call | Re-run the existing `ncl-llvm` integration tests |
| 5 | Wrap allocator-side helpers (`ncl_alloc_cons`, `ncl_alloc_boxed`, `ncl_load_value`, `ncl_add_promote`, etc.) | Hand-audit `Helpers` — mark which entries can trigger GC |
| 6 | Run Life under `gc-page-heap`. Expect either completion or a *qualitatively new* failure mode (not the I-6 family) | If completion: ship. If new failure: diagnose; likely a missed wrap in step 5 |

---

## 7. Anti-recommendations

- **Don't try to optimise away spills before the bug is fixed.**
  LLVM's register allocator will inline what it can after correctness
  is established.
- **Don't replace `push_root`/`pop_root` with stack-slot rooting
  yet.** That's the LLVM-statepoint Option A; it's a follow-up.
- **Don't remove zero-on-recycle once precise roots land.** It's
  cheap defensive infrastructure and the next moving-GC bug
  (whenever it appears, in some future workload) will produce
  cleaner failure modes with it in place. Keep it.

---

## 8. Open questions

1. Does the LLVM register allocator generate clean code for a
   function with N push_root/pop_root pairs in sequence? At what
   N does the prologue blow up? (Suspect: high; runtime helpers
   already have many.)
2. Do `ncl_push_root` / `ncl_pop_root` need to be `inline(always)`
   on the Rust side? Right now they're plain methods. The JIT
   calls them via global mapping; inlining is on the Rust side,
   not the JIT side, so no effect there. Should be fine.
3. What about lambdas that take many parameters via the `args`
   pointer? Each param is already a Lisp value. Need to root them
   on function entry, before the first inner call.
4. Is there any path where `locals` ordering matters for the
   *pop* sequence? (i.e., do we need a fixed depth at each call
   site, or is "push N, pop N" enough?) — should be enough as long
   as no exceptional control flow steals a root.
5. Cascade interaction: after this lands, `extend_mark_from_pinned(G1)`
   is still needed for the rare case of a Lisp pointer captured in
   a conservative-pin-only path (e.g. a thread that hasn't entered
   the wrapped codepath). Leave it in.

---

*Document state: ready for review. Next action: read this, decide
between Option A vs B (or accept Option B as recommended), then
start at step 1. Estimated time to step 6: 2–3 days of focused
work, no Life re-runs in between.*
