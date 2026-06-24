//! optimize.rs — Lisp-aware type/representation inference.
//!
//! See docs/compiler_completion.md. LLVM `-O2` does all the generic SSA
//! optimization once we emit IR, but it cannot reason about NCL's 3-bit
//! tag scheme or the heap-float box — those source-language facts are gone
//! by the time it sees `i64` Words. This pass supplies the one fact that
//! pays off most: *which values are provably double-floats*.
//!
//! ## What it does (Slices 1–3)
//!
//! A forward abstract interpretation over the `Expr` tree tracks one bit
//! per in-scope `Local` slot — "provably double-float" — seeded from float
//! literals, already-unboxed float reads, and `+`/`-`/`*` (CL contagion),
//! threaded through `Let`/`If`/`Progn`.
//!
//! When a `Let` binding's init is proven float, the local is **promoted to
//! an unboxed f64 slot**: the binding becomes `F64LocalStore` (stores the
//! f64 directly — no `ncl_box_float` allocation) and every read becomes
//! `F64LocalRead` (an unboxed load — no `coerce_to_f64` tag-check diamond,
//! and no GC root). This is exactly the representation lowering already
//! gives a `(declare (double-float …))` local; the pass derives it without
//! the declaration. A read used in a *Word* context (passed to a generic
//! call, stored in a cons, …) boxes on demand via `emit_expr`'s
//! `F64LocalRead` arm — so promotion is correct in any context.
//!
//! ## Soundness
//!
//! Over-promotion miscompiles; coarsening is always safe. Conservative by
//! construction:
//!   * a binding is promoted ONLY when its init is *proven* float — a
//!     float literal, an unboxed float read, or `+`/`-`/`*` on proven
//!     floats (contagion: if either operand is a float and the result is
//!     returned at all, it is a float);
//!   * a *mutated* local is never promoted: lowering boxes it into a cons
//!     cell, so its init is `(cons …)` (type `Other`), never proven float;
//!   * the walk is EXHAUSTIVE — every read of a promoted local is rewritten
//!     to `F64LocalRead`, so the binding's unused Word slot (a NIL
//!     placeholder) is never read;
//!   * promoted f64 slots are allocated above lowering's highest slot
//!     (`f64_slot_count`), so they never alias;
//!   * the env-stack mirrors lowering exactly — every local is introduced
//!     by a `Let` (the prologue is the outermost `Let`; synthetic temps are
//!     `Let`s), pushed on entry and truncated on exit — so `Local(i)`
//!     resolves to the right slot even though sibling scopes reuse indices
//!     (validated: Slice 1 shipped unchecked unboxing through this env-stack
//!     with the gauntlet green);
//!   * a nested `Lambda` body is a SEPARATE compilation (this pass runs on
//!     it when that function compiles); the outer walk rewrites its captures
//!     (a captured promoted-local boxes correctly) but does not recurse into
//!     the body, whose reads are `ClosureRef`, not `Local`.

use ncl_ir::Expr;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AbsType {
    Float,
    Other,
}

impl AbsType {
    #[inline]
    fn is_float(self) -> bool {
        matches!(self, AbsType::Float)
    }
}

struct Ctx {
    /// Abstract type of `Local(i)` for in-scope slots.
    env: Vec<AbsType>,
    /// `Some(s)` if `Local(i)` was promoted to f64 slot `s`.
    promo: Vec<Option<usize>>,
    /// Next free f64 slot index (above lowering's slots).
    next_slot: usize,
}

/// Run float-representation inference + promotion over a function body.
pub(crate) fn infer_float_unboxing(body: Expr) -> Expr {
    let mut cx = Ctx {
        env: Vec::new(),
        promo: Vec::new(),
        next_slot: f64_slot_count(&body),
    };
    walk(body, &mut cx).1
}

fn walk_box(e: Box<Expr>, cx: &mut Ctx) -> Box<Expr> {
    Box::new(walk(*e, cx).1)
}
fn walk_vec(v: Vec<Expr>, cx: &mut Ctx) -> Vec<Expr> {
    v.into_iter().map(|x| walk(x, cx).1).collect()
}

/// Forward walk: returns `(inferred type, rewritten expr)`.
fn walk(e: Expr, cx: &mut Ctx) -> (AbsType, Expr) {
    match e {
        // ── proven-float leaves (already unboxed) ─────────────────────────
        Expr::Float { .. } | Expr::F64ParamRead(_) | Expr::F64LocalRead(_) => {
            (AbsType::Float, e)
        }

        // ── local read: a promoted slot reads unboxed ─────────────────────
        Expr::Local(i) => {
            if let Some(Some(s)) = cx.promo.get(i).copied() {
                (AbsType::Float, Expr::F64LocalRead(s))
            } else {
                let t = cx.env.get(i).copied().unwrap_or(AbsType::Other);
                (t, Expr::Local(i))
            }
        }

        // ── let: promote proven-float bindings to f64 slots ───────────────
        Expr::Let { bindings, body } => {
            let base = cx.env.len();
            let mut new_bindings = Vec::with_capacity(bindings.len());
            for b in bindings {
                // Inits are evaluated in the env *before* this let's
                // bindings (a parallel `let`; sound for `let*`).
                let (t, b2) = walk(b, cx);
                if t.is_float() {
                    let s = cx.next_slot;
                    cx.next_slot += 1;
                    new_bindings.push(Expr::F64LocalStore {
                        slot: s,
                        value: Box::new(b2),
                    });
                    cx.env.push(AbsType::Float);
                    cx.promo.push(Some(s));
                } else {
                    new_bindings.push(b2);
                    cx.env.push(t);
                    cx.promo.push(None);
                }
            }
            let (bt, body2) = walk(*body, cx);
            cx.env.truncate(base);
            cx.promo.truncate(base);
            (
                bt,
                Expr::Let {
                    bindings: new_bindings,
                    body: Box::new(body2),
                },
            )
        }

        // ── arithmetic: float contagion ───────────────────────────────────
        Expr::Add(a, b) => bin_arith(*a, *b, cx, Expr::Add),
        Expr::Sub(a, b) => bin_arith(*a, *b, cx, Expr::Sub),
        Expr::Mul(a, b) => bin_arith(*a, *b, cx, Expr::Mul),

        // ── if: both branches in the incoming env; join result types ──────
        Expr::If(c, t, e2) => {
            let c2 = walk_box(c, cx);
            let (tt, t2) = walk(*t, cx);
            let (te, e22) = walk(*e2, cx);
            let jt = if tt.is_float() && te.is_float() {
                AbsType::Float
            } else {
                AbsType::Other
            };
            (jt, Expr::If(c2, Box::new(t2), Box::new(e22)))
        }

        // ── progn: sequence; type is the last form's ──────────────────────
        Expr::Progn(v) => {
            let mut out = Vec::with_capacity(v.len());
            let mut last = AbsType::Other;
            for x in v {
                let (t, x2) = walk(x, cx);
                last = t;
                out.push(x2);
            }
            (last, Expr::Progn(out))
        }

        // ── lambda: rewrite captures (outer env); body compiles separately ─
        Expr::Lambda {
            arity,
            body,
            captures,
        } => {
            let captures = walk_vec(captures, cx);
            (
                AbsType::Other,
                Expr::Lambda {
                    arity,
                    body,
                    captures,
                },
            )
        }

        // ── everything else: recurse ALL children (exhaustive), type Other ─
        // Complete recursion is required: a missed read of a promoted local
        // would read its NIL placeholder slot.
        Expr::F64LocalStore { slot, value } => (
            AbsType::Other,
            Expr::F64LocalStore { slot, value: walk_box(value, cx) },
        ),
        Expr::Values(v) => (AbsType::Other, Expr::Values(walk_vec(v, cx))),
        Expr::EnsureSingleMv(b) => (AbsType::Other, Expr::EnsureSingleMv(walk_box(b, cx))),
        Expr::OptArg { idx, default } => (
            AbsType::Other,
            Expr::OptArg { idx, default: walk_box(default, cx) },
        ),
        Expr::KeyArg { keyword_word, key_start, default } => (
            AbsType::Other,
            Expr::KeyArg { keyword_word, key_start, default: walk_box(default, cx) },
        ),
        Expr::Truncate(a, b) => (AbsType::Other, Expr::Truncate(walk_box(a, cx), walk_box(b, cx))),
        Expr::Rem(a, b) => (AbsType::Other, Expr::Rem(walk_box(a, cx), walk_box(b, cx))),
        Expr::LogAnd(a, b) => (AbsType::Other, Expr::LogAnd(walk_box(a, cx), walk_box(b, cx))),
        Expr::LogIor(a, b) => (AbsType::Other, Expr::LogIor(walk_box(a, cx), walk_box(b, cx))),
        Expr::LogXor(a, b) => (AbsType::Other, Expr::LogXor(walk_box(a, cx), walk_box(b, cx))),
        Expr::Ash(a, b) => (AbsType::Other, Expr::Ash(walk_box(a, cx), walk_box(b, cx))),
        Expr::Cons(a, b) => (AbsType::Other, Expr::Cons(walk_box(a, cx), walk_box(b, cx))),
        Expr::Car(a) => (AbsType::Other, Expr::Car(walk_box(a, cx))),
        Expr::Cdr(a) => (AbsType::Other, Expr::Cdr(walk_box(a, cx))),
        Expr::Eq(a, b) => (AbsType::Other, Expr::Eq(walk_box(a, cx), walk_box(b, cx))),
        Expr::Lt(a, b) => (AbsType::Other, Expr::Lt(walk_box(a, cx), walk_box(b, cx))),
        Expr::Gt(a, b) => (AbsType::Other, Expr::Gt(walk_box(a, cx), walk_box(b, cx))),
        Expr::Le(a, b) => (AbsType::Other, Expr::Le(walk_box(a, cx), walk_box(b, cx))),
        Expr::Ge(a, b) => (AbsType::Other, Expr::Ge(walk_box(a, cx), walk_box(b, cx))),
        Expr::NumEq(a, b) => (AbsType::Other, Expr::NumEq(walk_box(a, cx), walk_box(b, cx))),
        Expr::IsNull(a) => (AbsType::Other, Expr::IsNull(walk_box(a, cx))),
        Expr::IsCons(a) => (AbsType::Other, Expr::IsCons(walk_box(a, cx))),
        Expr::IsAtom(a) => (AbsType::Other, Expr::IsAtom(walk_box(a, cx))),
        Expr::IsListp(a) => (AbsType::Other, Expr::IsListp(walk_box(a, cx))),
        Expr::Call { sym_word, args } => (
            AbsType::Other,
            Expr::Call { sym_word, args: walk_vec(args, cx) },
        ),
        Expr::StoreGlobal { sym_word, value } => (
            AbsType::Other,
            Expr::StoreGlobal { sym_word, value: walk_box(value, cx) },
        ),
        Expr::Length(a) => (AbsType::Other, Expr::Length(walk_box(a, cx))),
        Expr::Equal(a, b) => (AbsType::Other, Expr::Equal(walk_box(a, cx), walk_box(b, cx))),
        Expr::StringEq(a, b) => (AbsType::Other, Expr::StringEq(walk_box(a, cx), walk_box(b, cx))),
        Expr::StringChar(a, b) => (AbsType::Other, Expr::StringChar(walk_box(a, cx), walk_box(b, cx))),
        Expr::Aref(a, b) => (AbsType::Other, Expr::Aref(walk_box(a, cx), walk_box(b, cx))),
        Expr::SetCar(a, b) => (AbsType::Other, Expr::SetCar(walk_box(a, cx), walk_box(b, cx))),
        Expr::SetCdr(a, b) => (AbsType::Other, Expr::SetCdr(walk_box(a, cx), walk_box(b, cx))),
        Expr::SetChar { s, idx, ch } => (
            AbsType::Other,
            Expr::SetChar { s: walk_box(s, cx), idx: walk_box(idx, cx), ch: walk_box(ch, cx) },
        ),
        Expr::SetAref { v, idx, val } => (
            AbsType::Other,
            Expr::SetAref { v: walk_box(v, cx), idx: walk_box(idx, cx), val: walk_box(val, cx) },
        ),
        Expr::Funcall { fn_expr, args } => (
            AbsType::Other,
            Expr::Funcall { fn_expr: walk_box(fn_expr, cx), args: walk_vec(args, cx) },
        ),
        Expr::Apply { fn_expr, prefix, tail } => (
            AbsType::Other,
            Expr::Apply {
                fn_expr: walk_box(fn_expr, cx),
                prefix: walk_vec(prefix, cx),
                tail: walk_box(tail, cx),
            },
        ),
        Expr::DynamicBind { sym_word, value, body } => (
            AbsType::Other,
            Expr::DynamicBind { sym_word, value: walk_box(value, cx), body: walk_box(body, cx) },
        ),
        Expr::TailLoop { arity, body } => (
            AbsType::Other,
            Expr::TailLoop { arity, body: walk_box(body, cx) },
        ),
        Expr::SelfTailNext { args } => (AbsType::Other, Expr::SelfTailNext { args: walk_vec(args, cx) }),
        Expr::FastLoop { test, result, body } => (
            AbsType::Other,
            Expr::FastLoop {
                test: walk_box(test, cx),
                result: walk_box(result, cx),
                body: walk_box(body, cx),
            },
        ),
        Expr::InlineLoop { body } => (AbsType::Other, Expr::InlineLoop { body: walk_box(body, cx) }),
        Expr::LoopBreak { value } => (AbsType::Other, Expr::LoopBreak { value: walk_box(value, cx) }),
        Expr::TheFloat(inner) => (AbsType::Other, Expr::TheFloat(walk_box(inner, cx))),

        // ── leaves with no Expr children ──────────────────────────────────
        Expr::Const(_)
        | Expr::Word(_)
        | Expr::Nil
        | Expr::True
        | Expr::Param(_)
        | Expr::BindRest(_)
        | Expr::OptSuppliedP(_)
        | Expr::KeySuppliedP { .. }
        | Expr::LoadGlobal(_)
        | Expr::ClosureRef(_)
        | Expr::LoadFunction(_) => (AbsType::Other, e),
    }
}

fn bin_arith(
    a: Expr,
    b: Expr,
    cx: &mut Ctx,
    build: fn(Box<Expr>, Box<Expr>) -> Expr,
) -> (AbsType, Expr) {
    let (ta, a2) = walk(a, cx);
    let (tb, b2) = walk(b, cx);
    let t = if ta.is_float() || tb.is_float() {
        AbsType::Float
    } else {
        AbsType::Other
    };
    (t, build(Box::new(a2), Box::new(b2)))
}

/// Highest f64 slot index used by lowering, + 1 (0 if none). Promoted
/// slots are allocated from here so they never alias lowering's. Must be
/// exhaustive — an undercount would collide.
fn f64_slot_count(e: &Expr) -> usize {
    let here = match e {
        Expr::F64LocalStore { slot, .. } => *slot + 1,
        Expr::F64LocalRead(s) => *s + 1,
        _ => 0,
    };
    let mut max = here;
    each_child(e, &mut |c| {
        let n = f64_slot_count(c);
        if n > max {
            max = n;
        }
    });
    max
}

/// Apply `f` to every immediate `Expr` child of `e` (including a Lambda's
/// captures AND body — the slot scan must see lowering's f64 slots wherever
/// they are; over-counting is harmless, under-counting collides).
fn each_child(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    match e {
        Expr::Float { .. }
        | Expr::F64ParamRead(_)
        | Expr::F64LocalRead(_)
        | Expr::Local(_)
        | Expr::Const(_)
        | Expr::Word(_)
        | Expr::Nil
        | Expr::True
        | Expr::Param(_)
        | Expr::BindRest(_)
        | Expr::OptSuppliedP(_)
        | Expr::KeySuppliedP { .. }
        | Expr::LoadGlobal(_)
        | Expr::ClosureRef(_)
        | Expr::LoadFunction(_) => {}
        Expr::F64LocalStore { value, .. } => f(value),
        Expr::EnsureSingleMv(b)
        | Expr::Car(b)
        | Expr::Cdr(b)
        | Expr::IsNull(b)
        | Expr::IsCons(b)
        | Expr::IsAtom(b)
        | Expr::IsListp(b)
        | Expr::Length(b)
        | Expr::TheFloat(b)
        | Expr::LoopBreak { value: b }
        | Expr::TailLoop { body: b, .. }
        | Expr::InlineLoop { body: b }
        | Expr::OptArg { default: b, .. }
        | Expr::KeyArg { default: b, .. }
        | Expr::StoreGlobal { value: b, .. } => f(b),
        Expr::Add(a, b)
        | Expr::Sub(a, b)
        | Expr::Mul(a, b)
        | Expr::Truncate(a, b)
        | Expr::Rem(a, b)
        | Expr::LogAnd(a, b)
        | Expr::LogIor(a, b)
        | Expr::LogXor(a, b)
        | Expr::Ash(a, b)
        | Expr::Cons(a, b)
        | Expr::Eq(a, b)
        | Expr::Lt(a, b)
        | Expr::Gt(a, b)
        | Expr::Le(a, b)
        | Expr::Ge(a, b)
        | Expr::NumEq(a, b)
        | Expr::Equal(a, b)
        | Expr::StringEq(a, b)
        | Expr::StringChar(a, b)
        | Expr::Aref(a, b)
        | Expr::SetCar(a, b)
        | Expr::SetCdr(a, b) => {
            f(a);
            f(b);
        }
        Expr::If(a, b, c) => {
            f(a);
            f(b);
            f(c);
        }
        Expr::SetChar { s, idx, ch } => {
            f(s);
            f(idx);
            f(ch);
        }
        Expr::SetAref { v, idx, val } => {
            f(v);
            f(idx);
            f(val);
        }
        Expr::DynamicBind { value, body, .. } => {
            f(value);
            f(body);
        }
        Expr::FastLoop { test, result, body } => {
            f(test);
            f(result);
            f(body);
        }
        Expr::Let { bindings, body } => {
            for b in bindings {
                f(b);
            }
            f(body);
        }
        Expr::Progn(v) | Expr::Values(v) | Expr::SelfTailNext { args: v } => {
            for x in v {
                f(x);
            }
        }
        Expr::Call { args, .. } => {
            for x in args {
                f(x);
            }
        }
        Expr::Funcall { fn_expr, args } => {
            f(fn_expr);
            for x in args {
                f(x);
            }
        }
        Expr::Apply { fn_expr, prefix, tail } => {
            f(fn_expr);
            for x in prefix {
                f(x);
            }
            f(tail);
        }
        Expr::Lambda { body, captures, .. } => {
            f(body);
            for x in captures {
                f(x);
            }
        }
    }
}
