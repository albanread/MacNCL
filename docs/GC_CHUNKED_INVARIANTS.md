# Chunked GC — invariants, current state, audit

*Written 2026-05-16 after seven iterative "fix-the-symptom" rounds
chasing `demos/life.lisp` to gen 25. The goal of this doc: replace
debugger-driven patches with a fixed list of invariants every code
path can be audited against.*

This is a working doc, not a spec. It exists so the next bug fix
starts from "which invariant does this violate?" instead of "what
does the new crash signature look like?"

---

## 0. Vocabulary

- **Mutator** — a Lisp thread; can be the trigger or parked.
- **Trigger** — the mutator that observes `young exhausted` and
  drives the cycle.
- **Cycle** — one `collect_minor_with_static` call. Multiple
  *chunks* inside it. May include a cascade (G1 → Tenured) at the
  end.
- **Chunk** — one iteration of the `while idx < from_pages.len()`
  loop in `evacuate_with_roots`. Inside one chunk: Phase 1 (copy)
  → Phase 2 (rewrite) → Phase 3 (reclaim).
- **from_gen** — generation being evacuated this `evacuate_with_roots`
  call (G0 for minor, G1 for cascade).
- **dest_gen** — destination generation (G0/G1 for minor, Tenured
  for cascade).
- **Live Word** — a `Word` slot the mutator may read after the
  cycle returns.
- **Forward marker** — `Word::forward(new_addr)` written by Phase 1
  at the source cell of a just-copied object.

---

## 1. Invariants

### I-1 (mark completeness)

> **Every object reachable from the root set has its mark bit set
> before Phase 1 copies anything.**

The root set is the union of:

- `MutatorState::roots` Vec (every registered mutator).
- Static-area dirty cards (cells found dirty when mark scans).
- Reservation dirty cards on pages in G1/Tenured.
- Conservatively pinned cells (every cell whose address appears as
  a tagged Word inside any `pin_stack_range`).
- The transitive closure under "payload of a reached object."

NCL's JIT does not call `push_root`/`pop_root` — every Lisp value
reaches the GC via the conservative scan or via dirty cards. So
"root set" effectively means "conservative scan + cards."

**Known violations (and fixes landed):**

- I-1a: precise mark pass (`mark_minor_with_static`) runs *before*
  conservative pin, so the closure of pinned-only-reachable objects
  isn't visited. Fix: `extend_mark_from_pinned(G0)` and
  `extend_mark_from_pinned(G1)` after the pin pass, before
  evacuation.
- I-1b: cards record old → young writes. If the mutator never
  re-writes a long-lived cross-gen field (e.g. a closure's `env`
  pointing at a never-mutated env Vector), and we clear cards at
  end of cycle, every subsequent cycle's mark misses that field.
  Fix: `clear_cards_unless_intergen` retains cards whose cells
  still contain heap-pointer Words.

### I-2 (forward-marker scope)

> **A cell containing `Word::forward(t)` is a current-cycle forward
> marker if and only if (a) its start_bit is set, (b) `t` is inside
> the heap reservation, and (c) the cell is on a page that was a
> from_gen source at any point in this cycle.**

Why each clause:

- (a) Float `HeapHeader`s have `TYPE=7=0b111` (same low 3 bits as
  `Tag::Forward`); Phase 1 doesn't clear start_bits, so a
  current-cycle marker has its start_bit set, but a Float in a
  random heap cell looks structurally identical without checking
  start_bit.
- (b) Float headers also pass the tag check; their decoded
  "forward target" is the `length | gc_bits` field — typically
  under a few hundred. Reservation membership rejects them.
- (c) After Phase 3 of an earlier chunk, source cells on flipped
  pages keep their forward-marker *bytes* but their start_bits get
  cleared (Phase 3 clears non-pinned start bits). Stale markers
  from prior cycles must not be followed.

**Known violations (and fixes landed):**

- I-2a: `is_forward()` alone is insufficient; led to corrupting
  references-to-Floats. Fix: `is_real_forward_target_at(heap,
  cell_idx, raw)` enforces all three clauses.
- I-2b: my first version of `maybe_rewrite_word` gated on
  `page.gen == from_gen`. Promotion (G0 → G1) flips source pages
  into dest_gen mid-cycle; later chunks of Phase 2 saw flipped
  pages with valid forward markers but mis-skipped them. Fix:
  gate on `page.gen != Free` plus the start-bit check above.

### I-3 (post-Phase-2 invariant)

> **After Phase 2 of chunk K, no Word anywhere in the heap, in any
> mutator stack range, or in any pinned object's payload, refers to
> a chunk-K source cell unless that cell is on a flipped (pinned)
> page and is itself a pinned object.**

The "anywhere" is the load-bearing word. Concretely, Phase 2 must
walk:

- Mutator roots via the closure (`visit_roots`).
- Every dirty card in static area + reservation (via the same
  closure).
- Every live page in `from_gen` and `dest_gen` — pages that were
  flipped earlier in this cycle, pages that have been allocated as
  dest pages so far, and pages still pending Phase 1 in later
  chunks. We currently walk every non-Free page to be safe.

**Known violations (and fixes landed):**

- I-3a: original `live_pages` filter was `from_gen ∪ dest_gen`.
  Missed Tenured pages that might hold cross-gen refs created by
  earlier mutator writes whose cards have since been cleared.
  Fix: walk every non-Free page.

### I-4 (Phase-3 reclaim safety)

> **Phase 3 of chunk K may only release a source page P if no Word
> reachable post-cycle still points at any cell in P.**

A page is "released" iff it has no pins. Pins are set by
`pin_pointers_in_ranges` for every cell whose address appears as a
tagged Word in any stack range. So the operative claim is:

> If a cell C on page P is the target of a live Word, then either
> (a) C is pinned (page P is flipped, not released), or (b) the
> Word was rewritten by Phase 2 to point at C's dest copy.

Today this is brittle. (a) covers stack-resident refs only because
the conservative pin scan walks the *stack ranges* — anywhere a
tagged Word appears, the target gets pinned. (b) covers in-heap
refs only because Phase 2 walks every live page.

**The shape of every crash we've seen** is this invariant failing:
some Word, in some location Phase 2 didn't reach AND that the pin
scan didn't pin, points at a chunk-K source cell that Phase 3
releases. After the page is recycled into a new dest, the stale
Word reads new bytes and the JIT misinterprets them.

### I-5 (object integrity post-cycle)

> **After the cycle returns, every cell on a non-Free page whose
> start_bit is set contains a valid object header (cons-start = a
> regular Word; boxed-start = a `HeapHeader`).**

Phase 1 writes `Word::forward` at source cells, which violates this
*during* the cycle. Phase 3 must restore it:

- For released pages: cells get blown away when the page is
  re-acquired (`acquire_free_page` zeros bytes); fine.
- For flipped pages: Phase 3 clears start_bits for non-pinned
  cells. Pinned cells keep their original content (Phase 1 didn't
  touch them — pinned cells are skipped). Forward markers on
  *flipped pages* persist as bytes but with start_bit cleared, so
  no walker that respects start_bits ever interprets them as
  objects.

This is currently correct *as long as nothing reads a non-pinned
cell on a flipped page through a stale Lisp pointer*. Which brings
us back to I-4.

### I-6 (no live Word ever points at a non-pinned cell on a flipped page)

> **After the cycle returns, no Word the mutator may dereference
> points at a non-pinned cell on a flipped page.**

This is the structural form of "no stale pointer to a forward
marker." It's what I-3 + I-4 must conspire to guarantee.

Two ways to make this true by construction:

- **Precise GC**: every Lisp pointer the mutator may read has been
  rewritten by Phase 2 (because every such pointer is reachable
  through a precise root, dirty card, or live page payload).
  Requires the JIT to push_root at every safepoint — large change.
- **Stale-pointer safety**: even if I-6 is violated, the cell the
  stale Word points at is zeroed/poisoned so no future read
  returns a usable Word bit pattern. Cheap, local change in
  Phase 3.

We are currently relying on **partial precise GC** (Phase 2 walks
every live page; conservative pin pins everything on the stack)
plus *no* stale-pointer safety. Whenever there is a single Lisp
pointer Phase 2 doesn't reach, I-6 fails.

---

## 2. Audit table

Mark each combination as ✓ (handled), ✗ (violated), or ? (uncertain).

| Invariant | Mutator stack | Static area | Tenured | G1 | G0 | Pinned-page payloads | Pinned-page non-pinned cells |
|---|---|---|---|---|---|---|---|
| I-1 mark complete | ✓ (extension+pin) | ✓ (cards retained) | ✓ (cards retained) | ✓ (cards retained) | ✓ (precise mark) | ✓ (extension) | n/a — never alive post-Phase-3 |
| I-2 forward scope | n/a | ✓ (start-bit + reserve + non-Free) | ✓ | ✓ | ✓ | ✓ | ✓ (rejected by start_bit) |
| I-3 post-Phase-2 refs | **?** (see §3.A) | ✓ (cards) | ✓ (page walk) | ✓ (page walk) | ✓ (page walk) | ✓ (page walk) | ✓ (no start_bit, never iterated) |
| I-4 reclaim safety | derived from I-3 | derived | derived | derived | derived | derived | derived |
| I-5 object integrity | n/a | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ (start_bits cleared) |
| I-6 no stale-to-flipped | **?** | depends on I-3-static | depends on I-3-Tenured | depends on I-3-G1 | depends on I-3-G0 | ✓ | depends on I-3 |

The two `?`s are the same `?` viewed from different angles: **a
Lisp pointer in the mutator's stack range that is NOT conservatively
pinned and whose target gets reclaimed**.

---

## 3. Where the remaining bug almost certainly lives

### 3.A The "every tagged-looking value on the stack is pinned" assumption

Conservative pin walks `[my_rsp, my_stack_hi)` as raw `u64`s and
pins any value whose:

1. Low 3 bits ∈ {Cons, Symbol, Vector, Function, String}.
2. Decoded payload points inside the heap reservation.
3. Target page is in the *target* generation (we pin both G0 and
   G1, per the recent fix).
4. Target cell has a start_bit set.
5. Tag matches the start_bit pair (`Cons` → `11`, others → `01`).

A stack slot containing a heap pointer to a **Tenured** object
fails gate (3): we only pin G0/G1. Tenured objects don't move under
minor GC, so this is normally fine. But:

- During the **G1 → Tenured cascade**, G1 source pages are being
  evacuated. If a stack-resident Word points at a Tenured object
  that *we don't know about because we don't pin Tenured*, that's
  no issue (Tenured doesn't move).
- The issue is the reverse: a stack-resident Word points at a G1
  cell that **we pin in step 3 with target=G1, NOT with
  target=Tenured**. So far so good. Then the cascade runs, sees
  pinned G1 cells, flips their pages, doesn't move them. Stack ref
  stays valid. ✓

Hmm. Restate the actual gap. The fix `extend_mark_from_pinned(G1)`
is necessary, and we landed it. So pinned-G1's transitive closure
is marked. Non-pinned G1 objects pointed to by pinned-G1 objects
get copied to Tenured. Phase 2 walks pinned-G1 payloads (page walk
of live pages) and rewrites the Words. ✓

So what's left?

### 3.B The 0x40 / `<Cons:0x…>` / `<forward:0x…>` signature

Each variant of the crash has been a Word *in some location not
walked* pointing at something we'd already reclaimed and recycled.
Suspect locations, ranked by how confident I am:

1. **A heap cell whose containing object lost its start_bit on a
   flipped page** in an earlier cycle, and that cell still holds a
   Lisp-valid Word from before the flip. Phase 2 doesn't walk it
   (no start_bit). The mutator never reads it directly. But —
   here's the issue — if a *different* live object holds a Word
   pointing at THAT cell, the dereference reads garbage.
   - Example: a closure was on a flipped page. After flip, its
     start_bit is cleared. Its `Function.env` field, however, lives
     in **static area**, dirtied since closure creation. Static
     dirty-card scan walks the env field. The env field's target
     is the env Vector on the flipped page. The env Vector's
     start_bit IS cleared. The env Vector's address still points
     into the heap (page is now dest_gen, not Free).
     `is_real_forward_target_at(env_vec_addr)` sees: start_bit
     cleared → not a forward marker. Don't rewrite. Word still
     points at the OLD env Vector address.
   - Wait — but the env Vector wasn't *just* on a flipped page; it
     was either pinned or copied. If pinned, start_bit re-set in
     Phase 3, all is well. If copied, the source cell has a real
     forward marker with start_bit still set during the SAME chunk's
     Phase 2 → rewrite happens.
   - The hole: env Vector copied in chunk K, source page flipped in
     chunk K's Phase 3 (cleared start_bits). Function.env's card
     was clean at the time (clean-clear thresh was after chunk K's
     Phase 2 → during Phase 2 it was scanned). So Function.env got
     rewritten correctly during chunk K's Phase 2. ✓
   - Then in cycle K+1, env Vector at its NEW address moves again.
     Cycle K+1's mark — sees the env Vector via static dirty cards
     (because Function.env card has heap pointer, retained from K).
     Phase 1 copies env Vector. Forward at K's-dest cell. Phase 2
     walks Function.env, sees Word pointing at K-dest, checks
     forward, rewrites to K+1-dest.
   - So the env Vector's Function.env pointer gets re-rewritten
     each cycle. Should be sound.

2. **The mutator's stack actually contains a Word pointing into a
   page that was recycled and re-allocated, AND the conservative
   pin scan didn't pin its original target** because the original
   target was in a non-mutating gen at scan time (e.g., Tenured
   from a *prior* cycle's cascade) but then a *later* cascade —
   wait, we don't cascade Tenured.

I'm guessing now. Time to stop guessing and instrument.

---

## 4. Recommendations

### 4.1 Stop adding rewrite-path guards

Every guard so far closed a real bug, but each is reactive. New
violations of I-6 will keep surfacing as the workload exposes more
of the heap's state space.

### 4.2 Two concrete next steps, in order

**Step 1 — Land "Phase 3 zeros non-pinned cells on flipped pages"
(implements stale-pointer safety from I-6).**

This guarantees I-6 by construction: even if Phase 2 misses a
Word, dereferencing it through a now-stale path reads `0` (a
fixnum), not a Word::forward, not a Tag::Cons-shaped HeapHeader.
The JIT will then either no-op (because the fixnum doesn't pass
its expected-type check) or fail loudly with a clear panic, never
crash by reading `[0x40]`.

Cost: writes ~95% of a page's cells to zero on every flip. Worst
case ~8 KB writes per flipped page per cycle. For Life with ~20
flipped pages per cycle, ~160 KB written per GC — negligible
relative to the copies Phase 1 already does.

This is the "by construction" fix the iterative path was crawling
toward.

**Step 2 — Add per-cycle instrumentation to surface I-3 violations.**

For each cycle, after Phase 2 completes, scan every non-Free page
and every dirty card for any Word that (a) has a heap-pointer tag,
(b) points into the heap reservation, (c) targets a cell whose
start_bit is cleared. Report (containing object, target address,
generation history).

These are exactly the I-6 violations. With the post-cycle
verifier, we'd know — before the mutator runs and crashes —
whether each cycle leaves stale pointers behind. The current
iteration loop would compress from "run Life, get a new
post-mutator crash, reason backward" to "run any program with
GC pressure, get a clean diff of stale pointers per cycle."

The verifier is expensive (O(total heap cells)) but is debug-only;
gate behind `NCL_GC_VERIFY=1`.

### 4.3 Things to NOT do

- Don't keep adding gates in `maybe_rewrite_word` / `is_real_forward_target_at`. Each one closes a leaf, not the trunk.
- Don't widen the conservative pin scan further (e.g. pinning
  Tenured). Tenured doesn't move, so pinning it does nothing
  useful; widening attracts more false positives.
- Don't add precise GC roots (push_root in the JIT) yet. That's the
  *right* long-term fix but it's a multi-day compiler change. Land
  the zero-on-flip fix first; revisit precise GC only if Life shows
  a *different* class of bug after that.

---

## 5. Status of fixes landed so far

| Fix | Invariant addressed | Tests | Land in main? |
|---|---|---|---|
| Chunked two-phase mark-evacuate-rewrite | I-3 (post-Phase-2 refs) | 99/99 unit tests pass | Yes |
| Crash handler `brk.rs` + JIT symbol registry + GC/heap dump | n/a — diagnostics | n/a | Yes |
| `extend_mark_from_pinned(G0)` | I-1a | 99/99 | Yes |
| `extend_mark_from_pinned(G1)` | I-1a (cascade) | 99/99 | Yes |
| `is_real_forward_target_at` (start_bit + reservation gates) | I-2a, I-2b | 99/99 | Yes |
| Phase 2 walks every non-Free page | I-3a | 99/99 | Yes |
| Conservative pin pass over G0 + G1 | I-1a (cascade) | 99/99 | Yes |
| `clear_cards_unless_intergen` | I-1b | 99/99 | Yes |
| `maybe_rewrite` gates on `gen != Free` not `gen == from_gen` | I-2b | 99/99 | Yes |

All of those compile clean, pass all `page_heap` unit tests, and
each closed a specific bug class. **None of them, alone or
together, makes I-6 hold by construction.** That requires either
the zero-on-flip step (cheap) or precise-roots (expensive).

---

*Document state: written as a pause-and-audit checkpoint. Next
action: implement §4.2 step 1 (zero-on-flip), re-run Life. If Life
lands, the cycle of patching is done and §4.2 step 2 stays as a
debugging tool we'd reach for next time. If Life still fails with
a NEW signature, that's the signal to invest in precise roots.*
