# NCL

NCL is a Common Lisp implementation for Windows. It compiles every form through LLVM, runs on a precise generational GC, and answers a `>` prompt within seconds of startup. Its language is the Corman Lisp dialect of ANSI Common Lisp; its core is written in Rust.

Start a text session:

```text
ncl
```

Start the MDI GUI:

```text
ncl --windows
```

## Feature highlights

- **JIT compiler** — every form, including REPL input, is compiled to machine code via LLVM. No interpreter.
- **CLOS** — full object system with multiple inheritance, generic functions, `:before`/`:after`/`:around` methods, and `eql`-specializers.
- **Conditions and restarts** — non-unwinding handlers, `HANDLER-BIND`, `RESTART-CASE`, `INVOKE-RESTART`.
- **Bignums** — arbitrary-precision integers; fixnum overflow promotes transparently.
- **FFI** — Corman-style `DEFUN-DLL` plus a pre-built Win32 metadata pack covering the entire Windows API.
- **iGui** — MDI frame with Direct2D rendering, ledit code editor, log view, and Lisp-driven child windows.
- **Audio** — game SFX synthesis and ABC/MIDI playback via the embedded NewAudio crate.
- **Hot reload** — filesystem watcher re-loads changed Library files between REPL prompts.
- **Threads** — OS threads with cooperative stop-the-world GC.

## Documentation

| Page | Contents |
|------|----------|
| [Getting Started](getting-started.md) | Launching NCL, startup flags, the init library, quitting |
| [Editor (ledit)](editor.md) | Code editor MDI child: file ops, running code, structural editing |
| [REPL pane](repl.md) | Using the GUI REPL child window |
| [Windows surface](windows-surface.md) | iGui MDI frame, child windows, event loop, demos |
| [Keyboard shortcuts](keyboard-shortcuts.md) | Complete shortcut reference |
| [Crash recovery](crash-recovery.md) | Crash dump pane, reading register snapshots, restarting |
