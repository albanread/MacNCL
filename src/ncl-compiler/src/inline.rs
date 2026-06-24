//! inline.rs — IR-level inlining of `(declaim (inline …))` functions.
//!
//! See docs/compiler_completion.md § "Interprocedural — IR-level inlining".
//! NCL compiles each function into
//! its own LLVM module with a late-bound, boxed calling convention, so LLVM
//! cannot inline or optimize across Lisp call boundaries — and CL's
//! redefinition semantics mean it *shouldn't* (a call must, in general, reach
//! whatever the symbol's function cell currently holds). This pass does the
//! one interprocedural transform that is both standard-sanctioned and
//! high-value: it splices the body of a `declaim inline` function into its
//! direct call sites at LOWERING time, as the source form
//! `(let ((p1 a1) …) body…)`. Lowering then assigns fresh slots, boxes
//! mutated params, threads specials through `DynamicBind`, and — crucially —
//! the float-unboxing pass (`optimize.rs`) runs over the spliced body, so
//! representation inference finally crosses the call boundary. Eliminating the
//! call also deletes its GC-root push/pop + safepoint wrap, which is the
//! single largest measured cost in call-heavy code.
//!
//! ## Contract (CLHS 3.2.2.3)
//!
//! A `declaim inline` function's call sites may use a saved copy of its
//! definition; redefining it afterwards need not update already-compiled
//! callers. Callers opt in by declaiming inline BEFORE the function and its
//! callers compile — exactly the standard ordering. A later
//! `(declaim (notinline f))` cancels it (future callers emit a normal call).
//!
//! ## Soundness
//!
//!   * Only SIMPLE lambda lists (required params only) are captured — the
//!     `(let …)` substitution is exact for these: one evaluation per argument,
//!     left-to-right, before the body.
//!   * Params are bound by the spliced `let`, so they correctly SHADOW any
//!     caller local of the same name — no param capture.
//!   * Function calls in the body resolve through the symbol cell, never the
//!     caller's lexical env, so they are never captured.
//!   * Special-variable references resolve dynamically (not via the lexical
//!     env), so a caller local cannot capture them.
//!   * Residual risk: a body reference to a NON-special global *variable*
//!     whose bare name collides with a caller LOCAL would be captured. That
//!     requires un-earmuffed global-variable use AND a name clash — outside
//!     idiomatic CL; gated behind opt-in `declaim inline` and exercised by the
//!     gauntlet/ANSI regression suites.
//!   * Recursion is bounded: a name currently being inlined on the active path
//!     emits a normal call (direct and mutual recursion terminate after one
//!     level), and a hard depth cap backstops pathological non-recursive
//!     chains.
//!
//! Set `NCL_NO_INLINE=1` to disable the splice (A/B measurement / safety
//! valve); the registry still records, but call sites emit normal calls.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use ncl_runtime::value::Value;

/// Max nested inline expansions on one active path. The per-name guard
/// already bounds recursion to *distinct* names; this caps code-size blowup
/// from long non-recursive chains (f→g→h→…).
const MAX_INLINE_DEPTH: usize = 16;

/// A captured inline definition: the macroexpanded, declare-stripped body and
/// the required-parameter names, frozen at the function's compile time.
pub(crate) struct InlineDef {
    pub params: Vec<Arc<str>>,
    pub body: Vec<Value>,
}

#[derive(Default)]
struct Registry {
    /// Names declaimed `inline` (and not since `notinline`d).
    requested: HashSet<String>,
    /// Captured bodies for requested + simple functions, keyed by name.
    defs: HashMap<String, Arc<InlineDef>>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

/// `(declaim (inline NAME …))` — mark NAME inlinable for future compiles.
pub(crate) fn request_inline(name: &str) {
    registry().lock().unwrap().requested.insert(name.to_string());
}

/// `(declaim (notinline NAME …))` — cancel inlining; drop any captured body.
pub(crate) fn request_notinline(name: &str) {
    let mut r = registry().lock().unwrap();
    r.requested.remove(name);
    r.defs.remove(name);
}

/// Capture NAME's body at defun time — IFF it was declaimed inline and has a
/// simple (required-only) lambda list. A cheap `HashSet` probe otherwise.
pub(crate) fn maybe_record(name: &str, simple: bool, params: &[Arc<str>], body: &[Value]) {
    if !simple {
        return;
    }
    let mut r = registry().lock().unwrap();
    if r.requested.contains(name) {
        r.defs.insert(
            name.to_string(),
            Arc::new(InlineDef {
                params: params.to_vec(),
                body: body.to_vec(),
            }),
        );
    }
}

/// The captured definition for NAME, if it is currently inlinable.
pub(crate) fn lookup(name: &str) -> Option<Arc<InlineDef>> {
    registry().lock().unwrap().defs.get(name).cloned()
}

thread_local! {
    /// Names being inlined on the active lowering path (a dynamic-extent
    /// stack). Compilation is single-threaded; this never crosses threads.
    static IN_PROGRESS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// True if NAME must NOT be inlined here: it is already being inlined on the
/// active path (recursion), or the depth cap is hit. Either way the caller
/// emits a normal call.
pub(crate) fn blocked(name: &str) -> bool {
    if std::env::var_os("NCL_NO_INLINE").is_some() {
        return true;
    }
    IN_PROGRESS.with(|s| {
        let s = s.borrow();
        s.len() >= MAX_INLINE_DEPTH || s.iter().any(|n| n == name)
    })
}

/// Enter NAME's inline expansion (push the dynamic-extent guard).
pub(crate) fn enter(name: &str) {
    IN_PROGRESS.with(|s| s.borrow_mut().push(name.to_string()));
}

/// Leave the current inline expansion (pop the guard). Always paired with
/// `enter`, including on the error path.
pub(crate) fn exit() {
    IN_PROGRESS.with(|s| {
        s.borrow_mut().pop();
    });
}
