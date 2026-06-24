//! Lowering: `ncl_runtime::Value` → `ncl_ir::Expr`.
//!
//! Phase 3a recognised fixnum literals and three arithmetic
//! operators. Cons/car/cdr added the first allocating forms.
//! Eq/if/quote added the first conditional. Now defun and function
//! calls land — `lower` takes a `LocalEnv` (parameter scope) and a
//! `GcCoordinator` (for symbol interning) so it can resolve symbol
//! references and emit calls through the symbol's function cell.
//!
//! Top-level forms are lowered with `LocalEnv::empty()`. Function
//! bodies are lowered with a `LocalEnv` that maps each parameter
//! name to its index. A bare symbol resolves locals first, then
//! special-cases `T`; anything else lowering as a function call
//! against an unknown head goes through symbol intern + `Expr::Call`.

use std::collections::HashSet;
use std::sync::Arc;

use ncl_ir::Expr;
use ncl_runtime::{symbol::Symbol, GcCoordinator, Value, Word};

/// Is `s` a keyword? A keyword is a symbol whose home package is
/// `KEYWORD`. The reader does the home-package assignment; the
/// compiler just needs to recognise the result.
pub fn is_keyword(s: &std::sync::Arc<Symbol>) -> bool {
    match &s.home {
        Some(pkg) => &*pkg.name == "KEYWORD",
        None => false,
    }
}

/// Intern a Value-level symbol into the runtime symbol table.
/// Keywords get a `:` prefix on their interned name so the printer
/// can render them as `:FOO` rather than `FOO`. Regular symbols
/// pass through unchanged.
pub fn intern_value_symbol(coord: &GcCoordinator, s: &std::sync::Arc<Symbol>) -> Word {
    if is_keyword(s) {
        let mut prefixed = String::with_capacity(s.name.len() + 1);
        prefixed.push(':');
        prefixed.push_str(&s.name);
        coord.intern(&prefixed)
    } else {
        coord.intern(&s.name)
    }
}

/// A binding kind in the lexical environment.
///
/// The `*Cell` variants represent variables that have been boxed
/// because they are the target of `setq`/`setf`. The slot itself
/// holds a 1-cell cons (init . nil); reads emit `(car slot)` and
/// writes emit `(rplaca slot new)`. Boxing is mandatory for any
/// mutated binding because the same cell may be shared with an
/// inner lambda's closure env — a non-boxed value would diverge
/// across the inner/outer copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// Function parameter — read from the call's args[idx].
    Param(usize),
    /// Function parameter declared `double-float` — read unboxed.
    /// Only assigned to required params that are NOT mutated and NOT
    /// special. See docs/performance-unbox-float.md Sprint 3.
    ParamF64(usize),
    /// Let-bound local — read from the emitter's locals stack[idx].
    Local(usize),
    /// Closure reference — read from env[idx] in the current
    /// function's closure environment.
    ClosureRef(usize),
    /// Function parameter, boxed for mutation. Reserved — the param
    /// boxing prologue isn't wired yet, so mutating a param still
    /// errors at compile time. Present so the binding-kind enum
    /// covers the full design.
    ParamCell(usize),
    /// Let-bound local, boxed for mutation.
    LocalCell(usize),
    /// Let-bound local declared `double-float` — stored unboxed in a
    /// dedicated f64 stack slot (the carried index is the f64-slot, not
    /// a `Local` index). Used for both immutable and (uncaptured)
    /// mutable float locals. See docs/performance-unbox-float.md Sprint 2.
    LocalF64(usize),
    /// Closure-captured cell. The captured Word IS the cons; reads
    /// auto-deref, writes go through SetCar.
    ClosureRefCell(usize),
}

/// Lexical environment for lowering. Tracks names → bindings.
/// Lambda bodies install a `capture_parent` that lookups fall
/// back to: any outer-env name referenced inside the lambda is
/// added to the lambda's captures list and recorded as a new
/// `ClosureRef` binding.
///
/// Lisp-2: variable lookups consult `bindings`; function-call
/// lookups consult `fn_bindings`. flet / labels populate
/// `fn_bindings`; let / lambda params populate `bindings`. A
/// shadow of one namespace doesn't shadow the other.
#[derive(Debug, Clone, Default)]
pub struct LocalEnv {
    bindings: Vec<(Arc<str>, Binding)>,
    /// Function-namespace bindings — local functions established
    /// by flet/labels. Same Binding kinds as variable bindings;
    /// the difference is which namespace's lookup will find them.
    fn_bindings: Vec<(Arc<str>, Binding)>,
    local_count: usize,
    closure_count: usize,
    /// Monotonic per-function counter of unboxed-f64 local slots. NOT
    /// reset by `restore` — each float local across the whole function
    /// gets a unique slot (allocas aren't stack-reused, which keeps the
    /// codegen trivial and avoids aliasing). See docs/performance-
    /// unbox-float.md Sprint 2.
    f64_count: usize,
    /// True while lowering the body of a loop the compiler proved safe
    /// to inline (see `native_loop_inlinable`). In this state a
    /// `(%loop-return v)` lowers to a direct `Expr::LoopBreak` (branch to
    /// the inline loop's exit) instead of the flag-based `%loop-return`
    /// runtime call. Reset to false inside any lambda body (for_lambda).
    in_inline_loop: bool,
    /// True once a `(double-float …)` / `(single-float …)` declaration
    /// has been seen in this function (on a param or an enclosing let).
    /// Auto-inlining of a simple `(loop …)` is gated on this — we only
    /// transform loops in functions that opted into float types, keeping
    /// the blast radius to exactly the code the unboxed-float work
    /// targets and leaving all other loops byte-identical on
    /// `%native-loop`. Reset per function / lambda.
    has_float_decl: bool,
    /// Set when this env is the inner env of a lambda body. Lookups
    /// that miss in `bindings` fall back to here, capturing on hit.
    capture_parent: Option<Box<LocalEnv>>,
    /// Outer-scope expressions corresponding to ClosureRef indices.
    /// Each is evaluated at lambda-creation time and stored in the
    /// closure's env vector.
    captures: Vec<Expr>,
}

impl LocalEnv {
    pub fn empty() -> LocalEnv { LocalEnv::default() }

    pub fn with_params(names: &[Arc<str>]) -> LocalEnv {
        let bindings = names
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::clone(n), Binding::Param(i)))
            .collect();
        LocalEnv {
            bindings,
            fn_bindings: Vec::new(),
            local_count: 0,
            closure_count: 0,
            f64_count: 0,
            in_inline_loop: false,
            has_float_decl: false,
            capture_parent: None,
            captures: Vec::new(),
        }
    }

    /// Build an env for lowering a lambda body. The lambda's own
    /// params are at Param(0..N); names not bound here fall back
    /// to `parent` and become captures (ClosureRef).
    pub fn for_lambda(params: &[Arc<str>], parent: LocalEnv) -> LocalEnv {
        let bindings = params
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::clone(n), Binding::Param(i)))
            .collect();
        LocalEnv {
            bindings,
            fn_bindings: Vec::new(),
            local_count: 0,
            closure_count: 0,
            f64_count: 0,
            // A lambda body is NOT part of any enclosing inline loop —
            // its (return) targets the lambda's own dynamic loop.
            in_inline_loop: false,
            // A lambda is its own function-ish scope; float declarations
            // from the outer function don't apply to its loops.
            has_float_decl: false,
            capture_parent: Some(Box::new(parent)),
            captures: Vec::new(),
        }
    }

    /// Read-only lookup in this env's bindings only. Does NOT
    /// consult capture_parent.
    pub fn find_local(&self, name: &str) -> Option<Binding> {
        self.bindings
            .iter()
            .rev()
            .find(|(n, _)| &**n == name)
            .map(|(_, b)| *b)
    }

    /// Lookup, with capture from `capture_parent` if needed. May
    /// add to `captures` and `bindings` (returning the new
    /// ClosureRef). Used by `lower_in_mut` for symbol resolution.
    ///
    /// When the parent binding is a Cell variant, the captured slot
    /// holds the cons (the box); the inner binding is a
    /// `ClosureRefCell` so reads and writes auto-deref through it.
    ///
    /// Recursive across nested lambdas: if the immediate parent
    /// doesn't have NAME in its own bindings, ask the parent to
    /// capture it from ITS parent. This grows a capture chain so
    /// `(lambda () (lambda () (lambda () outer-var)))` correctly
    /// threads `outer-var` through every intermediate env. Without
    /// the recursion, an inner lambda could only see vars one
    /// scope up — surprising behavior caught when maphash's
    /// nested loops produced "unbound variable: FN".
    pub fn find_or_capture(&mut self, name: &str) -> Option<Binding> {
        if let Some(b) = self.find_local(name) {
            return Some(b);
        }
        let parent = self.capture_parent.as_mut()?;
        let parent_b = parent.find_or_capture(name)?;
        let idx = self.closure_count;
        let (outer_expr, inner_binding) = match parent_b {
            Binding::Param(i) => (Expr::Param(i), Binding::ClosureRef(idx)),
            // A float param is immutable (mutated ones aren't ParamF64),
            // so capture-by-value is sound: snapshot the boxed value
            // (F64ParamRead boxes via coerce in a Word/captures context).
            Binding::ParamF64(i) => (Expr::F64ParamRead(i), Binding::ClosureRef(idx)),
            // A captured float local is immutable here (mutable ones are
            // gated away from LocalF64 when the body has a closure), so
            // snapshot-by-value (the boxed current value) is sound.
            Binding::LocalF64(slot) => (Expr::F64LocalRead(slot), Binding::ClosureRef(idx)),
            Binding::Local(i) => (Expr::Local(i), Binding::ClosureRef(idx)),
            Binding::ClosureRef(i) => (Expr::ClosureRef(i), Binding::ClosureRef(idx)),
            // Cell variants: capture the cons itself; inner sees a Cell.
            Binding::ParamCell(i) => (Expr::Param(i), Binding::ClosureRefCell(idx)),
            Binding::LocalCell(i) => (Expr::Local(i), Binding::ClosureRefCell(idx)),
            Binding::ClosureRefCell(i) => (Expr::ClosureRef(i), Binding::ClosureRefCell(idx)),
        };
        self.captures.push(outer_expr);
        self.bindings.push((Arc::from(name), inner_binding));
        self.closure_count += 1;
        Some(inner_binding)
    }

    /// Compatibility wrapper: callers that don't care about
    /// captures can still use `find`. Same as `find_local`.
    pub fn find(&self, name: &str) -> Option<Binding> {
        self.find_local(name)
    }

    /// Lookup in the function namespace — local-only, no
    /// capture. Used by the call-site dispatcher in lower_in_mut
    /// to decide between a local funcall and a global symbol-cell
    /// call.
    pub fn find_fn_local(&self, name: &str) -> Option<Binding> {
        self.fn_bindings
            .iter()
            .rev()
            .find(|(n, _)| &**n == name)
            .map(|(_, b)| *b)
    }

    /// Function-namespace counterpart of `find_or_capture`. If
    /// NAME isn't bound in this env's fn_bindings, try the
    /// capture parent's fn_bindings recursively; on a hit, add a
    /// closure-ref binding here so the lambda's emitted IR can
    /// reach the value via env[i] at runtime.
    pub fn find_fn_or_capture(&mut self, name: &str) -> Option<Binding> {
        if let Some(b) = self.find_fn_local(name) {
            return Some(b);
        }
        let parent = self.capture_parent.as_mut()?;
        let parent_b = parent.find_fn_or_capture(name)?;
        let idx = self.closure_count;
        let (outer_expr, inner_binding) = match parent_b {
            Binding::Param(i) => (Expr::Param(i), Binding::ClosureRef(idx)),
            Binding::ParamF64(i) => (Expr::F64ParamRead(i), Binding::ClosureRef(idx)),
            Binding::LocalF64(slot) => (Expr::F64LocalRead(slot), Binding::ClosureRef(idx)),
            Binding::Local(i) => (Expr::Local(i), Binding::ClosureRef(idx)),
            Binding::ClosureRef(i) => (Expr::ClosureRef(i), Binding::ClosureRef(idx)),
            Binding::ParamCell(i) => (Expr::Param(i), Binding::ClosureRefCell(idx)),
            Binding::LocalCell(i) => (Expr::Local(i), Binding::ClosureRefCell(idx)),
            Binding::ClosureRefCell(i) => (Expr::ClosureRef(i), Binding::ClosureRefCell(idx)),
        };
        self.captures.push(outer_expr);
        self.fn_bindings.push((Arc::from(name), inner_binding));
        self.closure_count += 1;
        Some(inner_binding)
    }

    /// Add a function-namespace binding pointing at an existing
    /// let-local slot. Used by flet/labels lowering.
    pub fn push_fn_binding(&mut self, name: Arc<str>, binding: Binding) {
        self.fn_bindings.push((name, binding));
    }

    pub fn push_local(&mut self, name: Arc<str>) -> usize {
        let idx = self.local_count;
        self.bindings.push((name, Binding::Local(idx)));
        self.local_count += 1;
        idx
    }

    /// Push a let-binding that needs boxing (because it's a setq /
    /// setf target somewhere in the body, possibly inside a captured
    /// lambda). The slot is a `LocalCell`; the caller is expected to
    /// have already wrapped the binding's init expression in a cons.
    pub fn push_local_cell(&mut self, name: Arc<str>) -> usize {
        let idx = self.local_count;
        self.bindings.push((name, Binding::LocalCell(idx)));
        self.local_count += 1;
        idx
    }

    /// Rebind a required parameter `Param(i)` to `ParamF64(i)` — read
    /// unboxed. Caller must ensure the param is declared double-float,
    /// not mutated, and not special. No-op if the name isn't a plain
    /// `Param` (e.g. already promoted to a cell by mutation analysis).
    /// Record that this function declared a float type (param or local),
    /// enabling auto-inline of its simple `(loop …)`s. See
    /// `has_float_decl` / docs/performance-unbox-float.md.
    pub fn mark_float_decl(&mut self) {
        self.has_float_decl = true;
    }

    pub fn rebind_as_param_f64(&mut self, name: &str) {
        for (n, b) in self.bindings.iter_mut().rev() {
            if &**n == name {
                if let Binding::Param(i) = *b {
                    *b = Binding::ParamF64(i);
                }
                break;
            }
        }
    }

    /// Allocate a fresh (monotonic, function-global) unboxed-f64 local
    /// slot index. Not reused across scopes — see `f64_count`.
    pub fn next_f64_slot(&mut self) -> usize {
        let s = self.f64_count;
        self.f64_count += 1;
        s
    }

    /// Push a `double-float` let-local bound to f64 slot `slot`. It also
    /// consumes a `Local`-vec position (the codegen pushes a dummy Word
    /// there) so the dense `Local(i)` indexing of sibling non-float
    /// bindings is undisturbed; reads go through `F64LocalRead(slot)`.
    pub fn push_local_f64(&mut self, name: Arc<str>, slot: usize) {
        self.bindings.push((name, Binding::LocalF64(slot)));
        self.local_count += 1;
    }

    /// Promote an existing required-parameter binding `Param(i)` to a
    /// `LocalCell`.  The caller is responsible for inserting a
    /// `(cons Param(i) nil)` init expression into the prologue at the
    /// matching local slot index.
    pub fn rebind_as_local_cell(&mut self, name: &str) -> usize {
        let cell_idx = self.local_count;
        // Walk bindings in reverse so the most-recent (innermost)
        // occurrence wins — mirrors `find_local` resolution order.
        for (n, b) in self.bindings.iter_mut().rev() {
            if &**n == name {
                *b = Binding::LocalCell(cell_idx);
                break;
            }
        }
        self.local_count += 1;
        cell_idx
    }

    pub fn checkpoint(&self) -> (usize, usize, usize) {
        (self.bindings.len(), self.local_count, self.fn_bindings.len())
    }

    pub fn restore(&mut self, cp: (usize, usize, usize)) {
        self.bindings.truncate(cp.0);
        self.local_count = cp.1;
        self.fn_bindings.truncate(cp.2);
    }

    /// Take ownership of the captures list. Called after lambda-
    /// body lowering completes, to extract the captures for the
    /// resulting `Expr::Lambda`.
    pub fn take_captures(&mut self) -> Vec<Expr> {
        std::mem::take(&mut self.captures)
    }
}

/// Build the IR that reads a binding's current value. Cell
/// variants dereference via Car. Used for variable lookup
/// (Symbol case in lower_in_mut) and function-call lookup
/// (when a local function bound by flet/labels needs to be
/// funcalled — same shape, different namespace).
fn binding_read(b: Binding) -> Expr {
    match b {
        Binding::Param(i) => Expr::Param(i),
        Binding::ParamF64(i) => Expr::F64ParamRead(i),
        Binding::Local(i) => Expr::Local(i),
        Binding::LocalF64(slot) => Expr::F64LocalRead(slot),
        Binding::ClosureRef(i) => Expr::ClosureRef(i),
        Binding::ParamCell(i) => Expr::car(Expr::Param(i)),
        Binding::LocalCell(i) => Expr::car(Expr::Local(i)),
        Binding::ClosureRefCell(i) => Expr::car(Expr::ClosureRef(i)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompileError {
    NotImplemented(String),
    BadArity { head: String, expected: &'static str, got: usize },
    ImproperList(String),
    /// `(defun)` form at non-top-level, or malformed.
    BadDefun(String),
    /// A condition was signaled while a macro's expander function ran
    /// (e.g. it called an undefined function, or did `(error …)`).
    /// Caught by the macroexpansion guard so it surfaces as a clean
    /// compile error instead of aborting the process.
    MacroError(String),
}

/// Lower a top-level Value to an Expr (no parameter scope).
pub fn lower(v: &Value, coord: &Arc<GcCoordinator>) -> Result<Expr, CompileError> {
    lower_in(v, &LocalEnv::empty(), coord)
}

/// Lower a Value in the given parameter environment.
pub fn lower_in(
    v: &Value,
    env: &LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    let mut env = env.clone();
    lower_in_mut(v, &mut env, coord)
}

/// Internal lowering with a mutable env (so `let` can extend it
/// in place and roll back at scope exit).
fn lower_in_mut(
    v: &Value,
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    match v {
        Value::Fixnum(n) => Ok(Expr::Const(*n)),
        Value::Bignum(s) => {
            let w = ncl_runtime::bignum::alloc_bignum_in_static(
                coord.static_area(), coord, s.as_str(),
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(format!(
                    "static area exhausted while allocating bignum literal {s}"
                ))
            })?;
            Ok(Expr::Word(w.raw()))
        }
        Value::Float(f) => {
            // Pre-box the literal in the static area (the constant-fold
            // target for Word contexts — zero per-eval allocation), and
            // carry the raw f64 bits so the JIT can compute on it
            // unboxed. See docs/performance-unbox-float.md Sprint 1.
            let w = ncl_runtime::float::alloc_float_in_static(
                coord.static_area(), coord, *f,
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(
                    "static area exhausted while allocating float literal".into(),
                )
            })?;
            Ok(Expr::Float { bits: f.to_bits(), boxed: w.raw() })
        }
        Value::Ratio(n, d) => {
            let w = ncl_runtime::ratio::alloc_ratio_in_static(
                coord.static_area(), coord, n.as_str(), d.as_str(),
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(format!(
                    "static area exhausted while allocating ratio literal {n}/{d}"
                ))
            })?;
            Ok(Expr::Word(w.raw()))
        }
        Value::Vector(items) => {
            // Allocate a Vector heap object in static and fill each
            // cell with the lowered word of each item. Items must
            // be quotable — fixnums, bignums, chars, strings,
            // symbols, conses, nested vectors — i.e., values that
            // build_quoted_word handles.
            let w = lower_vector_literal(items, coord)?;
            Ok(Expr::Word(w.raw()))
        }
        Value::Nil => Ok(Expr::Nil),
        Value::Char(c) => Ok(Expr::Word(ncl_runtime::Word::char(*c).raw())),
        Value::String(s) => {
            let w = ncl_runtime::gc_string::alloc_string_in_static(
                coord.static_area(),
                s.as_str(),
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(
                    "static area exhausted while allocating string literal".into(),
                )
            })?;
            Ok(Expr::Word(w.raw()))
        }
        Value::Symbol(s) => {
            // Keyword check FIRST. Without this, a local variable
            // named `test` (stored under name "TEST") would shadow
            // the keyword `:test` (whose unprefixed name is also
            // "TEST"), so `(call :test test)` inside `(defun foo
            // (test) ...)` would compile both args as references
            // to the local. A real subtle bug — caught while
            // rewriting list helpers to use :test/:key keyword
            // args. Keywords are package-qualified to KEYWORD;
            // we trust that distinction over the env's name match.
            if is_keyword(s) {
                return Ok(Expr::Word(intern_value_symbol(coord, s).raw()));
            }
            // Local (param/let/closure-capture), then T, then
            // global value-cell load.
            if let Some(b) = env.find_or_capture(&s.name) {
                Ok(binding_read(b))
            } else if &*s.name == "T" {
                Ok(Expr::True)
            } else {
                let sym_word = coord.intern(&s.name);
                Ok(Expr::load_global(sym_word.raw()))
            }
        }
        Value::Cons(_) => lower_call_in_mut(v, env, coord),
        other => Err(CompileError::NotImplemented(format!("{other:?}"))),
    }
}

/// Build a Vector-tagged Word in the static area whose cells are
/// the quoted-word version of each item. Used for `#(...)` literals
/// (Value::Vector). Items are recursively built via
/// `build_quoted_word`, so nested literal vectors / quoted lists /
/// numbers / strings all work.
fn lower_vector_literal(
    items: &std::sync::Arc<Vec<Value>>,
    coord: &Arc<GcCoordinator>,
) -> Result<Word, CompileError> {
    let n = items.len() as u32;
    let header_ptr = coord
        .static_area()
        .try_alloc_with_header(ncl_runtime::HeapType::Vector, n)
        .ok_or_else(|| {
            CompileError::NotImplemented(
                "static area exhausted while allocating vector literal".into(),
            )
        })?;
    let p = header_ptr.as_ptr() as *mut u64;
    for (i, item) in items.iter().enumerate() {
        let w = build_quoted_word(item, coord)?;
        unsafe {
            *p.add(1 + i) = w.raw();
        }
    }
    Ok(ncl_runtime::Word::from_ptr(
        p as *const u8,
        ncl_runtime::Tag::Vector,
    ))
}

/// Lower a quoted form `(quote x)`. Symbols are interned in the
/// coordinator's static area (returning a stable Symbol-tagged
/// Word). Cons-shaped data is built recursively in static at
/// compile time — each cons cell allocated via
/// `static_area.try_alloc_cons`. This means a `'(1 2 3)` literal
/// lives forever and is shared across all references; no GC
/// pressure, and `(eq '(1 2 3) '(1 2 3))` may even be true if the
/// compiler shares them (it doesn't yet, but could).
fn lower_quoted(v: &Value, coord: &Arc<GcCoordinator>) -> Result<Expr, CompileError> {
    let word = build_quoted_word(v, coord)?;
    // Specialise the IR variant where it's cleaner; otherwise emit
    // a raw Word constant.
    Ok(match word {
        w if w.is_nil() => Expr::Nil,
        w if w.is_t() => Expr::True,
        w if w.is_fixnum() => Expr::Const(w.as_fixnum().unwrap()),
        w => Expr::Word(w.raw()),
    })
}

/// Build a Word for a quoted Value, allocating compound structure
/// in the coordinator's static area as needed. Recurses into cons
/// cells.
///
/// Also used by `read-from-string` to materialise a reader Value
/// into a runtime Word.
pub fn build_quoted_word(
    v: &Value,
    coord: &Arc<GcCoordinator>,
) -> Result<Word, CompileError> {
    match v {
        Value::Fixnum(n) => Ok(Word::fixnum(*n)),
        Value::Bignum(s) => {
            ncl_runtime::bignum::alloc_bignum_in_static(
                coord.static_area(), coord, s.as_str(),
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(format!(
                    "static area exhausted while allocating quoted bignum {s}"
                ))
            })
        }
        Value::Float(f) => {
            ncl_runtime::float::alloc_float_in_static(
                coord.static_area(), coord, *f,
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(
                    "static area exhausted while allocating quoted float".into(),
                )
            })
        }
        Value::Ratio(n, d) => {
            ncl_runtime::ratio::alloc_ratio_in_static(
                coord.static_area(), coord, n.as_str(), d.as_str(),
            )
            .ok_or_else(|| {
                CompileError::NotImplemented(format!(
                    "static area exhausted while allocating quoted ratio {n}/{d}"
                ))
            })
        }
        Value::Vector(items) => lower_vector_literal(items, coord),
        Value::Nil => Ok(Word::NIL),
        Value::Symbol(s) if &*s.name == "T" => Ok(Word::T),
        Value::Symbol(s) => Ok(intern_value_symbol(coord, s)),
        Value::Cons(c) => {
            let car_word = build_quoted_word(&c.car, coord)?;
            let cdr_word = build_quoted_word(&c.cdr, coord)?;
            coord
                .static_area()
                .try_alloc_cons(car_word, cdr_word)
                .ok_or_else(|| {
                    CompileError::NotImplemented(
                        "static area exhausted while building quoted list".into(),
                    )
                })
        }
        Value::Char(c) => Ok(Word::char(*c)),
        Value::String(s) => ncl_runtime::gc_string::alloc_string_in_static(
            coord.static_area(),
            s.as_str(),
        )
        .ok_or_else(|| {
            CompileError::NotImplemented(
                "static area exhausted while allocating quoted string".into(),
            )
        }),
        other => Err(CompileError::NotImplemented(format!(
            "quoted sub-value: {other:?}"
        ))),
    }
}

fn lower_call_in_mut(
    v: &Value,
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    let items = list_to_vec(v)?;
    let head_name = match items.first() {
        Some(Value::Symbol(s)) => s.name.to_string(),
        Some(other) => {
            return Err(CompileError::NotImplemented(format!(
                "non-symbol head: {other:?}"
            )));
        }
        None => return Err(CompileError::NotImplemented("empty list".into())),
    };
    let args = &items[1..];

    match head_name.as_str() {
        "+" => fold_arithmetic(&head_name, args, env, coord, 0, Expr::add),
        "*" => fold_arithmetic(&head_name, args, env, coord, 1, Expr::mul),
        // Bitwise ops inline to dedicated nodes (raw tagged-fixnum op +
        // bignum-aware slow path). Integers only, so — unlike mod /
        // truncate / rem — they need no polymorphic Lisp wrapper.
        // Variadic: identity is -1 for logand, 0 for logior/logxor.
        "LOGAND" => fold_arithmetic(&head_name, args, env, coord, -1, Expr::logand),
        "LOGIOR" => fold_arithmetic(&head_name, args, env, coord, 0, Expr::logior),
        "LOGXOR" => fold_arithmetic(&head_name, args, env, coord, 0, Expr::logxor),
        "ASH" => binary_op(&head_name, args, env, coord, Expr::ash),
        // TRUNCATE and REM used to be special-form-intercepted and
        // lowered to inline LLVM srem / sdiv. We demoted them to
        // ordinary native calls (truncate_shim / rem_shim in
        // ncl-runtime::bignum) so that Library/numbers.lisp can
        // override them with polymorphic, multi-value-returning
        // versions. The fast int-int path still happens — it just
        // goes through one shim call instead of inline IR.
        "QUOTE" => {
            if args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "1",
                    got: args.len(),
                });
            }
            lower_quoted(&args[0], coord)
        }
        "LIST" => {
            // (list a b c) ≡ (cons a (cons b (cons c nil)))
            // Pure source-level desugaring.
            let mut result = Expr::Nil;
            for arg in args.iter().rev() {
                let lowered = lower_in_mut(arg, env, coord)?;
                result = Expr::cons(lowered, result);
            }
            Ok(result)
        }
        "LAMBDA" => lower_lambda(args, env, coord),
        "FUNCALL" => lower_funcall(args, env, coord),
        // (apply fn arg1 ... argN tail-list) — last arg is the
        // splatted list; everything between fn and the last arg
        // is the prefix. Requires at least 2 args.
        "APPLY" => lower_apply(args, env, coord),
        // (function name)  — equivalent to #'name in source. Loads
        // the symbol's function cell as a first-class value.
        "FUNCTION" => {
            if args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "1",
                    got: args.len(),
                });
            }
            // Two cases per CL: (function SYMBOL) loads the
            // symbol's function cell as a value; (function
            // (lambda …)) is just a lambda form value, the
            // shape `#'(lambda …)` reads as. Closette uses
            // both — generated emfun bodies do
            // `(apply #'(lambda (kludge-arglist) …) args)`.
            // Local fn-namespace bindings (flet/labels) also
            // resolve here.
            match &args[0] {
                Value::Symbol(s) => {
                    if let Some(b) = env.find_fn_or_capture(&s.name) {
                        Ok(binding_read(b))
                    } else {
                        let sym_word = coord.intern(&s.name);
                        Ok(Expr::load_function(sym_word.raw()))
                    }
                }
                Value::Cons(c) => {
                    // Expect (lambda (params...) body...).
                    let Value::Symbol(head) = &c.car else {
                        return Err(CompileError::NotImplemented(format!(
                            "(function …) form must be a lambda, got head {:?}",
                            c.car
                        )));
                    };
                    if &*head.name != "LAMBDA" {
                        return Err(CompileError::NotImplemented(format!(
                            "(function …) form must be (lambda …), got ({} …)",
                            head.name
                        )));
                    }
                    let lambda_args = list_to_vec(&c.cdr)?;
                    lower_lambda(&lambda_args, env, coord)
                }
                other => Err(CompileError::NotImplemented(format!(
                    "(function …) requires a symbol or (lambda …) form, got {other:?}"
                ))),
            }
        }
        // (setq name value)  — assign the value cell of `name`. CL
        // also allows (setq a 1 b 2 …) for parallel assignments;
        // v1 supports the binary form only.
        "SETQ" => lower_setq(&head_name, args, env, coord),
        // (setf place value)  — generalised assignment. We dispatch
        // by syntactic shape of `place`: a bare symbol falls through
        // to setq; (car x) / (cdr x) / (aref s i) / (char s i)
        // emit the matching mutation primitive.
        "SETF" => lower_setf(args, env, coord),
        // (defparameter name value [doc])
        // (defvar name value [doc])
        // For v1 both behave like setq — they install/replace the
        // value cell and ignore the optional doc string. The
        // distinction (defvar only sets if unbound) lands later.
        "DEFPARAMETER" | "DEFVAR" => lower_defparameter(&head_name, args, env, coord),
        "EQ" => {
            if args.len() != 2 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "2",
                    got: args.len(),
                });
            }
            Ok(Expr::eq(
                lower_in_mut(&args[0], env, coord)?,
                lower_in_mut(&args[1], env, coord)?,
            ))
        }
        "IF" => match args.len() {
            2 => Ok(Expr::if_(
                lower_in_mut(&args[0], env, coord)?,
                lower_in_mut(&args[1], env, coord)?,
                Expr::Nil,
            )),
            3 => Ok(Expr::if_(
                lower_in_mut(&args[0], env, coord)?,
                lower_in_mut(&args[1], env, coord)?,
                lower_in_mut(&args[2], env, coord)?,
            )),
            n => Err(CompileError::BadArity {
                head: head_name,
                expected: "2 or 3",
                got: n,
            }),
        },
        "CONS" => {
            if args.len() != 2 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "2",
                    got: args.len(),
                });
            }
            Ok(Expr::cons(
                lower_in_mut(&args[0], env, coord)?,
                lower_in_mut(&args[1], env, coord)?,
            ))
        }
        "CAR" => {
            if args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "1",
                    got: args.len(),
                });
            }
            Ok(Expr::car(lower_in_mut(&args[0], env, coord)?))
        }
        "CDR" => {
            if args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "1",
                    got: args.len(),
                });
            }
            Ok(Expr::cdr(lower_in_mut(&args[0], env, coord)?))
        }
        "-" => match args.len() {
            0 => Err(CompileError::BadArity {
                head: head_name,
                expected: "at least 1",
                got: 0,
            }),
            1 => {
                let x = lower_in_mut(&args[0], env, coord)?;
                Ok(Expr::sub(Expr::Const(0), x))
            }
            _ => {
                let mut acc = lower_in_mut(&args[0], env, coord)?;
                for a in &args[1..] {
                    acc = Expr::sub(acc, lower_in_mut(a, env, coord)?);
                }
                Ok(acc)
            }
        },
        "PROGN" => {
            if args.is_empty() {
                return Ok(Expr::Nil);
            }
            let lowered: Result<Vec<_>, _> =
                args.iter().map(|a| lower_in_mut(a, env, coord)).collect();
            Ok(Expr::progn(lowered?))
        }
        // Boolean operators desugar to existing IR (if + let). No
        // new Expr variants, no LLVM changes — pure lowering.
        "NOT" => {
            if args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: head_name,
                    expected: "1",
                    got: args.len(),
                });
            }
            // (not x) ≡ (if x nil t)
            Ok(Expr::if_(
                lower_in_mut(&args[0], env, coord)?,
                Expr::Nil,
                Expr::True,
            ))
        }
        "AND" => lower_and(args, env, coord),
        "OR" => lower_or(args, env, coord),
        "COND" => lower_cond(args, env, coord),
        // (when test body...) ≡ (if test (progn body...) nil)
        // (unless test body...) ≡ (if test nil (progn body...))
        // Implicit progn over body forms; empty body yields nil.
        "WHEN" => lower_when(&head_name, args, env, coord, /*invert*/ false),
        "UNLESS" => lower_when(&head_name, args, env, coord, /*invert*/ true),
        // Numeric comparisons. Binary only for now; CL allows
        // variadic with chained semantics ((< 1 2 3) = (and …))
        // — that lands when `and` does.
        "<" => chainable_cmp(&head_name, args, env, coord, Expr::lt),
        ">" => chainable_cmp(&head_name, args, env, coord, Expr::gt),
        "<=" => chainable_cmp(&head_name, args, env, coord, Expr::le),
        ">=" => chainable_cmp(&head_name, args, env, coord, Expr::ge),
        "=" => chainable_cmp(&head_name, args, env, coord, Expr::num_eq),
        // Type predicates. Each takes one argument.
        "NULL" => unary_op(&head_name, args, env, coord, Expr::is_null),
        "CONSP" => unary_op(&head_name, args, env, coord, Expr::is_cons),
        "ATOM" => unary_op(&head_name, args, env, coord, Expr::is_atom),
        "LISTP" => unary_op(&head_name, args, env, coord, Expr::is_listp),
        // EQL is NOT inlined like EQ. EQ is object identity (a single
        // word compare); EQL additionally treats two numbers of the
        // same type and value as equal (floats/bignums/ratios/complex
        // are boxed, so equal-valued instances are distinct objects).
        // That value comparison lives in `eql_shim`, so EQL falls
        // through to an ordinary call into it (see the default arm).
        "LENGTH" => unary_op(&head_name, args, env, coord, Expr::length),
        "EQUAL" => binary_op(&head_name, args, env, coord, Expr::equal),
        "STRING=" => binary_op(&head_name, args, env, coord, Expr::string_eq),
        // (char s i) / (string-char s i) — string-specialized read.
        "CHAR" | "STRING-CHAR" => {
            binary_op(&head_name, args, env, coord, Expr::string_char)
        }
        // (aref v i) / (svref v i) — polymorphic read; runtime
        // tag-dispatches between strings and vectors.
        "AREF" | "SVREF" => {
            binary_op(&head_name, args, env, coord, Expr::aref)
        }
        "LET" => lower_let(args, env, coord),
        "FAST-LOOP" => lower_fast_loop(args, env, coord),
        // Auto-inline a simple `(loop …)` (which macroexpanded to
        // `(%native-loop (lambda () body))`) when the compiler can prove
        // it safe — body calls no user functions and creates no closures
        // (native_loop_inlinable). Then loop carries stay unboxed / in
        // registers, exactly like `fast-loop`, with no source change. If
        // the guard fails, control falls through to the ordinary call
        // lowering (the catch-all), keeping the loop on %native-loop.
        "%NATIVE-LOOP"
            if env.has_float_decl
                && args.len() == 1
                && native_loop_lambda_body(&args[0])
                    .map_or(false, |b| native_loop_inlinable(&b, coord, Some(&*env))) =>
        {
            lower_native_loop_inline(&args[0], env, coord)
        }
        // `(return v)` inside an inlined loop body → a direct break.
        // Outside one, it falls through to the ordinary %loop-return
        // call (flag-based), preserving its dynamic semantics.
        "%LOOP-RETURN" if env.in_inline_loop => {
            let v = match args.first() {
                Some(a) => lower_in_mut(a, env, coord)?,
                None => Expr::Nil,
            };
            Ok(Expr::LoopBreak { value: Box::new(v) })
        }
        "FLET" => lower_flet(args, env, coord),
        "LABELS" => lower_labels(args, env, coord),
        // `(values v1 v2 ... vN)` — write all into the multi-value
        // slot, return `vals[0]` (or NIL). In tail position the
        // surrounding function's transform leaves this alone, so
        // multiple-value-bind sees the full set. In non-tail
        // position the slot is overwritten by the next call's
        // exit-time `EnsureSingleMv`, but the primary still flows
        // through as the form's value.
        "VALUES" => {
            let lowered: Result<Vec<_>, _> = args
                .iter()
                .map(|a| lower_in_mut(a, env, coord))
                .collect();
            Ok(Expr::values(lowered?))
        }
        "DEFUN" => lower_nested_defun(args, env, coord),
        // (declare ...) — declaration forms are processed at
        // let/lambda/defun lowering time, not evaluated. A bare
        // `declare` that somehow reaches evaluation (e.g. at top
        // level) is silently ignored per CL spec.
        "DECLARE" => Ok(Expr::Nil),
        // (declaim (inline f) (notinline g) (optimize …) …) — global
        // proclamation. We honour `inline`/`notinline` (the inliner reads
        // the registry at defun + call-site lowering); all other declspecs
        // (optimize / type / ftype / special …) are accepted and ignored.
        // declaim's return value is unspecified in CL; we return T ("the
        // declamation was accepted"), matching the de-facto convention the
        // ANSI suite annotates. See crate::inline (§ Interprocedural inlining).
        "DECLAIM" => {
            process_declaim(args);
            Ok(Expr::True)
        }
        // (locally (declare (special ...)) body...) — establishes
        // local special declarations for the enclosed body. The
        // declares affect only the lexical scope of `body`.
        "LOCALLY" => lower_locally(args, env, coord),
        // (proclaim '(special ...)) — globally proclaim variables
        // as special (dynamically scoped). Affects all future
        // compilations in this session.
        "PROCLAIM" => lower_proclaim(args, env, coord),
        // (unwind-protect protected cleanup...) — ensure cleanup
        // runs even on non-local exit. Lowers to
        // (%native-unwind-protect #'(lambda () protected)
        //                          #'(lambda () cleanup...))
        "UNWIND-PROTECT" => lower_unwind_protect(args, env, coord),
        // Unknown head: it's a function call. First check the
        // Lisp-2 function namespace — if NAME is bound by an
        // enclosing flet/labels, emit Funcall on the local; else
        // fall through to a global symbol-cell call.
        _ if env.find_fn_or_capture(&head_name).is_some() => {
            // Re-borrow because the is_some check moved out of
            // env.borrow above; this find returns the Binding now.
            let b = env.find_fn_or_capture(&head_name).unwrap();
            let fn_expr = binding_read(b);
            let lowered_args: Result<Vec<_>, _> = args
                .iter()
                .map(|a| lower_in_mut(a, env, coord))
                .collect();
            Ok(Expr::funcall(fn_expr, lowered_args?))
        }
        _ => {
            // Inline-expansion fast path: if NAME was `(declaim (inline …))`d
            // with a simple lambda list and this call is inlinable here, splice
            // its body as `(let ((p a) …) body…)` and lower that in-place. The
            // reachable-via-flet case was handled above, so NAME here is a
            // global function reference. See crate::inline.
            if let Some(result) = try_lower_inline_call(&head_name, args, env, coord) {
                return result;
            }
            let sym_word = coord.intern(&head_name);
            let lowered_args: Result<Vec<_>, _> = args
                .iter()
                .map(|a| lower_in_mut(a, env, coord))
                .collect();
            Ok(Expr::call(sym_word.raw(), lowered_args?))
        }
    }
}

/// Process `(declaim DECLSPEC …)` declspecs we care about. `inline`/
/// `notinline` update the inline registry; everything else is ignored.
fn process_declaim(args: &[Value]) {
    for spec in args {
        let Ok(items) = list_to_vec(spec) else { continue };
        let Some(Value::Symbol(head)) = items.first() else {
            continue;
        };
        match &*head.name {
            "INLINE" => {
                for n in &items[1..] {
                    if let Value::Symbol(s) = n {
                        crate::inline::request_inline(&s.name);
                    }
                }
            }
            "NOTINLINE" => {
                for n in &items[1..] {
                    if let Value::Symbol(s) = n {
                        crate::inline::request_notinline(&s.name);
                    }
                }
            }
            // optimize / type / ftype / special / ignore … — not our concern.
            _ => {}
        }
    }
}

/// If `head_name` names a `declaim inline` function whose captured body can be
/// spliced at this call site, build `(let ((p1 a1) …) body…)` and lower it in
/// the caller's env; otherwise return `None` (the caller emits a normal call).
///
/// The `let` reuses all of `lower_let`'s machinery: parallel left-to-right
/// arg evaluation, boxing of mutated params, dynamic binding of special-named
/// params, and f64-slot promotion — and the float-unboxing pass later runs
/// over the spliced body, so unboxing crosses the call boundary. Params are
/// bound by the `let`, so they shadow same-named caller locals correctly.
fn try_lower_inline_call(
    head_name: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Option<Result<Expr, CompileError>> {
    if crate::inline::blocked(head_name) {
        return None;
    }
    let def = crate::inline::lookup(head_name)?;
    if def.params.len() != args.len() {
        // Arity mismatch — fall through to a normal call, which signals the
        // proper condition at runtime rather than silently mis-substituting.
        return None;
    }

    // Build the binding list `((p1 a1) (p2 a2) …)`. Each param becomes a fresh
    // symbol carrying the original name; `lower_let` resolves the body's
    // references by name, so identity doesn't matter. Args are the ORIGINAL
    // call-site forms — `lower_let` lowers them in the caller env (correct
    // outer-scope, parallel-let evaluation).
    let mut bindings = Vec::with_capacity(def.params.len());
    for (p, a) in def.params.iter().zip(args.iter()) {
        let psym = Value::Symbol(Symbol::fresh_uninterned(Arc::clone(p)));
        bindings.push(Value::list([psym, a.clone()]));
    }
    let mut let_args = Vec::with_capacity(1 + def.body.len());
    let_args.push(Value::list(bindings));
    let_args.extend(def.body.iter().cloned());

    crate::inline::enter(head_name);
    let result = lower_let(&let_args, env, coord);
    crate::inline::exit();
    Some(result)
}

/// `(let ((var val) ...) body...)`. Bindings evaluated in OUTER env
/// (parallel let, not let*); body sees them as new Locals. Multiple
/// body forms are wrapped in implicit progn.
/// Walk a Value form and collect every name that appears as the
/// target of a mutating special form. The `shadowed` set records
/// names that have been re-bound by an enclosing scope inside the
/// form being walked — those don't refer to the outer binding so
/// their mutations don't count.
///
/// Recognised mutators:
///   - `(setq sym v)`              — sym is a target
///   - `(setf sym v)`              — sym is a target (place is a symbol)
///
/// Place-form setfs like `(setf (car x) v)` are NOT counted: those
/// mutate the *contents* of x, not the binding x itself.
///
/// Recognised scoping forms (these extend `shadowed`):
///   - `(let ((n1 v1) ...) body...)`
///   - `(lambda (params) body...)`
///   - `(defun name (params) body...)`
///
/// `(quote ...)` is skipped — quoted forms aren't evaluated.
fn collect_mutations(
    form: &Value,
    shadowed: &HashSet<Arc<str>>,
    into: &mut HashSet<Arc<str>>,
) {
    let items = match form {
        Value::Cons(_) => match list_to_vec(form) {
            Ok(v) => v,
            Err(_) => return, // improper list — nothing to analyse
        },
        _ => return, // atoms have no mutating forms inside
    };
    let head_name = match items.first() {
        Some(Value::Symbol(s)) => s.name.to_string(),
        _ => {
            for it in &items {
                collect_mutations(it, shadowed, into);
            }
            return;
        }
    };
    let args = &items[1..];

    match head_name.as_str() {
        "QUOTE" => {}
        "SETQ" if args.len() % 2 == 0 => {
            // (setq v1 e1 v2 e2 ...) — record each var and recurse
            // into each value form. Walk in pairs.
            let mut i = 0;
            while i + 1 < args.len() {
                if let Value::Symbol(s) = &args[i] {
                    if !shadowed.contains(&s.name) {
                        into.insert(Arc::clone(&s.name));
                    }
                }
                collect_mutations(&args[i + 1], shadowed, into);
                i += 2;
            }
        }
        "SETF" if args.len() % 2 == 0 => {
            // (setf p1 v1 p2 v2 ...) — for each (place, value):
            // if place is a bare symbol, record it as mutated;
            // otherwise recurse into the place sub-args. Then
            // recurse into the value form.
            let mut i = 0;
            while i + 1 < args.len() {
                if let Value::Symbol(s) = &args[i] {
                    if !shadowed.contains(&s.name) {
                        into.insert(Arc::clone(&s.name));
                    }
                } else {
                    collect_mutations(&args[i], shadowed, into);
                }
                collect_mutations(&args[i + 1], shadowed, into);
                i += 2;
            }
        }
        // Macros that expand to (setf place ...).  The first argument
        // is the place being mutated; if it's a bare symbol, record it.
        "INCF" | "DECF" => {
            if let Some(Value::Symbol(s)) = args.first() {
                if !shadowed.contains(&s.name) {
                    into.insert(Arc::clone(&s.name));
                }
            }
            for it in args {
                collect_mutations(it, shadowed, into);
            }
        }
        "PUSH" if args.len() >= 2 => {
            // (push item place) — place is the second arg
            if let Value::Symbol(s) = &args[1] {
                if !shadowed.contains(&s.name) {
                    into.insert(Arc::clone(&s.name));
                }
            }
            for it in args {
                collect_mutations(it, shadowed, into);
            }
        }
        "POP" if !args.is_empty() => {
            if let Value::Symbol(s) = &args[0] {
                if !shadowed.contains(&s.name) {
                    into.insert(Arc::clone(&s.name));
                }
            }
        }
        "LET" if !args.is_empty() => {
            if let Ok(bindings) = list_to_vec(&args[0]) {
                let mut new_shadowed = shadowed.clone();
                for binding in &bindings {
                    if let Ok(pair) = list_to_vec(binding) {
                        if pair.len() == 2 {
                            // Init expr sees OUTER scope.
                            collect_mutations(&pair[1], shadowed, into);
                            if let Value::Symbol(s) = &pair[0] {
                                new_shadowed.insert(Arc::clone(&s.name));
                            }
                        }
                    }
                }
                for body_form in &args[1..] {
                    collect_mutations(body_form, &new_shadowed, into);
                }
            }
        }
        "LAMBDA" if !args.is_empty() => {
            let mut new_shadowed = shadowed.clone();
            if let Ok(params) = list_to_vec(&args[0]) {
                for param in &params {
                    if let Value::Symbol(s) = param {
                        new_shadowed.insert(Arc::clone(&s.name));
                    }
                }
            }
            for body_form in &args[1..] {
                collect_mutations(body_form, &new_shadowed, into);
            }
        }
        "DEFUN" if args.len() >= 2 => {
            // (defun name (params) body...). Params shadow; defun's
            // own name does not (it's a global side effect, not a
            // lexical binding).
            let mut new_shadowed = shadowed.clone();
            if let Ok(params) = list_to_vec(&args[1]) {
                for param in &params {
                    if let Value::Symbol(s) = param {
                        new_shadowed.insert(Arc::clone(&s.name));
                    }
                }
            }
            for body_form in &args[2..] {
                collect_mutations(body_form, &new_shadowed, into);
            }
        }
        _ => {
            for it in args {
                collect_mutations(it, shadowed, into);
            }
        }
    }
}

/// Run `collect_mutations` over a slice of body forms, returning the
/// union. Each form is analysed with the same starting shadowed set
/// (no carry-over since these are siblings, not nested).
pub fn mutated_in_body(
    body_forms: &[Value],
    shadowed: &HashSet<Arc<str>>,
) -> HashSet<Arc<str>> {
    let mut out = HashSet::new();
    for f in body_forms {
        collect_mutations(f, shadowed, &mut out);
    }
    out
}

/// True if any form (recursively) creates a closure — a `lambda`,
/// `flet`, or `labels`. `#'(lambda …)` is found by recursion through
/// the `function` form; `#'symbol` is not a closure. Used to gate
/// unboxing of *mutable* float locals: a mutable float local kept in an
/// f64 stack slot cannot be shared by reference with a capturing
/// closure, so when a closure is present we fall back to the boxed
/// (cons-cell) representation. Immutable float locals are always safe
/// (captured by value). See docs/performance-unbox-float.md Sprint 2.
fn form_has_closure(form: &Value, coord: &Arc<GcCoordinator>) -> bool {
    let Ok(items) = list_to_vec(form) else { return false };
    if let Some(Value::Symbol(s)) = items.first() {
        match &*s.name {
            "LAMBDA" | "FLET" | "LABELS" => return true,
            "QUOTE" => return false,
            // (%native-loop (lambda () body…)). When the compiler will
            // inline this loop (its body calls no user fns / makes no
            // closures), the lambda evaporates — it is NOT a real
            // closure, so an enclosing float loop var stays unboxable.
            // When NOT inlinable, the lambda captures → it is a closure.
            "%NATIVE-LOOP" => {
                if let Some(body) = items.get(1).and_then(native_loop_lambda_body) {
                    if native_loop_inlinable(&body, coord, None) {
                        return false;
                    }
                }
                return true;
            }
            _ => {}
        }
    }
    items.iter().any(|f| form_has_closure(f, coord))
}

fn body_has_closure(body_forms: &[Value], coord: &Arc<GcCoordinator>) -> bool {
    body_forms.iter().any(|f| form_has_closure(f, coord))
}

/// Extract the body forms of a `(%native-loop ARG)`'s thunk: ARG must be
/// `(lambda () body…)` with an empty parameter list. Returns the body
/// forms, or None if ARG isn't a nullary lambda.
fn native_loop_lambda_body(arg: &Value) -> Option<Vec<Value>> {
    let items = list_to_vec(arg).ok()?;
    match items.first() {
        Some(Value::Symbol(s)) if &*s.name == "LAMBDA" => {
            let params_empty = match items.get(1) {
                Some(Value::Nil) => true,
                Some(v) => list_to_vec(v).map(|p| p.is_empty()).unwrap_or(false),
                None => false,
            };
            if params_empty { Some(items[2..].to_vec()) } else { None }
        }
        _ => None,
    }
}

/// Conservatively decide whether a `(loop …)` body (the forms inside its
/// `(lambda () …)`) is safe to lower as an inline loop. Safe means: no
/// closure-creating or non-local-control form, and every operator is a
/// special form or a *native primitive* — NOT a Lisp-compiled user
/// function. That guarantees the only way out is a *lexical* `(return)`
/// (lowered to a direct break) and rules out a *dynamic* `%loop-return`
/// or a signal/return-from that the inline form (which has no lambda
/// boundary for the ABORT_PENDING unwind to land on) can't model. A
/// head we don't recognise falls to the conservative default (treat as
/// a user call → not inlinable). See docs/performance-unbox-float.md.
fn native_loop_inlinable(
    body_forms: &[Value],
    coord: &Arc<GcCoordinator>,
    env: Option<&LocalEnv>,
) -> bool {
    body_forms.iter().all(|f| form_inline_safe(f, coord, env))
}

/// True iff VALUE is provably a `double-float` by the SAME rules the
/// `optimize` representation-inference pass uses to promote a local to an
/// unboxed f64 slot: a float literal, an in-scope declared-`double-float`
/// variable (`ParamF64`/`LocalF64` — immutable floats), or `+`/`-`/`*` of
/// provably-float operands (CL contagion). This is a *strict subset* of
/// what the pass proves float, which is what makes the inline-gate
/// relaxation below sound: anything this accepts, the pass will promote to
/// an f64 slot, so the would-be per-iteration box never reaches codegen.
fn value_provably_float(v: &Value, env: Option<&LocalEnv>) -> bool {
    match v {
        Value::Float(_) => true,
        Value::Symbol(s) => env.is_some_and(|e| {
            matches!(e.find(&s.name), Some(Binding::ParamF64(_) | Binding::LocalF64(_)))
        }),
        Value::Cons(_) => list_to_vec(v).map_or(false, |items| {
            matches!(
                items.first(),
                Some(Value::Symbol(h)) if matches!(&*h.name, "+" | "-" | "*")
            ) && items.len() >= 2
                && items[1..].iter().all(|a| value_provably_float(a, env))
        }),
        _ => false,
    }
}

fn form_inline_safe(form: &Value, coord: &Arc<GcCoordinator>, env: Option<&LocalEnv>) -> bool {
    let items = match form {
        Value::Cons(_) => match list_to_vec(form) {
            Ok(v) => v,
            Err(_) => return false, // improper list — bail conservatively
        },
        _ => return true, // atom (symbol / number / string / char / nil)
    };
    if items.is_empty() {
        return true; // () == nil
    }
    let Value::Symbol(head) = &items[0] else {
        return false; // computed / ((lambda …) …) head
    };
    let h = &*head.name;
    // Closures, nested loops, and non-local control are never inlinable.
    const BAD: &[&str] = &[
        "LAMBDA", "FLET", "LABELS", "FUNCTION",
        "LOOP", "%NATIVE-LOOP", "DO", "DO*", "DOTIMES", "DOLIST",
        "BLOCK", "RETURN-FROM", "%NATIVE-BLOCK", "CATCH", "THROW",
        "UNWIND-PROTECT", "%NATIVE-UNWIND-PROTECT", "HANDLER-CASE",
        "HANDLER-BIND", "TAGBODY", "GO", "MULTIPLE-VALUE-CALL",
        "MULTIPLE-VALUE-BIND", "MULTIPLE-VALUE-PROG1",
    ];
    if BAD.iter().any(|b| *b == h) {
        return false;
    }
    match h {
        "QUOTE" => true,   // quoted data — not code
        "DECLARE" => true, // a no-op type/ignore declaration — not code
        "THE" => items.get(2).map_or(true, |e| form_inline_safe(e, coord, env)),
        "LET" | "LET*" => {
            // A binding with a COMPUTED (cons) initialiser that is NOT
            // declared `double-float` would box its (possibly f64) result
            // into a Word local *every iteration* — and a per-iteration
            // heap box inside an inlined loop is the corruption source
            // (an undeclared float temp register-resident at a GC isn't
            // precisely rooted). Refuse to inline such a loop; it falls
            // back to the ordinary boxed loop macro, which is correct
            // (just slower). Atom/literal inits (e.g. `(n 0)`, `(x y)`)
            // allocate nothing per iteration, and a computed init that IS
            // declared double-float lands in an unboxed f64 slot — both
            // stay inlinable + fast. See docs/performance-unbox-float.md.
            let (float_names, special_names) = match items.get(2..) {
                Some(body) => {
                    let (decl_specs, _) = strip_declares(body);
                    (extract_float_names(&decl_specs), extract_special_names(&decl_specs))
                }
                None => Default::default(),
            };
            // For the inference-driven relaxation below: bindings mutated
            // in this let's body (mutated ⇒ boxed cell ⇒ not promotable to
            // an f64 slot), and whether the `optimize` pass is enabled (the
            // relaxation is sound ONLY because the pass promotes the
            // proven-float local — see value_provably_float).
            let mutated_here = match items.get(2..) {
                Some(body) => mutated_in_body(body, &HashSet::new()),
                None => Default::default(),
            };
            let inference_enabled = std::env::var_os("NCL_NO_INFER").is_none();
            let bindings_ok = match items.get(1) {
                Some(bl) => list_to_vec(bl).map_or(false, |bs| {
                    bs.iter().all(|b| match b {
                        Value::Symbol(_) => true,
                        Value::Cons(_) => list_to_vec(b).map_or(false, |pair| {
                            let name_ok = matches!(pair.first(), Some(Value::Symbol(_)));
                            let computed_ok = match (pair.first(), pair.get(1)) {
                                // A computed init is only box-free (and thus
                                // inline-loop-safe) when it lands in an
                                // unboxed f64 slot: declared double-float AND
                                // NOT special. `lower_let` boxes a special
                                // binding's init under dynamic-bind even when
                                // declared float (`is_float_local` requires
                                // `!is_special`), so a special+double-float
                                // computed init would box per iteration — the
                                // exact corruption hazard this gate forbids.
                                (Some(Value::Symbol(s)), Some(init @ Value::Cons(_))) => {
                                    let not_special = !special_names.contains(&s.name)
                                        && !coord.is_special(coord.intern(&s.name));
                                    // (a) declared double-float → an f64 slot at
                                    // lower time (the original allowance), OR
                                    (float_names.contains(&s.name) && not_special)
                                    // (b) the optimize pass will promote a provably-
                                    // float, immutable, non-special binding to an f64
                                    // slot — box-free — so this loop is safe to inline
                                    // (Slice 4). Sound only with the pass enabled and
                                    // only for immutable bindings (mutated ⇒ cell).
                                        || (inference_enabled
                                            && not_special
                                            && !mutated_here.contains(&s.name)
                                            && value_provably_float(init, env))
                                }
                                // atom/literal init or `(name)` → no float box
                                _ => true,
                            };
                            let init_ok =
                                pair.get(1).map_or(true, |i| form_inline_safe(i, coord, env));
                            name_ok && computed_ok && init_ok
                        }),
                        _ => false,
                    })
                }),
                None => true,
            };
            bindings_ok && items[2..].iter().all(|f| form_inline_safe(f, coord, env))
        }
        // Special forms + primitive operators whose arguments are all
        // ordinary expressions. The arithmetic / comparison / data
        // operators here are special-cased by `lower_call_in_mut` to
        // *dedicated* Expr nodes (Add/Mul/Lt/Car/…), never an
        // `Expr::Call` — so they're inline-safe regardless of whether the
        // symbol also carries a (Lisp or native) function cell for `#'`.
        "IF" | "PROGN" | "SETQ" | "SETF" | "WHEN" | "UNLESS" | "AND" | "OR"
        | "NOT" | "VALUES" | "PROG1" | "PROG2" | "%LOOP-RETURN" | "RETURN"
        | "PSETQ"
        | "+" | "-" | "*" | "<" | ">" | "<=" | ">=" | "=" | "/="
        | "EQ" | "EQL" | "EQUAL" | "CONS" | "LIST" | "CAR" | "CDR"
        | "FIRST" | "REST" | "NULL" | "CONSP" | "ATOM" | "LISTP"
        | "LENGTH" | "STRING=" | "CHAR" | "STRING-CHAR" | "AREF" | "SVREF" => {
            items[1..].iter().all(|f| form_inline_safe(f, coord, env))
        }
        _ => {
            // Operator application. Safe iff the head is a native
            // primitive (not a Lisp-compiled user function — those may
            // (return) dynamically or signal).
            let sym = coord.intern(&head.name);
            if call_is_lisp_compiled(sym.raw()) {
                return false;
            }
            items[1..].iter().all(|f| form_inline_safe(f, coord, env))
        }
    }
}

/// `(fast-loop TEST RESULT BODY...)` — an inline loop with no capturing
/// lambda, so loop-carried locals (declared `double-float`, or plain
/// fixnum cells) stay unboxed / in registers across iterations. Each
/// pass: if TEST is non-nil, stop and yield RESULT; otherwise run BODY
/// (which steps the loop variables via `setq`) and repeat. TEST,
/// RESULT, and BODY are lowered in the current env, so they see the
/// enclosing `let`'s loop variables. See docs/performance-unbox-float.md.
/// Lower a `(%native-loop (lambda () body…))` the compiler proved
/// inlinable (see `native_loop_inlinable`) to an `Expr::InlineLoop`. The
/// body is lowered IN THE CURRENT ENV (not a fresh lambda env), so loop
/// variables resolve directly to the enclosing locals — no capture, no
/// boxing. `in_inline_loop` is set so `(%loop-return v)` lowers to a
/// direct `LoopBreak`.
fn lower_native_loop_inline(
    lambda_arg: &Value,
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    let body_forms = native_loop_lambda_body(lambda_arg)
        .expect("caller's guard ensured a nullary lambda thunk");
    let saved = env.in_inline_loop;
    env.in_inline_loop = true;
    let lowered: Result<Vec<_>, _> = body_forms
        .iter()
        .map(|f| lower_in_mut(f, env, coord))
        .collect();
    env.in_inline_loop = saved;
    Ok(Expr::InlineLoop { body: Box::new(Expr::progn(lowered?)) })
}

fn lower_fast_loop(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.len() < 2 {
        return Err(CompileError::BadArity {
            head: "FAST-LOOP".into(),
            expected: "at least 2 (test result body...)",
            got: args.len(),
        });
    }
    let test = lower_in_mut(&args[0], env, coord)?;
    let result = lower_in_mut(&args[1], env, coord)?;
    let body = if args.len() == 2 {
        Expr::Nil
    } else {
        let lowered: Result<Vec<_>, _> = args[2..]
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };
    Ok(Expr::FastLoop {
        test: Box::new(test),
        result: Box::new(result),
        body: Box::new(body),
    })
}

fn lower_let(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "LET".into(),
            expected: "at least 1 (bindings)",
            got: 0,
        });
    }
    let bindings_form = &args[0];
    let body_forms = &args[1..];

    // Parse bindings list — each entry is (name value).
    let bindings_list = list_to_vec(bindings_form)?;
    let mut binding_exprs: Vec<Expr> = Vec::new();
    let mut binding_names: Vec<Arc<str>> = Vec::new();

    // Each binding's init is lowered in the OUTER env (CL parallel
    // semantics). BUT — the LLVM emit of Expr::Let pushes init
    // values to the locals vec one by one, so when we lower init[i],
    // emit-time locals already contains the values of init[0..i-1].
    // Any nested Expr::Let inside init[i] (e.g. produced by `or` /
    // `and` / `cond`) computes Local(idx) using `env.local_count`
    // at lower time. If we don't reserve placeholder slots for the
    // earlier inits, those nested Locals get the wrong index.
    //
    // We push an anonymous placeholder for each completed init.
    // Those placeholders are NOT named, so sibling inits can't
    // resolve them (parallel-semantics preserved), but they push
    // env.local_count up so nested constructs see the right slot
    // count. We pop them all before the second loop that creates
    // the real named bindings.
    let placeholder_cp = env.checkpoint();
    for binding in &bindings_list {
        // CL allows three shapes for each binding entry:
        //   * `(name value)` — explicit init form
        //   * `(name)`       — init defaults to nil
        //   * `name`         — bare symbol, same as `(name nil)`
        // The bare-symbol form is what tripped chapter-5
        // SUBFORM-EVALUATION's `(let (x) …)` setup. Without it the
        // dotests block aborts at compile time with a confusing
        // "ImproperList(Symbol(...))" from `list_to_vec`.
        let (name, init_form) = match binding {
            Value::Symbol(s) => (Arc::clone(&s.name), None),
            Value::Cons(_) => {
                let pair = list_to_vec(binding)?;
                if pair.is_empty() || pair.len() > 2 {
                    return Err(CompileError::NotImplemented(format!(
                        "let binding must be NAME, (NAME), or (NAME VALUE); \
                         got {pair:?}"
                    )));
                }
                let n = match &pair[0] {
                    Value::Symbol(s) => Arc::clone(&s.name),
                    other => {
                        return Err(CompileError::NotImplemented(format!(
                            "let binding name must be a symbol, got {other:?}"
                        )));
                    }
                };
                let init = if pair.len() == 2 { Some(pair[1].clone()) } else { None };
                (n, init)
            }
            other => {
                return Err(CompileError::NotImplemented(format!(
                    "let binding must be a symbol or list, got {other:?}"
                )));
            }
        };
        let val_expr = match init_form {
            Some(form) => lower_in_mut(&form, env, coord)?,
            None => Expr::Nil,
        };
        binding_exprs.push(val_expr);
        binding_names.push(name);
        // Reserve a slot so subsequent inits' nested-let indices
        // match emit time. Use a name that can't collide with a
        // real symbol.
        env.push_local(Arc::from("__let_placeholder__"));
    }
    // Drop the placeholders; the real bindings are pushed below.
    env.restore(placeholder_cp);

    // Consume leading (declare ...) forms from the body and collect
    // locally-declared-special names. These names, together with
    // globally proclaimed specials, get dynamic binding treatment.
    let (decl_specs, body_forms) = strip_declares(body_forms);
    let locally_special = extract_special_names(&decl_specs);

    // Classify each binding as special (dynamic) or lexical.
    // Special: globally proclaimed via defvar/defparameter/proclaim,
    //          or locally declared via (declare (special name)).
    let is_special_binding: Vec<bool> = binding_names
        .iter()
        .map(|n| {
            locally_special.contains(n)
                || coord.is_special(coord.intern(n))
        })
        .collect();

    // For LEXICAL bindings only: determine which need boxing
    // (mutated in the body, so captured lambdas share the cell).
    // Special bindings are never boxed — their backing store IS the
    // symbol's value cell; setq goes through StoreGlobal directly.
    let mutated = mutated_in_body(body_forms, &HashSet::new());
    let needs_box: Vec<bool> = binding_names
        .iter()
        .zip(is_special_binding.iter())
        .map(|(n, special)| !special && mutated.contains(n))
        .collect();
    // Unboxed-float locals: a binding declared (double-float ..) that is
    // not special. A *mutable* float local is only unboxed when the body
    // creates no closure — an f64 stack slot can't be shared by
    // reference with a capturing lambda, so we fall back to the boxed
    // cell there. Immutable float locals are always safe (captured by
    // value). See docs/performance-unbox-float.md Sprint 2.
    let locally_float = extract_float_names(&decl_specs);
    if !locally_float.is_empty() {
        // Float locals in this let → the function is float-aware; allow
        // auto-inlining of loops in this scope (and inner scopes).
        env.has_float_decl = true;
    }
    let body_closure = !locally_float.is_empty() && body_has_closure(body_forms, coord);
    let is_float_local: Vec<bool> = binding_names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            locally_float.contains(n)
                && !is_special_binding[i]
                && !(needs_box[i] && body_closure)
        })
        .collect();
    let mut f64_slots: Vec<Option<usize>> = vec![None; binding_names.len()];
    for i in 0..binding_names.len() {
        if is_float_local[i] {
            // Store the (coerced-to-f64) init into a dedicated f64 slot.
            let slot = env.next_f64_slot();
            f64_slots[i] = Some(slot);
            let init = std::mem::replace(&mut binding_exprs[i], Expr::Nil);
            binding_exprs[i] = Expr::F64LocalStore { slot, value: Box::new(init) };
        } else if needs_box[i] {
            // Wrap init in (cons init nil) — the cell representation.
            let init = std::mem::replace(&mut binding_exprs[i], Expr::Nil);
            binding_exprs[i] = Expr::cons(init, Expr::Nil);
        }
    }

    // Extend env with new LEXICAL locals (cell, f64, or plain). Special
    // bindings are NOT added to the lexical env — symbol reads/writes
    // for them fall through to LoadGlobal/StoreGlobal as intended.
    let cp = env.checkpoint();
    for (i, name) in binding_names.iter().enumerate() {
        if is_special_binding[i] {
            // Reserve a slot in the Let's bindings vector for the
            // special's pre-computed init value (a temp), but don't
            // add the name to the lexical env. The temp will be
            // passed to DynamicBind as Expr::Local(slot_idx).
            env.push_local(Arc::from("__dyn_temp__"));
        } else if is_float_local[i] {
            env.push_local_f64(Arc::clone(name), f64_slots[i].unwrap());
        } else if needs_box[i] {
            env.push_local_cell(Arc::clone(name));
        } else {
            env.push_local(Arc::clone(name));
        }
    }

    // Lower the body. Reads of special names fall through to
    // LoadGlobal (value cell), writes to StoreGlobal — correct
    // because we didn't add them to the lexical env.
    let raw_body = if body_forms.is_empty() {
        Expr::Nil
    } else if body_forms.len() == 1 {
        lower_in_mut(&body_forms[0], env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = body_forms
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };

    env.restore(cp);

    // Wrap the body in DynamicBind for each special binding,
    // innermost-first (so the first binding wraps outermost).
    // Compute the base local index for this let's bindings:
    // the locals allocated by the OUTER scope before this let
    // are not in binding_exprs; this let's slots start at
    // `saved_local_count` (which is the env's local_count at the
    // placeholder_cp snapshot). Recover it from placeholder_cp.
    let base_local_idx = placeholder_cp.1; // local_count before this let
    let mut body_with_dynbinds = raw_body;
    // Wrap in reverse order so binding[0] is the outermost DynamicBind.
    for (i, name) in binding_names.iter().enumerate().rev() {
        if !is_special_binding[i] { continue; }
        let sym_word = coord.intern(name);
        // The pre-computed init is at Local(base_local_idx + i)
        // inside the Expr::Let that wraps everything.
        let slot = base_local_idx + i;
        body_with_dynbinds = Expr::DynamicBind {
            sym_word: sym_word.raw(),
            value: Box::new(Expr::Local(slot)),
            body: Box::new(body_with_dynbinds),
        };
    }

    Ok(Expr::let_(binding_exprs, body_with_dynbinds))
}

/// `(flet ((name (params...) body...) ...) body...)` — establish
/// local function bindings visible only inside BODY. Sibling
/// functions can't see each other (parallel-style); use LABELS
/// for mutual recursion.
fn lower_flet(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "FLET".into(),
            expected: "at least 1 (binding list)",
            got: 0,
        });
    }
    let bindings_list = list_to_vec(&args[0])?;
    let body_forms = &args[1..];

    // Phase 1: lower each lambda in the OUTER env. No
    // self/sibling reference for FLET — siblings only become
    // visible in the body, not in each other's bodies.
    let mut names: Vec<Arc<str>> = Vec::new();
    let mut inits: Vec<Expr> = Vec::new();
    for binding in &bindings_list {
        let pieces = list_to_vec(binding)?;
        if pieces.len() < 2 {
            return Err(CompileError::NotImplemented(format!(
                "flet binding must be (name (params...) body...), got {pieces:?}"
            )));
        }
        let name = match &pieces[0] {
            Value::Symbol(s) => Arc::clone(&s.name),
            other => {
                return Err(CompileError::NotImplemented(format!(
                    "flet binding name must be a symbol, got {other:?}"
                )));
            }
        };
        // pieces[1..] is `((params...) body...)` — exactly the
        // shape of `lambda`'s args.
        let lambda_args: Vec<Value> = pieces[1..].to_vec();
        let lambda_expr = lower_lambda(&lambda_args, env, coord)?;
        names.push(name);
        inits.push(lambda_expr);
    }

    // Phase 2: bind each as a let-local AND register in the
    // function namespace. Both for the body to see.
    let cp = env.checkpoint();
    for name in &names {
        let idx = env.push_local(Arc::clone(name));
        env.push_fn_binding(Arc::clone(name), Binding::Local(idx));
    }

    // Phase 3: lower the body.
    let body_expr = if body_forms.is_empty() {
        Expr::Nil
    } else if body_forms.len() == 1 {
        lower_in_mut(&body_forms[0], env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = body_forms
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };

    env.restore(cp);
    Ok(Expr::let_(inits, body_expr))
}

/// `(labels ((name (params...) body...) ...) body...)` — like
/// flet but the bindings are visible to each other (mutual
/// recursion).
///
/// Implementation: pre-allocate a `(cons nil nil)` cell per
/// function name as the let-binding's value. Each cell becomes
/// a LocalCell in both the variable and function namespace.
/// Then lower each lambda IN the env that has those fn-bindings;
/// the lambda's calls to siblings resolve via `(car (Local ...))`
/// or, after closure capture, `(car (ClosureRef ...))`. Finally,
/// emit a sequence of `(set-car cell-i lambda-i)` followed by
/// the body.
///
/// At runtime: cells are filled in order. Since lambda creation
/// just packages captures (it doesn't call the body), a sibling's
/// cell can be empty when its capturer is created — what matters
/// is that the cell is filled by the time the capturing lambda
/// is actually CALLED.
fn lower_labels(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "LABELS".into(),
            expected: "at least 1 (binding list)",
            got: 0,
        });
    }
    let bindings_list = list_to_vec(&args[0])?;
    let body_forms = &args[1..];

    // Parse names + lambda forms upfront.
    let mut names: Vec<Arc<str>> = Vec::new();
    let mut lambda_forms: Vec<Vec<Value>> = Vec::new();
    for binding in &bindings_list {
        let pieces = list_to_vec(binding)?;
        if pieces.len() < 2 {
            return Err(CompileError::NotImplemented(format!(
                "labels binding must be (name (params...) body...), got {pieces:?}"
            )));
        }
        let name = match &pieces[0] {
            Value::Symbol(s) => Arc::clone(&s.name),
            other => {
                return Err(CompileError::NotImplemented(format!(
                    "labels binding name must be a symbol, got {other:?}"
                )));
            }
        };
        names.push(name);
        lambda_forms.push(pieces[1..].to_vec());
    }

    // Pre-allocate cells as let inits — each (cons nil nil).
    let cell_inits: Vec<Expr> = names
        .iter()
        .map(|_| Expr::cons(Expr::Nil, Expr::Nil))
        .collect();

    // Push locals + fn-bindings BEFORE lowering any lambda.
    // Each name is a LocalCell so reads through the cell at
    // call time and lambdas capture the cell (not the value),
    // which is what makes mutual recursion work.
    let cp = env.checkpoint();
    let mut local_indices: Vec<usize> = Vec::new();
    for name in &names {
        let idx = env.push_local_cell(Arc::clone(name));
        local_indices.push(idx);
        env.push_fn_binding(Arc::clone(name), Binding::LocalCell(idx));
    }

    // Lower each lambda IN the env that has all sibling fn-
    // bindings. References to siblings naturally lower to a
    // funcall on (car (Local idx)) / (car (ClosureRef idx)) via
    // binding_read on the LocalCell binding.
    let mut setf_forms: Vec<Expr> = Vec::with_capacity(names.len());
    for (lambda_args, idx) in lambda_forms.iter().zip(local_indices.iter()) {
        let lambda_expr = lower_lambda(lambda_args, env, coord)?;
        setf_forms.push(Expr::set_car(Expr::local(*idx), lambda_expr));
    }

    // Lower the body in the same env.
    let body_expr = if body_forms.is_empty() {
        Expr::Nil
    } else if body_forms.len() == 1 {
        lower_in_mut(&body_forms[0], env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = body_forms
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };

    env.restore(cp);

    // Compose: let cells := (cons nil nil); then progn the
    // setfs followed by the body.
    let mut progn_forms = setf_forms;
    progn_forms.push(body_expr);
    Ok(Expr::let_(cell_inits, Expr::progn(progn_forms)))
}

/// `(and)` → `t`. `(and x)` → `x`. `(and a b c)` short-circuits:
/// if `a` is nil, return nil; else if `b` is nil, return nil; else
/// return `c` (evaluated). Each form evaluated at most once.
///
/// Desugars to nested `if`. No `let` needed because each form
/// appears exactly once in the lowered tree.
fn lower_and(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Ok(Expr::True);
    }
    if args.len() == 1 {
        return lower_in_mut(&args[0], env, coord);
    }
    let first = lower_in_mut(&args[0], env, coord)?;
    let rest = lower_and(&args[1..], env, coord)?;
    Ok(Expr::if_(first, rest, Expr::Nil))
}

/// `(or)` → nil. `(or x)` → `x`. `(or a b c)` returns the first
/// non-nil value, or nil if none are. Each form evaluated at most
/// once — that means we can't write `(if a a (or b c))` (would
/// evaluate `a` twice if non-nil); we use `let` to bind the result
/// of evaluating the head.
fn lower_or(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Ok(Expr::Nil);
    }
    if args.len() == 1 {
        return lower_in_mut(&args[0], env, coord);
    }
    // Lower head in OUTER env first.
    let first = lower_in_mut(&args[0], env, coord)?;
    // Push a synthetic local for the head's value, lower the rest
    // with the local in scope, then restore.
    let cp = env.checkpoint();
    let tmp_idx = env.push_local(std::sync::Arc::from("__or_tmp__"));
    let rest = lower_or(&args[1..], env, coord)?;
    env.restore(cp);
    // (let ((tmp first)) (if tmp tmp rest))
    Ok(Expr::let_(
        vec![first],
        Expr::if_(Expr::Local(tmp_idx), Expr::Local(tmp_idx), rest),
    ))
}

/// `(cond)` → nil. Each clause is `(test form...)`. The first
/// clause whose test evaluates to non-nil has its body forms
/// evaluated as an implicit progn; the value of the last form is
/// the cond's result. If no test matches, the result is nil.
fn lower_cond(
    clauses: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if clauses.is_empty() {
        return Ok(Expr::Nil);
    }
    let clause = list_to_vec(&clauses[0])?;
    if clause.is_empty() {
        return Err(CompileError::NotImplemented(
            "cond clause must have at least a test".to_string(),
        ));
    }
    let test = lower_in_mut(&clause[0], env, coord)?;
    // Body: implicit progn of forms after the test.
    // CL's `(test)` (clause with only a test) would return test's
    // value if non-nil; defer that case.
    let body = if clause.len() == 1 {
        return Err(CompileError::NotImplemented(
            "cond clause with only a test (no body) not yet supported"
                .to_string(),
        ));
    } else if clause.len() == 2 {
        lower_in_mut(&clause[1], env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = clause[1..]
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };
    let rest = lower_cond(&clauses[1..], env, coord)?;
    Ok(Expr::if_(test, body, rest))
}

fn lower_when(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
    invert: bool,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: head.to_string(),
            expected: "at least 1 (test)",
            got: 0,
        });
    }
    let test = lower_in_mut(&args[0], env, coord)?;
    let body_forms = &args[1..];
    let body = if body_forms.is_empty() {
        Expr::Nil
    } else if body_forms.len() == 1 {
        lower_in_mut(&body_forms[0], env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = body_forms
            .iter()
            .map(|f| lower_in_mut(f, env, coord))
            .collect();
        Expr::progn(lowered?)
    };
    if invert {
        Ok(Expr::if_(test, Expr::Nil, body))
    } else {
        Ok(Expr::if_(test, body, Expr::Nil))
    }
}

fn lower_setq(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    // CL spec: `(setq var1 val1 var2 val2 ...)`. Evaluates and
    // assigns each pair in left-to-right order; returns the value
    // of the LAST assignment. `(setq)` with zero args returns nil.
    if args.is_empty() {
        return Ok(Expr::Nil);
    }
    if args.len() % 2 != 0 {
        return Err(CompileError::BadArity {
            head: head.to_string(),
            expected: "an even number of args (var/value pairs)",
            got: args.len(),
        });
    }
    // Common case: exactly one pair. Emit a single assignment so
    // the IR stays flat — no Progn wrap.
    if args.len() == 2 {
        return lower_setq_pair(head, &args[0], &args[1], env, coord);
    }
    // Multi-pair: lower each pair to its single-assignment Expr,
    // chain them with Progn. The Progn's value is its last form's
    // value, matching CL's "returns the last assignment's value."
    let mut exprs: Vec<Expr> = Vec::with_capacity(args.len() / 2);
    let mut i = 0;
    while i + 1 < args.len() {
        exprs.push(lower_setq_pair(head, &args[i], &args[i + 1], env, coord)?);
        i += 2;
    }
    Ok(Expr::progn(exprs))
}

/// Single-pair `(setq var value)` lowering — the original body of
/// `lower_setq`. Split out so the multi-pair entry point can loop
/// over pairs without re-implementing the local-vs-global resolve.
fn lower_setq_pair(
    head: &str,
    name_form: &Value,
    value_form: &Value,
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    let name = match name_form {
        Value::Symbol(s) => Arc::clone(&s.name),
        other => {
            return Err(CompileError::NotImplemented(format!(
                "{head} target must be a symbol, got {other:?}"
            )));
        }
    };
    // Resolve to a local first. If it's already a Cell variant, the
    // mutation analysis pass at the binding's declaration site has
    // already arranged for boxing; we emit a write through the box.
    // A non-cell local means the binding wasn't detected as mutated
    // at its declaration — most often because the declaration is a
    // function parameter (param boxing isn't wired yet). That's a
    // compile error rather than a silent miscompile.
    if let Some(b) = env.find_or_capture(&name) {
        let value = lower_in_mut(value_form, env, coord)?;
        return match b {
            Binding::LocalCell(i) => Ok(Expr::set_car(Expr::Local(i), value)),
            // Float local: store unboxed into its f64 slot (no cons box).
            Binding::LocalF64(slot) => {
                Ok(Expr::F64LocalStore { slot, value: Box::new(value) })
            }
            Binding::ParamCell(i) => Ok(Expr::set_car(Expr::Param(i), value)),
            Binding::ClosureRefCell(i) => {
                Ok(Expr::set_car(Expr::ClosureRef(i), value))
            }
            Binding::Param(_) => Err(CompileError::NotImplemented(format!(
                "setq of function parameter: {name} (mutable parameters not yet supported)"
            ))),
            Binding::ParamF64(_) => Err(CompileError::NotImplemented(format!(
                "setq of double-float parameter: {name} (a mutated param is never \
                 declared-float-unboxed — this is an internal inconsistency)"
            ))),
            Binding::Local(_) => Err(CompileError::NotImplemented(format!(
                "internal: non-cell local {name} reached setq path \
                 — mutation analysis missed this binding"
            ))),
            Binding::ClosureRef(_) => Err(CompileError::NotImplemented(format!(
                "setq of captured non-cell variable: {name} (parent scope did not box)"
            ))),
        };
    }
    let sym_word = coord.intern(&name);
    let value = lower_in_mut(value_form, env, coord)?;
    Ok(Expr::store_global(sym_word.raw(), value))
}

/// `(setf place value)`. We pattern-match on the shape of `place`:
///
/// - bare symbol → `(setq name value)`
/// - `(car x)`   → SetCar
/// - `(cdr x)`   → SetCdr
/// - `(first x)` → SetCar (alias)
/// - `(rest x)`  → SetCdr (alias)
/// - `(aref s i)` / `(char s i)` → SetChar
///
/// CL's full setf is a macro extensible via `defsetf` /
/// `define-setf-expander`. Until macros land, this is a built-in
/// that recognises a fixed set of place forms.
fn lower_setf(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    // CL spec: `(setf p1 v1 p2 v2 ...)`. Left-to-right evaluation
    // and assignment; returns the value of the LAST assignment.
    // Zero args is nil. Odd args is a syntax error.
    if args.is_empty() {
        return Ok(Expr::Nil);
    }
    if args.len() % 2 != 0 {
        return Err(CompileError::BadArity {
            head: "SETF".into(),
            expected: "an even number of args (place/value pairs)",
            got: args.len(),
        });
    }
    if args.len() > 2 {
        // Multi-pair: lower each pair separately, sequence with Progn.
        let mut exprs: Vec<Expr> = Vec::with_capacity(args.len() / 2);
        let mut i = 0;
        while i + 1 < args.len() {
            let pair = [args[i].clone(), args[i + 1].clone()];
            exprs.push(lower_setf(&pair, env, coord)?);
            i += 2;
        }
        return Ok(Expr::progn(exprs));
    }
    let place = &args[0];
    let value_form = &args[1];

    // Bare symbol: same as setq.
    if let Value::Symbol(_) = place {
        return lower_setq("SETF", args, env, coord);
    }

    // (head subargs...) — must be one of the recognised place forms.
    let place_items = list_to_vec(place)?;
    let place_head = match place_items.first() {
        Some(Value::Symbol(s)) => s.name.to_string(),
        _ => {
            return Err(CompileError::NotImplemented(format!(
                "setf place must be a symbol or recognised form, got {place:?}"
            )));
        }
    };
    let place_args = &place_items[1..];

    match place_head.as_str() {
        "CAR" | "FIRST" => {
            if place_args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(car place)",
                    got: place_args.len(),
                });
            }
            let cons = lower_in_mut(&place_args[0], env, coord)?;
            let value = lower_in_mut(value_form, env, coord)?;
            Ok(Expr::set_car(cons, value))
        }
        "CDR" | "REST" => {
            if place_args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(cdr place)",
                    got: place_args.len(),
                });
            }
            let cons = lower_in_mut(&place_args[0], env, coord)?;
            let value = lower_in_mut(value_form, env, coord)?;
            Ok(Expr::set_cdr(cons, value))
        }
        "AREF" | "SVREF" => {
            if place_args.len() != 2 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(aref v i)",
                    got: place_args.len(),
                });
            }
            // Polymorphic — runtime tag dispatches to vector or string.
            let v = lower_in_mut(&place_args[0], env, coord)?;
            let idx = lower_in_mut(&place_args[1], env, coord)?;
            let val = lower_in_mut(value_form, env, coord)?;
            Ok(Expr::set_aref(v, idx, val))
        }
        "CHAR" | "STRING-CHAR" => {
            if place_args.len() != 2 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(char s i)",
                    got: place_args.len(),
                });
            }
            // String-specialized.
            let s = lower_in_mut(&place_args[0], env, coord)?;
            let idx = lower_in_mut(&place_args[1], env, coord)?;
            let ch = lower_in_mut(value_form, env, coord)?;
            Ok(Expr::set_char(s, idx, ch))
        }
        "GETHASH" => {
            if place_args.len() != 2 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(gethash key ht)",
                    got: place_args.len(),
                });
            }
            // (setf (gethash KEY HT) VAL) → (%hash-set HT KEY VAL).
            // The Lisp-side %hash-set lives in core.lisp on top of
            // make-array + cons cells.
            let key = lower_in_mut(&place_args[0], env, coord)?;
            let ht = lower_in_mut(&place_args[1], env, coord)?;
            let val = lower_in_mut(value_form, env, coord)?;
            let sym = coord.intern("%HASH-SET");
            Ok(Expr::call(sym.raw(), vec![ht, key, val]))
        }
        "SYMBOL-FUNCTION" => {
            if place_args.len() != 1 {
                return Err(CompileError::BadArity {
                    head: format!("setf {place_head}"),
                    expected: "(symbol-function sym)",
                    got: place_args.len(),
                });
            }
            // (setf (symbol-function S) F) → (%set-symbol-function S F).
            let sym_arg = lower_in_mut(&place_args[0], env, coord)?;
            let val = lower_in_mut(value_form, env, coord)?;
            let setter_sym = coord.intern("%SET-SYMBOL-FUNCTION");
            Ok(Expr::call(setter_sym.raw(), vec![sym_arg, val]))
        }
        // Generic fallback: any unrecognised setf place is rewritten
        // as a call to the function named by mangling the place's
        // head. (setf (FOO arg1 ... argN) val) → (%setf-FOO val arg1
        // ... argN). Lets defstruct generate auto-setf-accessors by
        // simply (defun %setf-NAME-SLOT (val obj) ...). User code
        // can opt in by following the same naming convention.
        head => {
            let setter_name = format!("%SETF-{head}");
            let setter_sym = coord.intern(&setter_name);
            let val = lower_in_mut(value_form, env, coord)?;
            let mut call_args = Vec::with_capacity(1 + place_args.len());
            call_args.push(val);
            for a in place_args {
                call_args.push(lower_in_mut(a, env, coord)?);
            }
            Ok(Expr::call(setter_sym.raw(), call_args))
        }
    }
}

fn lower_defparameter(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    // (defparameter name value [doc])  — doc string is optional and
    // ignored in v1.
    if args.len() < 2 || args.len() > 3 {
        return Err(CompileError::BadArity {
            head: head.to_string(),
            expected: "2 or 3",
            got: args.len(),
        });
    }
    let name = match &args[0] {
        Value::Symbol(s) => Arc::clone(&s.name),
        other => {
            return Err(CompileError::NotImplemented(format!(
                "{head}'s first argument must be a symbol, got {other:?}"
            )));
        }
    };
    let sym_word = coord.intern(&name);
    // Mark the symbol as globally special so any future `let`
    // binding of this name uses dynamic rebinding (value cell)
    // rather than a lexical local slot.
    coord.proclaim_special(sym_word);
    let value = lower_in_mut(&args[1], env, coord)?;
    // CL spec: (defparameter name ...) returns the symbol, not the
    // new value. Returning the value tripped the REPL printer when
    // a CLOS class instance was bound — its circular metaclass
    // back-link blew the printer's stack. Returning the symbol is
    // also the spec-correct shape.
    Ok(Expr::progn(vec![
        Expr::store_global(sym_word.raw(), value),
        Expr::Word(sym_word.raw()),
    ]))
}

/// Match a `(declare decl1 decl2 ...)` form. Returns a Vec of
/// the declspec values (decl1, decl2, …) on a match, or None.
fn match_declare(form: &Value) -> Option<Vec<Value>> {
    let items = list_to_vec(form).ok()?;
    if let Some(Value::Symbol(s)) = items.first() {
        if &*s.name == "DECLARE" {
            return Some(items[1..].to_vec());
        }
    }
    None
}

/// Consume leading `(declare ...)` forms from `body`. Returns the
/// collected declspecs (each a `Value` — one declspec per form,
/// flattened) and the rest of the body (non-declare forms).
pub fn strip_declares(body: &[Value]) -> (Vec<Value>, &[Value]) {
    let mut specs: Vec<Value> = Vec::new();
    let mut n = 0;
    for form in body {
        if let Some(decl_specs) = match_declare(form) {
            specs.extend(decl_specs);
            n += 1;
        } else {
            break;
        }
    }
    (specs, &body[n..])
}

/// Given a slice of declspec Values (the arguments to `declare`
/// forms), collect the names appearing in `(special name1 name2 …)`
/// specs and return them as an `Arc<str>` set.
fn extract_special_names(specs: &[Value]) -> HashSet<Arc<str>> {
    let mut out = HashSet::new();
    for spec in specs {
        let Ok(items) = list_to_vec(spec) else { continue };
        let [Value::Symbol(head), rest @ ..] = items.as_slice() else { continue };
        if &*head.name != "SPECIAL" { continue; }
        for name_val in rest {
            if let Value::Symbol(s) = name_val {
                out.insert(Arc::clone(&s.name));
            }
        }
    }
    out
}

/// Collect names declared as a float type — `(double-float a b)`,
/// `(single-float x)` (treated as double — NCL floats are doubles, see
/// plan §2.7), the supertype `(float …)`, or the `(type <ftype> …)`
/// long form. Symbols are upcased by the reader, so we match upcase.
pub fn extract_float_names(specs: &[Value]) -> HashSet<Arc<str>> {
    fn is_float_type(n: &str) -> bool {
        matches!(
            n,
            "DOUBLE-FLOAT" | "SINGLE-FLOAT" | "LONG-FLOAT" | "SHORT-FLOAT" | "FLOAT"
        )
    }
    let mut out = HashSet::new();
    for spec in specs {
        let Ok(items) = list_to_vec(spec) else { continue };
        let [Value::Symbol(head), rest @ ..] = items.as_slice() else { continue };
        let names: &[Value] = if &*head.name == "TYPE" {
            // (type <typespec> name...) — typespec must be a float type.
            match rest {
                [Value::Symbol(ty), ns @ ..] if is_float_type(&ty.name) => ns,
                _ => continue,
            }
        } else if is_float_type(&head.name) {
            // (double-float name...) shorthand declaration.
            rest
        } else {
            continue;
        };
        for nv in names {
            if let Value::Symbol(s) = nv {
                out.insert(Arc::clone(&s.name));
            }
        }
    }
    out
}

/// (locally (declare ...) body...) — process local declarations
/// for the enclosed body; evaluate the body in their scope.
fn lower_locally(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    // Strip declare forms, ignore locally-declared specials for now
    // (they only affect let-lowering inside the body, which handles
    // its own declare stripping). The body is lowered normally.
    let (_, body_forms) = strip_declares(args);
    if body_forms.is_empty() {
        return Ok(Expr::Nil);
    }
    let lowered: Result<Vec<_>, _> = body_forms
        .iter()
        .map(|f| lower_in_mut(f, env, coord))
        .collect();
    Ok(Expr::progn(lowered?))
}

/// (proclaim '(special name1 name2 ...)) — globally proclaim
/// variables as dynamically scoped. `(proclaim '(special *x*))` is
/// equivalent to wrapping `*x*` with defvar; the symbol's value
/// cell is not touched, only the special registry.
fn lower_proclaim(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.len() != 1 {
        return Err(CompileError::BadArity {
            head: "PROCLAIM".into(),
            expected: "1",
            got: args.len(),
        });
    }
    // The argument is usually a quoted list: '(special *x* *y*).
    // After macroexpansion it's a (quote (special ...)) form.
    // We handle it at compile time: peek inside the quote.
    let inner = match &args[0] {
        Value::Cons(_) => {
            let items = list_to_vec(&args[0])?;
            if items.len() >= 1 {
                if let Value::Symbol(s) = &items[0] {
                    if &*s.name == "QUOTE" && items.len() == 2 {
                        // (quote (special ...)) — extract the inner list
                        items.get(1).cloned()
                    } else { None }
                } else { None }
            } else { None }
        }
        _ => None,
    };
    if let Some(decl) = inner {
        let specs = vec![decl];
        let names = extract_special_names(&specs);
        for name in &names {
            let sym = coord.intern(name);
            coord.proclaim_special(sym);
        }
    }
    // proclaim is a side-effect at compile time; at runtime it
    // evaluates the argument (for conformance) and returns nil.
    let arg_expr = lower_in_mut(&args[0], env, coord)?;
    Ok(Expr::progn(vec![arg_expr, Expr::Nil]))
}

/// Build a 0-arity lambda (thunk) from a slice of body forms.
/// Used by unwind-protect to wrap the protected and cleanup forms.
fn lower_thunk(
    body_forms: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    // lower_lambda expects args = [param-list, body-form1, body-form2, ...]
    // An empty param list is represented as Value::Nil.
    let mut lambda_args = Vec::with_capacity(1 + body_forms.len());
    lambda_args.push(Value::Nil); // empty param list
    lambda_args.extend_from_slice(body_forms);
    lower_lambda(&lambda_args, env, coord)
}

/// (unwind-protect protected cleanup...) — ensure cleanup always runs.
/// Lowers to (%native-unwind-protect #'protected-thunk #'cleanup-thunk).
fn lower_unwind_protect(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "UNWIND-PROTECT".into(),
            expected: "at least 1 (protected form)",
            got: 0,
        });
    }
    let protected_forms = &args[0..1]; // just the first form
    let cleanup_forms = &args[1..];    // everything after

    let protected_thunk = lower_thunk(protected_forms, env, coord)?;
    let cleanup_thunk = lower_thunk(cleanup_forms, env, coord)?;

    let sym_word = coord.intern("%NATIVE-UNWIND-PROTECT");
    Ok(Expr::call(sym_word.raw(), vec![protected_thunk, cleanup_thunk]))
}

/// Lower a `(lambda (params...) body...)` form. Builds an inner
/// env that captures any outer-env names referenced in the body,
/// lowers the body in that env (which records captures lazily),
/// and emits an `Expr::Lambda` with both the body and the
/// outer-scope expressions for each capture.
fn lower_lambda(
    args: &[Value],
    outer_env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "LAMBDA".into(),
            expected: "at least 1 (param list)",
            got: 0,
        });
    }
    let params = crate::parse_param_list_inner(&args[0])
        .map_err(CompileError::NotImplemented)?;
    let all_body_forms = &args[1..];
    // Strip leading (declare ...) forms — they're processed at
    // lowering time, not evaluated as calls. Locally-declared
    // specials inside a lambda body are handled by the (let ...)
    // forms within; we just need to not trip on the bare DECLARE.
    let (decl_specs, body_forms) = strip_declares(all_body_forms);

    // Inner env starts with required params at Param(0..N) and a
    // capture parent that begins as a clone of `outer_env`. The
    // clone gets mutated when the body's lookups call
    // `find_or_capture` recursively — those mutations need to
    // make it back to `outer_env` too, so the surrounding
    // function's captures vec grows in lockstep. We snapshot
    // `outer_env`'s sizes here, then reconcile the parent clone's
    // tail back after the body is lowered.
    let outer_captures_before = outer_env.captures.len();
    let outer_bindings_before = outer_env.bindings.len();
    let mut inner_env = LocalEnv::for_lambda(&params.required, outer_env.clone());

    // ── Mutable-parameter promotion ──────────────────────────────
    //
    // Required parameters start as Binding::Param(i) — direct reads
    // from the argument array.  If the body mutates a required param
    // (setq / push / incf / …), we promote it to a let-bound
    // LocalCell so the mutation has somewhere to write.
    //
    // Implementation: for each mutated required param at Param(i),
    // push a new local cell seeded with `(cons Param(i) nil)` and
    // rebind the name to point at that cell.  This is exactly how
    // `push_param_local` handles &optional/&key/&rest params in
    // `build_arglist_prologue`.
    let body_mutations = mutated_in_body(body_forms, &HashSet::new());
    let mut req_box_prologue: Vec<Expr> = Vec::new();
    for (i, name) in params.required.iter().enumerate() {
        if body_mutations.contains(name) {
            // Replace the Param binding with a LocalCell.
            let cell_init = Expr::cons(Expr::Param(i), Expr::Nil);
            inner_env.rebind_as_local_cell(name);
            req_box_prologue.push(cell_init);
        }
    }

    // Unboxed-float parameters: a required param declared `(double-float
    // ..)` that is not mutated (mutated ones were boxed above) and not
    // special is read as a native f64 — same as `defun`'s float handling.
    // Previously lambda bodies discarded their declares, so a float-typed
    // lambda never unboxed its params or enabled loop auto-inlining.
    let float_params = extract_float_names(&decl_specs);
    if !float_params.is_empty() {
        inner_env.mark_float_decl();
        for pname in &params.required {
            if float_params.contains(pname.as_ref())
                && !body_mutations.contains(pname)
                && !coord.is_special(coord.intern(pname))
            {
                inner_env.rebind_as_param_f64(pname);
            }
        }
    }

    let mut prologue = build_arglist_prologue(&params, body_forms, &mut inner_env, coord)?;
    // Prepend required-param cells BEFORE optional/key/rest locals
    // so cell indices line up with the push order in inner_env.
    if !req_box_prologue.is_empty() {
        let mut combined = req_box_prologue;
        combined.append(&mut prologue);
        prologue = combined;
    }

    let lowered_body = if body_forms.is_empty() {
        Expr::Nil
    } else if body_forms.len() == 1 {
        lower_in_mut(&body_forms[0], &mut inner_env, coord)?
    } else {
        let lowered: Result<Vec<_>, _> = body_forms
            .iter()
            .map(|f| lower_in_mut(f, &mut inner_env, coord))
            .collect();
        Expr::progn(lowered?)
    };

    let body_expr = if prologue.is_empty() {
        lowered_body
    } else {
        Expr::let_(prologue, lowered_body)
    };
    // Same tail-position MV instrumentation as for top-level defun.
    let body_expr = instrument_tail_for_mv(body_expr);

    let captures = inner_env.take_captures();

    // Reconcile: any names this lambda's body resolved by recursively
    // capturing through `outer_env` were applied to the parent
    // *clone* inside `inner_env.capture_parent`, not to `outer_env`
    // directly. Copy any new captures/bindings back so the
    // surrounding function knows about them. Without this, a
    // grandchild lambda that captures from a grandparent would
    // produce a parent Lambda IR with empty captures and a runtime
    // ClosureRef would read garbage.
    //
    // CRITICAL: this MUST walk the chain recursively. A 4-level
    // nest `(defun mk (x) (lambda () (lambda () (lambda (y) (+ x
    // y)))))` segfaults if we only copy one level. The innermost
    // body's find_or_capture(x) walks up through clones —
    // ParentOfInner → L1View1 → MView1 — adding captures at every
    // level *of the clone chain it walked*. The innermost
    // lambda's reconcile copies ParentOfInner's growth back into
    // the directly-surrounding lambda. But L1View1's growth is
    // held inside ParentOfInner.capture_parent, NOT inside the
    // directly-surrounding lambda's own .capture_parent (which is
    // an *independent* clone of L1, untouched by the inner walk).
    // So the L1 level never learns it needs to capture x from mk,
    // and L1 emits an empty captures list → its env is empty at
    // runtime → the next-level lambda reads garbage from env[0]
    // and segfaults.
    //
    // Fix: when reconciling, walk into parent_clone.capture_parent
    // and outer_env.capture_parent in lockstep, copying new
    // entries at every level. The "before" sizes for the deeper
    // levels are simply each grandparent's current sizes —
    // outer_env's grandparents weren't directly mutated during
    // body lowering (only inner_env's clone-chain was), so their
    // current sizes equal their pre-body sizes.
    if let Some(parent_clone) = inner_env.capture_parent.take() {
        reconcile_chain(*parent_clone, outer_env, outer_captures_before, outer_bindings_before);
    }

    Ok(Expr::lambda(params.required.len() as u32, body_expr, captures))
}

/// Lower a non-top-level `(defun NAME PARAMS BODY...)` into the
/// equivalent runtime install:
///
/// ```text
/// (progn
///   (%set-symbol-function 'NAME (lambda PARAMS BODY...))
///   'NAME)
/// ```
///
/// The top-level path (see `handle_defun` in lib.rs) compiles the
/// function eagerly at top level and installs it directly without
/// going through the global call. Inside a function body that's
/// impossible — the form might never execute, or might execute many
/// times — so we emit the same setup the JIT would emit for an
/// explicit `(setf (symbol-function 'NAME) (lambda ...))`. The lambda
/// captures the enclosing lexical scope per CL conformance (SBCL /
/// CCL behave the same way), so e.g.
///
/// ```text
/// (let ((x 10))
///   (defun foo () x))
/// ```
///
/// installs a closure that returns 10 when called.
///
/// The `block NAME` wrap that the top-level path optionally adds
/// when the body uses `return-from` is NOT applied here — wiring
/// that through the runtime-install path requires more machinery
/// (the block name has to land in the lambda's macro environment,
/// not the lexical one). The known corman ANSI hyperspec-examples
/// uses don't exercise `return-from` inside nested defuns, so we
/// punt for now. A user who needs it can `(block NAME body…)` by
/// hand.
///
/// `(defun (setf NAME) …)` is rejected here — the mangling lives in
/// `match_defun_like` and only fires from the top-level path. Add
/// it back when a test surfaces a real need.
fn lower_nested_defun(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.len() < 2 {
        return Err(CompileError::BadDefun(format!(
            "defun needs at least name and params, got {} args",
            args.len()
        )));
    }
    let name = match &args[0] {
        Value::Symbol(s) => Arc::clone(&s.name),
        other => {
            return Err(CompileError::BadDefun(format!(
                "nested defun name must be a symbol \
                 (the (setf …) shape is top-level only); got {other:?}"
            )));
        }
    };
    // args[0] = NAME, args[1] = PARAMS, args[2..] = BODY. lower_lambda
    // takes (PARAMS, BODY...), exactly args[1..].
    let lambda_expr = lower_lambda(&args[1..], env, coord)?;
    let sym_word = coord.intern(&name);
    let setter_sym = coord.intern("%SET-SYMBOL-FUNCTION");
    let install_call = Expr::call(
        setter_sym.raw(),
        vec![Expr::Word(sym_word.raw()), lambda_expr],
    );
    // CL: `defun` returns the function name as the form's value.
    Ok(Expr::progn(vec![install_call, Expr::Word(sym_word.raw())]))
}

/// Recursively reconcile a parent_clone chain back into its
/// outer_env chain after a lambda body has been lowered.
///
/// The clone-chain holds any new captures and bindings that
/// `find_or_capture` added while walking up looking for free
/// variables. Each level of the chain corresponds to one
/// enclosing lambda scope. We need every level's growth back in
/// the matching level of the real chain so the *outermost*
/// lambda that needed the variable ends up with the right
/// Param(i) capture in its IR.
///
/// The `before` sizes are only known precisely for the first
/// level (the direct caller passes them). For deeper levels we
/// use outer_env's current sizes — outer_env's chain wasn't
/// mutated during body lowering (only its clone was, inside
/// inner_env.capture_parent), so its current state equals its
/// pre-body state.
fn reconcile_chain(
    parent_clone: LocalEnv,
    outer_env: &mut LocalEnv,
    outer_captures_before: usize,
    outer_bindings_before: usize,
) {
    // Copy this level.
    let new_caps: Vec<Expr> = parent_clone
        .captures
        .iter()
        .skip(outer_captures_before)
        .cloned()
        .collect();
    for c in new_caps {
        outer_env.captures.push(c);
    }
    let new_bindings: Vec<(Arc<str>, Binding)> = parent_clone
        .bindings
        .iter()
        .skip(outer_bindings_before)
        .cloned()
        .collect();
    for b in new_bindings {
        outer_env.bindings.push(b);
    }
    if parent_clone.closure_count > outer_env.closure_count {
        outer_env.closure_count = parent_clone.closure_count;
    }

    // Recurse into the next level if both sides have a parent.
    // We can take parent_clone.capture_parent because we own
    // parent_clone (passed by value), and we use outer_env's
    // capture_parent by mutable reference so changes propagate
    // back into the chain the caller still holds.
    let pc_parent = parent_clone.capture_parent;
    if let Some(pc_grand) = pc_parent {
        if let Some(oe_grand_box) = outer_env.capture_parent.as_mut() {
            let oe_grand: &mut LocalEnv = oe_grand_box;
            let oe_caps_before = oe_grand.captures.len();
            let oe_bind_before = oe_grand.bindings.len();
            reconcile_chain(*pc_grand, oe_grand, oe_caps_before, oe_bind_before);
        }
    }
}

/// Build the entry-block let-bindings that materialise &optional,
/// &rest, and &key parameters. Pushes locals into `env` in the
/// order they're produced (optionals first, then rest, then keys),
/// returns the corresponding initialiser exprs in the same order.
/// The returned Vec is suitable as the bindings of an `Expr::Let`
/// wrapping the function body.
///
/// Default forms (for optionals and keys) are lowered against `env`
/// at the moment of binding, so each default sees all earlier
/// required params + earlier optionals/keys but not later ones.
/// Matches CL semantics.
///
/// Used by both the lambda lowerer here and `compile_function` over
/// in lib.rs (which pulls it in via `lower::build_arglist_prologue`).
pub(crate) fn build_arglist_prologue(
    params: &crate::ParamSpec,
    body_forms: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Vec<Expr>, CompileError> {
    let mut bindings: Vec<Expr> = Vec::new();
    let body_mutations = mutated_in_body(body_forms, &HashSet::new());
    let req_n = params.required.len() as u32;

    // `&whole` / `&environment` (macro lambda lists): non-positional
    // locals bound to NIL. Bound first so they're in scope for any
    // later default forms. A real binding (the whole call form / the
    // lexical macro environment) is a future enhancement.
    if let Some(whole_name) = &params.whole {
        push_param_local(env, whole_name, Expr::Nil, &body_mutations, &mut bindings);
    }
    if let Some(env_name) = &params.environment {
        // Bind to a non-nil marker (T) rather than NIL: the value is an
        // opaque "current lexical environment" token. Passing it to
        // macro-function / macroexpand signals them to consult the live
        // macrolet env (see macro_function_shim's local-macro bridge);
        // a NIL env there means "global environment only". CL leaves the
        // environment object's representation implementation-defined.
        push_param_local(env, env_name, Expr::True, &body_mutations, &mut bindings);
    }

    // Optionals are positional: arg index = required + i.
    for (i, opt) in params.optionals.iter().enumerate() {
        let arg_idx = req_n + i as u32;
        let default_expr = match &opt.default {
            Some(form) => lower_in_mut(form, env, coord)?,
            None => Expr::Nil,
        };
        let init = Expr::opt_arg(arg_idx, default_expr);
        push_param_local(env, &opt.name, init, &body_mutations, &mut bindings);
        // supplied-p variable, if declared
        if let Some(ref sp_name) = opt.supplied_p {
            let sp_init = Expr::opt_supplied_p(arg_idx);
            push_param_local(env, sp_name, sp_init, &body_mutations, &mut bindings);
        }
    }

    // Rest binding (if present) follows optionals. The rest list
    // captures everything from `required + optionals` to `n_args`.
    if let Some(rest_name) = &params.rest {
        let start = req_n + params.optionals.len() as u32;
        let init = Expr::bind_rest(start);
        push_param_local(env, rest_name, init, &body_mutations, &mut bindings);
    }

    // Keys: scan args[required+optional..] for matching keywords.
    // The interleaved (kw, value) pairs that follow the
    // required+optional region are also (per CL) included in the
    // rest list above — independent views of the same range.
    if !params.keys.is_empty() {
        let key_start = req_n + params.optionals.len() as u32;
        for key in &params.keys {
            let kw_word = coord.intern(&key.keyword).raw();
            let default_expr = match &key.default {
                Some(form) => lower_in_mut(form, env, coord)?,
                None => Expr::Nil,
            };
            let init = Expr::key_arg(kw_word, key_start, default_expr);
            push_param_local(env, &key.name, init, &body_mutations, &mut bindings);
            // supplied-p variable, if declared
            if let Some(ref sp_name) = key.supplied_p {
                let sp_init = Expr::key_supplied_p(kw_word, key_start);
                push_param_local(env, sp_name, sp_init, &body_mutations, &mut bindings);
            }
        }
    }

    Ok(bindings)
}

/// Walk an expression's tail positions. Where the tail position
/// already holds an `Expr::Values`, leave it alone — `values` will
/// write the multi-value slot itself. Anywhere else, wrap the tail
/// with `Expr::EnsureSingleMv`, which writes `[primary]` into the
/// slot at function exit. Together this guarantees the per-call
/// invariant that the multi-value slot reflects the actual return
/// values of the function the caller just invoked.
///
/// "Tail position" recurses through forms that propagate their last
/// sub-expression's value as their own — Progn (last form), Let
/// (body), If (both branches). Everything else is a leaf tail and
/// gets the single-MV wrap.
///
/// Special case — `%NATIVE-BLOCK`: the `block` macro expands to
/// `(%native-block 'NAME (lambda () body…))`. The lambda body is
/// independently instrumented by lower_lambda, so `%native-block`
/// already propagates whatever MV state the lambda exits with.
/// Wrapping the call with EnsureSingleMv would collapse `(values q
/// r)` returns to a single value. We skip the wrap for this
/// particular call symbol.
pub(crate) fn instrument_tail_for_mv(e: Expr) -> Expr {
    match e {
        Expr::Values(_) => e,
        Expr::Progn(mut es) => {
            if let Some(last) = es.pop() {
                es.push(instrument_tail_for_mv(last));
            }
            Expr::Progn(es)
        }
        Expr::Let { bindings, body } => {
            Expr::Let {
                bindings,
                body: Box::new(instrument_tail_for_mv(*body)),
            }
        }
        Expr::If(c, t, f) => Expr::If(
            c,
            Box::new(instrument_tail_for_mv(*t)),
            Box::new(instrument_tail_for_mv(*f)),
        ),
        // Self-tail-call loop: the loop body still has real value tails
        // (its base cases), so recurse into it to instrument those.
        Expr::TailLoop { arity, body } => Expr::TailLoop {
            arity,
            body: Box::new(instrument_tail_for_mv(*body)),
        },
        // A loop continuation, not a value-returning tail — it rebinds
        // params and branches back. The loop's eventual base-case tail
        // carries the MV contract; leave this alone.
        Expr::SelfTailNext { .. } => e,
        // EnsureSingleMv on top of EnsureSingleMv would be wasted
        // work; keep the inner one.
        Expr::EnsureSingleMv(_) => e,
        // Compiled Lisp functions (direct symbol calls, funcall, apply)
        // manage the multi-value slot themselves via their own
        // instrument_tail_for_mv pass. Wrapping them with EnsureSingleMv
        // would clobber any secondary values they correctly produced.
        //
        // We skip the wrap when the symbol's current function cell holds
        // a Lisp-compiled function (is_lisp_compiled == true), or when
        // the cell is unbound (the function isn't defined yet, which
        // always means it will be a Lisp defun — all native shims are
        // installed before any user code runs).
        //
        // %NATIVE-BLOCK is special regardless: its lambda body is
        // independently instrumented, so it propagates MV correctly.
        //
        // Funcall / Apply dispatch through a runtime function value;
        // in practice these are always closures or #'name references
        // to Lisp functions. We optimistically skip the wrap here too
        // (a funcall of a native shim is an extremely unusual pattern
        // and the cost of a wrong secondary value is mild — not a crash).
        Expr::Funcall { .. } | Expr::Apply { .. } => e,
        Expr::Call { sym_word, .. } if call_is_lisp_compiled(sym_word) => e,
        // Everything else is a leaf tail that does NOT manage the MV
        // slot — wrap it so the slot is always consistent on return.
        other => Expr::ensure_single_mv(other),
    }
}

/// Self-tail-call elimination — rewrite tail-position calls to the
/// function being compiled into `SelfTailNext` continuations, so
/// codegen can lower them to a loop branch-back instead of a real
/// call. Returns the (possibly) rewritten body and whether any
/// self-tail-call was found.
///
/// `self_sym` is the raw Word of the function's own symbol; `arity`
/// is its required-parameter count. A `Call` qualifies only when it
/// targets exactly that symbol with exactly that many arguments (a
/// differing count is an arity error, not a self-call to optimize).
///
/// Walks ONLY genuine tail positions — the last form of a `Progn`,
/// the body of a `Let`, and both arms of an `If`. It deliberately
/// does NOT descend into:
///   - `DynamicBind` bodies (the dynamic value is restored AFTER the
///     body, so a call there is not truly in tail position),
///   - `%native-block` / `%native-unwind-protect` calls (their
///     protected forms appear as plain `Call` arguments, never reached
///     here — and cleanup/return-from targets must outlive the call),
///   - `EnsureSingleMv` (this pass runs BEFORE `instrument_tail_for_mv`,
///     so no wrappers exist yet).
///
/// Caller contract: only invoke for fixed-arity functions (required
/// params only) so every parameter maps to a `Param(i)` the loop can
/// rebind; `&optional`/`&rest`/`&key` functions bind extra params as
/// let-locals via a prologue and are excluded.
///
/// Semantic note: turning a self-call into a loop means a redefinition
/// of the function *during* its own recursion is not observed by the
/// in-flight activation (the frame is reused). This matches what "tail
/// call optimization" means and what every TCO-doing CL does; the
/// pathological redefine-mid-self-recursion case is the only
/// observable difference.
pub(crate) fn rewrite_self_tail_calls(
    e: Expr,
    self_sym: u64,
    arity: u32,
) -> (Expr, bool) {
    match e {
        Expr::Progn(mut es) => {
            if let Some(last) = es.pop() {
                let (new_last, found) =
                    rewrite_self_tail_calls(last, self_sym, arity);
                es.push(new_last);
                (Expr::Progn(es), found)
            } else {
                (Expr::Progn(es), false)
            }
        }
        Expr::Let { bindings, body } => {
            let (nb, found) = rewrite_self_tail_calls(*body, self_sym, arity);
            (Expr::Let { bindings, body: Box::new(nb) }, found)
        }
        Expr::If(c, t, f) => {
            let (nt, ft) = rewrite_self_tail_calls(*t, self_sym, arity);
            let (nf, ff) = rewrite_self_tail_calls(*f, self_sym, arity);
            (Expr::If(c, Box::new(nt), Box::new(nf)), ft || ff)
        }
        Expr::Call { sym_word, args }
            if sym_word == self_sym && args.len() == arity as usize =>
        {
            (Expr::SelfTailNext { args }, true)
        }
        // Not a tail position we descend into, or not a self-call —
        // leave it untouched.
        other => (other, false),
    }
}

/// Return `true` when the symbol `sym_word` currently resolves to a
/// Lisp-compiled function (or is unbound, meaning it will be one).
/// Return `false` for known native shims.
///
/// Called only inside `instrument_tail_for_mv` at JIT-compile time.
/// The lookup is a single acquire-load on the function cell; it is
/// fast and never allocates.
fn call_is_lisp_compiled(sym_word: u64) -> bool {
    use ncl_runtime::{gc_function, gc_symbol, word::Word};
    let sym = Word::from_raw(sym_word);
    let fn_val = gc_symbol::function_acquire(sym);
    if fn_val.is_unbound() {
        // Symbol has no function binding yet. All native shims are
        // installed at startup before user code runs, so an unbound
        // symbol will certainly be defined by a Lisp defun later.
        return true;
    }
    if fn_val.tag() != ncl_runtime::word::Tag::Function {
        // Malformed cell — treat conservatively (wrap).
        return false;
    }
    gc_function::is_lisp_compiled(fn_val)
}

/// Push a name into `env` as either a plain local or a one-cell
/// box (if the body mutates it), and append the matching
/// initialiser to `bindings`. Boxing happens in lockstep so the
/// binding form stays consistent with what `(setq name ...)` will
/// expand to inside the body.
fn push_param_local(
    env: &mut LocalEnv,
    name: &Arc<str>,
    init: Expr,
    body_mutations: &HashSet<Arc<str>>,
    bindings: &mut Vec<Expr>,
) {
    let mutated = body_mutations.contains(name);
    if mutated {
        env.push_local_cell(Arc::clone(name));
        bindings.push(Expr::cons(init, Expr::Nil));
    } else {
        env.push_local(Arc::clone(name));
        bindings.push(init);
    }
}

/// `(apply fn arg1 arg2 ... tail-list)` — call `fn` with `arg1
/// arg2 …` as the prefix and the elements of `tail-list` spread as
/// additional args. Requires at least 2 args (fn + tail).
fn lower_apply(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.len() < 2 {
        return Err(CompileError::BadArity {
            head: "APPLY".into(),
            expected: "at least 2 (function and tail list)",
            got: args.len(),
        });
    }
    let fn_expr = lower_in_mut(&args[0], env, coord)?;
    let last_idx = args.len() - 1;
    let prefix: Result<Vec<_>, _> = args[1..last_idx]
        .iter()
        .map(|a| lower_in_mut(a, env, coord))
        .collect();
    let tail = lower_in_mut(&args[last_idx], env, coord)?;
    Ok(Expr::apply(fn_expr, prefix?, tail))
}

/// `(funcall fn arg1 arg2 ...)` — call an arbitrary function value.
fn lower_funcall(
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Err(CompileError::BadArity {
            head: "FUNCALL".into(),
            expected: "at least 1 (function)",
            got: 0,
        });
    }
    let fn_expr = lower_in_mut(&args[0], env, coord)?;
    let lowered_args: Result<Vec<_>, _> = args[1..]
        .iter()
        .map(|a| lower_in_mut(a, env, coord))
        .collect();
    Ok(Expr::funcall(fn_expr, lowered_args?))
}

/// Variadic comparison: `(< a b c)` → `(and (< a b) (< b c))`.
/// For 0 args returns T, 1 arg returns T, 2 args is a simple binary op.
/// Each intermediate value is evaluated exactly once (let-bound when
/// used in multiple comparisons).
fn chainable_cmp(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
    build: fn(Expr, Expr) -> Expr,
) -> Result<Expr, CompileError> {
    match args.len() {
        0 | 1 => Ok(Expr::True),
        2 => Ok(build(
            lower_in_mut(&args[0], env, coord)?,
            lower_in_mut(&args[1], env, coord)?,
        )),
        _ => {
            // Evaluate each operand exactly once, left-to-right, into a
            // fresh temp, then chain pairwise comparisons over the temps.
            // Cloning the lowered Exprs into adjacent comparisons would
            // emit every interior operand TWICE and double-run its side
            // effects — e.g. `(= (incf x) (incf y) (incf z))` must bump
            // each place once. (Same single-temp discipline as `lower_or`.)
            let lowered: Vec<Expr> = args
                .iter()
                .map(|a| lower_in_mut(a, env, coord))
                .collect::<Result<_, _>>()?;
            let cp = env.checkpoint();
            let idxs: Vec<usize> = (0..lowered.len())
                .map(|_| env.push_local(std::sync::Arc::from("__cmp_tmp__")))
                .collect();
            let mut pairs = Vec::with_capacity(idxs.len() - 1);
            for w in idxs.windows(2) {
                pairs.push(build(Expr::Local(w[0]), Expr::Local(w[1])));
            }
            env.restore(cp);
            // Chain with nested if (short-circuit AND semantics).
            let mut result = pairs.pop().unwrap();
            while let Some(p) = pairs.pop() {
                result = Expr::if_(p, result, Expr::Nil);
            }
            Ok(Expr::let_(lowered, result))
        }
    }
}

fn binary_op(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
    build: fn(Expr, Expr) -> Expr,
) -> Result<Expr, CompileError> {
    if args.len() != 2 {
        return Err(CompileError::BadArity {
            head: head.to_string(),
            expected: "2",
            got: args.len(),
        });
    }
    Ok(build(
        lower_in_mut(&args[0], env, coord)?,
        lower_in_mut(&args[1], env, coord)?,
    ))
}

fn unary_op(
    head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
    build: fn(Expr) -> Expr,
) -> Result<Expr, CompileError> {
    if args.len() != 1 {
        return Err(CompileError::BadArity {
            head: head.to_string(),
            expected: "1",
            got: args.len(),
        });
    }
    Ok(build(lower_in_mut(&args[0], env, coord)?))
}

fn fold_arithmetic(
    _head: &str,
    args: &[Value],
    env: &mut LocalEnv,
    coord: &Arc<GcCoordinator>,
    identity: i64,
    combine: fn(Expr, Expr) -> Expr,
) -> Result<Expr, CompileError> {
    if args.is_empty() {
        return Ok(Expr::Const(identity));
    }
    let mut acc = lower_in_mut(&args[0], env, coord)?;
    for a in &args[1..] {
        acc = combine(acc, lower_in_mut(a, env, coord)?);
    }
    Ok(acc)
}

fn list_to_vec(v: &Value) -> Result<Vec<Value>, CompileError> {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => return Ok(out),
            Value::Cons(c) => {
                out.push(c.car.clone());
                cur = c.cdr.clone();
            }
            other => {
                return Err(CompileError::ImproperList(format!("{other:?}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncl_reader::read_one;
    use ncl_runtime::{GcConfig, GcCoordinator};

    fn small_coord() -> Arc<GcCoordinator> {
        GcCoordinator::new(GcConfig {
            young_bytes: 16 * 1024,
            old_bytes: 16 * 1024,
            static_bytes: 8 * 1024,
            tlab_cells: 64,
        })
    }

    fn lower_top(src: &str) -> Result<Expr, CompileError> {
        let v = read_one(src).expect("read");
        lower(&v, &small_coord())
    }

    #[test]
    fn fixnum_literal() {
        assert_eq!(lower_top("42").unwrap(), Expr::Const(42));
    }

    #[test]
    fn binary_add() {
        assert_eq!(
            lower_top("(+ 1 2)").unwrap(),
            Expr::add(Expr::Const(1), Expr::Const(2)),
        );
    }

    #[test]
    fn unknown_head_lowers_to_call() {
        let coord = small_coord();
        let v = read_one("(foo 1 2)").unwrap();
        let e = lower(&v, &coord).unwrap();
        match e {
            Expr::Call { sym_word, args } => {
                // The symbol got interned.
                let interned = coord.find_interned("FOO").expect("interned");
                assert_eq!(sym_word, interned.raw());
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], Expr::Const(1));
                assert_eq!(args[1], Expr::Const(2));
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn param_in_function_body() {
        let coord = small_coord();
        let env = LocalEnv::with_params(&[Arc::from("X")]);
        let v = read_one("(+ x x)").unwrap();
        let e = lower_in(&v, &env, &coord).unwrap();
        // Both `x` references should be Param(0).
        assert_eq!(
            e,
            Expr::add(Expr::Param(0), Expr::Param(0)),
        );
    }

    #[test]
    fn param_priority_over_t() {
        // A parameter named T shadows the truth-value literal.
        let coord = small_coord();
        let env = LocalEnv::with_params(&[Arc::from("T")]);
        let e = lower_in(&read_one("t").unwrap(), &env, &coord).unwrap();
        assert_eq!(e, Expr::Param(0));
    }

    #[test]
    fn nested_defun_lowers_to_install_plus_name() {
        // `(defun ...)` in non-top-level position now lowers (it used
        // to error with BadDefun). The expansion is
        // (progn (%set-symbol-function 'foo (lambda …)) 'foo) — we
        // don't assert the exact Expr shape here (too much detail
        // about closure-env layout) but we do require the lowering
        // succeeds. The integration tests in
        // `nested_defun_installs_function_and_returns_name` and
        // `nested_defun_captures_enclosing_let` cover the runtime
        // behaviour end-to-end.
        let coord = small_coord();
        let v = read_one("(if t (defun foo () 1) 2)").unwrap();
        lower(&v, &coord).expect("nested defun should lower");
    }

    #[test]
    fn nested_defun_with_non_symbol_name_errors() {
        // The (setf NAME) mangling lives in match_defun_like and only
        // fires at top level. A nested-position defun with a non-symbol
        // name should fail loudly so users don't accidentally try
        // (defun (setf foo) …) inside a let and get silent garbage.
        let coord = small_coord();
        let v = read_one("(progn (defun (setf foo) (v) v))").unwrap();
        let r = lower(&v, &coord);
        assert!(matches!(r, Err(CompileError::BadDefun(_))),
                "expected BadDefun, got {r:?}");
    }

    #[test]
    fn eq_and_if_still_work() {
        assert_eq!(
            lower_top("(eq 1 2)").unwrap(),
            Expr::eq(Expr::Const(1), Expr::Const(2)),
        );
        assert_eq!(
            lower_top("(if t 1 2)").unwrap(),
            Expr::if_(Expr::True, Expr::Const(1), Expr::Const(2)),
        );
    }

    #[test]
    fn nil_lowers_to_nil() {
        assert_eq!(lower_top("nil").unwrap(), Expr::Nil);
        assert_eq!(lower_top("()").unwrap(), Expr::Nil);
    }
}
