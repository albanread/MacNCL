# MacNCL — Porting NCL to Apple Silicon

*Design draft — 2026-06-24. Based on a review of `albanread/NewCL` @ HEAD.*

Port target: **`aarch64-apple-darwin`** (Apple Silicon), with `x86_64-apple-darwin`
falling out for free where the toolchain allows.

Two jobs, separable:

1. **Core port** — get the compiler + runtime + GC + console REPL building and
   running natively on Apple Silicon via LLVM 22.
2. **GUI port** — re-implement the `iGui` shim from Direct2D/Direct3D11/DirectWrite
   on **Cocoa + Metal + Core Text**, behind the existing event-mailbox boundary.

The good news from the review: the *architecture already anticipates this*. The
MANIFESTO commits to "Mac is the second target, not a hypothetical," the OS surface
is supposed to live behind a thin shim, and `ncl-runtime` / `ncl-cl` are meant to be
OS-independent. The reality is *mostly* true with a handful of concrete leaks that
this document enumerates.

---

## 1. Repository map (NewCL today)

Rust workspace, ~183k LOC, edition 2024, `resolver = "3"`.

| Crate | Role | Port exposure |
|---|---|---|
| `ncl-reader` | s-expr reader | none — pure Rust |
| `ncl-ir` | high-level typed IR | none — pure Rust |
| `ncl-compiler` | IR → LLVM IR | **`new-asm` dep** (defasm) |
| `ncl-llvm` | LLVM bindings, MCJIT, object emit | **inkwell target, jit_mm, new-asm** |
| `ncl-loader` | source graph, dirty tracking, cache | none — pure Rust |
| `ncl-runtime` | GC, allocator, reader glue, **iGui**, win_* FFI | **the bulk of the work** |
| `ncl-cl` | native glue for the Lisp stdlib | none |
| `ncl-driver` | REPL/GUI entry (`ncl` bin) | **Win32 manifest, GUI launch, crash handler** |
| `crates/{docpane,doc-crate,selkie}` | markdown/Mermaid render | docpane is Direct2D (Win-only) |
| `tests/*` | unit + Corman-demo regression | none |

External path deps **not in the clone** (sibling repos on the original `E:\` tree):
- `new-asm` (`../../../../new-asm`) — **non-optional**, used by `ncl-compiler` + `ncl-llvm`.
- `newaudio` (`../../../../NewAudio/...`) — `cfg(windows)` only.
- `newgc-core` — git dep (`github.com/albanread/GC-`), fetched fine.

> A fresh clone **does not build on any platform** until `new-asm` is resolved.

---

## 2. What already works in our favour

- **LLVM main JIT path is triple-agnostic.** `ncl-llvm` builds the MCJIT engine and
  the object-emit `TargetMachine` from `TargetMachine::get_default_triple()`, which
  resolves to `arm64-apple-darwin` on this machine automatically. No hard-coded x86 triple in the hot path.
- **Toolchain lines up.** `llvm-sys = "=221.0.1"` ↔ LLVM 22.1 ↔ our Homebrew
  `llvm` is **22.1.2** at `/opt/homebrew/opt/llvm`. `LLVM_SYS_221_PREFIX` points there.
- **GC is pure Rust** with a non-Windows fallback already sketched: `static_area.rs`
  and `mutator.rs` gate the `VirtualAlloc` reservation behind `cfg(windows)` and fall
  back to a Box-backed fully-committed area otherwise. It will *run* on macOS today
  (just without lazy commit / `MAP_NORESERVE`).
- **Arch/OS already modelled.** `universe.rs` emits `*features*` for `:arm64`,
  `:os-macos` under the right `cfg`. `random.rs` gates `_rdtsc` to x86_64 with a
  wall-clock fallback. `stack_map.rs` notes the aarch64 32-GPR case.
- **Tagged representation is arch-neutral** — 3-bit tag on 64-bit little-endian words;
  both targets qualify. The boxed calling convention is plain `extern "C"`; LLVM lowers
  AAPCS64 for us.

---

## 3. Hard blockers to *compile* the core on macOS

These stop `ncl-runtime` / `ncl-llvm` from compiling at all. Phase-1 scope.

### 3.1 `new-asm` missing & x86-only
`ncl-compiler` and `ncl-llvm` `use new_asm` unconditionally. It backs the Lisp-facing
`defasm` form, which lets user code embed **Win64-ABI x86 assembly** (`#param` →
`rcx/rdx/r8/r9`), JIT-compiled via `LLVMAppendModuleInlineAsm`. Two problems: the
crate isn't in the clone, and its product is x86 text asm — meaningless on ARM.

**Plan:** vendor a `new-asm` crate into the workspace that keeps the *type surface*
(`AsmProc`, `AsmParam`, `AsmType`, `AsmRetType`, `build_module_asm_string`) so
`ncl-compiler`/`ncl-llvm` compile unchanged, but on aarch64 `defasm` raises a Lisp
error ("`defasm` unsupported on this architecture") instead of emitting x86. A real
AAPCS64 implementation (`x0–x7` mapping) is a later, optional item. `defasm` is a
power-user FFI escape hatch, not core — almost no demos need it.

### 3.2 Core runtime files import `windows` unconditionally
Three files in `ncl-runtime/src/` are compiled on all targets but pull in `windows::`:
- `win_ffi.rs` (23 refs) — the `defun-dll` / FFI bridge.
- `win_surface.rs` (18 refs) — drawing-surface handoff.
- `static_area.rs` — only the `VirtualFree`/release path (already `cfg(windows)`,
  needs the `else` arm verified to compile).

**Plan:** gate the Windows bodies behind `cfg(windows)` and add macOS stubs/equivalents:
`win_ffi` → a `libffi`/`dlopen`-based bridge (or a stub that errors) under a neutral
module name; `win_surface` → folds into the new Cocoa/Metal surface (Phase 2). For
Phase 1 (headless) these can be compile-stubbed so the console REPL links.

### 3.3 inkwell target feature is x86-only
`ncl-llvm` pins `inkwell = "=0.9.0"` with features `["llvm22-1", "target-x86"]`.
ARM codegen needs **`target-aarch64`**. Add it (keep `target-x86` for cross/AOT).

### 3.4 `jit_mm.rs` non-Windows paths are `unimplemented!()`
The custom JIT memory manager exists to register **Windows SEH** unwind tables
(`RtlAddFunctionTable`) so Rust panics unwind through JIT frames. Its `reserve`/
`commit`/finalize are `unimplemented!()` off-Windows.

**Plan:** on macOS, *don't* use the custom MM at all — let MCJIT's default
`SectionMemoryManager` allocate and register `eh_frame` (DWARF CFI) itself, which is
the normal macOS/ELF story. Gate the custom-MM engine constructor to `cfg(windows)`
and use the plain inkwell `create_jit_execution_engine` path elsewhere. (If we later
need precise unwinding, `__register_frame` over the emitted `.eh_frame` is the hook.)

### 3.5 `ncl-driver` Win32 bits
`build.rs` + `embed-manifest` inject a Win32 app manifest under `gui-app`; `main.rs`
calls `igui::crash_handler` / `splash` / `crash_view` under `#[cfg(windows)]` already.
`#![cfg_attr(feature="gui-app", windows_subsystem="windows")]` is a no-op elsewhere.

**Plan:** gate the build-script manifest step to `cfg(windows)` (it already early-returns
on non-Windows, verify). The console path is clean. Crash handling on macOS → a later
Mach/`signal` handler; stub for Phase 1.

---

## 4. The GUI port — iGui: Direct2D/D3D11/DirectWrite → Cocoa/Metal/Core Text

`ncl-runtime/src/igui/` is **~20.3k LOC across ~30 files**, gated `#![cfg(windows)]`.
This is the largest, most uncertain piece.

### 4.1 The boundary that saves us
The MANIFESTO's load-bearing rule: **the Lisp side only ever sees a typed event
mailbox + a drawing surface.** Events: `Key, Char, Mouse, Focus, Resize, Paint,
Close, Menu, DpiChange, ThemeChange`. If that boundary is honoured in the code (to be
verified precisely in `channels.rs` / `replies.rs` / the event enum), the port is:
re-implement everything *below* the mailbox, leave everything above untouched.

### 4.2 The Windows stack and its Mac mapping

| iGui file(s) | Windows API | Mac-native replacement |
|---|---|---|
| `window.rs`, `child.rs`, `menu.rs` | Win32 HWND, message pump, menus | **Cocoa**: `NSApplication`/`NSWindow`/`NSMenu`, the AppKit run loop |
| `d3d.rs` | Direct3D11 swapchain | **Metal**: `CAMetalLayer` + `MTLDevice`/command queue |
| `d2d.rs`, `renderer.rs`, `canvas.rs` | Direct2D immediate 2D | 2D engine — see decision below |
| `dwrite.rs`, `font_metrics.rs` | DirectWrite | **Core Text** (`CTFont`, `CTLine`) |
| `crash_handler.rs` | SEH / vectored EH | Mach exception port or `signal()` |
| `system_colors.rs` | Win32 theme | `NSColor`/appearance APIs |

Event pump: the UI thread runs the **AppKit run loop** instead of the Win32 message
pump — exactly the "thread boundary is the OS boundary" swap the MANIFESTO describes.
Lisp still runs on its worker thread and drains the same mailbox.

### 4.3 The one real design decision: the 2D renderer
Direct2D is an *immediate-mode 2D vector API* (rounded rects, strokes, glyph runs,
layers) plus DirectWrite for text and a pixel canvas for the demos (the tank sim,
animations). Three credible ways to reproduce it on Mac:

- **A. Core Graphics + Core Text into a Metal-backed layer** *(recommended)*.
  CG is an immediate 2D API that maps almost 1:1 onto Direct2D concepts; Core Text ≈
  DirectWrite. Most faithful, Apple-native, smallest conceptual gap, no giant deps.
  Pixel canvas → a Metal texture or `CGBitmapContext` blit.
- **B. `wgpu` + `vello` (or `lyon`)** — cross-platform GPU 2D. One renderer for all
  future targets, but a heavy dependency and a from-scratch reimplementation of the
  D2D drawing model; text shaping via `cosmic-text`/`swash`.
- **C. `skia-safe`** — Skia maps very closely to Direct2D (paths/paints/`SkShaper`),
  could even re-back Windows later for a single renderer. Large native dependency.

Trade-off in one line: **A** is the least code and most "Mac-correct"; **C** is the
most reusable; **B** is the most Rust-ecosystem-native. My recommendation is **A**
for fidelity and dependency hygiene, unless cross-platform renderer reuse is a stated
goal — then **C**.

### 4.4 docpane
`crates/docpane` (markdown + Mermaid for the help/doc pane) is Direct2D, `cfg(windows)`.
It shares `selkie` (pure-Rust Mermaid→IR, already portable). Re-target docpane's
renderer to whichever 2D engine §4.3 picks; `selkie` comes along for free. Lowest
priority — gate it off for Phases 1–2.

---

## 5. Crate dependencies to neutralise on macOS

| Dep | Status | Action |
|---|---|---|
| `new-asm` | missing, x86 | vendor stub crate (§3.1) |
| `windows = 0.62` | `cfg(windows)` in runtime manifest ✓ | but 3 src files leak it — gate them (§3.2) |
| `newaudio` | `cfg(windows)` ✓ | leave disabled; Mac audio = CoreAudio later |
| `docpane` | `cfg(windows)` ✓ | leave disabled until §4.4 |
| `embed-manifest` | build-dep | gate to `cfg(windows)` (§3.5) |
| `notify` | cross-platform (FSEvents) ✓ | none |
| `inkwell`/`llvm-sys` | x86 feature only | add `target-aarch64` (§3.3) |

New Mac-only deps (Phase 2): `objc2` + `objc2-app-kit` + `objc2-foundation` +
`objc2-metal` + `objc2-quartz-core` (CAMetalLayer) + `core-text`/`core-graphics`
(option A) under `[target.'cfg(target_os="macos")'.dependencies]`.

---

## 6. Phasing

**Phase 0 — Toolchain (½ day).**
Resolve `new-asm` (vendor stub), point `LLVM_SYS_221_PREFIX` at Homebrew LLVM, flip
inkwell to `target-aarch64`. Goal: `cargo build -p ncl-llvm` links against LLVM 22.1.

**Phase 1 — Headless core on Apple Silicon (the proof).**
cfg-gate the three `win_*` leaks, ensure the Box-backed static area + mutator paths
compile, route the JIT through the default memory manager (no SEH MM). Goal: console
`ncl` builds, JITs `core.lisp` + `clos.lisp` from source, runs the REPL, and passes
`tests/ncl-tests` + the `bench/gauntlet.lisp`. **This validates compiler+GC+runtime on
ARM with zero GUI.** Biggest risk concentrated here, smallest surface.

**Phase 2 — iGui Mac shim.**
Cocoa window + run loop + events behind the mailbox → Metal `CAMetalLayer` surface →
port the renderer (§4.3) → canvas → Core Text → menus/panes incrementally. Bring up
the pixel-canvas demos first (tank sim), then the REPL/editor panes.

**Phase 3 — Long tail.**
`defun-dll`/FFI via `libffi`+`dlopen`; `defasm` AAPCS64 (optional); CoreAudio for
`newaudio`; docpane; Mach-exception crash handler; lazy `mmap(MAP_NORESERVE)` to
recover the GC's lazy-commit behaviour the Box fallback loses.

---

## 7. Open questions for the user

1. **Repo strategy** — is MacNCL a *hard fork* that diverges, or a Mac *shim layer*
   we intend to upstream into NewCL (single tree, `cfg(target_os)`)? The MANIFESTO's
   whole design points at the latter; a hard fork will rot against upstream.
2. **`new-asm` source** — can you provide the real `new-asm` crate, or do we vendor a
   stub and treat `defasm` as unsupported-on-ARM for now?
3. **2D renderer** — option A (Core Graphics/Core Text), B (wgpu/vello), or C (Skia)?
   Drives the bulk of Phase 2.
4. **Cross-platform ambition** — should the Mac renderer also become the *future*
   Windows renderer (favouring B/C), or is Mac-native fidelity the only goal (A)?
