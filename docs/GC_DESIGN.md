# GC Design — Target Architecture for NCL

This document records the target garbage-collector architecture for
NCL and the migration plan to reach it. It was synthesised
from a research pass across **Clozure CL** (`E:\ccl`) and
**SBCL** (`E:\sbcl`) in late session work — those two codebases
represent the most mature open-source CL collectors and embody
complementary design traditions (CCL: precise + mark-compact;
SBCL: conservative + mark-evacuate + parallel).

The current `docs/GC.md` describes the GC NCL has today (a 2-semispace
generational copying collector with conservative stack scanning).
**This document describes where it needs to go**, and why.

Status: **design / target** — Phase 1 is the only part that's a
near-term commitment. Phases 3+ are research-grade plans pending the
priority push.

---

## 0. Diagnosis — why we're doing this

The macroexpand-all workload exposed a structural problem in the
current GC:

1. **Conservative stack scanning over-pins.** Macroexpand recurses
   50+ frames deep into the form tree. At each frame, several Words
   sit on the stack (form, env, partial result). The conservative
   pinner sees all of them on every stack scan and pins their targets
   — including false-positive matches where a frame pointer or a
   plain integer happens to share bit-pattern with a heap-shaped tag.

2. **"Promote on first survival" is too aggressive.** Anything that
   survives one minor GC is promoted straight to old. With (1)
   over-pinning, this means MBs of macroexpand transients land in
   old per minor cycle. Old (64 MB cap) fills in seconds. Full GC
   finds them still pinned by the still-deep recursion. Spiral,
   OOM, 2.4 GB resident before the OS kills the process.

3. **Static area is hard-capped at 64 MB.** Any workload that pushes
   past this (large image, many `defun`s, many `defstruct`-generated
   accessors) panics with "static area exhausted." It's been a
   correctness footgun more than once.

The problems are independent — (1) and (2) are GC tuning; (3) is a
memory-region shape issue. They all need fixing.

---

## 1. What CCL and SBCL actually do

### CCL (E:\ccl\lisp-kernel)

- **Precise, generational, stop-the-world mark-compact-in-place.**
  Markbits bitmap; compute relocation deltas from the bitmap; rewrite
  refs in place; slide survivors down.
- **No stack maps**, no precise-by-construction. Precision comes from
  **compiler discipline**: the value stack contains only tagged Lisp
  objects (compiler invariant), the control stack contains no Lisp
  objects, registers at GC time are read from a saved exception
  frame with a fixed mask.
- **Three ephemeral generations** (g1, g2, tenured); promotion via
  sliding boundaries, not per-object age.
- **Refbits write barrier** with a two-level compressed index — minor
  GC root scan is O(dirty cards), not O(old heap).
- **GC is tightly coupled to CCL's runtime**: 2-word dnodes, header
  conventions, TRA opcode peeking, fixed register convention. Not
  portable. ~11k lines of C across `gc-common.c`, `x86-gc.c`,
  `arm-gc.c`, `ppc-gc.c`.

**Key takeaway**: CCL's precision strategy doesn't transfer to NCL.
LLVM happily spills mixed-type SSA values onto a single native stack,
and we have no compiler mechanism to enforce "value stack is
tagged-only." But CCL's mark-compact-in-place idea is valuable —
it halves address-space footprint vs semispace copying.

### SBCL (E:\sbcl\src\runtime)

- **`gencgc.c`** is the workhorse: page-based, conservative,
  generational, mark-evacuate. **This is the closest match to NCL's
  constraints.**
- **Page-based heap**: one large reservation, divided into fixed-size
  pages (e.g. 64 KB). Each page has a `PageDesc` carrying its
  generation, type, words-used, scan-start, and pin metadata.
- **Sub-page pin bitmap (8 slots/page) plus a hopscotch hashtable**
  of pinned tagged pointers. Pinning is one byte-load + branch on
  the fast path. Critically: **pinning prevents movement, NOT
  reclamation of neighbours.** A page stays in oldspace-from but
  only the pinned object survives; surrounding garbage is freed
  when the page is promoted/recycled.
- **Mark-evacuate, not copy-to-newspace**: unpinned survivors are
  copied to open `alloc_region`s in the new space; pages with zero
  pins are freed entirely; pages with pins keep just the pins and
  become part of newspace via a page-generation byte flip.
- **Promotion is age-thresholded** via `number_of_gcs_before_promotion`
  per generation. Young gets collected several times before any of
  it is raised. There's also `minimum_age_before_gc` so we don't
  waste cycles on a near-empty generation.
- **Trigger**: `bytes_allocated > auto_gc_trigger`, recomputed as
  `bytes_allocated + bytes_consed_between_gcs` after each cycle.
- **Stop-the-world**: signal-based (`SIG_STOP_FOR_GC`) on POSIX
  non-safepoint builds; **safepoint-poll-based** on Windows and
  safepoint builds. Windows is mandatory because there's no portable
  thread-suspend signal there.
- **Self-stack-pointer exclusion** in conservative scan
  (`gencgc.c:3248-3267`): any stack word pointing back into the
  same thread's control stack is skipped, eliminating frame
  pointers from consideration.
- **`traceroot.c`**: builds an inverted heap, BFS backward from a
  target object to find why it's pinned. The right shape for the
  "why is this stuck?" question.

**Key takeaway**: SBCL solves the same problem NCL has (conservative
scanning + JIT without stack maps) and the techniques are directly
applicable. Several are 20-line changes.

### Other GCs in SBCL

- **`pmrgc.c`** — parallel mark-region. Modern direction.
  Region-based with parallel marking workers. Worth understanding
  but premature for NCL.
- **`fullcgc.c`** — full mark-sweep fallback for debugging.
  Reference implementation.
- **`cheneygc.c`** — original 2-semispace copying. Still buildable
  on some platforms. This is structurally what NCL has today.

---

## 2. Target architecture

### 2.1 Heap shape

**Page-based mark-evacuate, single dynamic space, three logical
generations (g0=nursery, g1=intermediate, tenured) carved out of one
address range.**

- Origin: SBCL pages + CCL generation count.
- Why for NCL: a single contiguous reservation with page-level
  metadata lets us free arbitrary subsets without compacting the
  whole heap, lets pinning be cheap, and never needs a "scratch"
  semispace.
- Cost: ~1500 lines Rust, ~3 weeks.

Concrete shape:

- Reserve 1 GB via `VirtualAlloc(MEM_RESERVE)` (Windows) /
  `mmap(PROT_NONE)` (POSIX). Commit pages on demand in 64 KB chunks.
- Page table is `Vec<PageDesc>` indexed by `(addr - heap_base) >> 16`.
- Each `PageDesc`: 12 bytes — `{gen: u8, type: u8, words_used: u16,
  scan_start_offset: u32, pin_byte: u8, _pad: u8}`.

### 2.2 Pinning strategy

**Sub-page pin bitmap (8 slots/page) + hashtable of pinned tagged
pointers**, direct lift from SBCL `gencgc.c:1929-1936`.

- Origin: SBCL.
- Why for NCL: the conservative scanner produces N candidate
  pointers per stack; testing each via
  `gc_page_pins[page] & (1<<slot)` is one byte-load + branch.
  Hashtable only consulted on pin-bit hit.
- Cost: ~250 lines, 3 days.

**Self-stack-pointer exclusion** in conservative scan (SBCL
`gencgc.c:3248`):

- Every NCL thread knows its `stack_lo..stack_hi`; skip any word
  in that range during conservative scanning. Frame pointers and
  return-address-on-stack patterns are eliminated as roots.
- Cost: ~20 lines, 1 hour.
- *This is the single highest-leverage change. Lands in Phase 1.*

### 2.3 Generation count and promotion policy

**Three generations (g0, g1, tenured), each with
`num_gcs_before_promotion` threshold** (defaults: g0→g1 after 3,
g1→tenured after 5).

- Origin: SBCL `gencgc-impl.h:200`.
- Why: NCL's pain is exactly that pinned-then-tenured-instantly
  closures live forever. Sliding promotion via per-page
  generation byte — flip `page.gen = gen+1` instead of copying.
  No per-object age counter; **the generation byte IS the age**.
- After a g0-only collection, survivors stay at g0 unless the
  threshold fires. Pinned objects keep their original page gen
  (and their original address). Macroexpand-all transients now
  get 3-5 chances to die in the nursery before they pollute
  tenured.
- Cost: ~150 lines, 2 days.

### 2.4 Inter-generational pointer tracking

**Soft card marks, 512-byte cards, one byte per card** (SBCL
`LISP_FEATURE_SOFT_CARD_MARKS`).

- Origin: SBCL `gencgc-impl.h:470`.
- Why over hardware-WP: Windows `VirtualProtect` is expensive and
  the signal-handler dance is fragile. Soft marks need a write
  barrier in JITted stores — NCL's IR lowering already handles
  stores; adding `card_mark[(addr-heap_base)>>9] = 0` is one
  cmp+store after a tag check.
- NCL already has a card table (`heap.rs:30`) — just shrink the
  granularity, drop hardware-protection ambitions, and emit the
  barrier from the IR lowering pass instead of post-write scanning.
- Cost: ~400 lines (300 runtime + 100 IR), 1 week.

### 2.5 Trigger policy

**`bytes_allocated > auto_gc_trigger`**, recomputed as
`bytes_allocated + budget` after each cycle, where
`budget = max(8 MB, 0.5 * tenured_bytes)`.

- Origin: SBCL.
- Why: NCL's current "young is full → minor; old is full → full"
  is too binary. By the time old is full, you're already in trouble.
  Budget-based trigger lets you minor-GC before any single space
  is exhausted.
- Cost: ~50 lines, 1 day.

### 2.6 Stop-the-world

**Safepoint polls emitted by LLVM IR-gen, plus the existing
`stop_requested` flag for blocking calls.**

- Origin: SBCL safepoint mechanism, simplified.
- Why: signal-based STW does not work on Windows; NCL's primary
  platform is Windows. Cross-platform parity demands safepoints
  anyway.
- Concrete shape: emit `if (poll_word.load(Relaxed)) call
  gc_pitstop();` at every back-edge and function entry in LLVM IR
  lowering. `poll_word` is a per-thread atomic byte. To trigger GC,
  set it to 1 for all threads, wait for each to enter `gc_pitstop`
  which spins on a condition variable.
- Threads in foreign calls (blocking I/O) are "auto-safe" because
  they're not touching the heap; they re-check the poll word on
  return.
- The bump-pointer allocation overflow check **is** the safepoint
  for allocating code — free safepoint for the hot path.
- Cost: ~600 lines (200 IR + 400 coordinator), 2 weeks.

### 2.7 Static area

**Replace 64 MB hard cap with elastic `VirtualAlloc(MEM_RESERVE,
256 MB)` + commit-on-demand in 1 MB chunks.**

- Origin: own conclusion (not from CCL or SBCL).
- Why: static space holds interned symbols, quoted constants,
  defun'd Function records, JIT code descriptors. Growth is
  monotonic but bounded by program size. Reserving 256 MB costs
  nothing (no commit); committing in 1 MB increments costs
  page-table entries we'd allocate anyway.
- Cost: ~100 lines, 1 day.

### 2.8 Debugging affordances

**`(why-pinned <obj>)`** — `traceroot`-equivalent.

- Origin: SBCL `traceroot.c`.
- Why for NCL specifically: macroexpand-all-style "why is this
  2 MB closure stuck?" is exactly the question the current GC
  can't answer.
- Implementation: walk the heap once, build object→referrers map,
  BFS backward to a stack frame or static slot.
- Cost: ~400 lines, 1 week.

**`(gc-stats)` extensions**: per-generation
`{bytes_allocated, num_gc, last_pause_us, pinned_count}`.
Already partially present (`last_pinned_objects`).

---

## 3. Migration plan

### Phase 1 — Defuse macroexpand-all *(LANDED in part)*

Status as of implementation pass:

- **DONE**: Self-stack-pointer exclusion added to
  `pin_pointers_in_range` (`heap.rs:432`). Any conservative scan
  candidate whose target falls within the very stack range being
  scanned is skipped. On Windows the stack and heap live in
  disjoint VMAs so this is a no-op in practice today, but the bug
  it prevents (silent heap corruption from frame-pointer overlap)
  is expensive enough to warrant the cheap check.

- **DEFERRED to Phase 3**: 2-cycle promotion. The "8-byte age word
  per young-survivor batch in a side table" hack the synthesis
  agent proposed turns out to be much more invasive than 80 lines.
  Doing it cleanly requires either an eden+survivor split (new
  semispace) or a copy-then-survivors-back-down compaction pass.
  Both conflict with Phase 3's page-based heap, so building either
  as a stopgap would be discarded work. Held until Phase 3.

- **DEFERRED to Phase 3**: Auto-full-GC trigger budget.
  Implementation attempted (`trigger_full_gc` extracting the
  stop-the-world orchestration from `trigger_minor_gc`) and
  reverted after a real-world repro caused corruption: NCL's
  `collect_full` only follows explicit roots, has no conservative-
  stack-pin pass, so any JIT-stack-only-rooted Word is lost across
  a full cycle, surfacing later as `+ : non-integer operand:
  <Cons:…>` style panics. The fix requires a `collect_full_with_
  static` mirror of `collect_minor_with_static`, which is the
  natural shape for Phase 3 (page-based heap with per-generation
  thresholds). The `do_minor_gc` rename and the `full_gcs`
  `GcStats` field were kept as scaffolding so Phase 3 has the
  hooks already in place.

What remains useful from Phase 1: **the self-stack-pointer check
is a small defensive win**. The other two items moved to Phase 3
where they fit the architecture naturally.

### Phase 2 — Elastic static area *(LANDED)*

`StaticArea::new_elastic(reserved_bytes, initial_commit_bytes)`
now backs the production static area via Windows `VirtualAlloc`:

- `MEM_RESERVE` for the full reservation (default: 256 MB)
- `MEM_COMMIT` for the initial chunk (default: 1/4 of reservation = 64 MB)
- Page-aligned commit-on-demand via `grow_committed_to`, called
  from `try_alloc_cells` when the bump pointer crosses the
  committed frontier. Commits in 128 K-cell (1 MB) chunks.
- `Drop` releases the reservation via `VirtualFree(MEM_RELEASE)`.

`GcCoordinator::new` picks between Box-backed (the old shape, kept
for tests at sub-megabyte static configs) and the elastic backing
based on `ELASTIC_STATIC_THRESHOLD_BYTES = 16 MB`. Tests behave
exactly as before; production sessions get the elastic backing.

`(gc-stats)` plist now reports `:static-cap` (reservation),
`:static-committed` (currently-backed pages), and `:static-used`
(bump-pointer high-water). Typical bare-REPL numbers: 256 MB
reserved, 64 MB committed (initial-commit), 10 MB used.

Non-Windows targets fall back to Box-backed for now; proper
`mmap(MAP_NORESERVE)` support is future work.

### Phase 3 — Page-based heap rewrite  *(3 weeks)*

The real GC work. Implement the target architecture described in
§2.1-§2.5.

- Replace semispace `young`/`old` with one page-table-backed space.
- Implement `PageDesc`, `pin_bitmap`, hopscotch `pinned_objects`.
- Mark-evacuate cycle: mark live, copy unpinned out of from-pages
  into to-region open allocation regions, flip `page.gen` on
  pinned pages.
- Three generations with thresholded promotion.
- Soft-card-mark write barrier in IR lowering.

### Phase 4 — Safepoints  *(2 weeks)*

§2.6. Required before any multi-threaded mutator work and before
Windows-correct STW. Can be deferred until threads become a real
concern.

### Phase 5 — Debugging  *(1 week)*

§2.8. `traceroot` clone, `(gc-stats)` extensions, `(why-pinned)`.

### Phase 6 — Optional, defer indefinitely

Parallel mark (pmrgc-style), incremental compaction, immobile space.
Only if profiling demands it after Phase 3-4 land.

---

## 4. Total commitment

**~6-8 weeks of focused work** for a fundamentally sound GC.

**Phase 1 alone fixes the user-visible bug in a day.**

The Phase 1+2 combination (2 days) defuses the two known failure
modes — macroexpand spiral and static-area exhaustion — without
touching the GC's overall architecture. After that, NCL has a
GC that holds up under the demos and the Win32 work currently
in flight. Phases 3-5 are future-NCL work, scheduled when GC
becomes a priority block again.

---

## 5. Key files

### NCL (current state)

- `src/ncl-runtime/src/heap.rs:432` — `pin_pointers_in_range`
  (Phase 1 self-stack exclusion lands here)
- `src/ncl-runtime/src/heap.rs:862` — `collect_minor_with_static`
  (promote-on-first-survival site)
- `src/ncl-runtime/src/static_area.rs` — fixed 64 MB pinned arena
  (Phase 2)
- `src/ncl-runtime/src/mutator.rs:36` — `GcConfig` (where heap
  sizes live)
- `src/ncl-runtime/src/gc_function.rs` — Function object shape
- `src/ncl-runtime/src/word.rs` — Word tagging (3 low-bit tags)

### SBCL references

- `E:\sbcl\src\runtime\gencgc.c:1377` — `conservative_root_p`
- `E:\sbcl\src\runtime\gencgc.c:1904` — `pin_object` sub-page bitmap
- `E:\sbcl\src\runtime\gencgc.c:3148` — `conservative_stack_scan`,
  self-stack exclusion
- `E:\sbcl\src\runtime\gencgc.c:3791` — `collect_garbage`,
  generation threshold logic
- `E:\sbcl\src\runtime\gencgc-impl.h:187` — `struct generation`
  with `number_of_gcs_before_promotion`
- `E:\sbcl\src\runtime\gencgc-impl.h:463` — soft card marks
- `E:\sbcl\src\runtime\safepoint.c:38` — safepoint state machine
- `E:\sbcl\src\runtime\traceroot.c:55` — inverted-heap structure

### CCL references

- `E:\ccl\lisp-kernel\gc.h`, `area.h` — core types
- `E:\ccl\lisp-kernel\gc-common.c:1544` — `gc()` main entry
- `E:\ccl\lisp-kernel\gc-common.c:1164` — `forward_memoized_area`
- `E:\ccl\lisp-kernel\x86-gc.c:467` — `mark_root`
- `E:\ccl\lisp-kernel\x86-gc.c:1327` — `mark_simple_area_range`
- `E:\ccl\lisp-kernel\x86-gc.c:1424` — `mark_vstack_area`
  (precision-by-discipline)
- `E:\ccl\lisp-kernel\x86-gc.c:1438` — `mark_cstack_area` (empty —
  no Lisp objects on cstack)
- `E:\ccl\lisp-kernel\thread_manager.c:2353` —
  `suspend_other_threads`

---

## 5b. Phase 3 sub-phase decomposition

Phase 3 (page-based heap rewrite) is ~3 weeks of focused work — too
large to land in one session. Decomposed into 12 sub-phases, each
shippable individually with green build and all tests passing.

**Architecture: side-by-side with feature flag.** A new
`src/ncl-runtime/src/page_heap/` module tree lives alongside the
existing `heap.rs` semispace heap. `GcConfig` picks via a
`HeapBackend` enum. After sub-phase 10 demonstrates parity, sub-phase
11 flips the default, sub-phase 12 deletes the old code. ~3-4 weeks
during which both heaps coexist. Strongly preferred over in-place
rewrite because the current heap is fully working and tested.

### Sub-phases

1. **Heap-backend abstraction (trait + scaffolding)** *(LANDED)*
   Trait surface = methods the coordinator already calls.
   `GcCoordinator` holds `Mutex<Box<dyn HeapBackend>>`. Refactor only,
   no semantic change. **Acceptance**: full test suite + hellowin
   pass. Files: new `heap_backend.rs` (~180 lines including the
   `HeapBackendKind` enum + env-var resolver), `impl HeapBackend for
   Heap` block appended to `heap.rs`, coordinator's `heap` field in
   `mutator.rs` changed to `Mutex<Box<dyn HeapBackend>>` and the
   `do_minor_gc` callsite reshaped to use `&mut dyn FnMut` for the
   visit_roots callback (closure bound to a local for inference).

   **Backend switch** (extra scope, added in same pass):
   `GcCoordinator::new` reads `NCL_HEAP_BACKEND` env var
   (semispace / page-heap; default semispace). `PageHeap` variant
   panics with a pointer to the design doc until sub-phase 7 lands.
   `(gc-stats)` plist gains `:heap-backend` showing the live
   selection. Tests construct via the new `new_with_backend(config,
   HeapBackendKind::Semispace)` path so the env var has no effect on
   them. Driver `--help` documents the env var.

   Verified: 219/219 ncl-runtime unit tests, hellowin opens, 1M-cons
   stress test produces same minor=1 / old-MB=14 / peak-young-MB=16
   as before the refactor. All four backend-selection paths exercised
   (unset / explicit semispace / page-heap panics / bogus value
   falls back with stderr warning).

2. **Page reservation + commit infrastructure** *(LANDED)*
   New module `src/ncl-runtime/src/page_heap/`:
   - `mod.rs` (~30 lines) — re-exports
   - `space.rs` (~430 lines) — `PageHeap` struct with
     `VirtualAlloc(MEM_RESERVE)` reservation, page-granular
     `commit_page` / `decommit_page`, atomic commit-bit bitmap for
     lock-free `is_committed` queries, `commit_lock` mutex for
     serialized `VirtualAlloc(MEM_COMMIT)` calls.

   Public API: `new(reserved_bytes)`, `base_ptr`, `page_count`,
   `reserved_bytes`, `committed_pages`, `committed_bytes`,
   `page_ptr(idx)`, `page_of(addr)`, `is_committed(idx)`,
   `commit_page(idx)`, `decommit_page(idx)`. Page size = 64 KB
   (Windows VirtualAlloc allocation granularity).

   Non-Windows: Box-backed fallback with all pages permanently
   "committed" (Rust allocator semantics; proper mmap-based commit
   is future work).

   **9 new unit tests** covering: fresh-heap state, single-page
   commit round-trip with read/write through committed memory,
   commit-then-decommit, idempotent commit, out-of-range error,
   page_of arithmetic at exact page boundaries, 64 KB alignment,
   and a 4-thread concurrent-commit race test.

   Total test count: **228 passing** (219 existing + 9 new).
   Hellowin still opens. The new module is registered in lib.rs
   and built but not wired into the coordinator — that's
   sub-phase 11.

3. **PageDesc + page table** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/page_desc.rs` (~330
   lines):
   - `Generation` enum (Free / G0 / G1 / Tenured) with `from_u8`
     decoder, `promoted()` ladder (G0→G1→Tenured, Free and
     Tenured are fixed points), `name()` for diagnostics.
   - `PageKind` enum (Free / Cons / Boxed / Large) with the same
     round-trip + name pair.
   - `PageDesc` struct, exactly 12 bytes with `#[repr(C)]`:
     `scan_start_offset: u32`, `words_used: u16`, `generation: u8`,
     `kind: u8`, `pin_byte: u8`, `age: u8`, `_pad: u16`.
     Methods: `FREE` constant, `fresh(generation, kind)` factory,
     `release()`, sub-page pin bitmap (`set_pin/is_pinned/
     clear_pins/has_pins`).

   *Note on naming*: the field is `generation`, not `gen` — Rust
   2024 reserved `gen` as a keyword.

   `PageHeap` gained a `descs: Vec<PageDesc>` field (192 KB for the
   1 GB default reservation) initialised to `PageDesc::FREE` for
   every page. Accessor methods: `desc(idx)`, `desc_mut(idx)`,
   `descs()` (slice), `pages_in_gen(gen)` iterator,
   `count_pages_in_gen(gen)`. No atomics yet — all writes happen
   under STW.

   **15 new unit tests**: PageDesc is 12 bytes; correct alignment;
   FREE / fresh constructors; generation promotion ladder is
   correct; generation+kind byte round-trips; pin-bitmap set/clear
   semantics with idempotency; release-back-to-free; fresh heap
   has only Free descriptors; descriptor mutation round-trips;
   `pages_in_gen` filters correctly; `descs()` slice length
   matches `page_count()`; out-of-range `desc()` panics.

   Total test count: **243 passing** (228 from previous + 15 new).
   Hellowin still opens. Page-heap backend selection still panics
   at construction — no behaviour change for production.

4. **Object allocation into pages** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/alloc.rs` (~440 lines)
   plus extensions to `space.rs`:

   - **`AllocRegion` struct** in `alloc.rs`: tracks the
     currently-open page and bump offset for one
     `(generation, kind)` pair. `current_page = usize::MAX`
     sentinel = no page open yet. Helpers: `has_page()`,
     `remaining_cells()`.

   - **Six alloc regions per heap** (`G0/G1/Tenured × Cons/Boxed`)
     stored as `[[AllocRegion; 2]; 3]` on `PageHeap`. `Free` and
     `Large` get no region. `region_index(generation, kind)`
     decodes; `alloc_region()` / `alloc_region_mut()` expose
     them.

   - **Global start-bit bitmap** matching `Semispace`'s shape: 2
     bits per cell (pair `01` = boxed header, `11` = cons start,
     `00` = not a start), packed into `AtomicU64` words.
     Allocated once in `PageHeap::new` (32 MB for the 1 GB
     default reservation; 3% overhead, same ratio as the
     semispace start-bit table). `start_bits_handle()` returns an
     `Arc<[AtomicU64]>` mutators can cache for the alloc fast
     path. Helpers `set_start_bit_at` / `set_cons_start_bit_at` /
     `is_start_at` / `is_cons_start_at` in `alloc.rs`.

   - **`try_alloc_cons_in(generation)`** — 2-cell bump, sets
     cons-start pair on the first cell, advances `words_used`.
     Cons-kind pages need no start-bit bitmap walk (every cell
     is a cons start), so the bit is recorded for symmetry with
     boxed but the scanner can skip consulting it on Cons pages.

   - **`try_alloc_boxed_in(generation, n_cells)`** — variable-
     length, sets header-start bit, rejects `n_cells = 0` or
     `n_cells > PAGE_SIZE_CELLS` (Large-object path is sub-phase
     7).

   - **Page acquisition**: `acquire_free_page(gen, kind)` linear-
     scans `descs` for a `Free` page, commits it via
     `commit_page`, sets the descriptor to `PageDesc::fresh(gen,
     kind)`, returns the index. O(n_pages) — sub-phase 7 may add
     a free-list if profiling demands.

   **12 new unit tests** including the acceptance test
   (100,000 cons cells across pages, content verified at 10
   sample points; spread checks the heap used 24-26 G0 pages as
   expected). Other coverage: alloc-region empty state, first
   alloc acquires a page, pointer alignment, cons-start vs
   header-start bit, contiguous bump within a page, overflow
   into the next page, heap exhaustion returns `None`, boxed
   start-bit set, cons + boxed use different pages, oversize
   boxed rejection, `words_used` tracking.

   Total: **255 passing**. Hellowin still opens.

   *First place the bump pointer is no longer monotonic across
   young.* TLAB integration (mutator-side wiring) is sub-phase
   11; for now the page heap is reachable only via its own
   tests, not through `ncl_alloc_cons` / friends.

5. **Mark pass** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/mark.rs` (~330 lines).
   PageHeap gained:

   - `mark_bits: Box<[u64]>` — global bitmap, 1 bit per cell,
     64 cells per `u64`. 16 MB for 1 GB reservation. Plain
     `Box<[u64]>` (no atomics) because the mark pass is STW —
     exclusive `&mut self` keeps races impossible.
   - `is_marked(cell_idx) -> bool` for downstream evacuation.
   - `mark_cell(cell_idx) -> bool` returns the previous mark
     state (used by BFS as the "have I seen this?" gate; cycles
     terminate).
   - `clear_mark_bits_in_gen(target)` iterates target-generation
     pages, zeroing each page's 128-word slice of the bitmap.
   - `count_marked_in_gen(target)` + `marked_cells_in_gen(target)`
     diagnostics.
   - **`mark_from_roots(target, &[Word])`** — the main entry
     point. Clears target's mark bits, seeds queue with roots that
     pass the tag + page-generation + start-bit checks, BFS until
     empty.

   Object-size determination: Cons pages → 2 cells; Boxed pages
   → `1 + HeapHeader::length_cells()` from the first cell.
   Payload walk treats every non-header cell as a candidate
   `Word`; non-pointer bit patterns are rejected naturally by
   the tag check. Large-kind pages are a no-op for now (sub-
   phase 7 will define their shape).

   **9 new unit tests** including the acceptance test (1000
   disjoint cons cells, mark every other → exactly 500
   marked, no extras). Other coverage: empty roots marks
   nothing, single cons marks one cell, chain head marks whole
   chain, idempotent (re-mark same root = same bitmap), 5-cycle
   in the object graph terminates, fixnums/immediates are not
   followed, out-of-range pointer words are ignored,
   minor mark of G0 root does not cross into G1.

   Total page_heap tests: **45 passing** (36 + 9 new). The
   semispace heap remains the production path; mark pass is
   reachable only via the page_heap unit tests.

6. **Conservative pin scan adapted for pages** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/pin.rs` (~330 lines):

   - **Two-level pin index**: `PageDesc.pin_byte` (8 slots per
     page = one per 8 KB sub-region) is the fast path —
     one byte-load + bit test. `PageHeap::pinned_cells:
     HashSet<usize>` is the precise level — consulted only on
     fast-path hit. Lets evacuation distinguish "this specific
     object is pinned" from "this page has *some* pinned object
     near it."
   - **`pin_pointers_in_ranges(target, &[(lo, hi)])`** — walks
     each byte range word-by-word; for each candidate Word
     passing the 6 gates (tag, self-stack-exclusion, page
     lookup, generation match, start-bit, dedup), sets the
     page-byte slot and inserts the global cell index into the
     hashtable.
   - **`is_pinned_cell(idx)`** — public predicate. Fast-rejects
     via page byte; falls through to set lookup.
   - **`clear_all_pins()`** — resets every page's `pin_byte` and
     empties the hashtable. Called at GC-cycle start.
   - **Constants exposed**: `PIN_SLOTS_PER_PAGE = 8`,
     `CELLS_PER_PIN_SLOT = 1024`.

   `PageHeap::descs` and `PageHeap::pinned_cells` changed to
   `pub(super)` so sibling modules can mutate without going
   through accessors.

   **10 new unit tests**: empty range pins nothing; cons pointer
   on fake stack pins the object; fixnums/NIL/T are not followed;
   out-of-range pointer rejected by page lookup; pointer into a
   cons cdr cell rejected by the start-bit gate; cross-gen
   pointer skipped when scanning G0; duplicate pointers pin once
   (HashSet dedup); self-stack-exclusion skips intra-range
   pointers; `clear_all_pins` resets both levels;
   `pin_byte` groups cells into the correct 1024-cell slot.

   Total page_heap tests: **55 passing** (45 + 10 new).

   Note on "deeply invasive": the structural change (pinner no
   longer asks "in young semispace?" but "page in target gen?")
   is contained to `pin.rs`. The PRODUCTION integration in sub-
   phase 11 is what will be invasive — callers of
   `pin_pointers_in_range` in `mutator.rs` need to be retargeted.
   Sub-phase 6 itself is purely additive to the page_heap module.

7. **Evacuation / compaction pass** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/evac.rs` (~500 lines)
   plus a 4-line registration in `mod.rs`. Cheney-style BFS
   evacuation ported from the semispace `copy_into` /
   `scan_to_completion` pair (`heap.rs:1050-1132`, `:977-1000`)
   with three page-heap-specific adaptations:

   - **`PageEvacuator`** stateful visitor handed to the caller's
     root closure: `visit(&mut Word)` reads a slot, runs the BFS,
     and rewrites in place. `evacuate_with_roots(from, dest, F)`
     is the entry point; `evacuate_from_word_roots(from, dest,
     &mut [Word])` is a thin test/driver helper.
   - **Page reclaim**: snapshot `from_pages` at start of cycle,
     run BFS, then iterate the snapshot:
     - No pins → `PageDesc::release()` back to Free. Page's
       start-bit slice zeroed via `clear_page_start_bits`.
     - Has pins → `desc.generation = dest_gen`, `age = 0`. Page
       stays in place; non-pinned start bits cleared so
       abandoned forwarding markers / dead-but-allocated cells
       become invisible to future scanners. Pinned-cell start
       bits re-set in a post-pass keyed off `pinned_cells`.
   - **Pinned-source short-circuit**: `maybe_copy` returns
     `Some(w)` (unchanged) on a pinned source so the slot keeps
     its original address. Mirrors `heap.rs:1086`. Works for
     boxed objects as well — unlike semispace, page-heap pinning
     is uniform across cons and boxed (sub-phase 6).

   **Five gates in `maybe_copy`** (in order, fast-rejects first):
   1. Tag must be a heap-pointer tag (Cons/Symbol/Vector/
      Function/String).
   2. Target page must be in `from_gen`.
   3. **Start-bit gate** — target cell must be a real object
      start. Closes the false-positive-pointer-shaped-payload
      hole that the design doc previously waved away by
      reference to "harmless rejection by tag check"; with the
      explicit gate, a stray bit pattern that tags as Cons but
      points at an interior cell is now refused at the GC layer
      rather than relying on probabilistic luck.
   4. **Tag-vs-page-kind consistency** — Cons-tagged Word must
      target a Cons-kind page; Symbol/Vector/Function/String-
      tagged Words must target Boxed pages. Prevents tag-confused
      Words from landing in root slots after an
      almost-correct false positive.
   5. Forwarding-pointer / pinned checks, as in semispace.

   **17 new unit tests** (~600 lines of tests):
   `rooted_cons_promotes_to_dest_gen` (acceptance),
   `unrooted_objects_get_reclaimed`, `chain_head_evacuates_
   every_link`, `cycle_in_object_graph_terminates`,
   `pinned_object_stays_and_page_flips`, `cdr_pointer_gets_
   fixed_up_after_evacuation`, `already_forwarded_slot_is_re_
   resolved`, `boxed_object_with_word_payload_evacuates_
   correctly`, `mixed_pinned_and_unpinned_on_same_page`,
   `within_gen_evacuation_is_supported`, `pins_and_mark_bits_
   are_cleared_after_cycle`, `released_page_can_be_re_acquired`,
   `flipped_page_has_pinned_start_bits_preserved`,
   `false_positive_payload_word_is_rejected` (regression test
   for gate 3), `pointer_tag_must_match_page_kind` (regression
   test for gate 4), `pinned_boxed_object_stays_in_place`
   (uniform-pin coverage), `empty_from_gen_is_a_noop`.

   Total page_heap tests: **72 passing** (55 + 17 new). Full
   ncl-runtime suite: **294 passing** (was 277 before sub-phase
   7). Hellowin still opens. Production path is still the
   semispace heap; the page heap remains reachable only via its
   own tests until sub-phase 11.

   **Deferred to sub-phase 10**: mid-evacuation OOM panics
   (`expect("page heap exhausted mid-evacuation")`) when
   `from_gen == dest_gen` and free-page headroom is short. The
   page-reclaim happens after the BFS drain, so a within-gen
   evacuation can transiently need more pages than the heap
   has free even when the post-reclaim state would fit.
   Sub-phase 10's trigger budget will pre-allocate.

8. **Three-generation policy + age threshold** *(LANDED)*
   New file `src/ncl-runtime/src/page_heap/cycle.rs` (~570 lines
   including 10 tests). Adds `collect_minor` and `collect_major`
   drivers that wrap sub-phase 7's `evacuate_with_roots` with
   per-heap promotion counters.

   **Promotion model** — **cohort promotion**, not per-page age:
   - `PageHeap::minors_since_g0_promote`: minor cycles since the
     last G0 → G1 promotion. Bumped at the start of each minor.
     When it reaches `G0_PROMOTION_THRESHOLD = 3`, that cycle's
     destination is G1 (rather than G0); the counter resets.
   - `PageHeap::g0_promotes_since_g1_promote`: G0-promotion
     events since the last G1 → Tenured promotion. Ticks only on
     cycles that already promoted G0. At `G1_PROMOTION_THRESHOLD
     = 5`, the minor cascades into a second G1 → Tenured
     evacuation pass.

   This is "every G0 object alive at the threshold cycle gets
   promoted, regardless of its individual age." Younger objects
   allocated just before a threshold cycle therefore promote
   sooner than the design ideal — a known shortcoming of cohort
   promotion vs SBCL's per-page-age dispatch. Refinement deferred
   to a future sub-phase; the `PageDesc::age` field (from sub-
   phase 3) is reserved for that work, currently unused.

   **API change in sub-phase 7**: `evacuate_with_roots` now takes
   `F: FnMut` (was `F: FnOnce`) so cascading evac passes can
   replay the same root closure. The closure isn't moved into the
   first evac — `|e| visit_roots(e)` wraps it each call so each
   pass has its own re-borrow.

   **`collect_major`** (the manual-hammer variant): runs `G1 →
   Tenured` then `G0 → G0` back to back, resetting both counters.
   Tenured itself is not recollected — that's sub-phase 10's
   trigger problem. Reports `promoted_g1` based on actual G1
   work done, not unconditionally.

   **OOM during evacuation** (inherited from sub-phase 7): a
   within-gen pass on a heap with no Free pages panics with
   `"page heap exhausted mid-evacuation"`. Documented in
   `collect_minor`'s rustdoc; trigger policy (sub-phase 10) will
   pre-allocate to avoid this.

   **10 new unit tests**: `fresh_heap_counters_are_zero`,
   `nursery_transients_die_in_g0_within_threshold_cycles`
   (acceptance), `rooted_survivor_promotes_after_threshold_
   cycles`, `promotion_resets_counter_and_starts_new_cohort`,
   `g1_promotes_to_tenured_after_threshold_g0_promotions`,
   `major_promotes_g1_and_collects_g0`, `major_on_empty_heap_is_
   a_noop`, `cascade_reports_nonzero_objects_copied_when_g1_has_
   data`, `unrooted_g1_cons_reclaimed_on_cascade`, `many_cycles_
   dont_overflow_counter`.

   Total page_heap tests: **82 passing** (72 + 10 new). Full
   ncl-runtime suite: **304 passing** (was 294 before sub-phase
   8). The semispace heap remains the production path.

9. **Soft card marks + IR write barrier** *(1-2 days)*
   Single 1 GB / 512 = 2 MB card table covering the dynamic space.
   IR-emitted barrier at every heap-pointer store. Minor GC scans
   only dirty cards of older generations. **Note**: NCL's IR
   lowering lives in `ncl-llvm/src/lib.rs` and `ncl-compiler/src/
   lower.rs` — the agent's reference to `newbcpl-ir/src/lower.rs`
   was a confusion; corrected. **Acceptance**: minor-old-to-young
   pointer test ports cleanly.

10. **Trigger policy + auto-full-GC** *(1 day)*
    `bytes_allocated > auto_gc_trigger` per cycle; budget =
    `max(8 MB, 0.5 * tenured_bytes)`. Auto-full when Tenured > 0.75
    × cap. *Auto-full is now safe* because the page-based
    `collect_full` shares the conservative pin pass. **Acceptance**:
    100 MB churn workload shows minor cycles fire on budget
    intervals; tenured-saturation triggers full GC.

11. **Production switchover** — split into 11a / 11b / 11c.

    **11a — Cargo features + PageHeap inherent API** *(LANDED)*
    Replaced `HeapBackend` trait + `Mutex<Box<dyn HeapBackend>>`
    with build-time selection via Cargo features. New file
    `src/ncl-runtime/src/gc.rs` re-exports the chosen concrete heap
    type (`gc::Heap`) under `#[cfg(feature = "gc-semispace")]` or
    `#[cfg(feature = "gc-page-heap")]`; the two features are
    mutually exclusive (`compile_error!` if both on). Deleted
    `heap_backend.rs` and the trait + 6 `#[deprecated]` methods.

    New file `src/ncl-runtime/src/page_heap/coordinator_api.rs`
    (~330 lines) implements every inherent method `GcCoordinator`
    / `MutatorState` calls on the heap:
    - `PageHeap::new(young_bytes, old_bytes)` matches `Heap::new`'s
      signature; sums args into a single reservation (min 1 page).
      The pre-existing 1-arg constructor is renamed to
      `with_reservation` and stays as the test helper.
    - Aggregate stats `used_bytes` / `young_used_bytes` /
      `old_used_bytes` walk `descs` and filter by generation.
    - `young_base_ptr` returns the reservation base (the page-heap's
      start-bit bitmap is indexed against the whole reservation,
      so the mutator's cached `young_base` works for cell-index
      math).
    - `young_starts_handle` delegates to `start_bits_handle`.
    - `young_try_alloc_slab(cells)` is new infrastructure: it
      reserves a `cells`-sized chunk on a G0 cons page WITHOUT
      setting any start bits (the mutator sets them per-allocation
      via the cached handle). Capped at one page; oversize requests
      get a page's worth.
    - `old_cards` returns a new `Arc<CardTable>` field on PageHeap,
      sized for the whole reservation. `old_live_base_ptr` returns
      the reservation base, and `old_capacity_bytes_per_semi`
      returns `reserved_bytes()` — the coordinator's barrier range
      check then covers every page (which is the desired semantics
      under page-heap, where mutations into G1/Tenured pages are
      what need to be tracked).
    - `collect_minor_with_static` and `last_pin_summary` are
      stubs: `collect_minor_with_static` panics with `unimplemented!`
      pointing at sub-phase 11b; `last_pin_summary` returns a
      stored `(0, 0)` tuple ready for 11b to populate.

    `:heap-backend` plist value now comes from
    `gc::ACTIVE_BACKEND_NAME`, a `&'static str` resolved at compile
    time (semispace / page-heap). `HeapBackendKind` enum + env-var
    resolver are gone.

    **8 new unit tests** in `coordinator_api.rs`:
    `ctor_sums_young_and_old_bytes_for_reservation`,
    `ctor_rounds_up_to_one_page_minimum`,
    `used_bytes_aggregates_by_generation`,
    `slab_alloc_returns_aligned_pointer_and_advances_words_used`,
    `slab_alloc_fits_multiple_slabs_per_page`,
    `slab_alloc_caps_at_page_size`,
    `slab_alloc_zero_cells_returns_none`,
    `old_cards_and_base_match_reservation`.

    Build matrix:

    | Command | Result |
    |---|---|
    | `cargo build` (default) | clean |
    | `cargo build --no-default-features --features gc-page-heap` | clean |
    | `cargo test -p ncl-runtime --lib` (default) | 312/312 |
    | `cargo test -p ncl-runtime --no-default-features --features gc-page-heap --lib` | 302/312 (10 failures: 7 hit the documented `collect_minor_with_static` `unimplemented!` stub, 3 hit semispace-specific assertions in test code that don't hold for the page-heap's whole-reservation card table) |

    **11b — Real `collect_minor_with_static` on PageHeap** *(LANDED)*

    New file `src/ncl-runtime/src/page_heap/scanner.rs` (~40 lines)
    defines a page-heap `RootScanner` analogue: a thin 2-lifetime
    wrapper over `PageEvacuator` exposing the same
    `visit(&mut Word)` surface as the semispace `RootScanner`.
    `gc::RootScanner` re-exports the right one per build.

    Refactored
    `coordinator_api::collect_minor_with_static` to drive a real
    cycle:
    1. Run `pin_pointers_in_ranges(G0, stack_ranges)`. Stash
       result into `last_pin_summary`.
    2. Snapshot the per-page descriptor table BEFORE evacuation
       runs (page generations get mutated during evac; the dirty-
       card scan needs the pre-evac state to filter G0/Free
       pages out).
    3. Call `cycle::collect_minor` with a closure that:
       - Visits caller's roots via the page-heap `RootScanner`.
       - Scans static-area dirty cards.
       - Scans reservation dirty cards, filtered to G1/Tenured
         pages via the snapshotted descriptors. G0/Free pages
         are skipped (intra-G0 refs come through BFS drain;
         Free pages have no live data).
    4. Clear both card tables.

    New helper `scan_dirty_cards_as_roots` (free function in
    `coordinator_api.rs`): for each dirty card, iterates its 64
    cells and offers each to the evacuator via `visit_cell`
    (newly `pub unsafe fn`). `page_filter: Option<&[PageDesc]>`
    distinguishes the static-area scan (no filter — every dirty
    card matters) from the reservation scan (G1/Tenured filter).

    **Configuration adjustment**: `PageHeap::new(young, old)`
    minimum reservation bumped to 4 pages (256 KB). Within-gen
    evacuation (G0 → G0 on a non-threshold minor) needs at least
    one Free page to copy survivors into; the 1-page minimum from
    sub-phase 11a caused mid-evac OOM panics on the smaller test
    configs.

    **Per-cycle GC timing instrumented** (works under both
    backends — semispace + page-heap):
    - `GcStats` gained 6 new fields:
      `last_minor_pause_us` / `max_minor_pause_us` /
      `total_minor_pause_us` and the same triple for full GCs
      (full timing wires up when sub-phase 10's auto-full path
      lands; the fields exist now for consistency).
    - `MutatorState::do_minor_gc` wraps the heap-lock+collect
      block in `Instant::now()..elapsed()`, publishes the
      microsecond pause via atomic store / fetch_add / CAS-up.
    - `(gc-stats)` plist gained 3 new keys:
      `:LAST-MINOR-PAUSE-US`, `:MAX-MINOR-PAUSE-US`,
      `:TOTAL-MINOR-PAUSE-US`. Both backends report.

    **Test fixes for backend-agnostic assertions**:
    - `rooted_cons_survives_minor_gc`: replaced
      `coord.old_used_bytes() >= 16` with `coord.used_bytes() >= 16`.
      Under semispace the survivor moves to old; under page-heap
      it stays in G0 until threshold cycles fire. Both backends
      preserve the cons; the generation-agnostic check is the
      correct invariant to test.
    - `mark_card_outside_old_is_noop`: the second half (testing
      that a young-heap address marks no card) is now gated
      `#[cfg(feature = "gc-semispace")]`. Under page-heap the
      card table covers the whole reservation by design; the
      dirty-card scanner filters by source page generation, so
      the spurious G0-card mark is no-op work but not incorrect.

    Build matrix:

    | Command | Result |
    |---|---|
    | `cargo build` (default = gc-semispace) | clean |
    | `cargo build --no-default-features --features gc-page-heap` | clean |
    | `cargo build --workspace` | clean |
    | `cargo test -p ncl-runtime --lib` (gc-semispace) | **312/312** |
    | `cargo test -p ncl-runtime --no-default-features --features gc-page-heap --lib` | **312/312** — **parity** |

    Sub-phase 11b delivers a functional page-heap under
    `--features gc-page-heap`: end-to-end minor GC cycles run,
    rooted survivors are preserved, transient garbage is reclaimed,
    cross-gen pointers via card barrier are honoured, and per-cycle
    pause timings are exposed for performance work.

    **11c — Workspace-level feature propagation + completeness** *(LANDED)*

    Card-mark dispatch was already correct after 11a (`GcCoordinator`
    populates its lock-free `cards`/`live_base`/`old_capacity`
    cache from the active backend's inherent `old_*` methods —
    page-heap returns reservation-wide values; semispace returns
    its old-semispace values). So the original 11c scope reduced
    to "make workspace-level feature switching work."

    The conversion to build-time features (11a) exposed a Cargo
    feature-propagation issue: workspace members depending on
    `ncl-runtime` were activating its default `gc-semispace`
    feature even when the consumer asked for `gc-page-heap`,
    causing `compile_error!` (both features active). Sub-phase
    11c sweeps each of the 8 dependent crates to:
    - Add `[features] default = ["gc-semispace"]` plus
      `gc-semispace = [...]` and `gc-page-heap = [...]` feature
      lists forwarding to every transitive `ncl-*` dependency.
    - Set `default-features = false` on every `ncl-*` path
      dependency so transitive defaults don't sneak the
      semispace feature back in.

    Crates updated: `ncl-cl`, `ncl-compiler`, `ncl-driver`,
    `ncl-llvm`, `ncl-loader`, `ncl-reader`, `ncl-tests`,
    `ncl-corman-demos`.

    **Switching the backend** is now:
    ```bash
    cargo build                                                    # gc-semispace (default)
    cargo build -p ncl-driver --no-default-features --features gc-page-heap
    cargo test  -p ncl-tests --no-default-features --features gc-page-heap
    ```

    Note: top-level `cargo build --workspace --no-default-features
    --features gc-page-heap` doesn't reliably propagate because
    Cargo's `--no-default-features` for workspace builds doesn't
    fully suppress workspace-member defaults. The per-package
    `-p` form is the supported workflow until Cargo's workspace
    feature unification matures further.

    Build matrix:

    | Command | Result |
    |---|---|
    | `cargo build --workspace` (defaults = gc-semispace) | clean |
    | `cargo build -p ncl-driver --no-default-features --features gc-page-heap` | clean |
    | `cargo build -p ncl-tests --no-default-features --features gc-page-heap` | clean |
    | `cargo test  -p ncl-runtime --lib` (default) | 312/312 |
    | `cargo test  -p ncl-runtime --no-default-features --features gc-page-heap --lib` | 312/312 |

    Sub-phase 11 is now complete. The page-heap is fully reachable
    from every workspace crate and exercisable end-to-end under
    `--features gc-page-heap`. Subsequent perf work (an optional
    write-time gen filter for `mark_card`, IR-emitted inline
    barriers in sub-phase 9) is layered on top of this foundation
    and doesn't block production-readiness of the page-heap.
    Refactor `GcCoordinator::mark_card` so under
    `gc-page-heap` the cached `live_base`/`old_capacity`/`cards`
    point at the page-heap's reservation-wide table. (Current
    foundation: these fields are already populated by
    `gc::Heap`'s inherent methods via `old_cards` etc. — sub-phase
    11a got this for free.) The integration tests under
    `gc-page-heap` that depend on real cycles will start passing
    as 11b lands.

    **12. Delete semispace code** *(pending, unchanged)*
    Same as before: remove `Semispace`, `OldGen`, etc. once page
    heap has soaked. With the build-time selection, "delete" is
    just removing the `gc-semispace` feature flag and the cfg
    arms in `gc.rs`, then deleting `heap.rs`.

12. **Delete semispace code** *(0.5 day)*
    Remove `Semispace`, `OldGen`, `Heap`, `MinorState`, `FullState`,
    `RootScanner`, `copy_into`, `scan_to_completion`. Keep
    `HeapHeader`, `HeapType`, `GcBit`, `CardTable` (page heap uses
    them). `heap.rs` shrinks from ~1700 to ~250 lines. **Acceptance**:
    `cargo build --release` clean, full test suite green.

### Dependency graph

```
        1 (backend trait)
        ↓
        2 (reservation)
        ↓
        3 (PageDesc)
        ↓
        4 (alloc into pages)
        ↓
        5 (mark pass)
        ↓
        6 (conservative pin)
        ↓
        7 (evacuate / compact)
       ↙ ↘
      8   9     (gen policy, card barrier — parallel-able)
       ↘ ↙
        10 (trigger + auto-full)
        ↓
        11 (default switch + soak)
        ↓
        12 (delete semispace)
```

8 and 9 can land in either order once 7 is done; everything else is
strictly serial.

### Total estimate

| Sub-phase | Days |
|-----------|------|
| 1. Backend trait | 1 |
| 2. Reservation | 1 |
| 3. PageDesc | 1 |
| 4. Alloc | 2 |
| 5. Mark | 2 |
| 6. Pin | 2 |
| 7. Evacuate | 2 |
| 8. Generations | 1 |
| 9. Card barrier | 1–2 |
| 10. Trigger | 1 |
| 11. Switchover | 1 |
| 12. Delete | 0.5 |

**~15–16 working days** plus 1–2 weeks of side-by-side soak between
11 and 12.

### Deeply invasive points (called out for caution)

1. **Conservative pinner assumes a contiguous young byte range.**
   `Semispace::pin_pointers_in_range` (`heap.rs:432-492`) takes
   `(range_lo, range_hi)` and asks "is target in
   `[base, base+capacity)`?". With pages, "young" is the union of
   G0-tagged pages, scattered across the reservation. Pinner becomes
   "candidate → page-of-target → check `PageDesc.gen == G0`."
   Propagates to all callers of "young_contains."

2. **Start-bit bitmap is per-semispace.** `StartBits` (`heap.rs:250`)
   is shared as `Arc<[AtomicU64]>` covering the whole young heap,
   handed to mutators at registration. In the page model, start-bits
   become per-page. Mutator-side TLAB alloc currently does one
   atomic OR into a known bitmap; new path adds one indirection
   through PageDesc array. **Microbench this** — if it costs >5%
   alloc throughput, sub-phase 4 should keep bitmaps in a single
   contiguous array indexed by global cell offset.

3. **Forwarding pointers live where the header was.** Stays the
   same; `Word::is_forward()` check is already there. But: in the
   page model, after partial-page evacuation, source page has a mix
   of forwards and live-pinned objects. Needs explicit test
   coverage in sub-phase 7.

4. **`OldGen` two-semispace dance disappears.** Full GC in the page
   model marks across all generations then evacuates from-pages
   into new to-pages within the same generation. The full-vs-minor
   distinction collapses to "which generations are we touching."
   `live_base` field becomes dead.

5. **Card table is currently sized to `old_capacity_bytes`,
   anchored at `old_live_base_ptr`.** Page model needs one card
   table covering the whole 1 GB reservation, anchored at a fixed
   base. IR-emitted write barrier needs to learn the new fixed base
   address rather than reading `live_base` atomically.

6. **Test scaffolding at `heap.rs:1287-1681`** uses `Heap::new(1024,
   1024)` and inspects raw `young_used_bytes()` /
   `old_used_bytes()`. Page heap can't be sized that small (page =
   64 KB minimum). Tests rewritten in terms of object-count rather
   than byte-precise capacity, run against a 1-page heap.

---

## 6. What we deliberately don't do

- **Lifting CCL's precise scanner.** Requires "tagged-everywhere on
  the value stack" compiler invariant; NCL's LLVM JIT can't enforce
  this without major surgery.
- **Whole-cloth port of either GC's C code to Rust.** Both are too
  tightly coupled to their host runtime's tag layout, header
  conventions, and compiler emit. Borrow algorithms, write clean
  Rust.
- **Hardware-WP write barrier.** Windows VirtualProtect is too
  expensive; soft card marks emitted as IR barriers are the right
  call for our platform mix.
- **Concurrent GC.** Years of work for marginal benefit at NCL's
  current scale. Mentioned in Phase 6 for completeness; not on the
  roadmap.
- **Immobile space.** Useful for FFI but adds complexity. The
  static area covers most "must not move" cases already.
