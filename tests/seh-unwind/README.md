# SEH unwind regression tests

Lisp programs that exercise the Windows x86_64 SEH-unwind path
through JIT'd Lisp frames. They run as standalone scripts under
`ncl.exe --load <file>` and exit 0 on success.

| File | What it exercises |
|---|---|
| `panic-test.lisp` | Single `panic_any` from a Rust shim called by a JIT'd top-level form. Exit 101 (Rust panic, no catch). |
| `exit-test.lisp` | `(exit-thread)` from inside a spawned worker; spawn thunk's `catch_unwind` catches; main joins; exit 0. |
| `deep-panic.lisp` | Recursive Lisp builds 200/500/1000 JIT frames, then signals via `(error …)`. (Uses the flag-based condition mechanism — included for completeness.) |
| `preempt-test.lisp` | Tight `(loop)` worker; main calls `(terminate-thread tid)`; worker panics from `(thread-safepoint)` at next poll. |
| `preempt-full.lisp` | Combined: preemptive terminate + worker `exit-thread` + deep recursion through a **real** `panic_any` (via `%test-panic`). All three caught by spawn thunk; main continues to `DONE`. |

These should be wired into the workspace `cargo test` harness as
shell-based integration tests. Until then they're a manual
sanity sweep: `for f in tests/seh-unwind/*.lisp; do
target/release/ncl.exe --load "$f"; done` should print clean
output and exit 0 for each (except `panic-test.lisp` which exits
101 by design — the panic propagates out the top).

## Why these exist

A Rust panic raised in a runtime helper must unwind through any
depth of JIT'd Lisp frames to reach the calling host's
`catch_unwind` boundary. Three things have to be right for this:

1. JIT'd functions carry `uwtable` so LLVM emits `.pdata`/`.xdata`.
2. The JIT memory manager registers each module's `.pdata` with
   `RtlAddFunctionTable` (filtering zero-padded slots).
3. Every Rust function on the panic path is `extern "C-unwind"` —
   `extern "C"` without the `-unwind` suffix turns into a
   `__fastfail` (`STATUS_STACK_BUFFER_OVERRUN`, 0xC0000409) when
   a panic escapes the boundary.

`docs/the_seg_windup.md` has the detailed write-up of the
diagnosis and fix.
