//! GC-managed Function layout.
//!
//! A `Function` is the heap object a Symbol's function cell points
//! at when the symbol has a defun'd binding. Allocated in the static
//! area (immortal — defun'd functions don't die in v1; later, the
//! loader's retirement-with-quiescent-epoch story handles the case
//! where a redefinition orphans a Function whose old code is still
//! on a live stack).
//!
//! Layout (5 cells = 40 bytes):
//!
//! ```text
//!   cell 0   HeapHeader      type=Function, length=4
//!   cell 1   code_ptr        raw machine-code pointer (NOT a Word)
//!   cell 2   arity           required-argument count, as u64
//!   cell 3   name            Word — the Symbol this was bound to
//!   cell 4   env             Word — Vector of captured values, or nil
//! ```
//!
//! The code pointer is the address of JIT'd native code with
//! signature
//!   `fn(*mut MutatorState, env: u64, args: *const u64, n: u64) -> u64`.
//! Calling a function dispatches through this signature so every
//! call site has the same shape regardless of arity OR closure
//! status. defun'd functions get env=nil; lambda-created closures
//! get a Vector-tagged Word pointing at their captured values.

use std::ptr::NonNull;

use crate::heap::{HeapHeader, HeapType};
use crate::static_area::StaticArea;
use crate::word::{Tag, Word};

pub const CODE_PTR_OFFSET: usize = 1;
pub const ARITY_OFFSET: usize = 2;
pub const NAME_OFFSET: usize = 3;
pub const ENV_OFFSET: usize = 4;
/// Cell 5: 1 if this Function was compiled by the NCL Lisp compiler
/// (defun / lambda); 0 for native Rust shims. Used by
/// `instrument_tail_for_mv` to decide whether to skip the
/// `EnsureSingleMv` wrapper in tail position — compiled Lisp
/// functions manage the multi-value slot themselves, so wrapping
/// them would clobber secondary return values.
pub const IS_LISP_OFFSET: usize = 5;
pub const FUNCTION_PAYLOAD_CELLS: u32 = 5;

/// Allocate a fresh Function in the static area. The code_ptr is a
/// raw machine-code address; the JIT keeps the underlying engine
/// alive so this pointer stays valid for the process lifetime.
///
/// All Function records live here for now (defun'd functions,
/// native shims, anonymous lambdas, Win32 callback trampolines).
/// A young-heap allocation path was tried in an earlier branch
/// and is documented in `docs/GC_DESIGN.md` — it requires GC-layer
/// fixes (Phase 3 in that doc: page-based heap with age-threshold
/// tenuring) before it's safe to use for lambdas.
///
/// `is_lisp` must be `true` for functions compiled by the NCL Lisp
/// compiler and `false` for native Rust shims. The value is stored in
/// cell `IS_LISP_OFFSET` and checked by `is_lisp_compiled`.
pub fn alloc_function_in_static(
    static_area: &StaticArea,
    code_ptr: usize,
    arity: u32,
    name: Word,
    env: Word,
    is_lisp: bool,
) -> Option<Word> {
    let header_ptr =
        static_area.try_alloc_with_header(HeapType::Function, FUNCTION_PAYLOAD_CELLS)?;
    let p = header_ptr.as_ptr() as *mut u64;
    unsafe {
        *p.add(CODE_PTR_OFFSET) = code_ptr as u64;
        *p.add(ARITY_OFFSET) = arity as u64;
        *p.add(NAME_OFFSET) = name.raw();
        *p.add(ENV_OFFSET) = env.raw();
        *p.add(IS_LISP_OFFSET) = is_lisp as u64;
    }
    Some(Word::from_ptr(p as *const u8, Tag::Function))
}

/// Allocate a NO-CAPTURE (empty env) Lisp closure in the static area, via
/// the process coordinator. Returns the Function-tagged Word (raw), or
/// `None` if no coordinator is installed or the static area is exhausted.
///
/// Called by the JIT at COMPILE time: a lambda with no free variables is a
/// constant, so we allocate its Function record ONCE here and embed the
/// Word as an IR constant, instead of calling `ncl_make_closure` on every
/// evaluation. The static area never moves, so the embedded Word stays
/// valid for the process lifetime. (`name` NIL, `env` NIL, `is_lisp` true —
/// matching the anonymous-Lisp-lambda case in `ncl_make_closure`.)
pub fn alloc_no_capture_closure_static(code_ptr: usize, arity: u32) -> Option<u64> {
    let coord = crate::brk::gc_coordinator()?;
    alloc_function_in_static(
        coord.static_area(),
        code_ptr,
        arity,
        Word::NIL,
        Word::NIL,
        true,
    )
    .map(|w| w.raw())
}

/// Return `true` if this Function was compiled by the NCL Lisp
/// compiler (i.e. it manages the multi-value slot on its own), or
/// `false` if it is a native Rust shim.
pub fn is_lisp_compiled(fn_word: Word) -> bool {
    let p = fn_ptr(fn_word);
    unsafe { *p.add(IS_LISP_OFFSET) != 0 }
}

/// Read the code pointer from a Function-tagged Word.
pub fn code_ptr(fn_word: Word) -> usize {
    let p = fn_ptr(fn_word);
    unsafe { *p.add(CODE_PTR_OFFSET) as usize }
}

pub fn arity(fn_word: Word) -> u32 {
    let p = fn_ptr(fn_word);
    unsafe { *p.add(ARITY_OFFSET) as u32 }
}

pub fn name(fn_word: Word) -> Word {
    let p = fn_ptr(fn_word);
    Word::from_raw(unsafe { *p.add(NAME_OFFSET) })
}

/// Read the closure environment from a Function. Nil for plain
/// defun'd functions; Vector-tagged for lambdas with captures.
pub fn env(fn_word: Word) -> Word {
    let p = fn_ptr(fn_word);
    Word::from_raw(unsafe { *p.add(ENV_OFFSET) })
}

fn fn_ptr(fn_word: Word) -> *mut u64 {
    debug_assert_eq!(fn_word.tag(), Tag::Function);
    fn_word.as_mut_ptr::<u64>(Tag::Function).expect("function ptr")
}

/// Pointer to the code-pointer cell — used by the GC to scan if
/// we ever start tracking JIT code via the GC.
pub fn code_ptr_cell_addr(fn_word: Word) -> *const u8 {
    let p = fn_ptr(fn_word);
    unsafe { p.add(CODE_PTR_OFFSET) as *const u8 }
}

/// Header read for tests / diagnostics.
pub fn header(fn_word: Word) -> HeapHeader {
    let p = fn_ptr(fn_word);
    HeapHeader::from_raw(unsafe { *p })
}

/// SAFETY: `code` must be a function pointer compatible with
/// `extern "C" fn(*mut MutatorState, env: u64, *const u64, u64) -> u64`,
/// `args` must hold `n_args` valid `Word`s. Used by the dispatch
/// helpers in `abi.rs`.
/// We use `extern "C-unwind"` so Rust panics can propagate through
/// JIT-compiled function frames. This is required for `error` /
/// `handler-case` — `error` panics with a condition payload, the
/// panic walks back up through the active Lisp call chain (each
/// frame is a function with this type), and the catch_unwind in
/// the handler-case shim catches it. With plain `extern "C"`,
/// Rust 1.71+ would abort the process when the panic tried to
/// cross the boundary.
pub type LispCodeFn = unsafe extern "C-unwind" fn(
    mutator: *mut crate::mutator::MutatorState,
    env: u64,
    args: *const u64,
    n_args: u64,
) -> u64;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fresh_static() -> Arc<StaticArea> {
        Arc::new(StaticArea::new(64 * 1024))
    }

    #[test]
    fn function_layout() {
        let s = fresh_static();
        let f = alloc_function_in_static(&s, 0xdeadbeef, 2, Word::NIL, Word::NIL, true).unwrap();
        assert_eq!(f.tag(), Tag::Function);
        assert_eq!(header(f).ty(), HeapType::Function);
        assert_eq!(header(f).length_cells(), FUNCTION_PAYLOAD_CELLS);
        assert_eq!(code_ptr(f), 0xdeadbeef);
        assert_eq!(arity(f), 2);
        assert!(name(f).is_nil());
        assert!(env(f).is_nil());
        assert!(is_lisp_compiled(f));
    }
}
