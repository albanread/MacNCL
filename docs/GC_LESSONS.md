# Garbage-Collector Implementation Lessons

*A field report from the NCL page-heap GC build, sub-phases 1-10.*

This document exists because we built a garbage collector with several
working open-source examples sitting next to us — SBCL's gencgc, CCL's
mark-compact, the cheneygc fallback — in a memory-safe language, with
312 unit tests passing, and **it still didn't work on the first real
Lisp workload we threw at it.** Each subsequent bug was, in retrospect,
obvious. None of them were obvious in advance. The bugs share a small
number of root patterns, and naming those patterns is more useful than
naming the bugs.
 
SBCL has fixed real gencgc correctness issues into the 2020s. Writing a GC is one of
the few engineering tasks where decades of public source code, multiple
textbooks, and a deceptively simple algorithm description still produce
a bug-per-week debugging schedule for the first month. Memory-safe Rust
doesn't help — the bug class is in *invariants between unsafe regions
and bookkeeping*, not in pointer arithmetic.

Below: the patterns, the specific bugs we hit, what the unit tests
missed, and what we'd do differently.

---

## Pattern 1: The abstraction trap

We started by adding a `HeapBackend` trait so the new page-based heap
could coexist with the existing two-semispace collector during the
migration soak period (sub-phase 1 of `GC_DESIGN.md`). The trait grew a
tail of `#[deprecated]` methods named after the old heap's shape
(`young_*`, `old_*`) plus a parallel `dynamic_*` family abstracting over
the difference. By sub-phase 5 the trait was 250 lines, half of which
existed only because the two backends disagreed on geometry.

**What went wrong**: the trait was justified by "we'll soak both
backends in production for 1-2 weeks for safe rollback." That benefit
is real once. The cost — a runtime `dyn` indirection in every heap
operation forever, plus naming-shape contortions visible to every
caller — is paid every cycle.

**What the codebase looked like trying to fight it back**:
`heap_backend.rs` had eight methods marked `#[deprecated]` to push
callers toward the `dynamic_*` shape. Nobody migrated. Every PR
re-introduced young-and-old reasoning at call sites because that's how
the only working backend's behaviour was naturally described.

**The fix that landed**: we deleted the trait. Cargo features
`gc-semispace` (default) and `gc-page-heap` flip the choice at build
time. `gc::Heap` re-exports the chosen concrete type. Every dispatch
becomes a direct call. The trade-off — "to switch GCs you rebuild" —
turns out to be fine for a desktop Lisp. The 250-line trait, the
deprecation tail, and the `dynamic_*` ghost-API: all gone.

**Lesson**: a runtime-dispatch abstraction in a hot-path subsystem is a
permanent tax for a transient benefit. Build-time selection through
features gives you the same "both implementations compile in CI"
guarantee. The trade-off cost shows up exactly once during a deploy,
not once per allocation forever.

---

## Pattern 2: Rust unit tests don't test GC correctness, they test GC *mechanics*

This is the sharpest single point in this document and the one that
took the longest to internalise: **all 312 of our Rust tests can pass
and the GC can still be wrong.** The tests assert that operations
complete and that data structures end up in expected shapes. They
don't assert that the GC's *semantics* hold.

What a GC's correctness actually means:

- Every object reachable from roots before a cycle is reachable after.
- Every Word that pointed at a live object before a cycle points at
  that same logical object after (perhaps at a new address, perhaps
  the same).
- No object reachable before a cycle gets freed.
- Eventually, every unreachable object is freed.
- Steady-state allocation against a bounded working set produces a
  bounded heap.
- After a cycle, every cell on every live page either contains a
  valid Word or is unreachable from any root.

What our Rust tests actually assert:

- `try_alloc_cons_in` returns a non-null pointer.
- After `mark_from_roots(target, roots)`, `count_marked_in_gen(target)`
  equals the expected count.
- After `evacuate_with_roots(from, dest, roots)`, the from-gen has zero
  pages.
- After `collect_minor`, `bytes_promoted_total` increased.

The mechanics passing doesn't prove the semantics work. **Mechanics
are pre-conditions for correctness, not evidence of it.** A GC that
correctly bumps every counter and correctly transitions every page
descriptor can still freely lose pointers, double-free objects, leave
dangling references, or grow the heap monotonically under steady-state
load. None of those would fail any of our 312 tests.

Some of our tests gesture in the right direction. `evac::tests::
chain_head_evacuates_every_link` builds a 50-cons chain, evacuates it,
and walks the chain after to verify every link's value survived. That
*is* a correctness test — it's checking "this specific object graph
survived intact." But it does this with hand-constructed roots, direct
allocator calls, one cycle, no pinning, no cards, no mutator. The
production GC has a dozen more axes of variation, each of which can
have its own correctness bug, none of which this test reaches.

**What an actual GC correctness test looks like**:

- Build an object graph with known reachability (e.g., a list of 1000
  cons cells, of which we hold references to 100, the rest unreachable).
- Run a GC cycle through the **mutator allocation path**, not a direct
  allocator call.
- Walk the 100 surviving cells: every car/cdr matches the pre-cycle
  value. Every cdr that was a Cons points at another live cell, not
  garbage.
- Verify the 900 unreachable cells' old addresses no longer hold their
  prior content (the pages they were on have been zeroed, recycled,
  or contain forwarding markers).
- Repeat with cycles in the object graph, with deep nesting, with
  mixed cons/boxed allocations, with pinned objects on the same page
  as garbage.
- Do all of the above **across multiple GC cycles in the same heap**.

We have approximately none of this. We have lots of "mark count is N"
and "page count in gen X is M" tests, which together produce the
illusion of coverage and the actual absence of it.

**The lesson generalises**: in a system where correctness is defined
over end-to-end state preservation, unit tests of the components don't
add up to a test of the system. You can have 100% code coverage of
the components and 0% behavioural coverage of the system's contract.

For a GC, the only thing that even *attempts* to test the contract is
a realistic workload that allocates, drops references, allocates more,
and continues running. If the heap stays bounded, references stay
valid, and the workload completes, the GC is *probably* correct on the
workload's exercised paths. Anything less than that proves you have a
data structure that page-flips well, not a garbage collector.

We built `page_heap` with ~95% test coverage of its module-internal
APIs. 312 tests passing under both feature flags. Every page-state
transition unit-tested. Every BFS edge case covered.

**It didn't work on `(+ 1 2)`.**

The unit tests used `PageHeap::with_reservation(N)` directly, called
`try_alloc_cons_in` and `try_alloc_boxed_in` directly, fed
hand-constructed `Word` arrays as roots, and asserted on the heap state
after one or two cycles. They never:

- Went through the mutator's TLAB allocator.
- Allocated mixed types in a single slab (the mutator does; the unit
  tests' direct allocators don't).
- Hit the conservative-stack-scan pin pass with real JIT'd code frames
  on the stack.
- Held heap pointers in arbitrary Rust containers across an alloc point.
- Allocated enough to force the auto-trigger path.
- Ran more than one GC cycle on the same heap.

The actual production path — `mutator.refill_tlab` →
`coordinator_api::young_try_alloc_slab` → `try_alloc_g0_cons_slab` →
`acquire_free_page` — wasn't covered by the page_heap unit tests at
all, because that path crosses a layer boundary the page_heap module
doesn't see.

**The first bug discovered this way**: the mutator's TLAB landed on a
`PageKind::Cons` page, but the mutator allocates *both* conses and
header-bearing objects (vectors, strings, symbols) into a single TLAB.
The walker dispatched object stride by page kind, so it read a boxed
object's payload as a 2-cell cons and crashed reading garbage as a
HeapHeader. Unit tests never put a vector on a cons-kind page; the
mutator-TLAB-composition layer did, immediately, the first time it ran.

**The second bug discovered this way**: `young_try_alloc_slab` returned
`Option<NonNull<u64>>` with no channel to communicate the granted size.
Semispace always granted exactly what was asked. Page-heap silently
capped to one page (`min(requested, PAGE_SIZE_CELLS)`). The mutator
trusted its `requested_cells` and set `tlab.limit` from it. Page-heap
gave 8192 cells, mutator thought it had 65536, bumped past the page
boundary. Three downstream corruptions per overrun: the data write, the
start-bit-bitmap write (computed against the wrong page), and the card-
table mark (same).

The contract was unobservably wrong. The fix was a one-line API change:
return `Option<(NonNull<u64>, usize)>` and assert in the caller that
`granted` is in `[min_cells, requested]`.

**Lesson**: GC bugs cluster at layer boundaries. Test the *integration*,
not the layers. The smallest meaningful test for a GC is the smallest
real workload that allocates through the production allocator and
triggers a real cycle. Anything smaller proves you can write data
structures, not that you can collect them.

---

## Pattern 3: Contracts that don't carry state are silent corruption

The `young_try_alloc_slab` story above is one instance. The general
form: a function returns `Option<T>` where `Some(T)` is supposed to
mean "request fulfilled," but two backends disagree on what
"fulfilled" means. The caller has no way to know.

We hit this same pattern in three places:

1. **`young_try_alloc_slab(cells) -> Option<NonNull<u64>>`**: caller
   doesn't know the granted size. Discussed above.
2. **`acquire_free_page(generation, kind)`**: caller doesn't know how
   many Free pages were left after the call. Mutator drains the Free
   pool to zero, then triggers GC. GC needs Free pages. Panic.
3. **Recycled-page contents** (sub-phase 11d): pages reclaimed by the
   collector kept their old cell contents. Mutator allocations into
   recycled pages assumed zero-initialised cells (this is true for
   freshly committed pages, false for recycled ones). Mutator helpers
   like `alloc_vector` initialise only the header, leaving payload
   cells as recycled-page garbage. The GC then walks vectors whose
   payload Words look like heap pointers and follows them.

**Lesson**: if a function returns information about "what state you're
in now," put that information in the return value. Don't make the
caller re-derive it from a global query that races with other callers.
For GC specifically: every allocator-runtime contract should be
explicit about what was granted, what's left, and what assumptions the
caller can make about the granted memory's contents.

---

## Pattern 4: Reserve-as-fix isn't a fix

When `try_alloc_boxed_in(dest_gen, size)` returned `None` during
evacuation, we panicked with `"page heap exhausted mid-evacuation"`.
The mutator had consumed the last Free page; the GC then needed one
and had none.

The first instinct was: reserve N pages for GC use. The mutator's
allocator refuses if `count_pages_in_gen(Free) <= reserve`; the GC's
allocator has no such check. Tried N=8 — Life crashed. Tried N=320
(25% of heap) — Life crashed, copied 21 MB before stalling.

The reserve was a workaround. The actual problem: the GC was hoarding
fresh pages as destinations while 922 from-pages of pure garbage sat
unreclaimed inside the same cycle. The structural fix is to recycle
from-pages during the BFS: when a page's last live cell is copied
out (or its initial live count is zero), release it back to Free
inside the cycle, so the next dest allocation can pick it up.

**But recycling requires per-page liveness info** — which the BFS
discovers incrementally and unidirectionally. Mid-BFS, "this page has
copied cells" doesn't mean "this page is fully drained"; it might have
more to copy later. To distinguish "drained" from "in progress," we
need to know the page's total live count in advance.

That means a **pre-evac mark pass**. The mark bitmap already exists
from sub-phase 5 but had been sitting unused since sub-phase 7's
Cheney BFS discovered liveness inline. The production minor cycle has
to run mark *first* with the same root set the evac will use, then
seed per-page `live_remaining` counters from the mark bitmap, then
evacuate with recycle-on-zero firing throughout the BFS.

**The recycler then exposed its own bug** (sub-phase 11d-ish): a page
recycled mid-BFS could be re-acquired by the dest allocator within the
same cycle. The post-cycle from-pages sweep didn't know that and tried
to release the page a second time, wiping its now-live dest contents.
Fix: track recycled pages in a per-cycle side set; the sweep skips
anything in it.

**And then the recycler exposed a second bug**: it only fired on copy-
out, so pages with zero live cells (pure-garbage pages, of which Life
had ~922 per cycle) never triggered a release. Fix: a pre-BFS sweep
right after the mark pass, releasing any from-page with zero marks AND
zero pins, *before* the BFS asks for a single dest.

**Lesson**: when a fix has the shape "reserve more headroom," it's
probably a workaround. The structural question is "why is the
collector hoarding resources it doesn't need?" Trace the resource flow
inside one cycle. The fix is almost always reclaiming sooner.

Also: each layer of fix exposes the next layer's bug. The reserve
workaround hid the recycling requirement; the recycling fix exposed
the post-sweep double-release; the recycle-on-copy-out fix exposed the
pure-garbage-no-fire bug. **Three independent bugs in the same
control-flow region**, each invisible behind the previous one. Plan
for this.

---

## Pattern 5: Conservative scan over-pins, but not as much as you fear

We feared the conservative-stack pin pass was wildly over-pinning
under deep recursion (the macroexpand-all hazard from `GC_DESIGN.md`
sub-phase 2.4). When Life's first GC reported `pin-set=134` across `36
pages`, we expected far higher numbers given how many `member-cell`
recursion frames were on the stack at trigger time.

**It wasn't pinning excess.** The pins were ~134 distinct objects
across 36 pages, ~3.7 per page. Reasonable.

**But Life still showed 21 MB of survivors** — way more than the
in-Lisp game state could explain. That's a real liveness problem,
not a pinning problem. The conservative scan correctly identified live
objects; the question is *why* 21 MB of stuff was reachable from
genuine roots.

Candidates:

- The static area was 92 MB used. Every JIT-compiled function record,
  every interned constant, every closure literal lives in static.
  Static is scanned by dirty-card pass; entries reach into the heap.
  A loaded core.lisp + Life's compiled forms = lots of static-rooted
  heap references.
- Rust-heap containers (e.g., `Vec<u64>`) holding raw Word pointers
  used by the compiler / macroexpander. The conservative-stack scan
  sees a pointer to the Vec's buffer, but the buffer's *contents* —
  the Words it holds — are on the Rust heap, not the stack. The GC
  doesn't traverse Rust-allocated containers. If any of those Words
  point at heap objects, those objects are reachable from a root the
  GC can't see *and* not reachable from a root it can see, so they
  become dangling pointers after a cycle.

**Lesson**: when the symptom is "GC keeps too much alive," the first
diagnostic is the mark bitmap snapshot at start-of-evac. It tells you
the genuine live-byte count. Compare to expected (your application's
working set). If they disagree by orders of magnitude, the problem is
upstream of the GC — over-pinning, rooting hazards, container leakage.
Don't tune the GC; fix the roots.

Also: pinning is bounded by the conservative scan's per-cycle output.
Rooting through Rust-heap containers is unbounded — every Word in
every Vec is a permanent root for as long as the container is alive.
The latter is far more dangerous.

---

## Pattern 6: The error channel matters more than the error

The early panics in `evac.rs` used `.expect("page heap exhausted mid-
evacuation")`. Three things were wrong with that:

1. **It abort()s the process.** Lisp's condition system can't catch
   it. There's no way for the REPL or a `handler-case` to recover.
2. **It tells you nothing.** You know mid-evac OOM happened, not what
   the heap looked like, not what was already copied, not why.
3. **It loses signal-safety guarantees.** The runtime had carefully-
   wired SEH unwind paths for Win32 callbacks. A Rust panic in the
   middle of an allocator call bypasses all of that.

The structured-error rewrite landed a `GcStallError` enum with: stall
reason, page counts by generation, pin-set size, pinned-pages count,
reserve config, objects/cells copied before failure, last GC trigger
reason, static-area usage. The runtime's native-boundary catch
converts the error into a Lisp `gc-stall` condition raised through
the existing condition path.

The first real-workload run with the new error channel produced:

```
gc-stall: reason=MidEvacOOM
  pages(free/g0/g1/tenured) = 0/1280/0/0
  pinned-pages              = 36
  pin-set                   = 134
  reserve-pages             = 320
  copied(objects/cells)     = 874699/2637453
```

From those eight numbers, in one round of analysis, we got: the
reserve isn't the issue (320 is large enough), the pin pass isn't
running wild (134 across 36 pages), 922 pages of pure garbage are
sitting unreclaimed (1280 - 322 dest - 36 pinned), the survivor
volume is genuinely large (21 MB), and the static area is suspiciously
big (92 MB).

Before the structured error, this same panic took five rounds of
adding `eprintln!` instrumentation to triangulate.

**Lesson**: a GC's failure mode is structurally distinct from its
success mode. Both deserve first-class engineering. When the
collector fails, the runtime should know what kind of failure it was,
what state was reached, and whether the caller can recover. Bake that
in early — before you need it. The cost is one enum and one Result
propagation; the benefit is being able to debug under realistic load
instead of constructing a synthetic repro.

---

## Pattern 7: Test workloads vs benchmark workloads vs deliberate GC stressors

Three categories of "test" we wrote, each useful for different things,
none sufficient by itself:

| Category | Example | What it proves | What it misses |
|---|---|---|---|
| Unit tests | `page_heap::evac::tests::cycle_in_object_graph_terminates` | The BFS terminates on cycles | Everything about the production path; nothing about correctness |
| Integration tests | `ncl-tests::numbers::truncate_int_int_quotient` | Lisp eval through the full pipeline produces the right result | Whether the GC fires at all (most don't allocate enough to trigger one) |
| Deliberate GC stressors | `demos/life.lisp` running 300 generations | The GC handles sustained real allocation pressure with realistic survivor patterns | What happens after hours of uptime |

We had the first two from day one. The third arrived only when we
explicitly wrote a garbage-generating Lisp program — `life.lisp` — to
provoke real GC behaviour. Up to that point we had 488 passing tests
and a GC that couldn't survive its own first real cycle.

**`demos/life.lisp` is not a Life implementation.** It's a deliberate
GC test in the shape of one. The choices that look gratuitously
inefficient are the point:

- It represents board state as a **list** of `(x . y)` cons cells, not
  a 2D array. The original Corman Lisp `examples/life.lisp` uses a
  `make-array` of nil, mutated in place — zero per-tick allocation.
  Ported faithfully, it would not exercise the GC at all.
- `member-cell` is a linear search through the live list, called from
  inside a triple-nested loop. Each call constructs candidate `(cons
  nx ny)` for comparison. The asymptotic complexity is deliberately
  bad so the per-tick allocation count scales superlinearly with live
  population.
- `candidate-positions` allocates ~9 cons cells per live cell, even
  knowing most will be duplicates that the dedup step throws away.
  Maximises pressure rather than minimising work.

The R-pentomino seed was chosen because it produces a 1100-step
chaotic evolution before settling — the longest natural runtime among
small Life seed patterns. Every tick reallocates the entire live-cell
list. Cumulative allocation across 300 generations is in the tens of
megabytes — enough to force multiple real GC cycles on the default
heap.

A faithful list-vs-array port that ran efficiently would have produced
~10 KB of allocation total and triggered zero GCs. That'd be useless
as a GC test. The inefficient port is the test.

This is a general technique: when you need a workload that exercises
a specific runtime path, **deliberately structure the user-space code
to lean on that path heavily.** A "test program" that incidentally
allocates is a benchmark of the program; a "test program" structured
to maximise allocation is a benchmark of the allocator and collector.
Both have a place; for GC bring-up, you need the second.

`TestSession`'s forced-GC-at-drop was particularly misleading: it
made every test report `MINOR-GCS=1` and looked like coverage. It
wasn't. One forced cycle on a near-empty session-drop heap exercises
a tiny fraction of the cycle code path and effectively none of the
"under pressure" path. **It also doesn't test correctness** — the
test asserts a Lisp-level value (e.g., `truncate-int-int-quotient`
returns 3); whether the GC corrupted other values that the test
didn't inspect is invisible.

**Lesson**: write a deliberate GC stressor on day one. Not a benchmark,
not a demo — a program whose user-visible behaviour is dominated by
its allocation pattern. The minimum is "this program allocates N MB
of garbage per second for M seconds and the process stays alive with
bounded memory **and produces the same result it would have without
GC**." Run it under every flag combination CI covers. If it crashes,
you have a GC bug whose existence you can prove without further
investigation. If it stays alive but slows down logarithmically, you
have a leak. If it stays alive and steady AND the result is correct,
you have evidence — not proof — that the GC works.

The "result is correct" piece is what we initially missed. Life crashes
or doesn't crash; that's a coarse signal. A version of Life that ran
to completion but produced a generation count of 287 instead of 300
(because the GC silently dropped a closure environment somewhere) would
look identical to the "stays alive" success case. The stressor needs a
correctness oracle baked in: a checksum of the live state, a fixed
expected final generation count, something the runtime is forced to
preserve correctly.

---

## Pattern 8: Memory-safe Rust is the wrong line of defence

Building a GC in Rust gave us, free of charge:

- No use-after-free in Rust-owned data structures.
- Borrow-checker enforcement that we couldn't alias the heap across
  threads incorrectly.
- `unsafe { ... }` blocks marked as audit boundaries.

Building a GC in Rust did **not** give us:

- Any guarantee that the bookkeeping data structures (start-bit
  bitmap, mark bitmap, card table, page descriptor table) agreed with
  each other.
- Any guarantee that the Words stored in heap cells pointed at the
  right kind of object.
- Any guarantee that the mutator's TLAB believed the same things about
  its slab as the page-heap allocator did.
- Any guarantee that the BFS's "I've seen this cell" check covered
  every reachability path.

Every single bug we hit was in *agreement between separately-correct
data structures*. The bookkeeping invariants live above the language;
the language can't see them. The `#[deny(unsafe_op_in_unsafe_fn)]`
flag and a tidy `unsafe` block discipline keep you from corrupting
arbitrary memory, which is a real win — but they don't keep you from
encoding the wrong invariants and then encoding their inverses next
door.

**Lesson**: if you're tempted to think "memory-safe language → no GC
bugs," check the bug list above. Memory safety prevents one class of
catastrophic failure (corrupt pointers cause AVs). The class that's
left — disagreement between bookkeeping structures — is where 100% of
our GC bugs lived. Plan accordingly: bookkeeping invariants need their
own assertion discipline, their own diagnostic dumps, their own
regression tests.

---

## Meta-pattern: each fix exposes the next bug

Here's the literal order in which the page-heap's "now it works"
moment kept receding:

1. Trait dispatch is heavy → switch to features. *Done.*
2. Page-heap builds but unit tests crash with TLAB-on-Cons-kind →
   switch slabs to Boxed-kind pages, walker dispatches by start-bit
   pattern. *Done.*
3. Unit tests pass but `(+ 1 2)` segfaults → diagnose silent TLAB
   contract violation. Return granted size, assert in caller. *Done.*
4. `(+ 1 2)` works, hellowin works, but Life crashes on first cycle →
   reserve N pages for GC. *Workaround.*
5. Reserve was capped at 8, too small → make proportional (25% of
   heap). Life crashes anyway. *Workaround.*
6. Even 25% reserve isn't enough → identified the structural fix:
   recycle from-pages during BFS. Needs liveness info. *Real fix.*
7. Recycling needs a pre-evac mark pass → wire mark in production
   minor cycle with the evac's root set. *Done.*
8. Recycler bookkeeping bug — pages re-acquired mid-cycle wrongly
   released by post-sweep → track recycled set, sweep skips it. *Done.*
9. Recycler only fires on copy-out, missing 922 pure-garbage pages per
   cycle → add pre-BFS sweep for zero-live, zero-pin pages. *Pending.*
10. Survivor volume is genuinely 21 MB for a Life game with ~50 cells —
    static area scanning, rooting hazards. *Open.*

**This is normal.** Every fix exposes the next layer. Every layer's
bug was invisible behind the previous one. There is no "land the
whole stack at once" — you can only ladder up, and each rung makes
you wait for the next workload to break.

What you can do:
- **Structured errors so each step's failure is actionable in one
  round, not five.**
- **Workload tests so each step's failure surfaces immediately, not
  three days later when a user hits it.**
- **A diagnostic vocabulary (page counts, pin counts, copy-progress,
  live-byte snapshot from mark) so each failure produces enough
  evidence to choose the next fix without speculation.**

---

## What we'd do differently from a clean start

Specific architectural decisions whose costs we learned the hard way:

1. **No trait. No `dyn HeapBackend`.** Build-time selection through
   features from the first commit. The trait was a 2-week migration
   tool that became a permanent indirection.

2. **Allocator contract returns granted state.** Every allocator
   primitive returns the actual size granted plus assertions that the
   caller can verify the request was met. Never `Option<Ptr>` when the
   caller has hidden state about what was given.

3. **Structured `GcStallError` from cycle 0.** Even a placeholder
   payload (just stall reason + page counts) is better than `.expect()`.
   You'll need it when you need it, and "when" is unpredictable.

4. **A workload test from day 1.** Doesn't matter if it's silly. A
   Lisp program that allocates a million conses, runs N tight loops,
   and reports `(gc-stats)` is sufficient. Wire it into CI.

5. **The mark bitmap exists from sub-phase 5 onward and is used by
   sub-phase 7.** Don't defer the mark pass on the assumption that
   Cheney BFS handles liveness "for free." Cheney does — until you
   want to recycle anything mid-cycle, at which point you needed the
   mark from the start.

6. **Recycle from-pages during BFS, including the pre-BFS sweep for
   pure-garbage pages.** From day one. The reserve approach has no
   future and you'll just go through it twice (cap, then proportional)
   before discarding it anyway.

7. **Bookkeeping invariants get their own assertion battery.** A debug-
   only "check the page-state histogram matches the per-page descriptor
   summary" pass run at every GC entry. Don't wait for a crash to
   notice that your `count_pages_in_gen(Free)` disagrees with what
   `acquire_free_page` finds.

8. **Decouple the GC's correctness from the application's rooting
   discipline.** Or at least be explicit about which rooting hazards
   you accept (Vec<u64> of Words in the compiler) and add diagnostics
   that surface them. We deferred this issue for weeks and it
   resurfaced as 21 MB of unexplained live data with no way to attribute
   it.

---

## Conclusion

The GC works at sub-phase 11d. Eventually it will work past sub-phase
11d. The page-heap will land. The cost of getting there has been
mostly debugging-time, not engineering-time — the *code* per layer is
small. The *time between symptom and diagnosis* is large, and that's
the cost the patterns above are trying to compress.

There's nothing in this document that's specific to NCL or to Rust.
Future readers building any moving-collector GC will hit some subset
of these. The list isn't exhaustive — concurrent GC will add its own
class of bugs we haven't sampled yet, and compaction-in-place
collectors have correctness gotchas no copying collector touches.

But for the moving-collector case, the patterns are nearly universal,
and writing them down here is cheaper than re-discovering them.

---

*Maintained alongside `docs/GC_DESIGN.md`. Update when a new pattern
joins the list.*
