# REPL Pane

The GUI REPL is a Lisp-implemented MDI child that provides an interactive session inside the iGui frame. It is loaded from `Lisp/demos/gui-repl.lisp` and can be opened from the Demos menu or by loading the file into ledit and running it.

For most interactive development, ledit with `F5` / `Ctrl+Enter` is faster. The REPL pane is useful when you want a persistent transcript window alongside your editor.

## Opening the REPL pane

From the Demos menu, choose **Gui Repl**. This loads the demo into ledit and evaluates it, which opens the REPL child window.

You can also open it from Lisp code:

```lisp
(load "Lisp/demos/gui-repl.lisp")
(run-gui-repl)
```

## Submitting input

Type a Lisp form at the `>` prompt and press `Enter`. If the form is not yet complete (unmatched parentheses), the prompt changes to `..` and the line continues. Forms that span multiple lines work naturally — the REPL detects balance and waits.

```lisp
> (+ 1
..   2)
3
> (defun sq (x) (* x x))
SQ
> (sq 7)
49
```

## Error handling

Each form is wrapped in `handler-case`. Errors print in red and return you to the prompt rather than crashing or clearing the session:

```lisp
> (/ 1 0)
** division by zero
> 
```

Previously defined functions remain live after an error.

## Keyboard reference

| Key | Action |
|-----|--------|
| `Enter` | Submit the current input (if balanced) or continue to next line |
| `Backspace` | Delete one character of input |
| Any printable key | Append to current input |

The REPL pane is driven through the iGui text-view API. Its keyboard handling is implemented in Lisp (`gui-repl.lisp`) rather than in the Rust runtime, so it is intentionally minimal — it does not have history navigation, clipboard, or Ctrl-key shortcuts beyond what the underlying text-view child provides.

## Multi-line entry

Multi-line forms work naturally. The depth counter tracks unmatched open parens and prints a `..` continuation prompt:

```lisp
> (let ((x 10)
..      (y 20))
..   (+ x y))
30
```

Two extra spaces of auto-indent are added per open paren level.

## Colours

| Element | Colour |
|---------|--------|
| Prompt `>` / `..` | Blue |
| Normal output | Light grey |
| Return values | Green |
| Errors | Red |

## Transcript

The text-view child keeps a scrollback buffer. Scroll with the mouse wheel to review earlier output. The buffer is not saved between sessions.

## See also

- [Editor (ledit)](editor.md) — the code editor MDI child
- [Windows surface](windows-surface.md) — opening other child windows from Lisp
- [Keyboard shortcuts](keyboard-shortcuts.md) — frame-level shortcuts
