//! Foreign buffer primitives for the Windows FFI. Phase 5 of
//! `docs/WINDOWS_FFI.md`.
//!
//! Win32 record types (RECT, POINT, MSG, WNDCLASSEXW, PAINTSTRUCT,
//! …) get passed by pointer. The standard C idiom is "caller
//! allocates, callee fills in (or reads)". Lisp can't malloc / read
//! / write arbitrary memory natively — those are the missing
//! primitives this file provides.
//!
//! Surface
//! ───────
//!
//!   (make-foreign-buffer SIZE)          → fixnum (raw pointer)
//!   (free-foreign-buffer PTR SIZE)      → nil
//!   (buffer-ref-u8  PTR OFFSET)         → fixnum
//!   (buffer-set-u8  PTR OFFSET VAL)     → nil
//!   (buffer-ref-i32 PTR OFFSET)         → fixnum (sign-extended)
//!   (buffer-set-i32 PTR OFFSET VAL)     → nil
//!   …                                   (u16/u32/u64/i8/i16/i64)
//!   (buffer-ref-ptr PTR OFFSET)         → fixnum (full u64)
//!   (buffer-set-ptr PTR OFFSET VAL)     → nil
//!   (buffer-read-wstring PTR LEN)       → string (UTF-16 → NCL chars)
//!   (buffer-write-wstring PTR OFFSET S) → fixnum (bytes written, incl NUL)
//!   (buffer-zero PTR SIZE)              → nil
//!
//! All reads/writes use `read_unaligned`/`write_unaligned` so the
//! caller doesn't have to think about field alignment — the C side
//! always packs structs to their natural alignment, which is fine.
//!
//! Safety
//! ──────
//! These primitives are unsound by construction — they let any Lisp
//! code dereference any pointer and write any value. That's the
//! point: they're the bridge between Lisp's safe world and the C
//! ABI. Mistakes here are debugging puzzles, not type errors.
//!
//! The `defstruct-win32` macro (Lisp/Library/win32-buffer.lisp)
//! layers offset/size discipline on top so user code doesn't have
//! to hand-write offsets.

use std::alloc::{alloc_zeroed, dealloc, Layout};

use crate::mutator::MutatorState;
use crate::word::Word;

fn arg_fixnum(args: *const u64, i: u64, name: &str) -> i64 {
    let w = Word::from_raw(unsafe { *args.add(i as usize) });
    w.as_fixnum()
        .unwrap_or_else(|| panic!("{name}: arg {i} must be an integer, got {w:?}"))
}

fn check_arity(name: &str, n: u64, want: u64) {
    if n != want {
        panic!("{name}: expected {want} args, got {n}");
    }
}

// ─── allocation ──────────────────────────────────────────────────

/// `(make-foreign-buffer SIZE)` — allocate SIZE bytes of zeroed
/// memory, return the raw address as a fixnum.
///
/// Allocation is via the global Rust allocator with 8-byte
/// alignment (matches Win32 record alignment requirements). The
/// buffer lives until `free-foreign-buffer` is called with the
/// same address+size. Leaks are leaks; this isn't GC'd.
pub extern "C-unwind" fn make_foreign_buffer_shim(
    _m: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    check_arity("make-foreign-buffer", n_args, 1);
    let size = arg_fixnum(args, 0, "make-foreign-buffer");
    if size <= 0 {
        panic!("make-foreign-buffer: size must be positive, got {size}");
    }
    let layout = Layout::from_size_align(size as usize, 8)
        .unwrap_or_else(|e| panic!("make-foreign-buffer: bad layout: {e}"));
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        panic!("make-foreign-buffer: allocation of {size} bytes failed");
    }
    Word::fixnum(ptr as i64).raw()
}

/// `(free-foreign-buffer PTR SIZE)` — deallocate. Pass the same
/// size you allocated with; the global allocator needs the layout
/// back to deallocate correctly.
pub extern "C-unwind" fn free_foreign_buffer_shim(
    _m: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    check_arity("free-foreign-buffer", n_args, 2);
    let ptr = arg_fixnum(args, 0, "free-foreign-buffer") as *mut u8;
    let size = arg_fixnum(args, 1, "free-foreign-buffer");
    if !ptr.is_null() && size > 0 {
        let layout = Layout::from_size_align(size as usize, 8)
            .unwrap_or_else(|e| panic!("free-foreign-buffer: bad layout: {e}"));
        unsafe { dealloc(ptr, layout) };
    }
    Word::NIL.raw()
}

/// `(buffer-zero PTR SIZE)` — zero SIZE bytes at PTR.
pub extern "C-unwind" fn buffer_zero_shim(
    _m: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    check_arity("buffer-zero", n_args, 2);
    let ptr = arg_fixnum(args, 0, "buffer-zero") as *mut u8;
    let size = arg_fixnum(args, 1, "buffer-zero");
    if size > 0 && !ptr.is_null() {
        unsafe { std::ptr::write_bytes(ptr, 0, size as usize) };
    }
    Word::NIL.raw()
}

// ─── typed reads ─────────────────────────────────────────────────

macro_rules! buffer_ref {
    ($shim:ident, $rust_ty:ty, $name:literal, $signed:expr) => {
        pub extern "C-unwind" fn $shim(
            _m: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            check_arity($name, n_args, 2);
            let ptr = arg_fixnum(args, 0, $name) as *const u8;
            let off = arg_fixnum(args, 1, $name);
            let raw = unsafe {
                let p = ptr.add(off as usize) as *const $rust_ty;
                p.read_unaligned()
            };
            if $signed {
                Word::fixnum(raw as i64).raw()
            } else {
                // For u64 specifically we cast through i64. Values
                // > i64::MAX truncate; bignum boxing is a Phase 6
                // refinement.
                Word::fixnum(raw as i64).raw()
            }
        }
    };
}

macro_rules! buffer_set {
    ($shim:ident, $rust_ty:ty, $name:literal) => {
        pub extern "C-unwind" fn $shim(
            _m: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            check_arity($name, n_args, 3);
            let ptr = arg_fixnum(args, 0, $name) as *mut u8;
            let off = arg_fixnum(args, 1, $name);
            let val = arg_fixnum(args, 2, $name);
            unsafe {
                let p = ptr.add(off as usize) as *mut $rust_ty;
                p.write_unaligned(val as $rust_ty);
            }
            Word::NIL.raw()
        }
    };
}

buffer_ref!(buffer_ref_u8_shim,  u8,  "buffer-ref-u8",  false);
buffer_ref!(buffer_ref_i8_shim,  i8,  "buffer-ref-i8",  true);
buffer_ref!(buffer_ref_u16_shim, u16, "buffer-ref-u16", false);
buffer_ref!(buffer_ref_i16_shim, i16, "buffer-ref-i16", true);
buffer_ref!(buffer_ref_u32_shim, u32, "buffer-ref-u32", false);
buffer_ref!(buffer_ref_i32_shim, i32, "buffer-ref-i32", true);
buffer_ref!(buffer_ref_u64_shim, u64, "buffer-ref-u64", false);
buffer_ref!(buffer_ref_i64_shim, i64, "buffer-ref-i64", true);

buffer_set!(buffer_set_u8_shim,  u8,  "buffer-set-u8");
buffer_set!(buffer_set_i8_shim,  i8,  "buffer-set-i8");
buffer_set!(buffer_set_u16_shim, u16, "buffer-set-u16");
buffer_set!(buffer_set_i16_shim, i16, "buffer-set-i16");
buffer_set!(buffer_set_u32_shim, u32, "buffer-set-u32");
buffer_set!(buffer_set_i32_shim, i32, "buffer-set-i32");
buffer_set!(buffer_set_u64_shim, u64, "buffer-set-u64");
buffer_set!(buffer_set_i64_shim, i64, "buffer-set-i64");

// ptr-sized read/write (8 bytes on x64). Same as u64 ABI-wise but
// gives the user a more meaningful name in struct-layout code.
buffer_ref!(buffer_ref_ptr_shim, u64, "buffer-ref-ptr", false);
buffer_set!(buffer_set_ptr_shim, u64, "buffer-set-ptr");

// ─── strings ─────────────────────────────────────────────────────

/// `(buffer-read-wstring PTR LEN)` — interpret PTR as a UTF-16
/// buffer of LEN u16 code units (NOT bytes), decode to a Lisp
/// string. Stops early at the first NUL u16 if any.
pub extern "C-unwind" fn buffer_read_wstring_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    check_arity("buffer-read-wstring", n_args, 2);
    let ptr = arg_fixnum(args, 0, "buffer-read-wstring") as *const u16;
    let cap = arg_fixnum(args, 1, "buffer-read-wstring");
    if cap < 0 {
        panic!("buffer-read-wstring: capacity must be non-negative");
    }
    let mut units = Vec::with_capacity(cap as usize);
    unsafe {
        for i in 0..cap as usize {
            let u = ptr.add(i).read_unaligned();
            if u == 0 { break; }
            units.push(u);
        }
    }
    let s = String::from_utf16_lossy(&units);
    let m = unsafe { &mut *mutator };
    crate::gc_string::alloc_string_in_young(m, &s).raw()
}

/// `(buffer-write-wstring PTR OFFSET STR)` — encode STR as UTF-16
/// and write it at PTR+OFFSET, followed by a NUL u16. Returns the
/// number of u16 code units written (excluding the NUL).
pub extern "C-unwind" fn buffer_write_wstring_shim(
    _m: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    check_arity("buffer-write-wstring", n_args, 3);
    let ptr = arg_fixnum(args, 0, "buffer-write-wstring") as *mut u16;
    let off = arg_fixnum(args, 1, "buffer-write-wstring");
    let s_w = Word::from_raw(unsafe { *args.add(2) });
    if s_w.tag() != crate::word::Tag::String {
        panic!("buffer-write-wstring: third arg must be a string, got {s_w:?}");
    }
    let s: String = crate::gc_string::chars_of(s_w).collect();
    let units: Vec<u16> = s.encode_utf16().collect();
    let dst = unsafe { ptr.add(off as usize) };
    unsafe {
        for (i, u) in units.iter().enumerate() {
            dst.add(i).write_unaligned(*u);
        }
        dst.add(units.len()).write_unaligned(0);
    }
    Word::fixnum(units.len() as i64).raw()
}
