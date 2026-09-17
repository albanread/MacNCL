# MacNCL — NCL on Apple Silicon

MacNCL is the macOS / Apple Silicon (`aarch64-apple-darwin`) port of
[NCL](https://github.com/albanread/NewCL) — a from-scratch, LLVM-JIT Common Lisp
modelled on Corman Lisp. It brings the compiler, GC, runtime, and a **native Cocoa
IDE** (a rich Lisp editor + REPL) to the Mac, replacing the Windows-only
Direct3D/Direct2D/DirectWrite stack with **Metal-free Core Graphics + Core Text** and
**AppKit**.

> Status: the headless core (compiler + GC + REPL) and the GUI IDE both run and are
> verified live on Apple Silicon. This is a hard fork of NewCL; the Windows code paths
> are preserved untouched, and everything shared between platforms goes through
> re-exported portable modules.

---

## Quick start

Requirements: a Rust toolchain and LLVM 22.1.x (Homebrew `llvm`).
`LLVM_SYS_221_PREFIX` must point at the LLVM install (`/opt/homebrew/opt/llvm`); it is
set in `.cargo/config.toml`.

```sh
# The graphical IDE (editor + REPL) — easiest:
./run-gui.sh                       # builds (debug) and opens the IDE
./run-gui.sh --release             # optimised build, faster startup
./run-gui.sh --demo othello-gui    # IDE + run a demo;  --app for standalone
./run-gui.sh --help                # all options

# Console REPL (headless — no GUI)
cargo run -p ncl-driver --release -- --repl
echo '(* 6 7)' | ./target/release/ncl            # one-shot via stdin
./target/release/ncl --eval '(+ 1 2)'            # evaluate and print

# The IDE, the underlying commands run-gui.sh wraps
cargo run -p ncl-driver --features mac-gui -- --windows
cargo run -p ncl-driver --features mac-gui -- --windows path/to/file.lisp
```

The `mac-gui` feature pulls the AppKit bindings (objc2). The **default build is fully
headless** and never links AppKit — the renderer and event translation are always
compiled and unit-tested without a display.

### Graphics apps

Unmodified NCL graphics demos run as **side windows** — each `(open-child …)` is its own
`NSWindow`, drawing via `(with-batch …)` (`SurfaceCmd` batches) and receiving events
through the shared mailbox:

```sh
cargo run -p ncl-driver --features mac-gui -- --windows \
  --load Lisp/demos/draw-square.lisp --eval "(run-draw-square)"

cargo run -p ncl-driver --features mac-gui -- --windows \
  --load Lisp/demos/othello-gui.lisp --eval "(run-othello-gui)"
```

The same Lisp that drives these on Windows drives them here. `--eval`/`--load` run app
code; a bare `file.lisp` argument opens in an editor tab instead.

---

## The IDE

`ncl --windows` opens a native window split into a **Lisp editor** (top) and a **REPL**
(bottom), backed by the real compiler `Session`.

Commands live in the **system menu bar** (MacNCL / File / Edit / Eval / View / Window /
Help) with their ⌘-glyph shortcuts shown; picking an item flows through the same event
mailbox as the keyboard. ⌘Q/⌘H/⌘M behave as standard Mac app controls. New: **⌘N** (File ▸
New) opens a buffer, in addition to ⌘T.

### Editor pane — a rich Lisp editor

- **Syntax highlighting** — special forms, `:keywords`, numbers, strings (multi-line
  aware), `#\` char literals, `; comments`, brackets, and quote/unquote forms each get
  their own colour.
- **Paren matching** — the bracket beside the cursor and its partner are outlined.
- **Paren-aware auto-indent** — Return and Tab indent Lisp-correctly: special forms
  indent their body by two; function calls align under the first argument; top level is
  flush left. Works on partially-typed (unclosed) forms.
- **Paredit structured editing** — slurp, barf, wrap, splice, raise, and forward/
  backward s-expression motion.
- **Incremental search** — Cmd-F, case-insensitive, with all matches highlighted and a
  `⌕ query  n/m` status line; Cmd-G / Return cycle, Shift reverses.
- **Comment toggle** — Cmd-/ comments or uncomments the line or selection.
- **Editing essentials** — undo/redo with typing coalescing, selection, an internal
  clipboard (cut/copy/paste/select-all), a line-number gutter, and vertical motion that
  remembers the preferred column.
- **Files** — open via the command line, **⌘O** (native open panel), **Open Recent ▸**,
  or by dropping a `.lisp` file on the window; **⌘S** saves, **⇧⌘S** saves as. A
  status bar shows `path • line:col • modified` (and an accent ● while an eval runs).
- **Clicks** — double-click selects the word; **triple-click selects the enclosing
  top-level form** (the ⌘↩ eval target); shift-click extends. A failed ⌘↩ eval
  squiggles the form in red — a clean re-eval clears it.
- **Font** — SF Mono (Menlo fallback) with SF Pro chrome; **⌘=/⌘−** zoom code
  12–20pt.
- **Tabs** — up to eight chips; past that a **»** menu lists the rest.
- **Examples** — File ▸ Examples lists the shipped demos; picking one opens it as a
  buffer (⌘R runs it, or load and call its `(run-…)` entry from the REPL).

### REPL pane

- Scrollback transcript with distinct colours for input, output, errors, and notices.
- An input line (with syntax highlighting) that submits on Return **when the parens are
  balanced** — otherwise Return inserts a newline so you can keep typing a multi-line
  form. Shift-Return always inserts a newline.
- Command history (Up / Down), wheel scrolling.
- Every result comes from the real compiler `Session::eval` — the same evaluator the
  console REPL uses.

### Editor → REPL

Definitions you write in the editor become live in the REPL:

- **Cmd-R** runs the whole editor buffer.
- **Cmd-Return** evaluates the top-level form at the cursor.
- Both print into the REPL transcript, so there is one log.

### Keybindings

| Key | Action | | Key | Action |
|---|---|---|---|---|
| **Cmd-R** | run editor buffer | | **Ctrl-→ / ←** | forward / backward sexp |
| **Cmd-Return** | eval form at cursor | | **Ctrl-Shift-→ / ←** | slurp / barf forward |
| **Cmd-N / Cmd-T** | new buffer / new tab | | **Ctrl-W** | wrap in `( )` |
| **Cmd-O** | open… (panel) | | **Ctrl-S** | splice (remove brackets) |
| **Cmd-S / Shift-Cmd-S** | save / save as… | | **Ctrl-Up** | select enclosing form |
| **Cmd-F / Cmd-G** | find / find next | | **Cmd-= / Cmd--** | font bigger / smaller |
| **Cmd-/** | toggle comment | | **Ctrl-R** | raise sexp |
| **Cmd-E / Cmd-L** | focus editor / REPL | | **Tab** | reindent line / selection |
| **Cmd-T / Cmd-W** | new / close tab | | **Cmd-D** | duplicate line |
| **Cmd-1…9** | switch to tab N | | **Cmd-Shift-K** | delete line |
| **Cmd-Z / Shift-Cmd-Z** | undo / redo | | **Alt-↑ / ↓** | move line up / down |
| **Cmd-X/C/V/A** | cut/copy/paste/all | | **Cmd-K** | clear REPL |
| **Return** (REPL) | eval (balanced) | | **↑ / ↓** (REPL) | history · **Esc** cancel search |

Convention: **Command** drives editing and IDE accelerators; **Control** drives
paredit. They never collide.

---

## Architecture of the port

NCL's design always intended a second platform: OS-specific code lives behind a thin
shim, and the Lisp side only ever sees a typed event mailbox and a drawing surface. The
port honours that boundary.

### Core (no GUI)

- **LLVM 22 JIT** on Apple Silicon. The MCJIT engine and object-emit target are built
  from the host triple, so AArch64 codegen comes for free; `inkwell` gains the
  `target-aarch64` feature.
- **No custom JIT memory manager off-Windows.** The Windows build uses a custom manager
  to register SEH unwind tables; on macOS a null manager lets MCJIT use LLVM's default
  `SectionMemoryManager`, which handles Apple Silicon's `MAP_JIT` / W^X correctly.
- **GC** runs on the existing pure-Rust page-heap (Box-backed reservation off-Windows).
- **`defasm`** (inline x86 assembly) is stubbed — it raises an "unsupported on this
  architecture" error rather than emitting x86.

### GUI (`mac-gui` feature)

A platform-neutral spine, with a macOS backend bolted underneath:

| Layer | Portable module | macOS backend |
|---|---|---|
| Drawing IR | `igui_paint` (`SurfaceCmd`) | `igui_mac::render` — Core Graphics + Core Text |
| Event mailbox | `igui_events` (`IGuiEvent`) | `igui_mac::events` — `NSEvent` → `IGuiEvent` |
| Text buffer | `igui_text` (rope) | shared |
| Window / loop | — | `igui_mac::window` — `NSApplication`/`NSWindow` + repaint timer |
| IDE | `igui_mac::ide` (`editor` / `repl` / `app` / `sexp`) | — |

- **Renderer** (`igui_mac::render::CgCanvas`) rasterises a `&[SurfaceCmd]` into a
  `CGBitmapContext` — shapes, text, clipping, blits — and can run **headlessly**, so the
  whole drawing path is unit-tested by rendering to a bitmap and asserting pixels.
- **Window** runs `NSApplication` on the main thread with the Lisp worker on a
  background thread (the same UI-thread-0 + Lisp-worker split NCL uses on Windows,
  which is exactly what macOS's main-thread-AppKit rule wants). A local `NSEvent`
  monitor translates key/mouse/scroll into `IGuiEvent`s; a 60 Hz timer renders the
  latest frame and blits it into an `NSImageView` (bridging the Core Graphics `CGImage`
  to AppKit's `NSImage`).
- The **same `IGuiEvent` enum, drawing IR, and rope buffer** are shared with the Windows
  panes via re-exports, so the two platforms can't drift on the boundary types.

---

## Testing & headless verification

```sh
cargo test -p ncl-runtime igui          # renderer + events + IDE (no display needed)
cargo run -p ncl-driver --release -- -l bench/gauntlet.lisp   # perf gauntlet
```

The renderer and IDE are exercised by ~50 unit tests with no window required. For the
GUI, two environment hooks make the live window inspectable headlessly:

- `NCL_IGUI_DUMP=frame.ppm` — dump the latest presented frame to a PPM.
- `NCL_GUI_SELFTEST='(* 6 7)'` (+ optional `NCL_GUI_RUNBUFFER=1`) — inject keystrokes
  through the real event mailbox so an evaluation runs without a human typing.

> On Apple Silicon, concurrent JIT from multiple threads aborts (the W^X write-protect
> toggle is per-thread), so the test harness forces `RUST_TEST_THREADS=1`. The shipping
> binary runs Lisp on a single worker thread and is unaffected.

---

## Distribution

`make-dmg.sh` produces `dist/MacNCL-<version>.dmg` — a drag-install disk image,
the standard distribution shape for a Mac application that installs into
`/Applications` without admin privileges:

```sh
./make-dmg.sh              # release build -> staged bundle -> styled, compressed DMG
./make-dmg.sh --no-layout  # same, minus the Finder window styling (no Finder
                           # Automation permission prompt)
```

What the user sees when they open the image: a 660×400 window with a custom
background (title, subtitle, and two marked drop zones); the `MacNCL.app` icon
sits in the left zone, an `Applications` symlink in the right, and the install
gesture is one drag between them. The background art lives in
`resources/dmg-background.png` (2× Retina: 1320×800).

The pipeline behind it:

1. `cargo build --release -p ncl-driver --features mac-gui`
2. Stage `MacNCL.app` — binary, `Info.plist`, icon, `MacNCL.sdef`, and a full
   copy of `Lisp/` under `Contents/Resources/Lisp/` so the app boots with its
   standard library and the Examples menu with no repo checkout.
3. Ad-hoc `codesign` (adds no identity; keeps Gatekeeper deterministic in dev).
4. `hdiutil create -format UDRW` → mount → write the volume's `.DS_Store`
   directly (window bounds, icon view, background, icon positions) with the
   Python `ds_store` library, installed into a throwaway venv under `dist/` →
   detach → `hdiutil convert -format UDZO` → `hdiutil verify`.

   The styling is written file-level rather than through Finder AppleScript
   because the macOS 26 Finder no longer exposes `view options` & co. to
   AppleScript. With no network (or `--no-layout`) the script falls back to a
   plain, unstyled image.

Signing notes: for distribution outside your own machines you need a Developer
ID Application certificate plus a notarization ticket (`notarytool submit` +
`stapler`); the ad-hoc signature this script applies is for local use only.
Code-signing for real also activates the `MacNCL.sdef` dictionary so
AppleScript can use named terms (`tell application "MacNCL" to eval "..."`)
instead of the raw `«event MNCLeval»` form.

---

## Roadmap

- Inline diagnostics (compile-check underlines) and multi-buffer / tabs.
- HiDPI: render at the display's backing scale factor for crisp Retina text.
- Native `NSOpenPanel` / `NSSavePanel` dialogs.
- Port the remaining Windows panes (`log_view`, `doc_pane`, `help_pane`) and a Metal
  fast-path for the pixel canvas / animation demos.
- A real AAPCS64 backend for `defasm`; CoreAudio for the audio shims.

See [`docs/PORTING_DESIGN.md`](docs/PORTING_DESIGN.md) for the full design and the
phase-by-phase record.
