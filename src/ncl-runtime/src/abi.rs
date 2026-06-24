//! C ABI surface for JIT'd Lisp code.
//!
//! Compiled Lisp code calls back into the runtime through these
//! `extern "C"` functions. They take and return `u64` (raw `Word`
//! bits) so the function signatures match what LLVM IR can express
//! without any extra translation layer.
//!
//! All functions in this module are entry points from JIT'd code
//! and live for the process lifetime — never reorder, never
//! rename. The compiler's emitter looks them up by name.

use crate::mutator::MutatorState;
use crate::word::{Tag, Word};

/// Common tail for every signal-a-condition path: store the
/// condition in the per-thread slot, set both the per-thread
/// CONDITION_PENDING flag (read by `%handler-case`) and the
/// unified ABORT_PENDING flag (polled by JIT-emitted post-call
/// abort checks). Returns NIL as the call-site's value; callers
/// return that NIL upward and the JIT's abort-check trips next.
///
/// Factored out because forgetting to set ABORT_PENDING was the
/// exact bug `Expr::LoadGlobal` / `Expr::LoadFunction` exposed —
/// the three flags must move together at every signal site.
fn record_pending_condition(cond_raw: u64) -> u64 {
    CONDITION_SLOT.with(|s| s.set(cond_raw));
    CONDITION_PENDING.with(|p| p.set(true));
    ABORT_PENDING.with(|p| p.set(true));
    Word::NIL.raw()
}

/// Distinct process exit code for an unhandled Lisp condition.
/// Chosen so it is NOT a crash signature: `std::process::abort()` on
/// Windows raises __fastfail and exits 0xC0000409, byte-identical to
/// a stack overflow or buffer overrun, and a Rust panic exits 101.
/// 70 (BSD sysexits EX_SOFTWARE, "internal software error") is stable
/// and unambiguously "your program signaled an error nobody caught,"
/// not "the runtime crashed."
pub const EXIT_UNHANDLED_CONDITION: i32 = 70;

/// Terminate the process on an unhandled Lisp condition: print a
/// clear, flushed diagnostic and exit with EXIT_UNHANDLED_CONDITION.
///
/// Used by every signal path when no `handler-case` is active. We
/// deliberately use `exit`, not `abort`, so the failure is legible:
/// a controlled "unhandled condition" is a normal (if terminal)
/// outcome of running Lisp, not a runtime bug, and should not look
/// like one in logs, CI, or a debugger.
pub(crate) fn die_unhandled_condition(msg: &str) -> ! {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "\nUnhandled Lisp condition: {msg}");
    let _ = writeln!(
        err,
        "No active handler-case to catch it; exiting (code {EXIT_UNHANDLED_CONDITION})."
    );
    let _ = err.flush();
    std::process::exit(EXIT_UNHANDLED_CONDITION);
}

fn signal_condition_word_and_abort(cond: Word) -> u64 {
    if HANDLER_DEPTH.with(|d| d.get()) == 0 {
        die_unhandled_condition(&crate::printer::format_word_aesthetic(cond));
    }
    record_pending_condition(cond.raw())
}

fn signal_static_condition_string_and_abort(
    mutator: *mut crate::mutator::MutatorState,
    msg: &str,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let cond = crate::gc_string::alloc_string_in_static(
        m.coord().static_area(),
        msg,
    )
    .unwrap_or_else(|| {
        eprintln!("gc-stall: static-area condition allocation failed; original message: {msg}");
        Word::NIL
    });
    signal_condition_word_and_abort(cond)
}

fn catch_gc_stall_as_condition<F>(
    mutator: *mut crate::mutator::MutatorState,
    body: F,
) -> u64
where
    F: FnOnce() -> u64,
{
    #[cfg(feature = "gc-page-heap")]
    {
        newgc_core::page_heap::evac::install_quiet_gc_stall_panic_hook();
        // Snapshot root depth so a mid-body unwind can't leave
        // half-pushed roots in place. Callees like
        // `ncl_build_rest_list` push roots around their alloc loop;
        // a panic from `alloc_cons` would otherwise leak them.
        let saved_root_depth = unsafe { (*mutator).root_count() };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
            Ok(value) => value,
            Err(payload) => {
                unsafe { (*mutator).truncate_roots(saved_root_depth) };
                if let Some(stall) =
                    payload.downcast_ref::<newgc_core::page_heap::evac::GcStallError>()
                {
                    let m = unsafe { &mut *mutator };
                    let static_area = m.coord().static_area();
                    let msg = stall.render_with_runtime_context(
                        "young exhausted",
                        static_area.used_bytes(),
                        static_area.committed_bytes(),
                    );
                    return signal_static_condition_string_and_abort(mutator, &msg);
                }
                std::panic::resume_unwind(payload);
            }
        }
    }
    #[cfg(not(feature = "gc-page-heap"))]
    {
        body()
    }
}

/// Allocate a cons cell. JIT'd `(cons car cdr)` lowers to a call
/// here.
///
/// SAFETY: `mutator` must be a valid pointer to a `MutatorState`
/// owned by the calling thread. The Lisp-thread discipline ensures
/// this: the driver passes the mutator pointer when invoking the
/// entry function, and the entry function threads it through every
/// call.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_alloc_cons(mutator: *mut MutatorState, car: u64, cdr: u64) -> u64 {
    catch_gc_stall_as_condition(mutator, || {
        let m = unsafe { &mut *mutator };
        let car_w = Word::from_raw(car);
        let cdr_w = Word::from_raw(cdr);
        m.alloc_cons(car_w, cdr_w).raw()
    })
}

/// Read the car field of a cons cell. JIT'd `(car x)` lowers to
/// untag-and-load directly without calling here, but this is also
/// exposed for use from FFI / debugger / tests.
///
/// SAFETY: `cons` is a Cons-tagged `Word` OR `nil`. (CL spec:
/// `(car nil) = (cdr nil) = nil`.)
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_car(cons: u64) -> u64 {
    let w = Word::from_raw(cons);
    if w.is_nil() {
        return Word::NIL.raw();
    }
    let p = w.as_ptr::<u64>(Tag::Cons).expect("ncl_car called on non-cons");
    unsafe { *p }
}

/// Read the cdr field of a cons cell. As `ncl_car` but at offset 1.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_cdr(cons: u64) -> u64 {
    let w = Word::from_raw(cons);
    if w.is_nil() {
        return Word::NIL.raw();
    }
    let p = w.as_ptr::<u64>(Tag::Cons).expect("ncl_cdr called on non-cons");
    unsafe { *p.add(1) }
}

/// Load a global symbol's value cell with acquire semantics.
/// JIT'd code calls this when it reads a bare symbol that wasn't
/// resolved to a local at compile time. On an unbound cell, signals
/// a Lisp condition (catchable with `handler-case`); aborts the
/// process if no handler is installed.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_load_value(
    mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
) -> u64 {
    let sym = Word::from_raw(sym_word);
    let value = crate::gc_symbol::value_acquire(sym);
    if value.is_unbound() {
        let name = crate::sym_names::lookup(sym_word)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{sym_word:#x}"));
        return signal_condition_string(
            mutator,
            &format!("unbound variable: {name}"),
        );
    }
    value.raw()
}

/// Store a value into a global symbol's value cell with release
/// semantics. JIT'd `(setq …)` and `(defparameter …)` lower to a
/// call here. Returns the stored value (CL's `setq` returns the
/// last value assigned).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_store_value(
    mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
    new_value: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let sym = Word::from_raw(sym_word);
    m.set_symbol_value(sym, Word::from_raw(new_value));
    new_value
}

/// Dynamic variable bind. Saves the current value of `sym`'s value
/// cell, sets it to `new_val`, and returns the saved value. The
/// caller must pass the saved value to `ncl_dynamic_unbind` when the
/// dynamic scope exits. JIT'd `(let ((*var* expr)) ...)` where `*var*`
/// is globally special lowers to this pattern.
///
/// Note: the save/restore is NOT automatically undone on non-local
/// exit (condition signals). Full correctness requires UNWIND-PROTECT,
/// which is the next feature after DECLARE SPECIAL. On normal exit
/// the binding is always correctly restored.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_dynamic_bind(
    _mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
    new_val: u64,
) -> u64 {
    let sym = Word::from_raw(sym_word);
    let saved = crate::gc_symbol::value_acquire(sym);
    // Store NIL if currently UNBOUND so the saved slot holds a valid
    // Word (UNBOUND is an internal sentinel, not a Lisp value).
    let saved_raw = if saved.is_unbound() { Word::NIL.raw() } else { saved.raw() };
    crate::gc_symbol::set_value_release(sym, Word::from_raw(new_val));
    saved_raw
}

/// Dynamic variable unbind. Restores `sym`'s value cell to
/// `saved_val` (the value returned by a prior `ncl_dynamic_bind`
/// call for the same symbol).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_dynamic_unbind(
    _mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
    saved_val: u64,
) {
    let sym = Word::from_raw(sym_word);
    crate::gc_symbol::set_value_release(sym, Word::from_raw(saved_val));
}

/// Length of a String or proper list (in codepoints / cons cells).
/// JIT'd `(length …)` lowers to a call here. Polymorphic on tag.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_length(w: u64) -> u64 {
    let word = Word::from_raw(w);
    if word.is_nil() {
        return Word::fixnum(0).raw();
    }
    match word.tag() {
        Tag::String => Word::fixnum(crate::gc_string::char_count(word) as i64).raw(),
        Tag::Vector => Word::fixnum(vector_payload_len(word) as i64).raw(),
        Tag::Cons => {
            // Walk the cons spine.
            let mut cur = word;
            let mut count: i64 = 0;
            while !cur.is_nil() {
                if cur.tag() != Tag::Cons {
                    panic!("length: improper list");
                }
                let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
                count += 1;
                cur = Word::from_raw(unsafe { *p.add(1) });
            }
            Word::fixnum(count).raw()
        }
        _ => panic!("length: not a sequence: {word:?}"),
    }
}

/// Read a Vector-tagged Word's element count from its header.
/// SAFETY: `v` must be a valid Vector-tagged Word.
fn vector_payload_len(v: Word) -> u32 {
    let p = v.as_ptr::<u64>(Tag::Vector).expect("vector_payload_len: not a vector");
    let header = crate::heap::HeapHeader::from_raw(unsafe { *p });
    header.length_cells()
}

/// Read element `i` of a vector. SAFETY: `v` must be a valid
/// Vector-tagged Word and `i` must be in bounds.
fn vector_cell(v: Word, i: u32) -> u64 {
    let p = v.as_ptr::<u64>(Tag::Vector).expect("vector_cell: not a vector");
    unsafe { *p.add(1 + i as usize) }
}

/// Write element `i` of a vector and card-mark via the mutator
/// for the next GC's old-to-young scan. SAFETY: as `vector_cell`,
/// plus `mutator` must be the live MutatorState for this thread.
fn set_vector_cell(
    mutator: *mut crate::mutator::MutatorState,
    v: Word,
    i: u32,
    val: u64,
) {
    let p = v.as_mut_ptr::<u64>(Tag::Vector).expect("set_vector_cell: not a vector");
    let slot = unsafe { p.add(1 + i as usize) };
    unsafe { *slot = val };
    let m = unsafe { &mut *mutator };
    m.mark_card(slot as *const u8);
}

/// Structural equality. Recurses through cons cells, treats
/// strings codepoint-by-codepoint, falls back to eq for atoms.
/// JIT'd `(equal a b)` lowers to a call here.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_equal(a: u64, b: u64) -> u64 {
    if equal_recursive(Word::from_raw(a), Word::from_raw(b)) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

fn equal_recursive(a: Word, b: Word) -> bool {
    // Fast path: same Word bits = trivially equal (covers eq case
    // for symbols, fixnums, immediates, and identical pointers).
    if a.raw() == b.raw() {
        return true;
    }
    match (a.tag(), b.tag()) {
        (Tag::Cons, Tag::Cons) => {
            let pa = a.as_ptr::<u64>(Tag::Cons).expect("cons");
            let pb = b.as_ptr::<u64>(Tag::Cons).expect("cons");
            let car_a = Word::from_raw(unsafe { *pa });
            let car_b = Word::from_raw(unsafe { *pb });
            if !equal_recursive(car_a, car_b) {
                return false;
            }
            let cdr_a = Word::from_raw(unsafe { *pa.add(1) });
            let cdr_b = Word::from_raw(unsafe { *pb.add(1) });
            equal_recursive(cdr_a, cdr_b)
        }
        (Tag::String, Tag::String) => crate::gc_string::string_eq(a, b),
        // Any other combination is an atom: `equal` reduces to `eql`
        // here (per CL, equal of non-structured objects is eql). This
        // is what makes `(equal 3.0 3.0)` true — two boxed floats of
        // the same value — where the bit-compare fast path above fails.
        _ => eql_values(a, b),
    }
}

/// EQL semantics: two values are EQL iff their tagged Words are
/// bit-identical, OR they are numbers of the SAME type with the same
/// value, OR they are the same character. Characters and fixnums live
/// inline in the Word, so the identity fast path already covers them;
/// the extra work is only for the boxed numeric tower — float, bignum,
/// ratio, complex — where two equal-valued instances are distinct heap
/// objects. Each branch requires BOTH operands to be that type, so a
/// cross-type pair is never EQL even when numerically equal:
/// `(eql 3 3.0)` is NIL.
pub fn eql_values(a: Word, b: Word) -> bool {
    if a.raw() == b.raw() {
        return true;
    }
    // All four boxed-numeric types are Vector-tagged heap objects
    // distinguished by HeapHeader type. Decode each operand's tag +
    // header ONCE and dispatch on the pair — the old predicate
    // chain (is_float && is_float, is_bignum && is_bignum, …)
    // re-decoded the tag and re-loaded the header per probe, up to
    // four times per operand, then ncl_cmp_full decoded both again.
    let ta = match crate::ratio::heap_numeric_type(a) {
        Some(t) => t,
        None => return false,
    };
    if crate::ratio::heap_numeric_type(b) != Some(ta) {
        // Different types (or b isn't a boxed number): never EQL,
        // even when numerically equal — (eql 3 3.0) is NIL.
        return false;
    }
    use crate::heap::HeapType;
    match ta {
        HeapType::Float | HeapType::Bignum | HeapType::Ratio => {
            crate::ratio::cmp_full_typed(a, b, Some(ta), Some(ta)) == 0
        }
        HeapType::Complex => {
            // Component-wise EQL: (eql #c(3 -4) #c(3 -4)) is true.
            eql_values(
                crate::complex::complex_real(a),
                crate::complex::complex_real(b),
            ) && eql_values(
                crate::complex::complex_imag(a),
                crate::complex::complex_imag(b),
            )
        }
        // heap_numeric_type only yields the four variants above.
        _ => false,
    }
}

/// String equality. Both args must be Tag::String. Returns Word::T
/// or Word::NIL.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_string_eq(a: u64, b: u64) -> u64 {
    let wa = Word::from_raw(a);
    let wb = Word::from_raw(b);
    if wa.tag() != Tag::String || wb.tag() != Tag::String {
        panic!("string=: arguments must be strings");
    }
    if crate::gc_string::string_eq(wa, wb) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// Read the i-th codepoint of a string as a character Word.
/// `(char s i)` and `(aref s i)` both lower here for strings.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_string_char(s: u64, idx: u64) -> u64 {
    let ws = Word::from_raw(s);
    if ws.tag() != Tag::String {
        panic!("string-char: argument must be a string");
    }
    let n = crate::gc_string::char_count(ws);
    let i = idx as usize;
    if i >= n {
        panic!("string-char: index {i} out of bounds for length {n}");
    }
    let cp = crate::gc_string::codepoint_at(ws, i);
    let c = char::from_u32(cp).expect("invalid codepoint in string");
    Word::char(c).raw()
}

/// Load a Symbol's function cell with acquire semantics. JIT'd
/// `#'name` (i.e. `(function name)`) lowers to a call here. Used
/// to pass defun'd functions as first-class values to higher-order
/// code. Signals on unbound — same condition shape as
/// `ncl_load_value` and `ncl_call`'s unbound case.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_load_function(
    mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
) -> u64 {
    let sym = Word::from_raw(sym_word);
    let fn_value = crate::gc_symbol::function_acquire(sym);
    if fn_value.is_unbound() {
        let name = crate::sym_names::lookup(sym_word)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{sym_word:#x}"));
        return signal_condition_string(
            mutator,
            &format!("undefined function: {name}"),
        );
    }
    fn_value.raw()
}

/// Dispatch a function call through a Symbol's function cell.
/// JIT'd `(name arg1 arg2 ...)` lowers to a call here.
///
/// Loads the symbol's function cell with acquire semantics, follows
/// the Function pointer to get the code address and the closure
/// env, and calls
///   `code(mutator, env, args, n_args)`.
/// Panics if the cell is unbound.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_call(
    mutator: *mut crate::mutator::MutatorState,
    sym_word: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let sym = Word::from_raw(sym_word);
    let fn_value = crate::gc_symbol::function_acquire(sym);
    if fn_value.is_unbound() {
        let name = crate::sym_names::lookup(sym_word)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{sym_word:#x}"));
        return signal_condition_string(
            mutator,
            &format!("undefined function: {name}"),
        );
    }
    if fn_value.tag() != Tag::Function {
        return signal_condition_string(
            mutator,
            &format!("not a function: {fn_value:?}"),
        );
    }
    if let Some(cond) = under_supplied_arity(mutator, fn_value, n_args) {
        return cond;
    }
    let env = crate::gc_function::env(fn_value);
    // Diagnostic: an env that isn't NIL and isn't a Vector pointer
    // means the Function's env field has been corrupted (typically
    // by a missed card-rewrite). Catch it here with full context
    // instead of letting the JIT'd code crash on a stale deref.
    debug_assert_closure_env_valid("ncl_call", sym_word, fn_value, env);
    let code = crate::gc_function::code_ptr(fn_value);
    let f: crate::gc_function::LispCodeFn =
        unsafe { std::mem::transmute(code) };
    unsafe { f(mutator, env.raw(), args, n_args) }
}

/// Verify that a Function's env field is in the expected shape
/// before we hand it to JIT-compiled code. NIL means "no captures
/// (plain defun)"; a Vector-tagged Word means "closure with a
/// captured env Vector." Anything else is a corruption signature
/// — almost always a Fixnum 0 left behind by `phase3_reclaim`
/// after the env Vector's page was reclaimed and the static-area
/// env cell wasn't rewritten because the mark pass missed the
/// Vector or the card was inadvertently cleared.
fn debug_assert_closure_env_valid(
    site: &str,
    sym_word: u64,
    fn_value: Word,
    env: Word,
) {
    if env.is_nil() {
        return;
    }
    if env.tag() == Tag::Vector {
        return;
    }
    let sym_name = crate::sym_names::lookup(sym_word)
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{sym_word:#x}"));
    eprintln!(
        "[{site}] CORRUPT closure env: sym={sym_name} fn={fn_value:?} env_raw={:#x} env_tag={:?}",
        env.raw(),
        env.tag(),
    );
    if let Some(p) = fn_value.as_ptr::<u8>(Tag::Function) {
        let cell_addr = unsafe { p.add(crate::gc_function::ENV_OFFSET * 8) };
        eprintln!(
            "[{site}] env cell address: {cell_addr:p} (static-area? card-marked at make_closure)"
        );
    }
    std::process::abort();
}

/// Coerce a "function designator" (CLHS glossary) to a callable
/// Function Word. CL spec says funcall/apply accept either a function
/// OR a symbol that names a function (its global function cell is
/// looked up). Without this, `(apply '+ '(1 2))` and friends panic
/// with "not a function" — chapter 5 APPLY in the corman ANSI suite
/// catches it.
///
/// Returns the resolved Function Word on success; signals a Lisp
/// condition (returns NIL) on a bad designator. The signaled
/// condition flows through the standard handler-case / ABORT_PENDING
/// machinery, so a caller wrapped in handler-case can recover.
fn resolve_function_designator(
    mutator: *mut crate::mutator::MutatorState,
    site: &str,
    w: Word,
) -> Result<Word, u64> {
    match w.tag() {
        Tag::Function => Ok(w),
        Tag::Symbol => {
            let f = crate::gc_symbol::function_acquire(w);
            if f.tag() == Tag::Function {
                Ok(f)
            } else {
                let name = crate::sym_names::lookup(w.raw())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{:#x}", w.raw()));
                Err(signal_static_condition_string_and_abort(
                    mutator,
                    &format!("{site}: symbol {name} has no function binding"),
                ))
            }
        }
        _ => Err(signal_static_condition_string_and_abort(
            mutator,
            &format!("{site}: not a function designator: {w:?}"),
        )),
    }
}

/// Guard against calling a Lisp-compiled function with fewer
/// arguments than its required-parameter count.
///
/// A Lisp callee's prologue loads each required parameter straight
/// from `args[i]`; under-supplying makes it read PAST the end of the
/// argument array into uninitialised memory — a stale stack slot or
/// a code pointer — and crash with an access violation. That is the
/// whole "too few arguments" crash family: `(funcall (complement
/// #'member) …)`, a stray `(some-fn)`, a predicate passed to
/// `mapcar`/`sort`/`reduce` that's called with the wrong arity, etc.
///
/// Native Rust shims are exempt: they manage their own (frequently
/// variadic) argument lists and validate `n_args` themselves, so a
/// fixed `arity` field would wrongly reject e.g. `(+)` or `(list)`.
///
/// Returns `Some(_)` — a value that flows through the condition
/// machinery (a recorded condition under a handler, else a clean
/// process exit) — when the call must be rejected; `None` to proceed.
#[inline]
fn under_supplied_arity(
    mutator: *mut crate::mutator::MutatorState,
    f_word: Word,
    n_args: u64,
) -> Option<u64> {
    if !crate::gc_function::is_lisp_compiled(f_word) {
        return None;
    }
    let required = crate::gc_function::arity(f_word) as u64;
    if n_args >= required {
        return None;
    }
    let fname = crate::sym_names::lookup(crate::gc_function::name(f_word).raw())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "anonymous function".to_string());
    Some(signal_condition_string(
        mutator,
        &format!(
            "{fname}: too few arguments — got {n_args}, requires at least {required}"
        ),
    ))
}

/// Call a Function value directly. Accepts a function designator —
/// either a Function Word or a symbol whose function cell is bound
/// to a Function. The latter case is what makes `(funcall '+ 1 2)`
/// and `(setq f '+) (funcall f 1 2)` work per CL spec.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_funcall(
    mutator: *mut crate::mutator::MutatorState,
    fn_word: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    catch_gc_stall_as_condition(mutator, || {
        let raw_word = Word::from_raw(fn_word);
        let f_word = match resolve_function_designator(mutator, "funcall", raw_word) {
            Ok(w) => w,
            Err(nil) => return nil,
        };
        if let Some(cond) = under_supplied_arity(mutator, f_word, n_args) {
            return cond;
        }
        let env = crate::gc_function::env(f_word);
        debug_assert_closure_env_valid("ncl_funcall", 0, f_word, env);
        let code = crate::gc_function::code_ptr(f_word);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), args, n_args) }
    })
}

/// Allocate a closure: a Function in static with the given code,
/// arity, and a freshly allocated env Vector containing the
/// captured values. JIT'd `(lambda …)` evaluates each capture
/// expression in outer scope, packs the values into a stack
/// buffer, and calls here to materialise the function value.
///
/// Static-area placement: the Function record is permanent for the
/// process lifetime (or until retirement-with-quiescent-epoch lands,
/// which it hasn't). Moving these to the young heap was tried in
/// an earlier branch and is documented in `docs/GC_DESIGN.md` —
/// the issue was conservative-stack-pinning + promote-on-first-
/// survival turning every macroexpand-time transient into a tenured
/// object, spiralling into OOM. The fix lives at the GC layer
/// (Phase 1 of GC_DESIGN.md: tighten pinner, age-threshold
/// promotion), not at the allocator layer. Once the GC handles
/// transient closures gracefully, this can revisit the young-heap
/// path.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_make_closure(
    mutator: *mut crate::mutator::MutatorState,
    code_ptr: u64,
    arity: u64,
    captures: *const u64,
    n_captures: u64,
) -> u64 {
    // Fast path: if the env Vector (or NIL for no captures) fits
    // in the current TLAB, the alloc can't trigger GC, so we don't
    // need `catch_gc_stall_as_condition` (panic-catch setup), don't
    // need to root the captures (no GC can move them), and don't
    // need to read them back from a precise-root list. Just bump
    // the TLAB, store the captures, alloc the Function record in
    // static, mark the env card, return.
    //
    // This is the closure-allocation hot path. Most closures are
    // small (n_captures in 0..=4) and the TLAB has plenty of room
    // — so this branch runs the vast majority of the time. The
    // slow path below stays as the fully-rooted catch-all.
    {
        let m = unsafe { &mut *mutator };
        let env_word_opt: Option<Word> = if n_captures == 0 {
            Some(Word::NIL)
        } else {
            m.try_alloc_vector_no_gc(n_captures as u32).map(|vec| {
                // No GC possible between alloc and store: TLAB bump
                // is pure arithmetic, captures still live at their
                // original addresses, raw store is safe.
                let p = vec
                    .as_mut_ptr::<u64>(Tag::Vector)
                    .expect("vector ptr");
                unsafe {
                    for i in 0..n_captures {
                        *p.add(1 + i as usize) =
                            *captures.add(i as usize);
                    }
                }
                vec
            })
        };
        if let Some(env_word) = env_word_opt {
            let coord = m.coord();
            let fn_word = match crate::gc_function::alloc_function_in_static(
                coord.static_area(),
                code_ptr as usize,
                arity as u32,
                Word::NIL,
                env_word,
                true, // lambda — Lisp-compiled; manages its own MV slot
            ) {
                Some(w) => w,
                None => {
                    return signal_static_condition_string_and_abort(
                        mutator,
                        "static area exhausted in lambda creation",
                    );
                }
            };
            if !env_word.is_nil() {
                let env_cell_addr = unsafe {
                    (fn_word.as_ptr::<u8>(Tag::Function).unwrap()
                        as *const u8)
                        .add(crate::gc_function::ENV_OFFSET * 8)
                };
                coord.mark_card(env_cell_addr);
            }
            return fn_word.raw();
        }
        // Fall through to the slow path: TLAB couldn't fit the env
        // Vector, so we need a refill, which may GC.
    }
    catch_gc_stall_as_condition(mutator, || {
        let m = unsafe { &mut *mutator };
        let env_word = if n_captures == 0 {
            Word::NIL
        } else {
            // Each capture is potentially a heap pointer held only in
            // the caller's stack alloca (`cap_buf`), which the GC
            // does NOT cover via precise roots. If we read them
            // before alloc_vector and the alloc triggers a GC,
            // the post-GC objects move but our copies in cap_buf
            // stay stale — the env Vector ends up holding dangling
            // pointers, and the closure crashes on first dereference
            // (the demos/life.lisp gen-25 / `lambda_1317` crash).
            //
            // Fix: push every capture into the mutator's precise
            // root list BEFORE alloc_vector, then read them back
            // from the root list afterward. The GC walks the root
            // list during any cycle inside alloc_vector and
            // forwards-updates them, so the read-back gives the
            // post-GC pointer.
            let root_base = m.root_count();
            for i in 0..n_captures {
                let cap = Word::from_raw(unsafe { *captures.add(i as usize) });
                m.push_root(cap);
            }
            let vec = m.alloc_vector(n_captures as u32);
            // Vec layout: cell 0 is header, cells 1..=n are payload.
            let p = vec
                .as_mut_ptr::<u64>(Tag::Vector)
                .expect("vector ptr");
            for i in 0..n_captures {
                let cap_post_gc = m.root_at(root_base + i as usize);
                unsafe {
                    *p.add(1 + i as usize) = cap_post_gc.raw();
                }
            }
            // Drop the precise roots — the env Vector now owns them
            // (the Function structure we allocate next will hold the
            // Vector, and the returned Function Word is the caller's
            // single live reference to the whole closure).
            for _ in 0..n_captures {
                m.pop_root().expect("make_closure capture root missing");
            }
            vec
        };
        // Lambdas live in static for now (with the env-pointer card
        // marked if env is in young — see below).
        let coord = m.coord();
        let fn_word = crate::gc_function::alloc_function_in_static(
            coord.static_area(),
            code_ptr as usize,
            arity as u32,
            Word::NIL,
            env_word,
            true, // lambda — Lisp-compiled; manages its own MV slot
        )
        .expect("static area exhausted in lambda creation");
        if !env_word.is_nil() {
            let env_cell_addr = unsafe {
                (fn_word.as_ptr::<u8>(Tag::Function).unwrap() as *const u8)
                    .add(crate::gc_function::ENV_OFFSET * 8)
            };
            coord.mark_card(env_cell_addr);
        }
        fn_word.raw()
    })
}

/// Mutate the car field of a cons cell. JIT'd `(setf (car x) v)`
/// lowers to a call here. Returns the new value (per setf
/// semantics — the *cell* gets the new value, but the form
/// evaluates to the value, not the cons).
///
/// The card containing the modified cell is marked so the GC's
/// inter-generational scan picks up any old→young pointer this
/// store may have created.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_set_car(
    mutator: *mut crate::mutator::MutatorState,
    cons: u64,
    new_value: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(cons);
    let p = w
        .as_mut_ptr::<u64>(Tag::Cons)
        .expect("ncl_set_car called on non-cons");
    unsafe { *p = new_value };
    m.mark_card(p as *const u8);
    new_value
}

/// Mutate the cdr field of a cons cell. As `ncl_set_car` but at
/// offset 1.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_set_cdr(
    mutator: *mut crate::mutator::MutatorState,
    cons: u64,
    new_value: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let w = Word::from_raw(cons);
    let p = w
        .as_mut_ptr::<u64>(Tag::Cons)
        .expect("ncl_set_cdr called on non-cons");
    let slot = unsafe { p.add(1) };
    unsafe { *slot = new_value };
    m.mark_card(slot as *const u8);
    new_value
}

// ---- Conditions: error / handler-case --------------------------------------
//
// The control-transfer mechanism is intentionally NOT panic-based.
// Rust panics work cleanly between Rust frames, but propagating
// them through LLVM-MCJIT-emitted Lisp frames on Windows requires
// SEH .pdata tables that MCJIT doesn't reliably register — the
// process aborts with code 0xe06d7363 ("C++ exception not caught")
// when the unwinder hits a JIT frame.
//
// Instead we use a thread-local condition slot + handler-depth
// counter:
//
//   - `error` checks the depth. If zero (no handler installed),
//     it prints the message and aborts. If non-zero, it stores
//     the condition in the slot and returns NIL — allowing the
//     JIT frames above to drain naturally up the stack.
//   - `handler-case` increments the depth, clears the slot, runs
//     the body, decrements the depth, then inspects the slot. If
//     set, it dispatches to the handler.
//
// Limitation: between `error` and the matching handler-case, the
// JIT code unwinds via *normal returns*. If those frames perform
// trapping operations on the body's return value (e.g.
// `(car (error ...))` would call `car` on the NIL return) the
// trap fires before the handler sees the condition. In practice
// `error` is overwhelmingly the last thing in its branch, so this
// is rarely observable. A proper unwinding mechanism (ORC JIT or
// custom SEH registration) lands later.

use std::cell::Cell;

thread_local! {
    /// The condition value set by `error`, cleared by
    /// `handler-case` on entry, read by `handler-case` on exit.
    static CONDITION_SLOT: Cell<u64> = const { Cell::new(0) };
    /// Set if a condition has been signalled and not yet handled.
    /// Distinct from CONDITION_SLOT==0 because nil is a legal
    /// condition value.
    static CONDITION_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Nesting depth of active `handler-case` invocations.
    /// `error` aborts the process when this is zero.
    static HANDLER_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Convenience wrapper kept around for FFI symmetry — exposes the
/// condition payload type to callers that want to manipulate it
/// programmatically. Currently empty since the pending-condition
/// state lives in thread-locals.
#[derive(Debug, Clone)]
pub struct NclCondition {
    pub value: u64,
}

/// Signal a condition with the given message. Allocates a String
/// in the calling thread's young heap, then stashes it in the
/// condition slot if a handler is installed; otherwise prints and
/// aborts. Used by `ncl_call` / `ncl_load_value` / `ncl_load_function`
/// to turn unbound-symbol panics into catchable Lisp conditions.
pub fn signal_condition_string(
    mutator: *mut crate::mutator::MutatorState,
    msg: &str,
) -> u64 {
    if HANDLER_DEPTH.with(|d| d.get()) == 0 {
        die_unhandled_condition(msg);
    }
    let m = unsafe { &mut *mutator };
    let cond = crate::gc_string::alloc_string_in_young(m, msg);
    record_pending_condition(cond.raw())
}

/// Condition guard — enter a region in which signaled conditions
/// DEFER instead of terminating the process.
///
/// Used in two places that run Lisp code outside any `handler-case`:
///   1. The macroexpander, which invokes a macro's compiled expander
///      function directly. A condition the expander signals (undefined
///      function, type error, explicit `(error …)`) would otherwise
///      hit `HANDLER_DEPTH == 0` and exit the process — on Windows
///      with code 0xC0000409, visually identical to a stack overflow.
///   2. Top-level eval (the REPL, file loading, stdlib bootstrap),
///      so a condition that escapes the form becomes a recoverable
///      error instead of killing the whole session.
///
/// `condition_guard_enter` bumps `HANDLER_DEPTH` so signals defer
/// (record a pending condition and return NIL) and snapshots the
/// current pending state to restore on exit. Pair with
/// `condition_guard_exit`.
pub fn condition_guard_enter() -> (bool, u64) {
    let saved_pending = CONDITION_PENDING.with(|p| p.replace(false));
    let saved_slot = CONDITION_SLOT.with(|s| s.get());
    HANDLER_DEPTH.with(|d| d.set(d.get() + 1));
    (saved_pending, saved_slot)
}

/// Condition guard — exit. Restores `HANDLER_DEPTH` and the saved
/// pending state. If a condition was signaled during the guarded
/// region, returns `Some(cond_raw)` (the condition Word) and clears
/// `ABORT_PENDING` so subsequent work isn't poisoned by a stale flag;
/// otherwise returns `None`.
pub fn condition_guard_exit(saved: (bool, u64)) -> Option<u64> {
    HANDLER_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    let was_pending = CONDITION_PENDING.with(|p| p.replace(saved.0));
    let cond = CONDITION_SLOT.with(|s| s.replace(saved.1));
    if was_pending {
        ABORT_PENDING.with(|p| p.set(false));
        Some(cond)
    } else {
        None
    }
}

/// `(error condition-or-message)` — signal a condition. With a
/// matching `handler-case` on the stack, returns NIL after stashing
/// the condition; the unwind happens through normal stack returns.
/// With no handler active, prints the message and aborts the
/// process — matches CL's "unhandled condition enters the
/// debugger; here we just abort" simplification.
pub extern "C-unwind" fn error_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let cond = if n_args < 1 {
        Word::NIL.raw()
    } else {
        unsafe { *args }
    };
    if HANDLER_DEPTH.with(|d| d.get()) == 0 {
        // No handler — render the condition and exit cleanly.
        die_unhandled_condition(&crate::printer::format_word_aesthetic(Word::from_raw(cond)));
    }
    record_pending_condition(cond)
}

/// `(%handler-case body-thunk handler-fn)` — internal primitive
/// behind the `handler-case` macro. Calls `body-thunk` with no
/// args; if a condition was signalled during the body, invokes
/// `handler-fn` with the condition as its single argument and
/// returns the handler's value. Otherwise returns the body's
/// result.
pub extern "C-unwind" fn handler_case_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("handler-case expects 2 args (body-thunk, handler-fn), got {n_args}");
    }
    let body_word = Word::from_raw(unsafe { *args });
    let handler_word = Word::from_raw(unsafe { *args.add(1) });
    if body_word.tag() != Tag::Function || handler_word.tag() != Tag::Function {
        panic!("handler-case: both args must be functions");
    }

    // Save the outer handler's pending state — we want a handler
    // entered while another is unhandled to start clean and not
    // accidentally catch the outer's condition.
    let saved_slot = CONDITION_SLOT.with(|s| s.get());
    let saved_pending = CONDITION_PENDING.with(|p| p.replace(false));
    HANDLER_DEPTH.with(|d| d.set(d.get() + 1));

    let body_result = {
        let env = crate::gc_function::env(body_word);
        let code = crate::gc_function::code_ptr(body_word);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), std::ptr::null(), 0) }
    };

    HANDLER_DEPTH.with(|d| d.set(d.get() - 1));
    let was_pending = CONDITION_PENDING.with(|p| p.replace(saved_pending));
    let cond = CONDITION_SLOT.with(|s| s.replace(saved_slot));

    if was_pending {
        // We're consuming this condition — clear the unified
        // abort flag so the call-site instrumentation in the
        // handler body doesn't see a stale pending.
        ABORT_PENDING.with(|p| p.set(false));
        let env = crate::gc_function::env(handler_word);
        let code = crate::gc_function::code_ptr(handler_word);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), &cond as *const u64, 1) }
    } else {
        body_result
    }
}

// ---- Loop / return ---------------------------------------------------------
//
// `loop` and `return` use the same thread-local-flag mechanism as
// `error` / `handler-case` because we can't unwind through JIT
// frames on Windows. `%native-loop` calls a thunk repeatedly,
// checking `LOOP_BREAK_PENDING` after each iteration; `%loop-
// return` sets the flag.
//
// Same limitation as error: code between `(return v)` and the end
// of the iteration body still runs, since the JIT frames don't
// know about the flag and just continue. Putting `return` in a
// terminal position (the last form of a cond/case clause)
// sidesteps the issue, matching idiomatic CL anyway.

thread_local! {
    static LOOP_BREAK_PENDING: Cell<bool> = const { Cell::new(false) };
    static LOOP_BREAK_VALUE: Cell<u64> = const { Cell::new(0) };
}

/// `(%native-loop thunk)` — call `thunk` with no args repeatedly
/// until `(%loop-return v)` is signalled. Returns `v`.
///
/// Saves and restores the outer LOOP_BREAK state so nested loops
/// each have their own break.
pub extern "C-unwind" fn native_loop_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%native-loop expects 1 arg (thunk), got {n_args}");
    }
    let thunk = Word::from_raw(unsafe { *args });
    if thunk.tag() != Tag::Function {
        panic!("%native-loop: arg must be a function");
    }

    let saved_pending = LOOP_BREAK_PENDING.with(|p| p.replace(false));
    let saved_value = LOOP_BREAK_VALUE.with(|v| v.replace(0));

    let result = loop {
        let env = crate::gc_function::env(thunk);
        let code = crate::gc_function::code_ptr(thunk);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        let body_value = unsafe { f(mutator, env.raw(), std::ptr::null(), 0) };

        // Three exit paths the body can request:
        //
        //   (1) LOOP_BREAK_PENDING — set by `(return ...)` inside a
        //       simple loop. The loop_return_shim now ALSO sets
        //       ABORT_PENDING so the JIT's per-call abort-check
        //       early-returns through any nested call sites, giving
        //       (return) proper unwind semantics — forms following
        //       the (return) in the body don't run. Without that
        //       short-circuit, code like
        //
        //         (loop (if (< j i) (return)) (rotatef ...))
        //
        //       would still run the rotatef after the return, which
        //       broke corman's quicksort and any other code that
        //       relies on return-skips-tail semantics.
        //
        //   (2) ABORT_PENDING alone — a (return-from BLOCK V) or a
        //       signal-pending condition. Exit the loop and let the
        //       outer block / handler observe the flag.
        //
        //   (3) Body returns normally — iterate again.
        //
        // Check LOOP_BREAK first so we report the user's intended
        // exit value (not the body's tail value, which is NIL after
        // a return). Clear ABORT_PENDING only in path (1) since
        // path (2)'s abort needs to propagate up to a real handler.
        if LOOP_BREAK_PENDING.with(|p| p.get()) {
            ABORT_PENDING.with(|p| p.set(false));
            break LOOP_BREAK_VALUE.with(|v| v.get());
        }
        if ABORT_PENDING.with(|p| p.get()) {
            break body_value;
        }
    };

    LOOP_BREAK_PENDING.with(|p| p.set(saved_pending));
    LOOP_BREAK_VALUE.with(|v| v.set(saved_value));
    result
}

/// `(%loop-return value)` — signal that the current loop should
/// exit with `value`. Returns NIL; the JIT frames between this
/// call and the matching `%native-loop` see NIL and drain via
/// normal returns.
pub extern "C-unwind" fn loop_return_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let value = if n_args >= 1 {
        unsafe { *args }
    } else {
        Word::NIL.raw()
    };
    LOOP_BREAK_VALUE.with(|v| v.set(value));
    LOOP_BREAK_PENDING.with(|p| p.set(true));
    // Also raise ABORT_PENDING so the JIT's post-call abort check
    // unwinds out of any nested calls between the (return) site and
    // the enclosing %native-loop. Without this, forms following the
    // (return) inside the loop body would keep running. The matching
    // %native-loop clears ABORT_PENDING on exit (see the comment
    // there).
    ABORT_PENDING.with(|p| p.set(true));
    Word::NIL.raw()
}

// ---- File I/O shims --------------------------------------------------------
//
// Each shim has the standard JIT calling convention so it can be
// installed in a symbol's function cell via the install_native
// mechanism. Internally, all dispatch to the higher-level
// functions in `file_sys`.

fn arg(args: *const u64, i: u64) -> Word {
    Word::from_raw(unsafe { *args.add(i as usize) })
}

fn arg_fixnum(args: *const u64, i: u64) -> Option<i64> {
    arg(args, i).as_fixnum()
}

/// (open-input-file path) → handle (fixnum) or 0
pub extern "C-unwind" fn open_input_file_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-input-file: expected 1 arg (path), got {n_args}");
    }
    let h = crate::file_sys::open_file(arg(args, 0), crate::file_sys::Mode::Input);
    Word::fixnum(h).raw()
}

/// (open-output-file path) → handle (truncates existing)
pub extern "C-unwind" fn open_output_file_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-output-file: expected 1 arg (path), got {n_args}");
    }
    let h = crate::file_sys::open_file(arg(args, 0), crate::file_sys::Mode::Output);
    Word::fixnum(h).raw()
}

/// (open-append-file path) → handle (creates or appends)
pub extern "C-unwind" fn open_append_file_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-append-file: expected 1 arg (path), got {n_args}");
    }
    let h = crate::file_sys::open_file(arg(args, 0), crate::file_sys::Mode::Append);
    Word::fixnum(h).raw()
}

/// (close-stream handle) → t
pub extern "C-unwind" fn close_stream_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("close-stream: expected 1 arg (handle), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    crate::file_sys::close_file(h);
    Word::T.raw()
}

/// (read-line handle) → string or nil (EOF)
pub extern "C-unwind" fn read_line_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("read-line: expected 1 arg (handle), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    match crate::file_sys::read_line(h) {
        Some(s) => {
            let m = unsafe { &mut *mutator };
            crate::gc_string::alloc_string_in_young(m, &s).raw()
        }
        None => Word::NIL.raw(),
    }
}

/// (read-char-from handle) → char or nil (EOF)
pub extern "C-unwind" fn read_char_from_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("read-char-from: expected 1 arg (handle), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    match crate::file_sys::read_char(h) {
        Some(c) => Word::char(c).raw(),
        None => Word::NIL.raw(),
    }
}

/// (write-string-to handle string) → string
pub extern "C-unwind" fn write_string_to_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("write-string-to: expected 2 args (handle, string), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    let s_word = arg(args, 1);
    if s_word.tag() != Tag::String {
        panic!("write-string-to: second arg must be a string, got {s_word:?}");
    }
    let s: String = crate::gc_string::chars_of(s_word).collect();
    crate::file_sys::write_string(h, &s);
    s_word.raw()
}

/// (file-position handle) → fixnum or -1
pub extern "C-unwind" fn file_position_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("file-position: expected 1 arg (handle), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    Word::fixnum(crate::file_sys::file_position(h)).raw()
}

/// (file-length handle) → fixnum or -1
pub extern "C-unwind" fn file_length_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("file-length: expected 1 arg (handle), got {n_args}");
    }
    let h = arg_fixnum(args, 0).unwrap_or(0);
    Word::fixnum(crate::file_sys::file_length(h)).raw()
}

/// (file-exists path) → t or nil
pub extern "C-unwind" fn file_exists_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("file-exists: expected 1 arg (path), got {n_args}");
    }
    if crate::file_sys::file_exists(arg(args, 0)) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// (delete-file path) → t or nil
pub extern "C-unwind" fn delete_file_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("delete-file: expected 1 arg (path), got {n_args}");
    }
    if crate::file_sys::delete_file(arg(args, 0)) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// Lisp-callable shim for `append`. Binary append — concatenates
/// two lists; the second list's tail is shared (not copied). Used
/// by backquote-splicing macros, so it has to be available before
/// the user-Lisp stdlib loads. Variadic CL `append` lands when
/// `&rest` argument unpacking does.
pub extern "C-unwind" fn append_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("append: expected 2 args, got {n_args}");
    }
    let a = Word::from_raw(unsafe { *args });
    let b = Word::from_raw(unsafe { *args.add(1) });
    let m = unsafe { &mut *mutator };

    // Walk `a`, collecting cars; then cons onto `b` right-to-left.
    let mut cars: Vec<Word> = Vec::new();
    let mut cur = a;
    while !cur.is_nil() {
        if cur.tag() != Tag::Cons {
            panic!("append: first argument must be a proper list, got {cur:?}");
        }
        let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
        cars.push(Word::from_raw(unsafe { *p }));
        cur = Word::from_raw(unsafe { *p.add(1) });
    }

    let mut result = b;
    for car in cars.iter().rev() {
        result = m.alloc_cons(*car, result);
    }
    result.raw()
}

/// Lisp-callable shim for `format`. Has the standard JIT function
/// signature so it can be installed in a Symbol's function cell
/// just like a defun'd function — making `format` a first-class
/// function (callable via `#'`, `apply`, `funcall`).
///
/// Arity is "at least 2" (dest + control); subsequent args become
/// the rest list, which `run_format` consumes one-by-one as
/// directives are processed.
pub extern "C-unwind" fn format_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 2 {
        panic!("format: expected at least 2 args (dest, control), got {n_args}");
    }
    let dest = Word::from_raw(unsafe { *args });
    let ctrl = Word::from_raw(unsafe { *args.add(1) });

    // Build the rest list from args[2..] (right-to-left).
    let m = unsafe { &mut *mutator };
    let mut rest = Word::NIL;
    let mut i = n_args;
    while i > 2 {
        i -= 1;
        let arg = Word::from_raw(unsafe { *args.add(i as usize) });
        rest = m.alloc_cons(arg, rest);
    }

    crate::format::run_format(m, dest, ctrl, rest).raw()
}

/// `(apply fn prefix-arg-1 ... prefix-arg-N tail-list)` —
/// call `fn` with the prefix args followed by the spread elements
/// of `tail-list`. The compiler emits a call here with the prefix
/// already packed into a stack buffer; we walk the list to count
/// it, allocate one combined args buffer, copy the prefix in,
/// splat the list, and dispatch through `ncl_funcall`.
///
/// Empty prefix (just `(apply f lst)`) is supported by passing
/// `n_prefix = 0` and a null `prefix` pointer.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_apply(
    mutator: *mut crate::mutator::MutatorState,
    fn_word: u64,
    prefix: *const u64,
    n_prefix: u64,
    tail_list: u64,
) -> u64 {
    let raw_word = Word::from_raw(fn_word);
    let f_word = match resolve_function_designator(mutator, "apply", raw_word) {
        Ok(w) => w,
        Err(nil) => return nil,
    };

    // Walk tail_list to count its length, then build a combined
    // args buffer of n_prefix + tail_len Words.
    let mut tail_len: usize = 0;
    let mut cur = Word::from_raw(tail_list);
    while !cur.is_nil() {
        if cur.tag() != Tag::Cons {
            panic!("apply: tail argument must be a proper list, got {cur:?}");
        }
        tail_len += 1;
        let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
        cur = Word::from_raw(unsafe { *p.add(1) });
    }

    let total = n_prefix as usize + tail_len;
    if let Some(cond) = under_supplied_arity(mutator, f_word, total as u64) {
        return cond;
    }

    // Args buffer. Virtually every apply is small — (apply f a b
    // '(c d)) under mapcar/reduce — so use a stack array for ≤16
    // args and only heap-allocate above that. `Vec::new()` doesn't
    // allocate, so the fast path performs zero heap allocations.
    // Nothing between here and the call can trigger GC, so raw u64
    // words on the Rust stack are safe (and conservatively scanned
    // if the callee parks).
    const APPLY_STACK_ARGS: usize = 16;
    let mut stack_buf = [0u64; APPLY_STACK_ARGS];
    let mut heap_buf: Vec<u64> = Vec::new();
    let buf: &mut [u64] = if total <= APPLY_STACK_ARGS {
        &mut stack_buf[..total]
    } else {
        heap_buf.resize(total, 0);
        &mut heap_buf[..]
    };
    let mut k = 0;
    for i in 0..n_prefix {
        buf[k] = unsafe { *prefix.add(i as usize) };
        k += 1;
    }
    let mut cur = Word::from_raw(tail_list);
    while !cur.is_nil() {
        let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons");
        buf[k] = unsafe { *p };
        k += 1;
        cur = Word::from_raw(unsafe { *p.add(1) });
    }
    debug_assert_eq!(k, total, "apply: tail list changed length between walks");

    let env = crate::gc_function::env(f_word);
    debug_assert_closure_env_valid("ncl_apply", 0, f_word, env);
    let code = crate::gc_function::code_ptr(f_word);
    let f: crate::gc_function::LispCodeFn =
        unsafe { std::mem::transmute(code) };
    unsafe { f(mutator, env.raw(), buf.as_ptr(), total as u64) }
}

/// Build a freshly-allocated list of `args[start..n_args]` in
/// order. The variadic function-entry prologue calls this to bind
/// `&rest` parameters. If `n_args <= start`, returns NIL.
///
/// Allocation goes through the mutator's TLAB, so the resulting
/// list lives in the young heap and participates in normal GC.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_build_rest_list(
    mutator: *mut crate::mutator::MutatorState,
    args: *const u64,
    start: u64,
    n_args: u64,
) -> u64 {
    catch_gc_stall_as_condition(mutator, || {
        let m = unsafe { &mut *mutator };
        if n_args <= start {
            return Word::NIL.raw();
        }

        let root_base = m.root_count();
        for i in start..n_args {
            let arg_w = Word::from_raw(unsafe { *args.add(i as usize) });
            m.push_root(arg_w);
        }
        let result_idx = m.push_root(Word::NIL);

        let mut i = n_args;
        while i > start {
            i -= 1;
            let arg_idx = root_base + (i - start) as usize;
            let next = m.alloc_cons(m.root_at(arg_idx), m.root_at(result_idx));
            m.set_root_at(result_idx, next);
        }

        let result = m.pop_root().expect("rest-list result root missing");
        for _ in start..n_args {
            m.pop_root().expect("rest-list arg root missing");
        }
        result.raw()
    })
}

/// Mutate the i-th codepoint of a string. JIT'd
/// `(setf (aref s i) c)` and `(setf (char s i) c)` lower here.
/// Returns the character word that was stored.
///
/// No write barrier: strings hold scalar codepoints, so a string
/// store can never create an old→young pointer.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_string_set(s: u64, idx: u64, ch: u64) -> u64 {
    let ws = Word::from_raw(s);
    if ws.tag() != Tag::String {
        panic!("(setf (aref string ...)): argument is not a string: {ws:?}");
    }
    let n = crate::gc_string::char_count(ws);
    let i = idx as usize;
    if i >= n {
        panic!("(setf (aref s {i}) ...): index out of bounds for length {n}");
    }
    let cw = Word::from_raw(ch);
    let cp = cw
        .as_char()
        .unwrap_or_else(|| panic!("(setf (aref string ...)): value is not a character: {cw:?}"))
        as u32;
    crate::gc_string::set_char_at(ws, i, cp);
    ch
}

// ───────────────────────────────────────────────────────────────────
// Foundation utilities for CLOS port: gensym, eql, typep.
//
// These are deliberately small. `gensym` and `eql` are essentially
// one-liners on top of existing primitives; `typep` dispatches on
// the Word tag plus a few keyword type names. Together they're the
// first prerequisite chunk for porting Closette.
// ───────────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for gensym names. Reset would defeat the
/// purpose (the symbols are interned by their generated name) so
/// we never reset it. AtomicU64 is sufficient — 2^64 gensyms is
/// not a realistic concern.
static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `(gensym)` / `(gensym prefix)` — return a freshly interned
/// symbol whose name is `prefix` followed by an unused decimal
/// integer. Default prefix is "G". CL's gensym returns an
/// uninterned symbol; we don't have uninterned symbols yet, so
/// we intern a name guaranteed never to collide with anything
/// the user has typed (the counter only goes up).
pub extern "C-unwind" fn gensym_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let prefix = if n_args >= 1 {
        let w = Word::from_raw(unsafe { *args });
        if w.tag() == Tag::String {
            crate::gc_string::chars_of(w).collect::<String>()
        } else {
            "G".to_string()
        }
    } else {
        "G".to_string()
    };
    let n = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("{prefix}{n}");
    let m = unsafe { &mut *mutator };
    m.coord().intern(&name).raw()
}

/// `(car x)` — Lisp-callable shim. CAR is a special form
/// lowering to Expr::Car; this shim covers `#'car` for use
/// with funcall / map / :key etc.
pub extern "C-unwind" fn car_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("car: expected 1 arg, got {n_args}");
    }
    let w = Word::from_raw(unsafe { *args });
    if w.is_nil() {
        return Word::NIL.raw();
    }
    ncl_car(unsafe { *args })
}

/// `(cdr x)` — sibling shim for car. NIL is a fixed point.
pub extern "C-unwind" fn cdr_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("cdr: expected 1 arg, got {n_args}");
    }
    let w = Word::from_raw(unsafe { *args });
    if w.is_nil() {
        return Word::NIL.raw();
    }
    ncl_cdr(unsafe { *args })
}

/// `(list e1 e2 …)` — Lisp-callable shim. LIST is a special
/// form (desugars to nested cons in the JIT's lowering), but
/// `#'list` needs a function value for things like
/// `(mapcar #'list xs ys)`.
pub extern "C-unwind" fn list_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let mut acc = Word::NIL;
    for i in (0..n_args).rev() {
        let elem = unsafe { *args.add(i as usize) };
        acc = m.alloc_cons(Word::from_raw(elem), acc);
    }
    acc.raw()
}

/// `(cons a b)` — Lisp-callable shim.
pub extern "C-unwind" fn cons_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("cons: expected 2 args, got {n_args}");
    }
    let car = unsafe { *args };
    let cdr = unsafe { *args.add(1) };
    ncl_alloc_cons(mutator, car, cdr)
}

/// `(equal a b)` — Lisp-callable shim around `ncl_equal`. The
/// EQUAL special form lowers directly to Expr::Equal; this shim
/// is needed for `#'equal` (taking the function as a value, e.g.
/// `(member item lst :test #'equal)`).
pub extern "C-unwind" fn equal_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("equal: expected 2 args, got {n_args}");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    ncl_equal(a, b)
}

/// `(eq a b)` — Lisp-callable shim. EQ is also a special form;
/// this is for `#'eq`.
pub extern "C-unwind" fn eq_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("eq: expected 2 args, got {n_args}");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if a == b { Word::T.raw() } else { Word::NIL.raw() }
}

/// `(eql a b)` — true if the two values are the same object, or two
/// numbers of the same type with the same value, or the same
/// character. The work is in `eql_values`: identity covers fixnums,
/// chars, symbols, NIL/T and same-pointer objects, and the boxed
/// numeric tower (float/bignum/ratio/complex) gets a same-type +
/// same-value comparison. `(eql a b)` lowers to a call into this shim
/// (it is NOT inlined like `eq`); the EQL special-form lowering was
/// removed so `eq` and `eql` no longer share `Expr::Eq`.
pub extern "C-unwind" fn eql_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(
            mutator,
            &format!("eql: expected 2 args, got {n_args}"),
        );
    }
    let a = Word::from_raw(unsafe { *args });
    let b = Word::from_raw(unsafe { *args.add(1) });
    if eql_values(a, b) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

// ─── Numeric comparison + length shims ──────────────────────────────────
//
// The compiler lowers `(< a b)`, `(>= a b)`, `(length x)` etc. as
// special forms, so direct use is fast. But `#'<`, `#'length`,
// `(funcall #'< 1 2)`, `(every #'< xs ys)` — every first-class
// reference to one of these names — needs an actual function
// object in the symbol's function cell. Without that, the load
// of `<`'s function cell returns UNBOUND and the call fails with
// `undefined function: <`.
//
// These shims fix that. Each is a one-line wrapper over the
// matching primitive (`ncl_cmp_full` for the comparison family,
// `ncl_length` for the polymorphic length). The arithmetic
// behaviour is unchanged — direct-call lowering still emits
// inline IR; only the funcall path now has a real callable.

/// `(< a b)` — numeric less-than across the full numeric tower.
pub extern "C-unwind" fn lt_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "<: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if !crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        && crate::ratio::ncl_cmp_full(a, b) < 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(> a b)`.
pub extern "C-unwind" fn gt_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, ">: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if !crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        && crate::ratio::ncl_cmp_full(a, b) > 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(<= a b)`.
pub extern "C-unwind" fn le_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "<=: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if !crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        && crate::ratio::ncl_cmp_full(a, b) <= 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(>= a b)`.
pub extern "C-unwind" fn ge_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, ">=: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if !crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        && crate::ratio::ncl_cmp_full(a, b) >= 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(= a b)` — numeric equality (NOT object identity; `=` walks the
/// numeric tower so `(= 1 1.0)` is T even though `(eql 1 1.0)` is NIL).
pub extern "C-unwind" fn num_eq_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "=: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if !crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        && crate::ratio::ncl_cmp_full(a, b) == 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(/= a b)` — numeric inequality. The inverse of `=`, not the
/// inverse of `EQL`. (No equivalent compiler special form, so
/// this is the *only* way `/=` works at all — direct calls land
/// here too.)
pub extern "C-unwind" fn num_ne_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "/=: expected 2 args");
    }
    let a = unsafe { *args };
    let b = unsafe { *args.add(1) };
    if crate::float::cmp_unordered(Word::from_raw(a), Word::from_raw(b))
        || crate::ratio::ncl_cmp_full(a, b) != 0
    {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(length x)` — polymorphic on tag: codepoint count for strings,
/// element count for vectors, spine length for cons-list. The
/// underlying `ncl_length` already implements this for the
/// direct-call path; we expose it as a callable.
pub extern "C-unwind" fn length_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "length: expected 1 arg");
    }
    ncl_length(unsafe { *args })
}

// ─── Arithmetic shims (the funcall path) ────────────────────────────────
//
// `+`, `-`, `*` are compiler special forms — the direct-call path
// generates inline IR through `Expr::add` / `Expr::sub` / `Expr::mul`
// and only spills to `ncl_*_full` on overflow / non-fixnum operands.
// But the symbols' function cells need real callables too, or
// `#'+` / `(funcall #'+ …)` / `(mapcar #'+ …)` crash with
// "undefined function: +".
//
// These are binary shims (CL's +/-/* are variadic but here we expose
// the two-arg fold step — `(mapcar #'+ '(1 2) '(3 4))` calls the
// shim with exactly two args at each step). The Lisp side already
// has variadic wrappers via the special-form lowering for direct
// calls; the funcall surface is the only thing that goes through
// these shims today.

/// `(+ &rest args)` — CL spec: variadic fold over the numeric
/// tower, with `(+)` returning 0 (the additive identity). Each
/// fold step calls `ncl_add_full`, which handles fixnum / bignum
/// / ratio / float / complex transparently.
pub extern "C-unwind" fn add_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let mut acc = Word::fixnum(0).raw();
    for i in 0..n_args {
        let next = unsafe { *args.add(i as usize) };
        acc = crate::ratio::ncl_add_full(mutator, acc, next);
        if ABORT_PENDING.with(|p| p.get()) {
            return acc;
        }
    }
    acc
}

/// `(- a)` negates; `(- a b c …)` is `a - b - c - …`. `(- )` with no
/// args is a CL spec error (and we follow suit).
pub extern "C-unwind" fn sub_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args == 0 {
        return signal_condition_string(mutator, "-: expected at least 1 arg");
    }
    let first = unsafe { *args };
    if n_args == 1 {
        // Unary negate: 0 - first.
        let zero = Word::fixnum(0).raw();
        return crate::ratio::ncl_sub_full(mutator, zero, first);
    }
    let mut acc = first;
    for i in 1..n_args {
        let next = unsafe { *args.add(i as usize) };
        acc = crate::ratio::ncl_sub_full(mutator, acc, next);
        if ABORT_PENDING.with(|p| p.get()) {
            return acc;
        }
    }
    acc
}

/// `(* &rest args)` — variadic fold; `(*)` returns 1 (the
/// multiplicative identity).
pub extern "C-unwind" fn mul_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let mut acc = Word::fixnum(1).raw();
    for i in 0..n_args {
        let next = unsafe { *args.add(i as usize) };
        acc = crate::ratio::ncl_mul_full(mutator, acc, next);
        if ABORT_PENDING.with(|p| p.get()) {
            return acc;
        }
    }
    acc
}

/// `(typep object type-spec)` — true if `object` belongs to the
/// CL type named by `type-spec`. `type-spec` is a symbol; compound
/// type specs like `(integer 0 100)` are not yet supported.
///
/// Recognised type names (case-insensitive comparison via the
/// printer name): T, NIL, NULL, ATOM, CONS, LIST, SYMBOL, KEYWORD,
/// FIXNUM, INTEGER, NUMBER, RATIONAL, REAL, STRING, SIMPLE-STRING,
/// CHARACTER, FUNCTION, VECTOR, SIMPLE-VECTOR, ARRAY, SEQUENCE,
/// HASH-TABLE, STANDARD-OBJECT.
///
/// Types we don't yet have storage for (vectors, arrays, hash
/// tables, standard objects) always return NIL; they'll start
/// returning T once the relevant chunks land.
pub extern "C-unwind" fn typep_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("typep: expected 2 args (object type), got {n_args}");
    }
    let obj = Word::from_raw(unsafe { *args });
    let type_w = Word::from_raw(unsafe { *args.add(1) });
    if type_w.tag() != Tag::Symbol {
        // Compound specs — `(integer ...)`, `(or ...)`, etc.
        // — fall through as "unsupported, returns NIL" for now.
        return Word::NIL.raw();
    }
    let name = match crate::sym_names::lookup(type_w.raw()) {
        Some(n) => n,
        None => return Word::NIL.raw(),
    };
    let matches = match name.as_ref() {
        "T" => true,
        "NIL" | "NULL" => obj.is_nil(),
        "ATOM" => obj.tag() != Tag::Cons,
        "CONS" => obj.tag() == Tag::Cons,
        "LIST" => obj.is_nil() || obj.tag() == Tag::Cons,
        "SYMBOL" => obj.tag() == Tag::Symbol || obj.is_nil(),
        "KEYWORD" => is_keyword(obj),
        "FIXNUM" => obj.tag() == Tag::Fixnum,
        "BIGNUM" => crate::bignum::is_bignum(obj),
        "INTEGER" => crate::bignum::is_integer(obj),
        "RATIO" => crate::ratio::is_ratio(obj),
        "RATIONAL" => crate::ratio::is_rational(obj),
        "FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" |
        "LONG-FLOAT" => crate::float::is_float(obj),
        "COMPLEX" => crate::complex::is_complex(obj),
        "REAL" => crate::float::is_real(obj) || crate::ratio::is_ratio(obj),
        "NUMBER" => crate::complex::is_number(obj),
        "STRING" | "SIMPLE-STRING" => obj.tag() == Tag::String,
        "CHARACTER" => obj.as_char().is_some(),
        "FUNCTION" => obj.tag() == Tag::Function,
        "VECTOR" | "SIMPLE-VECTOR" | "ARRAY" => {
            // Bignums, floats, ratios, complex share Tag::Vector
            // but aren't vectors at the language level.
            obj.tag() == Tag::Vector
                && !crate::bignum::is_bignum(obj)
                && !crate::float::is_float(obj)
                && !crate::ratio::is_ratio(obj)
                && !crate::complex::is_complex(obj)
        }
        "SEQUENCE" => {
            obj.is_nil()
                || obj.tag() == Tag::Cons
                || obj.tag() == Tag::String
                || obj.tag() == Tag::Vector
        }
        "HASH-TABLE" => false, // future
        "STANDARD-OBJECT" => false, // future
        _ => false,
    };
    if matches { Word::T.raw() } else { Word::NIL.raw() }
}

/// Keywords are symbols whose printer name starts with `:` (the
/// reader interns them with the leading colon).
fn is_keyword(w: Word) -> bool {
    if w.tag() != Tag::Symbol {
        return false;
    }
    crate::sym_names::lookup(w.raw())
        .map(|n| n.starts_with(':'))
        .unwrap_or(false)
}

// ───────────────────────────────────────────────────────────────────
// Multiple values — thread-local "extra return values" buffer.
//
// Per CL, a function that returns normally produces one value, but
// `(values v1 v2 ... vN)` lets it return N. The receiving site
// (multiple-value-bind / multiple-value-list) reads them.
//
// Storage: a thread-local Vec<u64>. The compiler enforces the
// invariant that, after every function call, the slot contains
// exactly the called function's return values:
//   - If the function's tail expression was `(values ...)`,
//     `Expr::Values` wrote the full set.
//   - Otherwise the tail-position transform wraps the body in
//     `Expr::EnsureSingleMv`, which writes [primary] at exit.
//
// Without that invariant, a subroutine call inside a function would
// pollute the slot, and the outer multiple-value-bind would observe
// stale data. With the invariant, the slot is always fresh.
//
// JIT-callable. Lowered from `Expr::Values` and `Expr::EnsureSingleMv`.
// ───────────────────────────────────────────────────────────────────

use std::cell::RefCell;

thread_local! {
    static MV_VALUES: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// `(%mv-clear)` — empty the multi-value slot. The
/// multiple-value-bind / multiple-value-list macros call this
/// before evaluating their form so that, if the form turns out to
/// be a non-function expression (constant, variable lookup, native
/// shim call), the slot is observably empty and the macro falls
/// back to `[primary]`. JIT-compiled function calls overwrite the
/// slot via EnsureSingleMv or Expr::Values; native shim calls
/// don't, hence the need for an explicit pre-clear.
pub extern "C-unwind" fn mv_clear_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    MV_VALUES.with(|cell| cell.borrow_mut().clear());
    Word::NIL.raw()
}

/// Write `[v]` into the multi-value slot. Used by the
/// EnsureSingleMv epilogue around any function body whose tail
/// expression isn't `(values ...)`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_set_mv_single(v: u64) {
    MV_VALUES.with(|cell| {
        let mut b = cell.borrow_mut();
        b.clear();
        b.push(v);
    });
}

/// `(values v1 v2 ... vN)` as a Lisp-callable FUNCTION — so
/// `#'values` works and `(apply #'values list)` does the right
/// thing. The compiler keeps its special-form fast path for
/// direct `(values …)` calls; this shim exists for the
/// function-value path.
pub extern "C-unwind" fn values_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args == 0 {
        MV_VALUES.with(|c| c.borrow_mut().clear());
        return Word::NIL.raw();
    }
    unsafe { ncl_set_mv_many(args, n_args) };
    unsafe { *args }
}

/// `(values-list list)` — splay LIST as multiple values. Returns
/// the first element (or NIL on empty list).
pub extern "C-unwind" fn values_list_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("values-list: expected 1 arg (list), got {n_args}");
    }
    // Walk the list, collecting raw words into a Vec.
    let mut collected: Vec<u64> = Vec::new();
    let mut cur = Word::from_raw(unsafe { *args });
    while !cur.is_nil() {
        if cur.tag() != Tag::Cons {
            panic!("values-list: argument is not a proper list");
        }
        let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons ptr");
        collected.push(unsafe { *p });
        cur = Word::from_raw(unsafe { *p.add(1) });
    }
    MV_VALUES.with(|cell| {
        let mut b = cell.borrow_mut();
        b.clear();
        for &v in &collected {
            b.push(v);
        }
    });
    collected.first().copied().unwrap_or(Word::NIL.raw())
}

/// Write `args[0..n]` into the multi-value slot. Used by
/// `Expr::Values` for `(values v1 v2 ... vN)` in tail position.
///
/// SAFETY: `args` must point to at least `n` valid `u64`s.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_set_mv_many(args: *const u64, n: u64) {
    MV_VALUES.with(|cell| {
        let mut b = cell.borrow_mut();
        b.clear();
        for i in 0..n {
            b.push(unsafe { *args.add(i as usize) });
        }
    });
}

/// `(multiple-value-list-of primary)` — return the multi-value slot
/// as a fresh Lisp list. If the slot is somehow empty (shouldn't
/// happen given the invariant, but defensive), fall back to
/// `(primary)`. Used inside the `multiple-value-bind` /
/// `multiple-value-list` macro expansions, called immediately
/// after the form being inspected.
pub extern "C-unwind" fn multiple_value_list_of_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!(
            "multiple-value-list-of: expected 1 arg (primary), got {n_args}"
        );
    }
    let primary = unsafe { *args };
    let m = unsafe { &mut *mutator };
    let snapshot: Vec<u64> = MV_VALUES.with(|cell| cell.borrow().clone());
    let source = if snapshot.is_empty() {
        vec![primary]
    } else {
        snapshot
    };
    let mut acc = Word::NIL;
    for &v in source.iter().rev() {
        acc = m.alloc_cons(Word::from_raw(v), acc);
    }
    acc.raw()
}

// ───────────────────────────────────────────────────────────────────
// block / return-from — named non-local exits.
//
// CL `(block NAME body)` establishes a named exit point;
// `(return-from NAME val)` returns VAL from the matching block.
//
// Implementation mirrors the existing error / handler-case flag
// mechanism: a thread-local "abort pending" slot per active
// block. return-from sets the slot; block on its way out checks
// the slot and either returns the captured value (matching
// name) or leaves the slot set so a surrounding block (or
// no-block-at-all panic) sees it.
//
// Trade-off shared with handler-case: the flag doesn't abort
// the body's execution. Code after a (return-from N V) call
// runs to completion before block reads the flag. For tail-
// position return-from this is fine; for non-tail uses inside
// a do-loop with code after the loop, the trailing code runs.
// (See setf-getf* in Closette for the worst-case shape.)
// Promoting to call-site instrumentation lands when handler-
// case grows the same.
// ───────────────────────────────────────────────────────────────────

struct BlockFrame {
    name: u64,
    value: u64,
}

thread_local! {
    /// Stack of active block frames in dynamic-extent order.
    /// Innermost is at the back. Each (block N body) pushes a
    /// frame on entry and pops on exit; (return-from N val)
    /// walks the stack for matching name, sets value on the
    /// frame, and signals "abort pending."
    static BLOCK_STACK: RefCell<Vec<BlockFrame>> = const { RefCell::new(Vec::new()) };

    /// Name (raw symbol Word) of the block return-from is
    /// targeting. Only meaningful when ABORT_PENDING is true
    /// AND the abort is a block return (not a condition signal).
    static BLOCK_TARGET: Cell<u64> = const { Cell::new(0) };
}

// Unified non-local exit flag. Set by either `error` or
// `return-from`; cleared by whichever (handler-case / native-
// block) ends up consuming it. The JIT instruments every Lisp
// function call to check this — if set, the calling function
// returns immediately without executing further forms,
// propagating the abort up to its consumer.
//
// This is the call-site instrumentation that makes (return-from
// …) actually abort the rest of the body, not just set a flag
// that's checked at block boundary. Same fix applies to (error
// …): code after `(error 'foo)` no longer runs.
thread_local! {
    pub(crate) static ABORT_PENDING: Cell<bool> = const { Cell::new(false) };
}

/// JIT-callable check. Returns 1 if a non-local exit is
/// pending; 0 otherwise. Lowered as an inline call after every
/// Expr::Call / Funcall / Apply.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_abort_pending() -> i32 {
    if ABORT_PENDING.with(|c| c.get()) {
        1
    } else {
        0
    }
}

/// JIT-callable explicit root push. Returns the root depth before
/// the push, mirroring `MutatorState::push_root`. Used by the runtime
/// itself (rest-list builder, make-closure); the JIT's safepoint wrap
/// goes through `ncl_roots_reserve` + inline stores instead.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_push_root(mutator: *mut MutatorState, w: u64) -> u64 {
    let m = unsafe { &mut *mutator };
    m.push_root(Word::from_raw(w)) as u64
}

/// JIT-callable batch root reservation. Ensures at least `n` free
/// slots above `cur` (growing/moving the buffer if needed) and returns
/// the `{cur, end}` header pointer. The JIT emits ONE call per
/// allocating call-site, then stores its `n` roots straight into the
/// reserved slots and bumps `cur` inline — replacing the old per-root
/// `ncl_push_root` / `ncl_pop_root` call storm (55-77% of call-heavy
/// runtime). The header pointer is stable for the mutator's lifetime,
/// so the post-call pop reloads `cur` from it (handling a realloc a
/// callee may have caused).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_roots_reserve(
    mutator: *mut MutatorState,
    n: u64,
) -> *mut crate::mutator::RootStackHdr {
    let m = unsafe { &mut *mutator };
    m.roots_reserve(n as usize)
}

/// JIT-callable explicit root pop. Returns the popped raw Word.
/// Popping an empty root stack is a compiler/runtime contract bug.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_pop_root(mutator: *mut MutatorState) -> u64 {
    let m = unsafe { &mut *mutator };
    m.pop_root()
        .expect("ncl_pop_root called with an empty root stack")
        .raw()
}

/// Debug trap for `(car x)` / `(cdr x)` when `x` is neither NIL nor
/// a Cons. The JIT calls this after the nil-check in
/// `emit_car_or_cdr` if `NCL_TRAP_BAD_CONS=1` was set at process
/// start; otherwise it isn't emitted at all (zero runtime cost).
///
/// On a bad Word the helper prints a structured line naming the
/// raw value, the tag bits, and the canonical Tag interpretation,
/// then aborts so the SEH crash handler in `brk.rs` writes the
/// full register / stack dump. The aborted-here site is much
/// closer to the corruption source than the eventual NULL deref
/// in the JIT-emitted `untag-and-load`.
///
/// Returns the input Word unchanged on the happy path so the
/// JIT can chain it inline: `let validated = ncl_debug_check_cons(w);`
/// then untag-and-load on `validated`.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_debug_check_cons(w: u64) -> u64 {
    let tag_bits = w & crate::word::TAG_MASK;
    let payload = w & !crate::word::TAG_MASK;

    // Gate 1: tag must be Cons.
    let tag_ok = tag_bits == Tag::Cons as u64;

    // Gate 2: payload must point above the Windows null-page reserve
    // (the bottom 64 KB is always unmapped on Windows, and 0..32 KB
    // on most Linuxes too). Any "Cons" Word whose payload lands
    // there is from a Fixnum-0 / NULL-pointer corruption — the
    // tag bits LOOK right but the address is impossible. Catching
    // these closes the case rbx left open in the GETF crash:
    // tag=Cons, payload=very-small-or-zero.
    const NULL_PAGE_CEILING: u64 = 0x1_0000; // 64 KB
    let payload_ok = payload >= NULL_PAGE_CEILING;

    if tag_ok && payload_ok {
        return w;
    }

    let tag = Word::from_raw(w).tag();
    let reason = match (tag_ok, payload_ok) {
        (false, _) => "wrong tag (not Cons, not NIL)",
        (true, false) => "Cons tag but payload in null-page region (likely zeroed-recycle)",
        (true, true) => unreachable!(),
    };
    eprintln!(
        "[CAR/CDR TRAP] bad cons word: {w:#018x}  tag={tag_bits:03b} ({tag:?})  \
         payload={payload:#x}  reason: {reason}"
    );
    eprintln!(
        "[CAR/CDR TRAP] JIT-emitted car/cdr would untag to {payload:#x} and \
         load from there. Forcing a NULL-deref now so the SEH crash \
         dumper in brk.rs catches it and writes a full stack/register \
         report — `abort()` would skip the SEH filter and we'd lose \
         the trace. Re-run with NCL_TRAP_BAD_CONS unset to disable."
    );
    // Force the same fault the JIT-emitted code WOULD have taken,
    // routing through SetUnhandledExceptionFilter → brk's dumper.
    // The crash dump's stack will show the JIT-emitted car/cdr call
    // site as the topmost frame.
    unsafe {
        let null: *mut u64 = core::ptr::null_mut();
        core::ptr::write_volatile(null, 0);
    }
    // Unreachable: the write_volatile above always faults on
    // Windows / Linux because page 0 is unmapped.
    std::process::abort();
}

/// `(%native-block NAME THUNK)` — the primitive behind the
/// `block` macro. Pushes a frame, invokes the body thunk, then
/// inspects the abort-pending flag: if set and targeting our
/// name, consume it and return the frame's value; otherwise
/// pass the body's value through (or leave the abort pending
/// for a surrounding block to catch).
pub extern "C-unwind" fn native_block_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%native-block: expected 2 args (name thunk), got {n_args}");
    }
    let name = unsafe { *args };
    let thunk = Word::from_raw(unsafe { *args.add(1) });
    if thunk.tag() != Tag::Function {
        panic!("%native-block: thunk must be a function, got {thunk:?}");
    }

    BLOCK_STACK.with(|s| {
        s.borrow_mut().push(BlockFrame {
            name,
            value: Word::NIL.raw(),
        });
    });

    let body_result = {
        let env = crate::gc_function::env(thunk);
        let code = crate::gc_function::code_ptr(thunk);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), std::ptr::null(), 0) }
    };

    // Pop our frame and decide what to do based on the abort flag.
    let our_frame_value = BLOCK_STACK.with(|s| {
        s.borrow_mut()
            .pop()
            .expect("BLOCK_STACK underflow at native-block exit")
            .value
    });

    let pending = ABORT_PENDING.with(|p| p.get());
    if pending {
        // Abort might be ours (a return-from targeting our name)
        // or somebody else's (a condition, or a return-from
        // targeting an outer block). BLOCK_TARGET == 0 means it's
        // not a block abort at all (it's a condition).
        let target = BLOCK_TARGET.with(|t| t.get());
        if target != 0 && target == name {
            // The abort targets us — consume it and return the
            // value the matching return-from stashed on our frame.
            ABORT_PENDING.with(|p| p.set(false));
            BLOCK_TARGET.with(|t| t.set(0));
            return our_frame_value;
        }
        // CL `(return v)` ≡ `(return-from nil v)`. NCL's `return`
        // lowers to the loop-break primitive (`%loop-return`), which
        // an enclosing `%native-loop` consumes. When the nearest
        // enclosing exit point is a `(block nil …)` instead of a loop,
        // THIS block must consume the loop-break: it gives `return` its
        // block semantics and stops a stray break from propagating to
        // the top level and aborting the load. Only a block actually
        // named NIL qualifies; loops, being innermost, still consume
        // their own breaks first.
        if name == Word::NIL.raw() && LOOP_BREAK_PENDING.with(|p| p.get()) {
            LOOP_BREAK_PENDING.with(|p| p.set(false));
            ABORT_PENDING.with(|p| p.set(false));
            return LOOP_BREAK_VALUE.with(|v| v.get());
        }
        // Not our abort — leave the flag pending so a surrounding
        // block (or handler-case for a condition) catches it.
        // Drop body_result.
    }
    body_result
}

/// `(%return-from NAME VALUE)` — find the topmost matching block
/// on BLOCK_STACK, write VALUE into its frame, set the abort
/// flag. Returns NIL (the body keeps running until natural exit;
/// see module docstring for the trade-off). Panics if no
/// matching block is currently active.
pub extern "C-unwind" fn return_from_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%return-from: expected 2 args (name value), got {n_args}");
    }
    let name = unsafe { *args };
    let val = unsafe { *args.add(1) };

    let found = BLOCK_STACK.with(|s| {
        let mut stack = s.borrow_mut();
        let pos = stack.iter().rposition(|f| f.name == name);
        match pos {
            Some(i) => {
                stack[i].value = val;
                true
            }
            None => false,
        }
    });
    if !found {
        // No matching block on the stack. This is a CONTROL-ERROR in
        // CL, not an internal invariant — e.g. `(return-from foo)`
        // where the enclosing `(defun foo …)` didn't establish an
        // implicit block, or a stale closure outliving its block.
        // Signal a catchable condition (handler-case sees it; with no
        // handler the process exits cleanly) rather than panicking,
        // which would unwind through JIT frames and abort the worker.
        let sym_name = crate::sym_names::lookup(name)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        return signal_condition_string(
            mutator,
            &format!("return-from: no enclosing block named {sym_name}"),
        );
    }
    ABORT_PENDING.with(|p| p.set(true));
    BLOCK_TARGET.with(|t| t.set(name));
    Word::NIL.raw()
}

/// `(%native-unwind-protect protected-thunk cleanup-thunk)` — the
/// primitive behind `unwind-protect`. Calls the protected thunk,
/// saves and clears any pending abort state, calls the cleanup
/// thunk to completion, then restores the abort state and returns
/// the value from the protected form.
///
/// This ensures cleanup always runs, even when a non-local exit
/// (return-from, error) was initiated inside the protected form.
/// The cleanup's own return value is discarded.
pub extern "C-unwind" fn unwind_protect_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%native-unwind-protect: expected 2 args (protected cleanup), got {n_args}");
    }
    let protected = Word::from_raw(unsafe { *args });
    let cleanup = Word::from_raw(unsafe { *args.add(1) });
    if protected.tag() != Tag::Function {
        panic!("%native-unwind-protect: protected must be a function, got {protected:?}");
    }
    if cleanup.tag() != Tag::Function {
        panic!("%native-unwind-protect: cleanup must be a function, got {cleanup:?}");
    }

    // 1. Call the protected thunk.
    let body_result = {
        let env = crate::gc_function::env(protected);
        let code = crate::gc_function::code_ptr(protected);
        let f: crate::gc_function::LispCodeFn = unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), std::ptr::null(), 0) }
    };

    // 2. Save and clear any pending abort so cleanup runs fully.
    //    Covers all three exit kinds: block-return (BLOCK_TARGET),
    //    catch-throw (THROW_PENDING/THROW_TAG), and conditions (the
    //    ABORT_PENDING flag alone).
    let saved_abort = ABORT_PENDING.with(|p| p.get());
    let saved_target = BLOCK_TARGET.with(|t| t.get());
    let saved_throw = THROW_PENDING.with(|p| p.get());
    let saved_throw_tag = THROW_TAG.with(|t| t.get());
    ABORT_PENDING.with(|p| p.set(false));
    BLOCK_TARGET.with(|t| t.set(0));
    THROW_PENDING.with(|p| p.set(false));
    THROW_TAG.with(|t| t.set(0));

    // 3. Call the cleanup thunk (its return value is discarded).
    {
        let env = crate::gc_function::env(cleanup);
        let code = crate::gc_function::code_ptr(cleanup);
        let f: crate::gc_function::LispCodeFn = unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), std::ptr::null(), 0) };
    }

    // 4. Restore abort state so the surrounding block / catch /
    //    handler-case can observe and handle it.
    ABORT_PENDING.with(|p| p.set(saved_abort));
    BLOCK_TARGET.with(|t| t.set(saved_target));
    THROW_PENDING.with(|p| p.set(saved_throw));
    THROW_TAG.with(|t| t.set(saved_throw_tag));

    // 5. Return the protected form's value.
    body_result
}

// ---- catch / throw ---------------------------------------------------------
//
// CATCH/THROW are the dynamic (run-time-tagged) cousins of BLOCK/
// RETURN-FROM. A `(catch TAG body…)` establishes a dynamic exit point
// keyed by the *value* of TAG (compared with EQ — raw Word identity);
// `(throw TAG value)` transfers control to the nearest enclosing catch
// whose tag is EQ to TAG, carrying VALUE out.
//
// Mechanism mirrors BLOCK exactly: a thread-local CATCH_STACK of
// frames, the shared ABORT_PENDING flag for call-site instrumentation,
// plus a dedicated THROW_PENDING boolean that discriminates a throw
// abort from a block-return / condition / loop-break abort. We need an
// explicit boolean (rather than "tag != 0" the way BLOCK keys off
// BLOCK_TARGET) because a catch tag can legitimately be any value,
// including NIL whose raw bits may be 0.

struct CatchFrame {
    tag: u64,
    value: u64,
}

thread_local! {
    /// Active catch frames in dynamic-extent order; innermost at the
    /// back. `(catch TAG …)` pushes on entry, pops on exit.
    static CATCH_STACK: RefCell<Vec<CatchFrame>> = const { RefCell::new(Vec::new()) };

    /// True while a `throw` is propagating. Distinguishes a throw
    /// abort from block / condition / loop aborts (which leave this
    /// false). Cleared by the catch that consumes the throw.
    static THROW_PENDING: Cell<bool> = const { Cell::new(false) };

    /// Raw Word of the tag the in-flight throw is seeking. Only
    /// meaningful while THROW_PENDING is true.
    static THROW_TAG: Cell<u64> = const { Cell::new(0) };
}

/// `(%native-catch TAG THUNK)` — the primitive behind the `catch`
/// macro. TAG is pre-evaluated (catch tags are dynamic). Pushes a
/// frame, runs the body thunk, then consumes the abort iff a matching
/// throw is pending.
pub extern "C-unwind" fn catch_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%native-catch: expected 2 args (tag thunk), got {n_args}");
    }
    let tag = unsafe { *args };
    let thunk = Word::from_raw(unsafe { *args.add(1) });
    if thunk.tag() != Tag::Function {
        panic!("%native-catch: thunk must be a function, got {thunk:?}");
    }

    CATCH_STACK.with(|s| {
        s.borrow_mut().push(CatchFrame {
            tag,
            value: Word::NIL.raw(),
        });
    });

    let body_result = {
        let env = crate::gc_function::env(thunk);
        let code = crate::gc_function::code_ptr(thunk);
        let f: crate::gc_function::LispCodeFn =
            unsafe { std::mem::transmute(code) };
        unsafe { f(mutator, env.raw(), std::ptr::null(), 0) }
    };

    let frame_value = CATCH_STACK.with(|s| {
        s.borrow_mut()
            .pop()
            .expect("CATCH_STACK underflow at catch exit")
            .value
    });

    // Consume the abort iff it's a throw targeting our tag.
    if ABORT_PENDING.with(|p| p.get()) && THROW_PENDING.with(|p| p.get()) {
        let target = THROW_TAG.with(|t| t.get());
        if target == tag {
            ABORT_PENDING.with(|p| p.set(false));
            THROW_PENDING.with(|p| p.set(false));
            THROW_TAG.with(|t| t.set(0));
            return frame_value;
        }
    }
    body_result
}

/// `(%throw TAG VALUE)` — find the nearest enclosing catch whose tag
/// is EQ to TAG, stash VALUE on its frame, and raise the abort. If no
/// matching catch is active, signal a (catchable) control error.
pub extern "C-unwind" fn throw_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%throw: expected 2 args (tag value), got {n_args}");
    }
    let tag = unsafe { *args };
    let val = unsafe { *args.add(1) };

    let found = CATCH_STACK.with(|s| {
        let mut stack = s.borrow_mut();
        match stack.iter().rposition(|f| f.tag == tag) {
            Some(i) => {
                stack[i].value = val;
                true
            }
            None => false,
        }
    });
    if !found {
        let tag_str = crate::printer::format_word_aesthetic(Word::from_raw(tag));
        return signal_condition_string(
            mutator,
            &format!("throw: no enclosing catch for tag {tag_str}"),
        );
    }
    ABORT_PENDING.with(|p| p.set(true));
    THROW_PENDING.with(|p| p.set(true));
    THROW_TAG.with(|t| t.set(tag));
    Word::NIL.raw()
}

// ───────────────────────────────────────────────────────────────────
// symbol-function — read / write / unbind a symbol's function cell.
//
// Closette installs JIT-generated discriminating functions into
// each generic function's symbol cell via (setf (symbol-function
// name) ...). These shims expose the existing function-cell API
// (gc_symbol::function_acquire / mutator::set_symbol_function) to
// Lisp.
// ───────────────────────────────────────────────────────────────────

/// `(intern name)` — intern NAME (a string) as a symbol and
/// return it. NAME is used verbatim — it's not upcased or
/// touched. Defstruct uses this from its macro expansion to
/// build constructor / accessor / setter symbol names from the
/// struct's name.
///
/// CL's full signature is `(intern name &optional package)`. NCL
/// has no package system, but accepts the optional second arg for
/// source compatibility — it's ignored.
pub extern "C-unwind" fn intern_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args == 0 || n_args > 2 {
        return signal_static_condition_string_and_abort(
            mutator,
            &format!("intern: expected 1 or 2 args (string [package]), got {n_args}"),
        );
    }
    let w = Word::from_raw(unsafe { *args });
    if w.tag() != Tag::String {
        return signal_static_condition_string_and_abort(
            mutator,
            &format!("intern: first argument must be a string, got {w:?}"),
        );
    }
    // Optional package arg: accepted (any tag), ignored.
    let name: String = crate::gc_string::chars_of(w).collect();
    let m = unsafe { &mut *mutator };
    m.coord().intern(&name).raw()
}

/// `(symbol-function sym)` — return the function bound to SYM, or
/// signal an error (panic for now) if the cell is unbound.
pub extern "C-unwind" fn symbol_function_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("symbol-function: expected 1 arg (symbol), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        panic!("symbol-function: argument must be a symbol, got {sym:?}");
    }
    let f = crate::gc_symbol::function_acquire(sym);
    if f.is_unbound() {
        let name = crate::sym_names::lookup(sym.raw())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        panic!("symbol-function: undefined function: {name}");
    }
    f.raw()
}

/// `(symbol-name sym)` — return the symbol's printable name as a
/// fresh String. Interned symbol names live in the process-wide
/// `sym_names` registry (the symbol's own name cell is left NIL
/// at allocation time — see `MutatorState::intern`), so we look
/// up by raw word and allocate a young-heap String to hand back.
pub extern "C-unwind" fn symbol_name_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(
            mutator,
            "symbol-name: expected 1 arg (symbol)",
        );
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return signal_condition_string(
            mutator,
            &format!("symbol-name: argument must be a symbol, got {sym:?}"),
        );
    }
    let m = unsafe { &mut *mutator };
    match crate::sym_names::lookup(sym.raw()) {
        Some(name) => {
            crate::gc_string::alloc_string_in_young(m, name.as_ref()).raw()
        }
        None => {
            // Uninterned / unknown symbol. Hand back an empty
            // string rather than NIL so callers can pass the
            // result to length/aref/char without an extra check.
            crate::gc_string::alloc_string_in_young(m, "").raw()
        }
    }
}

/// `(symbol-package sym)` — return the symbol's home package
/// (`COMMON-LISP`, `COMMON-LISP-USER`, `KEYWORD`, etc.) or NIL
/// for an uninterned symbol. Most callers want this to filter
/// out keywords or to print qualified names.
pub extern "C-unwind" fn symbol_package_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(
            mutator,
            "symbol-package: expected 1 arg (symbol)",
        );
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return signal_condition_string(
            mutator,
            &format!("symbol-package: argument must be a symbol, got {sym:?}"),
        );
    }
    crate::gc_symbol::package(sym).raw()
}

/// `(boundp sym)` — T iff SYM's value cell holds something other
/// than the UNBOUND sentinel. The companion to FBOUNDP.
pub extern "C-unwind" fn boundp_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "boundp: expected 1 arg (symbol)");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return signal_condition_string(
            mutator,
            &format!("boundp: argument must be a symbol, got {sym:?}"),
        );
    }
    if crate::gc_symbol::value_acquire(sym).is_unbound() {
        Word::NIL.raw()
    } else {
        Word::T.raw()
    }
}

/// `(symbol-value sym)` — read the value cell. Signals if SYM is
/// not currently BOUNDP.
pub extern "C-unwind" fn symbol_value_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(
            mutator,
            "symbol-value: expected 1 arg (symbol)",
        );
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return signal_condition_string(
            mutator,
            &format!("symbol-value: argument must be a symbol, got {sym:?}"),
        );
    }
    let v = crate::gc_symbol::value_acquire(sym);
    if v.is_unbound() {
        let name = crate::sym_names::lookup(sym.raw())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        return signal_condition_string(
            mutator,
            &format!("symbol-value: {name} is unbound"),
        );
    }
    v.raw()
}

/// `(set sym val)` — store VAL into the global value cell of the symbol SYM.
/// CL's `set` function; the functional equivalent of `(setq sym val)` when
/// the symbol is only known at run time.  Returns VAL.
pub extern "C-unwind" fn set_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "set: expected 2 args (symbol value)");
    }
    let sym = Word::from_raw(unsafe { *args });
    let val = Word::from_raw(unsafe { *args.add(1) });
    if sym.tag() != Tag::Symbol {
        return signal_condition_string(
            mutator,
            &format!("set: first arg must be a symbol, got {sym:?}"),
        );
    }
    let m = unsafe { &mut *mutator };
    m.set_symbol_value(sym, val);
    val.raw()
}

/// `(type-of x)` — a symbol naming the most specific type of X.
/// CL spec says the result is implementation-defined for many
/// cases; we return the obvious symbol that matches our TYPEP
/// vocabulary so `(typep x (type-of x))` always holds.
pub extern "C-unwind" fn type_of_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "type-of: expected 1 arg");
    }
    let w = Word::from_raw(unsafe { *args });
    let m = unsafe { &mut *mutator };
    let coord = m.coord();
    let name = if w.is_nil() {
        "NULL"
    } else if w.is_t() {
        // T is the boolean true; CL says (type-of t) is BOOLEAN.
        "BOOLEAN"
    } else {
        match w.tag() {
            Tag::Fixnum => "FIXNUM",
            Tag::Cons => "CONS",
            Tag::Symbol => {
                // Keyword if its name starts with ':' (our intern
                // convention prefixes keyword names that way).
                match crate::sym_names::lookup(w.raw()) {
                    Some(n) if n.starts_with(':') => "KEYWORD",
                    _ => "SYMBOL",
                }
            }
            Tag::String => "STRING",
            Tag::Function => "FUNCTION",
            Tag::Vector => {
                // Distinguish numeric heap-types from ordinary
                // simple-vectors via the HeapHeader.
                let p = w.as_ptr::<u64>(Tag::Vector);
                if let Some(p) = p {
                    let hdr = crate::heap::HeapHeader::from_raw(unsafe { *p });
                    match hdr.ty() {
                        crate::heap::HeapType::Float => "FLOAT",
                        crate::heap::HeapType::Bignum => "BIGNUM",
                        crate::heap::HeapType::Ratio => "RATIO",
                        crate::heap::HeapType::Complex => "COMPLEX",
                        _ => "SIMPLE-VECTOR",
                    }
                } else {
                    "SIMPLE-VECTOR"
                }
            }
            Tag::Immediate => {
                if w.as_char().is_some() {
                    "CHARACTER"
                } else {
                    "T"
                }
            }
            Tag::Forward => "T",
        }
    };
    coord.intern(name).raw()
}

/// `(%set-symbol-function sym fn)` — install FN as the function
/// bound to SYM. The setf-symbol-function lowering rewrites
/// `(setf (symbol-function s) f)` into a call to this. Returns
/// the new function value.
pub extern "C-unwind" fn set_symbol_function_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("%set-symbol-function: expected 2 args (sym fn), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    let new_fn = Word::from_raw(unsafe { *args.add(1) });
    if sym.tag() != Tag::Symbol {
        panic!("%set-symbol-function: first arg must be a symbol, got {sym:?}");
    }
    if new_fn.tag() != Tag::Function {
        panic!(
            "%set-symbol-function: second arg must be a function, got {new_fn:?}"
        );
    }
    let m = unsafe { &mut *mutator };
    m.set_symbol_function(sym, new_fn);
    new_fn.raw()
}

/// `(fmakunbound sym)` — clear SYM's function cell. Returns SYM.
pub extern "C-unwind" fn fmakunbound_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("fmakunbound: expected 1 arg (symbol), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        panic!("fmakunbound: argument must be a symbol, got {sym:?}");
    }
    let m = unsafe { &mut *mutator };
    m.set_symbol_function(sym, Word::UNBOUND);
    sym.raw()
}

/// `(fboundp sym)` — return T if SYM has a function cell bound,
/// NIL otherwise.
/// The ANSI Common Lisp special operators (CLHS Figure 3-2). Used by
/// SPECIAL-OPERATOR-P and FBOUNDP. Names are upper-case to match the
/// reader's case-folding and the symbol-name registry.
const ANSI_SPECIAL_OPERATORS: &[&str] = &[
    "BLOCK", "CATCH", "EVAL-WHEN", "FLET", "FUNCTION", "GO", "IF",
    "LABELS", "LET", "LET*", "LOAD-TIME-VALUE", "LOCALLY", "MACROLET",
    "MULTIPLE-VALUE-CALL", "MULTIPLE-VALUE-PROG1", "PROGN", "PROGV",
    "QUOTE", "RETURN-FROM", "SETQ", "SYMBOL-MACROLET", "TAGBODY", "THE",
    "THROW", "UNWIND-PROTECT",
];

fn is_ansi_special_operator(name: &str) -> bool {
    ANSI_SPECIAL_OPERATORS.contains(&name)
}

/// Standard CL macros that NCL bakes into the compiler (lowered directly
/// in ncl-compiler's `lower.rs`) rather than registering in the macro
/// table. They have no function cell and aren't ANSI special operators,
/// so FBOUNDP would otherwise miss them even though they are fbound. Used
/// by FBOUNDP only — they are macros, not special operators, so
/// SPECIAL-OPERATOR-P must still answer NIL for them. Keep in sync with
/// the corresponding `"NAME" =>` arms in lower.rs.
const NCL_COMPILER_MACROS: &[&str] = &[
    "AND", "OR", "WHEN", "UNLESS", "COND", "SETF", "DECF", "DEFVAR",
];

fn is_ncl_compiler_macro(name: &str) -> bool {
    NCL_COMPILER_MACROS.contains(&name)
}

/// `(fboundp symbol)` — true if SYMBOL is fbound: it names a function, a
/// macro, OR a special operator. Previously this checked only the
/// function cell, so (fboundp 'when) [macro] and (fboundp 'if) [special
/// operator] wrongly returned NIL.
pub extern "C-unwind" fn fboundp_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("fboundp: expected 1 arg (symbol), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return Word::NIL.raw();
    }
    if !crate::gc_symbol::function_acquire(sym).is_unbound() {
        return Word::T.raw();
    }
    // Not a plain function — is it a macro or a special operator?
    let m = unsafe { &mut *mutator };
    if let Some(name) = crate::sym_names::lookup(sym.raw()) {
        if m.coord().macro_for(&name).is_some()
            || is_ansi_special_operator(&name)
            || is_ncl_compiler_macro(&name)
        {
            return Word::T.raw();
        }
    }
    Word::NIL.raw()
}

/// Bridge to the compiler's lexical macro environment. `macrolet`-local
/// macros live in the compiler's thread-local macro env (ncl-compiler),
/// not in the global registry this crate can see. The compiler registers
/// a hook here at session start; runtime macro introspection
/// (`macro-function` / `macroexpand` given a non-nil `&environment`)
/// calls it to resolve a name against the *lexically* enclosing macros
/// that are live during the current macroexpansion. Takes a symbol-name
/// (ptr, len) and returns the local expander Function Word, or NIL.
static LOCAL_MACRO_HOOK: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn set_local_macro_hook(f: extern "C" fn(*const u8, usize) -> u64) {
    LOCAL_MACRO_HOOK.store(f as usize, std::sync::atomic::Ordering::Relaxed);
}

fn query_local_macro(name: &str) -> u64 {
    let p = LOCAL_MACRO_HOOK.load(std::sync::atomic::Ordering::Relaxed);
    if p == 0 {
        return Word::NIL.raw();
    }
    let f: extern "C" fn(*const u8, usize) -> u64 = unsafe { std::mem::transmute(p) };
    f(name.as_ptr(), name.len())
}

/// `(macro-function symbol &optional environment)` — the macro expander
/// Function for SYMBOL, or NIL if SYMBOL does not name a macro. NCL macro
/// expanders take the macro call's *arguments* (the cdr of the form)
/// positionally, matching how the compiler invokes them; the Lisp-level
/// MACROEXPAND-1 bridges that to the CL (form env) contract via APPLY.
///
/// When a non-nil ENVIRONMENT is supplied, the lexical (macrolet) macro
/// environment is consulted first — so an &environment-bearing macro can
/// see macros established by an enclosing macrolet.
pub extern "C-unwind" fn macro_function_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 1 {
        panic!("macro-function: expected at least 1 arg (symbol), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return Word::NIL.raw();
    }
    let name = crate::sym_names::lookup(sym.raw());
    // Non-nil &environment → consult the lexical macrolet env first.
    let env_given = n_args >= 2 && !Word::from_raw(unsafe { *args.add(1) }).is_nil();
    if env_given {
        if let Some(n) = &name {
            let local = query_local_macro(n);
            if local != Word::NIL.raw() {
                return local;
            }
        }
    }
    let m = unsafe { &mut *mutator };
    match name.and_then(|name| m.coord().macro_for(&name)) {
        Some(w) => w.raw(),
        None => Word::NIL.raw(),
    }
}

/// `(special-operator-p symbol)` — true if SYMBOL is one of the ANSI
/// special operators AND is not currently shadowed by a macro. (NCL
/// implements some standardized operators, e.g. LET*, as macros; CLHS
/// allows that, and such a symbol is then a macro, not a special
/// operator, so it answers NIL here.)
pub extern "C-unwind" fn special_operator_p_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("special-operator-p: expected 1 arg (symbol), got {n_args}");
    }
    let sym = Word::from_raw(unsafe { *args });
    if sym.tag() != Tag::Symbol {
        return Word::NIL.raw();
    }
    let m = unsafe { &mut *mutator };
    if let Some(name) = crate::sym_names::lookup(sym.raw()) {
        if m.coord().macro_for(&name).is_none()
            && is_ansi_special_operator(&name)
        {
            return Word::T.raw();
        }
    }
    Word::NIL.raw()
}

// ───────────────────────────────────────────────────────────────────
// Hash function — bit-mix on a Word's raw bits.
//
// The Lisp hash-table layer is built on top of Vectors and cons
// cells (see core.lisp). The only piece that genuinely has to be
// in Rust is a fast hash that takes a Word and produces a
// non-negative fixnum the bucket index calculation can mod.
//
// For the EQ / EQL cases (the only ones Closette and the GUI
// demos actually need), hashing the raw bits is correct because
// two values that are EQ have identical Word bits. Symbols are
// interned (so equal-named ones share a Word), fixnums and chars
// are immediate, T/NIL are unique.
//
// EQUAL hash tables on strings and conses would need a
// content-hash; that's deferred until something needs it.
// ───────────────────────────────────────────────────────────────────

/// `(%word-hash w)` — return a non-negative fixnum hash of the
/// raw Word bits. Uses a SplitMix64-style finaliser; one shift +
/// two multiplies. Caller mods by bucket count.
pub extern "C-unwind" fn word_hash_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%word-hash: expected 1 arg, got {n_args}");
    }
    let w = unsafe { *args };
    // SplitMix64 finaliser.
    let mut h = w.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    // Fixnums are 61 bits signed; mask to 60 bits to guarantee a
    // non-negative result that fits.
    let positive = (h & ((1u64 << 60) - 1)) as i64;
    Word::fixnum(positive).raw()
}

/// `(%equal-hash w)` — content-aware hash for EQUAL hash tables.
///
/// Strings are hashed by their character content (FNV-1a), so two
/// distinct string objects with identical text hash identically.
/// Fixnums, symbols, chars — same as `%word-hash` since they are
/// interned or immediate. Cons cells hash by car (car-only avoids
/// deep recursion on long lists; collisions are handled by the
/// comparison walk).
pub extern "C-unwind" fn equal_hash_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%equal-hash: expected 1 arg, got {n_args}");
    }
    let w = Word::from_raw(unsafe { *args });
    let h = equal_hash_word(w);
    let positive = (h & ((1u64 << 60) - 1)) as i64;
    Word::fixnum(positive).raw()
}

/// Compute a content-aware hash for a Word.
fn equal_hash_word(w: Word) -> u64 {
    match w.tag() {
        Tag::String => {
            // FNV-1a over the character codepoints.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for c in crate::gc_string::chars_of(w) {
                h ^= c as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
            h
        }
        Tag::Cons => {
            // Hash by car only — avoids deep recursion.
            let p = match w.as_ptr::<u64>(Tag::Cons) {
                Some(p) => p,
                None => return 0,
            };
            let car = Word::from_raw(unsafe { *p });
            equal_hash_word(car).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        }
        _ => {
            // For fixnums, symbols, chars, nil, t — raw-bit hash.
            let mut h = w.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15);
            h ^= h >> 30;
            h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            h ^= h >> 27;
            h
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Vectors — make-array, vector, aref, svref, (setf aref).
//
// CL's `make-array` is heavily overloaded (multidimensional, fill
// pointers, displaced arrays, element-type, adjustable). We support
// the simple-vector subset here — that's what Closette and the GUI
// demos need. If a richer surface is required later, `make-array`'s
// shim can grow keyword args without touching the underlying
// vector heap object.
// ───────────────────────────────────────────────────────────────────

/// Decode `make-array`'s dimension argument to a 1-D length.
///
/// Accepts a non-negative fixnum (`(make-array 3)`) or a *singleton*
/// dimension list (`(make-array '(3))` — CL's 1-D list form). A
/// multi-element dimension list is a multidimensional array, which
/// isn't supported yet; rather than panic the worker thread, this
/// returns `Err(msg)` so the caller can signal a catchable Lisp
/// condition. Same for malformed specifiers.
///
/// SAFETY: walks `dim_w` as a cons list when it is Cons-tagged; the
/// `Tag::Cons` check gates every dereference.
fn make_array_dimension(dim_w: Word) -> Result<u32, String> {
    if let Some(n) = dim_w.as_fixnum() {
        return if n >= 0 {
            Ok(n as u32)
        } else {
            Err(format!("make-array: dimension must be non-negative, got {n}"))
        };
    }
    if dim_w.is_nil() {
        return Err(
            "make-array: zero-dimensional arrays are not supported".to_string(),
        );
    }
    if dim_w.tag() == Tag::Cons {
        let mut dims: u32 = 0;
        let mut only: u32 = 0;
        let mut cur = dim_w;
        while !cur.is_nil() {
            if cur.tag() != Tag::Cons {
                return Err(
                    "make-array: dimension list must be a proper list".to_string(),
                );
            }
            let p = cur.as_ptr::<u64>(Tag::Cons).unwrap();
            let car = Word::from_raw(unsafe { *p });
            match car.as_fixnum() {
                Some(d) if d >= 0 => {
                    if dims == 0 {
                        only = d as u32;
                    }
                    dims += 1;
                }
                _ => {
                    return Err(format!(
                        "make-array: array dimension must be a non-negative \
                         fixnum, got {car:?}"
                    ));
                }
            }
            cur = Word::from_raw(unsafe { *p.add(1) });
        }
        return match dims {
            1 => Ok(only),
            0 => Err(
                "make-array: zero-dimensional arrays are not supported".to_string(),
            ),
            _ => Err(format!(
                "make-array: multidimensional arrays ({dims} dimensions) are \
                 not yet supported — only 1-D arrays are available"
            )),
        };
    }
    Err(format!("make-array: invalid dimension specifier {dim_w:?}"))
}

/// `(make-array dim &key initial-element initial-contents)`.
/// `dim` is a fixnum length or a singleton list `(N)` (CL's 1-D
/// form). Multidimensional shapes signal a catchable condition (not
/// yet supported). `initial-element` fills every cell; without it,
/// cells are NIL. `initial-contents` (a list) copies list elements
/// into positions; if shorter than `dim`, trailing positions stay
/// NIL (or the initial-element).
pub extern "C-unwind" fn make_array_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args == 0 {
        return signal_condition_string(
            mutator,
            "make-array: expected at least 1 arg (dimension)",
        );
    }
    let dim_w = Word::from_raw(unsafe { *args });
    let n = match make_array_dimension(dim_w) {
        Ok(n) => n,
        Err(msg) => return signal_condition_string(mutator, &msg),
    };
    // Scan keyword args. Only :initial-element and :initial-contents
    // recognised. Unknown keywords are silently ignored, matching
    // the ergonomic-but-permissive style of the existing shims.
    let mut init_element: Word = Word::NIL;
    let mut init_contents: Option<Word> = None;
    let mut i = 1u64;
    while i + 1 < n_args {
        let key = Word::from_raw(unsafe { *args.add(i as usize) });
        let val = Word::from_raw(unsafe { *args.add((i + 1) as usize) });
        if let Some(name) = crate::sym_names::lookup(key.raw()) {
            match name.as_ref() {
                ":INITIAL-ELEMENT" => init_element = val,
                ":INITIAL-CONTENTS" => init_contents = Some(val),
                _ => {}
            }
        }
        i += 2;
    }
    let m = unsafe { &mut *mutator };
    let v = m.alloc_vector(n);
    // Initialise. alloc_vector zero-fills (zero == fixnum 0 != NIL),
    // so we always do an explicit init here.
    for idx in 0..n {
        set_vector_cell(mutator, v, idx, init_element.raw());
    }
    if let Some(list) = init_contents {
        let mut cur = list;
        let mut idx = 0u32;
        while !cur.is_nil() && idx < n {
            if cur.tag() != Tag::Cons {
                panic!("make-array :initial-contents must be a proper list");
            }
            let p = cur.as_ptr::<u64>(Tag::Cons).unwrap();
            let elem = unsafe { *p };
            set_vector_cell(mutator, v, idx, elem);
            cur = Word::from_raw(unsafe { *p.add(1) });
            idx += 1;
        }
    }
    v.raw()
}

/// `(vector e1 e2 ... eN)` — construct a fresh vector containing
/// the given elements in order. Same length as the call's arg
/// count.
pub extern "C-unwind" fn vector_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    let m = unsafe { &mut *mutator };
    let v = m.alloc_vector(n_args as u32);
    for i in 0..n_args {
        let elem = unsafe { *args.add(i as usize) };
        set_vector_cell(mutator, v, i as u32, elem);
    }
    v.raw()
}

/// `(svref v i)` and `(aref v i)` for vectors. JIT-callable from
/// the polymorphic AREF lowering. Bounds-checked; out-of-range
/// indices panic.
///
/// SAFETY: arguments come from JIT'd code; both must be valid
/// Words. `v` must be Vector- or String-tagged for the dispatch
/// to make sense.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_aref_generic(v: u64, i: u64) -> u64 {
    let vw = Word::from_raw(v);
    let iw = Word::from_raw(i);
    let idx = match iw.as_fixnum() {
        Some(n) if n >= 0 => n as usize,
        _ => panic!("aref: index must be a non-negative fixnum, got {iw:?}"),
    };
    match vw.tag() {
        Tag::Vector => {
            let n = vector_payload_len(vw) as usize;
            if idx >= n {
                panic!("aref: index {idx} out of bounds for vector of length {n}");
            }
            vector_cell(vw, idx as u32)
        }
        // ncl_string_char takes a raw (untagged) index.
        Tag::String => crate::abi::ncl_string_char(v, idx as u64),
        _ => panic!("aref: not a sequence: {vw:?}"),
    }
}

/// `(setf (aref v i) val)` for vectors and strings. Polymorphic
/// dispatch. Returns `val`. Card-marks for vectors (strings have
/// their own GC story).
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_aset_generic(
    mutator: *mut crate::mutator::MutatorState,
    v: u64,
    i: u64,
    val: u64,
) -> u64 {
    let vw = Word::from_raw(v);
    let iw = Word::from_raw(i);
    let idx = match iw.as_fixnum() {
        Some(n) if n >= 0 => n as usize,
        _ => panic!("(setf aref): index must be a non-negative fixnum, got {iw:?}"),
    };
    match vw.tag() {
        Tag::Vector => {
            let n = vector_payload_len(vw) as usize;
            if idx >= n {
                panic!(
                    "(setf aref): index {idx} out of bounds for vector of length {n}"
                );
            }
            set_vector_cell(mutator, vw, idx as u32, val);
            val
        }
        // ncl_string_set takes a raw (untagged) index.
        Tag::String => crate::abi::ncl_string_set(v, idx as u64, val),
        _ => panic!("(setf aref): not a sequence: {vw:?}"),
    }
}

/// `ncl_lookup_keyword(args, key_start, n_args, keyword) -> value`.
///
/// Scan `args[key_start..n_args]` in steps of 2. If `args[i]`
/// matches `keyword`, return `args[i+1]`. If no match (or the
/// trailing slot has no following value), return `Word::UNBOUND`
/// — the IR's `KeyArg` lowering branches on that to evaluate the
/// default form.
///
/// JIT-callable. The compiler emits a call to this for every `&key`
/// parameter at function entry.
///
/// SAFETY: `args` must point to at least `n_args` valid `u64`s.
#[unsafe(no_mangle)]
pub extern "C-unwind" fn ncl_lookup_keyword(
    args: *const u64,
    key_start: u64,
    n_args: u64,
    keyword: u64,
) -> u64 {
    let mut i = key_start;
    while i + 1 < n_args {
        let here = unsafe { *args.add(i as usize) };
        if here == keyword {
            return unsafe { *args.add((i + 1) as usize) };
        }
        i += 2;
    }
    Word::UNBOUND.raw()
}

// ───────────────────────────────────────────────────────────────────
// String manipulation — string-upcase, string-downcase, concatenate
// ───────────────────────────────────────────────────────────────────

/// `(string-upcase s)` → fresh upper-cased string.
pub extern "C-unwind" fn string_upcase_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "string-upcase: expected 1 arg");
    }
    let s = Word::from_raw(unsafe { *args });
    if s.tag() != Tag::String {
        return signal_condition_string(mutator, "string-upcase: argument must be a string");
    }
    let upper: String = crate::gc_string::chars_of(s)
        .flat_map(|c| c.to_uppercase())
        .collect();
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, &upper).raw()
}

/// `(string-downcase s)` → fresh lower-cased string.
pub extern "C-unwind" fn string_downcase_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "string-downcase: expected 1 arg");
    }
    let s = Word::from_raw(unsafe { *args });
    if s.tag() != Tag::String {
        return signal_condition_string(mutator, "string-downcase: argument must be a string");
    }
    let lower: String = crate::gc_string::chars_of(s)
        .flat_map(|c| c.to_lowercase())
        .collect();
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, &lower).raw()
}

/// `(string-trim chars s)` → fresh string with leading/trailing
/// characters from CHARS removed. CHARS is a string of characters
/// to trim.
pub extern "C-unwind" fn string_trim_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "string-trim: expected 2 args (chars, string)");
    }
    let chars_w = Word::from_raw(unsafe { *args });
    let s_w = Word::from_raw(unsafe { *args.add(1) });
    if chars_w.tag() != Tag::String || s_w.tag() != Tag::String {
        return signal_condition_string(mutator, "string-trim: arguments must be strings");
    }
    let trim_chars: Vec<char> = crate::gc_string::chars_of(chars_w).collect();
    let s: String = crate::gc_string::chars_of(s_w).collect();
    let trimmed = s.trim_matches(|c: char| trim_chars.contains(&c));
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, trimmed).raw()
}

/// `(string-left-trim chars s)` → trim from left only.
pub extern "C-unwind" fn string_left_trim_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "string-left-trim: expected 2 args");
    }
    let chars_w = Word::from_raw(unsafe { *args });
    let s_w = Word::from_raw(unsafe { *args.add(1) });
    if chars_w.tag() != Tag::String || s_w.tag() != Tag::String {
        return signal_condition_string(mutator, "string-left-trim: arguments must be strings");
    }
    let trim_chars: Vec<char> = crate::gc_string::chars_of(chars_w).collect();
    let s: String = crate::gc_string::chars_of(s_w).collect();
    let trimmed = s.trim_start_matches(|c: char| trim_chars.contains(&c));
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, trimmed).raw()
}

/// `(string-right-trim chars s)` → trim from right only.
pub extern "C-unwind" fn string_right_trim_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        return signal_condition_string(mutator, "string-right-trim: expected 2 args");
    }
    let chars_w = Word::from_raw(unsafe { *args });
    let s_w = Word::from_raw(unsafe { *args.add(1) });
    if chars_w.tag() != Tag::String || s_w.tag() != Tag::String {
        return signal_condition_string(mutator, "string-right-trim: arguments must be strings");
    }
    let trim_chars: Vec<char> = crate::gc_string::chars_of(chars_w).collect();
    let s: String = crate::gc_string::chars_of(s_w).collect();
    let trimmed = s.trim_end_matches(|c: char| trim_chars.contains(&c));
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, trimmed).raw()
}

/// `(parse-integer s)` → fixnum parsed from string S. Returns NIL
/// on invalid input (a real CL would signal an error).
pub extern "C-unwind" fn parse_integer_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return signal_condition_string(mutator, "parse-integer: expected 1 arg");
    }
    let s = Word::from_raw(unsafe { *args });
    if s.tag() != Tag::String {
        return signal_condition_string(mutator, "parse-integer: argument must be a string");
    }
    let text: String = crate::gc_string::chars_of(s).collect();
    let trimmed = text.trim();
    match trimmed.parse::<i64>() {
        Ok(n) => Word::fixnum(n).raw(),
        Err(_) => signal_condition_string(
            mutator,
            &format!("parse-integer: not a valid integer: {trimmed:?}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutator::{GcConfig, GcCoordinator};

    fn small_config() -> GcConfig {
        GcConfig {
            young_bytes: 16 * 1024,
            old_bytes: 16 * 1024,
            static_bytes: 8 * 1024,
            tlab_cells: 64,
        }
    }

    #[test]
    fn alloc_cons_via_abi_returns_cons_tagged_word() {
        let coord = GcCoordinator::new(small_config());
        let mut m = coord.register_mutator();
        let result = ncl_alloc_cons(&mut m as *mut _, Word::fixnum(1).raw(), Word::fixnum(2).raw());
        let w = Word::from_raw(result);
        assert!(w.is_cons());
        assert_eq!(ncl_car(result), Word::fixnum(1).raw());
        assert_eq!(ncl_cdr(result), Word::fixnum(2).raw());
    }
}
