# The SEH wind-up: panic-through-JIT on Windows MCJIT

*A debugging story, 2026-05-11.*

This is a write-up of how we got Rust panics to unwind cleanly
through JIT'd Lisp frames on Windows x86_64. It's the kind of
problem where the first symptom and the actual cause are three
layers apart, and any single fix in isolation looks like progress
but doesn't move the needle. The goal here is to document the
chain of misdirections, because every one of them was a thing
someone reasonable could have stopped at and concluded "this is
just how it works."

## Why we cared

NCL needs to raise conditions from runtime helpers (the `error`
shim, the THREADS package's `exit-thread`, etc.) and have them
propagate back to a `catch_unwind` boundary in the host —
`Session::eval` on the main thread, the spawn thunk's catch on
a worker. Without proper unwind, the only mechanism is what we'd
been using: a per-thread `ABORT_PENDING` flag that every
JIT-emitted call site has to check after every call. That's an
ABI tax on every function call in the language. Concretely:

  call ncl_foo
  cmp byte ptr [abort_pending_tls], 0
  jne abort_propagate
  ; ...continue with normal use of the result

Two extra instructions per call. Worth it if you can't unwind;
not worth it once you can.

Beyond the cost, the flag approach has correctness gotchas: code
between the signal point and the next call site still runs.
`(when (some-condition) (error "x") (do-side-effect))` will
execute `do-side-effect` if the `error` doesn't unwind, because
the JIT doesn't know the call signalled anything until it returns
and checks the flag. The Lisp programmer expects the unwind.

So: get real SEH unwinding working through JIT frames, and the
whole condition system becomes both faster and more correct.

## The setup

- `ncl-runtime` exposes Rust functions called from JIT'd Lisp.
- `ncl-llvm` builds a fresh `Context`/`Module`/`ExecutionEngine`
  per JIT'd Lisp function via inkwell 0.9 + LLVM 22.1. Every Lisp
  function carries the `uwtable` attribute so LLVM emits
  Windows-style `.pdata` and `.xdata` sections.
- The driver invokes JIT'd code via `transmute`-cast function
  pointers, then catches panics at the appropriate boundary.

Goal: `panic_any` in a runtime helper → unwinds back through any
number of JIT frames → caught at `catch_unwind`.

First test:

    (defun simple-worker ()
      (format t "[worker] before exit-thread~%")
      (exit-thread)
      (format t "[worker] AFTER exit-thread (should not print)~%"))

`(exit-thread)` calls a Rust shim that does `panic_any(...)`.
Result:

    thread 'ncl-thread-2' panicked at src\ncl-runtime\src\threads.rs: Box<dyn Any>
    exit=-1073740791    # 0xC0000409 — STATUS_STACK_BUFFER_OVERRUN

The panic fires (we see Rust's default hook output). Then the
process dies with `STATUS_STACK_BUFFER_OVERRUN`. The classic
"the unwind is corrupting the stack" symptom.

## Misdirection #1: "LLVM isn't emitting unwind tables"

The first instinct on "JIT unwind doesn't work on Windows" is
that LLVM didn't emit unwind info. Long-standing folklore says
MCJIT on Windows is incomplete for SEH.

Diagnosis:

    pub fn build_lisp_function(...) -> Result<usize, String> {
        // ...
        if std::env::var_os("NCL_DUMP_IR").is_some() {
            let ll_path = format!("ncl-dump.{idx:03}.ll");
            module.print_to_file(&ll_path)?;
            // Also emit to .obj via TargetMachine::write_to_file
            // so we can inspect with dumpbin.
        }
        // ...
    }

`NCL_DUMP_IR=1 ncl.exe --load tiny.lisp`, then:

    > dumpbin /headers ncl-dump.000.obj | grep -E "\.text|\.pdata|\.xdata"
       C .pdata
      48 .text
       8 .xdata

`.pdata` and `.xdata` are present. 12 bytes of `.pdata` is one
`RUNTIME_FUNCTION` entry. Open `.xdata` and decode:

    01 04 01 00 04 62 00 00

      01 = Version=1, Flags=0 (no handler)
      04 = SizeOfProlog
      01 = CountOfCodes
      00 = no frame register
      04 62 = UNWIND_CODE @offset 4: UWOP_ALLOC_SMALL OpInfo=6
            → allocation size = (6+1)*8 = 0x38 bytes

Cross-check against the `.text` bytes:

    48 83 EC 38     sub rsp, 0x38

The unwind data describes the prolog exactly. LLVM is fine.
Misdirection #1 ruled out.

## Misdirection #2: "We're not registering the tables"

The OS unwinder doesn't scan memory for unwind info — it walks
registered tables. Statically-linked PE images get registered by
the loader. JIT'd code in `VirtualAlloc`-ed memory doesn't, until
you call `RtlAddFunctionTable(pdata_ptr, count, base_address)`.

inkwell 0.9 wraps `LLVMCreateExecutionEngineForModule`, which
doesn't accept a custom memory manager — so we can't see the
section addresses to register them. Drop to llvm-sys for engine
construction, write our own memory manager that captures
`.text`/`.pdata`/`.xdata` addresses by name as they're allocated,
register on finalize. Lots of unsafe Rust FFI, but conceptually
just "wire up an existing OS API."

First crash:

    LLVM ERROR: IMAGE_REL_AMD64_ADDR32NB relocation requires
    an ordered section layout

`IMAGE_REL_AMD64_ADDR32NB` is the COFF relocation type used in
`.pdata` to encode RVAs from `.pdata` entries back into `.text`
and `.xdata`. RVAs are u32, so all referenced sections must lie
within 4 GiB of each other. Our first cut allocated each section
with its own `VirtualAlloc` — Windows hands those back at
arbitrary 64 KiB-granular addresses across a huge virtual space.
`.pdata` and `.text` ended up further than u32 apart.

Fix: one contiguous 4 MiB reservation per module
(`MEM_RESERVE`, `PAGE_NOACCESS`), bump-allocate sections inside it
page-aligned (`MEM_COMMIT` on demand), VirtualProtect code pages
to `PAGE_EXECUTE_READ` on finalize. Now all sections in a module
are within a few pages of each other. Relocations resolve fine.

Re-run the test:

    [jit_mm] registered 4 SEH entries at base=0x...
    [worker] before exit-thread
    panicked: Box<dyn Any>
    exit=-1073740791

Same crash. Registration succeeded but didn't help. (And the
"depth-1000 recursion under handler-case works!" test result
that I'd cited earlier turned out to be misleading — that test
was using NCL's flag-based condition mechanism, not real SEH
unwind. The infrastructure was registered; the unwind wasn't
actually being exercised.)

## Misdirection #3: ".xdata is wrong / uwtable level is wrong / personality routine is missing / ..."

A long, unproductive afternoon of hypothesis-chasing. Each one
sounds plausible:

- Maybe `uwtable=1` (sync) isn't enough; try `uwtable=2` (async).
  No change.
- Maybe the personality routine slot is null and the unwinder
  treats it as "this frame eats the exception." Documentation
  says null personality means pass-through. No change.
- Maybe the function-call ABI through `extern "C"` Rust shims
  loses something at the seam. The signatures all *look* right.

In hindsight, every one of these was wrong because the actual
failure was further upstream. But the symptoms looked exactly
like an unwind-table mismatch. The runtime was correctly walking
the unwind tables; the panic was being aborted before it ever
got that far.

## The first real fix: zero-padded .pdata slots

`NCL_TRACE_SEH=1` printed one of the registration lines as:

    [jit_mm] registered 4 sorted SEH entries at base=...;
      first=[base, base+0x94)
      last=[base, base+0)

`last=[base, base)` — zero-length entry. Three of our four
"entries" were all-zero `RUNTIME_FUNCTION` structs.

What was happening: LLVM/RuntimeDyld over-allocated `.pdata` to
54 bytes for a section that actually populated only 12. The
trailing 42 bytes were zero-filled. `pdata.size / 12 = 4` —
naïve entry count includes the padding.

The unwinder's `RtlLookupFunctionEntry` does a binary search by
`BeginAddress`. All three zero entries share `BeginAddress=0`.
The search lands on one of them, sees `EndAddress=0`, concludes
"PC not in this function," and *skips the frame entirely* — which
means the prolog isn't reversed, RSP isn't restored. The next
function up the call chain reads its `/GS` canary from the wrong
stack offset, mismatches, calls `__fastfail`. That's the 0xC0000409.

Fix in `jit_mm.rs`:

    // In-place partition: live entries (Begin < End) to the front.
    let mut live = 0usize;
    for i in 0..raw_count {
        if entries[i].BeginAddress < entries[i].EndAddress {
            if i != live { entries[live] = entries[i]; }
            live += 1;
        }
    }
    entries[..live].sort_by_key(|e| e.BeginAddress);
    RtlAddFunctionTable(pdata.ptr, live as u32, base);

This had to be right, *and*:

## The second real fix: extern "C-unwind"

After the zero-slot fix, panic from a top-level form on the main
thread still surfaced as Rust exit code 101 — a Rust panic that
propagated cleanly out of the JIT, but past the (non-existent)
host `catch_unwind`. The unwind *was* working.

But the threaded test (`exit-thread` from a worker, spawn-thunk
catches) still crashed with 0xC0000409. Same error code, different
path.

The decisive clue was a comment from a colleague: 0xC0000409 is
*not* exclusively a stack-buffer-overrun. Microsoft repurposed
that NTSTATUS code for **all** `__fastfail` aborts. And starting
in Rust 1.71, when a panic tries to escape an `extern "C"`
boundary (no `-unwind` suffix), it's UB — rustc inserts a guard
that calls `core::intrinsics::abort()`, which on MSVC is
`__fastfail(...)`, which surfaces as 0xC0000409.

`grep -rn 'extern "C"' src/ncl-runtime/src` found 110 hits. Every
runtime helper that JIT'd code called — `ncl_alloc_cons`,
`ncl_call`, `ncl_funcall`, the bignum/complex/float/ratio math
helpers, `ncl_abort_pending`, all of them — was `extern "C"`, not
`extern "C-unwind"`. So:

- A panic raised in `exit_thread_shim` (Rust, `extern "C-unwind"`)
  begins propagating up through the call stack.
- It enters `ncl_call` (Rust, `extern "C"`).
- rustc's guard catches the escaping panic at the boundary and
  fires `__fastfail`. End of process.

The unwind never even reached our JIT frame. The SEH tables we'd
spent days getting right hadn't been wrong; they'd been
unreached.

Fix:

    pub extern "C" fn ncl_alloc_cons(...)   →   pub extern "C-unwind" fn ncl_alloc_cons(...)

…across every helper in `abi.rs`, `bignum.rs`, `complex.rs`,
`float.rs`, `ratio.rs`, and the `LispEntryFn` / `Fn1` typedefs in
`ncl-llvm`. A few dozen edits, all the same.

The threaded test:

    [worker] before exit-thread
    [threads] thread 2 called exit-thread
    main: joined
    DONE
    exit=0

A 100-deep JIT recursion calling `(%test-panic)` from depth 0:
unwind walks all 100 frames, spawn-thunk catches, main continues.
`(terminate-thread tid)` from another thread interrupts a
1.7-million-iteration tight loop in the target — clean unwind,
clean catch.

## The lessons

1. **`STATUS_STACK_BUFFER_OVERRUN` is the NTSTATUS for any
   `__fastfail`.** It is not exclusively about stack corruption.
   Anywhere Microsoft's runtime wants to abort hard — `/GS`
   failures, CFG violations, Rust's anti-UB guards — it calls
   `__fastfail` and you see 0xC0000409. Don't anchor on the name.

2. **`extern "C-unwind"` is mandatory on every Rust function in
   the panic propagation path.** Plain `extern "C"` is a
   silent-but-deadly trap in any codebase that mixes Rust panics
   with FFI. The compiler's not going to complain at build time.
   It complains at runtime, with a name that misdirects you.

3. **Pre-emit-side debugging beats post-mortem.** Once we dumped
   `.pdata` / `.xdata` to disk and decoded by hand, the Windows
   ABI side was straightforward: this byte means "alloc 0x38,"
   that one means "this is the prolog size." It was opaque only
   while we were guessing at it.

4. **MCJIT *can* unwind.** The folklore that "Windows JIT
   unwinding is broken" comes from real-world JITs that don't
   register their unwind tables. Once you wire up the memory
   manager + `RtlAddFunctionTable` + filter the zero-padded
   slots, it works. Today, every JIT'd Lisp function in NCL has
   its `.pdata` registered and the unwinder walks through it
   like any DLL-loaded function.

## Where this leaves us

- `error` / `handler-case` can drop the per-call-site flag
  check. Migration's separate; the SEH path is the prerequisite.
- `exit-thread` is preemptive, matching Corman Lisp's documented
  contract that it "never returns."
- `terminate-thread` is cooperative *only* in the sense that the
  target observes the request at its next `(thread-safepoint)`
  call — once observed, it panics out from inside its own
  context. No flag the user's loop needs to check; no
  early-return idiom required.
- Non-local exits (`return-from`, `throw`/`catch`,
  `unwind-protect`) can use the same mechanism.

The regression tests in `tests/seh-unwind/` exist to keep this
working. They aren't wired into `cargo test` yet — they're
single-file Lisp programs that run as `ncl.exe --load X.lisp` and
exit 0 on success. Get them into the harness before this drifts.

## Acknowledgements

The crucial bit — recognizing 0xC0000409 as a `__fastfail` and
pointing at the missing `-unwind` suffix on `extern "C"` — wasn't
mine. Without that diagnosis I'd have spent another day in
`.xdata` decoders. The infrastructure work (memory manager,
contiguous reservation, zero-slot filter) was the easier
half — the unblocking insight was the harder one.
