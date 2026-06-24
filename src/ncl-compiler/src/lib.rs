//! Lisp compiler: lowers `Value` (from the reader) through `Expr`
//! (in `ncl-ir`) down to LLVM IR (via `ncl-llvm`).
//!
//! Node 1: arithmetic, cons/car/cdr, eq/if/quote.
//! Node 2 (this commit): multi-form evaluation, `defun`, function
//! calls, recursive functions.
//!
//! The user-facing entry point is `eval_str(src)` for one-shot
//! evaluation. State that needs to persist across calls — the GC
//! coordinator, the mutator, defun'd functions — lives in a
//! `Session`.

use std::cell::Cell;
use std::sync::Arc;

use ncl_runtime::{
    format_word, gc_function, GcConfig, GcCoordinator, MutatorState, Value, Word,
};
use new_asm;

mod inline;
pub mod lower;
pub mod macroexpand;
mod optimize;

thread_local! {
    /// Pointer to the active Session for this thread. Set by
    /// `Session::activate()` and read by `eval_string_shim` so a
    /// Lisp call to `(eval-string ...)` can route into the running
    /// session's eval. Null when no Session has been activated;
    /// the shim returns a Lisp error in that case.
    static ACTIVE_SESSION: Cell<*mut Session> =
        const { Cell::new(std::ptr::null_mut()) };
}

pub use lower::{lower, lower_in, CompileError, LocalEnv};
pub use macroexpand::{macroexpand_all, value_to_word, word_to_value};

// ─── Startup timing ─────────────────────────────────────────────────────────
//
// Enabled by setting the environment variable `NCL_STARTUP_TIMING=1` before
// launching `ncl`. Reports to stderr:
//
//   [timing] native-install: Nms
//   [timing] core.lisp: Nms, M forms, avg X.Xms/form
//     NNNms  lower=NNms  jit=NNms  SORT
//     ...  (top-10 slowest)
//   [timing] clos.lisp: ...
//   [timing] Library/init.lisp: Nms
//   [timing] TOTAL: Nms
//
// The lower/jit split separates the Lisp→IR lowering cost (our code)
// from the LLVM IR→machine-code cost (LLVM's code per function).
mod startup_timing {
    use std::cell::RefCell;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    static ENABLED: OnceLock<bool> = OnceLock::new();

    pub fn enabled() -> bool {
        *ENABLED.get_or_init(|| std::env::var("NCL_STARTUP_TIMING").is_ok())
    }

    /// Return `Some(Instant::now())` only when timing is enabled,
    /// so callers pay zero cost on the normal path.
    #[inline]
    pub fn now() -> Option<Instant> {
        if enabled() { Some(Instant::now()) } else { None }
    }

    #[inline]
    pub fn elapsed(t: Option<Instant>) -> Duration {
        t.map(|i| i.elapsed()).unwrap_or_default()
    }

    pub struct FormRecord {
        pub name: String,
        pub lower: Duration,
        pub jit: Duration,
    }
    impl FormRecord {
        fn total(&self) -> Duration { self.lower + self.jit }
    }

    thread_local! {
        static ACCUMULATOR: RefCell<Vec<FormRecord>> = const { RefCell::new(Vec::new()) };
    }

    /// Called from `compile_function` with the just-compiled function's
    /// name and split lower/jit durations.
    pub fn push_form(name: &str, lower: Duration, jit: Duration) {
        if !enabled() { return; }
        ACCUMULATOR.with(|a| {
            a.borrow_mut().push(FormRecord { name: name.to_string(), lower, jit });
        });
    }

    /// Print the top-N slowest forms collected since the last call,
    /// then clear the accumulator. Call at the end of each load phase.
    pub fn drain_and_report(label: &str, phase_elapsed: Duration) {
        if !enabled() { return; }
        ACCUMULATOR.with(|a| {
            let mut forms = a.borrow_mut();
            let n = forms.len();
            let avg = if n > 0 {
                phase_elapsed.as_millis() as f64 / n as f64
            } else { 0.0 };
            eprintln!(
                "[timing] {}: {}ms, {} forms, avg {:.1}ms/form",
                label, phase_elapsed.as_millis(), n, avg
            );
            forms.sort_by(|a, b| b.total().cmp(&a.total()));
            for r in forms.iter().take(10) {
                eprintln!(
                    "  {:5}ms  lower={:4}ms  jit={:4}ms  {}",
                    r.total().as_millis(),
                    r.lower.as_millis(),
                    r.jit.as_millis(),
                    r.name,
                );
            }
            forms.clear();
        });
    }

    /// Simple one-liner phase report (for phases without per-form data).
    pub fn report_phase(label: &str, elapsed: Duration) {
        if enabled() {
            eprintln!("[timing] {}: {}ms", label, elapsed.as_millis());
        }
    }
}

/// A NewCormanLisp evaluation session. Owns the GC coordinator and
/// a single Lisp-thread mutator. defun'd functions persist across
/// `eval` calls because they live in the coordinator's static area
/// and intern table.
pub struct Session {
    coord: Arc<GcCoordinator>,
    mutator: Box<MutatorState>,
}

impl Session {
    pub fn new() -> Session {
        Session::with_config(GcConfig::default())
    }

    pub fn with_config(config: GcConfig) -> Session {
        let coord = GcCoordinator::new(config);
        let mut mutator = Box::new(coord.register_mutator());
        install_native_functions(&coord, &mut mutator);
        Session { coord, mutator }
    }

    pub fn coord(&self) -> &Arc<GcCoordinator> { &self.coord }

    /// Force a minor GC cycle. Used by integration tests and
    /// diagnostic tooling to drive the collector deterministically
    /// so a follow-up `(gc-stats)` reading is non-zero even on
    /// workloads that wouldn't naturally fill the nursery.
    pub fn force_gc(&mut self) {
        self.mutator.collect_minor();
    }

    /// Register this Session as the active one for the current
    /// thread, so `(eval-string ...)` from Lisp can route into it.
    /// Call this once after the Session is in its final memory
    /// location (Box, top-level let, etc.); calling before the
    /// final placement leaves a stale pointer behind.
    pub fn activate(&mut self) {
        ACTIVE_SESSION.with(|c| c.set(self as *mut Session));
    }

    /// Read every form in `src`, evaluate them in sequence, return
    /// the printed representation of the last value (or `nil` on
    /// empty input). `(defun …)` forms are intercepted and define
    /// the function in the session's symbol table; their result is
    /// `nil`.
    pub fn eval(&mut self, src: &str) -> Result<String, EvalError> {
        let values = ncl_reader::read_all(src)
            .map_err(|e| EvalError::Read(format!("{:?}", e.kind)))?;
        if values.is_empty() {
            return Ok("nil".to_string());
        }
        let mut last = Word::NIL;
        for v in &values {
            last = self.eval_value(v)?;
        }
        Ok(format_word(last))
    }

    /// Evaluate a single Value and return the resulting Word.
    /// First macroexpands; then recognises `(defun …)` and
    /// `(defmacro …)` at top level; everything else goes through
    /// lower → JIT → call.
    pub fn eval_value(&mut self, v: &Value) -> Result<Word, EvalError> {
        // Defmacro is recognised BEFORE macroexpansion: the body
        // shouldn't be expanded against the (yet-to-be-installed)
        // macro itself, and we want the body's *internal* macros
        // expanded normally during compilation. Same convention
        // as defun.
        if let Some((name, params, body_forms)) = match_defmacro(v)? {
            return self.handle_defmacro(&name, &params, &body_forms);
        }
        if let Some((name, params, body_forms)) = match_defun(v)? {
            return self.handle_defun(&name, &params, &body_forms);
        }
        if let Some((name, params, body_lines)) = match_defasm(v)? {
            return self.handle_defasm(&name, &params, &body_lines);
        }
        // Otherwise: macroexpand fully, then compile.
        let expanded = macroexpand_all(v, &self.coord, &mut self.mutator)?;
        // After full expansion, re-run the fast-path recognisers so that
        // macros which expand to `defmacro` / `defun` are properly handled.
        // Example: `(define-modify-macro appendf (&rest r) append)` expands
        // to `(defmacro appendf …)` which must be installed, not JIT-compiled
        // as a generic call.
        if let Some((name, params, body_forms)) = match_defmacro(&expanded)? {
            return self.handle_defmacro(&name, &params, &body_forms);
        }
        if let Some((name, params, body_forms)) = match_defun(&expanded)? {
            return self.handle_defun(&name, &params, &body_forms);
        }
        // CL convention: `(progn …)` at top level treats each body
        // form as itself a top-level form. This is what lets a
        // macro expand into multiple top-level definitions (e.g.
        // defstruct emits `(progn (defun make-x …) (defun x-y …)
        // (defun %setf-x-y …) …)`). Recurse into eval_value so each
        // body form re-runs the defun/defmacro recognisers.
        if let Some(body_forms) = match_top_level_progn(&expanded) {
            let mut last = Word::NIL;
            for form in body_forms {
                last = self.eval_value(&form)?;
            }
            return Ok(last);
        }
        let expr = lower(&expanded, &self.coord).map_err(EvalError::Compile)?;
        let mutator_ptr = &mut *self.mutator as *mut _;
        // Guard the JIT execution: establish a backstop handler frame so
        // a condition that escapes this top-level form (undefined
        // function, error, type error with no enclosing handler-case)
        // defers and surfaces as a recoverable EvalError::Runtime —
        // instead of hitting HANDLER_DEPTH == 0 and exiting the process.
        // This is what lets the REPL keep going after an error, and lets
        // stdlib load report the actual failing form rather than dying
        // with a bare 0xC0000409.
        let guard = ncl_runtime::abi::condition_guard_enter();
        let jit_result = ncl_llvm::jit_eval(&expr, mutator_ptr);
        if let Some(cond_raw) = ncl_runtime::abi::condition_guard_exit(guard) {
            let msg = ncl_runtime::format_word_aesthetic(Word::from_raw(cond_raw));
            return Err(EvalError::Runtime(msg));
        }
        jit_result.map_err(EvalError::Jit)
    }

    /// Dry-run check of a single Value: do parse + macroexpand + lower
    /// but skip JIT execution for forms that aren't definitions. This
    /// is the kernel of the driver's `--check` flag — it surfaces
    /// reader, macroexpand, and compile-time errors WITHOUT running
    /// the file's "main" side-effects (FFI calls, network I/O, window
    /// creation, etc.).
    ///
    /// Definition forms (`defun`, `defmacro`, top-level `progn` of
    /// definitions, `defparameter`/`defvar`/`defconstant`, the
    /// installer side of `require`/`provide`/`load`/`in-package`)
    /// are FULL-evaluated, because later forms' macroexpansion and
    /// lowering need them to be in place (a macro defined on line 10
    /// has to be available when line 20 is checked).
    ///
    /// Everything else is macroexpanded, then lowered, then thrown
    /// away — never JIT-evaluated. Returns `Ok(())` if the form
    /// passes; any reader / macroexpand / lower error propagates.
    pub fn check_value(&mut self, v: &Value) -> Result<(), EvalError> {
        // Same eager defmacro/defun recognition as eval_value — the
        // body is compiled (which is what catches errors), and the
        // resulting Function is installed so later forms can refer
        // to it during their own macroexpand pass.
        if let Some((name, params, body_forms)) = match_defmacro(v)? {
            self.handle_defmacro(&name, &params, &body_forms)?;
            return Ok(());
        }
        if let Some((name, params, body_forms)) = match_defun(v)? {
            self.handle_defun(&name, &params, &body_forms)?;
            return Ok(());
        }
        if let Some((name, params, body_lines)) = match_defasm(v)? {
            self.handle_defasm(&name, &params, &body_lines)?;
            return Ok(());
        }
        // Macroexpand so we can decide what kind of form this is
        // AFTER macros run. defstruct-win32, define-win32-callback,
        // require-with-side-effects, etc. all surface as something
        // different post-expansion.
        let expanded = macroexpand_all(v, &self.coord, &mut self.mutator)?;
        if let Some(body_forms) = match_top_level_progn(&expanded) {
            for form in body_forms {
                self.check_value(&form)?;
            }
            return Ok(());
        }
        // Recognise definition-flavoured forms and full-eval them so
        // their effect is visible to later check_value calls.
        if is_definition_like(&expanded) {
            self.eval_value(&expanded)?;
            return Ok(());
        }
        // Pure check: lower the form (which exercises macroexpand
        // and the lowering pass), then discard the resulting Expr
        // without handing it to the JIT.
        let _expr = lower(&expanded, &self.coord).map_err(EvalError::Compile)?;
        Ok(())
    }

    /// Dry-run check of a source string: parse it into top-level
    /// forms, then run `check_value` on each. Errors propagate.
    /// On success, returns the number of forms checked.
    pub fn check(&mut self, src: &str) -> Result<usize, EvalError> {
        let values = ncl_reader::read_all(src)
            .map_err(|e| EvalError::Read(format!("{:?}", e.kind)))?;
        for v in &values {
            self.check_value(v)?;
        }
        Ok(values.len())
    }

    fn handle_defmacro(
        &mut self,
        name: &str,
        params: &ParamSpec,
        body_forms: &[Value],
    ) -> Result<Word, EvalError> {
        // Macros and defun'd functions are compiled the same way:
        // they're both Lisp functions with the standard JIT
        // calling convention. The only difference is where they're
        // installed — macros into the coordinator's macro registry,
        // defuns into the symbol's function cell.
        //
        // Both share the implicit (block NAME …) wrap; see
        // `handle_defun` for the rationale and the gating.
        let body_wrapped = self.maybe_wrap_in_block(name, body_forms);
        let fn_word =
            self.compile_function(name, params, body_wrapped.as_deref().unwrap_or(body_forms))?;
        self.coord.install_macro(Arc::from(name), fn_word);
        Ok(Word::NIL)
    }

    fn handle_defun(
        &mut self,
        name: &str,
        params: &ParamSpec,
        body_forms: &[Value],
    ) -> Result<Word, EvalError> {
        // ANSI CL convention: the body of a DEFUN is implicitly
        // wrapped in (block NAME …), so (return-from NAME val)
        // anywhere inside works without an explicit BLOCK.
        //
        // Wrapping is gated on whether the BLOCK macro is
        // installed yet — early-bootstrap defuns in core.lisp run
        // before `(defmacro block …)` lands at line ~449. Those
        // pre-BLOCK defuns don't need auto-block and wrapping
        // prematurely would leave a literal (BLOCK …) form in
        // the IR.
        //
        // Only DEFUN / DEFMACRO go through this wrap. `(compile
        // NAME '(lambda …))` is intentionally left unwrapped —
        // CLOS uses it to install discriminating functions and
        // method bodies that already carry their own explicit
        // (block GF-NAME …) wrap.
        let body_wrapped = self.maybe_wrap_in_block(name, body_forms);
        let fn_word =
            self.compile_function(name, params, body_wrapped.as_deref().unwrap_or(body_forms))?;
        let sym_word = self.coord.intern(name);
        self.mutator.set_symbol_function(sym_word, fn_word);
        Ok(Word::NIL)
    }

    fn handle_defasm(
        &mut self,
        name: &str,
        param_names: &[String],
        body_lines: &[String],
    ) -> Result<Word, EvalError> {
        // CL upcases symbol names by default, but assembly conventions
        // are lowercase and don't accept hyphens in labels — `.globl
        // FAST-ADD` errors at the inline-asm parser with "unexpected
        // token". We mangle the name to a valid asm label by lowering
        // case and substituting `-` with `_`; the Lisp-side binding
        // keeps the original (FAST-ADD) so `(fast-add ...)` finds the
        // function as written.
        //
        // Same dance for parameter names: writing `#a` in a body
        // string is natural Lisp style, but the param symbol is
        // interned as `A`, and `substitute_params` is case-sensitive.
        // Lower the param names too so `#a`/`#b` substitutes cleanly.
        let asm_label = name.to_ascii_lowercase().replace('-', "_");
        let params: Vec<new_asm::AsmParam> = param_names
            .iter()
            .map(|n| new_asm::AsmParam {
                name: n.to_ascii_lowercase(),
                ty: new_asm::AsmType::Word,
            })
            .collect();
        let body = body_lines.join("\n");
        let proc = new_asm::AsmProc {
            name: asm_label,
            params,
            return_type: new_asm::AsmRetType::Word,
            body,
        };
        let code_ptr = ncl_llvm::jit_compile_asm_proc(&proc).map_err(EvalError::Jit)?;
        let arity = param_names.len() as u32;
        let sym_word = self.coord.intern(name);
        let fn_word = gc_function::alloc_function_in_static(
            self.coord.static_area(),
            code_ptr,
            arity,
            sym_word,
            Word::NIL,
            false, // asm proc — not Lisp-compiled; doesn't manage MV slot
        )
        .ok_or_else(|| EvalError::Jit("static area exhausted".to_string()))?;
        self.mutator.set_symbol_function(sym_word, fn_word);
        Ok(Word::NIL)
    }

    /// Returns a one-element Vec wrapping BODY_FORMS in `(BLOCK
    /// <NAME> body…)` when the BLOCK macro is installed AND the
    /// body actually uses `(return-from <NAME> …)` somewhere;
    /// otherwise None, meaning "leave the body unchanged."
    ///
    /// Skipping the wrap is a major performance win: the BLOCK
    /// macro expands to `(%native-block 'NAME (lambda () body…))`,
    /// which allocates an env Vector + a Function on every call
    /// to the defun. For tight recursive functions like
    /// `demos/life.lisp`'s `member-cell` that don't use
    /// `return-from`, those allocations are pure overhead and
    /// drive the GC into a per-call cycle.
    ///
    /// The walk is syntactic over the unexpanded body. It misses
    /// the case where a macro introduces `(return-from <NAME> …)`
    /// at expansion time; that's rare in practice and the
    /// trade-off is loud: such a macro will signal an
    /// unmatched-block error at runtime, which is the same
    /// failure mode as forgetting `(block …)` by hand. The
    /// conservative variant remains available via the `BLOCK`
    /// macro itself if a user writes `(block <NAME> body)`
    /// explicitly.
    fn maybe_wrap_in_block(
        &self,
        name: &str,
        body_forms: &[Value],
    ) -> Option<Vec<Value>> {
        if body_forms.is_empty() || self.coord.macro_for("BLOCK").is_none() {
            return None;
        }
        if !body_uses_return_from_name(body_forms, name) {
            return None;
        }
        Some(vec![wrap_body_in_block(name, body_forms)])
    }

    /// Shared lowering+JIT path for `defun` and `defmacro`. Returns
    /// a Function-tagged Word; the caller decides where to install
    /// it (function cell or macro registry).
    ///
    /// Body forms are macroexpanded against the current macro
    /// registry before lowering. The rest parameter, if any, is
    /// handled identically to a let-local — boxed if the mutation
    /// analysis sees a setq/setf of it.
    fn compile_function(
        &mut self,
        name: &str,
        params: &ParamSpec,
        body_forms: &[Value],
    ) -> Result<Word, EvalError> {
        compile_function_raw(
            &self.coord,
            &mut self.mutator,
            name,
            params,
            body_forms,
        )
    }
}

/// Free-function form of `Session::compile_function`. Takes the
/// coordinator and mutator explicitly instead of `&mut self`, so it
/// can be called from `macroexpand_all` (which holds coord + mutator
/// but no `Session`) to compile local macro expanders for MACROLET.
/// `Session::compile_function` delegates here, so there is a single
/// source of truth for the compile pipeline.
pub(crate) fn compile_function_raw(
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
    name: &str,
    params: &ParamSpec,
    body_forms: &[Value],
) -> Result<Word, EvalError> {
    let t_lower = startup_timing::now();

    // Capture `(declare ...)` specs from the RAW body, BEFORE
    // macroexpansion. `declare` is a no-op macro that expands to nil
    // (core.lisp), so the metadata is destroyed by macroexpand_all —
    // we must read the leading declares here to honour type
    // declarations (e.g. (double-float ...)). See
    // docs/performance-unbox-float.md Sprint 3.
    let (decl_specs, _) = lower::strip_declares(body_forms);

    // First, expand any macros used in the body.
    let mut expanded_body: Vec<Value> = Vec::with_capacity(body_forms.len());
    for form in body_forms {
        expanded_body.push(macroexpand_all(form, coord, mutator)?);
    }

    // Strip leading (declare ...) forms — after macroexpansion they are
    // nil (the declare macro), but strip any that survive to keep the
    // body clean.
    let (_, effective_body) = lower::strip_declares(&expanded_body);
    let effective_body = effective_body.to_vec();

    // Capture this body for inline expansion at direct call sites, IFF it was
    // `(declaim (inline NAME))`d with a simple (required-only) lambda list.
    // The function is still compiled normally below (it must remain callable
    // via #', apply, recursion, and non-inlinable sites) — this only records
    // a copy. See inline.rs / docs/compiler_completion.md § Interprocedural.
    let simple_lambda_list = params.optionals.is_empty()
        && params.rest.is_none()
        && params.keys.is_empty();
    inline::maybe_record(name, simple_lambda_list, &params.required, &effective_body);

    // Build the env. Required params first at Param(0..N); the
    // shared prologue helper then layers optionals/rest/keys on
    // as let-locals (boxed if the body mutates them).
    let mut env = LocalEnv::with_params(&params.required);

    // Mutable-parameter promotion: if a required param is
    // mutated in the body, promote it to a boxed local cell.
    let body_mutations = lower::mutated_in_body(
        &effective_body,
        &std::collections::HashSet::new(),
    );
    let mut req_box_prologue: Vec<ncl_ir::Expr> = Vec::new();
    for (i, pname) in params.required.iter().enumerate() {
        if body_mutations.contains(pname) {
            let cell_init = ncl_ir::Expr::cons(
                ncl_ir::Expr::Param(i),
                ncl_ir::Expr::Nil,
            );
            env.rebind_as_local_cell(pname);
            req_box_prologue.push(cell_init);
        }
    }

    // Unboxed-float parameters: a required param declared
    // `(double-float ...)` that is NOT mutated (mutated ones were just
    // boxed above) and NOT special is read as a native f64 — the JIT
    // unboxes the argument once and computes without per-op boxing.
    // See docs/performance-unbox-float.md Sprint 3.
    let float_params = lower::extract_float_names(&decl_specs);
    if !float_params.is_empty() {
        // The function opted into float types — allow auto-inlining of
        // its simple loops (gated in lower_call_in_mut on this flag).
        env.mark_float_decl();
        for pname in &params.required {
            if float_params.contains(pname.as_ref())
                && !body_mutations.contains(pname)
                && !coord.is_special(coord.intern(pname))
            {
                env.rebind_as_param_f64(pname);
            }
        }
    }

    let mut prologue = lower::build_arglist_prologue(
        params,
        &effective_body,
        &mut env,
        coord,
    )
    .map_err(EvalError::Compile)?;
    if !req_box_prologue.is_empty() {
        let mut combined = req_box_prologue;
        combined.append(&mut prologue);
        prologue = combined;
    }

    // Implicit progn over body forms.
    let lowered_body = if effective_body.len() == 1 {
        lower_in(&effective_body[0], &env, coord)
            .map_err(EvalError::Compile)?
    } else if effective_body.is_empty() {
        ncl_ir::Expr::Nil
    } else {
        let lowered: Result<Vec<_>, _> = effective_body
            .iter()
            .map(|f| lower_in(f, &env, coord))
            .collect();
        ncl_ir::Expr::progn(lowered.map_err(EvalError::Compile)?)
    };

    let body_expr = if prologue.is_empty() {
        lowered_body
    } else {
        ncl_ir::Expr::let_(prologue, lowered_body)
    };

    let arity = params.required.len() as u32;

    // Self-tail-call elimination — turn tail-position calls to THIS
    // function into a loop so deep self-recursion reuses the frame
    // instead of overflowing the native stack. Gated to fixed-arity
    // functions (required params only): with &optional/&rest/&key,
    // some parameters are bound as let-locals via the prologue, so
    // a self-call would have to re-run the prologue — out of scope
    // for this transform. Runs BEFORE instrument_tail_for_mv so the
    // self-call is matched as a bare `Call`, not wrapped in
    // EnsureSingleMv; the MV pass then recurses into the resulting
    // `TailLoop` and leaves the `SelfTailNext` continuations alone.
    let fixed_arity = params.optionals.is_empty()
        && params.rest.is_none()
        && params.keys.is_empty();
    let body_expr = if fixed_arity {
        let self_sym = coord.intern(name).raw();
        let (rewritten, found) =
            lower::rewrite_self_tail_calls(body_expr, self_sym, arity);
        if found {
            ncl_ir::Expr::tail_loop(arity, rewritten)
        } else {
            rewritten
        }
    } else {
        body_expr
    };

    // Tail-position transform: wrap any non-`(values ...)` tail
    // expression in EnsureSingleMv so the multi-value slot is
    // always exactly the function's actual return values when
    // the caller reads it. See lower::instrument_tail_for_mv.
    let body_expr = lower::instrument_tail_for_mv(body_expr);

    // Lisp-aware representation inference: prove which values are
    // double-floats and wrap their reads so emit unboxes them without the
    // tag-check diamond. Runs last so it sees the final tree emit will see.
    // Opt out with NCL_NO_INFER=1 for A/B comparison. See optimize.rs +
    // docs/compiler_completion.md.
    let body_expr = if std::env::var_os("NCL_NO_INFER").is_some() {
        body_expr
    } else {
        optimize::infer_float_unboxing(body_expr)
    };

    let lower_elapsed = startup_timing::elapsed(t_lower);
    let t_jit = startup_timing::now();
    let code_ptr = ncl_llvm::jit_compile_function(name, arity, &body_expr)
        .map_err(EvalError::Jit)?;

    startup_timing::push_form(name, lower_elapsed, startup_timing::elapsed(t_jit));
    #[cfg(windows)]
    ncl_runtime::igui::splash::tick();

    let sym_word = coord.intern(name);
    gc_function::alloc_function_in_static(
        coord.static_area(),
        code_ptr,
        arity,
        sym_word,
        Word::NIL, // top-level functions and macros have no closure env
        true,      // Lisp-compiled; manages its own MV slot
    )
    .ok_or_else(|| EvalError::Jit("static area exhausted".to_string()))
}

impl Default for Session {
    fn default() -> Self { Session::new() }
}

/// Wire up native (Rust-implemented) Lisp functions. Each becomes
/// a callable Function in a Symbol's function cell — first-class
/// from the Lisp side: usable via `#'foo`, `apply`, `funcall`. Run
/// once at session creation, before any user code evaluates.
fn install_native_functions(
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
) {
    // Let runtime macro introspection (macro-function / macroexpand
    // with a non-nil &environment) see macrolet-local macros, which
    // live in the macroexpander's lexical environment. Registering the
    // bridge hook is idempotent and cheap.
    ncl_runtime::abi::set_local_macro_hook(crate::macroexpand::local_macro_hook);
    install_native(coord, mutator, "FORMAT", ncl_runtime::format_shim, 2);
    // APPEND (binary) is needed natively because backquote-splicing
    // macros expand to `(append ... ...)`. Loading it here means
    // backquote works in a bare Session, before any user-Lisp
    // stdlib has been read.
    install_native(coord, mutator, "APPEND", ncl_runtime::append_shim, 2);

    // File I/O. The handle-table architecture is borrowed from the
    // sister NewCP repo's `host_file_sys.rs`, adapted for our
    // String-Word path encoding. User-Lisp wrappers (with-open-file,
    // read-file-string, write-file-string) live in Lisp/core.lisp.
    install_native(coord, mutator, "OPEN-INPUT-FILE",
                   ncl_runtime::open_input_file_shim, 1);
    install_native(coord, mutator, "OPEN-OUTPUT-FILE",
                   ncl_runtime::open_output_file_shim, 1);
    install_native(coord, mutator, "OPEN-APPEND-FILE",
                   ncl_runtime::open_append_file_shim, 1);
    install_native(coord, mutator, "CLOSE-STREAM",
                   ncl_runtime::close_stream_shim, 1);
    install_native(coord, mutator, "READ-LINE",
                   ncl_runtime::read_line_shim, 1);
    install_native(coord, mutator, "READ-CHAR-FROM",
                   ncl_runtime::read_char_from_shim, 1);
    install_native(coord, mutator, "WRITE-STRING-TO",
                   ncl_runtime::write_string_to_shim, 2);
    install_native(coord, mutator, "FILE-POSITION",
                   ncl_runtime::file_position_shim, 1);
    install_native(coord, mutator, "FILE-LENGTH",
                   ncl_runtime::file_length_shim, 1);
    install_native(coord, mutator, "FILE-EXISTS",
                   ncl_runtime::file_exists_shim, 1);
    install_native(coord, mutator, "DELETE-FILE",
                   ncl_runtime::delete_file_shim, 1);

    // Conditions. ERROR signals; %HANDLER-CASE is the primitive
    // behind the handler-case macro (Lisp/core.lisp). Condition
    // unwinding goes through Rust's catch_unwind, which works
    // because every Rust function on the active call chain (and
    // the LispCodeFn type) is declared `extern "C-unwind"` so
    // panics aren't aborted at the boundary.
    install_native(coord, mutator, "ERROR",
                   ncl_runtime::error_shim, 1);
    install_native(coord, mutator, "%HANDLER-CASE",
                   ncl_runtime::handler_case_shim, 2);
    install_native(coord, mutator, "%NATIVE-LOOP",
                   ncl_runtime::native_loop_shim, 1);
    install_native(coord, mutator, "%LOOP-RETURN",
                   ncl_runtime::loop_return_shim, 1);

    // Self-eval bridge: lets Lisp code re-enter the compiler.
    install_native(coord, mutator, "EVAL-STRING",
                   eval_string_shim, 1);
    // CL eval: takes a runtime form (Word), roundtrips through the
    // printer/reader, then compiles and evaluates. Returns the
    // actual result Word (not a printed string).
    install_native(coord, mutator, "%EVAL-FORM",
                   eval_form_shim, 1);
    // CL read-from-string: parse one form from a string and return
    // the materialised runtime object.
    install_native(coord, mutator, "READ-FROM-STRING",
                   read_from_string_shim, 1);
    // Disk-load bridge: read a .lisp file and eval every form.
    // The Lisp `load` in core.lisp wraps this with *load-path*
    // resolution and *modules* recording.
    install_native(coord, mutator, "%LOAD-FILE",
                   load_file_shim, 1);
    install_native(coord, mutator, "%FILE-PARSES-P",
                   file_parses_p_shim, 1);
    // Hot-reload watcher (Tier "fun extension").
    install_native(coord, mutator, "%WATCHER-START",
                   ncl_runtime::hot_reload::watcher_start_shim, 1);
    install_native(coord, mutator, "%WATCHER-PENDING",
                   ncl_runtime::hot_reload::watcher_pending_shim, 0);
    // (compile name '(lambda (params) body)) — JIT a lambda form
    // at runtime and (optionally) install in name's function cell.
    // Closette uses this to install its discriminating functions.
    install_native(coord, mutator, "COMPILE",
                   compile_shim, 2);
    install_native(coord, mutator, "PARSE-COMPLETE?",
                   parse_complete_shim, 1);
    install_native(coord, mutator, "SUBSTRING",
                   substring_shim, 3);
    // CLOS port prerequisites — foundation utilities.
    // GENSYM accepts an optional prefix; we register it as a 1-arg
    // shim and tolerate no-arg calls inside the shim body. (When
    // &optional lands in the compiler we'll declare it properly.)
    install_native(coord, mutator, "GENSYM",
                   ncl_runtime::gensym_shim, 0);
    // CAR / CDR / CONS are special forms in lower.rs; these
    // shims exist only so #'car / #'cdr / #'cons can be passed
    // as function values (to mapcar, :key, etc.).
    install_native(coord, mutator, "CAR",
                   ncl_runtime::car_shim, 1);
    install_native(coord, mutator, "CDR",
                   ncl_runtime::cdr_shim, 1);
    install_native(coord, mutator, "CONS",
                   ncl_runtime::cons_shim, 2);
    install_native(coord, mutator, "LIST",
                   ncl_runtime::list_shim, 0);
    install_native(coord, mutator, "EQ",
                   ncl_runtime::eq_shim, 2);
    install_native(coord, mutator, "EQL",
                   ncl_runtime::eql_shim, 2);
    install_native(coord, mutator, "EQUAL",
                   ncl_runtime::equal_shim, 2);
    install_native(coord, mutator, "TYPEP",
                   ncl_runtime::typep_shim, 2);
    // Bignum math (Tier 1.D.2): gcd / lcm / expt / abs / isqrt.
    // TRUNCATE and REM used to be compile-time special forms
    // lowered to inline LLVM srem; they're now ordinary native
    // calls so Library/numbers.lisp can override them with
    // polymorphic, multi-value-returning wrappers. The int-only
    // fast path is preserved by these shims (fixnum + bignum).
    install_native(coord, mutator, "TRUNCATE",
                   ncl_runtime::bignum::truncate_shim, 2);
    install_native(coord, mutator, "REM",
                   ncl_runtime::bignum::rem_shim, 2);
    install_native(coord, mutator, "GCD",
                   ncl_runtime::bignum::gcd_shim, 2);
    install_native(coord, mutator, "LCM",
                   ncl_runtime::bignum::lcm_shim, 2);
    install_native(coord, mutator, "EXPT",
                   ncl_runtime::bignum::expt_shim, 2);
    install_native(coord, mutator, "ABS",
                   ncl_runtime::bignum::abs_shim, 1);
    install_native(coord, mutator, "ISQRT",
                   ncl_runtime::bignum::isqrt_shim, 1);
    // Bit operations (Tier 1.D.3).
    install_native(coord, mutator, "ASH",
                   ncl_runtime::bignum::ash_shim, 2);
    // n-ary in CL: identity 0 for logior/logxor, -1 for logand.
    install_native(coord, mutator, "LOGAND",
                   ncl_runtime::bignum::logand_shim, 0);
    install_native(coord, mutator, "LOGIOR",
                   ncl_runtime::bignum::logior_shim, 0);
    install_native(coord, mutator, "LOGXOR",
                   ncl_runtime::bignum::logxor_shim, 0);
    install_native(coord, mutator, "LOGNOT",
                   ncl_runtime::bignum::lognot_shim, 1);
    install_native(coord, mutator, "INTEGER-LENGTH",
                   ncl_runtime::bignum::integer_length_shim, 1);
    install_native(coord, mutator, "LOGBITP",
                   ncl_runtime::bignum::logbitp_shim, 2);
    // Complex numbers (Tier 2.C).
    install_native(coord, mutator, "COMPLEX",
                   ncl_runtime::complex::complex_shim, 2);
    install_native(coord, mutator, "COMPLEXP",
                   ncl_runtime::complex::complexp_shim, 1);
    install_native(coord, mutator, "NUMBERP",
                   ncl_runtime::complex::numberp_shim, 1);
    install_native(coord, mutator, "REALPART",
                   ncl_runtime::complex::realpart_shim, 1);
    install_native(coord, mutator, "IMAGPART",
                   ncl_runtime::complex::imagpart_shim, 1);
    install_native(coord, mutator, "CONJUGATE",
                   ncl_runtime::complex::conjugate_shim, 1);
    install_native(coord, mutator, "PHASE",
                   ncl_runtime::complex::phase_shim, 1);
    // ABS gets the complex-aware override (falls through to
    // bignum's abs for real arguments).
    install_native(coord, mutator, "ABS",
                   ncl_runtime::complex::abs_complex_shim, 1);
    // Ratios (Tier 2.B).
    install_native(coord, mutator, "RATIOP",
                   ncl_runtime::ratio::ratiop_shim, 1);
    install_native(coord, mutator, "RATIONALP",
                   ncl_runtime::ratio::rationalp_shim, 1);
    install_native(coord, mutator, "NUMERATOR",
                   ncl_runtime::ratio::numerator_shim, 1);
    install_native(coord, mutator, "DENOMINATOR",
                   ncl_runtime::ratio::denominator_shim, 1);
    install_native(coord, mutator, "RATIONAL",
                   ncl_runtime::ratio::rational_shim, 1);
    // Floats (Tier 2.A).
    install_native(coord, mutator, "/",
                   ncl_runtime::float::div_shim, 2);
    install_native(coord, mutator, "FLOATP",
                   ncl_runtime::float::floatp_shim, 1);
    install_native(coord, mutator, "FLOAT",
                   ncl_runtime::float::float_shim, 1);
    // Transcendentals route to the complex-aware shims that lift
    // negative-real / complex inputs to complex results.
    install_native(coord, mutator, "SQRT",
                   ncl_runtime::complex::sqrt_complex_shim, 1);
    install_native(coord, mutator, "SIN",
                   ncl_runtime::complex::sin_complex_shim, 1);
    install_native(coord, mutator, "COS",
                   ncl_runtime::complex::cos_complex_shim, 1);
    install_native(coord, mutator, "TAN",
                   ncl_runtime::complex::tan_complex_shim, 1);
    install_native(coord, mutator, "ASIN",
                   ncl_runtime::complex::asin_complex_shim, 1);
    install_native(coord, mutator, "ACOS",
                   ncl_runtime::complex::acos_complex_shim, 1);
    install_native(coord, mutator, "ATAN",
                   ncl_runtime::complex::atan_complex_shim, 1);
    install_native(coord, mutator, "SINH",
                   ncl_runtime::complex::sinh_complex_shim, 1);
    install_native(coord, mutator, "COSH",
                   ncl_runtime::complex::cosh_complex_shim, 1);
    install_native(coord, mutator, "TANH",
                   ncl_runtime::complex::tanh_complex_shim, 1);
    install_native(coord, mutator, "EXP",
                   ncl_runtime::complex::exp_complex_shim, 1);
    install_native(coord, mutator, "LOG",
                   ncl_runtime::complex::log_complex_base_shim, 1);
    install_native(coord, mutator, "EXPT-FLOAT",
                   ncl_runtime::float::expt_float_shim, 2);
    install_native(coord, mutator, "TRUNCATE-FLOAT",
                   ncl_runtime::float::truncate_float_shim, 1);
    install_native(coord, mutator, "FLOOR-FLOAT",
                   ncl_runtime::float::floor_float_shim, 1);
    install_native(coord, mutator, "CEILING-FLOAT",
                   ncl_runtime::float::ceiling_float_shim, 1);
    install_native(coord, mutator, "ROUND-FLOAT",
                   ncl_runtime::float::round_float_shim, 1);

    // Character primitives — see `Lisp/Library/characters.lisp` for
    // the variadic CHAR= / CHAR< / CHAR-EQUAL family that builds on
    // top of these. Every shim takes one or two args; argument count
    // checks happen inside the shim because some are variadic in
    // the radix slot.
    install_native(coord, mutator, "CHAR-CODE",
                   ncl_runtime::chars::char_code_shim, 1);
    install_native(coord, mutator, "CODE-CHAR",
                   ncl_runtime::chars::code_char_shim, 1);
    // (%make-string-fill n ch) — one-pass string allocator behind
    // make-string. Library/strings.lisp delegates to it; without
    // this, make-string was an O(n²) string-append-char loop and
    // every fill-in-place string builder inherited the quadratic.
    install_native(coord, mutator, "%MAKE-STRING-FILL",
                   ncl_runtime::chars::make_string_fill_shim, 2);
    install_native(coord, mutator, "CHAR-UPCASE",
                   ncl_runtime::chars::char_upcase_shim, 1);
    install_native(coord, mutator, "CHAR-DOWNCASE",
                   ncl_runtime::chars::char_downcase_shim, 1);
    install_native(coord, mutator, "ALPHA-CHAR-P",
                   ncl_runtime::chars::alpha_char_p_shim, 1);
    install_native(coord, mutator, "ALPHANUMERICP",
                   ncl_runtime::chars::alphanumericp_shim, 1);
    install_native(coord, mutator, "UPPER-CASE-P",
                   ncl_runtime::chars::upper_case_p_shim, 1);
    install_native(coord, mutator, "LOWER-CASE-P",
                   ncl_runtime::chars::lower_case_p_shim, 1);
    install_native(coord, mutator, "BOTH-CASE-P",
                   ncl_runtime::chars::both_case_p_shim, 1);
    install_native(coord, mutator, "GRAPHIC-CHAR-P",
                   ncl_runtime::chars::graphic_char_p_shim, 1);
    install_native(coord, mutator, "DIGIT-CHAR-P",
                   ncl_runtime::chars::digit_char_p_shim, 2);
    install_native(coord, mutator, "DIGIT-CHAR",
                   ncl_runtime::chars::digit_char_shim, 2);

    // Multi-value support — `(values ...)` itself is a special form
    // lowered in lower.rs; this is the primitive used by the
    // multiple-value-bind / multiple-value-list macros to read the
    // thread-local slot back as a Lisp list.
    install_native(coord, mutator, "%MULTIPLE-VALUE-LIST-OF",
                   ncl_runtime::multiple_value_list_of_shim, 1);
    install_native(coord, mutator, "%MV-CLEAR",
                   ncl_runtime::mv_clear_shim, 0);
    // VALUES and VALUES-LIST as installed natives so #'values and
    // (apply #'values list) work. The compiler keeps its special-
    // form fast path for direct `(values ...)` calls — this is the
    // function-value-of-VALUES path.
    install_native(coord, mutator, "VALUES",
                   ncl_runtime::values_shim, 0);
    install_native(coord, mutator, "VALUES-LIST",
                   ncl_runtime::values_list_shim, 1);
    // Vectors. AREF and SVREF are special forms (lowered directly
    // to Expr::Aref); MAKE-ARRAY and VECTOR go through Lisp-callable
    // shims.
    install_native(coord, mutator, "MAKE-ARRAY",
                   ncl_runtime::make_array_shim, 1);
    install_native(coord, mutator, "VECTOR",
                   ncl_runtime::vector_shim, 0);
    install_native(coord, mutator, "%WORD-HASH",
                   ncl_runtime::word_hash_shim, 1);
    install_native(coord, mutator, "%EQUAL-HASH",
                   ncl_runtime::equal_hash_shim, 1);
    // Symbol function-cell access. SYMBOL-FUNCTION reads,
    // %SET-SYMBOL-FUNCTION is the target of the
    // (setf (symbol-function ...) ...) lowering, FMAKUNBOUND
    // clears, FBOUNDP probes.
    install_native(coord, mutator, "SYMBOL-FUNCTION",
                   ncl_runtime::symbol_function_shim, 1);
    install_native(coord, mutator, "%SET-SYMBOL-FUNCTION",
                   ncl_runtime::set_symbol_function_shim, 2);
    install_native(coord, mutator, "SYMBOL-NAME",
                   ncl_runtime::symbol_name_shim, 1);
    install_native(coord, mutator, "SYMBOL-PACKAGE",
                   ncl_runtime::symbol_package_shim, 1);
    install_native(coord, mutator, "BOUNDP",
                   ncl_runtime::boundp_shim, 1);
    install_native(coord, mutator, "SYMBOL-VALUE",
                   ncl_runtime::symbol_value_shim, 1);
    install_native(coord, mutator, "SET",
                   ncl_runtime::set_shim, 2);
    install_native(coord, mutator, "TYPE-OF",
                   ncl_runtime::type_of_shim, 1);

    // Numeric comparison + length: the compiler lowers `(< a b)`,
    // `(length x)` etc. as special forms (fast path), but `#'<` /
    // `#'length` need callable function-cell bindings too. These
    // shims supply the funcall path; direct calls still take the
    // special-form lowering. `/=` is the lone exception — it's
    // not a special form, so the shim is the only path.
    install_native(coord, mutator, "<",
                   ncl_runtime::lt_shim, 2);
    install_native(coord, mutator, ">",
                   ncl_runtime::gt_shim, 2);
    install_native(coord, mutator, "<=",
                   ncl_runtime::le_shim, 2);
    install_native(coord, mutator, ">=",
                   ncl_runtime::ge_shim, 2);
    install_native(coord, mutator, "=",
                   ncl_runtime::num_eq_shim, 2);
    install_native(coord, mutator, "/=",
                   ncl_runtime::num_ne_shim, 2);
    install_native(coord, mutator, "LENGTH",
                   ncl_runtime::length_shim, 1);

    // Arithmetic — same story as the comparison family above. Direct
    // (+ a b) takes the special-form fast path; #'+ / (funcall #'+ …)
    // / (mapcar #'+ …) need the function-cell shim. The shims wrap
    // ncl_add_full / ncl_sub_full / ncl_mul_full, which already
    // handle the full numeric tower.
    install_native(coord, mutator, "+",
                   ncl_runtime::add_shim, 2);
    install_native(coord, mutator, "-",
                   ncl_runtime::sub_shim, 2);
    install_native(coord, mutator, "*",
                   ncl_runtime::mul_shim, 2);
    install_native(coord, mutator, "FMAKUNBOUND",
                   ncl_runtime::fmakunbound_shim, 1);
    install_native(coord, mutator, "FBOUNDP",
                   ncl_runtime::fboundp_shim, 1);
    install_native(coord, mutator, "MACRO-FUNCTION",
                   ncl_runtime::macro_function_shim, 1);
    install_native(coord, mutator, "SPECIAL-OPERATOR-P",
                   ncl_runtime::special_operator_p_shim, 1);
    install_native(coord, mutator, "INTERN",
                   ncl_runtime::intern_shim, 1);
    // block / return-from — non-local exit via setjmp/longjmp
    // through JIT frames. Lisp-side macros in core.lisp wrap
    // the body in a thunk and call %native-block.
    install_native(coord, mutator, "%NATIVE-BLOCK",
                   ncl_runtime::native_block_shim, 2);
    install_native(coord, mutator, "%RETURN-FROM",
                   ncl_runtime::return_from_shim, 2);
    install_native(coord, mutator, "%NATIVE-UNWIND-PROTECT",
                   ncl_runtime::unwind_protect_shim, 2);
    // catch / throw — dynamic (runtime-tagged) non-local exit. The
    // Lisp macros in core.lisp wrap the body in a thunk and pass the
    // evaluated tag to %native-catch / %throw.
    install_native(coord, mutator, "%NATIVE-CATCH",
                   ncl_runtime::catch_shim, 2);
    install_native(coord, mutator, "%THROW",
                   ncl_runtime::throw_shim, 2);

    install_native(coord, mutator, "STRING-SPLIT-NEWLINES",
                   string_split_newlines_shim, 1);

    // String manipulation.
    install_native(coord, mutator, "STRING-UPCASE",
                   ncl_runtime::string_upcase_shim, 1);
    install_native(coord, mutator, "STRING-DOWNCASE",
                   ncl_runtime::string_downcase_shim, 1);
    install_native(coord, mutator, "STRING-TRIM",
                   ncl_runtime::string_trim_shim, 2);
    install_native(coord, mutator, "STRING-LEFT-TRIM",
                   ncl_runtime::string_left_trim_shim, 2);
    install_native(coord, mutator, "STRING-RIGHT-TRIM",
                   ncl_runtime::string_right_trim_shim, 2);
    install_native(coord, mutator, "PARSE-INTEGER",
                   ncl_runtime::parse_integer_shim, 1);

    // THREADS package — Roger Corman's API, cross-platform.
    // Lisp-side wrappers in Library/threads.lisp expose the public
    // names (create-thread, with-synchronization, critical-section
    // class, etc.) on top of these primitives. We install the
    // primitives unconditionally so `(require 'threads)` works on
    // every platform; the underlying OS support is `std::thread`.
    // Arity is 2 (function, report-when-finished?); the shim treats
    // the second arg as optional and defaults to T when only one
    // arg is supplied, so the Lisp wrapper in Library/threads.lisp
    // can pass either shape.
    install_native(coord, mutator, "%CREATE-THREAD",
                   ncl_runtime::create_thread_shim, 2);
    install_native(coord, mutator, "EXIT-THREAD",
                   ncl_runtime::exit_thread_shim, 1);
    install_native(coord, mutator, "THREAD-HANDLE",
                   ncl_runtime::thread_handle_shim, 1);
    install_native(coord, mutator, "SUSPEND-THREAD",
                   ncl_runtime::suspend_thread_shim, 1);
    install_native(coord, mutator, "RESUME-THREAD",
                   ncl_runtime::resume_thread_shim, 1);
    install_native(coord, mutator, "TERMINATE-THREAD",
                   ncl_runtime::terminate_thread_shim, 1);
    install_native(coord, mutator, "CURRENT-THREAD-ID",
                   ncl_runtime::current_thread_id_shim, 0);
    install_native(coord, mutator, "CURRENT-PROCESS-ID",
                   ncl_runtime::current_process_id_shim, 0);
    // Windows surface — Phase 1 of docs/WINDOWS_FFI.md.
    // (windows-enabled-p), (ui-thread-id), (ui-thread-p) are always
    // installed; they return NIL outside `--windows` mode. The
    // conditional (when (windows-enabled-p) (require 'win32-threading))
    // branch in init.lisp uses them to decide whether to pull in the
    // UI-thread surface.
    install_native(coord, mutator, "WINDOWS-ENABLED-P",
                   ncl_runtime::windows_enabled_p_shim, 0);
    install_native(coord, mutator, "UI-THREAD-ID",
                   ncl_runtime::ui_thread_id_shim, 0);
    install_native(coord, mutator, "UI-THREAD-P",
                   ncl_runtime::ui_thread_p_shim, 0);
    // (%ui-execute closure) — marshal a 0-arg closure to the UI
    // thread, block until it returns. The (on-ui-thread BODY) macro
    // in Lisp/Library/win32-threading.lisp wraps BODY in (lambda ()
    // …) and calls this. Errors if --windows wasn't passed.
    install_native(coord, mutator, "%UI-EXECUTE",
                   ncl_runtime::ui_execute_shim, 1);
    // (%ffi-call DLL FN ARG-TYPES RETURN-TYPE ARGS…) — the FFI
    // kernel. Variadic (arity 0 = no compile-time arg-count check);
    // the shim itself enforces "at least 4 fixed args plus one per
    // arg-type". Phase 3 of docs/WINDOWS_FFI.md.
    install_native(coord, mutator, "%FFI-CALL",
                   ncl_runtime::ffi_call_shim, 0);
    // (%win32-lookup NAME) -> plist or NIL. Used by (defwin32 …) at
    // macroexpansion time to bake the signature into a defun. The
    // metadata pack is loaded by the driver at --windows startup.
    install_native(coord, mutator, "%WIN32-LOOKUP",
                   ncl_runtime::win32_lookup_shim, 1);
    // (%win32-call NAME &rest user-args). Used by (win32 NAME …)
    // for one-shot dynamic dispatch (no Lisp-side defun generated).
    install_native(coord, mutator, "%WIN32-CALL",
                   ncl_runtime::win32_call_shim, 0);
    // Foreign buffer primitives (Phase 5). The defstruct-win32
    // macro (Lisp/Library/win32-buffer.lisp) layers offset/size
    // discipline on top so user code doesn't hand-roll layouts.
    install_native(coord, mutator, "MAKE-FOREIGN-BUFFER",
                   ncl_runtime::make_foreign_buffer_shim, 1);
    install_native(coord, mutator, "FREE-FOREIGN-BUFFER",
                   ncl_runtime::free_foreign_buffer_shim, 2);
    install_native(coord, mutator, "BUFFER-ZERO",
                   ncl_runtime::buffer_zero_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-U8",
                   ncl_runtime::buffer_ref_u8_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-I8",
                   ncl_runtime::buffer_ref_i8_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-U16",
                   ncl_runtime::buffer_ref_u16_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-I16",
                   ncl_runtime::buffer_ref_i16_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-U32",
                   ncl_runtime::buffer_ref_u32_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-I32",
                   ncl_runtime::buffer_ref_i32_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-U64",
                   ncl_runtime::buffer_ref_u64_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-I64",
                   ncl_runtime::buffer_ref_i64_shim, 2);
    install_native(coord, mutator, "BUFFER-REF-PTR",
                   ncl_runtime::buffer_ref_ptr_shim, 2);
    install_native(coord, mutator, "BUFFER-SET-U8",
                   ncl_runtime::buffer_set_u8_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-I8",
                   ncl_runtime::buffer_set_i8_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-U16",
                   ncl_runtime::buffer_set_u16_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-I16",
                   ncl_runtime::buffer_set_i16_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-U32",
                   ncl_runtime::buffer_set_u32_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-I32",
                   ncl_runtime::buffer_set_i32_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-U64",
                   ncl_runtime::buffer_set_u64_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-I64",
                   ncl_runtime::buffer_set_i64_shim, 3);
    install_native(coord, mutator, "BUFFER-SET-PTR",
                   ncl_runtime::buffer_set_ptr_shim, 3);
    install_native(coord, mutator, "BUFFER-READ-WSTRING",
                   ncl_runtime::buffer_read_wstring_shim, 2);
    install_native(coord, mutator, "BUFFER-WRITE-WSTRING",
                   ncl_runtime::buffer_write_wstring_shim, 3);
    // (%make-win32-callback CLOSURE ARITY) — Phase 6. Registers
    // CLOSURE in the runtime's callback registry and JIT-emits a
    // trampoline that Win32 can call directly. Returns the
    // trampoline's machine-code address as a fixnum, ready to
    // store in a WNDCLASSEXW.lpfnWndProc slot or similar.
    install_native(coord, mutator, "%MAKE-WIN32-CALLBACK",
                   ncl_llvm::make_win32_callback_shim, 2);
    install_native(coord, mutator, "ALLOCATE-CRITICAL-SECTION",
                   ncl_runtime::allocate_critical_section_shim, 0);
    install_native(coord, mutator, "DEALLOCATE-CRITICAL-SECTION",
                   ncl_runtime::deallocate_critical_section_shim, 1);
    install_native(coord, mutator, "ENTER-CRITICAL-SECTION",
                   ncl_runtime::enter_critical_section_shim, 1);
    install_native(coord, mutator, "LEAVE-CRITICAL-SECTION",
                   ncl_runtime::leave_critical_section_shim, 1);
    install_native(coord, mutator, "THREAD-SAFEPOINT",
                   ncl_runtime::thread_safepoint_shim, 0);
    install_native(coord, mutator, "%TEST-PANIC",
                   ncl_runtime::test_panic_shim, 0);
    install_native(coord, mutator, "GC-STATS",
                   ncl_runtime::gc_stats_shim, 0);
    install_native(coord, mutator, "GC",
                   ncl_runtime::gc_force_shim, 0);
    install_native(coord, mutator, "JOIN-THREAD",
                   ncl_runtime::join_thread_shim, 1);
    install_native(coord, mutator, "SLEEP",
                   ncl_runtime::sleep_shim, 1);
    install_native(coord, mutator, "GET-INTERNAL-REAL-TIME",
                   ncl_runtime::get_internal_real_time_shim, 0);
    // (random limit) — fixnum-limit only for v1 (no float, no
    // explicit random-state arg). Process-global xoshiro256**
    // seeded once from time + PID + TID + ASLR + RDTSC.
    install_native(coord, mutator, "RANDOM",
                   ncl_runtime::random_shim, 1);

    // Atomic counters (lock-free shared integers).
    install_native(coord, mutator, "MAKE-ATOMIC-COUNTER",
                   ncl_runtime::make_atomic_counter_shim, 1);
    install_native(coord, mutator, "RELEASE-ATOMIC-COUNTER",
                   ncl_runtime::release_atomic_counter_shim, 1);
    install_native(coord, mutator, "ATOMIC-INCF",
                   ncl_runtime::atomic_incf_shim, 2);
    install_native(coord, mutator, "ATOMIC-GET",
                   ncl_runtime::atomic_get_shim, 1);
    install_native(coord, mutator, "ATOMIC-SET",
                   ncl_runtime::atomic_set_shim, 2);
    install_native(coord, mutator, "ATOMIC-CAS",
                   ncl_runtime::atomic_cas_shim, 3);

    // Mailboxes (bounded/unbounded mpmc queues of Word values).
    install_native(coord, mutator, "MAKE-MAILBOX",
                   ncl_runtime::make_mailbox_shim, 1);
    install_native(coord, mutator, "RELEASE-MAILBOX",
                   ncl_runtime::release_mailbox_shim, 1);
    install_native(coord, mutator, "MAILBOX-SEND",
                   ncl_runtime::mailbox_send_shim, 2);
    install_native(coord, mutator, "%MAILBOX-RECEIVE",
                   ncl_runtime::mailbox_receive_shim, 2);
    install_native(coord, mutator, "MAILBOX-LEN",
                   ncl_runtime::mailbox_len_shim, 1);

    // Condition variables (paired with critical sections).
    install_native(coord, mutator, "MAKE-CONDVAR",
                   ncl_runtime::make_condvar_shim, 0);
    install_native(coord, mutator, "RELEASE-CONDVAR",
                   ncl_runtime::release_condvar_shim, 1);
    install_native(coord, mutator, "%CONDVAR-WAIT",
                   ncl_runtime::condvar_wait_shim, 3);
    install_native(coord, mutator, "CONDVAR-NOTIFY",
                   ncl_runtime::condvar_notify_shim, 1);
    install_native(coord, mutator, "CONDVAR-BROADCAST",
                   ncl_runtime::condvar_broadcast_shim, 1);

    // iGui (Windows only). Spawns the GUI thread and exposes the
    // window-management trio + event poll. Drawing primitives
    // come in a follow-up commit.
    #[cfg(windows)]
    install_igui(coord, mutator);

    // NewAudio shims — synthesis presets, simple playback, ABC.
    // The shims themselves are cross-platform (non-Windows builds
    // get NIL-returning stubs), so registration is unconditional;
    // only the live mixer is Windows-only inside the backend.
    install_audio(coord, mutator);
}

fn install_audio(coord: &Arc<GcCoordinator>, mutator: &mut MutatorState) {
    // Lifecycle.
    install_native(coord, mutator, "AUDIO-START",
                   ncl_runtime::audio_start_shim, 0);
    install_native(coord, mutator, "AUDIO-STOP-ALL",
                   ncl_runtime::audio_stop_all_shim, 0);
    install_native(coord, mutator, "AUDIO-MASTER-VOLUME",
                   ncl_runtime::audio_master_volume_shim, 1);
    // Playback.
    install_native(coord, mutator, "AUDIO-PLAY",
                   ncl_runtime::audio_play_shim, 1);
    install_native(coord, mutator, "AUDIO-PLAY-VOL",
                   ncl_runtime::audio_play_vol_shim, 3);
    // Synthesis (return SoundId fixnum).
    install_native(coord, mutator, "AUDIO-TONE",
                   ncl_runtime::audio_tone_shim, 2);
    install_native(coord, mutator, "AUDIO-BEEP",
                   ncl_runtime::audio_beep_shim, 2);
    install_native(coord, mutator, "AUDIO-BLIP",
                   ncl_runtime::audio_blip_shim, 2);
    install_native(coord, mutator, "AUDIO-COIN",
                   ncl_runtime::audio_coin_shim, 1);
    install_native(coord, mutator, "AUDIO-JUMP",
                   ncl_runtime::audio_jump_shim, 1);
    install_native(coord, mutator, "AUDIO-ZAP",
                   ncl_runtime::audio_zap_shim, 1);
    install_native(coord, mutator, "AUDIO-HIT",
                   ncl_runtime::audio_hit_shim, 1);
    install_native(coord, mutator, "AUDIO-CLICK",
                   ncl_runtime::audio_click_shim, 1);
    // ABC.
    install_native(coord, mutator, "ABC-PLAY",
                   ncl_runtime::audio_abc_play_shim, 1);
    install_native(coord, mutator, "ABC-STOP",
                   ncl_runtime::audio_abc_stop_shim, 0);
}

/// `(eval-string s)` — feed S into the active Session, return the
/// printed result of the last form. Signals on parse / eval errors.
/// Requires `Session::activate()` to have been called by the host.
extern "C-unwind" fn eval_string_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "eval-string: expected 1 arg",
        );
    }
    let src_word = Word::from_raw(unsafe { *args });
    if src_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "eval-string: argument must be a string",
        );
    }
    let src: String = ncl_runtime::gc_string::chars_of(src_word).collect();

    let session_ptr = ACTIVE_SESSION.with(|c| c.get());
    if session_ptr.is_null() {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "eval-string: no active session (call Session::activate first)",
        );
    }
    let session = unsafe { &mut *session_ptr };

    match session.eval(&src) {
        Ok(printed) => {
            let m = unsafe { &mut *mutator };
            ncl_runtime::gc_string::alloc_string_in_young(m, &printed).raw()
        }
        Err(e) => ncl_runtime::abi::signal_condition_string(mutator, &format!("{e}")),
    }
}

/// `(%eval-form form)` — evaluate a Lisp form (given as a runtime
/// Word). Prints the form readably, re-parses it, and runs it
/// through the compiler pipeline, returning the actual result Word
/// (not a printed string). This is the bootstrap implementation of
/// CL's EVAL.
extern "C-unwind" fn eval_form_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "eval: expected 1 argument",
        );
    }
    let form_word = Word::from_raw(unsafe { *args });

    // Print the form readably so we can re-parse it.
    let src = format_word(form_word);

    let session_ptr = ACTIVE_SESSION.with(|c| c.get());
    if session_ptr.is_null() {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "eval: no active session",
        );
    }
    let session = unsafe { &mut *session_ptr };

    // Parse the printed form back into reader Values.
    let values = match ncl_reader::read_all(&src) {
        Ok(v) => v,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("eval: read error: {:?}", e.kind),
            );
        }
    };

    // Evaluate each form; return the result of the last.
    let mut last = Word::NIL;
    for v in &values {
        match session.eval_value(v) {
            Ok(w) => last = w,
            Err(e) => {
                return ncl_runtime::abi::signal_condition_string(
                    mutator,
                    &format!("eval: {e}"),
                );
            }
        }
    }
    last.raw()
}

/// `(%read-from-string string)` — read one Lisp form from STRING
/// and return it as a runtime object (cons tree / atom). Returns
/// NIL on empty input. The second CL return value (position) is
/// not yet implemented.
extern "C-unwind" fn read_from_string_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "read-from-string: expected 1 argument",
        );
    }
    let str_word = Word::from_raw(unsafe { *args });
    if str_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "read-from-string: argument must be a string",
        );
    }
    let src: String = ncl_runtime::gc_string::chars_of(str_word).collect();

    let value = match ncl_reader::read_one(&src) {
        Ok(v) => v,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("read-from-string: {:?}", e.kind),
            );
        }
    };

    // Materialise the reader Value into a runtime Word using the
    // compiler's build_quoted_word (allocates in the static area).
    let session_ptr = ACTIVE_SESSION.with(|c| c.get());
    if session_ptr.is_null() {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "read-from-string: no active session",
        );
    }
    let session = unsafe { &mut *session_ptr };

    match lower::build_quoted_word(&value, &session.coord) {
        Ok(w) => w.raw(),
        Err(e) => ncl_runtime::abi::signal_condition_string(
            mutator,
            &format!("read-from-string: {e:?}"),
        ),
    }
}

/// `(%load-file path)` — read PATH as UTF-8 source and run every
/// top-level form through the active session's evaluator. Returns
/// T on success, signals on read or eval failure.
///
/// The Lisp-level `load` wrapper in core.lisp handles `*load-path*`
/// resolution and `*modules*` recording (CL's `provide`/`require`
/// surface). This shim does only the bytes-in → eval bridge.
///
/// Requires `Session::activate()`, like eval_string_shim.
extern "C-unwind" fn load_file_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "%load-file: expected 1 arg (path)",
        );
    }
    let path_word = Word::from_raw(unsafe { *args });
    if path_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "%load-file: argument must be a string",
        );
    }
    let path: String = ncl_runtime::gc_string::chars_of(path_word).collect();

    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("%load-file: cannot read {path}: {e}"),
            );
        }
    };

    // Update the splash with the module name (file stem, e.g. "streams").
    #[cfg(windows)]
    {
        let stem = std::path::Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&path)
            .to_string();
        ncl_runtime::igui::splash::set_module(&stem);
    }

    let session_ptr = ACTIVE_SESSION.with(|c| c.get());
    if session_ptr.is_null() {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "%load-file: no active session (call Session::activate first)",
        );
    }
    let session = unsafe { &mut *session_ptr };

    match session.eval(&src) {
        Ok(_) => Word::T.raw(),
        Err(e) => ncl_runtime::abi::signal_condition_string(
            mutator,
            &format!("%load-file: error while loading {path}: {e}"),
        ),
    }
}

/// `(%file-parses-p path)` — return T iff PATH reads as a sequence
/// of well-formed Lisp forms (every parenthesis balanced, no
/// dangling quotes, etc.). Returns a string describing the
/// problem if it doesn't parse, NIL if the file can't be read.
///
/// Used by hot-reload to skip reloading a half-written file. The
/// notify watcher debounces 500ms but editors can still race the
/// pre-save lockfile dance; a parse-check is the cheap correctness
/// boundary.
extern "C-unwind" fn file_parses_p_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "%file-parses-p: expected 1 arg (path)",
        );
    }
    let path_word = Word::from_raw(unsafe { *args });
    if path_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "%file-parses-p: argument must be a string",
        );
    }
    let path: String = ncl_runtime::gc_string::chars_of(path_word).collect();
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Word::NIL.raw(),
    };
    match ncl_reader::read_all(&src) {
        Ok(_) => Word::T.raw(),
        Err(e) => {
            // Return the error description as a string so the
            // hot-reload trace can show what's wrong.
            let m = unsafe { &mut *mutator };
            let msg = format!("{:?}", e.kind);
            ncl_runtime::gc_string::alloc_string_in_young(m, &msg).raw()
        }
    }
}

/// `(compile name definition)` — compile DEFINITION (a `(lambda
/// (params…) body…)` form) into a function. If NAME is non-NIL,
/// install the compiled function in NAME's function cell. Always
/// returns the compiled function value as primary.
///
/// CL's compile is heavily overloaded — the no-definition form
/// recompiles an existing function-cell entry, the no-name form
/// just returns the compiled function, with secondary values for
/// warnings / failure status. Closette only uses the
/// `(compile NAME '(lambda …))` shape, which is what this
/// supports. The other shapes can grow when needed.
extern "C-unwind" fn compile_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "compile: expected 2 args (name definition)",
        );
    }
    let name_word = Word::from_raw(unsafe { *args });
    let def_word = Word::from_raw(unsafe { *args.add(1) });

    // Convert the runtime cons-cell tree back into a Value tree
    // the compiler can chew on. macroexpand exposes this bridge
    // already (used by EVAL-STRING).
    let def_value = match word_to_value(def_word) {
        Ok(v) => v,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("compile: cannot convert definition: {e}"),
            );
        }
    };

    // Parse `(lambda (params...) body...)`.
    let (params_value, body_forms) = match &def_value {
        ncl_runtime::Value::Cons(c) => {
            let ncl_runtime::Value::Symbol(head) = &c.car else {
                return ncl_runtime::abi::signal_condition_string(
                    mutator,
                    "compile: definition must start with the LAMBDA symbol",
                );
            };
            if &*head.name != "LAMBDA" {
                return ncl_runtime::abi::signal_condition_string(
                    mutator,
                    &format!(
                        "compile: definition must be a (lambda …) form, got ({} …)",
                        head.name
                    ),
                );
            }
            // c.cdr is (params body...). Walk it as a proper list.
            let parts = match list_to_vec_of_value(&c.cdr) {
                Ok(v) => v,
                Err(e) => {
                    return ncl_runtime::abi::signal_condition_string(
                        mutator,
                        &format!("compile: malformed lambda form: {e}"),
                    );
                }
            };
            if parts.is_empty() {
                return ncl_runtime::abi::signal_condition_string(
                    mutator,
                    "compile: lambda must have a parameter list",
                );
            }
            (parts[0].clone(), parts[1..].to_vec())
        }
        _ => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                "compile: definition must be a (lambda …) form",
            );
        }
    };

    let params = match parse_param_list(&params_value) {
        Ok(p) => p,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("compile: bad lambda list: {e}"),
            );
        }
    };

    let session_ptr = ACTIVE_SESSION.with(|c| c.get());
    if session_ptr.is_null() {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "compile: no active session (call Session::activate first)",
        );
    }
    let session = unsafe { &mut *session_ptr };

    // Use the name (or a placeholder for the no-name case) as the
    // compiled function's debug-display name. Doesn't affect the
    // installation decision below.
    let display_name: String = if name_word.is_nil() {
        "<compiled>".to_string()
    } else if name_word.tag() == ncl_runtime::Tag::Symbol {
        ncl_runtime::sym_names::lookup(name_word.raw())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".to_string())
    } else {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "compile: name must be a symbol or NIL",
        );
    };

    let fn_word = match session.compile_function(&display_name, &params, &body_forms) {
        Ok(w) => w,
        Err(e) => {
            return ncl_runtime::abi::signal_condition_string(
                mutator,
                &format!("compile: {e}"),
            );
        }
    };

    if !name_word.is_nil() {
        session.mutator.set_symbol_function(name_word, fn_word);
    }
    fn_word.raw()
}

/// `(parse-complete? s)` — T iff S parses fully (no trailing
/// open-paren / open-string / dangling unquote). NIL if more input
/// is expected. Used by the in-GUI REPL widget to decide whether
/// to accumulate or evaluate.
extern "C-unwind" fn parse_complete_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let src_word = Word::from_raw(unsafe { *args });
    if src_word.tag() != ncl_runtime::Tag::String {
        return Word::NIL.raw();
    }
    let src: String = ncl_runtime::gc_string::chars_of(src_word).collect();
    if src.trim().is_empty() {
        // Empty input is "complete" in the trivial sense (no
        // pending form).
        return Word::T.raw();
    }
    match ncl_reader::read_all(&src) {
        Ok(_) => Word::T.raw(),
        Err(e) => match e.kind {
            ncl_reader::ReaderErrorKind::UnexpectedEof(_) => Word::NIL.raw(),
            ncl_reader::ReaderErrorKind::Lex(
                ncl_reader::LexErrorKind::UnexpectedEof(_),
            ) => Word::NIL.raw(),
            // Any other parse error means the input is malformed,
            // not incomplete — call it "complete enough to
            // attempt eval, which will then fail informatively."
            _ => Word::T.raw(),
        },
    }
}

/// `(string-split-newlines s)` — split S on each `\n`, returning a
/// fresh list of (fresh) strings. A trailing newline produces a
/// final empty string in the list. Used by the GUI REPL to render
/// multi-line input correctly.
extern "C-unwind" fn string_split_newlines_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "string-split-newlines: expected 1 arg",
        );
    }
    let src_word = Word::from_raw(unsafe { *args });
    if src_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "string-split-newlines: argument must be a string",
        );
    }
    let s: String = ncl_runtime::gc_string::chars_of(src_word).collect();
    // Use split('\n') so a trailing \n produces a final empty
    // string — matches the user's mental model when the cursor is
    // sitting on a fresh line.
    let m = unsafe { &mut *mutator };
    let parts: Vec<Word> = s
        .split('\n')
        .map(|p| ncl_runtime::gc_string::alloc_string_in_young(m, p))
        .collect();
    let mut acc = Word::NIL;
    for p in parts.iter().rev() {
        acc = m.alloc_cons(*p, acc);
    }
    acc.raw()
}

/// `(substring s start end)` — return a fresh string holding
/// codepoints S[start..end]. Bounds are clamped: negative start
/// becomes 0, end past the end becomes the length.
extern "C-unwind" fn substring_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 3 {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "substring: expected 3 args (string, start, end)",
        );
    }
    let src_word = Word::from_raw(unsafe { *args });
    if src_word.tag() != ncl_runtime::Tag::String {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "substring: first arg must be a string",
        );
    }
    let start_w = Word::from_raw(unsafe { *args.add(1) });
    let end_w = Word::from_raw(unsafe { *args.add(2) });
    let Some(start) = start_w.as_fixnum() else {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "substring: start must be a fixnum",
        );
    };
    let Some(end) = end_w.as_fixnum() else {
        return ncl_runtime::abi::signal_condition_string(
            mutator,
            "substring: end must be a fixnum",
        );
    };
    let n = ncl_runtime::gc_string::char_count(src_word) as i64;
    let s = start.max(0).min(n) as usize;
    let e = end.max(0).min(n) as usize;
    if s >= e {
        let m = unsafe { &mut *mutator };
        return ncl_runtime::gc_string::alloc_string_in_young(m, "").raw();
    }
    let chars: String = ncl_runtime::gc_string::chars_of(src_word)
        .skip(s)
        .take(e - s)
        .collect();
    let m = unsafe { &mut *mutator };
    ncl_runtime::gc_string::alloc_string_in_young(m, &chars).raw()
}

#[cfg(windows)]
fn install_igui(coord: &Arc<GcCoordinator>, mutator: &mut MutatorState) {
    install_native(coord, mutator, "IGUI-START",
                   ncl_runtime::igui_start_shim, 0);
    install_native(coord, mutator, "IGUI-WAIT",
                   ncl_runtime::igui_wait_shim, 0);
    install_native(coord, mutator, "IGUI-QUIT",
                   ncl_runtime::igui_quit_shim, 0);
    install_native(coord, mutator, "OPEN-CHILD",
                   ncl_runtime::open_child_shim, 1);
    install_native(coord, mutator, "OPEN-CHILD-SIZED",
                   ncl_runtime::open_child_sized_shim, 3);
    install_native(coord, mutator, "CLOSE-CHILD",
                   ncl_runtime::close_child_shim, 1);
    install_native(coord, mutator, "SET-TITLE",
                   ncl_runtime::set_title_shim, 2);
    install_native(coord, mutator, "NEXT-EVENT",
                   ncl_runtime::next_event_shim, 1);
    install_native(coord, mutator, "NEXT-EVENT-FOR",
                   ncl_runtime::next_event_for_shim, 2);
    install_native(coord, mutator, "FILTER-ON-WINDOW",
                   ncl_runtime::filter_on_window_shim, 1);
    install_native(coord, mutator, "UNFILTER-WINDOW",
                   ncl_runtime::unfilter_window_shim, 1);
    install_native(coord, mutator, "CLEAR-EVENT-FILTER",
                   ncl_runtime::clear_event_filter_shim, 0);
    install_native(coord, mutator, "DISCARD-STASHED-EVENTS",
                   ncl_runtime::discard_stashed_events_shim, 0);
    install_native(coord, mutator, "SET-REDRAW-RATE",
                   ncl_runtime::set_redraw_rate_shim, 2);

    // Drawing primitives. The user-Lisp `with-batch` macro wraps
    // begin/submit; the emit-* helpers push commands onto the
    // thread-local current batch.
    install_native(coord, mutator, "%BEGIN-BATCH",
                   ncl_runtime::begin_batch_shim, 1);
    install_native(coord, mutator, "%SUBMIT-BATCH",
                   ncl_runtime::submit_batch_shim, 0);
    install_native(coord, mutator, "%EMIT-CLEAR",
                   ncl_runtime::emit_clear_shim, 1);
    install_native(coord, mutator, "%EMIT-FILL-RECT",
                   ncl_runtime::emit_fill_rect_shim, 5);
    install_native(coord, mutator, "%EMIT-STROKE-RECT",
                   ncl_runtime::emit_stroke_rect_shim, 6);
    install_native(coord, mutator, "%EMIT-DRAW-LINE",
                   ncl_runtime::emit_draw_line_shim, 6);
    install_native(coord, mutator, "%EMIT-DRAW-TEXT",
                   ncl_runtime::emit_draw_text_shim, 5);
    install_native(coord, mutator, "%EMIT-DRAW-TEXT-STYLED",
                   ncl_runtime::emit_draw_text_styled_shim, 6);
    install_native(coord, mutator, "%MEASURE-TEXT",
                   ncl_runtime::measure_text_shim, 4);
    install_native(coord, mutator, "%EMIT-FILL-OVAL",
                   ncl_runtime::emit_fill_oval_shim, 5);
    install_native(coord, mutator, "%EMIT-STROKE-OVAL",
                   ncl_runtime::emit_stroke_oval_shim, 6);
    install_native(coord, mutator, "%EMIT-FILL-CIRCLE",
                   ncl_runtime::emit_fill_circle_shim, 4);
    install_native(coord, mutator, "%EMIT-STROKE-CIRCLE",
                   ncl_runtime::emit_stroke_circle_shim, 5);
    install_native(coord, mutator, "%EMIT-DRAW-ARC",
                   ncl_runtime::emit_draw_arc_shim, 7);

    // Log view bridge.
    install_native(coord, mutator, "LOG-WRITE",
                   ncl_runtime::log_write_shim, 1);

    // Text-view (terminal-style monospaced child).
    install_native(coord, mutator, "OPEN-TEXT-WINDOW",
                   ncl_runtime::open_text_window_shim, 1);
    install_native(coord, mutator, "TEXT-WRITE",
                   ncl_runtime::text_write_shim, 2);
    install_native(coord, mutator, "TEXT-WRITE-CHAR",
                   ncl_runtime::text_write_char_shim, 2);
    install_native(coord, mutator, "TEXT-CLEAR",
                   ncl_runtime::text_clear_shim, 1);
    install_native(coord, mutator, "TEXT-CLEAR-EOL",
                   ncl_runtime::text_clear_eol_shim, 1);
    install_native(coord, mutator, "TEXT-CLEAR-EOS",
                   ncl_runtime::text_clear_eos_shim, 1);
    install_native(coord, mutator, "TEXT-NEWLINE",
                   ncl_runtime::text_newline_shim, 1);
    install_native(coord, mutator, "TEXT-SCROLL-UP",
                   ncl_runtime::text_scroll_up_shim, 2);
    install_native(coord, mutator, "TEXT-SET-CURSOR",
                   ncl_runtime::text_set_cursor_shim, 3);
    install_native(coord, mutator, "TEXT-SET-PEN",
                   ncl_runtime::text_set_pen_shim, 3);
    install_native(coord, mutator, "TEXT-RESET-PEN",
                   ncl_runtime::text_reset_pen_shim, 1);
    install_native(coord, mutator, "TEXT-SHOW-CARET",
                   ncl_runtime::text_show_caret_shim, 2);

    // REPL child
    install_native(coord, mutator, "OPEN-REPL-WINDOW",
                   ncl_runtime::open_repl_window_shim, 1);
    install_native(coord, mutator, "REPL-OUTPUT",
                   ncl_runtime::repl_output_shim, 2);
    install_native(coord, mutator, "REPL-ERROR",
                   ncl_runtime::repl_error_shim, 2);
    install_native(coord, mutator, "REPL-POP-INPUT",
                   ncl_runtime::repl_pop_input_shim, 1);

    // Doc pane — Markdown + Mermaid renderer.
    install_native(coord, mutator, "OPEN-DOC-WINDOW",
                   ncl_runtime::open_doc_window_shim, 1);
    install_native(coord, mutator, "DOC-SET-MARKDOWN",
                   ncl_runtime::doc_set_markdown_shim, 2);
    install_native(coord, mutator, "DOC-APPEND-MARKDOWN",
                   ncl_runtime::doc_append_markdown_shim, 2);

    // Canvas — host-owned BGRA32 framebuffer with fast pixel-direct
    // writes from Lisp. CANVAS-OPEN returns the buffer base address;
    // poke pixels with BUFFER-SET-U32 then CANVAS-PRESENT to draw.
    install_native(coord, mutator, "CANVAS-OPEN",
                   ncl_runtime::canvas_open_shim, 3);
    install_native(coord, mutator, "CANVAS-PRESENT",
                   ncl_runtime::canvas_present_shim, 1);

    // MDI window-management verbs (incl. arranging minimized-child
    // icons when a window is maximized/full within the frame).
    install_native(coord, mutator, "MDI-ARRANGE-ICONS",
                   ncl_runtime::mdi_arrange_icons_shim, 0);
    install_native(coord, mutator, "MDI-CASCADE",
                   ncl_runtime::mdi_cascade_shim, 0);
    install_native(coord, mutator, "MDI-TILE",
                   ncl_runtime::mdi_tile_shim, 0);
}

fn install_native(
    coord: &Arc<GcCoordinator>,
    mutator: &mut MutatorState,
    name: &str,
    code: extern "C-unwind" fn(*mut MutatorState, u64, *const u64, u64) -> u64,
    arity: u32,
) {
    let sym_word = coord.intern(name);
    let fn_word = gc_function::alloc_function_in_static(
        coord.static_area(),
        code as usize,
        arity,
        sym_word,
        Word::NIL, // native functions don't carry a closure env
        false,     // native Rust shim; does NOT manage the MV slot itself
    )
    .expect("static area exhausted while installing native function");
    mutator.set_symbol_function(sym_word, fn_word);
}

/// User-Lisp portion of the standard library. Embedded at compile
/// time so the binary is self-contained — no filesystem lookup at
/// runtime, no working-directory dependence in tests. The Rust-side
/// glue (numeric primitives, condition machinery) lives in
/// `ncl-cl`; everything written in Lisp lives here.
const CORE_LISP_SOURCE: &str = include_str!("../../../Lisp/core.lisp");

/// Closette CLOS port — staged Lisp source. Loaded after the
/// core stdlib because it depends on every primitive (vectors,
/// hash tables, defstruct, &key, multiple values, block /
/// return-from, compile, flet/labels, the lot). See the staging
/// plan in the commit log for which sections cover which
/// chunks.
const CLOS_LISP_SOURCE: &str = include_str!("../../../Lisp/clos.lisp");

impl Session {
    /// Read and evaluate the embedded core-stdlib source. Each defun
    /// goes through the same JIT path as user code; the resulting
    /// Function objects are installed in the symbols' function
    /// cells. Idempotent only in the trivial sense — calling twice
    /// re-defines every function.
    /// Drain the startup-timing accumulator and print a report.
    /// Called from the driver after the Library/init.lisp load phase
    /// so that per-form Library timings are visible.
    pub fn drain_startup_timing(label: &str, elapsed_ms: u128) {
        startup_timing::drain_and_report(
            label,
            std::time::Duration::from_millis(elapsed_ms as u64),
        );
    }

    pub fn load_core_stdlib(&mut self) -> Result<(), EvalError> {
        #[cfg(windows)]
        ncl_runtime::igui::splash::set_module("core");
        let t = startup_timing::now();
        self.eval(CORE_LISP_SOURCE)?;
        startup_timing::drain_and_report("core.lisp", startup_timing::elapsed(t));
        Ok(())
    }

    /// Load Closette on top of the core stdlib. Order matters —
    /// CLOS uses everything in core (defstruct, hash tables, &key,
    /// labels, etc.). Idempotent in the same trivial sense as
    /// `load_core_stdlib`.
    pub fn load_clos(&mut self) -> Result<(), EvalError> {
        #[cfg(windows)]
        ncl_runtime::igui::splash::set_module("clos");
        let t = startup_timing::now();
        self.eval(CLOS_LISP_SOURCE)?;
        startup_timing::drain_and_report("clos.lisp", startup_timing::elapsed(t));
        Ok(())
    }

    /// Convenience: a session with the core stdlib + CLOS pre-loaded.
    pub fn with_stdlib() -> Result<Session, EvalError> {
        let t_init = startup_timing::now();
        let mut s = Session::new();
        startup_timing::report_phase("native-install", startup_timing::elapsed(t_init));

        // Activate the session for the duration of stdlib load so
        // any (compile ...) calls (e.g. CLOS method functions
        // generated during defmethod expansion) can find the
        // active session. The caller will (re)activate when they
        // start using the session for user code.
        s.activate();
        let r1 = s.load_core_stdlib();
        if let Err(e) = r1 {
            ACTIVE_SESSION.with(|c| c.set(std::ptr::null_mut()));
            return Err(e);
        }
        let r2 = s.load_clos();
        ACTIVE_SESSION.with(|c| c.set(std::ptr::null_mut()));
        r2?;
        Ok(s)
    }

    /// Minimal session: core stdlib only, no CLOS. The driver's
    /// `--lean` flag uses this — small enough for scripting or
    /// sandboxing where the user explicitly does not want CLOS
    /// and the Library/init.lisp auto-load.
    ///
    /// What's in: cons / car / cdr / arithmetic / let / cond / loop /
    /// format / mapcar / file I/O / hash tables / defstruct / sort /
    /// typep / handler-case / load / require / provide — everything
    /// in core.lisp.
    /// What's not: defclass / defmethod / defgeneric / make-instance
    /// / slot-value / standard-class — anything CLOS.
    pub fn with_minimal_stdlib() -> Result<Session, EvalError> {
        let mut s = Session::new();
        s.activate();
        let r = s.load_core_stdlib();
        ACTIVE_SESSION.with(|c| c.set(std::ptr::null_mut()));
        r?;
        Ok(s)
    }
}

/// Recognise `(defun name (params...) body...)`. Returns `Some` if
/// the form is a defun. Implicit progn is supported — multiple body
/// forms are returned as a Vec for the caller to wrap.
fn match_defun(
    v: &Value,
) -> Result<Option<(String, ParamSpec, Vec<Value>)>, EvalError> {
    match_defun_like(v, "DEFUN")
}

/// Recognise `(progn body...)` so top-level progn can splice its
/// body forms back into the top-level recogniser. Returns the
/// body as a Vec of Values, or None if the form isn't a progn.
/// Wrap a defun body in `(BLOCK <name> body…)` so that a
/// `(return-from <name> val)` anywhere in the body finds its
/// target. Per ANSI CL: every defun's body is implicitly a
/// (block NAME …). The result is a single Value the caller passes
/// as the new body — macroexpansion will turn the BLOCK form into
/// the existing `(%native-block 'NAME (lambda () body…))` shape.
///
/// Both symbols (`BLOCK` and `<name>`) are interned in
/// `COMMON-LISP`; the macroexpander matches by name string, so
/// the package choice is for hygiene only.
/// Syntactic walk: is `(return-from <target_name> …)` anywhere in
/// `body_forms`?
///
/// Matches by symbol name (case-insensitive — Common Lisp interns
/// uppercased by default). Walks every Cons recursively. Used by
/// `maybe_wrap_in_block` to decide whether a defun body actually
/// needs the BLOCK wrap.
fn body_uses_return_from_name(body_forms: &[Value], target_name: &str) -> bool {
    body_forms
        .iter()
        .any(|f| value_uses_return_from_name(f, target_name))
}

fn value_uses_return_from_name(v: &Value, target_name: &str) -> bool {
    let Value::Cons(c) = v else {
        return false;
    };
    if let Value::Symbol(head) = &c.car {
        if head.name.eq_ignore_ascii_case("return-from") {
            if let Value::Cons(rest) = &c.cdr {
                if let Value::Symbol(target) = &rest.car {
                    if target.name.eq_ignore_ascii_case(target_name) {
                        return true;
                    }
                }
            }
        }
    }
    value_uses_return_from_name(&c.car, target_name)
        || value_uses_return_from_name(&c.cdr, target_name)
}

fn wrap_body_in_block(name: &str, body_forms: &[Value]) -> Value {
    let cl = ncl_runtime::universe()
        .find_package("COMMON-LISP")
        .expect("CL bootstrapped");
    let (block_sym, _) = cl.intern("BLOCK");
    let (name_sym, _) = cl.intern(name);
    let mut items: Vec<Value> = Vec::with_capacity(2 + body_forms.len());
    items.push(Value::Symbol(block_sym));
    items.push(Value::Symbol(name_sym));
    items.extend(body_forms.iter().cloned());
    Value::list(items)
}

fn match_top_level_progn(v: &Value) -> Option<Vec<Value>> {
    let Value::Cons(c) = v else { return None };
    let Value::Symbol(head) = &c.car else { return None };
    if &*head.name != "PROGN" {
        return None;
    }
    list_to_vec_of_value(&c.cdr).ok()
}

/// Heuristic for the `--check` flag: is `v` a top-level form whose
/// side-effect matters for later forms in the same file?
///
/// Examples that return true: `defparameter`, `defvar`, `defconstant`,
/// `setq` of a global (people use it as a poor person's defparameter),
/// `require` / `provide` / `load` / `in-package` (module bookkeeping).
///
/// `defun` / `defmacro` are recognised separately by the caller —
/// they go through the explicit handle_* path so the function body
/// is compiled at top level. progn is also handled separately
/// (recursive splice).
fn is_definition_like(v: &Value) -> bool {
    let Value::Cons(c) = v else { return false };
    let Value::Symbol(head) = &c.car else { return false };
    matches!(
        &*head.name,
        "DEFPARAMETER"
            | "DEFVAR"
            | "DEFCONSTANT"
            | "SETQ"
            | "SETF"
            | "REQUIRE"
            | "PROVIDE"
            | "LOAD"
            | "IN-PACKAGE"
    )
}

/// Recognise `(defmacro name (params...) body...)`. Same shape as
/// defun; the body is compiled the same way but installed in the
/// coordinator's macro registry rather than the symbol's function
/// cell.
fn match_defmacro(
    v: &Value,
) -> Result<Option<(String, ParamSpec, Vec<Value>)>, EvalError> {
    match_defun_like(v, "DEFMACRO")
}

/// Recognise `(defasm name (param...) "line1" "line2" ...)`.
/// Returns `(name, param_names, body_lines)` or None.
fn match_defasm(
    v: &Value,
) -> Result<Option<(String, Vec<String>, Vec<String>)>, EvalError> {
    let Value::Cons(c) = v else { return Ok(None) };
    let Value::Symbol(head) = &c.car else { return Ok(None) };
    if &*head.name != "DEFASM" {
        return Ok(None);
    }
    let args = list_to_vec_of_value(&c.cdr).map_err(|e| {
        EvalError::Compile(CompileError::ImproperList(e))
    })?;
    if args.len() < 2 {
        return Err(EvalError::Compile(CompileError::BadDefun(
            "defasm needs name and param list".to_string(),
        )));
    }
    let name = match &args[0] {
        Value::Symbol(s) => s.name.to_string(),
        _ => return Err(EvalError::Compile(CompileError::BadDefun(
            "defasm name must be a symbol".to_string(),
        ))),
    };
    let param_names: Vec<String> = {
        let param_list = list_to_vec_of_value(&args[1]).map_err(|e| {
            EvalError::Compile(CompileError::ImproperList(e))
        })?;
        param_list.iter().map(|p| match p {
            Value::Symbol(s) => Ok(s.name.to_string()),
            _ => Err(EvalError::Compile(CompileError::BadDefun(
                "defasm param must be a symbol".to_string(),
            ))),
        }).collect::<Result<Vec<_>, _>>()?
    };
    let body_lines: Vec<String> = args[2..]
        .iter()
        .map(|form| match form {
            Value::String(s) => Ok(s.as_ref().clone()),
            _ => Err(EvalError::Compile(CompileError::BadDefun(
                "defasm body lines must be string literals".to_string(),
            ))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((name, param_names, body_lines)))
}

fn match_defun_like(
    v: &Value,
    head_name: &str,
) -> Result<Option<(String, ParamSpec, Vec<Value>)>, EvalError> {
    let Value::Cons(c) = v else { return Ok(None); };
    let Value::Symbol(head) = &c.car else { return Ok(None); };
    if &*head.name != head_name {
        return Ok(None);
    }
    let args = list_to_vec_of_value(&c.cdr).map_err(|e| {
        EvalError::Compile(CompileError::ImproperList(e))
    })?;
    if args.len() < 3 {
        return Err(EvalError::Compile(CompileError::BadDefun(format!(
            "{} needs name, params, and body — got {} args",
            head_name.to_lowercase(),
            args.len()
        ))));
    }
    let name = match &args[0] {
        Value::Symbol(s) => s.name.to_string(),
        // CL accepts `(setf NAME)` as a function name. We mangle
        // it to `%SETF-NAME` so the existing generic-setf-fallback
        // in lower.rs (which expands `(setf (NAME args…) val)`
        // into `(%SETF-NAME val args…)`) finds the user's setter
        // automatically. This is the standard pattern used by
        // every CL implementation; ours just happens to mangle
        // explicitly rather than carrying a separate function-name
        // namespace.
        Value::Cons(c) => {
            let parts = list_to_vec_of_value(&Value::Cons(c.clone())).map_err(|e| {
                EvalError::Compile(CompileError::ImproperList(e))
            })?;
            if parts.len() != 2 {
                return Err(EvalError::Compile(CompileError::BadDefun(format!(
                    "{} name must be SYMBOL or (SETF SYMBOL), got a list of length {}",
                    head_name.to_lowercase(),
                    parts.len(),
                ))));
            }
            let (Value::Symbol(head_sym), Value::Symbol(tgt_sym)) =
                (&parts[0], &parts[1])
            else {
                return Err(EvalError::Compile(CompileError::BadDefun(format!(
                    "{} name list must contain symbols, got {:?}",
                    head_name.to_lowercase(),
                    parts,
                ))));
            };
            if &*head_sym.name != "SETF" {
                return Err(EvalError::Compile(CompileError::BadDefun(format!(
                    "{} compound name must start with SETF, got ({} ...)",
                    head_name.to_lowercase(),
                    head_sym.name,
                ))));
            }
            format!("%SETF-{}", tgt_sym.name)
        }
        other => {
            return Err(EvalError::Compile(CompileError::BadDefun(format!(
                "{} name must be a symbol or (SETF SYMBOL), got {other:?}",
                head_name.to_lowercase()
            ))));
        }
    };
    let params = parse_param_list(&args[1])?;
    let body_forms = args[2..].to_vec();
    Ok(Some((name, params, body_forms)))
}

/// Parsed parameter list. CL canonical order: required, optional,
/// rest, key. Each later category may be empty; the only currently
/// unsupported feature is `&aux` (post-key let-bindings) and the
/// optional/key supplied-p flag (`(name default supplied-p)`).
#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub required: Vec<Arc<str>>,
    pub optionals: Vec<OptParam>,
    pub rest: Option<Arc<str>>,
    pub keys: Vec<KeyParam>,
    /// `&whole` variable (macro lambda lists). Bound to NIL for now —
    /// non-positional, so it never consumes an argument slot. A real
    /// binding (the whole macro-call form) is a future enhancement.
    pub whole: Option<Arc<str>>,
    /// `&environment` variable (macro lambda lists). Bound to NIL for
    /// now — non-positional. A real lexical macro environment is a
    /// future enhancement; until then `(macroexpand x env)` inside an
    /// expander sees only the global environment.
    pub environment: Option<Arc<str>>,
}

/// One `&optional` parameter. `default` is the raw form the user
/// wrote (or None for no default — implicit NIL); the lowering pass
/// evaluates it lazily, only when the caller didn't supply this
/// argument. The default form is lowered in an env that has the
/// required params and all earlier optionals already bound, so
/// `(defun f (a &optional (b (* a 2))))` works.
#[derive(Debug, Clone)]
pub struct OptParam {
    pub name: Arc<str>,
    pub default: Option<Value>,
    /// If present, a variable bound to T when the caller supplied
    /// this argument, NIL otherwise. CL `(name default supplied-p)`.
    pub supplied_p: Option<Arc<str>>,
}

/// One `&key` parameter. `keyword` is the colon-prefixed name used
/// by callers (e.g. `:FAMILY`); `name` is the binding name inside
/// the body. CL allows `((:other-name name) default)` to decouple
/// them; for now we only support the common case where keyword
/// equals `:NAME`. `default` lazily evaluated, same shape as for
/// optionals.
#[derive(Debug, Clone)]
pub struct KeyParam {
    pub name: Arc<str>,
    pub keyword: Arc<str>,
    pub default: Option<Value>,
    pub supplied_p: Option<Arc<str>>,
}

/// Parsing state machine for an arglist. Steps strictly through
/// `&optional` -> `&rest`/`&body` -> `&key` in that order; an
/// out-of-order marker is a hard error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArglistMode {
    Required,
    Optional,
    Rest,
    Key,
}

fn parse_param_list(v: &Value) -> Result<ParamSpec, EvalError> {
    parse_param_list_inner(v).map_err(|s| {
        EvalError::Compile(CompileError::BadDefun(s))
    })
}

/// Internal parse with String errors — used by both the defun-side
/// (lib.rs) and lambda-side (lower.rs) entry points so the shape
/// stays identical between them.
pub(crate) fn parse_param_list_inner(v: &Value) -> Result<ParamSpec, String> {
    let mut required: Vec<Arc<str>> = Vec::new();
    let mut optionals: Vec<OptParam> = Vec::new();
    let mut rest: Option<Arc<str>> = None;
    let mut keys: Vec<KeyParam> = Vec::new();
    let mut whole: Option<Arc<str>> = None;
    let mut environment: Option<Arc<str>> = None;
    let mut mode = ArglistMode::Required;
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => break,
            Value::Cons(c) => {
                let head = c.car.clone();
                cur = c.cdr.clone();
                // Lambda-list keyword?
                if let Value::Symbol(s) = &head {
                    match &*s.name {
                        "&OPTIONAL" => {
                            if mode != ArglistMode::Required {
                                return Err(format!(
                                    "&OPTIONAL out of order in arglist"
                                ));
                            }
                            mode = ArglistMode::Optional;
                            continue;
                        }
                        "&REST" | "&BODY" => {
                            if matches!(mode, ArglistMode::Rest | ArglistMode::Key) {
                                return Err(format!(
                                    "&REST/&BODY out of order in arglist"
                                ));
                            }
                            // Take exactly one symbol.
                            let Value::Cons(rc) = cur.clone() else {
                                return Err(
                                    "&REST/&BODY must be followed by a name".into(),
                                );
                            };
                            let Value::Symbol(rs) = &rc.car else {
                                return Err(format!(
                                    "&REST/&BODY name must be a symbol, got {:?}",
                                    rc.car
                                ));
                            };
                            rest = Some(Arc::clone(&rs.name));
                            cur = rc.cdr.clone();
                            mode = ArglistMode::Rest;
                            continue;
                        }
                        "&KEY" => {
                            if mode == ArglistMode::Key {
                                return Err("duplicate &KEY in arglist".into());
                            }
                            mode = ArglistMode::Key;
                            continue;
                        }
                        "&AUX" => {
                            return Err("&AUX is not yet supported".into());
                        }
                        // &WHOLE / &ENVIRONMENT (macro lambda lists).
                        // Each takes exactly one name. They are
                        // non-positional — recorded separately and
                        // bound to NIL in the prologue so they never
                        // consume an argument slot (and so a 0-arg
                        // macro call to a `(foo &environment e)` macro
                        // doesn't read past the supplied args).
                        "&WHOLE" | "&ENVIRONMENT" => {
                            let kw = s.name.clone();
                            let Value::Cons(nc) = cur.clone() else {
                                return Err(format!(
                                    "{kw} must be followed by a name"
                                ));
                            };
                            let Value::Symbol(ns) = &nc.car else {
                                return Err(format!(
                                    "{kw} name must be a symbol, got {:?}",
                                    nc.car
                                ));
                            };
                            if &*kw == "&WHOLE" {
                                whole = Some(Arc::clone(&ns.name));
                            } else {
                                environment = Some(Arc::clone(&ns.name));
                            }
                            cur = nc.cdr.clone();
                            continue;
                        }
                        "&ALLOW-OTHER-KEYS" => {
                            // ncl_lookup_keyword silently ignores
                            // unknown keywords already, so this
                            // marker is a no-op for us. Accept it
                            // so Closette code that uses it (e.g.
                            // initialize-instance dispatching to
                            // shared-initialize) parses.
                            continue;
                        }
                        _ => {}
                    }
                }
                // Non-keyword entry — interpret per current mode.
                match mode {
                    ArglistMode::Required => {
                        let Value::Symbol(s) = &head else {
                            return Err(format!(
                                "required parameter must be a symbol, got {head:?}"
                            ));
                        };
                        required.push(Arc::clone(&s.name));
                    }
                    ArglistMode::Optional => {
                        let (name, default, supplied_p) = parse_init_form(&head)?;
                        optionals.push(OptParam { name, default, supplied_p });
                    }
                    ArglistMode::Rest => {
                        return Err(format!(
                            "extra parameter after &REST: {head:?}"
                        ));
                    }
                    ArglistMode::Key => {
                        let (name, default, supplied_p) = parse_init_form(&head)?;
                        // Convention: the matching keyword is `:NAME`.
                        let kw_name = format!(":{}", name);
                        keys.push(KeyParam {
                            name,
                            keyword: Arc::from(kw_name.as_str()),
                            default,
                            supplied_p,
                        });
                    }
                }
            }
            other => {
                return Err(format!(
                    "param list must be a proper list, got {other:?}"
                ));
            }
        }
    }
    Ok(ParamSpec { required, optionals, rest, keys, whole, environment })
}

/// Parse one entry of the form `name`, `(name)`, `(name default)`,
/// or `(name default supplied-p)`. Returns the name, optional default
/// form, and optional supplied-p variable name.
fn parse_init_form(v: &Value) -> Result<(Arc<str>, Option<Value>, Option<Arc<str>>), String> {
    match v {
        Value::Symbol(s) => Ok((Arc::clone(&s.name), None, None)),
        Value::Cons(_) => {
            let elems = list_to_vec_of_value(v)?;
            if elems.is_empty() || elems.len() > 3 {
                return Err(format!(
                    "init-form must be (name [default [supplied-p]]), got {} elements",
                    elems.len()
                ));
            }
            let name = match &elems[0] {
                Value::Symbol(s) => Arc::clone(&s.name),
                other => return Err(format!(
                    "init-form name must be a symbol, got {other:?}"
                )),
            };
            let default = if elems.len() >= 2 {
                Some(elems[1].clone())
            } else {
                None
            };
            let supplied_p = if elems.len() == 3 {
                match &elems[2] {
                    Value::Symbol(s) => Some(Arc::clone(&s.name)),
                    other => return Err(format!(
                        "supplied-p must be a symbol, got {other:?}"
                    )),
                }
            } else {
                None
            };
            Ok((name, default, supplied_p))
        }
        other => Err(format!(
            "init-form must be a symbol or a list, got {other:?}"
        )),
    }
}

fn list_to_vec_of_value(v: &Value) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    let mut cur = v.clone();
    loop {
        match cur {
            Value::Nil => return Ok(out),
            Value::Cons(c) => {
                out.push(c.car.clone());
                cur = c.cdr.clone();
            }
            other => return Err(format!("{other:?}")),
        }
    }
}

/// One-shot evaluation: create a fresh `Session`, evaluate the
/// source, return the printed result. Equivalent to
/// `Session::new().eval(src)` — the convenience entry point used
/// by the driver's `--eval` flag and most tests.
pub fn eval_str(src: &str) -> Result<String, EvalError> {
    Session::new().eval(src)
}

#[derive(Debug)]
pub enum EvalError {
    Read(String),
    Compile(CompileError),
    Jit(String),
    /// A Lisp condition escaped to the top level with no handler.
    /// Captured by the top-level eval guard so it surfaces as a
    /// recoverable error (REPL keeps going, embedders get an Err)
    /// instead of terminating the process.
    Runtime(String),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Read(s) => write!(f, "read error: {s}"),
            EvalError::Compile(e) => write!(f, "compile error: {e:?}"),
            EvalError::Jit(s) => write!(f, "jit error: {s}"),
            EvalError::Runtime(s) => write!(f, "unhandled condition: {s}"),
        }
    }
}

impl std::error::Error for EvalError {}

#[cfg(test)]
mod end_to_end_tests {
    use super::*;

    // -- Existing tests, retained --------------------------------------------

    #[test]
    fn the_milestone_one_plus_two_equals_three() {
        assert_eq!(eval_str("(+ 1 2)").unwrap(), "3");
    }

    #[test]
    fn integer_literal_evaluates_to_itself() {
        assert_eq!(eval_str("42").unwrap(), "42");
        assert_eq!(eval_str("-7").unwrap(), "-7");
        assert_eq!(eval_str("0").unwrap(), "0");
    }

    #[test]
    fn nil_evaluates_to_nil() {
        assert_eq!(eval_str("nil").unwrap(), "nil");
        assert_eq!(eval_str("()").unwrap(), "nil");
    }

    #[test]
    fn arithmetic_combinations_eval_correctly() {
        assert_eq!(eval_str("(+ 1 2)").unwrap(), "3");
        assert_eq!(eval_str("(- 10 3)").unwrap(), "7");
        assert_eq!(eval_str("(* 6 7)").unwrap(), "42");
        assert_eq!(eval_str("(- 5 10)").unwrap(), "-5");
    }

    #[test]
    fn nested_arithmetic_evals_correctly() {
        assert_eq!(eval_str("(* (+ 1 2) (- 10 4))").unwrap(), "18");
        assert_eq!(eval_str("(* (+ 1 2 3) (- 10 7))").unwrap(), "18");
    }

    #[test]
    fn factorial_5_via_unrolled_multiplication() {
        assert_eq!(eval_str("(* 1 2 3 4 5)").unwrap(), "120");
    }

    #[test]
    fn cons_creates_a_pair() {
        assert_eq!(eval_str("(cons 1 2)").unwrap(), "(1 . 2)");
        assert_eq!(eval_str("(cons 1 nil)").unwrap(), "(1)");
    }

    #[test]
    fn car_and_cdr_extract() {
        assert_eq!(eval_str("(car (cons 1 2))").unwrap(), "1");
        assert_eq!(eval_str("(cdr (cons 1 2))").unwrap(), "2");
    }

    #[test]
    fn proper_list_via_nested_cons() {
        assert_eq!(
            eval_str("(cons 1 (cons 2 (cons 3 nil)))").unwrap(),
            "(1 2 3)",
        );
    }

    #[test]
    fn eq_returns_t_for_equal_fixnums() {
        assert_eq!(eval_str("(eq 1 1)").unwrap(), "T");
        assert_eq!(eval_str("(eq 1 2)").unwrap(), "nil");
    }

    #[test]
    fn if_chooses_correct_branch() {
        assert_eq!(eval_str("(if t 7 8)").unwrap(), "7");
        assert_eq!(eval_str("(if nil 7 8)").unwrap(), "8");
        assert_eq!(eval_str("(if (eq 1 1) 7 8)").unwrap(), "7");
    }

    #[test]
    fn quote_fixnum_and_nil_and_t() {
        assert_eq!(eval_str("(quote 42)").unwrap(), "42");
        assert_eq!(eval_str("'42").unwrap(), "42");
        assert_eq!(eval_str("(quote nil)").unwrap(), "nil");
        assert_eq!(eval_str("'t").unwrap(), "T");
    }

    // -- Multi-form evaluation ---------------------------------------------

    #[test]
    fn multi_form_returns_last_result() {
        assert_eq!(eval_str("(+ 1 2) (* 3 4)").unwrap(), "12");
        assert_eq!(eval_str("1 2 3 4 5").unwrap(), "5");
    }

    #[test]
    fn empty_input_evaluates_to_nil() {
        assert_eq!(eval_str("").unwrap(), "nil");
    }

    // -- defun + function calls --------------------------------------------

    #[test]
    fn defun_creates_a_function_then_call_runs() {
        let mut session = Session::new();
        session.eval("(defun double (x) (+ x x))").unwrap();
        assert_eq!(session.eval("(double 21)").unwrap(), "42");
    }

    #[test]
    fn defun_via_multi_form_string() {
        // defun followed by a call in the same source.
        let result = eval_str("(defun triple (x) (+ x (+ x x))) (triple 7)").unwrap();
        assert_eq!(result, "21");
    }

    #[test]
    fn defun_with_two_params() {
        let mut session = Session::new();
        session.eval("(defun mul-add (x y) (+ (* x x) y))").unwrap();
        assert_eq!(session.eval("(mul-add 3 7)").unwrap(), "16");
    }

    #[test]
    fn redefinition_replaces_function() {
        let mut session = Session::new();
        session.eval("(defun id (x) x)").unwrap();
        assert_eq!(session.eval("(id 42)").unwrap(), "42");

        // Redefine.
        session.eval("(defun id (x) (+ x 100))").unwrap();
        assert_eq!(session.eval("(id 42)").unwrap(), "142");
    }

    // -- The big one: recursive defun --------------------------------------

    #[test]
    fn recursive_factorial_5_equals_120() {
        let result = eval_str(
            "(defun fact (n) (if (eq n 0) 1 (* n (fact (- n 1)))))
             (fact 5)",
        )
        .unwrap();
        assert_eq!(result, "120");
    }

    #[test]
    fn recursive_factorial_10_equals_3628800() {
        let result = eval_str(
            "(defun fact (n) (if (eq n 0) 1 (* n (fact (- n 1)))))
             (fact 10)",
        )
        .unwrap();
        assert_eq!(result, "3628800");
    }

    #[test]
    fn recursive_function_returning_cons() {
        // (defun count-down (n) (if (eq n 0) nil (cons n (count-down (- n 1)))))
        // (count-down 4) → (4 3 2 1)
        let result = eval_str(
            "(defun count-down (n)
               (if (eq n 0) nil (cons n (count-down (- n 1)))))
             (count-down 4)",
        )
        .unwrap();
        assert_eq!(result, "(4 3 2 1)");
    }

    #[test]
    fn function_calling_function() {
        let mut session = Session::new();
        session.eval("(defun double (x) (+ x x))").unwrap();
        session.eval("(defun quadruple (x) (double (double x)))").unwrap();
        assert_eq!(session.eval("(quadruple 5)").unwrap(), "20");
    }

    // -- progn -------------------------------------------------------------

    #[test]
    fn progn_returns_last_value() {
        assert_eq!(eval_str("(progn 1 2 3)").unwrap(), "3");
        assert_eq!(eval_str("(progn (+ 1 1) (* 3 4))").unwrap(), "12");
    }

    #[test]
    fn empty_progn_is_nil() {
        assert_eq!(eval_str("(progn)").unwrap(), "nil");
    }

    #[test]
    fn progn_in_function_body() {
        let mut session = Session::new();
        session
            .eval(
                "(defun do-stuff (x)
                   (progn
                     (* x 2)
                     (* x 3)
                     (* x 10)))",
            )
            .unwrap();
        assert_eq!(session.eval("(do-stuff 7)").unwrap(), "70");
    }

    #[test]
    fn implicit_progn_in_defun_body() {
        // No explicit progn; multi-form body.
        let mut session = Session::new();
        session
            .eval("(defun sum-of-cubes (x) (* x x) (* x x x))")
            .unwrap();
        assert_eq!(session.eval("(sum-of-cubes 3)").unwrap(), "27");
    }

    // -- let ---------------------------------------------------------------

    #[test]
    fn let_binds_local_variable() {
        assert_eq!(eval_str("(let ((x 10)) x)").unwrap(), "10");
        assert_eq!(eval_str("(let ((x 10)) (+ x x))").unwrap(), "20");
    }

    #[test]
    fn let_with_multiple_bindings() {
        assert_eq!(
            eval_str("(let ((x 10) (y 20)) (+ x y))").unwrap(),
            "30",
        );
    }

    #[test]
    fn let_bindings_are_parallel() {
        // Outer x = 1; inner let evaluates `(+ x 100)` with x=1
        // (outer scope), THEN binds x=101 for the body. Body uses
        // x=101 and y=2.
        let mut session = Session::new();
        session.eval("(defun id (n) n)").unwrap();
        // No outer x; this just tests the basic binding.
        assert_eq!(
            session.eval("(let ((x 5) (y 7)) (* x y))").unwrap(),
            "35",
        );
    }

    #[test]
    fn nested_let() {
        assert_eq!(
            eval_str("(let ((x 10)) (let ((y 5)) (+ x y)))").unwrap(),
            "15",
        );
    }

    #[test]
    fn inner_let_shadows_outer() {
        assert_eq!(
            eval_str("(let ((x 1)) (let ((x 99)) x))").unwrap(),
            "99",
        );
        assert_eq!(
            eval_str("(let ((x 1)) (+ (let ((x 99)) x) x))").unwrap(),
            "100",
        );
    }

    #[test]
    fn let_in_function_body() {
        let mut session = Session::new();
        session
            .eval(
                "(defun hypot-sq (a b)
                   (let ((aa (* a a))
                         (bb (* b b)))
                     (+ aa bb)))",
            )
            .unwrap();
        assert_eq!(session.eval("(hypot-sq 3 4)").unwrap(), "25");
    }

    #[test]
    fn let_with_multiple_body_forms() {
        // Implicit progn inside let.
        assert_eq!(
            eval_str("(let ((x 5)) 99 (+ x x))").unwrap(),
            "10",
        );
    }

    #[test]
    fn let_can_shadow_param() {
        let mut session = Session::new();
        session
            .eval("(defun f (x) (let ((x 99)) x))")
            .unwrap();
        assert_eq!(session.eval("(f 1)").unwrap(), "99");
    }

    #[test]
    fn empty_let_body_is_nil() {
        assert_eq!(eval_str("(let ((x 5)))").unwrap(), "nil");
    }

    // -- lambda + closures ------------------------------------------------

    #[test]
    fn lambda_no_capture_via_funcall() {
        assert_eq!(eval_str("(funcall (lambda (x) (+ x 1)) 41)").unwrap(), "42");
        assert_eq!(eval_str("(funcall (lambda (x y) (* x y)) 6 7)").unwrap(), "42");
    }

    #[test]
    fn lambda_zero_args() {
        assert_eq!(eval_str("(funcall (lambda () 42))").unwrap(), "42");
    }

    #[test]
    fn lambda_can_be_assigned_and_called() {
        let mut session = Session::new();
        session.eval("(defparameter *square* (lambda (x) (* x x)))").unwrap();
        assert_eq!(session.eval("(funcall *square* 9)").unwrap(), "81");
    }

    #[test]
    fn closure_captures_outer_param() {
        // The classic "make-adder" pattern. Verifies closure capture
        // of a function parameter.
        let result = eval_str(
            "(defun make-adder (n) (lambda (x) (+ x n)))
             (funcall (make-adder 5) 10)",
        )
        .unwrap();
        assert_eq!(result, "15");
    }

    #[test]
    fn closure_captures_let_local() {
        let result = eval_str(
            "(let ((n 100))
               (funcall (lambda (x) (+ x n)) 5))",
        )
        .unwrap();
        assert_eq!(result, "105");
    }

    #[test]
    fn higher_order_compose() {
        let result = eval_str(
            "(defun compose (f g)
               (lambda (x) (funcall f (funcall g x))))
             (funcall (compose (lambda (x) (* x x))
                               (lambda (x) (+ x 1)))
                      4)",
        )
        .unwrap();
        // compose(square, succ)(4) = square(succ(4)) = square(5) = 25
        assert_eq!(result, "25");
    }

    #[test]
    fn map_list_with_lambda() {
        // The first higher-order list operation.
        let result = eval_str(
            "(defun map-list (f lst)
               (if (null lst)
                   nil
                   (cons (funcall f (car lst)) (map-list f (cdr lst)))))
             (map-list (lambda (x) (* x x)) '(1 2 3 4 5))",
        )
        .unwrap();
        assert_eq!(result, "(1 4 9 16 25)");
    }

    #[test]
    fn closure_captures_multiple_outer_vars() {
        let result = eval_str(
            "(defun make-affine (m b)
               (lambda (x) (+ (* m x) b)))
             (funcall (make-affine 3 7) 10)",
        )
        .unwrap();
        // 3 * 10 + 7 = 37
        assert_eq!(result, "37");
    }

    #[test]
    fn nested_closures_inner_captures_outer_lambda_param() {
        // (lambda (x) (lambda (y) (+ x y))) — inner lambda captures
        // outer lambda's param x.
        let result = eval_str(
            "(funcall (funcall (lambda (x) (lambda (y) (+ x y))) 10) 5)",
        )
        .unwrap();
        assert_eq!(result, "15");
    }

    #[test]
    fn closure_used_recursively_via_caller() {
        // A function that takes a function and applies it n times.
        let result = eval_str(
            "(defun apply-n (f x n)
               (if (eq n 0) x (apply-n f (funcall f x) (- n 1))))
             (apply-n (lambda (x) (* x 2)) 1 10)",
        )
        .unwrap();
        // 1 * 2^10 = 1024
        assert_eq!(result, "1024");
    }

    // -- #' (function) — first-class defun'd function values ------------

    #[test]
    fn function_quote_loads_defun_function() {
        let mut session = Session::new();
        session.eval("(defun square (x) (* x x))").unwrap();
        // #'square reads the function cell.
        // Use funcall to invoke it.
        assert_eq!(session.eval("(funcall #'square 9)").unwrap(), "81");
    }

    #[test]
    fn function_quote_long_form() {
        let mut session = Session::new();
        session.eval("(defun cube (x) (* x x x))").unwrap();
        assert_eq!(session.eval("(funcall (function cube) 3)").unwrap(), "27");
    }

    #[test]
    fn map_list_with_defun_via_function_quote() {
        let result = eval_str(
            "(defun square (x) (* x x))
             (defun map-list (f lst)
               (if (null lst) nil
                   (cons (funcall f (car lst)) (map-list f (cdr lst)))))
             (map-list #'square '(1 2 3 4 5))",
        )
        .unwrap();
        assert_eq!(result, "(1 4 9 16 25)");
    }

    #[test]
    fn compose_defun_with_lambda() {
        let result = eval_str(
            "(defun double (x) (+ x x))
             (defun succ (x) (+ x 1))
             (defun compose (f g)
               (lambda (x) (funcall f (funcall g x))))
             (funcall (compose #'double #'succ) 5)",
        )
        .unwrap();
        // compose(double, succ)(5) = double(succ(5)) = double(6) = 12
        assert_eq!(result, "12");
    }

    #[test]
    fn function_quote_redefinition_visibility() {
        let mut session = Session::new();
        session.eval("(defun id (x) x)").unwrap();
        // #'id loads at the time of evaluation. Storing it now
        // captures the CURRENT id; later redefinition is NOT seen
        // by an already-captured #' value.
        session.eval("(defparameter *f* #'id)").unwrap();
        session.eval("(defun id (x) (+ x 100))").unwrap();
        // *f* still refers to the old id (its function-cell value
        // at the time of #'id).
        // Wait — actually no. #'id returns the Function-tagged
        // Word, which IS in the symbol's function cell. The cell
        // got overwritten by the redefinition. The old Function
        // object is now garbage.
        // Hmm, actually the previous Function is in static (never
        // collected). The symbol's cell now points at the new
        // Function. *f* still points at the OLD Function. So
        // (funcall *f* 5) calls the old id which returns 5.
        assert_eq!(session.eval("(funcall *f* 5)").unwrap(), "5");
        // (id 5) goes through the symbol cell — calls the new id.
        assert_eq!(session.eval("(id 5)").unwrap(), "105");
    }

    #[test]
    fn closure_filter_with_predicate() {
        let result = eval_str(
            "(defun filter (pred lst)
               (cond ((null lst) nil)
                     ((funcall pred (car lst))
                      (cons (car lst) (filter pred (cdr lst))))
                     (t (filter pred (cdr lst)))))
             (filter (lambda (x) (> x 3)) '(1 5 2 6 3 7))",
        )
        .unwrap();
        assert_eq!(result, "(5 6 7)");
    }

    // -- Strings -----------------------------------------------------------

    #[test]
    fn ascii_string_round_trip() {
        assert_eq!(eval_str(r#""hello""#).unwrap(), r#""hello""#);
        assert_eq!(eval_str(r#""""#).unwrap(), r#""""#);
        assert_eq!(eval_str(r#""a""#).unwrap(), r#""a""#);
    }

    #[test]
    fn unicode_string_round_trip() {
        // Codepoints preserved end-to-end.
        assert_eq!(eval_str(r#""café""#).unwrap(), r#""café""#);
        assert_eq!(eval_str(r#""日本""#).unwrap(), r#""日本""#);
        assert_eq!(eval_str(r#""🦀""#).unwrap(), r#""🦀""#);
    }

    #[test]
    fn string_length_in_codepoints() {
        assert_eq!(eval_str(r#"(length "hello")"#).unwrap(), "5");
        assert_eq!(eval_str(r#"(length "")"#).unwrap(), "0");
        // Codepoints, NOT bytes — 日本 is 2 codepoints (not 6 UTF-8 bytes).
        assert_eq!(eval_str(r#"(length "日本")"#).unwrap(), "2");
        // 🦀 is 1 codepoint (U+1F980, outside BMP).
        assert_eq!(eval_str(r#"(length "🦀")"#).unwrap(), "1");
    }

    #[test]
    fn length_polymorphic_on_lists() {
        // (length '(a b c)) — 3 cons cells.
        assert_eq!(eval_str(r#"(length '(a b c))"#).unwrap(), "3");
        assert_eq!(eval_str(r#"(length nil)"#).unwrap(), "0");
        assert_eq!(eval_str(r#"(length '(1))"#).unwrap(), "1");
    }

    #[test]
    fn string_eq_works() {
        assert_eq!(eval_str(r#"(string= "foo" "foo")"#).unwrap(), "T");
        assert_eq!(eval_str(r#"(string= "foo" "bar")"#).unwrap(), "nil");
        assert_eq!(eval_str(r#"(string= "" "")"#).unwrap(), "T");
        assert_eq!(eval_str(r#"(string= "café" "café")"#).unwrap(), "T");
    }

    #[test]
    fn char_aref_on_string() {
        // (char s i) reads the i-th codepoint as a character.
        assert_eq!(eval_str(r#"(char "hello" 0)"#).unwrap(), "#\\h");
        assert_eq!(eval_str(r#"(char "hello" 4)"#).unwrap(), "#\\o");
        // aref is the same for strings.
        assert_eq!(eval_str(r#"(aref "hello" 1)"#).unwrap(), "#\\e");
        // Unicode: 4-byte codepoints work.
        assert_eq!(eval_str(r#"(char "café" 3)"#).unwrap(), "#\\é");
        assert_eq!(eval_str(r#"(char "🦀x" 0)"#).unwrap(), "#\\🦀");
    }

    #[test]
    fn string_in_list_prints_correctly() {
        // Strings in proper lists print as elements.
        assert_eq!(eval_str(r#"(list "a" "b" "c")"#).unwrap(), r#"("a" "b" "c")"#);
    }

    #[test]
    fn quoted_string_literal() {
        // '"hello" reads as (quote "hello") and evaluates to "hello".
        assert_eq!(eval_str(r#"'"hello""#).unwrap(), r#""hello""#);
    }

    #[test]
    fn quoted_list_with_strings() {
        assert_eq!(
            eval_str(r#"'("hello" "world")"#).unwrap(),
            r#"("hello" "world")"#,
        );
    }

    #[test]
    fn defparameter_holds_string() {
        let mut session = Session::new();
        session.eval(r#"(defparameter *greeting* "hello")"#).unwrap();
        assert_eq!(session.eval("*greeting*").unwrap(), r#""hello""#);
        assert_eq!(
            session.eval(r#"(string= *greeting* "hello")"#).unwrap(),
            "T",
        );
    }

    #[test]
    fn string_with_escapes_round_trips() {
        // "she said \"hi\"" — the inner quotes need escaping in the
        // printed form too.
        assert_eq!(
            eval_str(r#""she said \"hi\"""#).unwrap(),
            r#""she said \"hi\"""#,
        );
        // Backslash escapes itself.
        assert_eq!(eval_str(r#""back\\slash""#).unwrap(), r#""back\\slash""#);
    }

    #[test]
    fn strings_are_not_eq_even_when_equal() {
        // Each "foo" literal allocates fresh static storage; two
        // distinct strings with the same content are NOT eq.
        assert_eq!(eval_str(r#"(eq "foo" "foo")"#).unwrap(), "nil");
        // string= is the right predicate for content equality.
        assert_eq!(eval_str(r#"(string= "foo" "foo")"#).unwrap(), "T");
    }

    #[test]
    fn function_can_take_string_arg() {
        let mut session = Session::new();
        session
            .eval(r#"(defun greet (name) (string= name "alice"))"#)
            .unwrap();
        assert_eq!(session.eval(r#"(greet "alice")"#).unwrap(), "T");
        assert_eq!(session.eval(r#"(greet "bob")"#).unwrap(), "nil");
    }

    // -- equal: recursive structural equality ------------------------------

    #[test]
    fn equal_on_fixnums() {
        // Same as eq for fixnums.
        assert_eq!(eval_str("(equal 1 1)").unwrap(), "T");
        assert_eq!(eval_str("(equal 1 2)").unwrap(), "nil");
        assert_eq!(eval_str("(equal 0 0)").unwrap(), "T");
    }

    #[test]
    fn equal_on_nil_and_t() {
        assert_eq!(eval_str("(equal nil nil)").unwrap(), "T");
        assert_eq!(eval_str("(equal t t)").unwrap(), "T");
        assert_eq!(eval_str("(equal nil t)").unwrap(), "nil");
    }

    #[test]
    fn equal_on_symbols() {
        assert_eq!(eval_str("(equal 'foo 'foo)").unwrap(), "T");
        assert_eq!(eval_str("(equal 'foo 'bar)").unwrap(), "nil");
    }

    #[test]
    fn equal_on_lists() {
        // equal recurses through cons cells where eq would not.
        assert_eq!(eval_str("(equal '(1 2 3) '(1 2 3))").unwrap(), "T");
        assert_eq!(eval_str("(equal '(1 2 3) '(1 2 4))").unwrap(), "nil");
        assert_eq!(eval_str("(equal '(1 2) '(1 2 3))").unwrap(), "nil");
        // Two distinct list literals — eq says no, equal says yes.
        assert_eq!(eval_str("(eq '(1 2 3) '(1 2 3))").unwrap(), "nil");
    }

    #[test]
    fn equal_on_nested_lists() {
        assert_eq!(
            eval_str("(equal '(1 (2 3)) '(1 (2 3)))").unwrap(),
            "T",
        );
        assert_eq!(
            eval_str("(equal '(1 (2 3)) '(1 (2 4)))").unwrap(),
            "nil",
        );
        assert_eq!(
            eval_str("(equal '((a b) (c d)) '((a b) (c d)))").unwrap(),
            "T",
        );
    }

    #[test]
    fn equal_on_strings() {
        // equal compares strings by content (like string=).
        assert_eq!(eval_str(r#"(equal "foo" "foo")"#).unwrap(), "T");
        assert_eq!(eval_str(r#"(equal "foo" "bar")"#).unwrap(), "nil");
        assert_eq!(eval_str(r#"(equal "" "")"#).unwrap(), "T");
        assert_eq!(eval_str(r#"(equal "café" "café")"#).unwrap(), "T");
    }

    #[test]
    fn equal_mixed_types() {
        // Different types are never equal.
        assert_eq!(eval_str(r#"(equal 1 "1")"#).unwrap(), "nil");
        assert_eq!(eval_str(r#"(equal '(1) 1)"#).unwrap(), "nil");
        assert_eq!(eval_str(r#"(equal nil 0)"#).unwrap(), "nil");
        assert_eq!(eval_str(r#"(equal 'foo "foo")"#).unwrap(), "nil");
    }

    #[test]
    fn equal_lists_of_strings() {
        assert_eq!(
            eval_str(r#"(equal '("a" "b") '("a" "b"))"#).unwrap(),
            "T",
        );
        assert_eq!(
            eval_str(r#"(equal '("a" "b") '("a" "c"))"#).unwrap(),
            "nil",
        );
    }

    #[test]
    fn equal_in_function_body() {
        let mut session = Session::new();
        session
            .eval("(defun same (a b) (equal a b))")
            .unwrap();
        assert_eq!(
            session.eval("(same '(1 2 3) '(1 2 3))").unwrap(),
            "T",
        );
        assert_eq!(
            session.eval(r#"(same "hi" "hi")"#).unwrap(),
            "T",
        );
        assert_eq!(
            session.eval("(same '(1 2) '(1 3))").unwrap(),
            "nil",
        );
    }

    // -- setf: generalised assignment --------------------------------------

    #[test]
    fn setf_on_symbol_acts_like_setq() {
        let mut session = Session::new();
        session.eval("(defparameter *x* 0)").unwrap();
        session.eval("(setf *x* 42)").unwrap();
        assert_eq!(session.eval("*x*").unwrap(), "42");
    }

    #[test]
    fn setf_on_symbol_returns_value() {
        let mut session = Session::new();
        session.eval("(defparameter *x* 0)").unwrap();
        // setf evaluates to the assigned value, like setq.
        assert_eq!(session.eval("(setf *x* 99)").unwrap(), "99");
    }

    #[test]
    fn setf_car_mutates_cons() {
        let mut session = Session::new();
        session.eval("(defparameter *p* (cons 1 2))").unwrap();
        session.eval("(setf (car *p*) 99)").unwrap();
        assert_eq!(session.eval("(car *p*)").unwrap(), "99");
        // cdr unchanged.
        assert_eq!(session.eval("(cdr *p*)").unwrap(), "2");
    }

    #[test]
    fn setf_cdr_mutates_cons() {
        let mut session = Session::new();
        session.eval("(defparameter *p* (cons 1 2))").unwrap();
        session.eval("(setf (cdr *p*) 99)").unwrap();
        assert_eq!(session.eval("(car *p*)").unwrap(), "1");
        assert_eq!(session.eval("(cdr *p*)").unwrap(), "99");
    }

    #[test]
    fn setf_first_and_rest_aliases() {
        let mut session = Session::new();
        session.eval("(defparameter *p* (cons 1 2))").unwrap();
        session.eval("(setf (first *p*) 10)").unwrap();
        session.eval("(setf (rest *p*) 20)").unwrap();
        assert_eq!(session.eval("(car *p*)").unwrap(), "10");
        assert_eq!(session.eval("(cdr *p*)").unwrap(), "20");
    }

    #[test]
    fn setf_returns_new_value_for_cons() {
        // CL: setf returns the value, not the modified container.
        let mut session = Session::new();
        session.eval("(defparameter *p* (cons 1 2))").unwrap();
        assert_eq!(session.eval("(setf (car *p*) 7)").unwrap(), "7");
        assert_eq!(session.eval("(setf (cdr *p*) 8)").unwrap(), "8");
    }

    #[test]
    fn setf_car_on_nested_list() {
        let mut session = Session::new();
        session
            .eval("(defparameter *p* (cons 1 (cons 2 (cons 3 nil))))")
            .unwrap();
        // Mutate the second cell's car.
        session.eval("(setf (car (cdr *p*)) 99)").unwrap();
        assert_eq!(session.eval("*p*").unwrap(), "(1 99 3)");
    }

    #[test]
    fn setf_cdr_can_create_dotted_pair() {
        let mut session = Session::new();
        session.eval("(defparameter *p* (cons 1 nil))").unwrap();
        session.eval("(setf (cdr *p*) 2)").unwrap();
        assert_eq!(session.eval("*p*").unwrap(), "(1 . 2)");
    }

    #[test]
    fn setf_aref_mutates_string() {
        let mut session = Session::new();
        session.eval(r#"(defparameter *s* "hello")"#).unwrap();
        session.eval(r#"(setf (aref *s* 0) #\H)"#).unwrap();
        assert_eq!(session.eval("*s*").unwrap(), r#""Hello""#);
    }

    #[test]
    fn setf_char_mutates_string() {
        let mut session = Session::new();
        session.eval(r#"(defparameter *s* "world")"#).unwrap();
        session.eval(r#"(setf (char *s* 4) #\!)"#).unwrap();
        assert_eq!(session.eval("*s*").unwrap(), r#""worl!""#);
    }

    #[test]
    fn setf_string_returns_char() {
        let mut session = Session::new();
        session.eval(r#"(defparameter *s* "abc")"#).unwrap();
        assert_eq!(
            session.eval(r#"(setf (aref *s* 1) #\X)"#).unwrap(),
            r#"#\X"#,
        );
    }

    #[test]
    fn setf_string_unicode() {
        // Set a Unicode codepoint in a string.
        let mut session = Session::new();
        session.eval(r#"(defparameter *s* "cafe")"#).unwrap();
        session.eval(r#"(setf (aref *s* 3) #\é)"#).unwrap();
        assert_eq!(session.eval("*s*").unwrap(), r#""café""#);
    }

    #[test]
    fn setf_in_function_body() {
        let mut session = Session::new();
        session
            .eval("(defun set-head (p v) (setf (car p) v))")
            .unwrap();
        session.eval("(defparameter *q* (cons 0 0))").unwrap();
        session.eval("(set-head *q* 42)").unwrap();
        assert_eq!(session.eval("(car *q*)").unwrap(), "42");
    }

    #[test]
    fn setf_unsupported_place_errors() {
        // (setf 5 6) — not a place.
        let r = eval_str("(setf 5 6)");
        assert!(r.is_err(), "expected error for non-place setf, got {r:?}");
    }

    #[test]
    fn setf_unknown_form_falls_back_to_setf_mangled_call() {
        // (setf (FOO arg…) val) rewrites to a call to %SETF-FOO
        // when no built-in setf-expander matches the head. This is
        // the convention defstruct uses to auto-install slot
        // setters: each slot accessor X compiles `(setf (X obj)
        // val)` into `(%setf-X val obj)`, and the user (or the
        // defstruct expansion) defines `%setf-X` as an ordinary
        // function.
        //
        // The form compiles successfully — it's NOT a compile
        // error. We verify that by catching the runtime condition
        // (`%SETF-FOO` is unbound here) inside handler-case. If
        // the form failed to compile, eval would return an Err
        // and the handler-case would never run; if the runtime
        // error escaped the handler-case, the process would
        // abort.
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        let result = s.eval(
            "(handler-case
               (let ((x 0))
                 (setf (foo x) 1))
               (error (c) (list :caught)))",
        );
        assert_eq!(result.unwrap(), "(:CAUGHT)");
    }

    // -- Mutable lexical bindings ------------------------------------------

    #[test]
    fn setq_local_let_binding() {
        // (let ((x 0)) (setq x 7) x) — the simplest case.
        assert_eq!(eval_str("(let ((x 0)) (setq x 7) x)").unwrap(), "7");
    }

    #[test]
    fn setf_local_let_binding() {
        assert_eq!(eval_str("(let ((x 1)) (setf x 99) x)").unwrap(), "99");
    }

    #[test]
    fn mutated_let_binding_starts_at_init() {
        // The init value is observable before mutation.
        assert_eq!(
            eval_str("(let ((x 10)) (let ((y x)) (setq x 99) y))").unwrap(),
            "10",
        );
    }

    #[test]
    fn mutated_let_in_function() {
        let mut session = Session::new();
        session
            .eval(
                "(defun count-to (n) \
                   (let ((i 0)) \
                     (if (= i n) i \
                       (progn (setq i n) i))))",
            )
            .unwrap();
        assert_eq!(session.eval("(count-to 5)").unwrap(), "5");
    }

    #[test]
    fn nested_let_shadows_outer_mutation() {
        // Inner setq targets inner x; outer x is untouched.
        assert_eq!(
            eval_str(
                "(let ((x 1)) \
                   (let ((x 100)) (setq x 200)) \
                   x)"
            )
            .unwrap(),
            "1",
        );
    }

    #[test]
    fn nested_let_can_mutate_outer() {
        // Inner scope binds y but not x; setq targets outer x.
        assert_eq!(
            eval_str(
                "(let ((x 1)) \
                   (let ((y 2)) (setq x 99)) \
                   x)"
            )
            .unwrap(),
            "99",
        );
    }

    #[test]
    fn closure_captures_and_mutates() {
        // The make-counter pattern: lambda captures a let-binding
        // and mutates it. Each call increments and returns the new
        // value.
        let mut session = Session::new();
        session
            .eval(
                "(defun make-counter () \
                   (let ((n 0)) \
                     (lambda () (setf n (+ n 1)) n)))",
            )
            .unwrap();
        session.eval("(defparameter *c* (make-counter))").unwrap();
        assert_eq!(session.eval("(funcall *c*)").unwrap(), "1");
        assert_eq!(session.eval("(funcall *c*)").unwrap(), "2");
        assert_eq!(session.eval("(funcall *c*)").unwrap(), "3");
    }

    #[test]
    fn each_counter_has_its_own_state() {
        // Two counters from the same factory share no state — each
        // gets its own boxed cell.
        let mut session = Session::new();
        session
            .eval(
                "(defun make-counter () \
                   (let ((n 0)) \
                     (lambda () (setf n (+ n 1)) n)))",
            )
            .unwrap();
        session.eval("(defparameter *c1* (make-counter))").unwrap();
        session.eval("(defparameter *c2* (make-counter))").unwrap();
        assert_eq!(session.eval("(funcall *c1*)").unwrap(), "1");
        assert_eq!(session.eval("(funcall *c1*)").unwrap(), "2");
        assert_eq!(session.eval("(funcall *c2*)").unwrap(), "1");
        assert_eq!(session.eval("(funcall *c1*)").unwrap(), "3");
    }

    #[test]
    fn closure_reads_outer_mutations() {
        // The outer scope mutates n; the captured lambda sees the
        // new value.
        let mut session = Session::new();
        session
            .eval(
                "(defun make-pair () \
                   (let ((n 0)) \
                     (cons (lambda () n) \
                           (lambda (v) (setf n v)))))",
            )
            .unwrap();
        session.eval("(defparameter *p* (make-pair))").unwrap();
        assert_eq!(session.eval("(funcall (car *p*))").unwrap(), "0");
        session.eval("(funcall (cdr *p*) 42)").unwrap();
        assert_eq!(session.eval("(funcall (car *p*))").unwrap(), "42");
    }

    #[test]
    fn unmutated_let_still_unboxed() {
        // No setq in body — lowering takes the cheap path. We can't
        // assert "no cons allocated" from outside but the test
        // exercises the non-boxed path for coverage.
        assert_eq!(
            eval_str("(let ((a 1) (b 2)) (+ a b))").unwrap(),
            "3",
        );
    }

    #[test]
    fn setq_of_param_still_errors() {
        // Mutable function parameters aren't wired yet — boxing the
        // param at function entry is future work.
        let r = eval_str("((lambda (x) (setq x 1) x) 0)");
        assert!(matches!(
            r,
            Err(EvalError::Compile(CompileError::NotImplemented(_))),
        ));
    }

    #[test]
    fn setq_unbound_local_falls_through_to_global() {
        // No local x. setq targets global *g*. (Defparameter then setq.)
        let mut session = Session::new();
        session.eval("(defparameter *g* 0)").unwrap();
        session.eval("(setq *g* 5)").unwrap();
        assert_eq!(session.eval("*g*").unwrap(), "5");
    }

    // -- defparameter / setq / global value cells --------------------------

    #[test]
    fn defparameter_then_read() {
        let mut session = Session::new();
        session.eval("(defparameter *foo* 42)").unwrap();
        assert_eq!(session.eval("*foo*").unwrap(), "42");
    }

    #[test]
    fn defparameter_returns_symbol_name() {
        // CL semantics: defparameter / defvar return the symbol
        // being defined, NOT the assigned value. (setq returns the
        // value; the two forms differ on this point.) The symbol's
        // value cell is set as a side-effect.
        assert_eq!(eval_str("(defparameter *x* 99)").unwrap(), "*X*");
        assert_eq!(eval_str("(defparameter *x* 99) *x*").unwrap(), "99");
    }

    #[test]
    fn defparameter_overrides_existing() {
        let result = eval_str(
            "(defparameter *foo* 1)
             (defparameter *foo* 99)
             *foo*",
        )
        .unwrap();
        assert_eq!(result, "99");
    }

    #[test]
    fn setq_assigns() {
        let result = eval_str(
            "(defparameter *x* 1)
             (setq *x* 100)
             *x*",
        )
        .unwrap();
        assert_eq!(result, "100");
    }

    #[test]
    fn setq_returns_assigned_value() {
        let mut session = Session::new();
        session.eval("(defparameter *x* 0)").unwrap();
        assert_eq!(session.eval("(setq *x* 42)").unwrap(), "42");
    }

    #[test]
    fn function_can_modify_global_via_setq() {
        let result = eval_str(
            "(defparameter *counter* 0)
             (defun bump () (setq *counter* (+ *counter* 1)))
             (bump) (bump) (bump)
             *counter*",
        )
        .unwrap();
        assert_eq!(result, "3");
    }

    #[test]
    fn global_value_visible_inside_function() {
        let result = eval_str(
            "(defparameter *base* 10)
             (defun add-base (x) (+ x *base*))
             (add-base 7)",
        )
        .unwrap();
        assert_eq!(result, "17");
    }

    #[test]
    fn local_shadows_global() {
        let result = eval_str(
            "(defparameter *x* 100)
             (defun f (x) x)
             (f 7)",
        )
        .unwrap();
        assert_eq!(result, "7");
    }

    #[test]
    fn let_local_shadows_global() {
        let result = eval_str(
            "(defparameter *x* 100)
             (let ((*x* 5)) *x*)",
        )
        .unwrap();
        assert_eq!(result, "5");
    }

    #[test]
    fn setq_of_defun_param_promotes_to_cell() {
        // Mutable function parameters ARE supported for `defun`: a
        // `(setq param …)` in the body promotes the required param to
        // a boxed local cell (the mutable-parameter promotion in
        // `compile_function_raw`). Note the contrast with
        // `setq_of_param_still_errors`, which covers the *lambda*
        // path — that one is not wired for in-place param mutation yet.
        let r = eval_str(
            "(defun f (x) (setq x 99) x)
             (f 1)",
        )
        .unwrap();
        assert_eq!(r, "99");
    }

    #[test]
    fn quoted_symbol_with_setq() {
        // (setq 'foo 1) should be an error — first arg must be an
        // unquoted symbol literal. But our reader produces `'foo`
        // as `(quote foo)` which is a Cons, not a Symbol — so
        // setq's symbol-check fails with NotImplemented.
        let r = eval_str("(setq 'foo 1)");
        assert!(matches!(r, Err(EvalError::Compile(CompileError::NotImplemented(_)))));
    }

    // -- list, quoted symbols, quoted lists --------------------------------

    #[test]
    fn list_builds_proper_lists() {
        assert_eq!(eval_str("(list)").unwrap(), "nil");
        assert_eq!(eval_str("(list 1)").unwrap(), "(1)");
        assert_eq!(eval_str("(list 1 2 3)").unwrap(), "(1 2 3)");
        assert_eq!(
            eval_str("(list (+ 1 1) (* 3 4) (- 10 1))").unwrap(),
            "(2 12 9)",
        );
    }

    #[test]
    fn quoted_symbol_prints_as_name() {
        assert_eq!(eval_str("'foo").unwrap(), "FOO");
        assert_eq!(eval_str("(quote bar)").unwrap(), "BAR");
        // Case-folding: lowercase source becomes upper-case symbol.
        assert_eq!(eval_str("'Hello").unwrap(), "HELLO");
    }

    #[test]
    fn quoted_symbols_are_eq_when_same_name() {
        // Interning means two `'foo` references resolve to the same
        // Word — `eq` returns T.
        assert_eq!(eval_str("(eq 'foo 'foo)").unwrap(), "T");
        assert_eq!(eval_str("(eq 'foo 'bar)").unwrap(), "nil");
    }

    #[test]
    fn quoted_list_literal() {
        assert_eq!(eval_str("'(1 2 3)").unwrap(), "(1 2 3)");
        assert_eq!(eval_str("'(a b c)").unwrap(), "(A B C)");
        assert_eq!(eval_str("'(1 . 2)").unwrap(), "(1 . 2)");
        assert_eq!(eval_str("'((1 2) (3 4))").unwrap(), "((1 2) (3 4))");
    }

    #[test]
    fn quoted_list_with_mixed_atoms() {
        // Mix fixnums, symbols, nested lists.
        assert_eq!(
            eval_str("'(name 42 (a b) nil)").unwrap(),
            "(NAME 42 (A B) nil)",
        );
    }

    #[test]
    fn quoted_lists_share_static_storage() {
        // Two references to '(1 2 3) intern as the same symbol-
        // table entries, but each `quote` form allocates its own
        // cons chain (we don't yet share). They are distinct cons
        // cells, so eq is nil.
        assert_eq!(eval_str("(eq '(1 2) '(1 2))").unwrap(), "nil");
    }

    #[test]
    fn cond_with_quoted_symbol_branches() {
        let result = eval_str(
            "(defun classify (n)
               (cond ((< n 0) 'negative)
                     ((= n 0) 'zero)
                     (t 'positive)))
             (list (classify -3) (classify 0) (classify 5))",
        )
        .unwrap();
        assert_eq!(result, "(NEGATIVE ZERO POSITIVE)");
    }

    #[test]
    fn member_via_recursion() {
        // (defun member (x lst) ...)  classic CL pattern, manual
        // implementation since `member` isn't a builtin yet.
        let result = eval_str(
            "(defun my-member (x lst)
               (cond ((null lst) nil)
                     ((eq x (car lst)) lst)
                     (t (my-member x (cdr lst)))))
             (my-member 'b '(a b c d))",
        )
        .unwrap();
        // Returns the tail starting with the match.
        assert_eq!(result, "(B C D)");
    }

    // -- Numeric comparisons -----------------------------------------------

    #[test]
    fn lt_works() {
        assert_eq!(eval_str("(< 1 2)").unwrap(), "T");
        assert_eq!(eval_str("(< 2 1)").unwrap(), "nil");
        assert_eq!(eval_str("(< 1 1)").unwrap(), "nil");
        assert_eq!(eval_str("(< -5 0)").unwrap(), "T");
    }

    #[test]
    fn gt_works() {
        assert_eq!(eval_str("(> 2 1)").unwrap(), "T");
        assert_eq!(eval_str("(> 1 2)").unwrap(), "nil");
        assert_eq!(eval_str("(> 1 1)").unwrap(), "nil");
    }

    #[test]
    fn le_ge_eq_work() {
        assert_eq!(eval_str("(<= 1 1)").unwrap(), "T");
        assert_eq!(eval_str("(<= 1 2)").unwrap(), "T");
        assert_eq!(eval_str("(<= 2 1)").unwrap(), "nil");
        assert_eq!(eval_str("(>= 1 1)").unwrap(), "T");
        assert_eq!(eval_str("(>= 2 1)").unwrap(), "T");
        assert_eq!(eval_str("(= 1 1)").unwrap(), "T");
        assert_eq!(eval_str("(= 1 2)").unwrap(), "nil");
    }

    #[test]
    fn fibonacci_via_recursion() {
        let result = eval_str(
            "(defun fib (n)
               (if (< n 2)
                   n
                   (+ (fib (- n 1)) (fib (- n 2)))))
             (fib 10)",
        )
        .unwrap();
        assert_eq!(result, "55"); // fib(10)
    }

    #[test]
    fn fibonacci_15() {
        let result = eval_str(
            "(defun fib (n)
               (if (< n 2)
                   n
                   (+ (fib (- n 1)) (fib (- n 2)))))
             (fib 15)",
        )
        .unwrap();
        assert_eq!(result, "610"); // fib(15)
    }

    // -- Type predicates ---------------------------------------------------

    #[test]
    fn null_predicate() {
        assert_eq!(eval_str("(null nil)").unwrap(), "T");
        assert_eq!(eval_str("(null 0)").unwrap(), "nil");
        assert_eq!(eval_str("(null (cons 1 2))").unwrap(), "nil");
        assert_eq!(eval_str("(null t)").unwrap(), "nil");
    }

    #[test]
    fn consp_predicate() {
        assert_eq!(eval_str("(consp (cons 1 2))").unwrap(), "T");
        assert_eq!(eval_str("(consp nil)").unwrap(), "nil");
        assert_eq!(eval_str("(consp 0)").unwrap(), "nil");
        assert_eq!(eval_str("(consp t)").unwrap(), "nil");
    }

    #[test]
    fn atom_predicate() {
        assert_eq!(eval_str("(atom nil)").unwrap(), "T");
        assert_eq!(eval_str("(atom 42)").unwrap(), "T");
        assert_eq!(eval_str("(atom t)").unwrap(), "T");
        assert_eq!(eval_str("(atom (cons 1 2))").unwrap(), "nil");
    }

    #[test]
    fn listp_predicate() {
        assert_eq!(eval_str("(listp nil)").unwrap(), "T");
        assert_eq!(eval_str("(listp (cons 1 2))").unwrap(), "T");
        assert_eq!(eval_str("(listp 42)").unwrap(), "nil");
        assert_eq!(eval_str("(listp t)").unwrap(), "nil");
    }

    // -- not / and / or / cond ---------------------------------------------

    #[test]
    fn not_inverts_truthy_and_nil() {
        assert_eq!(eval_str("(not nil)").unwrap(), "T");
        assert_eq!(eval_str("(not t)").unwrap(), "nil");
        assert_eq!(eval_str("(not 0)").unwrap(), "nil"); // 0 is truthy in CL
        assert_eq!(eval_str("(not (cons 1 2))").unwrap(), "nil");
    }

    #[test]
    fn and_returns_last_or_nil() {
        // CL: (and) → t
        assert_eq!(eval_str("(and)").unwrap(), "T");
        // (and x) → x
        assert_eq!(eval_str("(and 5)").unwrap(), "5");
        // (and a b) → b if a is non-nil
        assert_eq!(eval_str("(and t 7)").unwrap(), "7");
        // (and a b) → nil if a is nil
        assert_eq!(eval_str("(and nil 7)").unwrap(), "nil");
        // Multi-arg
        assert_eq!(eval_str("(and 1 2 3 4 5)").unwrap(), "5");
        assert_eq!(eval_str("(and 1 2 nil 4 5)").unwrap(), "nil");
    }

    #[test]
    fn or_returns_first_non_nil_or_nil() {
        assert_eq!(eval_str("(or)").unwrap(), "nil");
        assert_eq!(eval_str("(or 5)").unwrap(), "5");
        assert_eq!(eval_str("(or nil 7)").unwrap(), "7");
        assert_eq!(eval_str("(or 7 nil)").unwrap(), "7");
        assert_eq!(eval_str("(or nil nil 9)").unwrap(), "9");
        assert_eq!(eval_str("(or nil nil nil)").unwrap(), "nil");
    }

    #[test]
    fn or_short_circuits_on_first_truthy() {
        // First non-nil wins. (- 5 5) is fixnum 0, which is TRUTHY
        // in CL — only nil is false — so this returns 0, not the
        // cons. Tests that 0 is truthy AND that or short-circuits.
        assert_eq!(eval_str("(or (- 5 5) (cons 1 2))").unwrap(), "0");
        // With a real nil first, the cons is reached.
        assert_eq!(eval_str("(or nil (cons 1 2))").unwrap(), "(1 . 2)");
    }

    #[test]
    fn cond_picks_first_matching_clause() {
        assert_eq!(eval_str("(cond (t 1))").unwrap(), "1");
        assert_eq!(eval_str("(cond (nil 1) (t 2))").unwrap(), "2");
        assert_eq!(eval_str("(cond (nil 1) (nil 2) (t 3))").unwrap(), "3");
        assert_eq!(eval_str("(cond ((eq 1 2) 10) ((eq 1 1) 20))").unwrap(), "20");
    }

    #[test]
    fn cond_with_no_match_returns_nil() {
        assert_eq!(eval_str("(cond (nil 1) (nil 2))").unwrap(), "nil");
    }

    #[test]
    fn cond_implicit_progn_in_clause() {
        // Multi-form body in a clause uses implicit progn.
        assert_eq!(eval_str("(cond (t 1 2 3))").unwrap(), "3");
    }

    #[test]
    fn boolean_combinations() {
        // (and (or nil 5) (not nil) 7) → 7
        assert_eq!(eval_str("(and (or nil 5) (not nil) 7)").unwrap(), "7");
        // (or (and t nil) 99) → 99
        assert_eq!(eval_str("(or (and t nil) 99)").unwrap(), "99");
    }

    #[test]
    fn cond_with_recursion() {
        // Classic FizzBuzz-style multi-branch. Just two branches
        // for now since we don't have mod yet — return "low",
        // "mid", "high" via fixnums 1/2/3.
        let result = eval_str(
            "(defun classify (n)
               (cond ((< n 0) -1)
                     ((= n 0) 0)
                     ((< n 10) 1)
                     ((< n 100) 2)
                     (t 3)))
             (cons (classify -5)
                   (cons (classify 0)
                         (cons (classify 7)
                               (cons (classify 42)
                                     (cons (classify 1000) nil)))))",
        )
        .unwrap();
        assert_eq!(result, "(-1 0 1 2 3)");
    }

    #[test]
    fn list_traversal_via_recursion() {
        // Compute the length of a proper list using car/cdr/null.
        let result = eval_str(
            "(defun length (lst)
               (if (null lst)
                   0
                   (+ 1 (length (cdr lst)))))
             (length (cons 1 (cons 2 (cons 3 (cons 4 nil)))))",
        )
        .unwrap();
        assert_eq!(result, "4");
    }

    #[test]
    fn list_sum_via_recursion() {
        let result = eval_str(
            "(defun sum-list (lst)
               (if (null lst)
                   0
                   (+ (car lst) (sum-list (cdr lst)))))
             (sum-list (cons 1 (cons 2 (cons 3 (cons 4 (cons 5 nil))))))",
        )
        .unwrap();
        assert_eq!(result, "15");
    }

    #[test]
    fn let_with_recursive_call_in_body() {
        // (defun fact-via-let (n)
        //   (let ((sub (- n 1)))
        //     (if (eq n 0) 1 (* n (fact-via-let sub)))))
        let result = eval_str(
            "(defun fact-via-let (n)
               (let ((sub (- n 1)))
                 (if (eq n 0) 1 (* n (fact-via-let sub)))))
             (fact-via-let 6)",
        )
        .unwrap();
        assert_eq!(result, "720");
    }

    // -- &rest / variadic functions ---------------------------------------

    #[test]
    fn rest_no_extra_args() {
        let mut s = Session::new();
        s.eval("(defun f (a &rest r) r)").unwrap();
        // Called with exactly the required count — rest is nil.
        assert_eq!(s.eval("(f 1)").unwrap(), "nil");
    }

    #[test]
    fn rest_collects_extra_args_in_order() {
        let mut s = Session::new();
        s.eval("(defun f (a &rest r) r)").unwrap();
        assert_eq!(s.eval("(f 1 2 3 4)").unwrap(), "(2 3 4)");
        assert_eq!(s.eval("(f 1 99)").unwrap(), "(99)");
    }

    #[test]
    fn rest_only() {
        // (defun f (&rest r) r) — all args go into r.
        let mut s = Session::new();
        s.eval("(defun all (&rest r) r)").unwrap();
        assert_eq!(s.eval("(all)").unwrap(), "nil");
        assert_eq!(s.eval("(all 1 2 3)").unwrap(), "(1 2 3)");
        assert_eq!(s.eval(r#"(all 'a "b" 3)"#).unwrap(), r#"(A "b" 3)"#);
    }

    #[test]
    fn rest_with_multiple_required() {
        let mut s = Session::new();
        s.eval("(defun f (a b c &rest r) (cons (+ a b c) r))").unwrap();
        assert_eq!(s.eval("(f 1 2 3)").unwrap(), "(6)");
        assert_eq!(s.eval("(f 1 2 3 4 5)").unwrap(), "(6 4 5)");
    }

    #[test]
    fn rest_lets_us_walk_args() {
        // Without apply we can't recursively call the variadic
        // function with fewer args, but we CAN walk the rest list
        // with a separate helper. This is the common shape of
        // "variadic frontend, list-walking backend."
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun sum-list (lst) \
               (if (null lst) 0 \
                 (+ (car lst) (sum-list (cdr lst)))))",
        )
        .unwrap();
        s.eval("(defun sum (&rest r) (sum-list r))").unwrap();
        assert_eq!(s.eval("(sum)").unwrap(), "0");
        assert_eq!(s.eval("(sum 1)").unwrap(), "1");
        assert_eq!(s.eval("(sum 1 2 3 4 5)").unwrap(), "15");
    }

    #[test]
    fn rest_in_lambda() {
        let mut s = Session::new();
        s.eval("(defparameter *f* (lambda (a &rest r) (cons a r)))").unwrap();
        assert_eq!(s.eval("(funcall *f* 1)").unwrap(), "(1)");
        assert_eq!(s.eval("(funcall *f* 1 2 3)").unwrap(), "(1 2 3)");
    }

    #[test]
    fn rest_can_be_mutated() {
        // setq of the rest binding works because the analysis
        // detects mutation and boxes it like any let-local.
        let mut s = Session::new();
        // Variadic length implemented by walking the rest list
        // destructively (just to exercise mutation, not because
        // it's a good idea — `length` already does this).
        s.eval(
            "(defun my-count (&rest r) \
               (let ((n 0)) \
                 (if (null r) n \
                   (progn (setq r (cdr r)) \
                          (setq n (+ n 1)) \
                          (if (null r) n \
                            (progn (setq r (cdr r)) (setq n (+ n 1))))))))",
        )
        .unwrap();
        assert_eq!(s.eval("(my-count)").unwrap(), "0");
        assert_eq!(s.eval("(my-count 'a)").unwrap(), "1");
        assert_eq!(s.eval("(my-count 'a 'b 'c)").unwrap(), "2");
    }

    #[test]
    fn rest_closes_over_correctly() {
        // Lambda with &rest, captured by another lambda.
        let mut s = Session::new();
        s.eval(
            "(defun make-collector () \
               (let ((items nil)) \
                 (lambda (&rest new-items) \
                   (setf items (append items new-items)) \
                   items)))",
        )
        .unwrap();
        // append is binary in our stdlib, so this needs stdlib.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun make-collector () \
               (let ((items nil)) \
                 (lambda (&rest new-items) \
                   (setf items (append items new-items)) \
                   items)))",
        )
        .unwrap();
        s.eval("(defparameter *c* (make-collector))").unwrap();
        assert_eq!(s.eval("(funcall *c* 1 2)").unwrap(), "(1 2)");
        assert_eq!(s.eval("(funcall *c* 3)").unwrap(), "(1 2 3)");
        assert_eq!(s.eval("(funcall *c*)").unwrap(), "(1 2 3)");
    }

    #[test]
    fn rest_malformed_errors() {
        // &rest with no name following.
        let r = eval_str("(defun f (a &rest) a)");
        assert!(r.is_err());
        // &rest followed by multiple names.
        let r = eval_str("(defun f (a &rest r s) a)");
        assert!(r.is_err());
    }

    // -- loop / return / getf --------------------------------------------

    #[test]
    fn loop_with_return_exits() {
        // (loop (return 42)) — first iteration returns.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(loop (return 42))").unwrap(), "42");
        // (return) with no value yields nil.
        assert_eq!(s.eval("(loop (return))").unwrap(), "nil");
    }

    #[test]
    fn loop_counts_via_mutable_local() {
        // Classic "count to 5 then exit." Uses a mutable let-local
        // for the counter, terminal return in a cond clause.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(
                "(let ((i 0))
                   (loop
                     (cond
                       ((= i 5) (return i))
                       (t (setq i (+ i 1))))))",
            )
            .unwrap(),
            "5",
        );
    }

    #[test]
    fn loop_accumulates() {
        // Sum 1+2+...+10 via a counter and accumulator.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(
                "(let ((i 0) (sum 0))
                   (loop
                     (cond
                       ((> i 10) (return sum))
                       (t (setq sum (+ sum i))
                          (setq i (+ i 1))))))",
            )
            .unwrap(),
            "55",
        );
    }

    #[test]
    fn self_tail_call_does_not_overflow_stack() {
        // Self-tail-call elimination: a function that tail-calls itself
        // must iterate (reuse its frame), not grow the native stack.
        // 5,000,000 deep would overflow any reasonable stack without
        // TCO; with it, the loop just runs. Guards the TailLoop /
        // SelfTailNext lowering in ncl-llvm.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun %tco-count (n acc)
               (if (= n 0) acc (%tco-count (- n 1) (+ acc 1))))",
        )
        .unwrap();
        assert_eq!(s.eval("(%tco-count 5000000 0)").unwrap(), "5000000");
    }

    #[test]
    fn self_tail_call_preserves_accumulated_heap_data() {
        // TCO must keep its accumulator rooted across the loop's
        // back-edge: building a long list by tail recursion triggers
        // many minor GCs, and the in-progress list (the `acc` param)
        // must survive every one. A broken root would lose conses and
        // the length would come back short (or the process would
        // crash). 200k conses is plenty of GC pressure for the test
        // heap while staying quick in debug.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun %tco-build (n acc)
               (if (= n 0) acc (%tco-build (- n 1) (cons n acc))))",
        )
        .unwrap();
        assert_eq!(
            s.eval("(length (%tco-build 200000 nil))").unwrap(),
            "200000",
        );
    }

    #[test]
    fn self_tail_call_nested_if() {
        // A self-tail-call in the else arm of a nested `if` exercises
        // the one-arm-diverts case in the If lowering (the else arm
        // branches back to the loop header, so only the then arm
        // reaches the merge). Two base cases, recursion stepping by 2.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun %tco-parity (n)
               (if (= n 0) 'even
                   (if (= n 1) 'odd
                       (%tco-parity (- n 2)))))",
        )
        .unwrap();
        assert_eq!(s.eval("(%tco-parity 2000000)").unwrap(), "EVEN");
        assert_eq!(s.eval("(%tco-parity 2000001)").unwrap(), "ODD");
    }

    #[test]
    fn non_tail_self_recursion_still_returns_values() {
        // A self-call that is NOT in tail position (here, an argument to
        // `+`) must be left as an ordinary call — TCO must not rewrite
        // it. Correctness check at a shallow depth that doesn't
        // overflow.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(
            "(defun %sum-to (n)
               (if (= n 0) 0 (+ n (%sum-to (- n 1)))))",
        )
        .unwrap();
        assert_eq!(s.eval("(%sum-to 100)").unwrap(), "5050");
    }

    #[test]
    fn nested_loops_each_have_own_break() {
        // Inner return only exits the inner loop. Outer continues
        // and exits via its own terminal return.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(
                "(let ((outer 0))
                   (loop
                     (cond
                       ((= outer 3) (return outer))
                       (t (setq outer (+ outer 1))
                          (loop (return :inner))))))",
            )
            .unwrap(),
            "3",
        );
    }

    #[test]
    fn loop_return_value_can_be_anything() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(loop (return 'foo))").unwrap(), "FOO");
        assert_eq!(s.eval("(loop (return '(1 2 3)))").unwrap(), "(1 2 3)");
        assert_eq!(s.eval(r#"(loop (return "hello"))"#).unwrap(), r#""hello""#);
    }

    #[test]
    fn getf_finds_value() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(getf '(:a 1 :b 2 :c 3) :b)").unwrap(),
            "2",
        );
    }

    #[test]
    fn getf_returns_nil_when_not_found() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(getf '(:a 1 :b 2) :z)").unwrap(),
            "nil",
        );
        assert_eq!(s.eval("(getf nil :a)").unwrap(), "nil");
    }

    #[test]
    fn getf_returns_first_match() {
        let mut s = Session::with_stdlib().unwrap();
        // Duplicate keys: returns the first value found.
        assert_eq!(
            s.eval("(getf '(:a 1 :a 2 :a 3) :a)").unwrap(),
            "1",
        );
    }

    #[test]
    fn loop_with_getf_event_pattern() {
        // The event-loop idiom we'd actually write for the GUI:
        // a loop that pulls items off a "queue" (here a list) and
        // exits when it sees a sentinel kind.
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defparameter *q* '((:kind :a :v 1) (:kind :b :v 2) (:kind :stop)))").unwrap();
        s.eval(
            "(defun next-item ()
               (cond
                 ((null *q*) nil)
                 (t (let ((it (car *q*)))
                      (setq *q* (cdr *q*))
                      it))))",
        )
        .unwrap();
        assert_eq!(
            s.eval(
                "(let ((collected nil))
                   (loop
                     (let ((ev (next-item)))
                       (cond
                         ((null ev) (return (reverse collected)))
                         ((eq (getf ev :kind) :stop) (return (reverse collected)))
                         (t (setq collected (cons (getf ev :v) collected)))))))",
            )
            .unwrap(),
            "(1 2)",
        );
    }

    // -- Conditions ------------------------------------------------------

    #[test]
    fn handler_case_no_signal_returns_body_value() {
        // When the body completes normally, handler-case returns
        // the body's value.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(handler-case 42 (error (c) (+ 100 0)))").unwrap(),
            "42",
        );
    }

    #[test]
    fn error_signals_handler_runs() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(handler-case (error "boom") (error (c) c))"#).unwrap(),
            r#""boom""#,
        );
    }

    #[test]
    fn handler_returns_replacement_value() {
        let mut s = Session::with_stdlib().unwrap();
        // Handler can compute a replacement value of any type.
        assert_eq!(
            s.eval(r#"(handler-case (error "x") (error (c) 99))"#).unwrap(),
            "99",
        );
    }

    #[test]
    fn handler_can_use_condition_value() {
        // The handler can format the condition into a message.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(
                r#"(handler-case (error "thing failed")
                     (error (c) (format nil "caught: ~A" c)))"#,
            )
            .unwrap(),
            r#""caught: thing failed""#,
        );
    }

    #[test]
    fn nested_handler_case_inner_catches() {
        let mut s = Session::with_stdlib().unwrap();
        // Inner handler catches; outer never sees it.
        assert_eq!(
            s.eval(
                r#"(handler-case
                     (handler-case (error "inner")
                       (error (c) "inner caught"))
                     (error (c) "outer caught"))"#,
            )
            .unwrap(),
            r#""inner caught""#,
        );
    }

    #[test]
    fn nested_handler_case_outer_catches_when_inner_skips() {
        let mut s = Session::with_stdlib().unwrap();
        // Inner doesn't run; outer handler catches the rethrown.
        assert_eq!(
            s.eval(
                r#"(handler-case
                     (progn (handler-case 1 (error (c) c))
                            (error "from outer body"))
                     (error (c) c))"#,
            )
            .unwrap(),
            r#""from outer body""#,
        );
    }

    #[test]
    fn error_unwinds_through_user_frames() {
        // (error) deep inside a call chain is caught by the outer
        // handler-case — proving panic propagation crosses JIT
        // frames cleanly.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(r#"(defun deep () (error "from deep"))"#).unwrap();
        s.eval("(defun mid () (deep))").unwrap();
        s.eval("(defun top () (mid))").unwrap();
        assert_eq!(
            s.eval("(handler-case (top) (error (c) c))").unwrap(),
            r#""from deep""#,
        );
    }

    #[test]
    fn condition_value_can_be_any_type() {
        // The condition isn't required to be a string. Any Word
        // works — here we pass a list.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(handler-case (error '(:bad 42)) (error (c) c))").unwrap(),
            "(:BAD 42)",
        );
    }

    #[test]
    fn handler_case_in_let_body() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(
                r#"(let ((x 10))
                     (handler-case (progn (error "oops") x)
                       (error (c) (+ x 1))))"#,
            )
            .unwrap(),
            "11",
        );
    }

    // -- Keywords --------------------------------------------------------

    #[test]
    fn keyword_self_evaluates() {
        // :foo evaluates to the symbol :FOO.
        assert_eq!(eval_str(":foo").unwrap(), ":FOO");
        assert_eq!(eval_str(":input").unwrap(), ":INPUT");
    }

    #[test]
    fn keyword_eq_to_itself() {
        // The same keyword is eq to itself (interned).
        assert_eq!(eval_str("(eq :foo :foo)").unwrap(), "T");
        assert_eq!(eval_str("(eq :foo :bar)").unwrap(), "nil");
    }

    #[test]
    fn keyword_distinct_from_symbol() {
        // :foo and 'foo are different symbols.
        assert_eq!(eval_str("(eq :foo 'foo)").unwrap(), "nil");
    }

    #[test]
    fn keyword_in_quoted_list() {
        // Quoted lists containing keywords preserve them.
        assert_eq!(
            eval_str("'(:input :output :append)").unwrap(),
            "(:INPUT :OUTPUT :APPEND)",
        );
    }

    #[test]
    fn keyword_as_arg() {
        // Pass keywords to functions — they evaluate to themselves
        // and travel through the call without ceremony.
        let mut s = Session::new();
        s.eval("(defun classify (k) (cond ((eq k :input) 1) ((eq k :output) 2) (t 0)))").unwrap();
        assert_eq!(s.eval("(classify :input)").unwrap(), "1");
        assert_eq!(s.eval("(classify :output)").unwrap(), "2");
        assert_eq!(s.eval("(classify :other)").unwrap(), "0");
    }

    #[test]
    fn keyword_in_macro_dispatch() {
        // A macro that compares against keyword literals at expansion
        // time (the with-open-file pattern).
        let mut s = Session::new();
        s.eval(
            "(defmacro pick (k) \
               (cond \
                 ((eq k :a) ''first) \
                 ((eq k :b) ''second) \
                 (t ''other)))",
        )
        .unwrap();
        assert_eq!(s.eval("(pick :a)").unwrap(), "FIRST");
        assert_eq!(s.eval("(pick :b)").unwrap(), "SECOND");
        assert_eq!(s.eval("(pick :z)").unwrap(), "OTHER");
    }

    #[test]
    fn keyword_through_format() {
        // Keywords print with the colon under both ~A and ~S.
        assert_eq!(
            eval_str(r#"(format nil "~A" :hello)"#).unwrap(),
            r#"":HELLO""#,
        );
        assert_eq!(
            eval_str(r#"(format nil "~S" :hello)"#).unwrap(),
            r#"":HELLO""#,
        );
    }

    #[test]
    fn keyword_through_apply() {
        let mut s = Session::new();
        s.eval("(defun marker-of (k) (cond ((eq k :red) 1) (t 0)))").unwrap();
        assert_eq!(s.eval("(apply #'marker-of '(:red))").unwrap(), "1");
        assert_eq!(s.eval("(apply #'marker-of '(:blue))").unwrap(), "0");
    }

    // -- File I/O --------------------------------------------------------

    fn temp_path(name: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("ncl_test_{}_{name}", std::process::id()));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn file_round_trip_string() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("roundtrip.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        s.eval(r#"(write-file-string *p* "hello, world!")"#).unwrap();
        assert_eq!(
            s.eval("(read-file-string *p*)").unwrap(),
            r#""hello, world!""#,
        );
        s.eval("(delete-file *p*)").unwrap();
    }

    #[test]
    fn file_exists_round_trip() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("exists.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        // Doesn't exist yet.
        assert_eq!(s.eval("(file-exists *p*)").unwrap(), "nil");
        s.eval(r#"(write-file-string *p* "hi")"#).unwrap();
        assert_eq!(s.eval("(file-exists *p*)").unwrap(), "T");
        s.eval("(delete-file *p*)").unwrap();
        assert_eq!(s.eval("(file-exists *p*)").unwrap(), "nil");
    }

    #[test]
    fn file_lines_round_trip() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("lines.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        s.eval(r#"(write-file-lines *p* '("alpha" "beta" "gamma"))"#).unwrap();
        assert_eq!(
            s.eval("(read-file-lines *p*)").unwrap(),
            r#"("alpha" "beta" "gamma")"#,
        );
        s.eval("(delete-file *p*)").unwrap();
    }

    #[test]
    fn with_open_file_macro_works() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("with_open.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        // Write via with-open-file (using real CL keywords).
        s.eval(
            "(with-open-file (out *p* :output) \
               (write-line out \"line 1\") \
               (write-line out \"line 2\"))",
        )
        .unwrap();
        // Read back via with-open-file.
        assert_eq!(
            s.eval("(with-open-file (in *p* :input) (read-line in))").unwrap(),
            r#""line 1""#,
        );
        s.eval("(delete-file *p*)").unwrap();
    }

    #[test]
    fn read_line_returns_nil_at_eof() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("eof.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        s.eval(r#"(write-file-string *p* "single")"#).unwrap();
        s.eval("(defparameter *h* (open-input-file *p*))").unwrap();
        assert_eq!(s.eval("(read-line *h*)").unwrap(), r#""single""#);
        // Past EOF.
        assert_eq!(s.eval("(read-line *h*)").unwrap(), "nil");
        s.eval("(close-stream *h*)").unwrap();
        s.eval("(delete-file *p*)").unwrap();
    }

    #[test]
    fn file_append_mode() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("append.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        // Write initial.
        s.eval(r#"(write-file-string *p* "first")"#).unwrap();
        // Append more.
        s.eval(
            "(with-open-file (out *p* :append) (write-string-to out \"-second\"))",
        )
        .unwrap();
        assert_eq!(
            s.eval("(read-file-string *p*)").unwrap(),
            r#""first-second""#,
        );
        s.eval("(delete-file *p*)").unwrap();
    }

    #[test]
    fn file_open_failure_returns_zero_handle() {
        let mut s = Session::with_stdlib().unwrap();
        // A path that almost certainly doesn't exist.
        let h = s
            .eval(r#"(open-input-file "Z:\\definitely-not-here-9876.xyz")"#)
            .unwrap();
        assert_eq!(h, "0");
    }

    #[test]
    fn file_unicode_round_trip() {
        let mut s = Session::with_stdlib().unwrap();
        let path = temp_path("unicode.txt");
        s.eval(&format!(r#"(defparameter *p* "{}")"#, path.replace('\\', "\\\\")))
            .unwrap();
        s.eval(r#"(write-file-string *p* "café 🦀 日本")"#).unwrap();
        assert_eq!(
            s.eval("(read-file-string *p*)").unwrap(),
            r#""café 🦀 日本""#,
        );
        s.eval("(delete-file *p*)").unwrap();
    }

    // -- defmacro / macros -----------------------------------------------

    #[test]
    fn defmacro_returns_nil() {
        let mut s = Session::new();
        // Defining a macro is a side-effecting form; it returns nil.
        assert_eq!(
            s.eval("(defmacro my-id (x) x)").unwrap(),
            "nil",
        );
    }

    #[test]
    fn macro_simplest_passthrough() {
        // The macro body returns its argument unchanged. Calling
        // (my-id 42) expands to 42.
        let mut s = Session::new();
        s.eval("(defmacro my-id (x) x)").unwrap();
        assert_eq!(s.eval("(my-id 42)").unwrap(), "42");
        // The arg is the literal form, so quoted forms work too.
        assert_eq!(s.eval("(my-id (+ 1 2))").unwrap(), "3");
    }

    #[test]
    fn macro_constructs_form_with_list() {
        // (my-when test body) → (if test body nil), built with list
        // and quote (no backquote yet).
        let mut s = Session::new();
        s.eval(
            "(defmacro my-when (test body) \
               (list 'if test body 'nil))",
        )
        .unwrap();
        assert_eq!(s.eval("(my-when t 5)").unwrap(), "5");
        assert_eq!(s.eval("(my-when nil 5)").unwrap(), "nil");
        // The arg forms are unevaluated until the expansion runs.
        assert_eq!(s.eval("(my-when (> 3 2) (+ 10 20))").unwrap(), "30");
    }

    #[test]
    fn macro_expansion_runs_at_compile_time() {
        // The macro body uses (list 'quote x) to embed a literal
        // form into the expansion. The expansion only runs the
        // outer form, not the embedded literal.
        let mut s = Session::new();
        s.eval(
            "(defmacro literal (x) \
               (list 'quote x))",
        )
        .unwrap();
        // (literal foo) → 'foo → the symbol FOO.
        assert_eq!(s.eval("(literal foo)").unwrap(), "FOO");
        assert_eq!(s.eval("(literal (1 2 3))").unwrap(), "(1 2 3)");
    }

    #[test]
    fn macro_with_rest_args() {
        // &rest in a macro captures all the remaining arg forms.
        // Build (progn arg1 arg2 ...) — return the last one's value.
        let mut s = Session::new();
        s.eval(
            "(defmacro my-progn (&rest forms) \
               (cons 'progn forms))",
        )
        .unwrap();
        assert_eq!(s.eval("(my-progn 1 2 3)").unwrap(), "3");
        assert_eq!(s.eval("(my-progn (+ 1 1) (+ 2 2))").unwrap(), "4");
    }

    #[test]
    fn macro_recursive_expansion() {
        // A macro whose expansion contains another macro call —
        // both get expanded.
        let mut s = Session::new();
        s.eval("(defmacro inc (x) (list '+ x 1))").unwrap();
        s.eval("(defmacro double-inc (x) (list 'inc (list 'inc x)))").unwrap();
        assert_eq!(s.eval("(double-inc 5)").unwrap(), "7");
    }

    #[test]
    fn macro_inside_defun_body() {
        // Macros used inside a defun's body get expanded at compile
        // time — the defun stores the expanded code, not a runtime
        // macro call.
        let mut s = Session::new();
        s.eval("(defmacro plus1 (x) (list '+ x 1))").unwrap();
        s.eval("(defun bump (n) (plus1 n))").unwrap();
        assert_eq!(s.eval("(bump 41)").unwrap(), "42");
    }

    #[test]
    fn macro_in_let_body() {
        let mut s = Session::new();
        s.eval("(defmacro plus1 (x) (list '+ x 1))").unwrap();
        assert_eq!(
            s.eval("(let ((x 10)) (plus1 x))").unwrap(),
            "11",
        );
    }

    #[test]
    fn macro_can_use_quote_in_expansion() {
        let mut s = Session::new();
        // Wraps the form in (quote ...).
        s.eval(
            "(defmacro literally (x) \
               (cons 'quote (cons x nil)))",
        )
        .unwrap();
        assert_eq!(s.eval("(literally hello)").unwrap(), "HELLO");
        assert_eq!(s.eval("(literally (a b c))").unwrap(), "(A B C)");
    }

    #[test]
    fn macro_redefinition_replaces() {
        // A second defmacro with the same name installs a new
        // expansion; subsequent calls use the new one. (Existing
        // compiled code bound to the OLD expansion stays bound to
        // the old expansion — that's CL semantics.)
        let mut s = Session::new();
        s.eval("(defmacro tag (x) (list 'list ''v1 x))").unwrap();
        assert_eq!(s.eval("(tag 5)").unwrap(), "(V1 5)");
        s.eval("(defmacro tag (x) (list 'list ''v2 x))").unwrap();
        assert_eq!(s.eval("(tag 5)").unwrap(), "(V2 5)");
    }

    // -- backquote / quasiquote ------------------------------------------

    #[test]
    fn backquote_atom_is_quoted() {
        // `foo → 'foo → FOO at evaluation time.
        assert_eq!(eval_str("`foo").unwrap(), "FOO");
        // Numbers self-quote.
        assert_eq!(eval_str("`42").unwrap(), "42");
        // nil self-quotes.
        assert_eq!(eval_str("`nil").unwrap(), "nil");
        // Strings.
        assert_eq!(eval_str(r#"`"hi""#).unwrap(), r#""hi""#);
    }

    #[test]
    fn backquote_unquote_evaluates() {
        // `,x → x.
        let mut s = Session::new();
        s.eval("(defparameter *v* 42)").unwrap();
        assert_eq!(s.eval("`,*v*").unwrap(), "42");
    }

    #[test]
    fn backquote_list_with_unquotes() {
        // `(a ,b c) → (cons 'a (cons b (cons 'c nil))).
        let mut s = Session::new();
        s.eval("(defparameter *x* 99)").unwrap();
        assert_eq!(
            s.eval("`(a ,*x* c)").unwrap(),
            "(A 99 C)",
        );
    }

    #[test]
    fn backquote_with_splice() {
        // `(a ,@xs b) splices xs into the list.
        let mut s = Session::new();
        s.eval("(defparameter *xs* '(1 2 3))").unwrap();
        assert_eq!(
            s.eval("`(a ,@*xs* b)").unwrap(),
            "(A 1 2 3 B)",
        );
        // Empty splice — disappears.
        s.eval("(defparameter *empty* nil)").unwrap();
        assert_eq!(
            s.eval("`(a ,@*empty* b)").unwrap(),
            "(A B)",
        );
    }

    #[test]
    fn backquote_only_splice() {
        // `(,@xs) is just xs.
        let mut s = Session::new();
        s.eval("(defparameter *xs* '(1 2 3))").unwrap();
        assert_eq!(s.eval("`(,@*xs*)").unwrap(), "(1 2 3)");
    }

    #[test]
    fn backquote_dotted() {
        // `(a . ,x) puts x in the cdr.
        let mut s = Session::new();
        s.eval("(defparameter *x* 5)").unwrap();
        assert_eq!(s.eval("`(a . ,*x*)").unwrap(), "(A . 5)");
    }

    #[test]
    fn backquote_in_macro_my_when() {
        // The classic example: write `when` cleanly using backquote.
        let mut s = Session::new();
        s.eval(
            "(defmacro my-when (test &rest body) \
               `(if ,test (progn ,@body) nil))",
        )
        .unwrap();
        assert_eq!(s.eval("(my-when t 1 2 3)").unwrap(), "3");
        assert_eq!(s.eval("(my-when nil 1 2 3)").unwrap(), "nil");
        assert_eq!(s.eval("(my-when (> 5 3) (+ 1 2) (+ 3 4))").unwrap(), "7");
    }

    #[test]
    fn backquote_in_macro_unless() {
        let mut s = Session::new();
        s.eval(
            "(defmacro my-unless (test &rest body) \
               `(if ,test nil (progn ,@body)))",
        )
        .unwrap();
        assert_eq!(s.eval("(my-unless nil 1 2 3)").unwrap(), "3");
        assert_eq!(s.eval("(my-unless t 1 2 3)").unwrap(), "nil");
    }

    #[test]
    fn backquote_in_macro_with_splicing_body() {
        // (let-1 var val body...) → (let ((var val)) body...)
        let mut s = Session::new();
        s.eval(
            "(defmacro let-1 (var val &rest body) \
               `(let ((,var ,val)) ,@body))",
        )
        .unwrap();
        assert_eq!(s.eval("(let-1 x 10 (+ x 1))").unwrap(), "11");
        assert_eq!(
            s.eval("(let-1 x 10 (+ x 1) (* x 2))").unwrap(),
            "20",
        );
    }

    #[test]
    fn backquote_nested_list_structure() {
        // `((a ,x) (b ,y))
        let mut s = Session::new();
        s.eval("(defparameter *x* 1)").unwrap();
        s.eval("(defparameter *y* 2)").unwrap();
        assert_eq!(
            s.eval("`((a ,*x*) (b ,*y*))").unwrap(),
            "((A 1) (B 2))",
        );
    }

    #[test]
    fn backquote_unquote_outside_backquote_errors() {
        // ,x at top level — not inside backquote.
        let r = eval_str(",foo");
        assert!(r.is_err());
    }

    #[test]
    fn quote_is_opaque_to_macroexpansion() {
        // (when ...) is the WHEN special form (already a primitive).
        // But if we redefine it as a macro and quote a (when ...)
        // form, the quoted data should not get expanded.
        let mut s = Session::new();
        s.eval("(defmacro mine (x) (list '+ x 1))").unwrap();
        // Inside quote, (mine 5) is just data — no expansion.
        assert_eq!(s.eval("(quote (mine 5))").unwrap(), "(MINE 5)");
        // Outside quote, it expands.
        assert_eq!(s.eval("(mine 5)").unwrap(), "6");
    }

    // -- format ----------------------------------------------------------

    #[test]
    fn format_nil_returns_string() {
        // (format nil "...") returns the formatted text as a String.
        assert_eq!(
            eval_str(r#"(format nil "hello, world")"#).unwrap(),
            r#""hello, world""#,
        );
    }

    #[test]
    fn format_a_directive_aesthetic() {
        // ~A: aesthetic — strings printed without quotes.
        assert_eq!(
            eval_str(r#"(format nil "name: ~A!" "Alice")"#).unwrap(),
            r#""name: Alice!""#,
        );
        // Numbers print as digits.
        assert_eq!(
            eval_str(r#"(format nil "x = ~A" 42)"#).unwrap(),
            r#""x = 42""#,
        );
        // nil prints as "nil".
        assert_eq!(
            eval_str(r#"(format nil "got ~A" nil)"#).unwrap(),
            r#""got nil""#,
        );
        // Lists print structurally.
        assert_eq!(
            eval_str(r#"(format nil "list: ~A" '(1 2 3))"#).unwrap(),
            r#""list: (1 2 3)""#,
        );
    }

    #[test]
    fn format_s_directive_readable() {
        // ~S: readable — strings keep their quotes.
        assert_eq!(
            eval_str(r#"(format nil "got ~S" "hi")"#).unwrap(),
            r#""got \"hi\"""#,
        );
        // Chars print as #\X. The eval result is itself a string;
        // when the REPL prints it back, the backslash inside the
        // payload gets escaped, so `#\a` round-trips as `"#\\a"`.
        // Use explicit escapes to dodge the Rust 2021 reserved-
        // prefix lint on `r#"#`.
        assert_eq!(
            eval_str("(format nil \"~S\" #\\a)").unwrap(),
            "\"#\\\\a\"",
        );
    }

    #[test]
    fn format_d_directive_decimal() {
        assert_eq!(
            eval_str(r#"(format nil "n = ~D" 1234)"#).unwrap(),
            r#""n = 1234""#,
        );
        assert_eq!(
            eval_str(r#"(format nil "neg: ~D" -7)"#).unwrap(),
            r#""neg: -7""#,
        );
    }

    #[test]
    fn format_percent_emits_newline() {
        assert_eq!(
            eval_str(r#"(format nil "line1~%line2")"#).unwrap(),
            "\"line1\nline2\"",
        );
    }

    #[test]
    fn format_tilde_tilde_is_literal() {
        assert_eq!(
            eval_str(r#"(format nil "~~ here")"#).unwrap(),
            r#""~ here""#,
        );
    }

    #[test]
    fn format_multiple_directives() {
        assert_eq!(
            eval_str(r#"(format nil "~A is ~D years old" "Bob" 30)"#).unwrap(),
            r#""Bob is 30 years old""#,
        );
    }

    #[test]
    fn format_unicode_in_control_string() {
        // The control string is UTF-32 internally; full Unicode pass-through.
        assert_eq!(
            eval_str(r#"(format nil "héllo ~A" "🦀")"#).unwrap(),
            r#""héllo 🦀""#,
        );
    }

    #[test]
    fn format_to_t_returns_nil() {
        // (format t ...) writes to stdout and returns nil. The
        // tests can't easily capture stdout, so just verify the
        // return value.
        let mut s = Session::new();
        assert_eq!(s.eval(r#"(format t "ignored")"#).unwrap(), "nil");
    }

    #[test]
    fn format_is_first_class() {
        // FORMAT is a real callable function — usable via funcall,
        // apply, #'.
        let mut s = Session::new();
        s.eval("(defparameter *fmt* #'format)").unwrap();
        assert_eq!(
            s.eval(r#"(funcall *fmt* nil "hi ~A" 42)"#).unwrap(),
            r#""hi 42""#,
        );
        // apply works too — splat a list of args.
        assert_eq!(
            s.eval(r#"(apply #'format nil "~A and ~A" '(1 2))"#).unwrap(),
            r#""1 and 2""#,
        );
    }

    #[test]
    fn format_no_args_passes_through() {
        // No directives, no args — returns the control unchanged.
        assert_eq!(
            eval_str(r#"(format nil "plain text")"#).unwrap(),
            r#""plain text""#,
        );
        // Empty control.
        assert_eq!(eval_str(r#"(format nil "")"#).unwrap(), r#""""#);
    }

    // -- apply -----------------------------------------------------------

    #[test]
    fn apply_simple() {
        // (apply f lst) — splat lst as args to f.
        let mut s = Session::new();
        s.eval("(defun add3 (a b c) (+ a b c))").unwrap();
        assert_eq!(s.eval("(apply #'add3 '(1 2 3))").unwrap(), "6");
    }

    #[test]
    fn apply_with_prefix() {
        // (apply f a b lst) — prefix a, b followed by lst.
        let mut s = Session::new();
        s.eval("(defun add4 (a b c d) (+ a b c d))").unwrap();
        assert_eq!(s.eval("(apply #'add4 1 '(2 3 4))").unwrap(), "10");
        assert_eq!(s.eval("(apply #'add4 1 2 '(3 4))").unwrap(), "10");
        assert_eq!(s.eval("(apply #'add4 1 2 3 '(4))").unwrap(), "10");
    }

    #[test]
    fn apply_with_empty_tail() {
        let mut s = Session::new();
        s.eval("(defun add2 (a b) (+ a b))").unwrap();
        assert_eq!(s.eval("(apply #'add2 1 2 nil)").unwrap(), "3");
    }

    #[test]
    fn apply_to_variadic() {
        // The classic apply use case: pass a list to a &rest-taking
        // function so it sees the elements as separate args.
        let mut s = Session::new();
        s.eval("(defun all (&rest r) r)").unwrap();
        assert_eq!(s.eval("(apply #'all '(1 2 3 4))").unwrap(), "(1 2 3 4)");
        assert_eq!(s.eval("(apply #'all 0 '(1 2 3))").unwrap(), "(0 1 2 3)");
    }

    #[test]
    fn apply_with_lambda() {
        let mut s = Session::new();
        assert_eq!(
            s.eval("(apply (lambda (a b) (* a b)) '(7 6))").unwrap(),
            "42",
        );
    }

    #[test]
    fn apply_to_stdlib_min() {
        // Use stdlib's variadic `min` via apply.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(apply #'min '(3 1 4 1 5 9 2 6))").unwrap(), "1");
        assert_eq!(s.eval("(apply #'max '(3 1 4 1 5 9 2 6))").unwrap(), "9");
    }

    #[test]
    fn apply_arity_too_few_errors_at_compile() {
        // (apply) and (apply f) — both lack the tail list.
        let r = eval_str("(apply)");
        assert!(r.is_err());
        let r = eval_str("(apply #'+)");
        assert!(r.is_err());
    }

    #[test]
    fn apply_returns_value() {
        let mut s = Session::new();
        s.eval("(defun double (x) (* x 2))").unwrap();
        assert_eq!(s.eval("(apply #'double '(21))").unwrap(), "42");
    }

    // -- truncate / rem (integer division primitives) ---------------------

    #[test]
    fn truncate_basic() {
        assert_eq!(eval_str("(truncate 10 3)").unwrap(), "3");
        assert_eq!(eval_str("(truncate 7 2)").unwrap(), "3");
        assert_eq!(eval_str("(truncate 6 3)").unwrap(), "2");
        // Truncate rounds toward zero — negative dividend.
        assert_eq!(eval_str("(truncate -7 2)").unwrap(), "-3");
        assert_eq!(eval_str("(truncate 7 -2)").unwrap(), "-3");
        assert_eq!(eval_str("(truncate -7 -2)").unwrap(), "3");
    }

    #[test]
    fn rem_basic() {
        assert_eq!(eval_str("(rem 10 3)").unwrap(), "1");
        assert_eq!(eval_str("(rem 7 2)").unwrap(), "1");
        assert_eq!(eval_str("(rem 6 3)").unwrap(), "0");
        // rem matches the sign of the dividend.
        assert_eq!(eval_str("(rem -7 2)").unwrap(), "-1");
        assert_eq!(eval_str("(rem 7 -2)").unwrap(), "1");
        assert_eq!(eval_str("(rem -7 -2)").unwrap(), "-1");
    }

    #[test]
    fn truncate_rem_invariant() {
        // (= a (+ (* (truncate a b) b) (rem a b))) for any a, b.
        let mut session = Session::new();
        session
            .eval("(defun ok (a b) (= a (+ (* (truncate a b) b) (rem a b))))")
            .unwrap();
        assert_eq!(session.eval("(ok 17 5)").unwrap(), "T");
        assert_eq!(session.eval("(ok -17 5)").unwrap(), "T");
        assert_eq!(session.eval("(ok 17 -5)").unwrap(), "T");
        assert_eq!(session.eval("(ok -17 -5)").unwrap(), "T");
        assert_eq!(session.eval("(ok 42 7)").unwrap(), "T");
    }

    #[test]
    fn stdlib_mod_matches_divisor_sign() {
        let mut s = Session::with_stdlib().unwrap();
        // Same sign as divisor — differs from rem when signs differ.
        assert_eq!(s.eval("(mod 10 3)").unwrap(), "1");
        assert_eq!(s.eval("(mod -7 2)").unwrap(), "1");   // rem returns -1
        assert_eq!(s.eval("(mod 7 -2)").unwrap(), "-1");  // rem returns 1
        assert_eq!(s.eval("(mod -7 -2)").unwrap(), "-1"); // matches rem
        // Exact divisions return 0 regardless of sign.
        assert_eq!(s.eval("(mod 6 3)").unwrap(), "0");
        assert_eq!(s.eval("(mod -6 3)").unwrap(), "0");
    }

    #[test]
    fn stdlib_oddp_evenp() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(evenp 0)").unwrap(), "T");
        assert_eq!(s.eval("(evenp 4)").unwrap(), "T");
        assert_eq!(s.eval("(evenp -4)").unwrap(), "T");
        assert_eq!(s.eval("(evenp 3)").unwrap(), "nil");
        assert_eq!(s.eval("(oddp 3)").unwrap(), "T");
        assert_eq!(s.eval("(oddp -3)").unwrap(), "T");
        assert_eq!(s.eval("(oddp 0)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_floor_rounds_toward_negative_infinity() {
        let mut s = Session::with_stdlib().unwrap();
        // Differs from truncate on mixed signs with non-zero remainder.
        assert_eq!(s.eval("(floor 7 2)").unwrap(), "3");
        assert_eq!(s.eval("(floor -7 2)").unwrap(), "-4");  // truncate would give -3
        assert_eq!(s.eval("(floor 7 -2)").unwrap(), "-4");
        assert_eq!(s.eval("(floor -7 -2)").unwrap(), "3");
        // Exact divisions match truncate.
        assert_eq!(s.eval("(floor 6 3)").unwrap(), "2");
        assert_eq!(s.eval("(floor -6 3)").unwrap(), "-2");
    }

    // -- when / unless ----------------------------------------------------

    #[test]
    fn when_true_runs_body() {
        assert_eq!(eval_str("(when t 1 2 3)").unwrap(), "3");
    }

    #[test]
    fn when_false_returns_nil() {
        assert_eq!(eval_str("(when nil 1 2 3)").unwrap(), "nil");
    }

    #[test]
    fn when_no_body_returns_nil() {
        assert_eq!(eval_str("(when t)").unwrap(), "nil");
        assert_eq!(eval_str("(when nil)").unwrap(), "nil");
    }

    #[test]
    fn unless_inverts_when() {
        assert_eq!(eval_str("(unless nil 1 2 3)").unwrap(), "3");
        assert_eq!(eval_str("(unless t 1 2 3)").unwrap(), "nil");
    }

    // -- Core stdlib (Lisp/core.lisp) -------------------------------------

    #[test]
    fn stdlib_loads_clean() {
        // Smoke test: the file evaluates without error.
        let mut s = Session::with_stdlib().expect("stdlib should load");
        // Bonus: a defined function is callable.
        assert_eq!(s.eval("(first '(1 2 3))").unwrap(), "1");
    }

    #[test]
    fn stdlib_accessors() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(first '(a b c))").unwrap(), "A");
        assert_eq!(s.eval("(rest '(a b c))").unwrap(), "(B C)");
        assert_eq!(s.eval("(second '(1 2 3))").unwrap(), "2");
        assert_eq!(s.eval("(third '(1 2 3 4))").unwrap(), "3");
        assert_eq!(s.eval("(fourth '(1 2 3 4 5))").unwrap(), "4");
        assert_eq!(s.eval("(cadr '(1 2 3))").unwrap(), "2");
        assert_eq!(s.eval("(cddr '(1 2 3 4))").unwrap(), "(3 4)");
    }

    #[test]
    fn stdlib_reverse() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(reverse '(1 2 3 4 5))").unwrap(), "(5 4 3 2 1)");
        assert_eq!(s.eval("(reverse nil)").unwrap(), "nil");
        assert_eq!(s.eval("(reverse '(a))").unwrap(), "(A)");
    }

    #[test]
    fn stdlib_append() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(append '(1 2 3) '(4 5))").unwrap(),
            "(1 2 3 4 5)",
        );
        assert_eq!(s.eval("(append nil '(a b))").unwrap(), "(A B)");
        assert_eq!(s.eval("(append '(a b) nil)").unwrap(), "(A B)");
    }

    #[test]
    fn stdlib_mapcar() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defun double (x) (* x 2))").unwrap();
        assert_eq!(
            s.eval("(mapcar #'double '(1 2 3 4))").unwrap(),
            "(2 4 6 8)",
        );
        // With a lambda.
        assert_eq!(
            s.eval("(mapcar (lambda (x) (+ x 10)) '(1 2 3))").unwrap(),
            "(11 12 13)",
        );
        assert_eq!(s.eval("(mapcar #'double nil)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_member() {
        let mut s = Session::with_stdlib().unwrap();
        // Fixnums are eql by value, so the default :test #'eql works.
        assert_eq!(s.eval("(member 3 '(1 2 3 4 5))").unwrap(), "(3 4 5)");
        assert_eq!(s.eval("(member 99 '(1 2 3))").unwrap(), "nil");
        // Symbols interned in the same package are eq, hence eql.
        assert_eq!(
            s.eval("(member 'b '(a b c))").unwrap(),
            "(B C)",
        );
        // Strings are NOT eql by content (they are eql only by
        // identity); the caller must opt into equal for content
        // comparison. This matches ANSI CL — and Corman.
        assert_eq!(
            s.eval(r#"(member "b" '("a" "b" "c") :test #'equal)"#).unwrap(),
            r#"("b" "c")"#,
        );
    }

    #[test]
    fn stdlib_position_and_find() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(position 'c '(a b c d))").unwrap(), "2");
        assert_eq!(s.eval("(position 'z '(a b c))").unwrap(), "nil");
        assert_eq!(s.eval("(find 3 '(1 2 3 4))").unwrap(), "3");
        assert_eq!(s.eval("(find 99 '(1 2 3))").unwrap(), "nil");
    }

    #[test]
    fn stdlib_assoc() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(assoc 'b '((a . 1) (b . 2) (c . 3)))").unwrap(),
            "(B . 2)",
        );
        assert_eq!(
            s.eval("(assoc 'z '((a . 1) (b . 2)))").unwrap(),
            "nil",
        );
    }

    #[test]
    fn stdlib_nth_and_nthcdr() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(nth 0 '(a b c))").unwrap(), "A");
        assert_eq!(s.eval("(nth 2 '(a b c))").unwrap(), "C");
        assert_eq!(s.eval("(nthcdr 0 '(a b c))").unwrap(), "(A B C)");
        assert_eq!(s.eval("(nthcdr 1 '(a b c))").unwrap(), "(B C)");
        assert_eq!(s.eval("(nthcdr 3 '(a b c))").unwrap(), "nil");
    }

    #[test]
    fn stdlib_last_returns_last_cons() {
        let mut s = Session::with_stdlib().unwrap();
        // CL: last returns the LAST CONS, not the last element.
        assert_eq!(s.eval("(last '(1 2 3))").unwrap(), "(3)");
        assert_eq!(s.eval("(last '(a))").unwrap(), "(A)");
        assert_eq!(s.eval("(last nil)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_butlast() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(butlast '(1 2 3 4))").unwrap(), "(1 2 3)");
        assert_eq!(s.eval("(butlast '(1))").unwrap(), "nil");
        assert_eq!(s.eval("(butlast nil)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_every_some() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(every #'plusp '(1 2 3))").unwrap(), "T");
        assert_eq!(s.eval("(every #'plusp '(1 -1 3))").unwrap(), "nil");
        assert_eq!(s.eval("(every #'plusp nil)").unwrap(), "T");
        assert_eq!(s.eval("(some #'minusp '(1 2 -3))").unwrap(), "T");
        assert_eq!(s.eval("(some #'minusp '(1 2 3))").unwrap(), "nil");
    }

    #[test]
    fn stdlib_numeric_helpers() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(zerop 0)").unwrap(), "T");
        assert_eq!(s.eval("(zerop 5)").unwrap(), "nil");
        assert_eq!(s.eval("(plusp 5)").unwrap(), "T");
        assert_eq!(s.eval("(plusp -5)").unwrap(), "nil");
        assert_eq!(s.eval("(minusp -5)").unwrap(), "T");
        assert_eq!(s.eval("(1+ 41)").unwrap(), "42");
        assert_eq!(s.eval("(1- 43)").unwrap(), "42");
        assert_eq!(s.eval("(min2 3 7)").unwrap(), "3");
        assert_eq!(s.eval("(max2 3 7)").unwrap(), "7");
        assert_eq!(s.eval("(abs -5)").unwrap(), "5");
        assert_eq!(s.eval("(abs 5)").unwrap(), "5");
    }

    #[test]
    fn stdlib_copy_list_unshares() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defparameter *a* '(1 2 3))").unwrap();
        s.eval("(defparameter *b* (copy-list *a*))").unwrap();
        // Same content...
        assert_eq!(s.eval("(equal *a* *b*)").unwrap(), "T");
        // ...but distinct conses.
        assert_eq!(s.eval("(eq *a* *b*)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_composes() {
        // Build a non-trivial pipeline using stdlib functions.
        let mut s = Session::with_stdlib().unwrap();
        // Reverse, then mapcar 1+, then take last cons.
        assert_eq!(
            s.eval("(last (mapcar #'1+ (reverse '(1 2 3 4))))").unwrap(),
            "(2)",
        );
    }

    #[test]
    fn stdlib_min_max_variadic() {
        let mut s = Session::with_stdlib().unwrap();
        // Single-arg forms.
        assert_eq!(s.eval("(min 5)").unwrap(), "5");
        assert_eq!(s.eval("(max 5)").unwrap(), "5");
        // Multi-arg.
        assert_eq!(s.eval("(min 3 1 4 1 5 9 2 6)").unwrap(), "1");
        assert_eq!(s.eval("(max 3 1 4 1 5 9 2 6)").unwrap(), "9");
        assert_eq!(s.eval("(min 7 7 7)").unwrap(), "7");
        // Mixed signs.
        assert_eq!(s.eval("(min -5 -2 -10)").unwrap(), "-10");
        assert_eq!(s.eval("(max -5 -2 -10)").unwrap(), "-2");
    }

    #[test]
    fn stdlib_list_star() {
        let mut s = Session::with_stdlib().unwrap();
        // (list* x) ≡ x.
        assert_eq!(s.eval("(list* 'a)").unwrap(), "A");
        // (list* a b) ≡ (cons a b).
        assert_eq!(s.eval("(list* 1 2)").unwrap(), "(1 . 2)");
        // (list* a b c lst) ≡ (cons a (cons b (cons c lst)))
        assert_eq!(s.eval("(list* 1 2 3 '(4 5))").unwrap(), "(1 2 3 4 5)");
        // (list* 1 2 3 nil) ≡ (1 2 3)
        assert_eq!(s.eval("(list* 1 2 3 nil)").unwrap(), "(1 2 3)");
    }

    #[test]
    fn stdlib_append_star_variadic() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(append*)").unwrap(), "nil");
        assert_eq!(s.eval("(append* '(a b))").unwrap(), "(A B)");
        assert_eq!(
            s.eval("(append* '(1 2) '(3 4) '(5 6))").unwrap(),
            "(1 2 3 4 5 6)",
        );
        assert_eq!(s.eval("(append* nil nil nil)").unwrap(), "nil");
    }

    #[test]
    fn stdlib_identity() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(identity 42)").unwrap(), "42");
        assert_eq!(s.eval("(identity 'foo)").unwrap(), "FOO");
        assert_eq!(s.eval("(mapcar #'identity '(1 2 3))").unwrap(), "(1 2 3)");
    }

    // -- Errors ------------------------------------------------------------

    #[test]
    fn nested_defun_installs_function_and_returns_name() {
        // `(defun ...)` in non-top-level position lowers to
        // (%set-symbol-function 'NAME (lambda ...)) and returns
        // 'NAME — the form value is the symbol per CL spec, and
        // the function is reachable through subsequent calls.
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        assert_eq!(s.eval("(if t (defun add3 (n) (+ n 3)) 'unreached)").unwrap(), "ADD3");
        assert_eq!(s.eval("(add3 10)").unwrap(), "13");
    }

    #[test]
    fn let_accepts_bare_symbol_and_short_binding() {
        // CL allows three shapes per binding entry:
        //   * (name init)  — explicit
        //   * (name)       — init defaults to nil
        //   * name         — bare symbol, same as (name nil)
        // The bare-symbol shape is the one chapter-5
        // SUBFORM-EVALUATION uses (`(let (x) …)`).
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        assert_eq!(s.eval("(let (x) x)").unwrap(), "nil");
        assert_eq!(s.eval("(let ((a 1) (b) c) (list a b c))").unwrap(),
                   "(1 nil nil)");
    }

    #[test]
    fn setq_setf_accept_multi_pair_forms() {
        // CL's `(setq v1 e1 v2 e2 ...)` and
        // `(setf p1 e1 p2 e2 ...)` are even-arity multi-assignment
        // forms; the value of the form is the last assignment.
        // Chapter 3 of the corman ANSI suite aborts mid-suite without
        // this — the EVAL block has `(setq a 1 b 2 c 3)` as its first
        // setup line.
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        // Multi-pair setq into globals.
        s.eval("(defparameter aa 0) (defparameter bb 0) (defparameter cc 0)").unwrap();
        assert_eq!(s.eval("(setq aa 1 bb 2 cc 3)").unwrap(), "3");
        assert_eq!(s.eval("(list aa bb cc)").unwrap(), "(1 2 3)");
        // Multi-pair setq into local lets.
        assert_eq!(
            s.eval("(let ((x 0) (y 0)) (setq x 7 y 8) (list x y))").unwrap(),
            "(7 8)"
        );
        // Multi-pair setf — bare symbols and recognised places.
        s.eval("(defparameter pp (cons 1 2))").unwrap();
        s.eval("(setf (car pp) 'a (cdr pp) 'b)").unwrap();
        assert_eq!(s.eval("pp").unwrap(), "(A . B)");
        // Empty: returns nil per CL spec.
        assert_eq!(s.eval("(setq)").unwrap(), "nil");
        assert_eq!(s.eval("(setf)").unwrap(), "nil");
    }

    #[test]
    fn funcall_apply_accept_symbol_designators() {
        // CL spec: funcall/apply take a function DESIGNATOR — either a
        // function or a symbol whose function cell is bound. Without
        // this `(funcall '+ 1 2)` panicked with "not a function";
        // chapter 5 APPLY surfaced the gap.
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        assert_eq!(s.eval("(funcall '+ 5 6)").unwrap(), "11");
        assert_eq!(s.eval("(apply '+ '(1 2 3 4))").unwrap(), "10");
        assert_eq!(s.eval("(let ((f '*)) (funcall f 3 4))").unwrap(), "12");
        assert_eq!(s.eval("(let ((f 'list)) (apply f 1 '(2 3)))").unwrap(),
                   "(1 2 3)");
    }

    #[test]
    fn nested_defun_captures_enclosing_let() {
        // CL semantics: a nested defun captures the surrounding
        // lexical scope. The function is globally named but holds a
        // closure over the let frame. SBCL / CCL behave this way.
        let mut s = Session::with_stdlib().expect("session boots");
        s.activate();
        assert_eq!(s.eval(
            "(let ((x 42)) (defun get-x () x)) (get-x)"
        ).unwrap(), "42");
    }

    #[test]
    fn calling_undefined_function_panics_at_runtime() {
        // The compile succeeds (we can't tell the function is
        // undefined at compile time — the symbol just isn't bound
        // yet). At runtime, ncl_call panics on unbound. We catch
        // it for this test.
        // (Disabled — JIT panics aren't easy to catch from Rust
        // tests without unwinding through C frames. Documenting
        // behaviour here.)
    }

    #[test]
    fn malformed_defun_errors() {
        let r = eval_str("(defun)");
        assert!(matches!(r, Err(EvalError::Compile(CompileError::BadDefun(_)))));
        let r = eval_str("(defun foo bar 1)"); // params must be list
        assert!(matches!(r, Err(EvalError::Compile(CompileError::BadDefun(_)))));
    }

    // (Previous "bare unknown symbol fails to compile" test removed
    // — bare globals now lower to LoadGlobal, which panics at
    // runtime when unbound. The panic crosses an FFI boundary and
    // is messy to catch from a unit test, so the behaviour is
    // documented in the commit log instead. Real condition handling
    // arrives later.)

    // ── symbols.lisp inline tests ─────────────────────────────────────
    //
    // These tests inline the key definitions from Library/symbols.lisp
    // so they can run under Session::with_stdlib() without needing the
    // full init.lisp chain.  They mirror the assertions in
    // test-symbols.lisp.

    /// Minimal preamble: property-list functions.
    const PLIST_PRELUDE: &str = r#"
(defvar *symbol-plists* (make-hash-table :test 'eq))
(defun symbol-plist (sym) (gethash sym *symbol-plists*))
(defun (setf symbol-plist) (new-plist sym)
  (if (null new-plist) (remhash sym *symbol-plists*)
      (setf (gethash sym *symbol-plists*) new-plist))
  new-plist)
(defun %plist-get (plist key default)
  (cond ((null plist) default)
        ((eq (car plist) key) (car (cdr plist)))
        (t (%plist-get (cdr (cdr plist)) key default))))
(defun %plist-set (plist key val)
  (cond ((null plist) (list key val))
        ((eq (car plist) key) (cons key (cons val (cdr (cdr plist)))))
        (t (cons (car plist) (cons (car (cdr plist))
                                  (%plist-set (cdr (cdr plist)) key val))))))
(defun %plist-remove (plist key)
  (cond ((null plist) (values nil nil))
        ((eq (car plist) key) (values (cdr (cdr plist)) t))
        (t (multiple-value-bind (rest found)
               (%plist-remove (cdr (cdr plist)) key)
             (values (cons (car plist) (cons (car (cdr plist)) rest)) found)))))
(defun get (sym indicator &optional default)
  (%plist-get (symbol-plist sym) indicator default))
(defun (setf get) (new-value sym indicator)
  (setf (symbol-plist sym) (%plist-set (symbol-plist sym) indicator new-value))
  new-value)
(defun remprop (sym indicator)
  (multiple-value-bind (new found)
      (%plist-remove (symbol-plist sym) indicator)
    (when found (setf (symbol-plist sym) new))
    found))
"#;

    #[test]
    fn property_list_get_set_remprop() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(PLIST_PRELUDE).unwrap();
        // Initial state — absent key returns nil
        assert_eq!(s.eval("(get 'foo 'color)").unwrap(), "nil");
        // Absent key with explicit default — symbols print uppercase in NCL
        assert_eq!(s.eval("(get 'foo 'color 'red)").unwrap(), "RED");
        // Set a property
        s.eval("(setf (get 'foo 'color) 'blue)").unwrap();
        assert_eq!(s.eval("(get 'foo 'color)").unwrap(), "BLUE");
        // Second property — first still intact
        s.eval("(setf (get 'foo 'size) 42)").unwrap();
        assert_eq!(s.eval("(get 'foo 'size)").unwrap(), "42");
        assert_eq!(s.eval("(get 'foo 'color)").unwrap(), "BLUE");
        // Overwrite existing property
        s.eval("(setf (get 'foo 'color) 'green)").unwrap();
        assert_eq!(s.eval("(get 'foo 'color)").unwrap(), "GREEN");
        // remprop returns T when found (printed uppercase), nil when absent
        assert_eq!(s.eval("(remprop 'foo 'color)").unwrap(), "T");
        assert_eq!(s.eval("(get 'foo 'color)").unwrap(), "nil");
        assert_eq!(s.eval("(get 'foo 'size)").unwrap(), "42");
        assert_eq!(s.eval("(remprop 'foo 'color)").unwrap(), "nil");
    }

    #[test]
    fn prog1_returns_first_value() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defmacro prog1 (fst &rest more) (let ((g (gensym \"P1\"))) `(let ((,g ,fst)) ,@more ,g)))").unwrap();
        assert_eq!(
            s.eval("(let ((x 0)) (prog1 (progn (setq x (+ x 1)) x) (setq x (+ x 1)) (setq x (+ x 1))))").unwrap(),
            "1"
        );
    }

    #[test]
    fn prog2_returns_second_value() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defmacro prog2 (f1 f2 &rest more) (let ((g (gensym \"P2\"))) `(progn ,f1 (let ((,g ,f2)) ,@more ,g))))").unwrap();
        assert_eq!(
            s.eval("(prog2 100 (+ 1 2) 200)").unwrap(),
            "3"
        );
    }

    /// Inline the core destructuring-bind helpers for testing.
    const DBB_PRELUDE: &str = r#"
(defun %dbb-lambda-keyword-p (x)
  (member x '(&optional &rest &key &allow-other-keys &aux &body &whole &environment)))
(defun %dbb-expand (pattern form-sym body)
  (cond
    ((null pattern) `(progn ,@body))
    ((symbolp pattern) `(let ((,pattern ,form-sym)) ,@body))
    ((consp pattern) (%dbb-list-expand pattern form-sym body 0))
    (t (error "bad dbb pattern ~S" pattern))))
(defun %dbb-list-expand (pattern form-sym body index)
  (cond
    ((null pattern) `(progn ,@body))
    ((atom pattern) `(let ((,pattern (nthcdr ,index ,form-sym))) ,@body))
    ((eq (car pattern) '&rest)
     `(let ((,(cadr pattern) (nthcdr ,index ,form-sym))) ,@body))
    ((eq (car pattern) '&body)
     `(let ((,(cadr pattern) (nthcdr ,index ,form-sym))) ,@body))
    ((eq (car pattern) '&optional)
     (%dbb-opt-expand (cdr pattern) form-sym body index))
    ((eq (car pattern) '&key)
     (%dbb-key-expand (cdr pattern) form-sym body index))
    ((%dbb-lambda-keyword-p (car pattern)) `(progn ,@body))
    ((consp (car pattern))
     (let ((sub (gensym "DBB-N"))
           (rest-code (%dbb-list-expand (cdr pattern) form-sym body (1+ index))))
       `(let ((,sub (nth ,index ,form-sym)))
          ,(%dbb-expand (car pattern) sub (list rest-code)))))
    (t `(let ((,(car pattern) (nth ,index ,form-sym)))
          ,(%dbb-list-expand (cdr pattern) form-sym body (1+ index))))))
(defun %dbb-opt-expand (opts form-sym body index)
  (cond
    ((null opts) `(progn ,@body))
    ((%dbb-lambda-keyword-p (car opts)) (%dbb-list-expand opts form-sym body index))
    (t (let* ((opt (car opts))
              (sym (if (consp opt) (car opt) opt))
              (def (if (consp opt) (cadr opt) nil)))
         `(let ((,sym (if (nthcdr ,index ,form-sym) (nth ,index ,form-sym) ,def)))
            ,(%dbb-opt-expand (cdr opts) form-sym body (1+ index)))))))
(defun %dbb-key-expand (keys form-sym body index)
  (cond
    ((null keys) `(progn ,@body))
    ((%dbb-lambda-keyword-p (car keys)) `(progn ,@body))
    (t (let* ((key-spec (car keys))
              (sym (if (consp key-spec) (car key-spec) key-spec))
              (def (if (consp key-spec) (cadr key-spec) nil))
              (key-kw (intern (string-concat ":" (symbol-name sym)))))
         `(let ((,sym (let ((tail (member ',key-kw (nthcdr ,index ,form-sym))))
                        (if tail (cadr tail) ,def))))
            ,(%dbb-key-expand (cdr keys) form-sym body index))))))
(defmacro destructuring-bind (pattern form &body body)
  (let ((g (gensym "DBB")))
    `(let ((,g ,form))
       ,(%dbb-expand pattern g body))))
"#;

    #[test]
    fn destructuring_bind_required() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(DBB_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(destructuring-bind (a b c) '(1 2 3) (list a b c))").unwrap(),
            "(1 2 3)"
        );
    }

    #[test]
    fn destructuring_bind_rest() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(DBB_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(destructuring-bind (a &rest r) '(1 2 3 4) (list a r))").unwrap(),
            "(1 (2 3 4))"
        );
    }

    #[test]
    fn destructuring_bind_optional_default() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(DBB_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(destructuring-bind (a &optional (b 99)) '(1) (list a b))").unwrap(),
            "(1 99)"
        );
    }

    #[test]
    fn destructuring_bind_nested() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(DBB_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(destructuring-bind ((x y) z) '((10 20) 30) (list x y z))").unwrap(),
            "(10 20 30)"
        );
    }

    #[test]
    fn destructuring_bind_dotted_rest() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(DBB_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(destructuring-bind (a . r) '(1 2 3) (list a r))").unwrap(),
            "(1 (2 3))"
        );
    }

    // ── boole / derived bitwise ops ──────────────────────────────────────────

    const BOOLE_PRELUDE: &str = r#"
(defun logandc1 (i1 i2) (logand (lognot i1) i2))
(defun logandc2 (i1 i2) (logand i1 (lognot i2)))
(defun logorc1  (i1 i2) (logior (lognot i1) i2))
(defun logorc2  (i1 i2) (logior i1 (lognot i2)))
(defun lognor   (i1 i2) (lognot (logior i1 i2)))
(defun logeqv   (i1 i2) (lognot (logxor i1 i2)))
(defun lognand  (i1 i2) (lognot (logand i1 i2)))
(defconstant boole-clr   0)
(defconstant boole-and   1)
(defconstant boole-andc1 2)
(defconstant boole-2     3)
(defconstant boole-andc2 4)
(defconstant boole-1     5)
(defconstant boole-xor   6)
(defconstant boole-ior   7)
(defconstant boole-nor   8)
(defconstant boole-eqv   9)
(defconstant boole-c1    10)
(defconstant boole-orc1  11)
(defconstant boole-c2    12)
(defconstant boole-orc2  13)
(defconstant boole-nand  14)
(defconstant boole-set   15)
(defun boole (op i1 i2)
  (unless (integerp i1) (error "boole: not integer"))
  (unless (integerp i2) (error "boole: not integer"))
  (case op
    (0  0)
    (1  (logand   i1 i2))
    (2  (logandc1 i1 i2))
    (3  i2)
    (4  (logandc2 i1 i2))
    (5  i1)
    (6  (logxor  i1 i2))
    (7  (logior  i1 i2))
    (8  (lognor  i1 i2))
    (9  (logeqv  i1 i2))
    (10 (lognot  i1))
    (11 (logorc1 i1 i2))
    (12 (lognot  i2))
    (13 (logorc2 i1 i2))
    (14 (lognand i1 i2))
    (15 -1)
    (otherwise (error "boole: bad op"))))
"#;

    #[test]
    fn boole_basic_ops() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BOOLE_PRELUDE).unwrap();
        // boole-clr always returns 0
        assert_eq!(s.eval("(boole 0 #xF0 #x0F)").unwrap(), "0");
        // boole-and: #xF0 & #x0F = 0
        assert_eq!(s.eval("(boole 1 #xF0 #x0F)").unwrap(), "0");
        // boole-ior: #xF0 | #x0F = 255
        assert_eq!(s.eval("(boole 7 #xF0 #x0F)").unwrap(), "255");
        // boole-xor: #xFF ^ #x0F = #xF0 = 240
        assert_eq!(s.eval("(boole 6 #xFF #x0F)").unwrap(), "240");
        // boole-set always returns -1
        assert_eq!(s.eval("(boole 15 0 0)").unwrap(), "-1");
        // boole-1 returns i1
        assert_eq!(s.eval("(boole 5 42 99)").unwrap(), "42");
        // boole-2 returns i2
        assert_eq!(s.eval("(boole 3 42 99)").unwrap(), "99");
    }

    #[test]
    fn boole_derived_ops() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BOOLE_PRELUDE).unwrap();
        // lognand: (lognot (logand #xF0 #xFF)) = (lognot #xF0) = -241
        assert_eq!(s.eval("(lognand #xF0 #xFF)").unwrap(), "-241");
        // lognor: (lognot (logior #xF0 #x0F)) = (lognot #xFF) = -256
        assert_eq!(s.eval("(lognor #xF0 #x0F)").unwrap(), "-256");
        // logeqv: (lognot (logxor #xFF #x0F)) = (lognot #xF0) = -241
        assert_eq!(s.eval("(logeqv #xFF #x0F)").unwrap(), "-241");
        // logandc1: (logand (lognot #xF0) #xFF) = 15
        assert_eq!(s.eval("(logandc1 #xF0 #xFF)").unwrap(), "15");
        // logandc2: (logand #xFF (lognot #x0F)) = 240
        assert_eq!(s.eval("(logandc2 #xFF #x0F)").unwrap(), "240");
        // boole-eqv via boole fn
        assert_eq!(s.eval("(boole 9 #xFF #x0F)").unwrap(), "-241");
        // boole-nand
        assert_eq!(s.eval("(boole 14 #xF0 #xFF)").unwrap(), "-241");
    }

    #[test]
    fn boole_constant_values() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BOOLE_PRELUDE).unwrap();
        assert_eq!(s.eval("boole-clr").unwrap(),  "0");
        assert_eq!(s.eval("boole-set").unwrap(),  "15");
        assert_eq!(s.eval("boole-and").unwrap(),  "1");
        assert_eq!(s.eval("boole-ior").unwrap(),  "7");
        assert_eq!(s.eval("boole-xor").unwrap(),  "6");
        assert_eq!(s.eval("boole-nor").unwrap(),  "8");
        assert_eq!(s.eval("boole-eqv").unwrap(),  "9");
        assert_eq!(s.eval("boole-nand").unwrap(), "14");
    }

    // ── SET / PROGV / ASSERT / REMF / COERCE fixes ───────────────────────────

    // SET preludes (symbols.lisp defs inlined)
    const MISC_PRELUDE: &str = r#"
(defvar *symbol-plists* (make-hash-table :test 'eq))
(defun symbol-plist (sym) (gethash sym *symbol-plists*))
(defun (setf symbol-plist) (new-plist sym)
  (if (null new-plist)
      (remhash sym *symbol-plists*)
      (setf (gethash sym *symbol-plists*) new-plist))
  new-plist)
(defun %plist-remove (plist key)
  (cond
    ((null plist) (values nil nil))
    ((eq (car plist) key) (values (cdr (cdr plist)) t))
    (t (multiple-value-bind (rest found)
           (%plist-remove (cdr (cdr plist)) key)
         (values (cons (car plist) (cons (car (cdr plist)) rest)) found)))))
(defmacro remf (place indicator)
  (let ((ind-g (gensym "IND")) (new-g (gensym "NEW")) (fnd-g (gensym "FND")))
    `(let ((,ind-g ,indicator))
       (multiple-value-bind (,new-g ,fnd-g)
           (%plist-remove ,place ,ind-g)
         (when ,fnd-g (setf ,place ,new-g))
         ,fnd-g))))
(defmacro assert (test-form &optional places &rest message-args)
  (declare (ignore places))
  (if message-args
      `(unless ,test-form (error (format nil ,@message-args)))
      `(unless ,test-form (error "assertion failed: ~S" ',test-form))))
(defmacro progv (symbols values &body body)
  (let ((syms-g (gensym "PVSY")) (vals-g (gensym "PVVL"))
        (old-g (gensym "PVOLD")) (result-g (gensym "PVRES")))
    `(let* ((,syms-g ,symbols)
            (,vals-g ,values)
            (,old-g  (mapcar #'symbol-value ,syms-g))
            (,result-g (progn (mapc #'set ,syms-g ,vals-g) ,@body)))
       (mapc #'set ,syms-g ,old-g)
       ,result-g)))
"#;

    #[test]
    fn set_native_writes_value() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defvar *x* 10)").unwrap();
        s.eval("(set '*x* 42)").unwrap();
        assert_eq!(s.eval("*x*").unwrap(), "42");
    }

    #[test]
    fn progv_binds_and_restores() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(MISC_PRELUDE).unwrap();
        s.eval("(defvar *a* 1)").unwrap();
        s.eval("(defvar *b* 2)").unwrap();
        // inside progv the values should be rebound
        assert_eq!(
            s.eval("(progv '(*a* *b*) '(10 20) (list *a* *b*))").unwrap(),
            "(10 20)"
        );
        // after exit the originals are restored
        assert_eq!(s.eval("(list *a* *b*)").unwrap(), "(1 2)");
    }

    #[test]
    fn assert_passes_when_true() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(MISC_PRELUDE).unwrap();
        // assert on a true form returns nil (no error)
        assert_eq!(s.eval("(assert (= 1 1))").unwrap(), "nil");
    }

    #[test]
    fn assert_fires_when_false() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(MISC_PRELUDE).unwrap();
        // Wrap in handler-case so the error is caught inside Lisp, not as a
        // process-fatal unhandled condition.  Note: NCL's handler-case
        // requires a non-nil binding list — use a named-but-ignored variable.
        assert_eq!(
            s.eval("(handler-case (progn (assert (= 1 2)) 'no-error) \
                      (error (e) e 'caught-error))").unwrap(),
            "CAUGHT-ERROR"
        );
    }

    #[test]
    fn assert_custom_message() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(MISC_PRELUDE).unwrap();
        // Just confirm that assert with a custom message signals an error.
        assert_eq!(
            s.eval("(handler-case \
                      (assert nil nil \"boom ~A\" 42) \
                      (error (e) e 'error-was-signalled))").unwrap(),
            "ERROR-WAS-SIGNALLED"
        );
    }

    #[test]
    fn remf_removes_key_from_plist() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(MISC_PRELUDE).unwrap();
        s.eval("(defvar *pl* '(:a 1 :b 2 :c 3))").unwrap();
        // remove :b
        s.eval("(remf *pl* ':b)").unwrap();
        // :b gone, :a and :c still there
        assert_eq!(s.eval("(getf *pl* ':a)").unwrap(), "1");
        assert_eq!(s.eval("(getf *pl* ':c)").unwrap(), "3");
        assert_eq!(s.eval("(getf *pl* ':b)").unwrap(), "nil");
    }

    #[test]
    fn coerce_vector_to_list() {
        let mut s = Session::with_stdlib().unwrap();
        // build a vector, coerce to list
        assert_eq!(
            s.eval("(coerce (vector 1 2 3) 'list)").unwrap(),
            "(1 2 3)"
        );
    }

    #[test]
    fn coerce_list_to_vector() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(length (coerce '(10 20 30) 'vector))").unwrap(),
            "3"
        );
    }

    #[test]
    fn coerce_identity() {
        let mut s = Session::with_stdlib().unwrap();
        // string → string: identity
        assert_eq!(s.eval("(coerce \"hello\" 'string)").unwrap(), "\"hello\"");
        // list → list: identity
        assert_eq!(s.eval("(coerce '(1 2) 'list)").unwrap(), "(1 2)");
    }

    // ── etypecase / ctypecase ─────────────────────────────────────────────────

    #[test]
    fn etypecase_matches_type() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(etypecase 42 (string 'str) (integer 'int) (t 'other))").unwrap(),
            "INT"
        );
        assert_eq!(
            s.eval("(etypecase \"hi\" (string 'str) (integer 'int))").unwrap(),
            "STR"
        );
    }

    #[test]
    fn etypecase_signals_on_no_match() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(handler-case \
                      (etypecase 3.14 (string 'str) (integer 'int)) \
                      (error (e) e 'no-match))").unwrap(),
            "NO-MATCH"
        );
    }

    // ── ignore-errors ─────────────────────────────────────────────────────────

    // ignore-errors needs the symbols prelude for handler-case to work
    const IGNORE_ERRORS_PRELUDE: &str = r#"
(defmacro ignore-errors (&body body)
  (let ((e-g (gensym "IE")))
    `(handler-case (progn ,@body)
       (error (,e-g) (values nil ,e-g)))))
"#;

    #[test]
    fn ignore_errors_passes_through_success() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(IGNORE_ERRORS_PRELUDE).unwrap();
        assert_eq!(s.eval("(ignore-errors (+ 1 2))").unwrap(), "3");
    }

    #[test]
    fn ignore_errors_catches_error() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(IGNORE_ERRORS_PRELUDE).unwrap();
        // On error: first value is nil
        assert_eq!(
            s.eval("(car (multiple-value-list \
                      (ignore-errors (error \"oops\"))))").unwrap(),
            "nil"
        );
    }

    // ── copy-symbol ───────────────────────────────────────────────────────────

    const COPY_SYMBOL_PRELUDE: &str = r#"
(defvar *symbol-plists* (make-hash-table :test 'eq))
(defun symbol-plist (sym) (gethash sym *symbol-plists*))
(defun (setf symbol-plist) (new-plist sym)
  (if (null new-plist) (remhash sym *symbol-plists*)
      (setf (gethash sym *symbol-plists*) new-plist))
  new-plist)
(defun copy-symbol (sym &optional copy-props)
  (let ((new-sym (make-symbol (symbol-name sym))))
    (when copy-props
      (when (boundp sym)
        (set new-sym (symbol-value sym)))
      (when (fboundp sym)
        (setf (symbol-function new-sym) (symbol-function sym)))
      (let ((plist (symbol-plist sym)))
        (when plist (setf (symbol-plist new-sym) plist))))
    new-sym))
"#;

    #[test]
    fn copy_symbol_same_name() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(COPY_SYMBOL_PRELUDE).unwrap();
        // NCL's make-symbol can't create truly uninterned symbols: the copy
        // gets a unique name that STARTS WITH the original name.
        assert_eq!(
            s.eval("(let ((c (copy-symbol 'foo))) \
                      (and (not (eq c 'foo)) \
                           (> (length (symbol-name c)) 0)))").unwrap(),
            "T"
        );
        // The copy is a different object than the original
        assert_eq!(
            s.eval("(eq (copy-symbol 'foo) 'foo)").unwrap(),
            "nil"
        );
    }

    #[test]
    fn copy_symbol_copies_value() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(COPY_SYMBOL_PRELUDE).unwrap();
        s.eval("(defvar *my-sym* 99)").unwrap();
        assert_eq!(
            s.eval("(symbol-value (copy-symbol '*my-sym* t))").unwrap(),
            "99"
        );
    }

    // ── bits.lisp (byte / ldb / dpb / mask-field / deposit-field) ────────────

    const BITS_PRELUDE: &str = r#"
(defun byte (size position) (cons size position))
(defun byte-size (bs) (car bs))
(defun byte-position (bs) (cdr bs))
(defun ldb (bytespec integer)
  (let ((mask (- (ash 1 (byte-size bytespec)) 1)))
    (logand (ash integer (- (byte-position bytespec))) mask)))
(defun ldb-test (bytespec integer) (not (zerop (ldb bytespec integer))))
(defun dpb (newbyte bytespec integer)
  (let* ((size (byte-size bytespec))
         (pos  (byte-position bytespec))
         (mask (ash (- (ash 1 size) 1) pos)))
    (logior (logand integer (lognot mask))
            (logand (ash newbyte pos) mask))))
(defun mask-field (bytespec integer) (logand integer (dpb -1 bytespec 0)))
(defun deposit-field (newbyte bytespec integer)
  (let ((mask (mask-field bytespec -1)))
    (logior (logand newbyte mask) (logand integer (lognot mask)))))
"#;

    #[test]
    fn byte_ldb_dpb_basics() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BITS_PRELUDE).unwrap();
        // ldb: extract bits 4-7 from #xABCD (= 43981):
        //   nibble at pos 4 = 0xC = 12
        assert_eq!(s.eval("(ldb (byte 4 4) #xABCD)").unwrap(), "12");
        // ldb: low nibble of #xABCD = 0xD = 13
        assert_eq!(s.eval("(ldb (byte 4 0) #xABCD)").unwrap(), "13");
        // dpb: replace low nibble of #xABCD with 0 → #xABC0 = 43968
        assert_eq!(s.eval("(dpb 0 (byte 4 0) #xABCD)").unwrap(), "43968");
        // dpb: replace low nibble with 7 → #xABC7 = 43975
        assert_eq!(s.eval("(dpb 7 (byte 4 0) #xABCD)").unwrap(), "43975");
    }

    #[test]
    fn byte_ldb_dpb_roundtrip() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BITS_PRELUDE).unwrap();
        // dpb/ldb round-trip: put 7 into bits 8-11, read it back
        assert_eq!(
            s.eval("(ldb (byte 4 8) (dpb 7 (byte 4 8) 0))").unwrap(),
            "7"
        );
        // ldb-test: true when bits are set
        assert_eq!(s.eval("(ldb-test (byte 4 4) #xFF)").unwrap(), "T");
        assert_eq!(s.eval("(ldb-test (byte 4 4) #x000F)").unwrap(), "nil");
    }

    #[test]
    fn mask_field_deposit_field() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(BITS_PRELUDE).unwrap();
        // mask-field: keep only bits 4-7 of #xFF
        assert_eq!(s.eval("(mask-field (byte 4 4) #xFF)").unwrap(), "240"); // #xF0
        // deposit-field: replace bits 4-7 of #xFF with bits 4-7 of #x50
        // #x50 bits 4-7 = 5; result = (#xFF & ~#xF0) | (#x50 & #xF0) = 0xF | 0x50 = 0x5F = 95
        assert_eq!(s.eval("(deposit-field #x50 (byte 4 4) #xFF)").unwrap(), "95");
    }

    // ── check-type ────────────────────────────────────────────────────────────

    const CHECK_TYPE_PRELUDE: &str = r#"
(defmacro check-type (place typespec &optional string)
  (let ((val-g (gensym "CT")))
    `(let ((,val-g ,place))
       (unless (typep ,val-g ',typespec)
         (error ,(if string
                     `(format nil "The value of ~S, ~~S, is not ~A." ',place ,string)
                     `(format nil "The value of ~S, ~~S, is not of type ~A."
                              ',place ',typespec))
                ,val-g)))))
"#;

    #[test]
    fn check_type_passes() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(CHECK_TYPE_PRELUDE).unwrap();
        // check-type on a matching type returns nil silently
        assert_eq!(s.eval("(check-type 42 integer)").unwrap(), "nil");
        assert_eq!(s.eval("(check-type \"hi\" string)").unwrap(), "nil");
    }

    #[test]
    fn check_type_signals_on_mismatch() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(CHECK_TYPE_PRELUDE).unwrap();
        assert_eq!(
            s.eval("(handler-case \
                      (check-type \"hi\" integer) \
                      (error (e) e 'type-error-signalled))").unwrap(),
            "TYPE-ERROR-SIGNALLED"
        );
    }

    // ── ffloor / fceiling / ftruncate / fround / asinh / acosh / atanh ───────

    const NUMBERS_EXT_PRELUDE: &str = r#"
(defun ffloor (number &optional (divisor 1))
  (let* ((pair (multiple-value-list (floor number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))
(defun fceiling (number &optional (divisor 1))
  (let* ((pair (multiple-value-list (ceiling number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))
(defun ftruncate (number &optional (divisor 1))
  (let* ((pair (multiple-value-list (truncate number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))
(defun fround (number &optional (divisor 1))
  (let* ((pair (multiple-value-list (round number divisor)))
         (q    (car pair))
         (r    (cadr pair)))
    (values (* 1.0 q) r)))
(defun asinh (x) (log (+ x (sqrt (+ 1.0 (* x x))))))
(defun acosh (x) (log (+ x (sqrt (- (* x x) 1.0)))))
(defun atanh (x) (/ (log (/ (+ 1.0 x) (- 1.0 x))) 2.0))
"#;

    #[test]
    fn ffloor_mv_baseline() {
        // Baseline: does multiple-value-list work on floor at ALL?
        let mut s = Session::with_stdlib().unwrap();
        // Step 1: direct values cadr
        assert_eq!(s.eval("(cadr (multiple-value-list (values 3 1)))").unwrap(), "1", "mvl values 3 1");
        // Step 2: custom simple function
        s.eval("(defun %two-vals () (values 3 1))").unwrap();
        assert_eq!(s.eval("(cadr (multiple-value-list (%two-vals)))").unwrap(), "1", "two-vals cadr");
        // Step 3: multiple-value-bind on floor
        assert_eq!(s.eval("(multiple-value-bind (q r) (floor 7 2) (list q r))").unwrap(), "(3 1)", "mvb floor");
        // Step 4: floor cadr via mvl
        assert_eq!(s.eval("(car (multiple-value-list (floor 7 2)))").unwrap(), "3", "floor car mvl");
        assert_eq!(s.eval("(cadr (multiple-value-list (floor 7 2)))").unwrap(), "1", "floor cadr mvl");
    }

    #[test]
    fn ffloor_returns_float_quotient() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(NUMBERS_EXT_PRELUDE).unwrap();
        // (ffloor 7 2) → (values 3.0 1)
        assert_eq!(
            s.eval("(let ((p (multiple-value-list (ffloor 7 2)))) (list (car p) (cadr p)))").unwrap(),
            "(3.0 1)"
        );
        // Remainder for negative: (ffloor -7 2) → (values -4.0 1)
        assert_eq!(
            s.eval("(let ((p (multiple-value-list (ffloor -7 2)))) (car p))").unwrap(),
            "-4.0"
        );
    }

    #[test]
    fn fceiling_and_ftruncate() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(NUMBERS_EXT_PRELUDE).unwrap();
        // (fceiling 7 2) → (values 4.0 -1)
        assert_eq!(
            s.eval("(car (multiple-value-list (fceiling 7 2)))").unwrap(),
            "4.0"
        );
        // (ftruncate -7 2) → (values -3.0 -1)
        assert_eq!(
            s.eval("(car (multiple-value-list (ftruncate -7 2)))").unwrap(),
            "-3.0"
        );
    }

    #[test]
    fn inverse_hyperbolic_roundtrip() {
        let mut s = Session::with_stdlib().unwrap();
        s.eval(NUMBERS_EXT_PRELUDE).unwrap();
        // sinh(asinh(x)) ≈ x for x = 1.5
        let v = s.eval("(let ((x 1.5)) (- (sinh (asinh x)) x))").unwrap();
        let diff: f64 = v.trim().parse().unwrap();
        assert!(diff.abs() < 1e-10, "sinh(asinh(1.5)) roundtrip error: {diff}");
        // cosh(acosh(x)) ≈ x for x = 2.0
        let v2 = s.eval("(let ((x 2.0)) (- (cosh (acosh x)) x))").unwrap();
        let diff2: f64 = v2.trim().parse().unwrap();
        assert!(diff2.abs() < 1e-10, "cosh(acosh(2.0)) roundtrip error: {diff2}");
        // tanh(atanh(x)) ≈ x for x = 0.5
        let v3 = s.eval("(let ((x 0.5)) (- (tanh (atanh x)) x))").unwrap();
        let diff3: f64 = v3.trim().parse().unwrap();
        assert!(diff3.abs() < 1e-10, "tanh(atanh(0.5)) roundtrip error: {diff3}");
    }

    // ── CL Printer Names ────────────────────────────────────────────

    #[test]
    fn prin1_to_string_readable() {
        let mut s = Session::with_stdlib().unwrap();
        // Returns a string type.
        assert_eq!(s.eval("(stringp (prin1-to-string 42))").unwrap(), "T");
        // Content matches readable (~S) form.
        assert_eq!(s.eval(r#"(equal (prin1-to-string 42) "42")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(equal (prin1-to-string 'foo) "FOO")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(equal (prin1-to-string '(1 2)) "(1 2)")"#).unwrap(), "T");
    }

    #[test]
    fn princ_to_string_aesthetic() {
        let mut s = Session::with_stdlib().unwrap();
        // Strings without quotes in princ output.
        assert_eq!(s.eval(r#"(equal (princ-to-string "hello") "hello")"#).unwrap(), "T");
        // Characters as raw glyphs.
        assert_eq!(s.eval(r#"(equal (princ-to-string #\A) "A")"#).unwrap(), "T");
    }

    #[test]
    fn write_to_string_default_readable() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(equal (write-to-string 42) "42")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(equal (write-to-string '(1 2 3)) "(1 2 3)")"#).unwrap(), "T");
    }

    #[test]
    fn prin1_returns_object() {
        let mut s = Session::with_stdlib().unwrap();
        // prin1 returns its object argument, not the string.
        assert_eq!(s.eval("(prin1 42)").unwrap(), "42");
        assert_eq!(s.eval("(prin1 'foo)").unwrap(), "FOO");
    }

    #[test]
    fn princ_returns_object() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(princ 42)").unwrap(), "42");
        assert_eq!(s.eval(r#"(princ "hi")"#).unwrap(), r#""hi""#);
    }

    #[test]
    fn print_returns_object() {
        let mut s = Session::with_stdlib().unwrap();
        // print outputs newline + readable + space, returns the object.
        assert_eq!(s.eval("(print 42)").unwrap(), "42");
    }

    #[test]
    fn write_returns_object() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(write 99)").unwrap(), "99");
    }

    #[test]
    fn write_char_returns_char() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(write-char #\\X)").unwrap(), "#\\X");
    }

    #[test]
    fn terpri_returns_nil() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(terpri)").unwrap(), "nil");
    }

    #[test]
    fn fresh_line_returns_t() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(fresh-line)").unwrap(), "T");
    }

    #[test]
    fn standard_output_is_t() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("*standard-output*").unwrap(), "T");
        assert_eq!(s.eval("*standard-input*").unwrap(), "T");
        assert_eq!(s.eval("*error-output*").unwrap(), "T");
    }

    #[test]
    fn print_control_vars_exist() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("*print-escape*").unwrap(), "T");
        assert_eq!(s.eval("*print-base*").unwrap(), "10");
        assert_eq!(s.eval("*print-pretty*").unwrap(), "nil");
    }

    #[test]
    fn write_string_returns_string() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(write-string "test")"#).unwrap(),
            r#""test""#,
        );
    }

    #[test]
    fn write_line_returns_string() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(write-line "test")"#).unwrap(),
            r#""test""#,
        );
    }

    // ── eval & read-from-string ─────────────────────────────────────

    #[test]
    fn eval_simple_expression() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate(); // eval needs an active session
        // eval of a quoted form yields the value.
        assert_eq!(s.eval("(eval '(+ 1 2))").unwrap(), "3");
        assert_eq!(s.eval("(eval ''foo)").unwrap(), "FOO");
    }

    #[test]
    fn eval_returns_actual_value() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // eval returns a number, not a string.
        assert_eq!(s.eval("(numberp (eval '(+ 10 20)))").unwrap(), "T");
        assert_eq!(s.eval("(+ 1 (eval '(* 3 4)))").unwrap(), "13");
    }

    #[test]
    fn eval_list_construction() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(s.eval("(eval '(list 1 2 3))").unwrap(), "(1 2 3)");
    }

    #[test]
    fn read_from_string_atom() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate(); // read-from-string also needs active session
        assert_eq!(s.eval(r#"(read-from-string "42")"#).unwrap(), "42");
        assert_eq!(s.eval(r#"(read-from-string "FOO")"#).unwrap(), "FOO");
        assert_eq!(s.eval(r#"(numberp (read-from-string "42"))"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(symbolp (read-from-string "FOO"))"#).unwrap(), "T");
    }

    #[test]
    fn read_from_string_list() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(
            s.eval(r#"(read-from-string "(1 2 3)")"#).unwrap(),
            "(1 2 3)",
        );
        assert_eq!(
            s.eval(r#"(car (read-from-string "(a b c)"))"#).unwrap(),
            "A",
        );
    }

    #[test]
    fn eval_of_read_from_string() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // The canonical REPL loop: read → eval.
        assert_eq!(
            s.eval(r#"(eval (read-from-string "(+ 10 20)"))"#).unwrap(),
            "30",
        );
    }

    // ── EQUAL hash tables ───────────────────────────────────────────

    #[test]
    fn equal_hash_table_string_keys() {
        let mut s = Session::with_stdlib().unwrap();
        // EQUAL table: two distinct string objects with the same text
        // should hash to the same bucket and match on lookup.
        s.eval(r#"(defparameter *ht* (make-hash-table :test 'equal))"#).unwrap();
        s.eval(r#"(setf (gethash "hello" *ht*) 42)"#).unwrap();
        // Look up with a fresh string — must still find it.
        assert_eq!(
            s.eval(r#"(gethash (format nil "~A" "hello") *ht*)"#).unwrap(),
            "42",
        );
    }

    #[test]
    fn eql_hash_table_symbol_keys() {
        let mut s = Session::with_stdlib().unwrap();
        // EQL table: interned symbols are EQ, so lookup works.
        s.eval("(defparameter *ht* (make-hash-table :test 'eql))").unwrap();
        s.eval("(setf (gethash 'foo *ht*) 99)").unwrap();
        assert_eq!(s.eval("(gethash 'foo *ht*)").unwrap(), "99");
    }

    #[test]
    fn hash_table_p_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(hash-table-p (make-hash-table))").unwrap(), "T");
        assert_eq!(s.eval("(hash-table-p '(1 2 3))").unwrap(), "nil");
        assert_eq!(s.eval("(hash-table-p 42)").unwrap(), "nil");
    }

    #[test]
    fn string_equal_case_insensitive() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(string-equal "Hello" "hello")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string-equal "ABC" "abc")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string-equal "abc" "def")"#).unwrap(), "nil");
    }

    #[test]
    fn equalp_basics() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(equalp "Hello" "hello")"#).unwrap(), "T");
        assert_eq!(s.eval("(equalp 1 1.0)").unwrap(), "T");
        assert_eq!(s.eval("(equalp '(1 2) '(1 2))").unwrap(), "T");
        assert_eq!(s.eval(r#"(equalp "abc" "def")"#).unwrap(), "nil");
    }

    // ── String functions ────────────────────────────────────────────

    #[test]
    fn string_eq_and_neq() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(string= "abc" "abc")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string= "abc" "ABC")"#).unwrap(), "nil");
        assert_eq!(s.eval(r#"(string/= "abc" "def")"#).unwrap(), "T");
    }

    #[test]
    fn string_order() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(string< "abc" "abd")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string> "abd" "abc")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string<= "abc" "abc")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string>= "abc" "abc")"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(string< "abc" "abc")"#).unwrap(), "nil");
    }

    #[test]
    fn string_upcase_downcase() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(equal (string-upcase "hello") "HELLO")"#).unwrap(), "T",
        );
        assert_eq!(
            s.eval(r#"(equal (string-downcase "HELLO") "hello")"#).unwrap(), "T",
        );
    }

    #[test]
    fn string_trim_functions() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(equal (string-trim " " "  hello  ") "hello")"#).unwrap(), "T",
        );
        assert_eq!(
            s.eval(r#"(equal (string-left-trim " " "  hello  ") "hello  ")"#).unwrap(), "T",
        );
        assert_eq!(
            s.eval(r#"(equal (string-right-trim " " "  hello  ") "  hello")"#).unwrap(), "T",
        );
    }

    #[test]
    fn string_capitalize_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(equal (string-capitalize "hello world") "Hello World")"#).unwrap(), "T",
        );
    }

    #[test]
    fn parse_integer_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(parse-integer "42")"#).unwrap(), "42");
        assert_eq!(s.eval(r#"(parse-integer "-7")"#).unwrap(), "-7");
        assert_eq!(s.eval(r#"(parse-integer "  100  ")"#).unwrap(), "100");
    }

    // ── Sequence functions ──────────────────────────────────────────

    #[test]
    fn count_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(count 3 '(1 2 3 3 4))").unwrap(), "2");
        assert_eq!(s.eval("(count 5 '(1 2 3))").unwrap(), "0");
    }

    #[test]
    fn count_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(count-if #'oddp '(1 2 3 4 5))").unwrap(), "3");
    }

    #[test]
    fn reduce_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(reduce #'+ '(1 2 3 4))").unwrap(), "10");
        assert_eq!(s.eval("(reduce #'+ '(1 2 3) :initial-value 10)").unwrap(), "16");
        assert_eq!(s.eval("(reduce #'* '(2 3 4))").unwrap(), "24");
    }

    #[test]
    fn concatenate_strings() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval(r#"(equal (concatenate 'string "hello" " " "world") "hello world")"#).unwrap(),
            "T",
        );
    }

    #[test]
    fn concatenate_lists() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(concatenate 'list '(1 2) '(3 4))").unwrap(),
            "(1 2 3 4)",
        );
    }

    // ── List utilities ──────────────────────────────────────────────

    #[test]
    fn subst_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(subst 'x 'b '(a b c b))").unwrap(),
            "(A X C X)",
        );
    }

    #[test]
    fn copy_tree_deep_copies() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(equal (copy-tree '(a (b c) d)) '(a (b c) d))").unwrap(),
            "T",
        );
    }

    #[test]
    fn tree_equal_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(tree-equal '(a (b c)) '(a (b c)))").unwrap(),
            "T",
        );
        assert_eq!(
            s.eval("(tree-equal '(a (b c)) '(a (b d)))").unwrap(),
            "nil",
        );
    }

    #[test]
    fn adjoin_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(adjoin 1 '(1 2 3))").unwrap(), "(1 2 3)");
        assert_eq!(s.eval("(adjoin 0 '(1 2 3))").unwrap(), "(0 1 2 3)");
    }

    #[test]
    fn delete_same_as_remove() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(delete 2 '(1 2 3 2 4))").unwrap(), "(1 3 4)");
    }

    // ── Control flow extras ─────────────────────────────────────────

    #[test]
    fn ignore_errors_catches() {
        let mut s = Session::with_stdlib().unwrap();
        // Normal case: returns the value.
        assert_eq!(s.eval("(ignore-errors (+ 1 2))").unwrap(), "3");
    }

    #[test]
    fn complement_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(funcall (complement #'plusp) -5)").unwrap(),
            "T",
        );
        assert_eq!(
            s.eval("(funcall (complement #'plusp) 5)").unwrap(),
            "nil",
        );
    }

    #[test]
    fn constantly_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(funcall (constantly 42) 1 2 3)").unwrap(), "42");
    }

    // ── Batch 3: type predicates ──────────────────────────────────

    #[test]
    fn atom_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(atom 42)").unwrap(), "T");
        assert_eq!(s.eval("(atom 'x)").unwrap(), "T");
        assert_eq!(s.eval("(atom nil)").unwrap(), "T");
        assert_eq!(s.eval("(atom '(1 2))").unwrap(), "nil");
    }

    #[test]
    fn arrayp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(arrayp "hello")"#).unwrap(), "T");
        assert_eq!(s.eval("(arrayp #(1 2 3))").unwrap(), "T");
        assert_eq!(s.eval("(arrayp 42)").unwrap(), "nil");
    }

    // ── Batch 3: number utilities ─────────────────────────────────

    #[test]
    fn gcd_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(gcd 12 8)").unwrap(), "4");
        assert_eq!(s.eval("(gcd 15 25 10)").unwrap(), "5");
        assert_eq!(s.eval("(gcd)").unwrap(), "0");
        assert_eq!(s.eval("(gcd 7)").unwrap(), "7");
    }

    #[test]
    fn lcm_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(lcm 4 6)").unwrap(), "12");
        assert_eq!(s.eval("(lcm)").unwrap(), "1");
        assert_eq!(s.eval("(lcm 5)").unwrap(), "5");
    }

    #[test]
    fn logtest_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(logtest #xFF #x0F)").unwrap(), "T");
        assert_eq!(s.eval("(logtest #xF0 #x0F)").unwrap(), "nil");
    }

    #[test]
    fn logcount_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(logcount #xFF)").unwrap(), "8");
        assert_eq!(s.eval("(logcount 0)").unwrap(), "0");
        assert_eq!(s.eval("(logcount 7)").unwrap(), "3");
    }

    #[test]
    fn logbitp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(logbitp 0 5)").unwrap(), "T");
        assert_eq!(s.eval("(logbitp 1 5)").unwrap(), "nil");
        assert_eq!(s.eval("(logbitp 2 5)").unwrap(), "T");
    }

    #[test]
    fn integer_length_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(integer-length 0)").unwrap(), "0");
        assert_eq!(s.eval("(integer-length 1)").unwrap(), "1");
        assert_eq!(s.eval("(integer-length 7)").unwrap(), "3");
        assert_eq!(s.eval("(integer-length 255)").unwrap(), "8");
    }

    // ── Batch 3: alist utilities ──────────────────────────────────

    #[test]
    fn acons_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(acons 'a 1 '((b . 2)))").unwrap(),
            "((A . 1) (B . 2))",
        );
    }

    #[test]
    fn pairlis_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(pairlis '(a b c) '(1 2 3))").unwrap(),
            "((A . 1) (B . 2) (C . 3))",
        );
    }

    #[test]
    fn rassoc_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(rassoc 2 '((a . 1) (b . 2) (c . 3)))").unwrap(),
            "(B . 2)",
        );
        assert_eq!(
            s.eval("(rassoc 99 '((a . 1)))").unwrap(),
            "nil",
        );
    }

    #[test]
    fn copy_alist_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(copy-alist '((a . 1) (b . 2)))").unwrap(),
            "((A . 1) (B . 2))",
        );
    }

    // ── Batch 3: more sequences ───────────────────────────────────

    #[test]
    fn elt_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(elt '(a b c) 1)").unwrap(), "B");
        assert_eq!(s.eval("(elt #(10 20 30) 2)").unwrap(), "30");
    }

    #[test]
    fn substitute_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(substitute 0 3 '(1 2 3 4 3))").unwrap(),
            "(1 2 0 4 0)",
        );
    }

    #[test]
    fn substitute_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(substitute-if 0 #'oddp '(1 2 3 4 5))").unwrap(),
            "(0 2 0 4 0)",
        );
    }

    #[test]
    fn search_seq_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(search '(3 4) '(1 2 3 4 5))").unwrap(), "2");
        assert_eq!(s.eval("(search '(9) '(1 2 3))").unwrap(), "nil");
    }

    #[test]
    fn mismatch_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(mismatch '(1 2 3) '(1 2 9))").unwrap(), "2");
        assert_eq!(s.eval("(mismatch '(1 2) '(1 2))").unwrap(), "nil");
        assert_eq!(s.eval("(mismatch '(1 2 3) '(1 2))").unwrap(), "2");
    }

    #[test]
    fn mapcan_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(mapcan (lambda (x) (if (oddp x) (list x) nil)) '(1 2 3 4 5))").unwrap(),
            "(1 3 5)",
        );
    }

    #[test]
    fn make_string_works() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(
            s.eval(r#"(equal (make-string 3 :initial-element #\x) "xxx")"#).unwrap(),
            "T",
        );
    }

    // ── Batch 3: control flow ─────────────────────────────────────

    #[test]
    fn nth_value_primary() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // nth-value 0 returns the primary value
        assert_eq!(s.eval("(nth-value 0 (values 42))").unwrap(), "42");
    }

    #[test]
    fn multiple_value_setq_primary() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // multiple-value-setq at least binds the primary value
        assert_eq!(
            s.eval("(let ((a 0)) (multiple-value-setq (a) (+ 10 3)) a)").unwrap(),
            "13",
        );
    }

    #[test]
    fn assert_core_passes() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(assert (= 1 1))").unwrap(), "nil");
    }

    #[test]
    fn check_type_core_passes() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(check-type 42 integer)").unwrap(), "nil");
    }

    // ── Batch 4: list predicates & utilities ──────────────────────

    #[test]
    fn endp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(endp nil)").unwrap(), "T");
        assert_eq!(s.eval("(endp '(1 2))").unwrap(), "nil");
    }

    #[test]
    fn tailp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(let ((x '(1 2 3))) (tailp (cddr x) x))").unwrap(),
            "T",
        );
    }

    #[test]
    fn ldiff_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(let ((x '(1 2 3 4))) (ldiff x (cddr x)))").unwrap(),
            "(1 2)",
        );
    }

    #[test]
    fn nbutlast_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(nbutlast (list 1 2 3 4))").unwrap(),
            "(1 2 3)",
        );
        assert_eq!(
            s.eval("(nbutlast (list 1 2 3) 2)").unwrap(),
            "(1)",
        );
    }

    #[test]
    fn revappend_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(revappend '(1 2 3) '(4 5))").unwrap(),
            "(3 2 1 4 5)",
        );
    }

    #[test]
    fn nsubst_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(nsubst 'z 'a (list 'a (list 'b 'a) 'c))").unwrap(),
            "(Z (B Z) C)",
        );
    }

    #[test]
    fn nsubstitute_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(let ((x (list 1 2 3 2 1))) (nsubstitute 9 2 x) x)").unwrap(),
            "(1 9 3 9 1)",
        );
    }

    // ── Batch 4: char predicates ──────────────────────────────────

    #[test]
    fn alphanumericp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(alphanumericp #\A)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(alphanumericp #\5)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(alphanumericp #\!)"#).unwrap(), "nil");
    }

    #[test]
    fn both_case_p_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(both-case-p #\A)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(both-case-p #\5)"#).unwrap(), "nil");
    }

    #[test]
    fn char_int_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(char-int #\A)"#).unwrap(), "65");
    }

    // ── Batch 4: with-gensyms ─────────────────────────────────────

    #[test]
    fn with_gensyms_works() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // with-gensyms should bind fresh symbols
        assert_eq!(
            s.eval("(with-gensyms (a b) (list (symbolp a) (symbolp b)))").unwrap(),
            "(T T)",
        );
    }

    // ── Batch 5: flet ─────────────────────────────────────────────

    #[test]
    fn flet_basic() {
        let mut s = Session::with_stdlib().unwrap();
        // flet creates local function bindings
        assert_eq!(
            s.eval("(flet ((double (x) (* x 2))) (double 21))").unwrap(),
            "42",
        );
    }

    #[test]
    fn flet_shadows_global() {
        let mut s = Session::with_stdlib().unwrap();
        // flet can shadow a user-defined global function
        s.eval("(defun my-op (a b) (+ a b))").unwrap();
        assert_eq!(
            s.eval("(flet ((my-op (a b) (* a b))) (my-op 6 7))").unwrap(),
            "42",
        );
        // after flet, the global is restored
        assert_eq!(s.eval("(my-op 6 7)").unwrap(), "13");
    }

    #[test]
    fn flet_multiple_bindings() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(flet ((add1 (x) (+ x 1)) (add2 (x) (+ x 2))) (+ (add1 10) (add2 20)))").unwrap(),
            "33",
        );
    }

    // ── Batch 5: labels (mutual recursion!) ───────────────────────

    #[test]
    fn labels_self_recursion() {
        let mut s = Session::with_stdlib().unwrap();
        // labels allows self-recursion in the binding
        assert_eq!(
            s.eval("(labels ((fact (n) (if (<= n 1) 1 (* n (fact (- n 1)))))) (fact 10))").unwrap(),
            "3628800",
        );
    }

    #[test]
    fn labels_mutual_recursion_even_odd() {
        let mut s = Session::with_stdlib().unwrap();
        // The classic mutual recursion test:
        // my-even? calls my-odd? and vice versa
        assert_eq!(
            s.eval("(labels ((my-even (n) (if (= n 0) t (my-odd (- n 1)))) \
                             (my-odd (n) (if (= n 0) nil (my-even (- n 1))))) \
                      (list (my-even 10) (my-odd 10) (my-even 7) (my-odd 7)))").unwrap(),
            "(T nil nil T)",
        );
    }

    #[test]
    fn labels_three_way_mutual() {
        let mut s = Session::with_stdlib().unwrap();
        // Three mutually recursive functions: state machine
        // A→B→C→A cycling, counting down
        assert_eq!(
            s.eval("(labels ((state-a (n) (if (= n 0) 'a (state-b (- n 1)))) \
                             (state-b (n) (if (= n 0) 'b (state-c (- n 1)))) \
                             (state-c (n) (if (= n 0) 'c (state-a (- n 1))))) \
                      (list (state-a 0) (state-a 1) (state-a 2) (state-a 6)))").unwrap(),
            "(A B C A)",
        );
    }

    #[test]
    fn labels_captures_outer() {
        let mut s = Session::with_stdlib().unwrap();
        // labels closures can capture outer variables
        assert_eq!(
            s.eval("(let ((factor 3)) \
                      (labels ((scale (x) (* x factor))) \
                        (scale 14)))").unwrap(),
            "42",
        );
    }

    #[test]
    fn labels_funcall_sharp_quote() {
        let mut s = Session::with_stdlib().unwrap();
        // labels-bound functions work with #' and funcall
        assert_eq!(
            s.eval("(labels ((double (x) (* x 2))) \
                      (funcall #'double 21))").unwrap(),
            "42",
        );
    }

    #[test]
    fn labels_as_higher_order() {
        let mut s = Session::with_stdlib().unwrap();
        // labels function passed to mapcar
        assert_eq!(
            s.eval("(labels ((sq (x) (* x x))) \
                      (mapcar #'sq '(1 2 3 4 5)))").unwrap(),
            "(1 4 9 16 25)",
        );
    }

    // ── Batch 5: more CL coverage ─────────────────────────────────

    #[test]
    fn notany_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(notany #'oddp '(2 4 6))").unwrap(), "T");
        assert_eq!(s.eval("(notany #'oddp '(2 3 6))").unwrap(), "nil");
    }

    #[test]
    fn notevery_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(notevery #'oddp '(1 2 3))").unwrap(), "T");
        assert_eq!(s.eval("(notevery #'oddp '(1 3 5))").unwrap(), "nil");
    }

    #[test]
    fn string_search_via_search() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // search works on strings too via %as-list coercion
        assert_eq!(
            s.eval(r#"(search '(#\l #\l) (coerce "hello" 'list))"#).unwrap(),
            "2",
        );
    }

    #[test]
    fn reduce_with_initial_value() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(reduce #'+ '(1 2 3) :initial-value 10)").unwrap(),
            "16",
        );
    }

    #[test]
    fn copy_tree_is_deep() {
        let mut s = Session::with_stdlib().unwrap();
        // Verify copy-tree makes a deep copy (not just top-level)
        assert_eq!(
            s.eval("(let* ((orig '((a . 1) (b . 2))) \
                           (copy (copy-tree orig))) \
                      (equal orig copy))").unwrap(),
            "T",
        );
    }

    // ── Sweep II: type predicates ─────────────────────────────────

    #[test]
    fn floatp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(floatp 3.14)").unwrap(), "T");
        assert_eq!(s.eval("(floatp 42)").unwrap(), "nil");
    }

    #[test]
    fn rationalp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(rationalp 1/3)").unwrap(), "T");
        assert_eq!(s.eval("(rationalp 42)").unwrap(), "T");
        assert_eq!(s.eval("(rationalp 3.14)").unwrap(), "nil");
    }

    #[test]
    fn realp_and_complexp_work() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(realp 42)").unwrap(), "T");
        assert_eq!(s.eval("(realp 3.14)").unwrap(), "T");
        assert_eq!(s.eval("(complexp #C(1 2))").unwrap(), "T");
        assert_eq!(s.eval("(complexp 42)").unwrap(), "nil");
    }

    // ── Sweep II: assoc-if / rassoc-if / member-if ────────────────

    #[test]
    fn assoc_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(assoc-if #'oddp '((2 . a) (3 . b) (4 . c)))").unwrap(),
            "(3 . B)",
        );
    }

    #[test]
    fn rassoc_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(rassoc-if #'symbolp '((1 . 10) (2 . hello)))").unwrap(),
            "(2 . HELLO)",
        );
    }

    #[test]
    fn member_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(member-if #'evenp '(1 3 5 6 7))").unwrap(),
            "(6 7)",
        );
    }

    // ── Sweep II: position-if / find-if-not ───────────────────────

    #[test]
    fn position_if_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(position-if #'evenp '(1 3 4 5))").unwrap(), "2");
        assert_eq!(s.eval("(position-if #'evenp '(1 3 5))").unwrap(), "nil");
    }

    #[test]
    fn find_if_not_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(find-if-not #'oddp '(1 3 4 5))").unwrap(), "4");
    }

    // ── Sweep II: char case-insensitive ───────────────────────────

    #[test]
    fn char_lessp_greaterp() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval(r#"(char-lessp #\a #\B)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(char-greaterp #\b #\A)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(char-not-greaterp #\A #\a)"#).unwrap(), "T");
        assert_eq!(s.eval(r#"(char-not-lessp #\a #\A)"#).unwrap(), "T");
    }

    // ── Sweep II: nstring destructives ────────────────────────────

    #[test]
    fn nstring_upcase_works() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(
            s.eval(r#"(equal (nstring-upcase (copy-seq "hello")) "HELLO")"#).unwrap(),
            "T",
        );
    }

    #[test]
    fn nstring_downcase_works() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(
            s.eval(r#"(equal (nstring-downcase (copy-seq "HELLO")) "hello")"#).unwrap(),
            "T",
        );
    }

    #[test]
    fn nstring_capitalize_works() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        assert_eq!(
            s.eval(r#"(equal (nstring-capitalize (copy-seq "hello world")) "Hello World")"#).unwrap(),
            "T",
        );
    }

    // ── Sweep II: copy-seq / replace ──────────────────────────────

    #[test]
    fn copy_seq_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(copy-seq '(1 2 3))").unwrap(), "(1 2 3)");
    }

    #[test]
    fn replace_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(let ((v (vector 1 2 3 4 5))) \
                      (replace v '(9 8) :start1 1) \
                      v)").unwrap(),
            "#(1 9 8 4 5)",
        );
    }

    // ── Sweep II: set operations ──────────────────────────────────

    #[test]
    fn set_exclusive_or_works() {
        let mut s = Session::with_stdlib().unwrap();
        // Elements in either but not both: {1,2,3} XOR {2,3,4} = {1,4}
        assert_eq!(
            s.eval("(sort (set-exclusive-or '(1 2 3) '(2 3 4)) #'<)").unwrap(),
            "(1 4)",
        );
    }

    #[test]
    fn subsetp_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(subsetp '(1 2) '(1 2 3))").unwrap(), "T");
        assert_eq!(s.eval("(subsetp '(1 4) '(1 2 3))").unwrap(), "nil");
    }

    // ── Sweep II: mapl / mapcon / map ─────────────────────────────

    #[test]
    fn mapcon_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(mapcon (lambda (l) (list (length l))) '(a b c d))").unwrap(),
            "(4 3 2 1)",
        );
    }

    #[test]
    fn map_generic_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(map 'list #'+ '(1 2 3) '(10 20 30))").unwrap(),
            "(11 22 33)",
        );
    }

    // ── Sweep II: nsubstitute ─────────────────────────────────────

    #[test]
    fn nsubstitute_destructive() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(let ((lst (list 1 2 3 2 1))) \
                      (nsubstitute 9 2 lst) \
                      lst)").unwrap(),
            "(1 9 3 9 1)",
        );
    }

    // ── Sweep II: multiple-value-prog1 ────────────────────────────

    #[test]
    fn multiple_value_prog1_primary() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        // Primary value preserved across side-effecting forms
        assert_eq!(
            s.eval("(multiple-value-prog1 42 (+ 1 2) (+ 3 4))").unwrap(),
            "42",
        );
    }

    // ── Sweep II: nreconc ─────────────────────────────────────────

    #[test]
    fn nreconc_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(nreconc (list 3 2 1) '(4 5))").unwrap(),
            "(1 2 3 4 5)",
        );
    }

    // ── Mutable parameters ────────────────────────────────────────

    #[test]
    fn setq_required_param() {
        let mut s = Session::with_stdlib().unwrap();
        // Basic setq on a required parameter
        assert_eq!(
            s.eval("(funcall (lambda (x) (setq x 99) x) 1)").unwrap(),
            "99",
        );
    }

    #[test]
    fn setq_accumulate_param() {
        let mut s = Session::with_stdlib().unwrap();
        // Multiple setq on a required parameter (accumulator pattern)
        assert_eq!(
            s.eval("(funcall (lambda (n) (setq n (+ n 1)) (setq n (+ n 1)) n) 10)").unwrap(),
            "12",
        );
    }

    #[test]
    fn push_on_required_param() {
        let mut s = Session::with_stdlib().unwrap();
        // push on a required list parameter
        assert_eq!(
            s.eval("(funcall (lambda (lst) (push 0 lst) lst) '(1 2 3))").unwrap(),
            "(0 1 2 3)",
        );
    }

    #[test]
    fn mutable_param_with_closure_capture() {
        let mut s = Session::with_stdlib().unwrap();
        // Mutated param captured by an inner lambda
        assert_eq!(
            s.eval("(funcall (lambda (x) \
                      (setq x 42) \
                      (funcall (lambda () x))) 0)").unwrap(),
            "42",
        );
    }

    #[test]
    fn mutable_param_accumulator_pattern() {
        let mut s = Session::with_stdlib().unwrap();
        // The pattern that blocked the LOOP macro: accumulate via push on param
        assert_eq!(
            s.eval("(funcall (lambda (acc items) \
                      (dolist (x items) (push (* x x) acc)) \
                      (reverse acc)) \
                    nil '(1 2 3 4))").unwrap(),
            "(1 4 9 16)",
        );
    }

    #[test]
    fn defun_with_mutable_param() {
        let mut s = Session::with_stdlib().unwrap();
        // defun (not just lambda) should also handle mutable params
        s.eval("(defun test-mut-param (n) (setq n (* n 2)) n)").unwrap();
        assert_eq!(s.eval("(test-mut-param 21)").unwrap(), "42");
    }

    #[test]
    fn non_mutated_param_unchanged() {
        let mut s = Session::with_stdlib().unwrap();
        // Params that AREN'T mutated should still work as before
        assert_eq!(
            s.eval("(funcall (lambda (a b) (+ a b)) 20 22)").unwrap(),
            "42",
        );
    }

    // ── Supplied-p ────────────────────────────────────────────────

    #[test]
    fn optional_supplied_p() {
        let mut s = Session::with_stdlib().unwrap();
        // With supplied-p flag on &optional
        assert_eq!(
            s.eval("(funcall (lambda (&optional (x 10 x-p)) (list x x-p)) 42)").unwrap(),
            "(42 T)",
        );
        assert_eq!(
            s.eval("(funcall (lambda (&optional (x 10 x-p)) (list x x-p)))").unwrap(),
            "(10 nil)",
        );
    }

    #[test]
    fn key_supplied_p() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(funcall (lambda (&key (x 10 x-p)) (list x x-p)) :x 42)").unwrap(),
            "(42 T)",
        );
        assert_eq!(
            s.eval("(funcall (lambda (&key (x 10 x-p)) (list x x-p)))").unwrap(),
            "(10 nil)",
        );
    }

    #[test]
    fn defun_supplied_p() {
        let mut s = Session::with_stdlib().unwrap();
        // defun also supports supplied-p
        s.eval("(defun sp-test (&key (value nil value-p)) (if value-p value 'default))").unwrap();
        assert_eq!(s.eval("(sp-test :value 42)").unwrap(), "42");
        assert_eq!(s.eval("(sp-test)").unwrap(), "DEFAULT");
    }

    // ── Variadic comparisons ──────────────────────────────────────

    #[test]
    fn variadic_less_than() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(< 1 2 3)").unwrap(), "T");
        assert_eq!(s.eval("(< 1 3 2)").unwrap(), "nil");
        assert_eq!(s.eval("(< 1)").unwrap(), "T");
        assert_eq!(s.eval("(<)").unwrap(), "T");
    }

    #[test]
    fn variadic_equals() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(= 5 5 5)").unwrap(), "T");
        assert_eq!(s.eval("(= 5 5 6)").unwrap(), "nil");
    }

    #[test]
    fn variadic_greater_equal() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(>= 3 2 1)").unwrap(), "T");
        assert_eq!(s.eval("(>= 3 2 2)").unwrap(), "T");
        assert_eq!(s.eval("(>= 3 2 3)").unwrap(), "nil");
    }

    // ── catch / throw ─────────────────────────────────────────────

    #[test]
    fn catch_no_throw() {
        let mut s = Session::with_stdlib().unwrap();
        // catch without a throw returns the body value
        assert_eq!(s.eval("(catch 'foo 1 2 3)").unwrap(), "3");
    }

    #[test]
    fn catch_throw_basic() {
        let mut s = Session::with_stdlib().unwrap();
        // throw exits the catch with the thrown value
        assert_eq!(s.eval("(catch 'foo (throw 'foo 42) 99)").unwrap(), "42");
    }

    #[test]
    fn catch_throw_aborts_rest() {
        let mut s = Session::with_stdlib().unwrap();
        // Code after a throw must not run (verify via side effect)
        s.eval("(defvar *ct-marker* 0)").unwrap();
        s.eval("(catch 'tag (throw 'tag 1) (setq *ct-marker* 999))").unwrap();
        assert_eq!(s.eval("*ct-marker*").unwrap(), "0");
    }

    #[test]
    fn catch_throw_through_function() {
        let mut s = Session::with_stdlib().unwrap();
        // throw crosses a function-call boundary to reach the catch
        s.eval("(defun ct-thrower () (throw 'escape 77))").unwrap();
        assert_eq!(s.eval("(catch 'escape (ct-thrower) 5)").unwrap(), "77");
    }

    #[test]
    fn catch_nested_distinct_tags() {
        let mut s = Session::with_stdlib().unwrap();
        // Inner throw to outer tag skips the inner catch
        assert_eq!(
            s.eval("(catch 'outer (catch 'inner (throw 'outer 100)) 1)").unwrap(),
            "100",
        );
    }

    #[test]
    fn catch_nested_inner_tag() {
        let mut s = Session::with_stdlib().unwrap();
        // Inner throw to inner tag; outer returns its own last form
        assert_eq!(
            s.eval("(catch 'outer (catch 'inner (throw 'inner 50)) 7)").unwrap(),
            "7",
        );
    }

    #[test]
    fn catch_dynamic_tag() {
        let mut s = Session::with_stdlib().unwrap();
        // Tags are compared by EQ at runtime, not by lexical name
        assert_eq!(
            s.eval("(let ((tg 'dynamic)) (catch tg (throw tg 21)))").unwrap(),
            "21",
        );
    }

    #[test]
    fn catch_throw_computed_value() {
        let mut s = Session::with_stdlib().unwrap();
        // The thrown value is an arbitrary expression
        assert_eq!(
            s.eval("(catch 'c (throw 'c (+ 20 22)))").unwrap(),
            "42",
        );
    }

    #[test]
    fn unmatched_throw_is_catchable() {
        let mut s = Session::with_stdlib().unwrap();
        // A throw with no matching catch signals a catchable condition
        assert_eq!(
            s.eval("(handler-case (throw 'nope 1) (error (e) e 'caught))").unwrap(),
            "CAUGHT",
        );
    }

    // ── tagbody / go ──────────────────────────────────────────────

    #[test]
    fn tagbody_returns_nil() {
        let mut s = Session::with_stdlib().unwrap();
        // tagbody always returns NIL
        assert_eq!(s.eval("(tagbody)").unwrap(), "nil");
        s.eval("(defvar *tb-x* 0)").unwrap();
        assert_eq!(s.eval("(tagbody (setq *tb-x* 5))").unwrap(), "nil");
        assert_eq!(s.eval("*tb-x*").unwrap(), "5");
    }

    #[test]
    fn tagbody_sequential() {
        let mut s = Session::with_stdlib().unwrap();
        // Statements run top to bottom
        s.eval("(defvar *tb-acc* nil)").unwrap();
        s.eval("(tagbody (push 1 *tb-acc*) (push 2 *tb-acc*) (push 3 *tb-acc*))").unwrap();
        assert_eq!(s.eval("(reverse *tb-acc*)").unwrap(), "(1 2 3)");
    }

    #[test]
    fn tagbody_go_backward_loop() {
        let mut s = Session::with_stdlib().unwrap();
        // The classic counting loop via go
        s.eval("(defun tb-count (n) \
                  (let ((i 0) (acc nil)) \
                    (tagbody \
                      top \
                      (when (>= i n) (go done)) \
                      (push i acc) \
                      (setq i (+ i 1)) \
                      (go top) \
                      done) \
                    (reverse acc)))").unwrap();
        assert_eq!(s.eval("(tb-count 5)").unwrap(), "(0 1 2 3 4)");
    }

    #[test]
    fn tagbody_go_forward() {
        let mut s = Session::with_stdlib().unwrap();
        // go can skip forward over statements
        s.eval("(defvar *tb-fwd* nil)").unwrap();
        s.eval("(tagbody \
                  (push 'a *tb-fwd*) \
                  (go skip) \
                  (push 'b *tb-fwd*) \
                  skip \
                  (push 'c *tb-fwd*))").unwrap();
        // 'b is skipped
        assert_eq!(s.eval("(reverse *tb-fwd*)").unwrap(), "(A C)");
    }

    #[test]
    fn tagbody_go_aborts_rest_of_segment() {
        let mut s = Session::with_stdlib().unwrap();
        // Code after (go X) in the same segment must not run
        s.eval("(defvar *tb-abort* nil)").unwrap();
        s.eval("(tagbody \
                  (go target) \
                  (push 'should-not-run *tb-abort*) \
                  target \
                  (push 'ran *tb-abort*))").unwrap();
        assert_eq!(s.eval("*tb-abort*").unwrap(), "(RAN)");
    }

    #[test]
    fn tagbody_integer_tags() {
        let mut s = Session::with_stdlib().unwrap();
        // Tags can be integers, not just symbols
        s.eval("(defvar *tb-int* 0)").unwrap();
        s.eval("(tagbody (setq *tb-int* 1) (go 10) (setq *tb-int* 99) 10 (setq *tb-int* (+ *tb-int* 1)))").unwrap();
        assert_eq!(s.eval("*tb-int*").unwrap(), "2");
    }

    #[test]
    fn tagbody_nested() {
        let mut s = Session::with_stdlib().unwrap();
        // Nested tagbodies with distinct tags
        s.eval("(defvar *tb-nest* nil)").unwrap();
        s.eval("(tagbody \
                  outer \
                  (push 'o *tb-nest*) \
                  (tagbody \
                    inner \
                    (push 'i *tb-nest*)) \
                  (go fin) \
                  (push 'x *tb-nest*) \
                  fin)").unwrap();
        assert_eq!(s.eval("(reverse *tb-nest*)").unwrap(), "(O I)");
    }

    #[test]
    fn tagbody_do_style_sum() {
        let mut s = Session::with_stdlib().unwrap();
        // A realistic sum loop, like DO would expand to
        s.eval("(defun tb-sum (n) \
                  (let ((i 1) (sum 0)) \
                    (tagbody \
                      loop \
                      (when (> i n) (go end)) \
                      (setq sum (+ sum i)) \
                      (setq i (+ i 1)) \
                      (go loop) \
                      end) \
                    sum))").unwrap();
        assert_eq!(s.eval("(tb-sum 10)").unwrap(), "55");
    }

    // ── Extended LOOP ─────────────────────────────────────────────

    #[test]
    fn loop_simple_still_works() {
        let mut s = Session::with_stdlib().unwrap();
        // The simple (loop FORM...) form must still work (dispatch)
        assert_eq!(
            s.eval("(let ((i 0)) (loop (when (>= i 3) (return i)) (setq i (+ i 1))))").unwrap(),
            "3",
        );
    }

    #[test]
    fn loop_for_in_collect() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for x in '(1 2 3 4) collect (* x x))").unwrap(),
            "(1 4 9 16)",
        );
    }

    #[test]
    fn loop_for_from_to_sum() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(loop for i from 1 to 10 sum i)").unwrap(), "55");
    }

    #[test]
    fn loop_for_from_below() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for i from 0 below 5 collect i)").unwrap(),
            "(0 1 2 3 4)",
        );
    }

    #[test]
    fn loop_for_from_by() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for i from 0 to 10 by 2 collect i)").unwrap(),
            "(0 2 4 6 8 10)",
        );
    }

    #[test]
    fn loop_for_downto() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for i from 3 downto 0 collect i)").unwrap(),
            "(3 2 1 0)",
        );
    }

    #[test]
    fn loop_for_on() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for tail on '(1 2 3) collect (car tail))").unwrap(),
            "(1 2 3)",
        );
    }

    #[test]
    fn loop_repeat() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(loop repeat 4 sum 10)").unwrap(), "40");
    }

    #[test]
    fn loop_for_count_when() {
        let mut s = Session::with_stdlib().unwrap();
        // count with a when-gated clause: count odd numbers 1..10
        assert_eq!(
            s.eval("(loop for i from 1 to 10 when (oddp i) count i)").unwrap(),
            "5",
        );
    }

    #[test]
    fn loop_for_in_when_collect() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for x in '(1 2 3 4 5 6) when (evenp x) collect x)").unwrap(),
            "(2 4 6)",
        );
    }

    #[test]
    fn loop_maximize_minimize() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for x in '(3 1 4 1 5 9 2 6) maximize x)").unwrap(),
            "9",
        );
        assert_eq!(
            s.eval("(loop for x in '(3 1 4 1 5 9 2 6) minimize x)").unwrap(),
            "1",
        );
    }

    #[test]
    fn loop_while_with() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop with i = 0 while (< i 5) do (setq i (+ i 1)) collect i)").unwrap(),
            "(1 2 3 4 5)",
        );
    }

    #[test]
    fn loop_until() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for i from 0 until (> i 3) collect i)").unwrap(),
            "(0 1 2 3)",
        );
    }

    #[test]
    fn loop_append() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(loop for x in '(1 2 3) append (list x x))").unwrap(),
            "(1 1 2 2 3 3)",
        );
    }

    #[test]
    fn loop_for_equals_then() {
        let mut s = Session::with_stdlib().unwrap();
        // for v = init then step: powers of 2
        assert_eq!(
            s.eval("(loop repeat 5 for p = 1 then (* p 2) collect p)").unwrap(),
            "(1 2 4 8 16)",
        );
    }

    #[test]
    fn loop_finally() {
        let mut s = Session::with_stdlib().unwrap();
        // finally runs after the loop; its forms can reference the result
        assert_eq!(
            s.eval("(loop for i from 1 to 3 sum i)").unwrap(),
            "6",
        );
        // initially + finally with side effects
        s.eval("(defvar *loop-fin* nil)").unwrap();
        s.eval("(loop for i from 1 to 3 do (push i *loop-fin*) finally (push 'done *loop-fin*))").unwrap();
        assert_eq!(s.eval("(reverse *loop-fin*)").unwrap(), "(1 2 3 DONE)");
    }

    #[test]
    fn loop_return_clause() {
        let mut s = Session::with_stdlib().unwrap();
        // return clause exits the loop early
        assert_eq!(
            s.eval("(loop for i from 0 to 100 when (= i 7) return i)").unwrap(),
            "7",
        );
    }

    #[test]
    fn loop_always_never_thereis() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(s.eval("(loop for x in '(2 4 6) always (evenp x))").unwrap(), "T");
        assert_eq!(s.eval("(loop for x in '(2 4 5) always (evenp x))").unwrap(), "nil");
        assert_eq!(s.eval("(loop for x in '(1 3 5) never (evenp x))").unwrap(), "T");
        assert_eq!(s.eval("(loop for x in '(1 2 3) thereis (and (evenp x) x))").unwrap(), "2");
    }

    #[test]
    fn loop_nested() {
        let mut s = Session::with_stdlib().unwrap();
        // Nested loops: flatten a 3x3 multiplication grid sum
        assert_eq!(
            s.eval("(loop for i from 1 to 3 sum (loop for j from 1 to 3 sum (* i j)))").unwrap(),
            "36",
        );
    }

    // ── Probes: undefined-function failure mode ───────────────────

    #[test]
    fn probe_undefined_fn_runtime() {
        // Calling an undefined function at runtime should signal a
        // catchable condition, NOT crash the process.
        let mut s = Session::with_stdlib().unwrap();
        let r = s.eval("(handler-case (this-fn-does-not-exist 1 2) (error (e) e 'caught))");
        // We only care that it returns (catchable) rather than crashing.
        assert_eq!(r.unwrap(), "CAUGHT");
    }

    #[test]
    fn probe_undefined_fn_in_macroexpand() {
        // A macro whose expander calls an undefined helper. This used
        // to surface as STATUS_STACK_BUFFER_OVERRUN (really a Windows
        // process abort, code 0xC0000409) with no diagnostic. The
        // macroexpansion guard now turns it into a clean compile error
        // that names the offending function.
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defmacro probe-bad-macro (x) (probe-undefined-helper x))").unwrap();
        let r = s.eval("(probe-bad-macro 5)");
        let err = format!("{r:?}");
        assert!(r.is_err(), "expected a clean error, got {r:?}");
        // The diagnostic should name the undefined helper and flag it
        // as a macroexpansion-time failure.
        assert!(
            err.contains("PROBE-UNDEFINED-HELPER") || err.contains("probe-undefined-helper"),
            "error should name the undefined function; got: {err}"
        );
        assert!(
            err.to_lowercase().contains("undefined function"),
            "error should say 'undefined function'; got: {err}"
        );
    }

    #[test]
    fn probe_macro_explicit_error_is_clean() {
        // A macro expander that explicitly (error ...)s should also
        // produce a clean compile error, not abort the process.
        let mut s = Session::with_stdlib().unwrap();
        s.eval(r#"(defmacro probe-erroring-macro (x) (error "boom from expander"))"#).unwrap();
        let r = s.eval("(probe-erroring-macro 1)");
        let err = format!("{r:?}");
        assert!(r.is_err(), "expected a clean error, got {r:?}");
        assert!(err.contains("boom from expander"), "got: {err}");
    }

    #[test]
    fn probe_good_macro_still_works_after_guard() {
        // Regression: the guard must not disturb normal macroexpansion.
        let mut s = Session::with_stdlib().unwrap();
        s.eval("(defmacro probe-good-macro (a b) (list '+ a b))").unwrap();
        assert_eq!(s.eval("(probe-good-macro 19 23)").unwrap(), "42");
    }

    // ── Unhandled-condition recovery (top-level eval guard) ───────

    #[test]
    fn unhandled_condition_at_toplevel_is_recoverable() {
        // An undefined function at top level (no handler-case) used to
        // abort the process (0xC0000409). It must now return an Err.
        let mut s = Session::with_stdlib().unwrap();
        let r = s.eval("(no-such-function-xyz 1 2 3)");
        assert!(r.is_err(), "expected Err, got {r:?}");
        let msg = format!("{r:?}").to_lowercase();
        assert!(msg.contains("undefined function"), "got: {msg}");
    }

    #[test]
    fn toplevel_explicit_error_is_recoverable() {
        let mut s = Session::with_stdlib().unwrap();
        let r = s.eval(r#"(error "boom at top level")"#);
        assert!(r.is_err(), "expected Err, got {r:?}");
        assert!(format!("{r:?}").contains("boom at top level"), "got: {r:?}");
    }

    #[test]
    fn session_survives_unhandled_condition() {
        // The REPL/session must remain usable after an unhandled error —
        // the guard cleared the abort flag so the next form runs clean.
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        let _ = s.eval("(no-such-function-xyz)");
        assert_eq!(s.eval("(+ 1 2)").unwrap(), "3");
        let _ = s.eval(r#"(error "second boom")"#);
        assert_eq!(s.eval("(* 6 7)").unwrap(), "42");
    }

    #[test]
    fn handler_case_unaffected_by_toplevel_guard() {
        // The top-level guard is only a backstop: a handler-case inside
        // the form must still catch its own condition normally.
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(handler-case (error \"x\") (error (e) e 'handled))").unwrap(),
            "HANDLED",
        );
    }

    // ── Symbol plists + sublis (Prolog scouting) ──────────────────

    #[test]
    fn symbol_get_put() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        s.eval("(setf (get 'pkim 'color) 'red)").unwrap();
        s.eval("(setf (get 'pkim 'size) 'large)").unwrap();
        assert_eq!(s.eval("(get 'pkim 'color)").unwrap(), "RED");
        assert_eq!(s.eval("(get 'pkim 'size)").unwrap(), "LARGE");
        // Missing property returns the default.
        assert_eq!(s.eval("(get 'pkim 'weight 'unknown)").unwrap(), "UNKNOWN");
    }

    #[test]
    fn symbol_get_overwrite() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        s.eval("(setf (get 'pnode 'next) 'a)").unwrap();
        s.eval("(setf (get 'pnode 'next) 'b)").unwrap();
        assert_eq!(s.eval("(get 'pnode 'next)").unwrap(), "B");
    }

    #[test]
    fn symbol_plist_and_remprop() {
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        s.eval("(setf (get 'pr 'a) 1)").unwrap();
        s.eval("(setf (get 'pr 'b) 2)").unwrap();
        assert_eq!(s.eval("(remprop 'pr 'a)").unwrap(), "T");
        assert_eq!(s.eval("(get 'pr 'a 'gone)").unwrap(), "GONE");
        assert_eq!(s.eval("(get 'pr 'b)").unwrap(), "2");
        assert_eq!(s.eval("(remprop 'pr 'missing)").unwrap(), "nil");
    }

    #[test]
    fn sublis_works() {
        let mut s = Session::with_stdlib().unwrap();
        assert_eq!(
            s.eval("(sublis '((a . 1) (b . 2)) '(a (b c) a))").unwrap(),
            "(1 (2 C) 1)",
        );
    }

    #[test]
    fn prolog_unify_integration() {
        // A self-contained slice of the Prolog scout: the unifier must
        // produce a correct binding list. Permanent regression for the
        // plist/sublis/recursion combination real CL code leans on.
        let mut s = Session::with_stdlib().unwrap();
        s.activate();
        s.eval(r#"
          (defun pvar-p (x) (and (symbolp x) (not (null x))
                                 (eql (char (symbol-name x) 0) #\?)))
          (defun uv (var x b)
            (let ((vb (assoc var b)))
              (cond (vb (u (cdr vb) x b))
                    ((and (pvar-p x) (assoc x b)) (u var (cdr (assoc x b)) b))
                    (t (cons (cons var x) b)))))
          (defun u (x y b)
            (cond ((eq b 'fail) 'fail)
                  ((eql x y) b)
                  ((pvar-p x) (uv x y b))
                  ((pvar-p y) (uv y x b))
                  ((and (consp x) (consp y))
                   (u (cdr x) (cdr y) (u (car x) (car y) b)))
                  (t 'fail)))
        "#).unwrap();
        // (?x + 1) unify (2 + ?y) -> ?x=2, ?y=1
        assert_eq!(
            s.eval("(u '(?x + 1) '(2 + ?y) nil)").unwrap(),
            "((?Y . 1) (?X . 2))",
        );
        // occurs mismatch fails
        assert_eq!(s.eval("(u '(?x ?x) '(1 2) nil)").unwrap(), "FAIL");
    }
}
