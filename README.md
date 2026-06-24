# MacNCL

An Apple Silicon (macOS / `aarch64-apple-darwin`) port of
[NCL](https://github.com/albanread/NewCL) — a from-scratch, LLVM-JIT
Common Lisp modelled on Corman Lisp.

Two tracks:
- **Core** — compiler + GC + runtime + console REPL on LLVM 22 for Apple Silicon.
- **GUI** — re-target the `iGui` shell from Direct2D/Direct3D11/DirectWrite to
  **Cocoa + Metal + Core Text**.

See [docs/PORTING_DESIGN.md](docs/PORTING_DESIGN.md) for the full review and plan.

Status: **design / scoping.** No code ported yet.
