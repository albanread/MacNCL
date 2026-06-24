# Windows Surface

The Windows surface is NCL's native GUI layer: an MDI frame rendered with Direct2D and DirectWrite, driven by a Win32 message pump on the main thread while the Lisp evaluator runs on a worker thread.

## Enabling the surface

Start NCL with `--windows`:

```text
ncl --windows
```

From Lisp, check whether the surface is active:

```lisp
(windows-enabled-p)   ; => T when started with --windows, NIL otherwise
```

The `init.lisp` bootstrap conditionally loads the `win32-*` modules when this returns `T`.

## The MDI frame

The frame window is titled **NewCL** and hosts MDI child windows. The default menu bar has five menus:

- **File** — New, Open, Save, Save As, Exit
- **Edit** — Undo, Redo, Cut, Copy, Paste, Select All
- **Lisp** — Run Buffer, Run Form at Point, structural editing commands
- **Demos** — one entry per `.lisp` file found in `Lisp/demos/`
- **Tools** — ledit (`Ctrl+Shift+E`), Log (`Ctrl+Shift+L`)

## Built-in tool windows

### ledit

The code editor. Press `Ctrl+Shift+E` or choose **Tools → ledit**. See [Editor](editor.md) for full details.

### Log view

The log view shows diagnostic output from the runtime. Press `Ctrl+Shift+L` or choose **Tools → Log**.

Log lines from `LOG-FORMAT` in Lisp code appear here, as do `[ledit]` run notifications and runtime diagnostics. The view is a singleton MDI child; newest entries appear at the top and the buffer is capped at 16 384 entries.

## Opening child windows from Lisp

```lisp
;; Open a render-host child (Direct2D drawing surface)
(igui:open-child "title")          ; => child-id (integer)

;; Open a text-grid child (scrollable text, like a terminal)
(igui:open-text-child "title")     ; => child-id

;; Open a REPL child (loads and runs gui-repl.lisp)
;; This is a Lisp-level helper, not a primitive
(igui:open-repl-child "title")
```

The returned `child-id` is an opaque integer you use in subsequent calls:

```lisp
(igui:close-child child-id)
(igui:set-child-title child-id "new title")
```

## Drawing to a render-host child

Drawing uses a batch model: collect drawing calls in `WITH-BATCH` and submit them atomically.

```lisp
(let ((id (igui:open-child "hello")))
  (with-batch id
    (clear +slate+)
    (fill-rect 60 80 100 60 +red+)
    (draw-text 76 142 "hello" 13 +white+)))
```

Predefined colours: `+black+`, `+white+`, `+red+`, `+green+`, `+blue+`, `+yellow+`, `+slate+`, `+panel+`. Build custom colours with `(rgb r g b)` or `(rgba r g b a)`.

## Event loop

```lisp
(igui:next-event)   ; => event plist or NIL (non-blocking)
```

Events are property lists. Common keys: `:kind`, `:child-id`, `:width`, `:height`, `:x`, `:y`, `:char`, `:key`, `:codepoint`, `:mods`, `:time-ms`.

The `events` module (loaded by default) provides a higher-level macro:

```lisp
(event-loop-for child-id
  (:resize  (format t "resized to ~A x ~A~%"
                    (getf ev :width) (getf ev :height)))
  (:char    (handle-key (getf ev :char)))
  (:close   (return :done)))
```

## Timer-driven animation

```lisp
(set-redraw-rate child-id 16)   ; fire :tick every 16 ms (~60 fps)
(set-redraw-rate child-id 0)    ; clear the timer
```

Inside an event loop, handle `:tick` to redraw each frame:

```lisp
(event-loop-for id
  (:tick  (with-batch id (draw-frame state)))
  (:close (return :done)))
```

## The EvalBuffer event

When you press `F5` in ledit or choose **Lisp → Run Buffer**, NCL pushes an `EvalBuffer` event containing the buffer text. The default handler in `events.lisp` calls `eval-string` on it and writes the result to the log view.

This fires even when the REPL pane is idle — you do not need a running event loop to use `F5`. The Lisp worker thread services the event from its own event-draining loop.

## Demos

The **Demos** menu lists every `.lisp` file in `Lisp/demos/`. Choosing a demo loads it into ledit and evaluates it immediately.

Notable demos:

| File | Description |
|------|-------------|
| `hello-igui.lisp` | Rectangles and text |
| `shapes.lisp` | Full shape vocabulary |
| `text-styles.lisp` | Fonts, sizes, weights |
| `buttons.lisp` | Click handling |
| `click-counter.lisp` | Stateful click handler |
| `bouncing.lisp` | Timer-driven animation |
| `clos-tour.lisp` | CLOS dispatch driving the renderer |
| `heap-monitor.lisp` | Live GC stats in a child window |
| `gui-repl.lisp` | An in-process REPL (see [REPL pane](repl.md)) |

## Cross-thread marshalling

All Win32 HWND operations happen on the UI thread. Calls from the Lisp worker thread (such as `open-child`, `close-child`, `set-child-title`) are automatically marshalled via `SendMessageW` and block until the UI thread completes them. You do not need to do anything special.

To run arbitrary code on the UI thread from Lisp:

```lisp
(on-ui-thread (lambda () (win32 user32 MessageBoxW nil "hi" "NCL" 0)))
```

## See also

- [Editor (ledit)](editor.md) — the built-in code editor
- [REPL pane](repl.md) — the GUI REPL child
- [Keyboard shortcuts](keyboard-shortcuts.md) — frame and tool shortcuts
- `docs/WINDOWS_FFI.md` — detailed FFI and Win32 API documentation
