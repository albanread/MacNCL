# Keyboard Shortcuts

## Frame / Global

These shortcuts work regardless of which MDI child has focus.

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+E` | Open / activate ledit editor |
| `Ctrl+Shift+L` | Open / activate Log view |
| `F5` | Run buffer (ledit: send buffer to Lisp worker) |
| `Alt+F4` | Exit NCL |

## File menu

| Shortcut | Action |
|----------|--------|
| `Ctrl+N` | New buffer |
| `Ctrl+O` | Open file |
| `Ctrl+S` | Save |
| `Ctrl+Shift+S` | Save As |

## Edit menu (ledit)

| Shortcut | Action |
|----------|--------|
| `Ctrl+Z` | Undo |
| `Ctrl+Y` | Redo |
| `Ctrl+X` | Cut |
| `Ctrl+C` | Copy |
| `Ctrl+V` | Paste |
| `Ctrl+A` | Select All |

## Lisp menu (ledit)

| Shortcut | Action |
|----------|--------|
| `F5` | Run Buffer (whole buffer, or selection if active) |
| `Ctrl+Enter` | Run Form at Point (top-level form under cursor) |
| `F7` | Run compile check on buffer |
| `F8` | Jump to next diagnostic |

## Cursor navigation (ledit)

| Shortcut | Action |
|----------|--------|
| `Left` / `Right` | Move one character |
| `Up` / `Down` | Move one line |
| `Home` | Start of line |
| `End` | End of line |
| `PgUp` / `PgDn` | Page up / down |
| `Ctrl+Right` | Forward S-expression |
| `Ctrl+Left` | Backward S-expression |
| `Shift+Left` / `Shift+Right` | Extend selection left / right |
| `Shift+Up` / `Shift+Down` | Extend selection up / down |
| `Shift+Home` / `Shift+End` | Extend selection to start / end of line |
| `Shift+Ctrl+Right` | Extend selection forward one S-expression (see Slurp below) |

## Structural editing (ledit)

| Shortcut | Action |
|----------|--------|
| `Ctrl+Right` | Move forward over S-expression |
| `Ctrl+Left` | Move backward over S-expression |
| `Ctrl+Shift+Right` | Slurp Forward — pull next sibling into current list |
| `Ctrl+Shift+Left` | Barf Forward — push last inner sexp out of current list |
| `Alt+W` | Wrap next sexp with `( )` |
| `Alt+S` | Splice / Unwrap — remove enclosing parens |
| `Alt+R` | Raise — replace enclosing form with sexp at point |

### What the structural commands do

**Slurp Forward** (`Ctrl+Shift+Right`):

```lisp
(foo |bar) baz   →   (foo |bar baz)
```

**Barf Forward** (`Ctrl+Shift+Left`):

```lisp
(foo |bar baz)   →   (foo |bar) baz
```

**Wrap** (`Alt+W`):

```lisp
|foo bar   →   (|foo) bar
```

**Splice** (`Alt+S`):

```lisp
(foo |bar baz)   →   foo |bar baz
```

**Raise** (`Alt+R`):

```lisp
(foo |bar baz)   →   |bar
```

Each structural operation is a single undo step (`Ctrl+Z`).

## MDI system shortcuts

These are standard Windows MDI shortcuts provided by the OS:

| Shortcut | Action |
|----------|--------|
| `Ctrl+F4` | Close active MDI child |
| `Ctrl+F6` | Cycle to next MDI child |
| `Ctrl+Shift+F6` | Cycle to previous MDI child |

## See also

- [Editor (ledit)](editor.md) — full editor documentation
- [REPL pane](repl.md) — REPL keyboard usage
- [Windows surface](windows-surface.md) — frame and tool windows
