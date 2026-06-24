# Editor (ledit)

ledit is the built-in code editor. It is an MDI child window driven entirely from the UI thread, which means it stays responsive even when the Lisp worker thread is busy or has faulted.

Features: syntax highlighting, line numbers, paren-balance indicator in the status bar, bracket-flash on matching delimiters, auto-indent on Enter, structural S-expression editing, and a compile-check pass.

## Opening ledit

- Press `Ctrl+Shift+E` anywhere in the frame.
- Choose **Tools → ledit** from the menu bar.

If ledit is already open, these actions activate the existing window rather than creating a second one.

## Status bar

The status bar at the bottom of the ledit window shows:

```
 [*] path/to/file.lisp   Ln  12, Col  5   84 lines   ()   F7 check
```

- `*` marks unsaved changes.
- `()` is the paren-balance count for the whole buffer. `(+2)` means two unmatched opens; `(-1)` one unmatched close.
- After running `F7`, errors are shown inline. `F8` jumps to the next error.

## Running code

| Action | Key |
|--------|-----|
| Run the whole buffer (or current selection) | `F5` |
| Run the top-level form at the cursor | `Ctrl+Enter` |
| Run compile check on the buffer | `F7` |
| Jump to next diagnostic | `F8` |

Running sends an `EvalBuffer` event to the Lisp worker. The result is written to the [Log view](windows-surface.md#log-view) (`Ctrl+Shift+L`). If there is an active selection, `F5` runs the selection only.

## File operations

| Action | Key |
|--------|-----|
| New buffer | `Ctrl+N` |
| Open file | `Ctrl+O` |
| Save | `Ctrl+S` |
| Save As | `Ctrl+Shift+S` |

Saving also re-runs the compile check automatically.

## Basic editing

| Action | Key |
|--------|-----|
| Undo | `Ctrl+Z` |
| Redo | `Ctrl+Y` |
| Cut | `Ctrl+X` |
| Copy | `Ctrl+C` |
| Paste | `Ctrl+V` |
| Select All | `Ctrl+A` |

Undo coalesces consecutive single-character inserts and consecutive backspace strokes into single steps. Structural editing operations (slurp, barf, wrap, splice, raise) each count as one undo step.

## Cursor motion

| Action | Key |
|--------|-----|
| Character left/right | `Left` / `Right` |
| Line up/down | `Up` / `Down` |
| Start of line | `Home` |
| End of line | `End` |
| Page up/down | `PgUp` / `PgDn` |
| Forward S-expression | `Ctrl+Right` |
| Backward S-expression | `Ctrl+Left` |

Add `Shift` to any motion key to extend the selection.

## Structural editing

These commands operate on balanced S-expressions. They are available from the **Lisp** menu and via keyboard shortcuts.

### Slurp and Barf

**Slurp forward** pulls the next sibling sexp into the current list:

```lisp
(foo |bar) baz   →   (foo |bar baz)
```

**Barf forward** pushes the last inner sexp out of the current list:

```lisp
(foo |bar baz)   →   (foo |bar) baz
```

| Command | Key |
|---------|-----|
| Slurp Forward | `Ctrl+Shift+Right` |
| Barf Forward | `Ctrl+Shift+Left` |

### Wrap, Splice, Raise

**Wrap** surrounds the next sexp at point with `( )`:

```lisp
|foo bar   →   (|foo) bar
```

**Splice** (unwrap) removes the enclosing parens:

```lisp
(foo |bar baz)   →   foo |bar baz
```

**Raise** replaces the enclosing form with the sexp at point:

```lisp
(foo |bar baz)   →   |bar
```

| Command | Key |
|---------|-----|
| Wrap with `( )` | `Alt+W` |
| Splice / Unwrap | `Alt+S` |
| Raise | `Alt+R` |

## Syntax highlighting

ledit highlights the following token classes:

| Token class | Colour |
|-------------|--------|
| Keywords (`defun`, `let`, `if`, …) | Blue |
| Numbers | Amber |
| String literals | Green |
| Line comments (`;…`) | Grey |

Highlighting is refreshed on every edit.

## See also

- [Keyboard shortcuts](keyboard-shortcuts.md) — full shortcut table
- [Windows surface](windows-surface.md) — opening ledit from Lisp code
- [REPL pane](repl.md) — the interactive REPL child window
