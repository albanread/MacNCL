# NCL User Guide

*A Lisp for working hackers, by way of Corman, McCarthy, and a JIT
that thinks it's an interpreter.*

---

## FOREWORD

NCL is a Common Lisp. It compiles every form
through LLVM, runs on a generational GC, talks to Win32 via a
metadata-backed FFI surface, and answers a `>` prompt within a
few hundred milliseconds of the icon being clicked. Its language is
the Corman Lisp dialect of ANSI Common Lisp; its implementation is a
from-scratch Rust core. The compatibility lives at the source level.
Recompile from source and your old `.lisp` files run.

If you know SBCL, CCL, Clozure, or Allegro, nothing in here will
surprise you for long. If you came up on Corman and miss it, some of the
demos still run. If LISP for you means McCarthy 1960, you'll find
everything from `(car (cdr ...))` up to `defmethod :around` waiting
for you, and the same `(quote foo)` trick still works.

You start a session by typing

```
    ncl
```

at the command prompt of your operating system. The system answers
with `ncl>`. You leave by typing `(quit)`, `(exit)`, or by striking
end-of-file (Ctrl+D on Unix, Ctrl+Z+Enter on Windows). Everything
else is detail.

> **Note:** This guide is a work in progress. Sections 1 and 2 are
> complete; later sections are being written as the implementation
> matures. Features marked *(planned)* exist in the design but are
> not yet shipped.

---

## SECTION 1 — Atoms, Lists, and the Five Primitives

### 1.1 Atoms

An *atom* is a symbol, a number, a string, or a character. Symbols
are written as plain words:

```
    FOO          X1          NIL          *PRINT-CASE*
```

The reader folds unquoted letters to upper case before interning, so
`foo`, `Foo`, and `FOO` denote the same symbol. You can change the
readtable to keep case, but: don't. The whole standard library
expects upper-case interned names and you will spend an afternoon
debugging if you fight it.

NCL recognises numbers in the full Common Lisp menagerie:

```
    42                  fixnum (signed 61-bit on x86-64)
   -17                  ditto, negative
    100000000000000000000  bignum (arbitrary precision; integer math
                          promotes through fixnum → bignum on overflow,
                          transparently)
    3/4                 ratio (exact rational; reduces automatically)
    3.14                double float
    1.5e3               float in scientific notation
    #b1011              binary integer = 11
    #o755               octal integer = 493
    #x1A                hexadecimal integer = 26
    #36rZZ              radix-36 integer = 1295
    #c(1 2)             complex number, real 1 imag 2
```

A *string* is `"like so"`; a *character* is `#\a`, `#\Space`,
`#\Newline`. Backslash inside a string escapes the next character;
the recognised escapes are `\"` and `\\`.

Atoms evaluate as follows:

> Numbers, strings, and characters evaluate to themselves. So do `T`
> and `NIL`. Keywords (symbols whose name begins with `:`) evaluate
> to themselves. Everything else is a variable name; the value of
> the variable is returned.

`T` and `NIL` are sacred. They cannot be reassigned. `NIL` is both
boolean falsity and the empty list — those are the same object.
Every other value is true.

### 1.2 Lists and S-Expressions

Every datum in Lisp is an S-expression. Every program in Lisp is an
S-expression. This is the whole trick.

```
    (A B C)
    (1 2 3 4)
    ((A B) (C D) E)
    ()                                  this is NIL — the empty list
```

To evaluate a non-empty list `(F A1 A2 ... AN)`, the system
evaluates each `Ai` in turn, then applies the function named by `F`
to the resulting values:

```
    > (* 3 4)
    12
    > (+ (- 5 3) (* 2 7))
    16
```

A handful of forms — the *special forms* — break this rule and look
at their arguments unevaluated. You'll meet them as we go.

### 1.3 The Five Primitives

Following McCarthy, you can build every list operation out of five
primitives. NCL provides them:

```
    (car   X)         the first element of list X
    (cdr   X)         X with its first element removed
    (cons  X Y)       prepend X to list Y
    (atom  X)         T if X is an atom; NIL if a cons
    (eq    X Y)       T if X and Y are the same object; NIL otherwise
```

`car` and `cdr` are pronounced "car" and "could-er", because in 1958
the IBM 704 had instructions named `CAR` and `CDR` that took the
address part and decrement part of a register pair. We've kept the
names for the same reason Latin lives on in anatomy. The
compositions `caar`, `cadr`, `cddr`, `caddr`, ..., up to four levels
deep, are predefined and expand as you'd expect.

```
    > (car '(A B C))
    A
    > (cdr '(A B C))
    (B C)
    > (cons 'A '(B C))
    (A B C)
    > (atom 'A)
    T
    > (atom '(A B))
    NIL
    > (eq 'A 'A)
    T
```

The leading `'X` is `(quote X)` — a special form that hands its
argument back unevaluated. Without it, `(car (A B C))` would try to
call a function named `A`.

### 1.4 The Conditional

`COND` is the conditional McCarthy gave us; NCL also provides `IF`,
`WHEN`, `UNLESS`, `CASE`, and `TYPECASE`. The shape of `COND`:

```
    (COND (P1 E1)
          (P2 E2)
          ...
          (PN EN))
```

The predicates `P1, P2, ...` are evaluated in turn until one yields
a non-`NIL` value; the corresponding `Ei` is then evaluated and its
value returned.

```
    > (cond ((= 1 2)  'no)
            ((= 1 1)  'yes)
            (T        'fallback))
    YES
```

`IF` is the binary form: `(IF P THEN ELSE)`. The `ELSE` branch may
be omitted, in which case the false case returns `NIL`. `WHEN` and
`UNLESS` allow multiple body forms and return the last one's value.

If you find your `COND` tower wanting more visual structure, do
`(require 'ifstar)` and use Foderaro's `if*`:

```
    (if* (eq op :add) then
           (incf counter)
           (push val results)
         elseif (eq op :reset) then
           (setq counter 0)
           (setq results nil)
         else
           (error "unknown op ~A" op))
```
