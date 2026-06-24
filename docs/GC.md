# NCL Garbage Collector - Page-Heap Design

*Status: synced to the NewGC design on 2026-05-23. This replaces the older semispace/TLAB design sketch that no longer matches the implementation direction.*

> ## NCL integration status — verified against code, 2026-05-28
>
> **Read this first.** The rest of this document describes the *extracted*
> collector (`E:\NewGC` / `newgc-core`). NCL **vendors its own copy** under
> `src/ncl-runtime/src/page_heap/` and its integration layer
> (`src/ncl-runtime/src/mutator.rs`, `src/ncl-llvm/src/lib.rs`) has built
> things the extracted-core sections below say "do not exist yet." Where this
> document and the code disagree, the code wins. As of this date, verified
> against the tree:
>
> - **Precise roots are wired and primary.** The JIT emits
>   `ncl_push_root` / `ncl_pop_root` via `emit_safepoint_wrap`
>   (`ncl-llvm/src/lib.rs`) around every GC-triggering call site —
>   function calls, cons/closure allocation, global/function loads,
>   bignum-promote slow paths. Conservative pinning is now a *complement*
>   (stack-range scan for parked threads + belt-and-suspenders), not the
>   primary root path. NCL uses explicit push/pop roots, **not**
>   `gc.statepoint`.
> - **Cooperative multi-mutator stop-the-world is implemented.**
>   `mutator.rs` has the `stop_requested` flag + condvar park, per-thread
>   stack-range publication, per-thread precise-root enumeration, and
>   per-thread TLABs. Multiple Lisp threads share one heap today.
> - **Block-incremental two-phase evacuation is implemented**
>   (`page_heap/evac.rs`): chunked copy → rewrite → reclaim, reusing
>   source pages mid-cycle.
> - **`collect_major` / `collect_full` exist** (`page_heap/cycle.rs`) but
>   are **not wired into NCL's automatic trigger** — a live session runs
>   only the minor (G0) cycle, so Tenured is not reclaimed.
>
> **What NCL genuinely still lacks** (these claims below *are* current):
> - lock-free TLAB *refill* — refill takes the global heap mutex
>   (`mutator.rs:789`);
> - JIT-emitted back-edge safepoint polls — a tight non-allocating loop
>   cannot be parked, only alloc-slow-path and explicit `safepoint()`
>   sites observe a stop request;
> - the recoverable `try_collect_*` + heap-poison path and the
>   `should_collect_major` / `collect_auto` auto-major trigger policy —
>   present in `newgc-core` HEAD, **not** synced into NCL's copy
>   (MidEvacOOM is a fatal `gc-stall` condition, not a `Result`);
> - the standalone large-object module (NCL inlines `try_alloc_large`
>   into `alloc.rs`).
>
> The "extracted core" framing of the sections below (Conservative
> pinning, Allocation model, Threading status, Trigger policy, Immediate
> implications) is accurate *for `newgc-core`* but stale *for NCL's
> integration*. They are kept as the upstream reference; do not read their
> "not yet / future work" lines as NCL's status without checking the code.

## What this is

This document describes the GC shape NCL now targets: a page-based,
mark-evacuate, generational collector. The current executable reference
for this design is `E:\NewGC`, which was extracted from NCL's
`page_heap` work so the collector could be hardened in isolation.

The old version of this file described a different collector:

- young/old semispaces
- per-thread TLAB allocation
- cooperative stop-the-world across multiple concurrent mutators
- forwarding-pointer copying as the primary collection geometry

That is no longer the current plan. The live design is the page heap
documented in `E:\NewGC\README.md`, `E:\NewGC\DESIGN_REVIEW.md`, and
`E:\NewGC\THREADING.md`.

## Design lineage

The page heap is informed by four sources:

- The original Corman Lisp collector, especially the practical emphasis
  on a generational Common Lisp heap for interactive workloads.
- SBCL `gencgc.c`, which contributes the page-based, generational,
  mark-evacuate shape and the card-table discipline.
- NCL's [GC_DESIGN.md](GC_DESIGN.md), which captured the research pass
  that chose page heap over semispace as the long-term direction.
- NCL's [GC_LESSONS.md](GC_LESSONS.md), which records the failure modes
  that mattered in real workloads.

The important shift is this: NCL is no longer designing around
"copy the whole young heap into an old semispace". It is designing
around a single reserved page heap with generations represented by page
descriptors, mark state, evacuation policy, and card metadata.

## Heap geometry

The collector reserves one contiguous virtual address range and manages
it in fixed-size pages.

- Page size is currently 64 KB.
- Pages are tracked by descriptors, mark state, generation, and kind.
- Commit/decommit happens at page granularity.
- Windows uses `VirtualAlloc`; Unix uses `mmap` plus `mprotect`.

At the logical level the heap is split into generations, not semispaces:

- `G0` - nursery / youngest generation
- `G1` - intermediate promoted generation
- `Tenured` - long-lived generation
- `Free` - unallocated pages inside the reservation

Large objects are part of the design surface but are still a follow-on
area. The current layout and tests are optimized for normal cons/boxed
page traffic.

## Collection strategy

The collector is generational and evacuating, but page-based rather than
semispace-based.

### Minor collection

A minor cycle collects from `G0`.

- Roots are scanned.
- Dirty cards are scanned so old-to-young pointers are treated as roots.
- Live `G0` objects are evacuated out of `G0`.
- Survivors are promoted according to the current cohort policy.
- Dead `G0` pages can be recycled.

The critical point is that page state, mark bits, start bits, and page
descriptors drive the walk. The collector is not trying to discover heap
shape by blindly traversing a semispace from base to top.

### Major collection

A major cycle collects the older generations as well.

- Mark state is built from the root set and cross-generation edges.
- Live objects are evacuated into destination pages.
- Page live counts are rebuilt from marks.
- Zero-live unpinned pages can be released back to the free pool.

This is still stop-the-world, but it is not "copy old semi-A into old
semi-B and swap". The unit of movement and reclamation is the page.

## Object discovery and scanning

The page heap relies on explicit object-shape knowledge.

- Pointer tags classify values before the collector treats them as heap
  references.
- Start bits identify valid object starts on managed pages.
- Header decoding determines object length and scan discipline.
- Conservative candidates must pass the same pointer gates before they
  are treated as roots.

This is one of the major corrections relative to the retired design.
The collector is no longer defined in terms of "young is a contiguous
copying area, so walk linearly until top". Instead it validates each
candidate slot against page ownership, generation, page kind, start-bit
state, and header/tag consistency.

## Write barrier and cards

Card marking is part of the real design, not a deferred optimization.

- Heap-pointer writes into managed objects mark cards.
- Minor and major cycles both consult dirty cards.
- Destination copies also mark destination cards.
- G0 cards persist across cycles when needed so later major scans can
  see cross-generation chains created by prior movement.

This already matters for correctness. The page-heap work found real bugs
where mixed G0/G1 chains survived only once card handling covered the
actual cross-generation shapes created by evacuation.

## Conservative pinning

Conservative pinning remains part of the NCL story because NCL does not
yet have precise roots everywhere.

- Potential pointers discovered conservatively can pin objects instead of
  forcing unsound movement.
- Pinned-object handling extends mark reachability when pinned objects
  point across generations.
- The cross-generation pinned-field case is load-bearing: a pinned older
  object can keep a younger target live even when the older object was
  not discovered through the precise root walk.

In `E:\NewGC`, conservative pinning is feature-gated so a precise-roots
client can compile it out. *(NCL update: NCL's JIT now emits precise
push/pop roots — see the integration-status block at the top. Conservative
pinning is retained as a complement for parked threads and as
belt-and-suspenders, no longer the primary root path.)*

## Allocation model

The current page-heap design does not promise concurrent mutators.

- Allocation happens through page-local allocation regions.
- Start bits are updated as objects are allocated.
- Page commit/decommit is synchronized.
- The public API is intentionally shaped around exclusive mutable access
  to the heap during mutation.

This is a direct change from the retired TLAB design. `E:\NewGC` is
`Send + Sync`, so a heap can be moved across threads or wrapped in a
`Mutex`, but there is no supported model today where several mutator
threads allocate into the same heap concurrently without external
serialization.

## Threading status

The older version of this document assumed true multi-mutator support as
part of the base GC architecture. That is not true of the current page
heap.

What works today in the extracted design:

- independent heaps on different threads
- one shared heap behind a `Mutex`
- concurrent read-only stats access

What does not exist yet:

- per-thread TLABs on a shared heap
- safepoint / poll-word protocol
- cooperative mutator parking
- per-thread root enumeration for a stop-the-world cycle

That work is still planned *for the extracted core*, but it is separate
from the current page-heap collector. *(NCL update: NCL's integration
layer has already built cooperative multi-mutator parking, per-thread
root enumeration, and per-thread TLABs on top of the core — see the
status block at the top. The remaining gaps for NCL are lock-free TLAB
refill and JIT back-edge safepoint polls, not multi-mutator support as a
whole.)*

## Trigger policy and OOM handling

The extracted collector now has an explicit trigger policy.

- `should_collect()` decides when the heap should collect.
- `collect_auto()` chooses minor vs major based on tenured pressure.
- The byte budget is recomputed from live tenured usage after a cycle.

The crate also added `try_collect_*` variants so hosts can receive a
`Result` instead of taking an unconditional process-killing panic on
mid-evacuation OOM.

For NCL, that means the design is no longer "we will eventually avoid
OOM by trigger tuning". The collector now has a documented recovery
surface, even if the heap must be considered poisoned and dropped after
an evacuation failure.

## Introspection

The page heap exposes a unified stats snapshot instead of a handful of
semispace-shaped accessors.

`GcStats` includes:

- reservation and commit totals
- per-generation used/free pages and bytes
- trigger-policy counters
- last-cycle telemetry
- minor/major cohort counters

This is the right vocabulary for NCL now. Old names like
`young_used_bytes` and `old_capacity_bytes_per_semi` are legacy shims,
not the design surface the runtime should grow around.

## What NCL should assume from this document

If code or docs in NCL still assume any of the following, they are
behind the current design:

- young/old semispace geometry
- old-space A/B swaps
- a collector that can be explained without page descriptors, mark bits,
  start bits, pinned state, and card tables

*(Note: earlier revisions of this list also named "TLABs" and
"cooperative multi-mutator stop-the-world" as not-yet-implemented. Those
are now built in NCL's integration layer — see the status block at the
top. They are removed from this list to stop it misleading NCL readers.)*

The authoritative mental model is:

1. Reserve one virtual heap.
2. Manage it in pages.
3. Track generation and kind per page.
4. Mark and evacuate by page-driven metadata.
5. Use cards for old-to-young and mixed-generation reachability.
6. Use conservative pinning where precise roots are not available.

## Immediate implications for NCL integration

NCL still has integration work ahead, but less than this section once
implied — see the status block at the top for what's already done.

- **Done:** precise root enumeration (push/pop roots in the JIT) and
  cooperative multi-mutator parking. These were the two big items here.
- **Still open:** lock-free TLAB refill (refill takes the heap mutex);
  JIT-emitted back-edge safepoint polls (so non-allocating loops can be
  parked); auto-major / recoverable-OOM sync from `newgc-core`; an
  explicit FFI object-pin API for after conservative scan is dialed back.
- Runtime APIs should keep moving toward page-heap-native names and away
  from semispace compatibility shims.
- Any documentation that describes the GC as two-generation semispace
  copying is obsolete.

## References

- [GC_DESIGN.md](GC_DESIGN.md) - architectural research and migration plan
- [GC_LESSONS.md](GC_LESSONS.md) - real-workload failures and lessons
- `E:\NewGC\README.md` - current extracted collector status
- `E:\NewGC\DESIGN_REVIEW.md` - mismatches, resolved issues, and next steps
- `E:\NewGC\THREADING.md` - exact threading guarantees and missing pieces
