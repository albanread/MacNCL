//! Macroexpansion: walk a `Value` tree, replace any cons form whose
//! head names a registered macro by calling the macro's compiled
//! function and recursing on the result.
//!
//! Macros store JIT-compiled functions in the coordinator's macro
//! registry (separate from symbol function cells). At expansion
//! time we:
//!   1. Convert each unevaluated argument form from `Value` to a
//!      runtime `Word` (cons-cell tree).
//!   2. Call the macro function via the standard JIT calling
//!      convention.
//!   3. Convert the returned `Word` back into a `Value`.
//!   4. Recurse on the result — the expansion may itself contain
//!      macro calls.
//!
//! `(quote ...)` is opaque: we do not recurse inside quoted data.
//! Every other cons form, whether a special form or a function
//! call, has its sub-forms walked.

use std::sync::Arc;

use ncl_runtime::{
    gc_function, gc_string, sym_names, symbol::Symbol, universe, GcCoordinator,
    MutatorState, Tag, Value, Word,
};

use crate::EvalError;

/// Convert a compile-time `Value` into a runtime `Word`. Cons cells
/// and strings allocate on the calling thread's young heap; symbols
/// go through the coordinator's intern table; T and NIL get their
/// canonical immediate words.
pub fn value_to_word(
    v: &Value,
    m: &mut MutatorState,
    coord: &Arc<GcCoordinator>,
) -> Result<Word, EvalError> {
    Ok(match v {
        Value::Nil => Word::NIL,
        Value::Fixnum(n) => Word::fixnum(*n),
        Value::Bignum(s) => {
            // Allocate a fresh bignum in static — same as
            // lower's Value::Bignum path. This is on the
            // macro-input shuttle so the bignum literal
            // survives across passes.
            ncl_runtime::bignum::alloc_bignum_in_static(
                coord.static_area(), coord, s.as_str(),
            )
            .ok_or_else(|| EvalError::Compile(
                crate::CompileError::NotImplemented(format!(
                    "static area exhausted while allocating bignum {s} in macro input"
                ))
            ))?
        }
        Value::Float(f) => {
            ncl_runtime::float::alloc_float_in_static(
                coord.static_area(), coord, *f,
            )
            .ok_or_else(|| EvalError::Compile(
                crate::CompileError::NotImplemented(
                    "static area exhausted while allocating float in macro input".into()
                )
            ))?
        }
        Value::Ratio(n, d) => {
            ncl_runtime::ratio::alloc_ratio_in_static(
                coord.static_area(), coord, n.as_str(), d.as_str(),
            )
            .ok_or_else(|| EvalError::Compile(
                crate::CompileError::NotImplemented(format!(
                    "static area exhausted while allocating ratio {n}/{d} in macro input"
                ))
            ))?
        }
        Value::Vector(items) => {
            // Allocate a static-area Vector and fill each cell with
            // the recursively-converted Word of each item. Matches
            // the lower.rs Value::Vector branch.
            let n = items.len() as u32;
            let header_ptr = coord
                .static_area()
                .try_alloc_with_header(ncl_runtime::HeapType::Vector, n)
                .ok_or_else(|| EvalError::Compile(
                    crate::CompileError::NotImplemented(
                        "static area exhausted while allocating vector in macro input".into()
                    )
                ))?;
            let p = header_ptr.as_ptr() as *mut u64;
            for (i, item) in items.iter().enumerate() {
                let w = value_to_word(item, m, coord)?;
                unsafe { *p.add(1 + i) = w.raw(); }
            }
            ncl_runtime::Word::from_ptr(
                p as *const u8, ncl_runtime::Tag::Vector,
            )
        }
        Value::Char(c) => Word::char(*c),
        Value::Symbol(s) if &*s.name == "T" => Word::T,
        Value::Symbol(s) => crate::lower::intern_value_symbol(coord, s),
        Value::String(s) => gc_string::alloc_string_in_young(m, s.as_str()),
        Value::Cons(c) => {
            let car = value_to_word(&c.car, m, coord)?;
            let cdr = value_to_word(&c.cdr, m, coord)?;
            m.alloc_cons(car, cdr)
        }
        other => {
            return Err(EvalError::Compile(crate::CompileError::NotImplemented(
                format!("value_to_word: unsupported value {other:?}"),
            )));
        }
    })
}

/// Convert a runtime `Word` back into a compile-time `Value`. Used
/// to consume a macro's expansion so that the lowering pass can
/// process it as ordinary source.
pub fn word_to_value(w: Word) -> Result<Value, EvalError> {
    if w.is_nil() {
        return Ok(Value::Nil);
    }
    if w.is_t() {
        return Ok(Value::Symbol(Symbol::fresh_uninterned(Arc::from("T"))));
    }
    Ok(match w.tag() {
        Tag::Fixnum => Value::Fixnum(w.as_fixnum().unwrap()),
        Tag::Cons => {
            let p = w.as_ptr::<u64>(Tag::Cons).expect("cons");
            let car = word_to_value(Word::from_raw(unsafe { *p }))?;
            let cdr = word_to_value(Word::from_raw(unsafe { *p.add(1) }))?;
            Value::cons(car, cdr)
        }
        Tag::Symbol => {
            let name = sym_names::lookup(w.raw()).unwrap_or_else(|| {
                Arc::from(format!("<sym {:#x}>", w.raw()).as_str())
            });
            // Keywords are interned with a colon prefix on the
            // runtime side. When converting back to a Value we need
            // to restore the KEYWORD home package, otherwise the
            // lowering pass treats the result as a value-cell load
            // of a symbol named ":FOO" instead of a self-evaluating
            // keyword.
            if let Some(stripped) = name.strip_prefix(':') {
                let kw = universe().find_package("KEYWORD")
                    .expect("KEYWORD package missing");
                Value::Symbol(kw.intern_external(stripped))
            } else {
                Value::Symbol(Symbol::fresh_uninterned(name))
            }
        }
        Tag::Immediate => match w.as_char() {
            Some(c) => Value::Char(c),
            None => {
                return Err(EvalError::Compile(crate::CompileError::NotImplemented(
                    format!("word_to_value: unknown immediate {:#x}", w.raw()),
                )));
            }
        },
        Tag::String => {
            let s: String = gc_string::chars_of(w).collect();
            Value::String(Arc::new(s))
        }
        Tag::Vector => {
            // Bignums, floats, and ratios share Tag::Vector.
            // Discriminate via header type and rebuild the
            // corresponding Value variant so macroexpansion can
            // carry them through.
            if ncl_runtime::bignum::is_bignum(w) {
                let s = ncl_runtime::bignum::bignum_to_decimal(w);
                return Ok(Value::Bignum(Arc::new(s)));
            }
            if ncl_runtime::float::is_float(w) {
                return Ok(Value::Float(ncl_runtime::float::float_value(w)));
            }
            if ncl_runtime::ratio::is_ratio(w) {
                let q = ncl_runtime::ratio::ratio_to_bigrational(w);
                return Ok(Value::Ratio(
                    Arc::new(q.numer().to_string()),
                    Arc::new(q.denom().to_string()),
                ));
            }
            if ncl_runtime::complex::is_complex(w) {
                // Complex doesn't currently have a Value variant —
                // we'd need to round-trip through the reader's #C
                // form. For now, signal: complex values shouldn't
                // appear in macro EXPANSIONS in practice (they live
                // as values at runtime, not source). If this fires
                // in real code, add a Value::Complex variant.
                return Err(EvalError::Compile(crate::CompileError::NotImplemented(
                    "word_to_value: complex number in macro expansion not supported".into(),
                )));
            }
            // Plain Tag::Vector heap object — walk cells and rebuild
            // Value::Vector. Used when a macro returns a vector
            // literal expansion.
            let p = w.as_ptr::<u64>(Tag::Vector)
                .ok_or_else(|| EvalError::Compile(
                    crate::CompileError::NotImplemented(
                        "word_to_value: vector ptr was null".into()
                    )
                ))?;
            let header = ncl_runtime::heap::HeapHeader::from_raw(unsafe { *p });
            let n = header.length_cells() as usize;
            let mut items = Vec::with_capacity(n);
            for i in 0..n {
                let cell = unsafe { *p.add(1 + i) };
                items.push(word_to_value(Word::from_raw(cell))?);
            }
            return Ok(Value::Vector(Arc::new(items)));
        }
        other => {
            return Err(EvalError::Compile(crate::CompileError::NotImplemented(
                format!("word_to_value: unsupported tag {other:?}"),
            )));
        }
    })
}

/// Recursively walk `v`, expanding any macro calls. Returns a new
/// `Value` with all reachable expansions performed.
///
/// Special forms whose structure includes non-evaluated positions
/// — `quote`, `defun`, `defmacro`, `lambda`, `let`, `let*` — are
/// recognised so we don't try to "expand" the names in their
/// binding lists. (Without this, `(let ((when 1)) ...)` would walk
/// into the binding pair `(when 1)`, see `when` as the head, and
/// erroneously invoke the `when` macro on the literal `1`.)
///
/// Every other cons form is treated uniformly: if its head names a
/// registered macro, call it and recurse on the result; otherwise
/// walk all subforms.
// ---- lexical macro environment (macrolet / symbol-macrolet) ---------------
//
// A stack of frames threaded as a thread-local rather than a parameter, so
// the existing recursive macroexpand_all signature stays intact. Each
// macrolet / symbol-macrolet pushes a frame around the expansion of its
// body and pops it after. Lookups walk innermost-first.
//
// Scoping caveat: inner LET/LAMBDA bindings that shadow a symbol-macro name
// are not removed from the env (no per-binding shadow frames yet), so a
// lexical rebinding of a symbol-macro'd name inside the body would still see
// the macro. Rare in practice; the common (and ANSI-tested) uses don't rely
// on it.

struct MacroFrame {
    /// local macros: name -> compiled expander Function word.
    macros: Vec<(Arc<str>, Word)>,
    /// symbol macros: name -> expansion form.
    symbol_macros: Vec<(Arc<str>, Value)>,
}

thread_local! {
    static MACRO_ENV: std::cell::RefCell<Vec<MacroFrame>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn macro_env_push(frame: MacroFrame) {
    MACRO_ENV.with(|e| e.borrow_mut().push(frame));
}
fn macro_env_pop() {
    MACRO_ENV.with(|e| { e.borrow_mut().pop(); });
}

/// RAII guard: pushes a `MacroFrame` and pops it when dropped. Using a
/// guard (instead of a manual push/pop pair) makes the thread-local
/// `MACRO_ENV` leak-proof — the frame is removed on EVERY exit path,
/// including early `?` returns and panics. A leaked frame would make
/// `any_symbol_macros()` / `lookup_*_macro` see stale bindings in
/// later top-level forms, which previously caused ordinary `setq`s to
/// be misrewritten.
struct FrameGuard;
impl Drop for FrameGuard {
    fn drop(&mut self) {
        macro_env_pop();
    }
}
#[must_use]
fn macro_env_push_guard(frame: MacroFrame) -> FrameGuard {
    macro_env_push(frame);
    FrameGuard
}
pub(crate) fn lookup_local_macro(name: &str) -> Option<Word> {
    MACRO_ENV.with(|e| {
        for frame in e.borrow().iter().rev() {
            for (n, w) in frame.macros.iter().rev() {
                if &**n == name {
                    return Some(*w);
                }
            }
        }
        None
    })
}

/// Bridge hook registered with ncl-runtime so that `macro-function`
/// (and macroexpand with a non-nil `&environment`) can resolve a name
/// against the live macrolet-local macros. Called — on the same thread
/// that is running macroexpansion — from the runtime shim; reads the
/// thread-local `MACRO_ENV`. Returns the local expander Function Word,
/// or NIL's raw bits when the name has no lexical macro binding.
pub(crate) extern "C" fn local_macro_hook(ptr: *const u8, len: usize) -> u64 {
    let name = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len))
    };
    lookup_local_macro(name)
        .map(|w| w.raw())
        .unwrap_or_else(|| Word::NIL.raw())
}
fn lookup_symbol_macro(name: &str) -> Option<Value> {
    MACRO_ENV.with(|e| {
        for frame in e.borrow().iter().rev() {
            for (n, expansion) in frame.symbol_macros.iter().rev() {
                if &**n == name {
                    return Some(expansion.clone());
                }
            }
        }
        None
    })
}
/// True if `v` is `(SETQ var val ...)` where at least one assigned
/// *variable* (an even-indexed argument) is currently a symbol macro.
/// Only such a SETQ needs the SETQ→SETF rewrite; an ordinary SETQ must
/// be left alone so it doesn't bounce SETQ→SETF→SETQ forever (SETF of a
/// plain symbol expands straight back to SETQ).
fn setq_has_symbol_macro_target(v: &Value) -> bool {
    let mut cur = cdr_of(v);
    let mut idx = 0usize;
    while let Value::Cons(c) = &cur {
        if idx % 2 == 0 {
            if let Value::Symbol(s) = &c.car {
                if lookup_symbol_macro(&s.name).is_some() {
                    return true;
                }
            }
        }
        let next = c.cdr.clone();
        cur = next;
        idx += 1;
    }
    false
}

/// Intern NAME and return it as a `Value::Symbol` (for synthesising forms
/// like `(setf …)` / `(progn …)` during expansion).
fn sym_value(coord: &Arc<GcCoordinator>, name: &str) -> Value {
    let w = coord.intern(name).raw();
    word_to_value(Word::from_raw(w)).unwrap_or(Value::Nil)
}

pub fn macroexpand_all(
    v: &Value,
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
) -> Result<Value, EvalError> {
    match v {
        // A bare symbol in a value position may be a symbol-macro.
        Value::Symbol(s) => {
            if let Some(expansion) = lookup_symbol_macro(&s.name) {
                return macroexpand_all(&expansion, coord, mutator);
            }
            return Ok(v.clone());
        }
        Value::Cons(_) => {}
        _ => return Ok(v.clone()),
    }
    if let Some(head_name) = head_symbol_name(v) {
        match &*head_name {
            "QUOTE" => return Ok(v.clone()),
            // (declare ...) carries compile-time metadata (type / ignore
            // / special declarations) that the lowering reads. It must
            // survive macroexpansion intact — NOT be expanded by the
            // no-op `declare` macro (which returns nil) — or nested
            // declares (e.g. inside `let`) would be destroyed before
            // lower_let sees them. A stray declare that reaches lowering
            // is neutralised there (DECLARE => nil). See
            // docs/performance-unbox-float.md Sprint 2.
            "DECLARE" => return Ok(v.clone()),
            "BACKQUOTE" => {
                // ` form — desugar to a tree of cons/list/append/
                // quote, then macroexpand the result (it may contain
                // macros; e.g. `(when ,x ,@body) inside a defmacro).
                let items = list_to_vec(v)?;
                if items.len() != 2 {
                    return Err(EvalError::Compile(
                        crate::CompileError::NotImplemented(
                            "backquote takes exactly one form".into(),
                        ),
                    ));
                }
                let desugared = expand_quasiquote(&items[1])?;
                return macroexpand_all(&desugared, coord, mutator);
            }
            "UNQUOTE" | "UNQUOTE-SPLICING" | "UNQUOTE-NSPLICING" => {
                return Err(EvalError::Compile(
                    crate::CompileError::NotImplemented(format!(
                        "{} only valid inside backquote",
                        head_name
                    )),
                ));
            }
            "DEFUN" | "DEFMACRO" => {
                // (defun/defmacro NAME PARAMS body...) — leave name
                // and params alone, expand body forms.
                let items = list_to_vec(v)?;
                return rebuild_with_body_skip(&items, 3, coord, mutator);
            }
            "LAMBDA" => {
                // (lambda PARAMS body...) — leave params alone,
                // expand body forms.
                let items = list_to_vec(v)?;
                return rebuild_with_body_skip(&items, 2, coord, mutator);
            }
            "LET" => {
                // (let BINDINGS body...). BINDINGS is a list of
                // (name init) pairs; we expand each init but leave
                // the binding names alone. `let*` is a user-Lisp
                // macro (in core.lisp) that desugars to nested
                // `let`s, so we don't need a special case for it
                // here — it'll fall through to the macro-lookup
                // branch below.
                let items = list_to_vec(v)?;
                return rebuild_let(&items, coord, mutator);
            }
            "SYMBOL-MACROLET" => {
                // (symbol-macrolet ((name expansion) ...) body...)
                let items = list_to_vec(v)?;
                if items.len() < 2 {
                    return Ok(v.clone());
                }
                let bindings = list_to_vec(&items[1])?;
                let mut sym_macros = Vec::new();
                for b in &bindings {
                    let pair = list_to_vec(b)?;
                    if pair.len() >= 2 {
                        if let Value::Symbol(s) = &pair[0] {
                            sym_macros.push((Arc::clone(&s.name), pair[1].clone()));
                        }
                    }
                }
                let _frame = macro_env_push_guard(MacroFrame {
                    macros: Vec::new(),
                    symbol_macros: sym_macros,
                });
                let mut progn = vec![sym_value(coord, "PROGN")];
                for body in &items[2..] {
                    progn.push(macroexpand_all(body, coord, mutator)?);
                }
                return Ok(Value::list(progn));
            }
            "MACROLET" => {
                // (macrolet ((name lambda-list body...) ...) body...)
                // Compile each local macro's expander into a Function
                // word, then macroexpand the body with those expanders
                // in scope. Per CLHS the expander bodies see the
                // surrounding macro environment (outer macrolet /
                // symbol-macrolet) but NOT each other, so we compile
                // them BEFORE pushing the new frame — exactly like the
                // parallel scoping of `flet`.
                let items = list_to_vec(v)?;
                if items.len() < 2 {
                    return Ok(v.clone());
                }
                let defs = list_to_vec(&items[1])?;
                let mut macros: Vec<(Arc<str>, Word)> = Vec::new();
                for def in &defs {
                    let parts = list_to_vec(def)?;
                    if parts.len() < 2 {
                        return Err(EvalError::Compile(
                            crate::CompileError::NotImplemented(
                                "malformed macrolet binding".into(),
                            ),
                        ));
                    }
                    let name = match &parts[0] {
                        Value::Symbol(s) => Arc::clone(&s.name),
                        _ => {
                            return Err(EvalError::Compile(
                                crate::CompileError::NotImplemented(
                                    "macrolet macro name must be a symbol".into(),
                                ),
                            ))
                        }
                    };
                    let params = crate::parse_param_list_inner(&parts[1])
                        .map_err(|e| {
                            EvalError::Compile(crate::CompileError::BadDefun(e))
                        })?;
                    let body = &parts[2..];
                    let expander = crate::compile_function_raw(
                        coord, mutator, &name, &params, body,
                    )?;
                    macros.push((name, expander));
                }
                let _frame = macro_env_push_guard(MacroFrame {
                    macros,
                    symbol_macros: Vec::new(),
                });
                let mut progn = vec![sym_value(coord, "PROGN")];
                for body in &items[2..] {
                    progn.push(macroexpand_all(body, coord, mutator)?);
                }
                return Ok(Value::list(progn));
            }
            // SETQ where at least one target is a symbol macro: rewrite
            // each such pair to `(setf <expansion> val)` and leave the
            // ordinary pairs as `(setq var val)`. Gating on *this* SETQ
            // having a symbol-macro target (not merely any symbol macro
            // being in scope) is essential: an ordinary SETQ left as
            // SETQ would otherwise re-enter this arm, expand to SETF,
            // and SETF of a plain symbol expands straight back to SETQ —
            // an infinite SETQ→SETF→SETQ loop. The emitted plain SETQs
            // have no symbol-macro target, so they fall through to the
            // ordinary subform walk on re-expansion.
            "SETQ" if setq_has_symbol_macro_target(v) => {
                let items = list_to_vec(v)?;
                let pairs = &items[1..];
                let mut forms = vec![sym_value(coord, "PROGN")];
                let mut i = 0;
                while i + 1 < pairs.len() {
                    let val = macroexpand_all(&pairs[i + 1], coord, mutator)?;
                    match &pairs[i] {
                        Value::Symbol(s) if lookup_symbol_macro(&s.name).is_some() => {
                            let place = lookup_symbol_macro(&s.name).unwrap();
                            forms.push(Value::list(vec![
                                sym_value(coord, "SETF"),
                                place,
                                val,
                            ]));
                        }
                        other => {
                            forms.push(Value::list(vec![
                                sym_value(coord, "SETQ"),
                                other.clone(),
                                val,
                            ]));
                        }
                    }
                    i += 2;
                }
                return macroexpand_all(&Value::list(forms), coord, mutator);
            }
            _ => {}
        }
        // Macro head? Local macros (macrolet) shadow global ones.
        if let Some(macro_fn) =
            lookup_local_macro(&head_name).or_else(|| coord.macro_for(&head_name))
        {
            let args_value = cdr_of(v);
            let args_vec = list_to_vec(&args_value)?;
            let mut arg_words: Vec<u64> = Vec::with_capacity(args_vec.len());
            for a in &args_vec {
                arg_words.push(value_to_word(a, mutator, coord)?.raw());
            }
            // Defensive arity check. A macro expander's prologue reads
            // its required positional parameters straight out of the
            // args array; calling it with fewer would read PAST the end
            // into uninitialised memory, whose bytes can carry any tag —
            // a forwarding pointer, a bogus symbol — and then crash deep
            // in `word_to_value` or at runtime. Report a clean compile
            // error instead. (`arity` is the required-param count set by
            // `compile_function_raw`.)
            let required = gc_function::arity(macro_fn) as usize;
            if arg_words.len() < required {
                return Err(EvalError::Compile(crate::CompileError::MacroError(
                    format!(
                        "macro ({head_name} …) called with {} argument(s) \
                         but requires at least {}",
                        arg_words.len(),
                        required
                    ),
                )));
            }
            let env = gc_function::env(macro_fn);
            let code = gc_function::code_ptr(macro_fn);
            let f: gc_function::LispCodeFn =
                unsafe { std::mem::transmute(code) };
            // Guard the expander call: bump HANDLER_DEPTH so any
            // condition it signals (undefined function, type error,
            // explicit `error`) defers and is reported as a clean
            // compile error instead of calling std::process::abort()
            // — which on Windows looks exactly like a stack overflow.
            let guard = ncl_runtime::abi::condition_guard_enter();
            let result_word_raw = unsafe {
                f(
                    mutator as *mut _,
                    env.raw(),
                    arg_words.as_ptr(),
                    arg_words.len() as u64,
                )
            };
            if let Some(cond_raw) = ncl_runtime::abi::condition_guard_exit(guard) {
                let msg = ncl_runtime::format_word_aesthetic(Word::from_raw(cond_raw));
                return Err(EvalError::Compile(crate::CompileError::MacroError(
                    format!("while expanding macro ({head_name} …): {msg}"),
                )));
            }
            let result_value = word_to_value(Word::from_raw(result_word_raw))?;
            return macroexpand_all(&result_value, coord, mutator);
        }
    }
    // Non-macro cons with no special structure: walk subforms,
    // preserving any dotted tail.
    let (cars, dotted_tail) = list_to_vec_or_dotted(v);
    let mut acc = match dotted_tail {
        None => Value::Nil,
        Some(t) => macroexpand_all(&t, coord, mutator)?,
    };
    for car in cars.iter().rev() {
        let expanded = macroexpand_all(car, coord, mutator)?;
        acc = Value::cons(expanded, acc);
    }
    Ok(acc)
}

/// Rebuild a form like `(head ... a1 a2 a3)` where the first
/// `keep` items pass through unchanged and the rest are
/// macroexpanded body forms.
fn rebuild_with_body_skip(
    items: &[Value],
    keep: usize,
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
) -> Result<Value, EvalError> {
    let mut out: Vec<Value> = items.iter().take(keep).cloned().collect();
    for body in &items[keep.min(items.len())..] {
        out.push(macroexpand_all(body, coord, mutator)?);
    }
    Ok(Value::list(out))
}

/// Rebuild a `(let BINDINGS body...)` or `(let* BINDINGS body...)`
/// form: each binding's init expression gets macroexpanded but the
/// binding's name does not; body forms get expanded normally.
fn rebuild_let(
    items: &[Value],
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
) -> Result<Value, EvalError> {
    if items.len() < 2 {
        // Malformed; leave it for the lowering pass to error on.
        return Ok(Value::list(items.to_vec()));
    }
    let head = items[0].clone();
    let bindings_form = &items[1];
    let new_bindings = match list_to_vec(bindings_form) {
        Ok(bindings) => {
            let mut out = Vec::with_capacity(bindings.len());
            for b in &bindings {
                match list_to_vec(b) {
                    Ok(pair) if pair.len() == 2 => {
                        let init = macroexpand_all(&pair[1], coord, mutator)?;
                        out.push(Value::list(vec![pair[0].clone(), init]));
                    }
                    _ => out.push(b.clone()),
                }
            }
            Value::list(out)
        }
        Err(_) => bindings_form.clone(),
    };
    let mut new_items = vec![head, new_bindings];
    for body in &items[2..] {
        new_items.push(macroexpand_all(body, coord, mutator)?);
    }
    Ok(Value::list(new_items))
}

// ---- helpers --------------------------------------------------------------

fn head_symbol_name(v: &Value) -> Option<Arc<str>> {
    let Value::Cons(c) = v else { return None; };
    match &c.car {
        Value::Symbol(s) => Some(Arc::clone(&s.name)),
        _ => None,
    }
}

fn cdr_of(v: &Value) -> Value {
    match v {
        Value::Cons(c) => c.cdr.clone(),
        _ => Value::Nil,
    }
}

fn list_to_vec(v: &Value) -> Result<Vec<Value>, EvalError> {
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
                return Err(EvalError::Compile(crate::CompileError::ImproperList(
                    format!("{other:?}"),
                )));
            }
        }
    }
}

// ---- Backquote / quasiquote desugaring ------------------------------------
//
// Classical CL transformation: `body becomes a tree of cons / list /
// append / quote calls that, when evaluated, reconstructs the body
// with each `,x` replaced by x's value and each `,@x` spliced in.
//
//   `atom         → 'atom        (or self-quoting)
//   `,x           → x
//   `(a b ,c)     → (cons 'a (cons 'b (cons c nil)))
//   `(a ,@xs b)   → (cons 'a (append xs (cons 'b nil)))
//   `(,@xs . ys)  → (append xs ys)
//
// Nested backquotes are not supported in this slice — they require
// a level counter and rules around how `, and `,@ behave at each
// depth. Single-level covers the vast majority of macro-writing use
// cases.

/// Top-level entry: transform a `body` of `` `body `` into a form
/// that, when evaluated, reconstructs `body` with unquoted parts
/// substituted.
fn expand_quasiquote(form: &Value) -> Result<Value, EvalError> {
    match form {
        // Self-quoting atoms — leave bare; they evaluate to themselves.
        Value::Nil
        | Value::Fixnum(_)
        | Value::Char(_)
        | Value::String(_)
        | Value::Float(_) => Ok(form.clone()),
        // T self-evaluates.
        Value::Symbol(s) if &*s.name == "T" => Ok(form.clone()),
        // Other symbols: '(quote sym).
        Value::Symbol(_) => Ok(quote_form(form.clone())),
        Value::Cons(_) => {
            // Direct `,x → x.
            if let Some(inner) = match_form(form, "UNQUOTE") {
                return Ok(inner);
            }
            // Direct `,@x at top-level — error: splicing only valid in lists.
            if match_form(form, "UNQUOTE-SPLICING").is_some() {
                return Err(EvalError::Compile(
                    crate::CompileError::NotImplemented(
                        "unquote-splicing not allowed outside list".into(),
                    ),
                ));
            }
            if match_form(form, "BACKQUOTE").is_some() {
                return Err(EvalError::Compile(
                    crate::CompileError::NotImplemented(
                        "nested backquote not yet supported".into(),
                    ),
                ));
            }
            // Otherwise it's a (possibly dotted) list — walk elements.
            expand_quasiquote_list(form)
        }
        // Vectors etc. — not yet supported in backquote; quote them.
        _ => Ok(quote_form(form.clone())),
    }
}

/// Walk a cons structure cell-by-cell. The reader normalises
/// `(a . (b c))` into `(a b c)`, so we can't distinguish a dotted
/// tail at the surface level — but `(a . ,x)` is structurally
/// `(a . (unquote x))`, and Steele's CLtL algorithm recognises that
/// pattern by inspecting the cdr at each step:
///
///   if cdr matches (unquote x):  the dotted-tail expansion is x
///   if cdr matches (unquote-splicing x): same idea (rare)
///   otherwise: recurse on cdr
///
/// Combined with the splice-in-car case (which uses APPEND
/// regardless of position), this gives the standard backquote
/// behaviour including dotted unquotes.
fn expand_quasiquote_list(form: &Value) -> Result<Value, EvalError> {
    let Value::Cons(c) = form else {
        return expand_quasiquote(form);
    };
    let car = &c.car;
    let cdr = &c.cdr;

    if let Some(spliced) = match_form(car, "UNQUOTE-SPLICING") {
        let cdr_form = expand_quasiquote_cdr(cdr)?;
        return Ok(mk_call("APPEND", vec![spliced, cdr_form]));
    }
    let car_expanded = if let Some(unquoted) = match_form(car, "UNQUOTE") {
        unquoted
    } else {
        expand_quasiquote(car)?
    };
    let cdr_expanded = expand_quasiquote_cdr(cdr)?;
    Ok(mk_call("CONS", vec![car_expanded, cdr_expanded]))
}

/// Expand the cdr position of a cons. Recognises `(unquote x)` in
/// cdr position as a dotted unquote — meaning the cdr is the
/// raw value of x, not a list ending with x.
fn expand_quasiquote_cdr(cdr: &Value) -> Result<Value, EvalError> {
    if let Some(unquoted) = match_form(cdr, "UNQUOTE") {
        return Ok(unquoted);
    }
    if let Some(spliced) = match_form(cdr, "UNQUOTE-SPLICING") {
        // `(... . ,@x) — uncommon but well-defined: cdr is x.
        return Ok(spliced);
    }
    expand_quasiquote(cdr)
}

/// If `v` is `(name x)` for the given `name`, return `x`.
fn match_form(v: &Value, name: &str) -> Option<Value> {
    let Value::Cons(c) = v else { return None; };
    let Value::Symbol(head) = &c.car else { return None; };
    if &*head.name != name {
        return None;
    }
    let Value::Cons(rest) = &c.cdr else { return None; };
    if !matches!(rest.cdr, Value::Nil) {
        return None;
    }
    Some(rest.car.clone())
}

fn quote_form(v: Value) -> Value {
    mk_call("QUOTE", vec![v])
}

fn mk_call(head: &str, args: Vec<Value>) -> Value {
    let head_sym = Value::Symbol(Symbol::fresh_uninterned(Arc::from(head)));
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push(head_sym);
    all.extend(args);
    Value::list(all)
}

/// Walk a (possibly dotted) cons chain, collecting cars and
/// reporting the tail. `None` means the chain ended with NIL
/// (a proper list); `Some(v)` means it ended dotted with `v`.
fn list_to_vec_or_dotted(v: &Value) -> (Vec<Value>, Option<Value>) {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => return (out, None),
            Value::Cons(c) => {
                out.push(c.car.clone());
                cur = c.cdr.clone();
            }
            other => return (out, Some(other)),
        }
    }
}
