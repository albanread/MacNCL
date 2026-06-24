//! Win32 callback bridge — runtime side. Phase 6 of
//! `docs/WINDOWS_FFI.md`.
//!
//! Two pieces of this design live here in `ncl-runtime`:
//!
//!   - the per-process registry mapping `slot` (a u64 baked into a
//!     trampoline as an immediate constant) → closure Word
//!   - the `ncl_callback_dispatch` function the trampolines tail-
//!     call into
//!
//! The trampolines themselves (the JIT codegen + the
//! `(%make-win32-callback …)` shim that constructs them) live in
//! `ncl-llvm` because they go through the LLVM execution engine.
//! Runtime can't depend on ncl-llvm (it's the other way round), so
//! we expose only what the trampoline needs and let ncl-llvm bind
//! it via `LLVMAddGlobalMapping`.

use std::sync::Mutex;

use crate::word::Word;

/// Process-global table of registered Win32 callbacks. Each entry
/// is the closure Word that the corresponding trampoline routes
/// to. Slot 0 is reserved as a sentinel ("no callback").
static REGISTRY: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// Register CLOSURE in the callback table. Returns the slot index
/// (≥ 1; slot 0 is the sentinel).
pub fn register(closure: Word) -> u64 {
    let mut reg = REGISTRY.lock().unwrap();
    if reg.is_empty() {
        reg.push(0);
    }
    reg.push(closure.raw());
    (reg.len() - 1) as u64
}

/// Read back the closure Word for SLOT, or None on a bad slot.
pub fn lookup(slot: u64) -> Option<Word> {
    let reg = REGISTRY.lock().unwrap();
    reg.get(slot as usize).copied().map(Word::from_raw)
}

/// Dispatcher — called by every JIT-emitted Win32 trampoline.
/// Signature is fixed across all trampolines so a single LLVM
/// `declare` covers them all. `slot` identifies the closure;
/// `args_ptr` is a pointer to `n_args` u64 slots holding the
/// marshalled Win32 args; the return value gets passed back to
/// Windows in RAX.
///
/// Win32 calls trampolines on whichever thread it likes —
/// typically the UI thread (for WNDPROC during DispatchMessageW).
/// We use the UI thread's lazy-init'd MutatorState (same one
/// `WM_NCL_EXECUTE` uses) so the funcall lands in a registered
/// Lisp thread.
#[allow(non_snake_case)]
pub extern "C-unwind" fn ncl_callback_dispatch(
    slot: u64,
    args_ptr: *const u64,
    n_args: u64,
) -> u64 {
    let closure = lookup(slot).unwrap_or_else(|| {
        panic!(
            "ncl_callback_dispatch: bad slot {slot} (registry size = {})",
            REGISTRY.lock().unwrap().len()
        )
    });
    let mutator = crate::win_surface::ui_mutator_or_panic();
    unsafe { crate::abi::ncl_funcall(mutator, closure.raw(), args_ptr, n_args) }
}
