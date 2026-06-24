# Getting started

NCL ships as a single `ncl.exe` plus the `docs/` tree this page is
part of. The shortest path from "I have the executable" to "I'm
talking to NCL" is two steps.

## 1. Run it

Double-click `ncl.exe`, or:

```powershell
> ncl.exe
```

A window opens with an empty MDI client. The menu bar has **File**,
**Edit**, **Lisp**, **Tools**, and **Help**. There is no command line
to type at; everything starts from the menu or a keyboard accelerator.

To get a REPL: **Lisp → New REPL** (or `Ctrl+Shift+R`). A new pane
appears with a `>` prompt.

## 2. Say hello

```lisp
> (format t "~&Hello, ~a!~%" "Lisp")
Hello, Lisp!
> (+ 1 2 3 4)
10
> (defun fact (n) (if (zerop n) 1 (* n (fact (- n 1)))))
FACT
> (fact 100)
93326215443944152681699238856266700490715968264381621468592963895217599993229915608941463976156518286253697920827223758251185210916864000000000000000000000000
```

Two things are worth noting at the keyboard.

**Bignums work automatically.** `(fact 100)` exceeds 64-bit and NCL
quietly switches into a `num-bigint`-backed representation. The
runtime stores the limb data inside its own heap, so the GC sees
those bytes; you never have to think about it.

**The REPL is multi-line.** A top-level form spread across several
lines holds the prompt until you complete it:

```lisp
> (defun greet (whom)
    (format t "~&Hello, ~a!~%" whom))
```

Press `Enter` at the end of an open form and the prompt continues on
the next line. Press `Enter` after the closing paren and the form is
submitted.

## 3. Open a source file

**File → Open** (or `Ctrl+O`) opens a `.lisp` file in a new editor
pane. The editor is sexp-aware: `Ctrl+M` jumps to the matching paren,
parens are auto-balanced as you type, and the buffer's content is
re-checked against the reader on every keystroke so syntax errors
appear in the gutter immediately.

**Lisp → Compile File** (`Ctrl+F5`) compiles the active editor pane's
buffer into the running session — every `defun` and `defvar` is
re-evaluated, every `defmacro` re-installed. The REPL stays where it
was; only the global environment changes.

## 4. Read this manual without leaving NCL

**Help → Documentation** (or `F1`) opens this manual in a doc pane
inside the same window. The pane is a peer of the editor and the
REPL — drag it, tile it, leave it open while you work. The Markdown
files live next to the executable, under `docs/user/`. You can edit
them with the editor pane and reload to see your changes; this
manual is rebuildable in place.

## 5. Drive the doc viewer from Lisp

Three primitives let any program open and update doc panes:

```lisp
(open-doc-window TITLE)              ; → pane-id, or NIL
(doc-set-markdown PANE-ID MARKDOWN)  ; → T or NIL
(doc-append-markdown PANE-ID MARKDOWN) ; → T or NIL
```

A click-along tutorial is a few lines:

```lisp
(defun tutorial ()
  (let ((doc (open-doc-window "Tutorial")))
    (doc-set-markdown doc "# Step 1

Try `(+ 2 2)` at the REPL.")
    (sleep 5)
    (doc-append-markdown doc "

# Step 2

Now try `(fact 20)`.")))
```

The `doc-append-markdown` form is fast — a tight loop streaming
output into a doc pane (say, a long log build-up with formatted
sections) coalesces into one repaint per Win32 idle.

## What's next

The four chapters in this manual are designed to be read in any
order. If you only want one, read [the pipeline](pipeline.md) — it's
the shortest explanation of why your `(+ 1 2)` was JIT-compiled to
machine code and not interpreted from a bytecode tape.

If you want all four, the order in [the index](index.md) is the
order I'd suggest: pipeline → GC → REPL. Each builds on language
you've seen in the previous.
