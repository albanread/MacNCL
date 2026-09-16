# MacNCL

A from-scratch, LLVM-JIT **Common Lisp** for Apple Silicon — compiler, GC,
runtime, and a native macOS IDE (Lisp editor + REPL) — ported from
[NCL/NewCL](https://github.com/albanread/NewCL) and modelled on Corman Lisp.
See [MANIFESTO.md](MANIFESTO.md) for the declaration of intent.

**Status: working.** The headless core (compiler + GC + REPL) and the native
Cocoa IDE both run on Apple Silicon. The IDE has a system menu bar, tracks
light/dark and the accent colour, uses SF Pro/SF Mono, native open/save
panels and drag-drop, momentum scrolling, and inline eval-error squiggles.

## Quick start

Requirements: a Rust toolchain and LLVM 22 (`brew install llvm`; the repo's
`.cargo/config.toml` points `LLVM_SYS_221_PREFIX` at Homebrew's).

```sh
./run-gui.sh                 # build + open the IDE as a real .app (lambda icon)
./run-gui.sh --release       # optimised build, faster startup
./run-gui.sh --demo othello-gui  # IDE + run a graphics demo side window
./run-gui.sh --app othello-gui   # demo standalone -- no IDE chrome

cargo run -p ncl-driver --release -- --repl   # console REPL (headless)
echo '(* 6 7)' | cargo run -p ncl-driver --release
```

## Documentation

- **[NCLMac.md](NCLMac.md)** -- the main readme: IDE features, keybindings,
  architecture of the port, testing.
- **[docs/MAC_UI_DESIGN.md](docs/MAC_UI_DESIGN.md)** -- the native-look UI
  design the IDE sprints implement.
- **[docs/MAC_UI_SPRINTS.md](docs/MAC_UI_SPRINTS.md)** -- sprint plan with
  acceptance criteria (all seven sprints landed).
- **[docs/PORTING_DESIGN.md](docs/PORTING_DESIGN.md)** -- the original
  Windows -> macOS porting design and phase record.

## Layout

| Path | What |
|---|---|
| `crates/selkie` | the NCL core (compiler internals, GC engine bindings) |
| `src/ncl-*` | runtime, reader, compiler, LLVM JIT, CLI driver |
| `src/ncl-runtime/src/igui_mac/` | the macOS GUI backend + IDE |
| `Lisp/` | the Lisp standard library and Corman-era demos |

MIT OR Apache-2.0.
