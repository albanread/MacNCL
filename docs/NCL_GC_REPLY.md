# Reply: NewOpenDylan GC feedback

*Drafted 2026-05-17 in response to
[`NCL_GC_FEEDBACK.md`](file:///E:/NewOpenDylan/NCL_GC_FEEDBACK.md)
from the NewOpenDylan team.*

---

Thanks for the report. Both of your concrete recommendations land
cleanly in NCL today, plus answers to your two questions.

## What we did

**Commit [`756f4e6`](git):** `docs+test(gc): act on NewOpenDylan
feedback`.

1. **`rewind_past_pinned` SOUNDNESS doc-block.** Verbatim shape of
   the wording you suggested, expanded to spell out the failure
   mode (conservative roots can't go through a forwarding-aware
   loader). Lives at `src/ncl-runtime/src/heap.rs:383`. The
   apparent inefficiency now reads as load-bearing rather than
   "candidate for cleanup."

2. **`forward_round_trip` multi-alignment sweep.** Yours found a
   real bug; ours (`src/ncl-runtime/src/word.rs:330`) had the same
   single-pointer shape that would have masked it, even though our
   Tag::Forward encoding leaves 61 bits for the payload and is
   structurally lossless. Now sweeps across `0x..00`, `0x..08`,
   `0x..10`, … `0x..38` in one 64-byte window. Pointer at
   `NCL_GC_FEEDBACK.md` so the provenance is discoverable next
   time someone touches the encoding. Test passes on the existing
   encoding — confirms the bug isn't reachable here, but the
   coverage gap closes.

The class-driven scanning observation (your §3) lands as a
structural cost-of-cons-cell-privilege note. We've thought about
it before — there's a "what would we do if we dropped cons
privilege" sketch in the GC design backlog — but the conclusion
matches yours: it's structural. NCL pays it; you don't have to;
both designs are internally consistent. Worth knowing the cost
is explicit.

## Your two questions

### 1. Page-heap timeline

Honest answer: **page-heap is the de-facto development backend
already, even though semispace is still the default-feature
shape** per `Cargo.toml`. Our last several weeks of work — including
this session's GC-review pass and the corman ANSI port — have
been built and tested on the page-heap backend with
`--no-default-features --features gc-page-heap`. It survives
`pressure.lisp` (50000 ticks mixed type pressure), `conses.lisp`
(100k build-and-walk cycles), `closures.lisp` (1M closure
allocations) cleanly.

What's not yet done:

- The default feature flip in `Cargo.toml` (semispace stays
  default until page-heap has soaked at the REPL for a few weeks
  without regressions surfacing).
- Mode-switching mid-process (you build for one or the other; no
  runtime selection).
- Conservative-pin retirement (precise roots via stack maps
  remains the planned end state — there's a `stack_map.rs` with
  the data shapes, but the JIT-side `gc.statepoint` emission
  isn't wired yet).

For your "Dylan REPL + JIT'd kernel" use case, our judgment is
**semispace will suffice indefinitely** — it's the right shape
for interactive workloads with bounded heap residency. The
page-heap is aimed at long-running workloads where semispace's
full-GC-copies-all-of-old becomes the dominant pause. If your
working set fits comfortably in the old semispace, your full GCs
will be rare and cheap; the page-heap's chunked Phase 1 / Phase 2
machinery is overhead you don't need to pay for.

We don't promise either backend will retire the other. Two
backends behind a Cargo feature is a real choice we're keeping;
the manifesto's simplicity rule explicitly carves out the GC.

### 2. Things to know before porting the multi-threaded mutator

Three things we wish we'd known sooner:

**(a) The closure-environment precise-root bug.** When a JIT-emitted
`make-closure` call pushes capture values to a stack alloca and
then calls into `alloc_vector` (which can GC), the captures on the
alloca aren't covered by precise roots. After a GC inside the
alloc, the alloca's pointer values are stale; the resulting
closure env Vector ends up holding dangling pointers, and the
closure crashes on first dereference. This is the
`demos/life.lisp` gen-25 / `lambda_1317` crash class.

The fix is in `ncl_make_closure` (`src/ncl-runtime/src/abi.rs`):
push each capture into the mutator's precise-root list BEFORE
calling `alloc_vector`, then read them back from the root list
afterward. The GC's per-cycle root walk handles them correctly.
Same pattern applies to any C-ABI helper that takes pre-fetched
Word values across an allocation. Anywhere your generated Rust
code reads a Word into a local before calling something that can
allocate, that local needs precise-rooting.

If you're using stack-map-driven precise roots from the start
(your Sprint 11b plan), this is automatic — the statepoint
machinery covers the stack alloca. If you're using a manual
push_root / pop_root API like ours, every C-side helper that
touches Words across an alloc has to follow the protocol.

**(b) Page-heap closure env card-marking.** When a Function lives
in the static area (we put closures there to dodge precise-root
issues) and its env Vector lives in young, the inter-gen write
barrier needs to fire on the Function's env slot AT
make-closure time, not at first reference. Otherwise the first
minor GC misses the env Vector entirely. See the
`make_closure_marks_env_card` test pattern for the shape.

This isn't a TLAB or park-protocol issue — but it's the same
"write barrier interacts with cross-gen pointers introduced at
construction time" class of bug that's easy to miss until a
specific GC timing surfaces it.

**(c) The conservative-pin pass is one fix away from being the
soundness fault line.** Right now it pins the live set well
enough that we haven't had a non-precise-root crash in months.
But the pinner walks each mutator's `[parked_rsp, stack_hi)` and
treats every word that looks like a heap-pointer as a root. If
your TLAB refill ever yields a fresh page whose previous tenant
left Word-shaped garbage on it, and the conservative scan was
sampling that page's prior contents off a stale stack frame,
you'd pin garbage. We haven't seen this in practice (the GC
zeros pages on recycle); but the *protocol* depends on every
TLAB-recyclable page being zeroed before re-handing-out. If your
Dylan port re-implements the recycler, double-check the zero step.

**No ABA hazards we've seen in TLAB refill.** Refill takes the heap
mutex; the mutex serialises slab handout against GC initiation.
The TLAB itself is per-thread (no cross-thread access). The only
shared atomic is `stop_requested` (set by the GC trigger, polled
by all mutators at safe points); it's an unconditional store + a
single read, no CAS, no ABA.

The park-state Mutex<ParkState> has `handles: Vec<Arc<MutatorHandle>>` +
`parked_count: usize`. The trigger waits on a Condvar for
`parked_count == total - 1`. Nothing fancy.

If you find a race we missed, please send another report — that's
exactly the kind of feedback worth the round-trip.

## On the docs §0 callout

Thanks for the §0 macroexpand-all callout. That diagnosis was
genuinely "we don't understand why this is happening; let's write
down what we see until we do." We'll write the next one in the
same shape.

## Closing

Your bug-find on the lossy encoding made our test better even
though it caught nothing here. That's the kind of cross-port
feedback that's worth the round-trip. If/when you wire up the
multi-threaded mutator and find something we missed, the door's
open.

Everything else: please keep doing what you're doing too.

— NCL team, 2026-05-17
