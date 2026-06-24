# GC bughunt: the tiny leak that wasn't a leak

*A retrospective on the random-data-loss bug in `newgc-core`'s minor
cascade, found through NCL workloads in May 2026, fixed in upstream
commit `c500539`. Written down because the wrong fix landed once,
the trail went cold, and the actual cause was non-obvious enough
that the methodology is worth preserving.*

## Symptom

Under sustained GC pressure, NCL workloads would lose a tiny,
randomized number of objects. The canonical reproducer was a list
walker:

```lisp
(defun build-list (n)
  (let ((acc nil)) (dotimes (i n) (setq acc (cons i acc))) acc))

(defun walk (lst)
  (let ((c 0)) (dolist (x lst) (declare (ignore x)) (setq c (+ c 1))) c))

(defun run-conses (iters n)
  (let ((sum 0))
    (dotimes (i iters) (setq sum (+ sum (walk (build-list n)))))
    sum))

(format t "conses sum=~A (expect 5000000)~%" (run-conses 100000 50))
```

Expected: `5000000` exactly. Observed: `4999949`, or `4999988`, or
`4999971`. Always close, never right, never the same number twice.
Running under `NCL_YOUNG_MB=8` to force ~33 minor GCs reproduced
reliably.

A list walked with `dolist` cannot return the wrong count unless
the chain itself is corrupt. A 50-element list silently turning
into a 47-element list means three nodes' `cdr` got rewritten —
or the nodes themselves got zeroed and a stale pointer in a
`pins[]` slot now references freed memory holding a non-NIL,
non-pointer value.

The bug was non-deterministic in both *which* iterations corrupted
and *how* (sometimes the count dropped by 1, sometimes by 3, once
by 51 in a single 100k-iter run). Pages were getting zeroed
underneath live pointers somewhere.

## First diagnosis — and why it was wrong

Reading `newgc-core`'s `apply_explicit_pins` revealed a gap: when
the durable explicit-pin set was empty, the function early-returned
without running the extension mark from the *conservative* pin set.
A conservatively-pinned cons's children would be unmarked, the
unmark gate would release their pages, and the cons's `cdr`/`car`
would dangle.

This was real, and the fix landed as `15b50c6`:

> "Fix conservatively-pinned objects' children dropped in the bare
> collect path"

The function was renamed `apply_pins_and_extend_mark` and the
extension mark moved outside the `is_empty` gate. NCL was re-pinned.
The workload was rerun.

`conses sum=4999949 (expect 5000000)`.

**Same deficit. Same magnitude. Same flavor.** The fix had landed,
the tests passed, and the symptom was unchanged.

This is the part of the story to remember. The first fix was *a*
fix — a real bug, a real conservative-pin path that needed the
extension mark — but it was not *the* fix. The lesson: when a code
review surfaces a plausible-looking bug, the workload disagreeing
is more important than the code disagreeing. A correct-looking
patch that doesn't move the symptom is a sign that the actual
cause is further down, not that the symptom is flaky.

## Building a reproducer in pure newgc

The first move was to drop down into pure `newgc-core` (no NCL, no
JIT, no Lisp stack) and write a Rust test that mimicked what NCL
was doing. NCL's mutator publishes a conservative scan window
covering its whole JIT stack; stale slots in that window are
*incidental* pins — the user never asked for them, they're just
old pointers that haven't been overwritten yet. The repro had to
reproduce that pattern.

`crates/newgc-core/tests/cons_elision_repro.rs::random_interior_pin_debug`:

```rust
let mut pins: [u64; MAXPIN] = [Word::NIL.raw(); MAXPIN];
m.set_stack_range(pins.as_ptr() as usize,
                  unsafe { pins.as_ptr().add(MAXPIN) } as usize);

for iter in 0..200_000usize {
    for p in pins.iter_mut() { *p = Word::NIL.raw(); }
    let mut head = Word::NIL.raw();
    let mut np = 1usize;
    let mut pinned: Vec<i64> = Vec::new();
    for i in 0..N {
        let p = loop {
            match m.try_alloc_cons_in(Generation::G0) {
                Some(p) => break p,
                None => {
                    m.collect_minor(&mut [], |_| {});
                    let (c, na, g) = check_chain(head, i);
                    assert!(c == i && na == -1, "BREAK ...");
                }
            }
        };
        // ... append node to head ...
        rng = next(rng);
        if np < MAXPIN && rng % 4 == 0 {
            pins[np] = head;  // randomly snapshot interior nodes
            np += 1;
            pinned.push(i);
        }
    }
}
```

The key trick: `pins[1..np]` accumulate snapshots of *interior*
chain nodes (chosen by xorshift), then those slots are never
cleared. The first time a `try_alloc_cons_in` fails and triggers a
minor GC, the snapshots are valid G0 addresses and act as
conservative pins. After that minor, those addresses now live on
G1 pages (FLIPped). The slots still hold the same raw bits — they
look like valid G1 pointers to the conservative scanner.

The test reproduced the bug deterministically: BREAK at iter
`156463`, before node `i=43`, with `count=1, null_at=1,
gap=(43,0)`. The chain head decoded as a cons whose `car` was
fixnum 0 — exactly what you get from a zeroed cell.

## Three assertions that didn't fire

Before going further, instrumentation:

1. **Release-of-pinned assert** (in `phase3_reclaim`'s RELEASE
   branch): if a page about to be released has a cell in
   `pinned_with_kind`, panic. Did not fire.

2. **Survival assert** (in `evacuate_with_roots` before
   `clear_all_pins`): for every cell in `pinned_cells`, the start
   bit must still be set. Did not fire.

3. **Skip logs in `pin_range_one`** (env-gated on
   `NEWGC_DBG_ADDR`): log every gate that rejects a candidate
   targeting the dbg address. No skip ever fired for the
   broken-out cell.

The contradiction:
- The cell *was* pinned in the conservative scan (no skip fired).
- The cell *was* in `pinned_cells` at the end of evac with its
  start bit intact (survival assert didn't fire).
- The cell's content was nonetheless zeroed.

This is impossible through the standard FLIP path. FLIP preserves
the cell's bytes by collecting `pinned_ranges` *before* zeroing,
then `zero_page_outside_ranges` skips them. If the cell was
in `pinned_cells` and pinned in scan, FLIP should have preserved
it. The mental model had a gap somewhere.

## DBG_PRE + DBG_REPORT — the smoking gun

The next instrumentation added two env-gated reports in
`evacuate_with_roots`: one immediately after
`apply_pins_and_extend_mark` (DBG_PRE), one at the end of the
function (DBG_REPORT). Each logs, for `NEWGC_DBG_ADDR`'s cell:
- whether it's in `pinned_cells`
- its start bit
- the descriptor generation of its page
- whether the page has any pin bits set
- the first two cell contents

The test was rerun with `pins[0]`'s address piped to
`NEWGC_DBG_ADDR` before each collect. The output ending with the
BREAK:

```
DBG_REPORT 0x...132 cell=1089534 from=G0 dest=G1 pagegen=G1 in_pinned=true  start_bit=true  has_pins=true  c0=0x0150
DBG_PRE    0x...132 cell=1089534 from=G1 dest=Tenured pagegen=G1 in_pinned=false start_bit=true  has_pins=false c0=0x0150 pinset_size=0
DBG_REPORT 0x...132 cell=1089534 from=G1 dest=Tenured pagegen=Free start_bit=false has_pins=false c0=0x0
```

Three log lines, one cell, one cycle. Read top to bottom:

1. End of the G0→G1 sub-evac: cell is on a G1 page, pinned, with
   `c0=0x0150` (a real cons header).
2. Start of the G1→Tenured cascade sub-evac (same outer
   `collect_minor` call): `in_pinned=false`, `pinset_size=0`. The
   pin set has been wiped.
3. End of the cascade: the page is `Free` and the cells are zero.
   The cell was reclaimed because nothing rooted it.

The pin set was emptied between the two sub-evacs. That was the
bug.

## Root cause

`Mutator::drive_collect` (the multi-mutator entry point used by
NCL's `collect_minor`) does the conservative pin scan *once* per
logical cycle, and it explicitly seeds pins for **both** G0 and G1:

```rust
self.drive_collect(
    roots,
    |heap, visit| heap.collect_minor(visit),
    &[Generation::G0, Generation::G1],  // pin_gens
    extra,
)
```

The reason for the two-gen seed is exactly the bug's victim: a
`collect_minor` may CASCADE — every `G0_PROMOTION_THRESHOLD ×
G1_PROMOTION_THRESHOLD` (= 15) minors, the G0→G1 evac is followed
by a G1→Tenured evac in the same logical cycle. Mutator
stack-resident pointers to G1 must be pinned through the cascade,
or the cascade's pass treats them as garbage.

But `evacuate_with_roots`, until the fix, called
`self.clear_all_pins()` at the end of *every* pass:

```rust
// Step 4: end-of-cycle cleanup.
self.clear_all_pins();              // <-- wipes G1 pins
self.clear_mark_bits_in_gen(from_gen);
self.clear_recycle_live_counts();
```

The G0→G1 pass wiped both its own G0 pins (correct) and the G1
pins seeded by the same pre-cycle scan (wrong). The cascade then
ran `apply_pins_and_extend_mark`, found an empty pin set, and
`phase3_reclaim` released every unmarked G1 page — including
pages that held the chain interior. The mutator's `pins[1..np]`
slots and any stack-resident G1 pointer dangled.

The bug had been latent since multi-mutator was wired in: the
single-gen `drive_collect` path (with `pin_gens=[G0]`) never hit
it because there was no G1 pin to clear. Adding G1 to `pin_gens`
to fix an *earlier* `demos/life.lisp` crash (cascade-invalidates-
stack-resident-G1, fb72193) was correct, but it relied on the
pins surviving the cascade, which they didn't.

The fix is in `c500539`:

> Move `clear_all_pins` from per-evac to per-logical-cycle. The
> per-evac cleanup keeps mark bits and recycle counts; the
> per-cycle pin set survives every sub-evac it was meant to
> protect.

In code: drop the line from `evacuate_with_roots`, add it at the
tail of `collect_minor` / `collect_major` / `collect_full` in
`cycle.rs`, after all sub-evacs complete.

A bonus from the move: pre-pinning a Tenured object via
`pin_pointers_in_ranges(Tenured, ...)` before `collect_full` now
survives all three of full's sub-evacs. The previous behavior was
documented as a gap in
`vm1_collect_full_does_not_preserve_pre_pinned_tenured`; that
test inverted to `vm1_collect_full_preserves_pre_pinned_tenured`.

## Lessons

**1. "Fix landed, symptom unchanged" is a hard signal.**
The first fix (`15b50c6`) was for a real bug in
`apply_explicit_pins`. It passed code review, passed every newgc
test including new regression coverage, and shipped. NCL's
deficit did not move by a single conscell. The temptation at that
point is to declare the deficit a separate flake — *the GC fix
worked, the remaining loss is something else*. It is almost never
something else. A symptom that survives a confident fix is the
loudest signal you'll get that the cause is one layer deeper.

**2. Reproduce in the tightest possible environment.**
NCL hits the bug through ~50 layers of JIT codegen, foreign code,
multi-threaded coordinator state, and an opaque stack scanner.
Every one of those layers is a candidate for the bug; every one
is a candidate for the *appearance* of a bug. Building
`random_interior_pin_debug` — pure Rust, single mutator, no JIT,
no FFI — moved the question from "where in this stack is it" to
"can I make a bare PageHeap drop a pin." Once that test
reproduced, the bug was in newgc-core; that was the *only* thing
the test exercised.

**3. Mimic the failure mode, not the workload.**
NCL didn't fail because its stack happened to contain a list head.
It failed because its conservative scan covered slots the user
never asked to pin, and those slots had become stale. The repro
deliberately leaks `pins[1..np]` snapshots forward — not "what
real code does," but "what real code accidentally does." Most
conservative-pin bugs hide behind *accidental* roots, not
deliberate ones. The repro should mimic the accident.

**4. Run the contradiction to ground.**
Three assertions did not fire. The hypothesis after that round
was "all my checkpoints pass, somehow the cell ends up zeroed."
The wrong move is to start guessing — "must be a race," "must be
zero-page-outside-ranges has an off-by-one," "must be the page
descriptor cache." The right move is to refine the
instrumentation until one of the propositions is provably false.
DBG_PRE caught it: `in_pinned=true` at the end of one evac,
`in_pinned=false` at the start of the next, no mutator code in
between, same outer call. *That* is a smoking gun.

**5. Env-gated targeted logging beats verbose logging.**
The pin scanner runs thousands of candidate Words per cycle. A
plain `eprintln!` would have drowned every signal in noise. The
pattern used here:

```rust
let dbg = std::env::var("NEWGC_DBG_ADDR")
    .ok()
    .and_then(|s| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
    .unwrap_or(0);
let dbg_on = |a: usize| dbg != 0 && a == dbg && target == Generation::G0;
```

…lets the *test* set the address right before the suspect call
(`std::env::set_var` is fine in single-threaded Rust tests), and
production code is unaffected when the var is unset. The logs are
worth keeping during a hunt and worth removing on the merge —
they're a debugging idiom, not a feature.

**6. Cycle boundaries are about lifetime, not about location.**
The original design comment on `clear_all_pins` was:

> Clear every pin bit AND empty the pinned-cells set. Called at
> the start of each GC cycle so stale pins from earlier cycles
> don't carry forward.

True, but ambiguous on what "each GC cycle" means. The function
ran at the boundary of each *evac pass*, which lined up with "a
GC cycle" in the single-pass case and broke in the
cascade-as-one-cycle case. The fix didn't change what the call
does — it changed *whose responsibility* the call is. Cleanup
calls in long-lived state should match the lifetime of the
producer (here, the conservative pin scan, which runs once per
logical cycle), not the lifetime of the consumer (here,
`evacuate_with_roots`, which runs once per sub-pass).

## Verification trail

- `random_interior_pin_debug` (the smoking-gun reproducer): 200k
  iterations, no break.
- `pinned_partial_cons_chain_keeps_integrity_under_churn` (head-
  only conservative pin under churn): 100k iterations, no
  corruption.
- newgc-core test suite: ~300 tests across 24 files, all pass,
  including the long stochastic workload (~54s).
- `vm1_collect_full_preserves_pre_pinned_tenured` (inverted from
  the old gap-documenting test): passes.
- NCL `demos/_verify.lisp` under `NCL_YOUNG_MB=8` (forces 33
  minor GCs, ~93 MB promoted): `conses sum=5000000`. Was
  `4999949` before `c500539`.
- NCL workspace: 302 pass; the 2 failing tests
  (`end_to_end_tests::ffloor_mv_baseline`,
  `end_to_end_tests::ffloor_returns_float_quotient`) are
  multiple-value-return semantics bugs that pre-date this work
  and persist unchanged.

## Commits

- `15b50c6` — `apply_explicit_pins` → `apply_pins_and_extend_mark`,
  extension mark now runs whenever any cell is pinned (real fix
  for a real bug, but not for *this* bug).
- `c500539` — Move `clear_all_pins` from per-evac to
  per-logical-cycle. The actual fix for the cons deficit.
- NCL `8669008` — Pin `newgc-core` to `c500539`.
