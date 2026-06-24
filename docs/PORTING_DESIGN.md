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

### 4.5 Phase 2 architecture — what the deep read found (2026-06-24)

Three subsystem reads pinned the exact contracts. The picture:

**The boundary is real but the panes are split.**
- `batch.rs` defines `SurfaceCmd` — a **platform-neutral drawing IR** (Clear, Fill/Stroke
  Rect/Oval/Circle, DrawLine, DrawArc, DrawPath, DrawTextRun + 3 sync text queries,
  clip/offset/scroll, Save/RestoreRect, Mark/Caret/Selection/FocusRing, Blit). Pure
  geometry/color/text data; its only Windows tie is the repaint trigger (`InvalidateRect`).
- **Lisp-driven graphics + canvas panes** render *through* `SurfaceCmd`: `batch::begin`
  → `push` → `submit` → `child.rs::execute_d2d_batch` (the single D2D chokepoint).
- **The Rust-native workspace panes** — `text_view`, `repl_child`, `ledit`, `log_view`
  — call Direct2D/DirectWrite **directly** in their own `paint()` (~6k LOC), bypassing
  `SurfaceCmd`. This is the editor/REPL/log UI itself.

**Mailbox & threading (portable / compatible).**
- `channels::IGuiEvent` (Key/Char/Mouse/Focus/Resize/Close/FrameClose/Theme/Dpi/Menu/
  Tick/EvalBuffer/ReplSubmit) is a bounded MPSC, GUI→Lisp, with per-thread window
  filtering. `replies::Reply` is the 5s-timeout oneshot for the 3 synchronous text
  queries. Both are platform-neutral.
- `window::run<F: FnOnce()+Send>(worker)` takes the calling thread as the **UI thread**
  (Win32 pump) and spawns the Lisp worker. macOS requires AppKit on the **main thread**
  — the model already matches; `run` becomes NSApplication on thread 0 + worker.
- ~53 Lisp functions form the public API (igui-start/-wait/-quit, open-child[-sized],
  close-child, set-title, set-redraw-rate, next-event[-for], filter/-unfilter-window,
  %begin/submit-batch + %emit-*, %measure-text, open-text-window + text-*, open-repl +
  repl-*, open-doc + doc-*, canvas-open/-present, mdi-*). **This surface must be preserved.**

**The unifying move: a `DrawTarget` trait.** Both `execute_d2d_batch` and the panes'
`paint()` call the same ~20 primitives (clear, fill/stroke rect/ellipse, line, geometry,
draw-text-layout, push/pop clip+transform, copy-rect, draw-bitmap). Define that as a
trait; implement it twice — `D2dTarget` (Windows) and `CgTarget` (Core Graphics). Then
`execute_batch<T: DrawTarget>` is one body, and each pane's paint is one body. This is
the lever that makes the 6k LOC of direct-D2D panes portable without per-pane rewrites.

**Surface/present contract to provide on macOS** (replacing `renderer`/`d3d`/`d2d`/`dwrite`):
one process-wide Metal device + Core Text; per child a `CAMetalLayer`-backed `NSView`
(create/resize/present); a begin/end-frame cycle; text via `CTFont`/`CTLine` with
`GetMetrics`/`HitTestPoint`/`HitTestTextPosition` equivalents
(`CTLineGetTypographicBounds`/`…StringIndexForPosition`/`…OffsetForStringIndex`).
`canvas.rs` is already platform-neutral (BGRA32 buffer → `Blit`); reuse verbatim.

### 4.6 Phase 2 execution slices
1. **2.0** Add macOS deps (`core-graphics`/`core-text`/`core-foundation`, later
   `objc2`/`objc2-app-kit`/`objc2-metal`/`objc2-quartz-core`). Extract the portable
   paint types out of `batch.rs` into `igui_paint` (un-gated), re-exported by `batch.rs`
   so Windows is untouched.
2. **2.1** *(done first because it's verifiable headlessly)* A Core Graphics renderer
   that rasterizes a `&[SurfaceCmd]` into a `CGBitmapContext` — **no window needed**.
   Unit-tested by rendering a batch and asserting pixels / dumping a PNG, mirroring the
   `doc-crate` render-to-PNG snapshot philosophy. Proves the renderer port.
3. **2.2** Cocoa `window::run`: NSApplication/NSWindow on thread 0, `NSView`+`CAMetalLayer`
   surface, `NSEvent`→`IGuiEvent` mailbox, marshalled `open_child`/`close`/`set_title`.
4. **2.3** The `DrawTarget` trait + port `text_view`/`repl_child`/`ledit`/`log_view`.
5. **2.4** Menus (`NSMenu`), cursors, system colors/appearance, then `docpane`.

### 4.7 Phase 2.0–2.2 results (2026-06-24) — renderer + event boundary live ✅

- **2.0** `igui_paint` (drawing IR) and `igui_events` (mailbox) extracted as
  platform-neutral modules; `igui::batch`/`igui::channels` re-export them, Windows
  untouched. macOS deps added (`core-graphics`/`core-text`/`core-foundation`).
- **2.1** `igui_mac::render::CgCanvas` — Core Graphics + Core Text rasteriser for
  `SurfaceCmd`, headless (CGBitmapContext). Shapes, rounded rects, ovals/circles,
  arcs, paths, clip/offset, overlays, BGRA blit, styled text + measurement.
  **10 pixel-assertion tests**; the gated `demo_scene` renders a showcase PNG.
- **2.2** `igui_mac::events` — pure `NSEvent`→`IGuiEvent` translation (modifier
  bits, macOS keycodes/chars → Win32 VKs, y-flip), **6 unit tests**.
  `igui_mac::window::run` (feature `mac-gui`) — `NSApplication`/`NSWindow` on the
  main thread, Lisp worker on a background thread, a local `NSEvent` monitor feeding
  the mailbox, AppKit run loop. **Verified live**: window opens and real mouse/key
  events arrive on the worker thread with correct coordinates.
  Run it: `cargo run -p ncl-runtime --example mac_window --features mac-gui`.

- **2.2c** ✅ `window::present(cmds)` (worker-thread frame submit) + a main-thread
  60 Hz `NSTimer` that renders the latest frame to a `CgCanvas` and blits it into the
  window's `NSImageView` (servo `CGImage` → objc2 `&CGImage` → `initWithCGImage:size:`).
  **Verified live**: the window shows the rendered scene and a circle follows the mouse,
  closing the loop `NSEvent → mailbox → worker → present → render → window`. Set
  `NCL_IGUI_DUMP=path.ppm` to dump the presented frame for headless inspection.

**Remaining:** wire `window::run`/`present` into `ncl-driver` (so `ncl --windows`
launches it) and map the Lisp `igui-*` shims onto it; HiDPI (render at backing scale);
`CAMetalLayer` for the canvas fast-path. Then 2.3 (DrawTarget trait + the Rust-native
panes) and 2.4 (menus/cursors/colours/docpane).

The default (headless) build never pulls AppKit: renderer + event translation are
always built and tested; only the live window sits behind `mac-gui`.

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

## 7. Decisions (locked 2026-06-24)

1. **Repo strategy: hard fork.** MacNCL is a full copy of NewCL that diverges. We
   re-point the out-of-tree path deps (`new-asm`, `newaudio`) to in-repo locations so
   the fork is self-contained. Upstream re-sync is manual.
2. **`new-asm`: vendor a stub.** `crates/new-asm` keeps the type surface
   (`AsmProc`/`AsmParam`/`AsmType`/`AsmRetType`/`build_module_asm_string`) so
   `ncl-compiler`/`ncl-llvm` compile unchanged; `defasm` raises an "unsupported on this
   architecture" Lisp error. Real AAPCS64 codegen is deferred to Phase 3.
3. **2D renderer: Core Graphics + Core Text**, into a Metal `CAMetalLayer` surface.
   Apple-native, ~1:1 with Direct2D/DirectWrite, smallest gap, no heavy deps.
4. **Scope: Mac-native fidelity** (not a cross-platform renderer). Reinforces #3.

**Active work:** Phase 0 + Phase 1 — get the headless core building and JITing on
Apple Silicon. No GUI.

## 7a. Phase 0/1 results (2026-06-24) — core works on Apple Silicon ✅

The headless core builds, boots, JITs, and runs on `aarch64-apple-darwin`. What it took:

- **Toolchain:** `LLVM_SYS_221_PREFIX=/opt/homebrew/opt/llvm`; inkwell features
  `+target-aarch64`; vendored `crates/new-asm` stub; repointed/removed out-of-repo
  path deps (`new-asm`, `newaudio`). The `ncl` binary links LLVM 22.1 statically and
  is a native arm64 Mach-O.
- **`ncl-runtime` compiled unchanged** — the `win_*` files were already `cfg`-gated;
  the Box-backed (non-`VirtualAlloc`) GC static area + mutator paths just work.
- **One code fix:** `jit_mm::make_mm()` returns a **null MM** off-Windows (§3.4), so
  MCJIT uses LLVM's default `SectionMemoryManager`. This was the only runtime blocker.

**Validation (all on Apple Silicon):**
- `(+ 1 2)`, `(fact 30)` → exact bignum, `(fib 30)`, Ackermann, CLOS
  `defclass`/`make-instance`/accessors, `format`, 1000-entry hash tables, and a
  1,000,000-iteration `double-float` `sqrt` kernel — all correct.
- **`bench/gauntlet.lisp` → GAUNTLET ALL-PASS**, including every float-unboxing test
  (the unboxing optimization pass is correct on ARM).
- **`tests/ncl-tests` pass** (characters, closures, declare-special, describe, …).

### §8 — Apple Silicon concurrent-JIT W^X hazard (important)
Running the libtest harness multi-threaded, the `describe` binary aborts (SIGABRT)
after a few tests; **single-threaded it passes 14/14.** Cause: on Apple Silicon the
JIT write-protect toggle (`pthread_jit_write_protect_np`) is **per-thread**, so two
threads JIT-compiling concurrently in one process corrupt each other's W^X state.

- **Tests:** worked around via `RUST_TEST_THREADS=1` in `.cargo/config.toml`.
- **Shipping binary:** unaffected — `ncl` runs Lisp on a single worker thread.
- **Deeper implication (Phase 3):** NCL's multi-mutator design lets Lisp `make-thread`
  spawn threads that could JIT concurrently. On Apple Silicon that needs either a
  global lock around MCJIT compile/finalize, or correct per-thread
  `pthread_jit_write_protect_np(false/true)` bracketing around code emission. Track as
  a real item before multi-threaded Lisp programs are supported on Apple Silicon.
