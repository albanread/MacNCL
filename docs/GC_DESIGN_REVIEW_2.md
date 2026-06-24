# GC Design Review #2 — Mid-Evac Recycling and the In-Heap Forwarding Constraint

> ⚠ **Historical snapshot.** "Design 2 (two-phase) is the immediate
> direction" has since landed as block-incremental two-phase evacuation
> in `src/ncl-runtime/src/page_heap/evac.rs`, and precise roots are wired
> in the JIT. The retention numbers here (e.g. "957 live pages",
> "21 MB live for a 40-cons game") predate precise roots and should be
> re-measured before being trusted. Verify against the code and
> `docs/GC.md`'s integration-status block.

*Updated after the pre-BFS sweep histogram landed. This version retires the
original arithmetic mistake and records the remaining decision clearly.*

This document reviews the current state of the page-heap garbage
collector, the specific failure blocking `demos/life.lisp`, the
diagnostic data we now have, and the structural options on the table.

Audience: anyone touching `src/ncl-runtime/src/page_heap/` who needs to
understand where the current design actually breaks.

---

## 1. Current state

### What works under `--features gc-page-heap`

- 315 page-heap unit tests pass (all of `src/ncl-runtime/src/page_heap/*`
  plus `mutator::tests`).
- `cargo build` is clean under both `gc-semispace` (default) and
  `gc-page-heap`.
- `ncl --lean --eval "(+ 1 2)"` runs to completion.
- `ncl --windows -l demos/hellowin.lisp` opens and runs steady-state.
- All 18 `ncl-tests` integration test files (~170 Lisp-eval tests) pass
  under page-heap with per-test `(gc-stats)` reporting via the
  `TestSession` Drop guard.

### What doesn't work

- `demos/life.lisp` (the deliberate GC stressor) stalls partway through.
  The structured `GcStallError` payload is now actionable. Latest panic:

  ```
  gc-stall: reason=MidEvacOOM
    pages(free/g0/g1/tenured) = 0/1280/0/0
    pinned-pages              = 37
    pin-set                   = 134
    reserve-pages             = 320
    copied(objects/cells)     = 874627/2637454
    mark(live-bytes/live-pages/zero-live-pages-released)
                              = 20195080/957/3
    recycled-mid-evac         = 0
    static(used/committed)    = 96513584/97517568
  ```

- The pre-BFS sweep histogram has now been captured:

  ```
  [pre-bfs sweep] target=G0 total=960 zero=3 zero_unpinned=3 zero_pinned=0
                  nonzero_unpinned=937 nonzero_pinned=20
  [pre-bfs sweep] releasable.len()=3
  ```

### The pieces that landed since the last review

- **Build-time feature selection** replaced runtime trait dispatch.
  `Mutex<Box<dyn HeapBackend>>` became `Mutex<gc::Heap>` where `gc::Heap`
  cfg-resolves to either the semispace or page-heap concrete type.
- **`young_try_alloc_slab` contract fix**: returns
  `Option<(NonNull<u64>, usize)>` so the mutator cannot bump past the
  granted slab boundary.
- **Proportional reserve**: page-heap reserves `page_count / 4` pages for
  GC use (was a fixed 8). The mutator's slab path refuses to acquire when
  free-page count is `<= reserve`.
- **Structured `GcStallError`** with page-state histogram, copy progress,
  pin counts, and mark counts. Replaces `.expect()` panic and becomes a
  Lisp `gc-stall` condition at the native boundary.
- **Pre-evac mark pass** (`mark_minor_with_static` in
  `coordinator_api.rs`). Uses the same root closure as evacuation,
  streamed via `MarkScanner`. Seeds per-page `recycle_live_counts` from
  popcount of the mark bitmap.
- **Pre-BFS sweep** (`release_zero_live_unpinned_pages`): releases G0
  pages whose `recycle_live_counts == 0 && !has_pins()`.
- **Mid-BFS recycler scaffolding**: decrement `recycle_live_counts` on
  each copy-out; release page when the counter hits zero. Bookkeeping
  protects against double-release of pages reused mid-cycle as dest.
- **Game of Life stressor** (`demos/life.lisp`): deliberately inefficient
  list-based implementation that allocates ~1000 cons cells per
  generation.

---

## 2. The mid-evac OOM, correctly reconstructed

The page-heap stalls because evacuation's destination allocation exhausts
the available `Free` pages mid-BFS.

The original reading of the panic payload was wrong. The corrected
sequence is:

- At panic time, `pages(free/g0/g1/tenured) = 0/1280/0/0`, but that is
  after the BFS has already consumed the 320-page reserve and converted
  those pages into `G0` destinations.
- At pre-BFS sweep time, the target generation contains **960 pages**, and
  the reserve still sits in `Free`.
- Mark identifies **957 of those 960** `G0` pages as having at least one
  reachable cell.
- That leaves **exactly 3** zero-live pages, all unpinned.
- The pre-BFS sweep releases all 3 of them. The sweep is correct.
- BFS therefore starts with **323 destination pages available** (320
  reserve + 3 reclaimed), copies **322 pages worth of survivors**, and
  then needs one more page. No more `Free` pages. Stall.
- Mid-BFS recycler fires 0 times because with in-heap forwarding, none of
  the 957 source pages containing live objects can be safely reused before
  rewrite completes.

### The actual problem structure

There are two active issues now:

**Immediate structural problem**: the collector needs more destination
space than `reserve + zero-live pre-BFS reclaim` can provide, because
almost every `G0` page already has at least one reachable object on it.

**Parallel retention problem**: the live set for `life.lisp` is still far
too large. Whether that is genuine workload liveness or a rooting /
retention bug is unresolved.

What is *not* a problem anymore: pre-BFS reclaim logic. The histogram
closed that question.

---

## 3. Bug A retired — the pre-BFS sweep is correct

The original version of this review assumed:

- `last_mark_live_pages = 957`
- `G0 pages = 1280`
- therefore `323` pages should be zero-live at sweep time

That inference was wrong because it mixed panic-time `G0` with pre-BFS
`G0`.

The actual runtime data is:

```text
[pre-bfs sweep] target=G0 total=960 zero=3 zero_unpinned=3 zero_pinned=0
                nonzero_unpinned=937 nonzero_pinned=20
[pre-bfs sweep] releasable.len()=3
```

That means:

- `total=960` is the actual `G0` page count at pre-BFS time.
- `last_mark_live_pages = 957` is consistent with the histogram.
- only 3 target-generation pages have zero marked cells.
- all 3 are unpinned.
- all 3 are released.

So the sweep logic is doing exactly what it should do. There is no broken
predicate, no seeding mismatch, and no evidence here of massive
conservative over-pinning. The earlier arithmetic was simply performed on
the wrong page population.

### Consequence

The collector is not failing because it is missing hundreds of reclaimable
pages. It is failing because the live-set distribution leaves almost no
zero-live pages to reclaim before copying starts.

---

## 4. Bug B — in-heap forwarding constrains mid-BFS reuse

### The architectural constraint

Cheney-style evacuation writes the forwarding pointer at the source cell,
overwriting the first word of the source object with
`Word::forward(new_addr)`. The `maybe_copy` function reads this cell on
every visit to detect "already forwarded" and short-circuit. The
correctness contract is:

> A source page must remain readable for the entirety of the cycle in
> which its objects are being evacuated, because any remaining reference
> to a source-object address is going to read the forwarding marker stored
> at that address.

This means a source page cannot be safely freed and reused while the BFS
is still running, even if every object on the page has already been copied
out. Untraversed roots or payload cells elsewhere in the frontier may
still point into that page and need to consult the forwarding marker.

### Why this matters for Life now

Life's mark pass identifies 957 of 960 pre-BFS `G0` pages as live. Under
the in-heap-forwarding constraint, none of those 957 can be reused as
destination space until BFS completes.

Only 3 pages have zero live cells. Those can be freed before BFS starts,
and they are. That still leaves the collector structurally short on
destination pages.

This is not a future concern anymore. `life.lisp` is already a workload
that requires a collector capable of handling high page-level survival.

### Two structural designs

**Design 1 — Side forwarding table.**

Replace the in-heap forwarding marker with a separate data structure keyed
by source cell index, holding the new tagged `Word`.

```rust
// On PageEvacuator:
forwards: HashMap<usize /* source cell idx */, u64 /* new Word */>
// Or, for cache-friendliness:
forwards: SparseVec<u64>
```

`maybe_copy` becomes:

- look up source cell index in `forwards`
- if present, return the new `Word`
- otherwise allocate, copy, insert, return

Pros:

- minimal structural change
- BFS shape stays Cheney
- mid-BFS recycle becomes straightforward

Cons:

- substantial per-cycle memory overhead
- allocator pressure during collection
- hash lookup overhead on a hot path
- extra GC-time state in a collector already under memory pressure

For the current Life run, ~875K survivors implies a very large per-cycle
side table. That is the wrong shape.

**Design 2 — Two-phase evacuation (mark-evacuate-rewrite).**

Restructure evacuation into three discrete phases:

```text
Phase 1 — Copy:
  For each marked object in from_gen (iterate via mark bitmap):
    Allocate in dest.
    Copy bytes from source to dest.
    Record forwarding mapping in the source object.

Phase 2 — Rewrite:
  Walk all roots and all live dest-page payload cells.
  For each Word that points at a from-page address, look up its
  forwarding marker and update in place.

Phase 3 — Reclaim:
  For each from-page:
    If pin_byte != 0: flip generation in place.
    Else: release back to Free.
```

Pros:

- no new per-cycle side structure
- in-heap forwarding remains usable
- each phase has a cleaner invariant
- aligns with the basic shape used in SBCL gencgc

Cons:

- bigger code reorganisation
- root walking and card scanning happen again during rewrite
- current BFS-oriented evac code needs real restructuring

### Recommendation between Design 1 and Design 2

Given the histogram result, **Design 2 is now the immediate collector
direction, not just the long-term one**.

The bounded way to make the GC handle the workload it actually sees today
is to land two-phase evacuation. The retention investigation in §5 should
proceed in parallel, but it should not be the only path to unblocking
Life.

Skip Design 1. It is a step sideways, not forward.

---

## 5. The unsaid problem: 957 live pages for a Life game

The mark pass reports 957 of 960 pre-BFS `G0` pages with at least one
reachable cell. That's 20 MB live during a Life simulation whose
user-visible state is a list of ~40 cons cells.

This is not explained by the pre-BFS sweep. It is either genuine workload
liveness or a rooting / retention issue.

Candidate root sources, in order of suspicion:

1. **Static area at ~92 MB, almost fully committed.** Every JIT-compiled
   function record, every interned cons in a closure's captured
   environment, every macroexpansion-produced literal. These all live in
   static and are scanned via dirty-card pass for outgoing pointers into
   the heap.

2. **`Vec<u64>` rooting hazard in compiler-side code.** Macroexpand builds
   candidate forms in Rust-heap `Vec<Value>` and `HashMap` structures that
   hold words pointing at heap-resident cons cells. A conservative scan of
   those buffers can retain objects far longer than Lisp-level semantics
   would suggest.

3. **Unfreed compile-time intermediates.** The compiler's `Session` holds
   caches: macro definitions, interned-symbol tables, recent-function-cell
   records, and similar long-lived structures.

This retention question now matters for two reasons:

- it may explain why Life is in a high-survival regime today
- even after Design 2 lands, it will dominate GC efficiency

### Diagnostic that would actually answer the question

A per-root-source breakdown of live-bytes attribution:

- live bytes attributable to mutator explicit roots (`push_root`)
- live bytes attributable to mutator conservative-stack scan
- live bytes attributable to static-area dirty cards
- live bytes attributable to reservation dirty cards (`G1/Tenured -> G0`)
- live bytes attributable to any compiler/runtime-owned root path not
  covered above

Compute by running mark with those root classes isolated well enough to
produce useful attribution. It does not need to be perfect on the first
pass. It needs to be strong enough to stop guessing.

This investigation should proceed **in parallel** with Design 2. If the
retention source is found and fixed quickly, great. If not, the collector
still needs to tolerate the workload it sees now.

---

## 6. Open questions

1. **What is driving the 957-page live set?** The histogram settled the
   pre-BFS question; it did not explain the retention.

2. **Does Design 2 compose cleanly with conservative pinning?** Rewrite
   must visit pinned-but-still-in-from-gen pages too, because they may
   contain words pointing at moved objects.

3. **What should steady-state Life survival actually be?** Is 20 MB live
   genuine for this runtime today, or is it mostly retention baggage?

4. **What synthetic test best captures the structural limit?** A workload
   with exactly one survivor per page and no zero-live pages would be a
   much cleaner proof case than Life alone.

---

## 7. Recommendations summary

1. **[Immediate]** Record the histogram result and retire Bug A. The
   pre-BFS sweep is correct.

2. **[Immediate]** Start Design 2 two-phase evacuation. This is now the
   bounded fix for the present stall.

3. **[Parallel]** Investigate the 24% survival rate with per-root-source
   attribution.

4. **[Validation]** Re-run Life against Design 2 and validate: completes
   300 generations, `MINOR-GCS > 1`, `PEAK-YOUNG-BYTES` remains bounded,
   and source-page reclaim happens only after rewrite.

5. **[Anti-recommendation]** Don't land Design 1 (side table). The
   per-cycle memory overhead and Cheney-shape preservation are not worth
   the implementation savings.

6. **[Anti-recommendation]** Don't raise the reserve further as a
   workaround. 25% is already large. The fundamental problem is source
   page reuse under high page-level survival, not reserve sizing.

---

*Document state: post-histogram. Bug A has been retired; the remaining
questions are structural evacuation and retention attribution.*
