# Conservative pin scan — regression tests

Lisp programs that exercise the case the GC couldn't handle
before: multiple threads allocating on every iteration, peers
triggering minor GC mid-iteration. Without the conservative pin
scan, a JIT-resident pointer in another thread's locals would
become a stale `<forward:...>` after the peer's GC swept young.

| File | What it stresses |
|---|---|
| `alloc-test.lisp` | 8 threads × 2000 iters; each iter allocates a fresh `(cons i (+ i 1))` and reads both halves. Asserts `expected==got` to catch lost increments. |
| `stress.lisp` | 16 threads × 1500 iters; each iter allocates a cons, a 3-element list, and a `format` string, then uses all three. Print and pass-through. |

## What's being tested

When a peer thread triggers a minor GC while a Lisp worker is
mid-call, the worker parks at its next safepoint. Its stack
contains JIT-emitted local Words holding pointers to young
objects (cons-tagged words, vector-tagged words for strings/
lists, function-tagged for closures). Without precise stack
maps, the GC doesn't know which slots are pointers and which
are just integers that happen to look like them.

The conservative scan in `Heap::collect_minor_with_static`
walks every parked mutator's stack range `[parked_rsp, stack_hi)`
8 bytes at a time, decodes each slot as a `Word`, and if it
looks like a heap pointer into young, sets the `Pinned` bit on
the target's header. `copy_into` skips pinned objects — they
stay at their addresses. `rewind_past_pinned` preserves the
young cells they occupy.

Net effect: any object a stack slot might reference survives
in place. Slot pointers remain valid. False positives (a
fixnum that happens to look like a heap pointer) cost a few
extra bytes of survived garbage, no correctness issue.

## Caveats (known v1 properties)

- **Pinned objects stay in young.** They aren't promoted. If a
  conservative ref persists across many GC cycles (a worker
  thread that holds a long-lived pointer), the object stays in
  young forever. Old-gen promotion of long-lived pinned objects
  is a follow-up.
- **Fragmentation.** Free cells below a pinned object in young
  are unreclaimable until the pinned object is freed (its
  references all drop). For typical Lisp programs (lots of
  short-lived garbage above each pinned object), this is fine.
- **Cons cells can't be pinned through this path.** Cons cells
  are headerless (no GcBit::Pinned to set). The scan ignores
  them; they're treated as precise references. If a stack slot
  holds a stale cons-tagged pointer, the cons cell's
  forwarding pointer (written by the copier when some other
  precise root reached it) keeps it valid. Forwarding works
  because the from-space cons cell holds the forward word
  intact for the rest of this GC cycle.

`docs/the_seg_windup.md` covers the SEH unwind story. This
GC change is the other half of "Lisp threads can do real
work" — together they make THREADS package programs viable
on multi-core hardware.
