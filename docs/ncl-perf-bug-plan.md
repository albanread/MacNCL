# NCL — Performance & Bug Review + Fix Plan

*Produced 2026-06-13 by an 8-reviewer fan-out (JIT, runtime ABI/numeric
tower, GC integration, NewGC page heap, Lisp stdlib, compiler front-end),
with every finding piped through an adversarial verifier that confirmed
or refuted it against the actual code. 33 findings confirmed, 0 refuted;
deduped to 24 actionable items. The corruption-class and the
highest-stakes items were then re-verified by hand. Severities below are
the verifier's corrected values.*

---

## TL;DR

- **One latent GC-safety hole underlies the worst risk** (item 1): a heap
  object held only in a JIT register across a later GC-capable call is
  invisible to *both* the precise root `Vec` and the conservative *stack*
  scan (precise stack maps aren't wired up). Boxed floats are the most
  exposed instance, and this is the true root of the already-gated
  "inline-loop float footgun." Fixing it properly **supersedes the gate**
  (item 2) and is the foundation everything else can lean on.
- **Five correctness/corruption bugs** (items 1–5) land first, each behind
  a regression test.
- **The single biggest avoidable perf cost on the new float path** (item 6)
  is a *process-global mutex + hash* taken on **every numeric box** to
  re-intern a constant marker symbol — trivially cacheable.
- **A cluster of quadratic stdlib regressions** (items 7–10, 16–17): the
  live `sequences.lisp` overrides index *lists* with `elt`.
- **NewGC per-cycle O(reservation) sweeps** (items 11, 13–15) that the
  existing `pages_high_water` / `words_used` bounds already make easy.

---

## Part A — Correctness & corruption bugs (do first, test-gated)

### 1. Boxed heap objects aren't precisely rooted across later GC-capable calls  ·  **corruption · large**
`coerce_to_word(F64)` → `ncl_box_float` → `alloc_float` polls the safepoint
and can drive/await a GC. The fresh young-heap `Float` Word is returned as
a bare SSA value. `Expr::Call`/`Funcall`/`Apply` (lib.rs ~4211 / 3348 /
3410) evaluate **all** arguments into registers first and only root them
at the final dispatch — so evaluating a *later* sibling can run a GC that
evacuates an *earlier* already-boxed argument. NCL's conservative pinner
scans only the Rust **stack window** (`heap.rs` `pin_pointers_in_range`),
never the JIT register file; precise stack maps (`stack_map.rs`) are *not
wired up* — the working contract is `push_root`/`pop_root` via the
safepoint wrap. At `-O2` a box can ride a callee-saved register no frame
spills → invisible to both. Result: stale pointer → silent heap
corruption. The `float.rs:62-64` comment ("kept alive … by the
conservative stack pin while it lives in a JIT register") is exactly
backwards.

**Honest scope:** *not* float-specific — the same inter-sibling window
exists for cons/vector/string/closure results and predates floats; the
multithreaded conservative-pin stress tests (`tests/conservative-pin/`)
exercise structurally identical cons patterns and pass, so the trigger is
**register-allocation-dependent and probabilistic, not deterministic**.
Floats are marginally *more* exposed (conses have a forwarding fallback;
headered floats rely on pinning). Ranked #1 on **consequence**, not
observed frequency.

**Fix:** make `coerce_to_word(F64)` store the fresh box into a GC-rooted
slot (a tracked alloca / `extra_roots` entry that `emit_safepoint_wrap`
reloads post-GC) before any subsequent alloc/call. Equivalently, in
Call/Funcall/Apply, evaluate each arg → store into the `call_args` alloca
→ extend the rooted region over the filled prefix → then evaluate the
next arg. Correct the false comments. *The real long-term cure is
wiring up precise stack maps so the JIT register file is covered — but
the rooted-slot discipline closes the float sites now.* This fix makes
in-loop boxing safe and **removes the need for the item-2 gate entirely.**

### 2. Inline-loop gate has a hole: `special` + `double-float` computed-init `let`  ·  **corruption · small**
The gate I added (`form_inline_safe` `computed_ok`, lower.rs ~1315) accepts
a computed-init binding iff the name is float-declared — with no
*special*-variable check. But `lower_let` only unboxes a binding when it
is **not** special (`is_float_local`, lower.rs:1551-1555); a special
binding's float init is boxed and emitted under `DynamicBind`. So a
`(declare (special x) (double-float x))` computed-float `let` inside an
auto-inlined loop passes the gate yet still boxes per iteration across the
`ncl_dynamic_bind` safepoint — the exact hazard the gate exists to forbid.
Narrow but legal/reachable. **Subsumed by item 1** (rooting the box makes
it safe); until then, also require `!is_special` in `computed_ok` (compute
the special set via `extract_special_names` + `coord.is_special`, as
`lower_let` does at ~1517).

### 3. `InlineLoop` leaves locals/params holding non-dominating SSA at exit  ·  **correctness · small**
`FastLoop` resets every slot to its dominating header-phi snapshot at exit
(lib.rs ~4095); `InlineLoop` (~4169) builds the result phi without
resetting. If the body had an allocating safepoint-wrapped call, the
safepoint reload reassigned the slots to body-latch SSA values that don't
dominate the exit block (reached only via `LoopBreak` from mid-body).
Code after a non-tail `InlineLoop` then reads a non-dominating value — and
with no module verify before JIT, that skews to a miscompile (publishing
garbage as a GC root or reading it as the local). **Fix:** after
`position_at_end(exit)`, before the result phi, reset each `locals[i]` /
`params[i]` to `local_phis[i]` / `param_phis[i]`. Test: an allocating call
in the body + a post-loop read of the slots.

### 4. `ncl_cmp_real` treats NaN as equal  ·  **correctness · small**
`float.rs:290` maps `partial_cmp == None` (a NaN operand) → `0` (the equal
sentinel). The comparison shims read `cmp {==,<=,>=} 0` as truth, so on
NaN: `=`, `<=`, `>=` all return `T` and `/=` returns `NIL`. IEEE-754/CL:
NaN is unordered — all of `=,<,>,<=,>=` must be `NIL`, `/=` must be `T`.
**Fix:** NaN-guard the six *comparison* shims only (not `ncl_cmp_real`'s
`None` globally — `cmp_full_typed` is also the `eql`/`equal` engine and
`(eql nan nan)` must stay `T`).

### 5. Chained comparisons double-evaluate middle operands (≥3 args)  ·  **correctness · small**
`chainable_cmp`'s 3+ arm (lower.rs ~3039) lowers each arg once into a vec,
then builds pairwise comparisons from `lowered[i].clone()` and
`lowered[i+1].clone()`. `Expr` is deep-`Clone` (no `Rc`), so each interior
operand emits as **two** IR subtrees that both execute. CL requires each
form evaluated exactly once, L-to-R — so `(< f g h)` evaluates `g` twice;
`(= (incf x) (incf y) (incf z))` increments `y` twice. **Fix:** bind each
operand to a fresh `Expr::Local` temp once, compare the temps (mirror
`lower_or`'s single-temp pattern ~1854). Fix the stale doc comment that
claims let-binding already happens.

---

## Part B — Performance

### Hot path
**6. Every numeric box takes a global mutex + hash to re-intern its marker  ·  perf-major · small.**
`alloc_float` calls `m.coord().intern("%FLOAT")` on *every* box (float.rs:30;
likewise ratio.rs:99, bignum.rs:41, complex.rs:125). `intern` locks the
process-global `intern_table` `Mutex<HashMap>` and hashes the string —
serializing all mutator threads and adding lock+hash latency to what is
otherwise a lock-free TLAB bump, on the float-escape hot path. **Fix:**
intern each marker once at `GcCoordinator` construction, cache the raw
`Word` (`OnceLock<Word>` / fields); the four allocators read the cached
Word. Stable because the symbol lives forever in the static area.

### Quadratic stdlib (the live `sequences.lisp` overrides index lists with `elt`)
- **7. `reduce`** folds via `elt` → O(n²) on lists (seq.lisp 211–232). *perf-major · medium.*
- **8. `map`** indexes every input with `elt` → O(m·n²) (157–198). *perf-major · medium.*
- **9. `concatenate`** to a string grows via `string-append-char` → O(n²) (137–141). *perf-major · small.*
- **10. `search`** uses `elt` on a list haystack → O(n²·m) (462–492). *perf-major · medium.*
- **16. `mismatch`** element-by-`elt` → O(n²) (435–460). *perf-minor · small.*
- **17. `map`/`remove`/`substitute` string branches** grow via `string-append-char` → O(n²) (192–197, 335–342, 385–396). *perf-minor · small.*

**Fix pattern:** add `listp` car/cdr fast paths (mirroring the `find`/
`position` fast path already in the file; or delegate to the still-defined
core walkers), keep `elt` only for vectors/strings; two-pass
`(make-string n)` + indexed `(setf (char …))` for string builders
(the `string-upcase` pattern). All become O(n).

### NewGC per-cycle O(reservation) sweeps
- **11. `rebuild_cards_for_old_gens`** scans full 8192-cell page capacity ignoring `words_used` (space.rs 895–944) — a 1%-live Tenured page pays a full-page classify every minor. *perf-major · small.* **Fix:** bound the cell scan to `words_used`; clear cards past it without reading memory (pages are zeroed on acquire).
- **13. `collect_minor` clones the whole descriptor table** every minor (cycle.rs:156, *twice* on the production path) — ~384–512 KB memcpy proportional to reservation, not live set. *perf-minor · small.* **Fix:** clone only `[..pages_high_water]` (or borrow the slice / reuse a scratch Vec).
- **14. `phase2_rewrite` + other sweeps** iterate the full `descs` instead of the `pages_high_water` prefix (evac.rs 1091–1105; space.rs 1292/1326/682/728). *perf-minor · small.* **Fix:** apply the existing high-water bound consistently.
- **15. `count_pages_in_gen` (O(n_pages))** on the TLAB-refill / fresh-page path (coordinator_api.rs:489, alloc.rs 303/402/507) — scans 32K descriptors per refill on a near-empty heap. *perf-minor · medium.* **Fix:** maintain a running per-generation page counter (like the free-page list) and compare O(1).

### JIT codegen
- **12. Arg-array allocas emitted at the call site** land inside loop bodies → re-reserved every iteration (lib.rs 4218/3355/3423/3288). *perf-minor · small.* **Fix:** emit fixed-size arg allocas in the entry block once (the `f64_slot_ptr` save/restore pattern ~2654); GEP+store at the call site. (Arrays escape by pointer, so the win is only the per-iter stack adjust — hence minor.)
- **18. Float arithmetic tower re-decodes tags ~6× per op** (float.rs mul 219–236, add 176–196, sub 198–216, div 251–266) — `ncl_mul_complex→full→float→to_f64` re-runs `is_complex`/`is_float` on the same objects. Slow path only (JIT fast paths bypass it). *perf-minor · small.* **Fix:** decode `heap_numeric_type` once and thread down, like `cmp_full_typed`.
- **19. Lambda bodies discard declares** (lower.rs:2452) — a lambda whose body declares `double-float` never unboxes its params or enables loop auto-inline. *perf-minor · small.* **Fix:** mirror the `defun` float handling (`extract_float_names` → `mark_float_decl` → `rebind_as_param_f64`).
- **20. Float-loop gate over-restrictive for `let*` chains** — `let*` desugars so the outer `let` loses the inner declare; the computed float init fails `computed_ok` → loop refuses inlining (safe, but leaves the fast path on the table). *perf-minor · medium.* **Fix:** thread per-binding float declares through the `let*` desugar (fixes both `form_inline_safe` and `lower_let`); do **not** merely relax the gate (reintroduces the item-2 hazard).
- **21. `ncl_abort_pending` emitted as an opaque uninlinable call** after every Lisp call (lib.rs ~1976 + ~14 sites) to read one TLS bool — backend can't inline/hoist it. *perf-minor · medium.* **Fix (min):** mark the decl `willreturn` + `memory(inaccessiblemem: read)` (not `nounwind` — it's `extern "C-unwind"`). **Better:** move the flag onto `MutatorState` (arg 0) and emit the load+branch inline, as root push/pop was inlined.

### Micro
- **22. `every`/`some` multi-list** cons two fresh N-lists per step via `%cars-of`/`%cdrs-of` (core.lisp 100–108, 162–202; `mapcar`/`mapc` share). *perf-minor · medium.* **Fix:** walk N parallel cursors in let-locals + an apply buffer; or dedicated `every-2`/`some-2`.
- **23. `bignum_to_bigint` allocates an extra intermediate Vec** per operand read (bignum.rs 87–99). *perf-minor · small.* **Fix:** push the two u32 halves straight from the heap read into the preallocated vec.
- **24. `SETQ`→`SETF` symbol-macro rewrite re-macroexpands** value forms twice (macroexpand.rs 514–540). *perf-minor · small.* **Fix:** build the progn from already-expanded pieces; expand only the `setf` place; skip the outer re-walk.

### Closure allocation — capturing closures leak Function records into static (finding 2026-06-15)
- **25. Every evaluation of a *capturing* `(lambda …)` permanently consumes static-area memory.**  *correctness (latent OOM) + perf-major · medium.*
  `ncl_make_closure` (abi.rs:648) allocates the Function record via `alloc_function_in_static` (gc_function.rs:61) — the static area is **never reclaimed** (gc_function.rs:1–8, abi.rs:654–664). So a capturing closure created in a loop leaks one ~48-byte Function record per evaluation, plus a young-heap env Vector. abi.rs:654–664 documents *why* it's static: the young-heap path was tried and reverted — conservative stack-pinning + promote-on-first-survival turned every transient into a tenured object and spiralled to OOM; the documented cure was "tighten pinner + age-threshold promotion" at the GC layer.

  **Evidence (`(gc-stats)`, 1M evaluations each, release build):**
  | workload | ΔSTATIC-USED | ΔYOUNG-USED | minor GCs |
  |---|---|---|---|
  | no-capture `(lambda (x) (+ x 1))` *(post-elision)* | **+1.6 KB** (one cached closure) | 0 | 0 |
  | capturing `(lambda (x) (+ x k))` | **+48 MB** (Function records, **leaked**) | +16 MB (env Vectors, GC-reclaimable) | 0 |

  At ~48 B/closure, ~22M capturing-closure evaluations exhaust the 1 GB static area → hard OOM. A long-running `(loop … (mapcar (lambda (x) (+ x k)) …))` will eventually crash. The dominant cost is the **static leak**, not young-GC pressure (the env Vectors *are* reclaimable).

  **Status:** the *no-capture* case is fixed — `perf(jit): elide no-capture closures` (01d351b) allocates the record once at compile time and embeds the Word as an IR constant (a no-free-var lambda is a constant; closure identity for a stateless lambda is CL-implementation-defined). The *capturing* leak is **deferred by decision (2026-06-15)** — recorded here, not yet scheduled.

  **Fix options (when scheduled):**
  - **(A) Young-allocate + reclaim** the Function record so dead closures are collected. The page-heap (now the only backend) already has age-threshold promotion (G0→G1→Tenured), which lifts *one* of the two documented blockers; it still uses conservative stack pinning, so this needs careful rooting validation (cf. the old `demos/life.lisp` gen-25 `lambda_1317` crash). Touches the GC alloc path → gated by the "don't touch GC without a very good reason" rule (a latent OOM is a good reason, but validation cost is real). Fixes *all* capturing closures, escaping or not.
  - **(B) Stack-allocate non-escaping closures** (Tier-2 escape analysis): closures that provably don't outlive their creating frame go on the stack — avoids the heap entirely, no GC code touched, aligns with the "avoid GC" framing. Larger compiler change; does **not** help closures that genuinely escape (those still need a reclaimable home — i.e. still want (A)).

---

## Sprint plan

1. **GC value-safety** — item 1 (root boxed objects at creation), item 2 (gate hole), item 3 (InlineLoop slot reset). Each behind a **float-allocation GC-stress** regression test. Item 1 should also dissolve the Gap-2 gate.
2. **Numeric correctness** — items 4 (NaN-unordered), 5 (chained-compare double-eval), with conformance tests.
3. **Allocation hot path** — item 6 (cache marker symbols; kill the per-box global mutex+hash).
4. **Stdlib de-quadratication** — items 7–10, 16–17 (`listp` fast paths + two-pass string builders).
5. **NewGC per-cycle bounds** — items 11, 13–15 (`words_used` card bound, `pages_high_water` on snapshots/sweeps, O(1) per-gen counters).
6. **JIT codegen leverage** — items 12, 18, 19, 20 (entry-block allocas, decode-once tower, lambda + `let*` float-decl propagation).
7. **Micro-cleanups** — items 21–24 (abort-check inlining, bignum/div redundant work, `every`/`some` consing, macroexpand re-walk).

## Caveats

- **Item 1's trigger is probabilistic** (register-allocation-dependent), not a deterministic repro — it's ranked on consequence. The general register-residency gap (not just floats) is the deeper issue; precise stack maps are the long-term cure, the rooted-slot discipline is the near-term fix.
- The **gauntlet** (`bench/gauntlet.lisp`) and **ANSI** (`demos/ansi-runner.lisp`) are the regression gates; note ANSI's pass count is non-deterministic by ±1 (the chapter-3 `(< (zap 5 3) 3)` example uses `random`).
