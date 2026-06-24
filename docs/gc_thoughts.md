# GC Thoughts

*Working notes on the current page-heap GC failure mode, updated after the
pre-BFS sweep histogram settled the reclaim question.*

This note is narrower than [GC_DESIGN_REVIEW_2.md](GC_DESIGN_REVIEW_2.md).
That document lays out the collector state and the design branch. This one
records the practical advice that follows from the new data.

---

## 1. Treat this as two active workstreams, not one argument

The histogram resolved the first dispute. The active work now splits into
two tracks:

1. **Structural GC limit:** in-heap forwarding prevents true mid-evac
   source-page reuse.
2. **Retention:** the nursery survival rate for `life.lisp` is far too
   high.

What is no longer active:

- **Bug A** is gone. The pre-BFS sweep is correct. The bad arithmetic came
  from mixing panic-time `G0` with pre-BFS `G0`.

The order now matters like this:

- land the structural fix that lets high-survival workloads complete
- investigate why the live set is so large
- do both in parallel when possible

If those get blurred together, the likely outcome is open-ended leak
hunting while the collector remains unable to handle the workload it is
already seeing.

---

## 2. The counting discrepancy is resolved

The pre-BFS histogram answered the disputed question directly:

```text
[pre-bfs sweep] target=G0 total=960 zero=3 zero_unpinned=3 zero_pinned=0
                nonzero_unpinned=937 nonzero_pinned=20
[pre-bfs sweep] releasable.len()=3
```

That result means:

- pre-BFS `G0` contains 960 pages, not 1280
- the 320-page reserve is still `Free` at sweep time
- 957 of 960 `G0` pages have at least one marked cell
- only 3 `G0` pages are zero-live
- all 3 are unpinned
- all 3 are released

So the sweep did the right thing. The earlier expectation of hundreds of
reclaimed pages was based on the wrong denominator.

This matters because it removes the last factual basis for postponing the
structural collector work.

---

## 3. Pinning is not the immediate culprit

The histogram does not support the theory that conservative pinning is the
main reason pre-BFS reclaim collapses.

At sweep time:

- `nonzero_pinned = 20`
- `zero_pinned = 0`
- `zero_unpinned = 3`

That is a reasonable, tight pin profile. It does not explain the stall.

The more important point is narrower: pinning is not what turned a
potentially healthy pre-BFS reclaim pass into a 3-page reclaim pass.
There were only 3 zero-live pages available to begin with.

---

## 4. "Pure garbage pages" was the wrong mental model here

A page with zero marks is reclaimable only if the runtime metadata agrees.
That part was always true. The specific mistake was using panic-time `G0`
as though it were pre-BFS `G0`.

The right arithmetic is:

- pre-BFS `G0 = 960`
- marked-live pages in pre-BFS `G0 = 957`
- zero-live pages in pre-BFS `G0 = 3`

That is why the reclaim pass is small. Not because the predicate failed,
but because almost every source page is genuinely in play.

---

## 5. Do not land a side forwarding table

The side-table idea solves the forwarding-lifetime problem, but it is the
wrong production direction.

Why it is unattractive:

- it adds substantial GC-time memory overhead
- it introduces a second forwarding truth alongside the source object
- it pushes more allocation and lookup work into the most stressed phase
  of collection
- it solves the structural limitation while doing nothing to improve the
  retention picture

If it is ever used, it should be as a short-lived experiment to validate a
hypothesis, not as the intended collector design.

The concern here is not elegance. It is operational shape. A collector that
allocates a large side structure while debugging evacuation pressure is
asking for another family of failures.

---

## 6. Two-phase evacuation is the immediate bounded fix

The most credible direction is a two-phase collector:

1. copy all marked live objects
2. rewrite all roots and live payload references
3. reclaim the source pages

That was previously framed as a later architectural step. The histogram
changes that.

The structural fix is now the bounded way to get the collector past the
current workload. The retention investigation is still necessary, but it is
open-ended and should not be the only path to getting Life to complete.

The right order is:

1. land two-phase evacuation
2. confirm `life.lisp` completes
3. investigate retention attribution in parallel
4. then tune or simplify based on what the retention work finds

---

## 7. The 20 MB live set is still the long-term bug

Even once the structural collector limit is removed, the reported live set
is still bad news.

A tiny list-based Life workload should not look like a 20 MB nursery
survivor set unless one of these is happening:

- the static area is retaining young objects through dirty-card scanning
- compiler/runtime Rust containers are holding heap words that the
  conservative scanner treats as roots
- compile-time intermediates are cached longer than intended
- conservative scanning is accepting too many false positives

This should happen in parallel with structural work, not strictly after it.

A useful breakdown is:

- explicit Lisp roots
- conservative stack scan
- static-area outgoing references
- remembered old-to-young edges
- any compiler/runtime-owned root path that is not represented above

The goal is not a perfect ownership proof. The goal is to identify the
largest retaining source quickly enough to stop guessing.

---

## 8. Conservative scanning still deserves active suspicion

Even though pinning is not the immediate explanation for the failed sweep,
conservative scanning remains a plausible source of excessive retention.

Examples:

- `Vec<u64>` buffers containing heap words
- temporary compiler data structures
- old stack frames
- hash-table buckets with stale words

If the conservative scanner accepts any aligned in-range word as a valid
reference, the collector will keep memory alive for reasons that never
appear in Lisp code.

The validation path should be as strict as the implementation can tolerate.
At a minimum, a candidate should satisfy checks like:

- address is inside a collectable page
- address lands on a plausible object start, unless interior pointers are
  intentionally supported
- tag/layout is valid for the object class found there
- page metadata agrees that the cell is currently meaningful

If interior conservative pointers are supported, the expected retention cost
is much higher and should be called out explicitly.

---

## 9. Add a synthetic structural test before or during architecture work

`life.lisp` is useful because it is real and noisy. It is not ideal as the
only acceptance case for structural GC work.

Add a targeted test that does this:

- allocate enough objects to fill many pages
- keep exactly one object alive on each page
- ensure there are no zero-live pages available for reclaim
- trigger a collection with reserve insufficient to hold all survivors

That test should fail today for the structural reason described in the
review. Later, it becomes the proof case for two-phase evacuation.

Without a test like this, the architecture work risks chasing Life's mixed
symptoms rather than the collector invariant itself.

---

## 10. Write down the key invariant in code

The earlier attempt to free source pages sooner crashed because it violated
an invariant that should be written near the evacuation logic itself:

> With in-heap forwarding, a source page containing forwarded objects
> remains part of the forwarding table until all roots and copied payloads
> have been rewritten away from that page.

That is the reason mid-BFS source-page reuse is unsafe in the current
shape. The code should say so plainly where the temptation to recycle early
actually appears.

---

## 11. Recommended sequence

The practical next steps are now:

1. Record the histogram result and retire Bug A.
2. Implement two-phase evacuation.
3. Make `life.lisp` complete under the new structure.
4. Add root-source attribution to explain the 20 MB live set.
5. Add a synthetic high-survival structural test.

The main advice is now: **do not let open-ended retention debugging block a
bounded collector fix.**

The accounting is no longer the issue. The remaining choice is whether to
wait for the retention investigation to save the current design, or to land
the collector shape that handles high-survival workloads and debug the
retention with a working system. The better bet is the latter.
