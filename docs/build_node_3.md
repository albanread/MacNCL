# Build node 3 — first pixels

*Written 2026-05-10, after commit `b5507ef`. 18 commits since
[build_node_2.md](build_node_2.md).*

Node 2 ([build_node_2.md](build_node_2.md)) ended with
NCL running real Lisp programs — closures, higher-order
functions, recursive list manipulation, the `compose` /
`map-list` / `(lambda (x) (* x x))` flavour of code. It was a
small Lisp but a real one.

Node 3 took us from there to:

```
$ ncl --load Lisp/demos/hello-igui.lisp \
      --eval "(run-hello-igui)"
```

— and a child window pops onto the screen, painted by Lisp,
through the same Direct2D / DirectWrite pipeline the sister NewCP
repo uses for its IDE. Resize the child, the scene repaints. Close
the frame, the process exits cleanly.

Pixels on screen from Lisp. That's the milestone.

The `run-hello-igui` body that produced the swatches reads like
ordinary CL:

```lisp
(defun paint-hello (child-id width height)
  (with-batch child-id
    (clear +slate+)
    (fill-rect 0 0 width 40 +panel+)
    (fill-rect 60 80 100 60 +red+)
    (fill-rect 200 80 100 60 +green+)
    (fill-rect 340 80 100 60 +blue+)
    (draw-line 0 110 width 110 1 +white+)
    (draw-line 250 60 250 160 1 +white+)
    (stroke-rect 4 4 (- width 8) (- height 8) 2 +yellow+)))

(defun run-hello-igui ()
  (igui-start)
  (let ((id (open-child "hello-igui")))
    (paint-hello id 480 320)
    (loop
      (let ((ev (next-event -1)))
        (cond
          ((null ev) nil)
          ((eq (getf ev :kind) :frame-close) (return :done))
          ((and (eq (getf ev :kind) :resize)
                (= (getf ev :child-id) id))
           (paint-hello id (getf ev :width) (getf ev :height)))
          ((and (eq (getf ev :kind) :close)
                (= (getf ev :child-id) id))
           (close-child id) (return :done))
          (t nil))))))
```

That's a 23-line program, half of it whitespace, and it really runs.

## What landed, in rough order

The journey from "a Lisp" to "a Lisp that paints" went through 18
commits. They split into three eras: language polish (where most
of the work happened in user-Lisp), runtime support for things
macros couldn't reach, and the iGui port + bindings.

**Strings, equal, mutation**

We started the node with strings. The reader already had string
literals; the runtime got UTF-32 packed storage (matching the
sister repo's design), `string=`, `aref`/`char` indexing,
codepoint iteration, and the printer learned to wrap them in
quotes. `equal` followed — recursive structural equality through
cons trees and string-content equality, with `eq` as the
short-circuit fast path. Then `setf` for cons cells and string
elements: `(setf (car x) v)` and `(setf (aref s i) c)`, lowering
to a small handful of new IR variants.

The big win was **mutable let-locals**. A pre-pass over the body
collects every name that's a `setq`/`setf` target — including
inside nested lambdas — and the let-binding wraps those names'
init expressions in a `(cons init nil)` cell. Reads emit `(car
slot)`, writes emit `(rplaca slot v)`. When a boxed binding is
captured by a lambda, the cons (the box) is what gets stored in
the env vector, so mutations propagate. `make-counter` works:

```lisp
(defun make-counter ()
  (let ((n 0))
    (lambda () (setf n (+ n 1)) n)))

(defparameter *c1* (make-counter))
(funcall *c1*) ; 1
(funcall *c1*) ; 2
(funcall *c1*) ; 3
```

That's the closures-as-objects pattern, and it works because the
mutation analysis sees `(setf n …)` inside the lambda body when
walking the let's body and decides to box `n`.

**Stdlib in user-Lisp**

`Lisp/core.lisp` started filling out: `reverse`, `append`,
`mapcar`, `member`, `find`, `position`, `assoc`, `nth`,
`nthcdr`, `last`, `butlast`, `every`, `some`, `identity`,
`copy-list`. None of these required compiler changes — they
compose from `defun` + `if`/`cond` + `cons`/`car`/`cdr` +
recursion. `Session::with_stdlib()` evaluates the file once at
session start; tests opt in.

**Integer division and variadic functions**

`truncate` and `rem` landed as primitives — the tagged-fixnum
trick works out neatly here: `srem` on two tagged values returns
an already-tagged result because the shifts cancel. `mod`,
`floor`, `oddp`, `evenp` are user-Lisp wrappers in core.lisp.

`&rest` was the bigger piece. The reader produces a `&REST`
symbol; the param-list parser recognises it; the function-entry
prologue gains a synthetic `let` binding that calls
`ncl_build_rest_list(mutator, args, start, n_args)` to allocate
the trailing args as a fresh cons chain. Mutation analysis even
runs over the synthetic binding so `(setq r ...)` inside a
variadic body works. The stdlib variadic `min` and `max` came
along for the ride.

`apply` followed naturally: a runtime helper walks the tail
list, allocates a combined args buffer, copies the prefix in,
splats the list, dispatches through `ncl_funcall`. The compiler
emits the prefix into a stack-alloca'd array (same pattern
`funcall` already used) and threads it to the helper.

**format**

`format` is where the install_native pattern earned its keep.
`run_format` walks the control string codepoint-by-codepoint,
handles `~A` (princ), `~S` (prin1), `~D` (decimal), `~%` (newline),
`~~` (literal). `format_shim` is the standard JIT-callable
function pointer that builds the rest list from variadic args
and calls `run_format`. We install it under the name `FORMAT` in
`Session::with_config`, and now `format` is a first-class Lisp
function — `(funcall #'format ...)`, `(apply #'format ...)`, all
of it works. The mechanism (`install_native` + a Rust shim with
the JIT calling convention) becomes the home for every other
"Rust-implemented Lisp function" we add later.

**defmacro and backquote**

The biggest single jump in the node. The macroexpand module
gained `value_to_word` / `word_to_value` — the bridge between
compile-time `Value` and runtime `Word` representations. The
pass walks the form tree, dispatching to a registered macro
when a cons-form's head names one. Special forms whose
structure includes non-evaluated positions — `quote`, `defun`,
`defmacro`, `lambda`, `let`, `let*` — are recognised so we
don't try to expand names in binding lists.

`defmacro` itself is just `defun` with the resulting Function
installed in the macro registry instead of the symbol's
function cell. The body runs at expansion time with the call's
unevaluated argument forms passed as Words.

Backquote landed as a separate commit. The reader already
produced `(BACKQUOTE …)` / `(UNQUOTE …)` / `(UNQUOTE-SPLICING …)`
marker forms; the macroexpander gained a Steele-style cell-by-
cell walker that turns ` `(a ,b ,@c d) ` into `(cons 'a (cons b
(append c (cons 'd nil))))`. The dotted-unquote case
(`` `(a . ,x) ``) works because `expand_quasiquote_cdr`
recognises `(unquote x)` in cdr position and returns `x`
directly, bypassing the cons.

After backquote, the `with-open-file` and `handler-case` and
`with-batch` macros all came in as ordinary user-Lisp definitions.
The compiler stopped needing to grow new special forms; macros
took over.

**File I/O**

A thin port from the sister NewCP repo's `host_file_sys.rs`.
Process-global handle table behind a `Mutex<HashMap>`; opens
return positive i64 handles, -1 / 0 sentinels for failure. We
wrap in Lisp:

```lisp
(with-open-file (out "log.txt" :output)
  (write-line out "ready"))
```

The `with-open-file` macro dispatches at expansion time on the
direction keyword (`:input` / `:output` / `:append`) so the
runtime form is just a `let` around the right open-fn. No
runtime cost for the dispatch. Reads / writes are UTF-8 on disk,
UTF-32 in memory.

**Keywords**

`:input` and friends were nominally in the reader but didn't
self-evaluate — bare `:input` produced "unbound variable". The
reader was already interning keywords in the KEYWORD package;
the compiler just had to recognise that and emit a Word literal
instead of a value-cell load. We intern with a colon prefix on
the runtime side (`:INPUT`) so the printer renders them right.

**Conditions**

`error` and `handler-case`. The natural-feeling implementation
was `std::panic::catch_unwind` with `extern "C-unwind"` function
pointers, and we tried that first — and discovered that
LLVM-MCJIT on Windows doesn't reliably register the SEH
`.pdata` tables the unwinder needs to walk JIT frames. The panic
escapes to the OS as a 0xe06d7363 ("MSVC C++ exception not
caught") and aborts.

So we used a software approach: a thread-local condition slot
and a handler-depth counter. `error` checks the depth — if zero
(no handler installed), prints the message and aborts; if
non-zero, stashes the condition and returns NIL. `handler-case`
clears the slot, runs the body, checks the slot on the way out,
dispatches to the handler if set.

The limitation: between `(error …)` and the matching handler,
the JIT frames see NIL as the "value" of the error form, and if
those frames perform a trapping operation on that value (`(car
(error …))` — calling car on NIL — would trap) the trap fires
before the handler sees the condition. In practice `error` is
overwhelmingly the last thing in its branch, so this is rarely
observable. `LispCodeFn` and the call shims still use
`extern "C-unwind"` so a future ORC-JIT migration removes the
limitation entirely.

`loop` and `return` use the same software-unwind trick. A
thread-local LOOP_BREAK_PENDING flag, `%native-loop` runs the
body thunk in a Rust loop and checks the flag, `(return v)`
sets it. Same caveat as `error`; same idiomatic workaround
(put `return` last in its branch, typically inside a `cond`).

## The iGui port

Around the middle of the node, the project crossed the
"language is enough; now we want it to do something" line. We
went over to the sister NewCP repo at `E:/NewCP/NewCP/` — the
Component Pascal port — and lifted its iGui module wholesale.

iGui is a Direct-rendered MDI frame: D3D11 swap chain wrapped
by D2D, DirectWrite for text, an event mailbox for the language
thread, batch-builders for drawing commands. The integration
layer (NewCP's `cp_exports.rs`) routed everything through CP's
module system — `NativeModuleArtifact` / `ExportDirectory` /
`NativeExportBinding` and friends. We don't have any of that;
we have `install_native`, which is the same idea, simpler.

So: 9,741 lines of iGui — `batch.rs`, `channels.rs`, `child.rs`,
`window.rs`, `d2d.rs`, `d3d.rs`, `dwrite.rs`, `executor.rs`,
the redit editor and log_view tools, all of it — copied byte-
for-byte and gated behind `#[cfg(windows)]`. The only file that
needed surgery was `cp_exports.rs`, where we deleted the
~280-line `native_module_artifact()` registration boilerplate
and stripped the imports. Every other file is the NewCP source.

The Cargo.toml for `ncl-runtime` gained a Windows-only
dependency block matching NewCP's: the `windows` crate at 0.62
with the right Win32 / Direct2D / Direct3D / DirectWrite /
DXGI / HiDPI feature flags. Build time on the runtime crate
went from a few seconds to ~20s the first time the windows
crate hit the cache; incremental rebuilds stayed fast.

This was the largest single act of borrowing in the project so
far, and it dropped in cleanly because both projects share the
"Rust core, no module-system entanglement, Win32 wrapped behind
the COM types from the windows crate" architecture that the
MANIFESTO laid out. That payoff felt earned.

## The Lisp side of iGui

After the port came the Lisp bindings. A new
`ncl-runtime/src/igui/lisp_shims.rs` (sibling of `cp_exports.rs`,
also under `igui/`) holds the standard-ABI shim functions:

```
igui-start  igui-wait  igui-quit
open-child  close-child  set-title
next-event
%begin-batch  %submit-batch
%emit-clear  %emit-fill-rect  %emit-stroke-rect  %emit-draw-line
```

They do Word-to-native translation, call the corresponding iGui
function, convert the result back. `Session::with_config`
installs them only on Windows, gated by `cfg(windows)`.

`(igui-start)` spawns a thread that calls `window::run` (which
blocks for the lifetime of the message pump); the JoinHandle is
stashed in a `Mutex<Option<JoinHandle>>` so `(igui-wait)` can
join it. A bare `--eval "(igui-start)"` returns immediately and
the process exits before you see anything; `--eval "(progn
(igui-start) (igui-wait))"` keeps it alive until you close the
frame.

`(next-event timeout-ms)` pulls from the mailbox and converts
the Rust `IGuiEvent` enum to a Lisp plist:

```
(:KIND :KEY    :CHILD-ID 1 :VKEY 65 :MODS 8 …)
(:KIND :MOUSE  :CHILD-ID 1 :X 200 :Y 150 :OP :MOVE …)
(:KIND :RESIZE :CHILD-ID 1 :WIDTH 480 :HEIGHT 320)
(:KIND :CLOSE  :CHILD-ID 1)
(:KIND :FRAME-CLOSE)
```

The keyword conversion goes through the same intern-with-colon
machinery the compiler uses for `:foo` literals, so user code
can `(eq (getf ev :kind) :key)` and have it actually work.

The drawing primitives are thin wrappers over `batch_mod::push`.
Color is a packed-fixnum 0xRRGGBBAA. `(rgb r g b)` and
`(rgba r g b a)` build them; `+slate+` / `+panel+` / `+red+` /
`+green+` etc. are named constants. `with-batch` is a macro
that wraps `(%begin-batch CHILD-ID)` / body / `(%submit-batch)`
into a scoped form.

The driver gained `--load FILE` alongside `--eval`. Both can
chain, both share a single session, both load the stdlib by
default. The `hello-igui.lisp` demo loads cleanly via
`--load Lisp/demos/hello-igui.lisp` followed by
`--eval "(run-hello-igui)"`.

## Lessons / observations

1. **The MANIFESTO's "no module system" rule paid off.** When we
   went to borrow from NewCP, the only thing we had to remove
   was NewCP's module-registration boilerplate. Everything else
   compiled clean. If we'd built a parallel module system we'd
   have spent a week reconciling it.

2. **Macros were the inflection point.** Everything before
   `defmacro` had to be a special form in the compiler. After
   `defmacro` + backquote, `with-open-file`, `handler-case`,
   `with-batch`, `loop`, `return` — all user-Lisp. The
   compiler crate stopped growing.

3. **Software-unwound conditions are a working compromise.**
   The Windows-MCJIT-no-SEH-tables problem could've eaten a
   week of investigation; the thread-local-flag fallback gave
   us conditions in 30 lines and one commit. We documented the
   limitation, kept the call signatures `extern "C-unwind"`
   anyway, and moved on. The path to "real" unwinding (ORC JIT
   migration) is a config flip away from this state.

4. **Plists for events were the right call.** Worth saying out
   loud. The CP side had to use 7 separate INTEGER out-params
   because of FFI constraints; we got to use a property list,
   which is one of those CL idioms that just works for "untyped
   structured data". Pattern-matching with `getf`+`cond` is
   clear, easy to extend, and prints reasonably.

5. **The asymmetry holds.** The "one event channel in, fast
   per-pane channels out" architecture you flagged early in
   this node was the right design lens. The Lisp surface ended
   up reflecting it naturally: `(next-event ...)` is a single
   global poll; drawing operations always target a specific
   child via `with-batch`. Multi-threaded Lisp lands cleanly on
   top whenever it does.

6. **The iGui-Lisp surface stayed small.** Six event-loop
   functions, four drawing primitives, three colors-and-helpers
   forms, the `with-batch` macro — that's the entire user-
   facing GUI vocabulary so far, plus eleven event keywords.
   The whole thing fits in your head while you're writing demo
   code.

7. **The cross-thread story is built-in, not bolted on.** The
   GUI thread is an OS thread that owns HWNDs and pumps Win32
   messages; the Lisp thread is whatever called `(igui-start)`;
   the shims marshal via SendMessageW for synchronous RPCs and
   via the mailbox / per-pane batch slots for async traffic.
   Nothing in the language side knows or cares; the boundary is
   correctly drawn and stays drawn.

8. **Latest-wins is the right semantics for graphics.** The
   per-pane "current batch" slot replaces previous submissions
   atomically, so a Lisp loop that submits 200 batches per
   second to a 60Hz pump silently drops the in-between frames.
   No flicker, no torn frames, no special handling needed in
   user code.

## Where it leaves us

NCL is now a Lisp that paints. Real Direct2D rendering,
real D3D11 swap chains, real DirectWrite (waiting for `draw-text`
bindings to surface). 264 ncl-compiler tests, all green. A 23-line
demo file producing a window with coloured rectangles you can
drag and watch repaint.

The MANIFESTO's "FFI is a feature, not a foundation" rule shows
up in the shape of the iGui binding: the Win32 APIs aren't
exposed to Lisp at all — what Lisp sees is a small set of native
functions installed in symbol cells, none of which is conceptually
"Win32." The same Lisp surface (`with-batch`, `fill-rect`,
`next-event`, plists carrying events) maps onto AppKit + Metal
just as naturally when the Mac iGui shows up.

Node 4's natural starts:

- **`draw-text`** — DirectWrite is already running; binding it
  gives us labels, status bars, simple text widgets.
- **More primitives** — circles, ovals, arcs, paths. Already in
  `cp_exports.rs` waiting for shims.
- **A real REPL** — `read-line` from stdin, eval, print result,
  back to loop. Once that's there the system feels like a Lisp.
- **Mac iGui** — when the sister repo's Mac line moves, we follow.

The walking boots are off; we put on running shoes somewhere
around the macros commit. Where to next is the only question.
