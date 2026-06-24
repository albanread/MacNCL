//! Typed intermediate representation. The reader produces
//! `ncl_runtime::Value`s; the compiler lowers those to `Expr`; the
//! LLVM back-end lowers `Expr` to LLVM IR.
//!
//! Phase 3 starts with the smallest IR that can represent fixnum
//! arithmetic. As the language grows, this enum grows alongside —
//! it is deliberately not factored into a generic AST framework
//! (see MANIFESTO.md, "When in doubt, be Lisp": don't build a
//! pass framework before you need one).

/// A typed expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// An immediate fixnum.
    Const(i64),
    /// A pre-tagged raw Word constant. Used for symbol references,
    /// statically-allocated quoted lists, and other compile-time-
    /// resolved values whose tagged bit pattern is known. Emitted
    /// as a single i64 constant.
    Word(u64),
    /// A float literal. `bits` is the IEEE-754 f64 bit pattern (used
    /// for the unboxed `Repr::F64` value the JIT can compute on
    /// natively); `boxed` is the raw Word of a pre-allocated static-area
    /// boxed Float — the constant-fold target when the literal is needed
    /// as a tagged Word, so a float literal in a Word context costs zero
    /// per-evaluation allocation (matching pre-unboxing behaviour). See
    /// `docs/performance-unbox-float.md` Sprint 1.
    Float { bits: u64, boxed: u64 },
    /// The literal `nil`.
    Nil,
    /// The truth value `t`.
    True,
    /// Reference to the Nth parameter of the current function.
    /// Only valid inside a function body — top-level expressions
    /// don't have parameters and Param is a compile error there.
    Param(usize),
    /// Read the Nth parameter as an unboxed `f64`. Emitted when the
    /// parameter is `(declare (double-float ...))`'d. In a float
    /// context the JIT unboxes the (boxed-float) argument once and
    /// computes natively; in a Word context it uses the original boxed
    /// argument. The unbox is unchecked — the declaration is a promise
    /// (CL `(safety 0)` semantics). See docs/performance-unbox-float.md.
    F64ParamRead(usize),
    /// `&rest` accessor: build and return a freshly-allocated list
    /// containing args[start..n_args] in order. Used at the entry
    /// of variadic functions to bind the rest parameter. The
    /// allocation lives in the calling thread's young heap.
    BindRest(u32),
    /// `&optional` accessor: if `n_args > idx`, return args[idx];
    /// otherwise evaluate `default` and return its value. Generated
    /// at the entry of functions that declare optional parameters.
    /// `default` is lowered in an env that has all earlier required
    /// and optional params already bound, so CL's
    /// `(defun foo (a &optional (b (* a 2))))` semantics work.
    OptArg { idx: u32, default: Box<Expr> },
    /// `supplied-p` test for optional args: T if `n_args > idx`, NIL otherwise.
    OptSuppliedP(u32),
    /// `supplied-p` test for keyword args: T if the keyword was found, NIL otherwise.
    KeySuppliedP { keyword_word: u64, key_start: u32 },
    /// `(values v1 v2 ... vN)` — write all `vals` into the
    /// thread-local multi-value slot, return `vals[0]` (or NIL if
    /// empty). The caller (typically multiple-value-bind) reads the
    /// slot to recover the secondaries; for non-tail uses, the slot
    /// is just discarded by the surrounding code.
    Values(Vec<Expr>),
    /// `Expr::EnsureSingleMv(primary)` — evaluate `primary`, write
    /// `[primary]` into the multi-value slot, return `primary`. The
    /// tail-position transform wraps every function-body tail that
    /// isn't `Expr::Values` in this op so the slot always reflects
    /// the function's actual return values, even when the body
    /// internally called another function that itself returned
    /// multiple values.
    EnsureSingleMv(Box<Expr>),
    /// `&key` accessor: scan `args[key_start..n_args]` for a pair
    /// whose first element `eq`s `keyword_word` (a tagged Symbol);
    /// if found, return the following arg. If not found, evaluate
    /// `default` and return its value. The runtime helper does the
    /// scan; LLVM emits the absent-then-default branch.
    KeyArg {
        keyword_word: u64,
        key_start: u32,
        default: Box<Expr>,
    },
    /// Reference to the Nth let-bound local. Indexed in the order
    /// the let bindings were entered (per nested let scopes), reset
    /// when the let scope exits.
    Local(usize),
    /// Read an unboxed `f64` local from its dedicated stack slot
    /// (`slot` is a per-function f64-slot index, distinct from `Local`
    /// indices). Emitted for a `let` local declared `(double-float ..)`.
    /// See docs/performance-unbox-float.md Sprint 2.
    F64LocalRead(usize),
    /// Store an unboxed `f64` into local slot `slot` (the value is
    /// coerced to f64) and yield it. The lowered form of a float local's
    /// `let`-init and of `(setq float-local v)`. Mutation is in-place in
    /// the stack slot — no heap cons box, so the value stays unboxed
    /// across a loop.
    F64LocalStore { slot: usize, value: Box<Expr> },
    /// Sequential evaluation: each form runs, the last one's value
    /// is the result. Empty progn yields `nil` per CL convention.
    Progn(Vec<Expr>),
    /// Lexical binding scope. Each `bindings[i]` is evaluated in
    /// the *outer* env (parallel binding, like CL's `let`); their
    /// results are pushed to the locals vec for `body` to read via
    /// `Local(prev_top + i)`. Body is single-expression (the
    /// lowering wraps multiple body forms in a `Progn`).
    Let { bindings: Vec<Expr>, body: Box<Expr> },
    /// Binary addition (overflows wrap silently in Phase 3; the
    /// trap-and-promote-to-bignum path lands when the numeric
    /// tower does).
    Add(Box<Expr>, Box<Expr>),
    /// Binary subtraction.
    Sub(Box<Expr>, Box<Expr>),
    /// Binary multiplication.
    Mul(Box<Expr>, Box<Expr>),
    /// Integer division, truncating toward zero. Both operands are
    /// tagged fixnums; the lowering untags, runs LLVM `sdiv`, and
    /// re-tags the quotient. Division by zero is currently UB at
    /// the LLVM level (lands as a hardware SIGFPE / STATUS_INTEGER_
    /// DIVIDE_BY_ZERO) — proper condition signalling waits on the
    /// condition system.
    Truncate(Box<Expr>, Box<Expr>),
    /// Remainder. Result has the sign of the dividend (matches
    /// LLVM `srem` and CL's `rem`). Both operands tagged; result is
    /// already tagged because `srem (a<<3) (b<<3) = (a rem b) << 3`.
    Rem(Box<Expr>, Box<Expr>),
    /// Bitwise `logand` / `logior` / `logxor` on integers. Fixnums are
    /// `n<<3` (tag 000), so the raw bitwise op on the tagged words yields
    /// the correctly-tagged result with no untag/retag — the JIT inlines
    /// that for the both-fixnum case and calls a bignum-aware helper
    /// otherwise. Integers only (no float contagion), so unlike `mod` /
    /// `truncate` these are safe to lower to dedicated nodes.
    LogAnd(Box<Expr>, Box<Expr>),
    LogIor(Box<Expr>, Box<Expr>),
    LogXor(Box<Expr>, Box<Expr>),
    /// `(ash n shift)` — arithmetic shift. The JIT inlines the common
    /// case (both fixnum, small non-negative shift, no overflow) as a
    /// shifted-and-overflow-checked tagged op; everything else (bignum,
    /// negative/large shift, overflow) falls to `ncl_ash_promote`.
    Ash(Box<Expr>, Box<Expr>),
    /// Allocate a cons cell. Calls `ncl_alloc_cons` at runtime.
    Cons(Box<Expr>, Box<Expr>),
    /// Read the car field of a cons.
    Car(Box<Expr>),
    /// Read the cdr field of a cons.
    Cdr(Box<Expr>),
    /// Object identity. Returns `t` if the two operands have the
    /// same Word bits, else `nil`.
    Eq(Box<Expr>, Box<Expr>),
    /// Signed integer comparisons on tagged fixnums. `a < b` etc.
    /// Both operands must already be tagged fixnums (low 3 bits 0);
    /// the comparison is on the raw 64-bit values, which preserves
    /// signed ordering of the un-tagged values because shifting both
    /// by 3 doesn't change relative ordering.
    Lt(Box<Expr>, Box<Expr>),
    Gt(Box<Expr>, Box<Expr>),
    Le(Box<Expr>, Box<Expr>),
    Ge(Box<Expr>, Box<Expr>),
    NumEq(Box<Expr>, Box<Expr>),
    /// Type predicates. Each returns `t` (Word::T) or `nil`.
    /// `null` checks `is x == nil`. `consp` / `atom` / `listp`
    /// check the tag bits (atom = !cons; listp = nil or cons).
    IsNull(Box<Expr>),
    IsCons(Box<Expr>),
    IsAtom(Box<Expr>),
    IsListp(Box<Expr>),
    /// Conditional. If the first sub-expression evaluates to
    /// anything other than `nil`, evaluate the second; else the
    /// third.
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Call a Lisp function via its Symbol's function cell.
    /// `sym_word` is the raw bits of the Symbol-tagged Word
    /// addressing the symbol in static memory; the call dispatches
    /// through `ncl_call` which atomically loads the function cell.
    Call { sym_word: u64, args: Vec<Expr> },
    /// Read a Symbol's value cell (acquire). Used for global
    /// variable references like `*counter*` after a `defparameter`
    /// or `setq`.
    LoadGlobal(u64),
    /// Atomically store a value into a Symbol's value cell
    /// (release). The lowered form of `(setq name value)` and
    /// `(defparameter name value)`.
    StoreGlobal { sym_word: u64, value: Box<Expr> },
    /// Closure reference: read `env[i]` from the current call's env
    /// argument. Only valid inside a lambda body that has at least
    /// `i+1` captures.
    ClosureRef(usize),
    /// Load a Symbol's function cell as a first-class Function
    /// value. The lowered form of `#'name` and `(function name)`.
    /// Calling the returned Word with `funcall` invokes the named
    /// function at the time of THIS lookup (atomic load_acquire on
    /// the symbol's function cell).
    LoadFunction(u64),
    /// Polymorphic `(length s)` — works on strings (codepoint count)
    /// and lists (cons-cell count). Calls `ncl_length`.
    Length(Box<Expr>),
    /// Structural equality. Recurses through cons trees and
    /// compares strings codepoint-by-codepoint; falls back to eq
    /// for other atoms. Calls `ncl_equal`.
    Equal(Box<Expr>, Box<Expr>),
    /// `(string= a b)` — both operands must be strings. Returns T
    /// or NIL.
    StringEq(Box<Expr>, Box<Expr>),
    /// `(string-char s i)` or `(char s i)` for strings — read the
    /// i-th codepoint as a character. Emitted by lowering of CHAR
    /// and STRING-CHAR; the polymorphic `(aref ...)` form goes
    /// through `Aref` instead.
    StringChar(Box<Expr>, Box<Expr>),
    /// Polymorphic `(aref v i)` — works on strings (returns a
    /// character) and vectors (returns the element). Lowers to a
    /// call to `ncl_aref_generic` which dispatches on the tag of
    /// `v`. Out-of-bounds and non-sequence cases panic.
    Aref(Box<Expr>, Box<Expr>),
    /// Mutate the car of a cons cell. The lowered form of
    /// `(setf (car x) v)`. Evaluates to the new value.
    SetCar(Box<Expr>, Box<Expr>),
    /// Mutate the cdr of a cons cell. The lowered form of
    /// `(setf (cdr x) v)`.
    SetCdr(Box<Expr>, Box<Expr>),
    /// Mutate the i-th codepoint of a string. The lowered form of
    /// `(setf (char s i) c)`. Evaluates to the new character.
    SetChar {
        s: Box<Expr>,
        idx: Box<Expr>,
        ch: Box<Expr>,
    },
    /// Polymorphic `(setf (aref v i) val)` — dispatches on `v`'s
    /// tag at runtime. For vectors, writes the cell with card
    /// marking; for strings, mutates the codepoint. Evaluates to
    /// `val`.
    SetAref {
        v: Box<Expr>,
        idx: Box<Expr>,
        val: Box<Expr>,
    },
    /// Lambda expression. JIT-compiles `body` as a separate function
    /// with the lambda's signature; at the construction site,
    /// evaluates each `captures[i]` in outer scope and packs the
    /// values into the lambda's env vector.
    Lambda {
        arity: u32,
        body: Box<Expr>,
        captures: Vec<Expr>,
    },
    /// Call a first-class function value. `fn_expr` evaluates to a
    /// Function-tagged Word; the call dispatches through `ncl_funcall`.
    Funcall {
        fn_expr: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `(apply fn prefix... tail-list)` — call `fn` with the
    /// prefix args followed by the spread elements of `tail-list`.
    /// `prefix` may be empty. The runtime helper builds the
    /// combined args buffer and dispatches through `ncl_funcall`.
    Apply {
        fn_expr: Box<Expr>,
        prefix: Vec<Expr>,
        tail: Box<Expr>,
    },
    /// Dynamic variable binding — CL's `(let ((*x* v)) body)` where `*x*`
    /// is a special variable. Saves the symbol's value cell, stores the
    /// new value, evaluates `body`, then restores the old value.  The
    /// `value` expression is already lowered and lives in a `Local` slot
    /// (allocated by the enclosing `Let`); `sym_word` is the raw Word of
    /// the symbol whose value cell is manipulated.
    DynamicBind {
        sym_word: u64,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    /// Self-tail-call loop wrapper. Wraps a function body whose tail-
    /// position self-calls have been rewritten to `SelfTailNext`.
    /// Codegen lowers it to a loop: it creates a `loop_header` block
    /// with one phi per parameter (seeded from the entry-block param
    /// values), runs `body` inside the loop, and a `SelfTailNext`
    /// rebinds the param phis and branches back instead of calling.
    /// This turns self-recursion into iteration — the frame is reused,
    /// so deep self-recursion no longer grows the native stack.
    ///
    /// Generated only by the self-tail-call rewrite, only for fixed-
    /// arity functions (required params only — no &optional/&rest/&key),
    /// and only ever as the outermost node of a function body. `arity`
    /// is the number of params (= number of phis to build).
    TailLoop { arity: u32, body: Box<Expr> },
    /// Self-tail-call continuation. Evaluate `args` (exactly the
    /// enclosing `TailLoop`'s arity), rebind the loop's parameter phis
    /// to the new values, and branch back to the loop header. Appears
    /// only in tail position, only inside a `TailLoop`. It never
    /// "returns" — its block ends in the back-branch — so its result
    /// value is unused (codegen yields NIL as a placeholder).
    SelfTailNext { args: Vec<Expr> },
    /// Inline loop — the lowered form of `(fast-loop TEST RESULT
    /// BODY…)`. Semantics: `loop { if TEST → break with RESULT; else
    /// BODY }`. Unlike `(loop …)` (which expands to a capturing lambda
    /// and forces every loop-carried variable to be heap-boxed), this
    /// emits a real back-edge in the SAME function, so loop variables
    /// stay in registers / unboxed f64 stack slots. Loop variables live
    /// in the enclosing scope and are stepped by `setq` in BODY. See
    /// docs/performance-unbox-float.md.
    FastLoop {
        test: Box<Expr>,
        result: Box<Expr>,
        body: Box<Expr>,
    },
    /// Inline infinite loop with embedded breaks — the lowered form of a
    /// `(loop …)` the compiler proved safe to inline (body calls no user
    /// functions and creates no closures; see `native_loop_inlinable`).
    /// `body` runs forever; a `LoopBreak` exits with a value. Same
    /// register/unboxed-carry benefit as `FastLoop`, but matches the
    /// `(loop … (return v) …)` shape. See docs/performance-unbox-float.md.
    InlineLoop { body: Box<Expr> },
    /// Break the innermost enclosing `InlineLoop`, yielding `value` —
    /// the lowered form of `(return v)` (`%loop-return`) inside an
    /// inlined loop. A direct branch to the loop exit; no flag, no
    /// ABORT_PENDING.
    LoopBreak { value: Box<Expr> },
    /// Representation-inference marker: `inner` is *proven* (by the
    /// `optimize` type-inference pass) to evaluate to a heap double-float.
    /// Semantically transparent — `(the-float e)` ≡ `e` — but it tells
    /// codegen it may unbox without the runtime tag-check diamond:
    /// `emit_expr_repr` reads the f64 payload at cell 2 directly. Inserted
    /// ONLY where the type is statically proven, so the unchecked load is
    /// safe. See docs/compiler_completion.md (Slice 1).
    TheFloat(Box<Expr>),
}

impl Expr {
    pub fn add(a: Expr, b: Expr) -> Expr { Expr::Add(Box::new(a), Box::new(b)) }
    pub fn sub(a: Expr, b: Expr) -> Expr { Expr::Sub(Box::new(a), Box::new(b)) }
    pub fn mul(a: Expr, b: Expr) -> Expr { Expr::Mul(Box::new(a), Box::new(b)) }
    pub fn truncate(a: Expr, b: Expr) -> Expr {
        Expr::Truncate(Box::new(a), Box::new(b))
    }
    pub fn rem(a: Expr, b: Expr) -> Expr { Expr::Rem(Box::new(a), Box::new(b)) }
    pub fn logand(a: Expr, b: Expr) -> Expr { Expr::LogAnd(Box::new(a), Box::new(b)) }
    pub fn logior(a: Expr, b: Expr) -> Expr { Expr::LogIor(Box::new(a), Box::new(b)) }
    pub fn logxor(a: Expr, b: Expr) -> Expr { Expr::LogXor(Box::new(a), Box::new(b)) }
    pub fn ash(n: Expr, shift: Expr) -> Expr { Expr::Ash(Box::new(n), Box::new(shift)) }
    pub fn cons(car: Expr, cdr: Expr) -> Expr { Expr::Cons(Box::new(car), Box::new(cdr)) }
    pub fn car(x: Expr) -> Expr { Expr::Car(Box::new(x)) }
    pub fn cdr(x: Expr) -> Expr { Expr::Cdr(Box::new(x)) }
    pub fn eq(a: Expr, b: Expr) -> Expr { Expr::Eq(Box::new(a), Box::new(b)) }
    pub fn lt(a: Expr, b: Expr) -> Expr { Expr::Lt(Box::new(a), Box::new(b)) }
    pub fn gt(a: Expr, b: Expr) -> Expr { Expr::Gt(Box::new(a), Box::new(b)) }
    pub fn le(a: Expr, b: Expr) -> Expr { Expr::Le(Box::new(a), Box::new(b)) }
    pub fn ge(a: Expr, b: Expr) -> Expr { Expr::Ge(Box::new(a), Box::new(b)) }
    pub fn num_eq(a: Expr, b: Expr) -> Expr { Expr::NumEq(Box::new(a), Box::new(b)) }
    pub fn is_null(x: Expr) -> Expr { Expr::IsNull(Box::new(x)) }
    pub fn is_cons(x: Expr) -> Expr { Expr::IsCons(Box::new(x)) }
    pub fn is_atom(x: Expr) -> Expr { Expr::IsAtom(Box::new(x)) }
    pub fn is_listp(x: Expr) -> Expr { Expr::IsListp(Box::new(x)) }
    pub fn if_(c: Expr, t: Expr, e: Expr) -> Expr {
        Expr::If(Box::new(c), Box::new(t), Box::new(e))
    }
    pub fn call(sym_word: u64, args: Vec<Expr>) -> Expr {
        Expr::Call { sym_word, args }
    }
    pub fn load_global(sym_word: u64) -> Expr { Expr::LoadGlobal(sym_word) }
    pub fn store_global(sym_word: u64, value: Expr) -> Expr {
        Expr::StoreGlobal { sym_word, value: Box::new(value) }
    }
    pub fn closure_ref(idx: usize) -> Expr { Expr::ClosureRef(idx) }
    pub fn load_function(sym_word: u64) -> Expr { Expr::LoadFunction(sym_word) }
    pub fn length(x: Expr) -> Expr { Expr::Length(Box::new(x)) }
    pub fn equal(a: Expr, b: Expr) -> Expr {
        Expr::Equal(Box::new(a), Box::new(b))
    }
    pub fn string_eq(a: Expr, b: Expr) -> Expr {
        Expr::StringEq(Box::new(a), Box::new(b))
    }
    pub fn string_char(s: Expr, i: Expr) -> Expr {
        Expr::StringChar(Box::new(s), Box::new(i))
    }
    pub fn aref(v: Expr, i: Expr) -> Expr {
        Expr::Aref(Box::new(v), Box::new(i))
    }
    pub fn set_aref(v: Expr, idx: Expr, val: Expr) -> Expr {
        Expr::SetAref { v: Box::new(v), idx: Box::new(idx), val: Box::new(val) }
    }
    pub fn set_car(cons: Expr, value: Expr) -> Expr {
        Expr::SetCar(Box::new(cons), Box::new(value))
    }
    pub fn set_cdr(cons: Expr, value: Expr) -> Expr {
        Expr::SetCdr(Box::new(cons), Box::new(value))
    }
    pub fn set_char(s: Expr, idx: Expr, ch: Expr) -> Expr {
        Expr::SetChar { s: Box::new(s), idx: Box::new(idx), ch: Box::new(ch) }
    }
    pub fn lambda(arity: u32, body: Expr, captures: Vec<Expr>) -> Expr {
        Expr::Lambda { arity, body: Box::new(body), captures }
    }
    pub fn funcall(fn_expr: Expr, args: Vec<Expr>) -> Expr {
        Expr::Funcall { fn_expr: Box::new(fn_expr), args }
    }
    pub fn apply(fn_expr: Expr, prefix: Vec<Expr>, tail: Expr) -> Expr {
        Expr::Apply {
            fn_expr: Box::new(fn_expr),
            prefix,
            tail: Box::new(tail),
        }
    }
    pub fn param(idx: usize) -> Expr { Expr::Param(idx) }
    pub fn bind_rest(start: u32) -> Expr { Expr::BindRest(start) }
    pub fn values(vals: Vec<Expr>) -> Expr { Expr::Values(vals) }
    pub fn ensure_single_mv(primary: Expr) -> Expr {
        Expr::EnsureSingleMv(Box::new(primary))
    }
    pub fn opt_arg(idx: u32, default: Expr) -> Expr {
        Expr::OptArg { idx, default: Box::new(default) }
    }
    pub fn opt_supplied_p(idx: u32) -> Expr {
        Expr::OptSuppliedP(idx)
    }
    pub fn key_supplied_p(keyword_word: u64, key_start: u32) -> Expr {
        Expr::KeySuppliedP { keyword_word, key_start }
    }
    pub fn key_arg(keyword_word: u64, key_start: u32, default: Expr) -> Expr {
        Expr::KeyArg {
            keyword_word,
            key_start,
            default: Box::new(default),
        }
    }
    pub fn local(idx: usize) -> Expr { Expr::Local(idx) }
    pub fn progn(forms: Vec<Expr>) -> Expr { Expr::Progn(forms) }
    pub fn let_(bindings: Vec<Expr>, body: Expr) -> Expr {
        Expr::Let { bindings, body: Box::new(body) }
    }
    pub fn dynamic_bind(sym_word: u64, value: Expr, body: Expr) -> Expr {
        Expr::DynamicBind { sym_word, value: Box::new(value), body: Box::new(body) }
    }
    pub fn tail_loop(arity: u32, body: Expr) -> Expr {
        Expr::TailLoop { arity, body: Box::new(body) }
    }
    pub fn self_tail_next(args: Vec<Expr>) -> Expr {
        Expr::SelfTailNext { args }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_round_trip() {
        let e = Expr::Const(42);
        assert_eq!(e, Expr::Const(42));
    }

    #[test]
    fn add_constructs() {
        let e = Expr::add(Expr::Const(1), Expr::Const(2));
        match e {
            Expr::Add(a, b) => {
                assert_eq!(*a, Expr::Const(1));
                assert_eq!(*b, Expr::Const(2));
            }
            _ => panic!(),
        }
    }
}
