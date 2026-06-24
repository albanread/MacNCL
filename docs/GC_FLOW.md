# GC Flow — Page-Heap Minor Cycle

> ⚠ **Historical snapshot (2026-05-16).** The Life-OOM diagnosis and the
> "Path C not yet / Cheney can't reuse source pages mid-cycle" framing
> below were superseded by the code: block-incremental two-phase
> evacuation (chunked copy → rewrite → reclaim, reusing source pages
> mid-cycle) is implemented in `src/ncl-runtime/src/page_heap/evac.rs`,
> and precise roots are now wired in the JIT. Treat every "not yet" /
> "stalls" / "blocker" claim here as describing a past state. Verify
> against the code and `docs/GC.md`'s integration-status block before
> relying on anything here.

*Companion to `GC_DESIGN.md` and `GC_DESIGN_REVIEW_2.md`. Last updated
2026-05-16.*

This document maps the actual code paths a minor GC walks under
`--features gc-page-heap`, plus a corrected analysis of what blocks
`demos/life.lisp`. It is the artefact missing from the previous review.

---

## 1. Caller path (mutator → STW)

```
   MUTATOR (running Lisp code)
        │
        │  bump-alloc into TLAB
        │  TLAB exhausted → refill_tlab
        ▼
   ┌────────────────────────────┐
   │ MutatorState::refill_tlab  │   mutator.rs
   │   try young_try_alloc_slab │
   │   if None (Free ≤ reserve) │
   │       ↓                    │
   │   trigger_minor_gc()       │
   └────────────┬───────────────┘
                │
                ▼
   ┌────────────────────────────┐
   │ MutatorState::do_minor_gc  │   mutator.rs:653
   │   set stop_requested       │
   │   wait until all other     │
   │     mutators parked        │
   │   collect pin_ranges from  │
   │     each thread's RSP→hi   │
   │   lock heap mutex          │
   └────────────┬───────────────┘
                │  STW achieved
                ▼
   ┌────────────────────────────┐
   │ heap.mark_minor_with_static│   coordinator_api.rs:250
   │ heap.collect_minor_w_static│   coordinator_api.rs:186
   └────────────┬───────────────┘
                ▼
        … see §2 …
                │
                ▼
   ┌────────────────────────────┐
   │ clear_all_pins             │
   │ clear_mark_bits_in_gen     │
   │ clear_recycle_live_counts  │
   │ release heap mutex         │
   │ wake parked mutators       │
   └────────────┬───────────────┘
                │
                ▼
        MUTATOR resumes
```

---

## 2. The mark pass

`mark_minor_with_static` runs BEFORE evacuation. Marks alive objects
across G0 so evacuation can recycle drained pages and so `(gc-stats)`
can report a live-bytes estimate.

```
   ┌─────────────────────────────────────────────────┐
   │ mark_minor_with_static  (coordinator_api.rs)    │
   │ ─────────────────────────────────────────────── │
   │ 1. PageMarker::new(target = G0)                 │
   │    └ clear_mark_bits_in_gen(G0)                 │
   │                                                 │
   │ 2. visit_roots(MarkScanner)                     │
   │    └ for each mutator handle:                   │
   │        for r in roots:                          │
   │          marker.visit(r)                        │
   │          └ try_mark_root:                       │
   │              gate on tag, page found,           │
   │              gen == G0, kind Cons|Boxed,        │
   │              start-bit set, tag matches start   │
   │            if not already marked:               │
   │              mark_cell(cell_idx)  ← one bit     │
   │              queue.push(cell_idx)               │
   │                                                 │
   │ 3. scan_dirty_cards_as_marks(static area)       │
   │    └ for each dirty static card:                │
   │        for each cell c in card:                 │
   │          marker.visit_cell(c)                   │
   │                                                 │
   │ 4. scan_dirty_cards_as_marks(reservation,       │
   │     page_filter = G1|Tenured)                   │
   │    └ same but only on older-gen pages           │
   │                                                 │
   │ 5. marker.drain()                               │
   │    └ while queue nonempty:                      │
   │        cell_idx = queue.pop()                   │
   │        scan_marked_object:                      │
   │          determine size from start-bit pattern  │
   │          for c in payload_start..payload_end:   │
   │            try_mark_root(read_cell(c))          │
   │                                                 │
   │ 6. prepare_recycle_live_counts_from_marks(G0)   │
   │    └ for each G0 page p:                        │
   │        c = popcount(mark_bits[p_slice])         │
   │        recycle_live_counts[p] = c               │
   │        live_pages += (c > 0)                    │
   │        live_cells += c                          │
   │      ← "live_cells" is misnamed — it's the      │
   │        bit count = object-start count, not the  │
   │        actual cell count.                       │
   └─────────────────────────────────────────────────┘
```

After mark: `mark_bits` has one bit set per reachable G0 object start.
`recycle_live_counts[p]` = number of reachable object starts whose
first cell lives on page p.

---

## 3. The evacuation pass

```
   ┌─────────────────────────────────────────────────┐
   │ collect_minor_with_static  (coordinator_api.rs) │
   │ ─────────────────────────────────────────────── │
   │ 1. pin_pointers_in_ranges(G0, stack_ranges)     │
   │    └ for each candidate word in stack range:    │
   │        if it tags-and-aligns as a G0 pointer:   │
   │          set pin_byte on its page,              │
   │          insert cell_idx in pinned_cells set    │
   │                                                 │
   │ 2. release_zero_live_unpinned_pages(G0)         │
   │    └ for each G0 page p:                        │
   │        if !has_pins(p) and counts[p] == 0:      │
   │          desc[p].release()                      │
   │          (page returns to Free)                 │
   │    ← For Life: releases 3 of 960 pages.         │
   │                                                 │
   │ 3. collect_minor(visit_roots)  (cycle.rs)       │
   │    ├ tick minors_since_g0_promote               │
   │    ├ dest = G0 (within-gen) unless threshold    │
   │    │       fires → dest = G1 (promotion)        │
   │    └ evacuate_with_roots(G0, dest, …)           │
   │                                                 │
   │ 4. cards.clear_all() (reservation + static)     │
   └─────────────────────────────────────────────────┘
```

### evacuate_with_roots (evac.rs:634)

This is where Life stalls. The function is Cheney BFS with in-heap
forwarding.

```
                 ┌──────────────────────────┐
                 │ snapshot from_pages list │
                 │ alloc released_from_pages│
                 │   bool slice             │
                 └──────────┬───────────────┘
                            │
                            ▼
        ┌─────────────────────────────────────────────┐
        │ Second pre-BFS release pass — redundant on  │
        │ Life because step 2 already covered it.     │
        └──────────┬──────────────────────────────────┘
                   │
                   ▼
        Reset from_gen alloc regions
                   │
                   ▼
        Build PageEvacuator { heap, from, dest, queue }
                   │
                   ▼
       ┌────────────────────────────────────────────────┐
       │ visit_roots(evac) — caller's closure walks     │
       │ mutator roots, static-area dirty cards,        │
       │ reservation dirty cards. Each slot reaches:    │
       │                                                │
       │   evac.visit(slot) → maybe_copy(w):            │
       │     ┌── tag check + start-bit gate ───────┐    │
       │     │ if forward at source:               │    │
       │     │   return forward target             │    │
       │     │ if pinned:                          │    │
       │     │   return original word              │    │
       │     │ else:                               │    │
       │     │   allocate in dest                  │    │
       │     │   copy bytes source → dest          │    │
       │     │   write Word::forward(dest) at src  │    │
       │     │   decrement recycle_live_counts[p]  │    │
       │     │   if counts[p] hits 0 and !pinned:  │    │
       │     │     ▲▲▲ UNSAFE — Bug B ▲▲▲          │    │
       │     │     desc[p].release()               │    │
       │     │     forwarding markers gone         │    │
       │     │   queue.push(CopiedObject)          │    │
       │     │   return dest word                  │    │
       │     └──────────────────────────────────── ┘    │
       │   if dest OOMs:                                │
       │     panic_any(GcStallError::mid_evac_oom)      │
       └────────────────────────┬───────────────────────┘
                                │
                                ▼
       ┌────────────────────────────────────────────────┐
       │ evacuate_marked_pages(from_pages):             │
       │   for p in from_pages:                         │
       │     if released_from_pages[p]: continue        │
       │     for cell_idx in p's used range:            │
       │       if !is_marked(cell_idx): continue        │
       │       if !is_start_at(cell_idx): continue      │
       │       copy_marked_source_cell(cell_idx)        │
       │       └ same body as maybe_copy's copy branch  │
       │     (forwarded-already check short-circuits    │
       │      objects already copied via the root path) │
       └────────────────────────┬───────────────────────┘
                                │
                                ▼
       ┌────────────────────────────────────────────────┐
       │ drain():                                       │
       │   while queue.pop():                           │
       │     for each payload cell of the copied        │
       │       (now-in-dest) object:                    │
       │         visit_cell(cell_ptr) → maybe_copy      │
       │     (this is the BFS that finds children       │
       │     of newly-copied objects)                   │
       └────────────────────────┬───────────────────────┘
                                │
                                ▼
       ┌────────────────────────────────────────────────┐
       │ Post-BFS page reclaim:                         │
       │   snapshot pinned_cells with cons-vs-boxed bit │
       │   for p in from_pages:                         │
       │     if released_from_pages[p]: continue        │
       │     if has_pins(p):                            │
       │       desc[p].generation = dest                │
       │       desc[p].age = 0                          │
       │       clear all start bits on p                │
       │       (then re-set bits for each pinned cell)  │
       │       pages_flipped += 1                       │
       │     else:                                      │
       │       desc[p].release()                        │
       │       clear all start bits on p                │
       │       pages_freed += 1                         │
       │                                                │
       │   clear_all_pins()                             │
       │   clear_mark_bits_in_gen(from_gen)             │
       │   clear_recycle_live_counts()                  │
       └────────────────────────────────────────────────┘
```

---

## 4. The actual Life data (re-captured 2026-05-16)

Built `--no-default-features --features gc-page-heap --release` and
ran `demos/life.lisp`. Stalls at generation 25 with the structured
`GcStallError`:

```
[pre-bfs sweep] target=G0 total=960 zero=3 zero_unpinned=3 zero_pinned=0
                nonzero_unpinned=945 nonzero_pinned=12
[pre-bfs sweep] releasable.len()=3
unhandled condition: gc-stall: reason=MidEvacOOM
  trigger=young exhausted
  from=G0 dest=G0
  attempted-kind=Boxed attempted-cells=3
  pages(free/g0/g1/tenured)=0/1280/0/0
  pinned-pages=12 pin-set=38
  reserve-pages=320
  copied(objects/cells)=874658/2637479
  mark(live-bytes/live-pages/zero-live-pages-released)=20195120/957/3
  recycled-mid-evac=0
  static(used/committed)=96511336/97517568
```

Decoded:

| Quantity                        | Value          | Source                                              |
| ------------------------------- | -------------- | --------------------------------------------------- |
| Total pages                     | 1280           | 80 MB heap / 64 KB                                  |
| G0 pages at pre-BFS time        | 960            | histogram `total`                                   |
| Reserve pages                   | 320            | `page_count / 4`                                    |
| Zero-mark pages released        | 3              | histogram `releasable.len`                          |
| G0 dest pages available for BFS | **323**        | reserve + zero-mark released                        |
| Marked object starts in G0      | **2,524,390**  | `mark-live-bytes / 8` (live_cells is a bit count)   |
| Objects copied at OOM           | 874,658        | `objects-copied`                                    |
| Cells copied at OOM             | 2,637,479      | `cells-copied`                                      |
| Avg cells per copied object     | **3.015**      | cells_copied / objects_copied                       |
| Dest pages already consumed     | ~322           | cells_copied / 8192                                 |
| Mid-evac recycles               | **0**          | the unsafe mid-BFS recycler never fired             |

### What the BFS still owes when OOM hits

Marked but not yet copied: `2,524,390 − 874,658 ≈ 1,649,732 objects`.

At avg 3.015 cells/object, that's `≈ 4,973,932 cells = ~607 pages of dest`.

Available dest at OOM: `0`.

**Shortfall: ~607 pages.**

The previous review's "we needed one more page" is wrong. The
arithmetic mistake was reading `cells_copied = 2,637,479` as "total
survivor volume." It is "survivor volume successfully copied before the
allocator gave up." The BFS was only 35% of the way through.

### What the workload actually contains

Conway's Life seeded with the R-pentomino, board state ~40 conses
= 80 cells.

The collector sees 2.5M live objects = ~7.6M cells of live data —
**95,000× the game state**.

That is the §5 retention question from `GC_DESIGN_REVIEW_2.md`,
restated with sharper numbers. The collector is not failing to
reclaim a small overage; it is being asked to evacuate ~7.6 MB of
"live" memory on every minor cycle in a workload whose Lisp-visible
state is under 1 KB.

---

## 5. Why `Design 2 (single-shot)` does not unblock Life

Design 2 as written in `GC_DESIGN_REVIEW_2.md` §4.3:

```
   Phase 1: copy all marked objects from G0 to dest, leave
            forwarding marker at each source cell
   Phase 2: walk all roots + live dest cells + dirty cards,
            rewrite each Word that points into G0 via the
            forwarding marker
   Phase 3: reclaim source pages (release or flip)
```

The structural fix Design 2 buys is **correctness** under in-heap
forwarding: Phase 3 only runs after Phase 2 has consumed all
forwarding markers, so no Word survives pointing at a freed source
page. That fixes the latent UB in the current mid-BFS recycler.

It does **not** reduce peak memory demand. Phase 1 still copies every
marked object to dest. For Life that is ~7.6M cells = ~928 pages of
dest. Source pages stay live through Phase 2. Phase 3 reclaim happens
at end. Peak dest demand is identical to Cheney's.

If Design 2 lands single-shot, Life still OOMs in Phase 1 with the
same shortfall (~600 pages). The only difference: the panic is from
Phase 1 instead of from Cheney BFS.

---

## 6. What actually unblocks Life

Three real paths. Each is bounded; their costs differ.

### Path R — Retention attribution (§5)

The collector reports 2.5M reachable object starts. Game state is ~40
conses. The 60,000× gap is either:

- compiler/macroexpand intermediates retained in a `Vec<Value>` on the
  Rust heap that the conservative scanner walks,
- interned tables / symbol caches in `Session` not being cleared
  between top-level forms,
- REPL history / `*last-result*` globals binding old structure.

Add per-root-source live-bytes attribution (mutator-roots,
conservative-stack, static-cards, reservation-cards). Run Life,
sort by contribution. The smallest-fix-with-largest-effect probably
sits in one of those buckets.

If real live set drops to ~10K objects (~30 KB), dest demand drops to
single-digit pages and the current reserve handles Life trivially.
No Design 2 needed.

**Cost**: 1–3 days of investigation if the source is obvious; open-
ended if not. **Highest expected value per hour.**

### Path C — Block-incremental two-phase

A real fix for sustained high-survival workloads even if retention is
genuine.

Loop over source pages in chunks. After each chunk: copy → rewrite
refs to that chunk's source addresses → reclaim chunk's source pages.
Chunk size bounded by available Free pages at each iteration.

```
   for chunk in source_pages.chunks(K):
       Phase 1a: copy chunk's marked objects to dest
       Phase 2a: walk roots + live dest + dirty cards;
                 rewrite Words whose forwarding marker is in
                 a chunk page
       Phase 3a: release chunk's pages (or flip if pinned)
```

Rewrite-pass cost: O(N_chunks × (roots + dest_so_far + cards)). For
Life with ~10 chunks the rewrite cost is bounded and acceptable.

Compatible with the design review's structural vision; this is its
necessary extension.

**Cost**: ~1.5–2 weeks of careful work. The largest item is choosing
the chunk-boundary policy (per-page is correct but wasteful; per-N
pages requires tracking forward-marker provenance).

### Path S — Side forwarding table

Anti-recommended in the review and that recommendation holds. For
Life, the per-cycle side-table size is ~875K entries × ~16 bytes ≈
14 MB, allocated under pressure. The argument against it stands.

The one place a side table is worth reconsidering is as a **bounded
fallback** when the in-heap forwarding path runs out of from-page
slack mid-cycle. We have not designed that and I do not recommend it
ahead of Path R.

### Anti-path — Reserve bump

The review anti-recommends this. For Life the gap is so wide
(reserve would need to roughly triple) that the mutator's effective
working set drops by the same factor. Throughput collapses. Not
worth it.

---

## 7. Recommendation

1. **Path R first.** Add per-root-source attribution to the mark
   pass. One Life run will tell us where the 7.6M cells come from.
   If 95% of the retention has one source (very likely for a workload
   this skewed), fixing that source unblocks Life immediately.

2. **Remove the unsafe mid-BFS recycler in `maybe_copy` and
   `copy_marked_source_cell`** independently of Path R. Currently it
   never fires on Life, but if any other workload pushes mark counts
   high enough that a page drains, the release at `remaining == 1`
   trashes forwarding markers. It is correctness-debt either way.

3. **Defer Path C** until either Path R fails to bring live-set
   under control, or a real workload exhibits genuine high-survival
   behavior. Path C is the right architecture; it is not the urgent
   blocker.

4. **Keep `GC_DESIGN_REVIEW_2.md` recommendation #4** (the validation
   bar for Life). It applies regardless of which path lands.

The previous review's instinct — "Design 2 is bounded, retention
investigation is open-ended" — is correct in general but assumes
Design 2 fixes Life. The data says it doesn't, on its own. Path R is
the bounded fix; Path C is the structural fix; they are not
substitutes.

---

*Document state: post-data-recapture, 2026-05-16. The Life panic was
re-run live; numbers are reproducible.*
