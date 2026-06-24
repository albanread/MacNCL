//! GC-managed Symbol layout.
//!
//! See `docs/GC.md`. A Symbol lives in the static area (immortal,
//! never moved) and has the layout:
//!
//! ```text
//!   cell offset
//!   ───────────────────────────────────────────────────────────────
//!     [0]  HeapHeader      (type=Symbol, length=7)
//!     [1]  name            Word — pointer to print-name string
//!     [2]  package         Word — pointer to home package
//!     [3]  value           AtomicU64 — store-release/load-acquire
//!     [4]  function        AtomicU64 — store-release/load-acquire
//!     [5]  plist           Word — property list head
//!     [6]  flags           Word — constant-p, special-p, etc.
//!     [7]  jump_cache      AtomicU64 — optimised dispatch (Phase 4+)
//! ```
//!
//! The function cell is the load-bearing piece of the redefinition
//! story (MANIFESTO.md, "Note: function redefinition and dispatch"):
//! `defun` is exactly one `store-release` here, dispatch is one
//! `load-acquire`. Multi-threaded redefinition is correct without
//! locks.
//!
//! Cells [1], [2], [5], [6] are written once at intern time and then
//! treated as immutable from the mutator's perspective. The
//! park/unpark synchronisation (and the discipline that intern
//! happens before any other thread observes the symbol) provides
//! the cross-thread visibility.
//!
//! Step 8 builds the layout and the atomic accessors. The intern
//! table that maps `(name, package)` to a stable Symbol Word lands
//! when Phase 1a's reader-side Symbol gets retired in favour of this
//! one — that's a Phase 3 integration concern.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::heap::{HeapHeader, HeapType};
use crate::static_area::StaticArea;
use crate::word::{Tag, Word};

// Cell offsets within a Symbol object (header is at cell 0).
pub const NAME_OFFSET: usize = 1;
pub const PACKAGE_OFFSET: usize = 2;
pub const VALUE_OFFSET: usize = 3;
pub const FUNCTION_OFFSET: usize = 4;
pub const PLIST_OFFSET: usize = 5;
pub const FLAGS_OFFSET: usize = 6;
pub const JUMP_CACHE_OFFSET: usize = 7;

/// Number of payload cells (excluding the header). Header + 7 = 8.
pub const SYMBOL_PAYLOAD_CELLS: u32 = 7;

/// Total size of a Symbol object in cells (header included).
pub const SYMBOL_TOTAL_CELLS: usize = 8;

/// Allocate a fresh symbol in the static area. Returns a
/// Symbol-tagged Word, or `None` if static is exhausted.
///
/// The freshly allocated symbol's value, function, and jump_cache
/// cells start as `UNBOUND`. plist starts as `NIL`. flags starts as
/// fixnum 0. name and package are stored as provided.
pub fn alloc_symbol_in_static(
    static_area: &StaticArea,
    name: Word,
    package: Word,
) -> Option<Word> {
    let header_ptr = static_area
        .try_alloc_with_header(HeapType::Symbol, SYMBOL_PAYLOAD_CELLS)?;
    let p = header_ptr.as_ptr() as *mut u64;
    // SAFETY: `try_alloc_with_header` reserved 1 + 7 cells starting
    // at p. Cells 0..8 are within bounds and exclusively owned by
    // this thread until a safe point publishes them.
    unsafe {
        *p.add(NAME_OFFSET) = name.raw();
        *p.add(PACKAGE_OFFSET) = package.raw();
        atomic_at(p, VALUE_OFFSET).store(Word::UNBOUND.raw(), Ordering::Relaxed);
        atomic_at(p, FUNCTION_OFFSET).store(Word::UNBOUND.raw(), Ordering::Relaxed);
        *p.add(PLIST_OFFSET) = Word::NIL.raw();
        *p.add(FLAGS_OFFSET) = Word::fixnum(0).raw();
        atomic_at(p, JUMP_CACHE_OFFSET).store(Word::NIL.raw(), Ordering::Relaxed);
    }
    Some(Word::from_ptr(p as *const u8, Tag::Symbol))
}

/// Read the symbol's name cell.
pub fn name(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { *p.add(NAME_OFFSET) })
}

/// Read the symbol's home package cell.
pub fn package(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { *p.add(PACKAGE_OFFSET) })
}

/// Read the property list head.
pub fn plist(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { *p.add(PLIST_OFFSET) })
}

/// Read the flags word.
pub fn flags(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { *p.add(FLAGS_OFFSET) })
}

/// Read the value cell with **acquire** semantics — sees every
/// store that happened-before any prior store-release on this cell.
/// Bound dynamic-variable lookup uses this.
pub fn value_acquire(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { atomic_at(p, VALUE_OFFSET).load(Ordering::Acquire) })
}

/// Read the function cell with **acquire** semantics — every prior
/// `defun` is visible. Function-call dispatch uses this.
pub fn function_acquire(sym: Word) -> Word {
    let p = sym_ptr(sym);
    Word::from_raw(unsafe { atomic_at(p, FUNCTION_OFFSET).load(Ordering::Acquire) })
}

/// Atomically swap the function cell to `new_fn`, with **release**
/// semantics — every prior store on this thread happens-before any
/// load-acquire on this cell. **This is `defun` at the runtime level.**
///
/// Note: this is the raw mechanism; `MutatorState::set_symbol_function`
/// adds the write-barrier card mark.
pub fn set_function_release(sym: Word, new_fn: Word) {
    let p = sym_ptr(sym);
    unsafe { atomic_at(p, FUNCTION_OFFSET).store(new_fn.raw(), Ordering::Release) };
}

/// Atomically set the value cell with release semantics. The
/// non-atomic `defparameter` is exactly this.
pub fn set_value_release(sym: Word, new_val: Word) {
    let p = sym_ptr(sym);
    unsafe { atomic_at(p, VALUE_OFFSET).store(new_val.raw(), Ordering::Release) };
}

/// CAS the function cell. Returns `Ok(())` if the swap succeeded,
/// `Err(observed)` if the cell didn't match `expected`. Used by
/// inline-cache invalidation and conditional redefinition.
pub fn cas_function(sym: Word, expected: Word, new_fn: Word) -> Result<(), Word> {
    let p = sym_ptr(sym);
    match unsafe {
        atomic_at(p, FUNCTION_OFFSET).compare_exchange(
            expected.raw(),
            new_fn.raw(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    } {
        Ok(_) => Ok(()),
        Err(actual) => Err(Word::from_raw(actual)),
    }
}

/// Pointer to the function cell as a `*const u8` — used by the
/// write barrier to compute the right card.
pub fn function_cell_addr(sym: Word) -> *const u8 {
    let p = sym_ptr(sym);
    unsafe { p.add(FUNCTION_OFFSET) as *const u8 }
}

/// Pointer to the value cell as a `*const u8` — used by the
/// write barrier to compute the right card.
pub fn value_cell_addr(sym: Word) -> *const u8 {
    let p = sym_ptr(sym);
    unsafe { p.add(VALUE_OFFSET) as *const u8 }
}

/// Read the header of a symbol. For tests and assertion-checking.
pub fn header(sym: Word) -> HeapHeader {
    let p = sym_ptr(sym);
    HeapHeader::from_raw(unsafe { *p })
}

// -- Internals ---------------------------------------------------------------

fn sym_ptr(sym: Word) -> *mut u64 {
    debug_assert_eq!(sym.tag(), Tag::Symbol, "not a symbol: {sym:?}");
    sym.as_mut_ptr::<u64>(Tag::Symbol).expect("symbol ptr")
}

unsafe fn atomic_at(p: *mut u64, offset: usize) -> &'static AtomicU64 {
    // SAFETY: AtomicU64 is layout-compatible with u64. The caller
    // promises `p.add(offset)` is a valid, exclusively-mutable u64
    // cell for the lifetime of any access through the returned
    // reference (which we use immediately and don't store).
    unsafe { &*(p.add(offset) as *const AtomicU64) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn fresh_static() -> Arc<StaticArea> {
        Arc::new(StaticArea::new(64 * 1024))
    }

    #[test]
    fn fresh_symbol_layout() {
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::fixnum(99), Word::NIL).unwrap();
        assert_eq!(sym.tag(), Tag::Symbol);

        let h = header(sym);
        assert_eq!(h.ty(), HeapType::Symbol);
        assert_eq!(h.length_cells(), SYMBOL_PAYLOAD_CELLS);

        // Initial state.
        assert!(matches!(name(sym).as_fixnum(), Some(99)));
        assert!(package(sym).is_nil());
        assert!(value_acquire(sym).is_unbound());
        assert!(function_acquire(sym).is_unbound());
        assert!(plist(sym).is_nil());
        assert!(matches!(flags(sym).as_fixnum(), Some(0)));
    }

    #[test]
    fn function_set_then_get_round_trip() {
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::NIL, Word::NIL).unwrap();
        // Stand in a fixnum for an actual function-tagged Word; the
        // atomic doesn't care about the tag, only the bits.
        let fn_word = Word::fixnum(7);
        set_function_release(sym, fn_word);
        assert_eq!(function_acquire(sym).raw(), fn_word.raw());
    }

    #[test]
    fn value_set_then_get_round_trip() {
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::NIL, Word::NIL).unwrap();
        let val = Word::fixnum(123);
        set_value_release(sym, val);
        assert_eq!(value_acquire(sym).raw(), val.raw());
    }

    #[test]
    fn cas_function_succeeds_on_match_fails_on_mismatch() {
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::NIL, Word::NIL).unwrap();

        // Initial is UNBOUND.
        assert!(cas_function(sym, Word::UNBOUND, Word::fixnum(5)).is_ok());
        assert_eq!(function_acquire(sym).as_fixnum(), Some(5));

        // CAS with wrong expected fails.
        let res = cas_function(sym, Word::fixnum(99), Word::fixnum(6));
        assert!(matches!(res, Err(observed) if observed.as_fixnum() == Some(5)));
        // Cell unchanged after failed CAS.
        assert_eq!(function_acquire(sym).as_fixnum(), Some(5));
    }

    #[test]
    fn cells_are_distinct_between_symbols() {
        let s = fresh_static();
        let a = alloc_symbol_in_static(&s, Word::fixnum(1), Word::NIL).unwrap();
        let b = alloc_symbol_in_static(&s, Word::fixnum(2), Word::NIL).unwrap();
        assert_ne!(a.raw(), b.raw());
        set_function_release(a, Word::fixnum(100));
        set_function_release(b, Word::fixnum(200));
        assert_eq!(function_acquire(a).as_fixnum(), Some(100));
        assert_eq!(function_acquire(b).as_fixnum(), Some(200));
    }

    #[test]
    fn function_cell_address_in_static() {
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::NIL, Word::NIL).unwrap();
        let addr = function_cell_addr(sym);
        assert!(s.contains_ptr(addr));
        // Cell address is at offset 4 cells (32 bytes) from the
        // symbol's start (cell 0 = header).
        let sym_base = sym.as_ptr::<u8>(Tag::Symbol).unwrap();
        assert_eq!(
            addr as usize - sym_base as usize,
            FUNCTION_OFFSET * 8,
        );
    }

    #[test]
    fn concurrent_writers_one_reader_no_torn_reads() {
        // Two writer threads each repeatedly setting the function
        // cell to one of two known Word values. A reader thread
        // observes the cell. Every observation must be one of the
        // two known values — never a torn read or stale UNBOUND.
        //
        // A Barrier syncs all three threads at start so neither
        // writer is starved by scheduling jitter. The reader spins
        // up to a max iteration count but exits early once both
        // values have been observed.
        use std::sync::Barrier;
        let s = fresh_static();
        let sym = alloc_symbol_in_static(&s, Word::NIL, Word::NIL).unwrap();

        let v1 = Word::fixnum(1);
        let v2 = Word::fixnum(2);
        set_function_release(sym, v1);

        let barrier = Arc::new(Barrier::new(3));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sym_addr = sym.raw();

        let bar1 = Arc::clone(&barrier);
        let stop1 = Arc::clone(&stop);
        let w1 = thread::spawn(move || {
            bar1.wait();
            let sym = Word::from_raw(sym_addr);
            while !stop1.load(Ordering::Relaxed) {
                set_function_release(sym, v1);
            }
        });
        let bar2 = Arc::clone(&barrier);
        let stop2 = Arc::clone(&stop);
        let w2 = thread::spawn(move || {
            bar2.wait();
            let sym = Word::from_raw(sym_addr);
            while !stop2.load(Ordering::Relaxed) {
                set_function_release(sym, v2);
            }
        });

        barrier.wait();
        let mut saw_v1 = 0u64;
        let mut saw_v2 = 0u64;
        for _ in 0..1_000_000 {
            let observed = function_acquire(sym);
            if observed.raw() == v1.raw() {
                saw_v1 += 1;
            } else if observed.raw() == v2.raw() {
                saw_v2 += 1;
            } else {
                panic!("torn or unexpected read: {observed:?}");
            }
            if saw_v1 > 0 && saw_v2 > 0 { break; }
        }

        stop.store(true, Ordering::Relaxed);
        w1.join().unwrap();
        w2.join().unwrap();

        assert!(saw_v1 > 0, "never observed v1");
        assert!(saw_v2 > 0, "never observed v2");
    }
}
