# Heap-walk closure — formal definition and Phase 2 audit

The chunked two-phase GC has crashed in Life with a pattern: each iteration
produces a new stack trace pointing back at an invariant we hadn't been
maintaining, we patch it, run again, hit another. The pattern suggests the
collector is operating on **incomplete heap-walk closure** — there exist
Words it should visit but doesn't, or visits but can't act on safely. This
document writes the closure down explicitly so we can audit Phase 2 against
a stable spec instead of patching reactively.

## Pin diagnostic data

`NCL_GC_VERBOSE=1` per-pin-pass histogram for Life under chunked GC,
steady state after warmup:

```
[pin target=G0] candidates=2782 tag_pass=1338 in_range_pass=1337 in_heap_pass=603 in_gen_pass=0   start_bit_pass=0   new_pins=0
[pin target=G1] candidates=2782 tag_pass=1338 in_range_pass=1337 in_heap_pass=603 in_gen_pass=603 start_bit_pass=603 new_pins=119
```

Each minor cycle, the conservative stack pin produces **119 distinct G1
pins**, 100% of which pass every gate (tag / range / in-heap / gen /
start-bit). 119 is dramatically more than Life's actual live set
(5–39 game-state conses plus a small fixed compiler/runtime overhead).
The 603-vs-119 (≈5×) duplication factor is consistent with the same
heap pointer being spilled into multiple register-save slots in the
safepoint frame.

The high but stable pin count is the GC's input: every cycle, 119 G1
pages are retained-by-pin, their payload pointers are extension-marked,
and the resulting transitive closure of "kept alive by something on
the stack that looks like a pointer" feeds back into Phase 1 / Phase 2 /
Phase 3 of each chunk.

## The closure, formally

Define `R(t)`, the set of cells whose current Word the mutator could
dereference at time `t` (the next safepoint exit). The collector's
correctness depends on rewriting every Word in `R(t)` whose target was
moved this cycle. Equivalently, every cell in `R(t)` must be **visited
by Phase 2** in `Rewrite` mode at least once between the first
forward-marker write (Phase 1) and the last forward-marker erase
(Phase 3 zero-on-flip / zero-on-release).

`R(t)` is the smallest set containing:

- **R-stack**: every cell in any mutator stack range that the safepoint
  captured (registers spill into these ranges before the safepoint).
- **R-static**: every cell in the static area that holds a Word the
  mutator can still reach (special-variable cells, NIL, T, symbol
  function slots, FFI registration tables, the JIT-produced static
  Function objects with their captured env Words).
- **R-survivor**: every cell on a page that survives this cycle (i.e.,
  is in a generation NOT being collected, OR is on a page that flips
  rather than releases). Survivor cells include both reachable-by-
  marker objects and reachable-only-by-pin objects.
- **R-closure**: every cell whose start bit is set, that lives inside
  an object whose first cell is itself in `R(stack ∪ static ∪
  survivor)` — taken transitively. (This is the BFS closure.)

The collector must walk all of `R(t)` in `Rewrite` mode. The collector
must NOT walk any cell outside `R(t)` (walking outside risks reading
recycled bytes that look like Lisp values).

## What Phase 2 walks today

`PageHeap::phase2_rewrite` runs after each Phase-1 chunk:

| Step | Cells walked | Source |
|---|---|---|
| 2a (closure call) | Caller's `visit_roots` slots | `RootScanner` over mutator stacks |
| 2a (closure call) | Every cell in every dirty card of the static area | `scan_dirty_cards_as_roots(static_cards, ...)` |
| 2a (closure call) | Every cell in every dirty card of the reservation, restricted to G1/Tenured pages | `scan_dirty_cards_as_roots(reservation_cards, ..., page_filter=descs[G1\|Tenured])` |
| 2b (in-heap sweep) | Every payload cell of every object on every live page in every generation | `rewrite_page` over all non-Free pages |

## What the Mark pass walks today

`PageHeap::mark_minor_with_static` runs BEFORE evacuation:

| Step | Cells walked | Source |
|---|---|---|
| M1 | Caller's `visit_roots` slots | `MarkScanner` over mutator stacks |
| M2 | Every cell in every dirty card of the static area | `scan_dirty_cards_as_marks(static_cards, ...)` |
| M3 | Every cell in every dirty card of the reservation, G1/Tenured only | `scan_dirty_cards_as_marks(reservation_cards, ..., page_filter)` |
| M4 | Transitive BFS from marked G0 cells | `PageMarker::drain` |

Then `extend_mark_from_pinned(G0)` and `extend_mark_from_pinned(G1)` add:

| Step | Cells walked | Source |
|---|---|---|
| MP | Every payload of every pinned object | `extend_mark_from_pinned` |

## Mismatches between Mark and Rewrite

The Mark pass determines **which objects get evacuated** (only marked
cells get a forward marker in Phase 1). The Rewrite pass determines
**which Words see that marker**. For the collector to be sound, the
two closures must cover the same set of pointer-bearing cells — or
the Rewrite closure must be a *subset* of the Mark closure (a
strictly tighter Rewrite is safe but wastes work).

The current closures are not aligned:

- **Mismatch A** — *Rewrite walks every live page; Mark walks only
  dirty cards.* Phase 2b iterates every live page in every generation,
  including clean G1/Tenured pages and the from-gen itself. The Mark
  pass only visits dirty G1/Tenured cards. Result: Phase 2b can find a
  Word `W` on a clean G1 page whose target `C` lives in G0. Because no
  card was dirty for `W`'s container, the Mark pass never visited `W`,
  so `C` was never marked, so Phase 1 didn't evacuate `C`. Phase 3
  released or flipped `C`'s page and zeroed `C`'s bytes. Phase 2b sees
  `W` pointing at `C`, reads `C`'s now-zero content, finds no
  forwarding marker (zero is `Fixnum 0`, not `Tag::Forward`), and
  leaves `W` alone. On resume, mutator dereferences `W` → reads zero
  → silent corruption or a delayed crash.

  *Why this is alive in production despite the soft-card barrier*:
  the barrier requires every Word-write in older-gen storage to call
  `mark_card`. Production paths that DO call it: `ncl_set_car`,
  `ncl_set_cdr`, `set_vector_cell` (`aset_generic`),
  `set_symbol_value`, `set_symbol_function`, `ncl_make_closure`'s
  env-pointer. If any production path mutates an older-gen Word
  without going through one of these — or if cards are cleared
  incorrectly between cycles — `Mismatch A` fires.

- **Mismatch B** — *MP (extend_mark_from_pinned) walks pinned objects'
  payloads; Mark and Rewrite do not specially walk through pinned
  objects' fields for staleness.* When a pinned object is alive only
  via a conservatively-retained stack value, its fields can hold
  pointers to cells that **were** live in a previous cycle but
  have since died, been promoted away, or been recycled into a new
  object. Once those targets are no longer in `R(t)` from any other
  path, Phase 2 has nowhere to rewrite to — the field stays pointing
  at recycled bytes. Pin retention via stale stack values is the
  steady-state failure mode of conservative pinning when the live
  pin set drifts faster than the GC can flush it.

  The 119-pins-per-cycle datum makes this concrete: 119 G1 pages
  retained-by-pin every minor; each has fields that the
  extension-mark traversed last cycle but that may not have any
  live-data backing **this** cycle.

- **Mismatch C** — *Static-area clean cells.* M2 and 2a both walk
  only dirty cards of the static area. A static-area Word that was
  written before the most recent card-clear, and whose card was
  subsequently cleared by `clear_cards_unless_intergen` because no
  Word in the card looked like a heap pointer at clear time, is
  invisible to the next cycle's Mark and Rewrite. The
  `clear_cards_unless_intergen` heuristic explicitly keeps cards
  dirty when any tagged-pointer Word is present, so the failure mode
  is: a Word that *was* a pointer becomes `Fixnum 0` via a GC-induced
  zero (Phase 3 release / flip on a different page), the card now
  has no pointer-shaped Words, gets cleared — and a *later* mutator
  write back into that same card without a barrier slips through.

  *Severity*: this requires a barrier-bypassing path. None has been
  identified in the audit of `abi.rs` and `mutator.rs`. Listed for
  completeness; not the priority fix.

## Why the structural fix is precise roots, not a Phase-2 patch

We could close Mismatch A by walking every page in Mark too (or
removing the every-page walk from Rewrite). But:

- Walking every page in Mark conflates "what's reachable" with "what
  has a start bit set" — it pulls in pin-retained objects' transitive
  closure unconditionally, growing the live set monotonically with
  stale pins.
- Removing the every-page walk from Rewrite re-exposes any
  barrier-bypassing write path as an immediate corruption (today the
  every-page walk papers over those by re-finding the forward
  markers).

Mismatch B is the real story. With conservative stack scanning, the
pin set is a noisy upper bound that retains 5–10× more pages than
the precise live set warrants. Each retained page contributes its
field closure to the marked set. Over many cycles, the cumulative
pin-retained closure grows faster than precise reachability shrinks
it, and the failure isn't "we forgot to walk something" — it's "we
walked something we shouldn't have, into recycled bytes that no
discipline could keep coherent."

Conservatively pinning is correct *for the pinned object itself*. It
is unsound *for the pinned object's transitive children* the moment
any other reference to those children dies. Precise roots remove the
noise: only Words that the JIT (or runtime) has declared live get
retained; the rest of the stack is opaque bytes the GC won't touch.

## Audit decision

- **Fix in this pass**: tighten Mark to cover the same closure as
  Rewrite, by extending the Mark BFS to walk every page's start-bit
  cells in the from-gen (chunk-after-chunk, the new state). This
  closes Mismatch A symmetrically: anything Rewrite can find, Mark
  has already marked.
  - Lower-impact alternative: gate Phase 2b on `is_marked(cell_idx)`
    at the object's start. That way Phase 2b is automatically a
    subset of what Mark covered, and we cannot Rewrite-only-find
    Words whose target wasn't marked.
  - **Choosing the alternative** — it's a 2-line change in
    `rewrite_page`, doesn't alter Mark semantics, and provides the
    structural guarantee.
- **Mismatch B**: deferred to the precise-roots landing
  (`GC_PRECISE_ROOTS_PLAN.md`, Option B). Once stack scanning is
  replaced by explicit `push_root` / `pop_root`, the noise floor
  disappears and Mismatch B collapses to a non-issue.
- **Mismatch C**: not addressed; no concrete bypass identified.

## Closure spec, going forward

Phase 2 (Rewrite) walks **exactly** the cells in:

```
mutator_stack_ranges
    ∪ static_area_cells_in_dirty_cards
    ∪ reservation_cells_in_dirty_cards_on_G1_or_Tenured_pages
    ∪ start_cells_on_any_live_page_whose_object_was_marked_this_cycle
```

The last clause is the proposed tightening — today's
`rewrite_page` walks every start-cell unconditionally. After the
fix, it walks only marked start-cells, which makes Phase 2's
closure a strict subset of the Mark pass's outputs. Any rewrite
Phase 2 issues is provably backed by an evacuation Phase 1
performed.
