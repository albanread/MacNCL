# NewCormanLisp

NewCormanLisp (NCL) is a from-scratch Common Lisp built in Rust on
LLVM. It JIT-compiles your code; it runs under a generational page-
heap garbage collector designed specifically for it; it ships its own
integrated editor, REPL, and documentation viewer in a single Win32
window.

You are reading this manual *inside that documentation viewer* — the
pane you're looking at parses Markdown and renders Mermaid diagrams
through the same Direct2D pipeline the editor and REPL use. There is
no web view, no external browser, and no separate doc program. The
pane is a peer of the editor — you can open as many as you like and
feed each from Lisp.

## What's in this manual

**Getting going**

- **[Getting started](getting-started.md)** — install, build, write
  your first NCL program, run it.
- **[The integrated REPL](repl.md)** — the read/eval/print loop, how
  the GUI thread and the worker thread talk to each other.

**The Lisp language**

- **[Numbers](numbers.md)** — fixnums, bignums, rationals, floats,
  complex, and the arithmetic that promotes between them.
- **[Lists](lists.md)** — cons cells, list operations, mapping,
  filtering, folding.
- **[Variables and bindings](variables.md)** — `let`, `defvar`,
  `setf`, lexical and dynamic scope, closures.
- **[Control flow](control-flow.md)** — `if`, `cond`, `case`,
  `loop`, `dotimes`, `dolist`, `block`, `return-from`.
- **[Functions](functions.md)** — `defun`, `lambda`, lambda lists,
  closures, `funcall` / `apply`, higher-order functions, multiple
  values.
- **[Macros](macros.md)** — `defmacro`, quasiquote, expansion,
  `gensym` hygiene, when (and when not) to write one.

**Under the hood**

- **[The pipeline](pipeline.md)** — what happens between a `.lisp` file
  and a running expression. Reader, compiler, IR, LLVM JIT, runtime.
- **[The garbage collector](gc.md)** — a tour of NCL's generational
  page-heap GC, with the cascade and the multi-mutator handshake.

## Why it's shaped this way

Three principles run through everything in NCL.

**Direct-rendered, native, fast.** Everything you see on screen lives
under a single `ID2D1HwndRenderTarget` per pane, Direct2D + DirectWrite
all the way down. No HTML, no JavaScript, no embedded browser. The
doc pane shares its render core with the editor and REPL and uses the
same `RopeBuffer` for text. The cost is being Windows-only today; the
benefit is a startup so fast that "open a doc and read it" is a key
press and a frame, not a process spawn.

**A GC you can argue with.** NCL has its own collector — multi-
mutator generational page-heap with conservative stack pinning, a
chunked two-phase mark / evacuate / rewrite, and a recoverable
mid-evac OOM. Every cycle's behaviour is visible from Lisp via
`(gc-stats)`. The collector is in its own crate (`newgc-core`) and
the [GC chapter](gc.md) takes its design apart visually.

**The manual is a living thing.** This manual is rendered by NCL
itself, in a pane you can hand-edit, that you can also drive from
Lisp:

```lisp
(let ((doc (open-doc-window "Notes")))
  (doc-set-markdown doc "# Hello\n\nThis is **live**."))
```

Your own program can open documentation panes and feed them Markdown.
A tutorial can be a click-along against the running Session — open a
pane, walk the reader through it, update it as they make progress.
The viewer is a primitive, not a separate app.
