# Getting Started

## Launching NCL

### Text REPL

Run `ncl` (or `ncl --repl`) at any command prompt:

```text
ncl
ncl>
```

The prompt is `ncl>`. Continued lines show `...>` until the input is a complete S-expression. Type `(exit)`, `(quit)`, or press `Ctrl+Z` then `Enter` to leave.

### GUI mode

```text
ncl --windows
```

This starts the iGui MDI frame on the main thread and runs the Lisp evaluator on a worker thread. `(windows-enabled-p)` returns `T`. See [Windows surface](windows-surface.md) for what you can do from here.

## Startup flags

```text
ncl [--lean] [--windows]
    [--eval <src> | --load <file> | --check <file>]...
    [--repl]
```

| Flag | Short | Effect |
|------|-------|--------|
| `--eval <src>` | `-e` | Evaluate a source string, print the result |
| `--load <file>` | `-l` | Load and evaluate every form in a file |
| `--check <file>` | `-c` | Dry-run: parse + macroexpand + lower; execute only definitions |
| `--repl` | `-r` | Enter the interactive REPL (default when no other action is given) |
| `--lean` | `-L` | Load only the bare compiler — no CLOS, no `Library/init.lisp` |
| `--windows` | `-W` | Enable the Windows surface and iGui MDI frame |
| `--version` | `-V` | Print version and exit |
| `--help` | `-h` | Print usage and exit |

Multiple `--eval`, `--load`, and `--check` actions can be chained. `--repl` at the end drops you into the prompt after they all complete:

```text
ncl --load setup.lisp --eval "(run-tests)" --repl
```

## The init library

On startup the driver looks for `Library/` next to the executable (or the path in `NCL_LIBRARY`). If found, it prepends that directory to `*LOAD-PATH*` and runs `Library/init.lisp`.

The shipping `init.lisp` loads the standard layer: `streams`, `conditions`, `loop`, `sequences`, `trees`, `characters`, `lists`, `places`, `numbers`, `xp`, `describe`, `events`, `hot-reload`, and — when `--windows` is on — the `win32-*` modules.

Use `--lean` to skip all of this (bare compiler only, no CLOS).

## Brief example session

```lisp
ncl> (+ 1 2)
3
ncl> (defun fact (n)
...>   (if (= n 0) 1 (* n (fact (- n 1)))))
FACT
ncl> (fact 10)
3628800
ncl> (fact 30)
265252859812191058636308480000000
ncl> (exit)
```

Bignum promotion is transparent — `(fact 30)` returns a bignum without any extra work.

## Environment variables

| Variable | Effect |
|----------|--------|
| `NCL_HEAP_BACKEND` | GC implementation: `semispace` (default) or `page-heap` |
| `NCL_LIBRARY` | Override the `Library/` directory path |
| `NCL_PACK_DIR` | Override the `packs/` directory (Win32 metadata pack) |
| `NCL_YOUNG_MB` | Young-heap reservation in MB (default 256) |
| `NCL_OLD_MB` | Old-heap reservation in MB (default 2048) |
| `NCL_STATIC_MB` | Static-area reservation in MB (default 1024) |

## See also

- [Editor (ledit)](editor.md) — the GUI code editor
- [REPL pane](repl.md) — the GUI REPL child window
- [Windows surface](windows-surface.md) — MDI frame and iGui API
- [Keyboard shortcuts](keyboard-shortcuts.md) — complete shortcut reference
