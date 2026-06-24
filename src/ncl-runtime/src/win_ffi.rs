//! Windows FFI kernel. Phase 3 of `docs/WINDOWS_FFI.md`.
//!
//! Lisp-callable shim `(%ffi-call DLL FN ARG-TYPES RETURN-TYPE ARGS…
//! &key (route :auto))` plus the supporting caches and the
//! type-tag marshalling.
//!
//! Calling convention
//! ──────────────────
//! x64 Windows has one calling convention for everything we care
//! about. Integer/pointer args go through RCX, RDX, R8, R9, then
//! the stack; the callee gets 32 bytes of shadow space for spilling
//! them back. Floats go through XMM0..3, then the stack. Return is
//! RAX or XMM0.
//!
//! For Phase 3 we support only integer-like arg types — anything
//! that passes through the integer register file. That covers:
//!
//!   :i8 :i16 :i32 :i64 :u8 :u16 :u32 :u64 :isize :usize :bool
//!   :handle :ptr :wstr :cstr :void
//!
//! Float types (`:f32`, `:f64`) come later — they need a separate
//! dispatcher because they ride in XMM registers, and a function
//! with mixed int+float args needs both register banks loaded with
//! correctly-typed values. Easy to add when needed; not needed for
//! the initial Console + WindowsAndMessaging bindings.
//!
//! The dispatcher is a per-arity transmute. We support up to 12
//! args — Win32 functions with more are vanishingly rare; we
//! diagnose and refuse them at the boundary rather than silently
//! truncating.
//!
//! Routing
//! ───────
//! `:route :any` — call directly on the invoking thread.
//! `:route :ui`  — marshal to the UI thread (thread 0) via
//!                  `WM_NCL_FFI_CALL`. Phase 3b will populate this
//!                  arm in `win_surface::dispatch_wnd_proc`. For
//!                  now `:ui` and `:any` behave identically — the
//!                  Win32 functions we're testing first (Beep,
//!                  GetTickCount64, etc.) are thread-safe.
//!
//! Memory
//! ──────
//! `:wstr` args build a UTF-16 NUL-terminated buffer on the
//! caller's stack (or a Box if too big for sane stack use); the
//! pointer is passed as a u64. Strings live for the duration of
//! the call.
//!
//! Return marshalling for `:wstr` would need the caller to specify
//! a length (no NUL guarantee on Win32 return strings). Phase 3
//! doesn't return strings — Phase 5 adds it with explicit length.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Mutex;

use crate::mutator::MutatorState;
use crate::word::{Tag, Word};

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{FARPROC, HMODULE};
#[cfg(windows)]
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
#[cfg(windows)]
use windows::core::PCSTR;

// ─── Type tags ────────────────────────────────────────────────────────
//
// Each arg / return type is one of these enum values. The Lisp
// representation is a keyword (`:i32`, `:wstr`, …); the parser maps
// to TypeTag. We use plain u8 internally so `arg_type_tags: &[u8]`
// is simple.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
#[allow(non_camel_case_types)]
pub enum TypeTag {
    Void = 0,
    I8 = 1,
    U8 = 2,
    I16 = 3,
    U16 = 4,
    I32 = 5,
    U32 = 6,
    I64 = 7,
    U64 = 8,
    Isize = 9,
    Usize = 10,
    Bool = 11,
    Handle = 12,
    Ptr = 13,
    Wstr = 14,
    Cstr = 15,
}

impl TypeTag {
    /// Parse a `:type` keyword Word into a TypeTag. Returns None
    /// for unrecognised keywords (the shim raises a clean error).
    fn from_keyword_name(name: &str) -> Option<TypeTag> {
        match name {
            ":VOID"   | "VOID"   => Some(TypeTag::Void),
            ":I8"     | "I8"     => Some(TypeTag::I8),
            ":U8"     | "U8"     => Some(TypeTag::U8),
            ":I16"    | "I16"    => Some(TypeTag::I16),
            ":U16"    | "U16"    => Some(TypeTag::U16),
            ":I32"    | "I32"    => Some(TypeTag::I32),
            ":U32"    | "U32"    => Some(TypeTag::U32),
            ":I64"    | "I64"    => Some(TypeTag::I64),
            ":U64"    | "U64"    => Some(TypeTag::U64),
            ":ISIZE"  | "ISIZE"  => Some(TypeTag::Isize),
            ":USIZE"  | "USIZE"  => Some(TypeTag::Usize),
            ":BOOL"   | "BOOL"   => Some(TypeTag::Bool),
            ":HANDLE" | "HANDLE" => Some(TypeTag::Handle),
            ":PTR"    | "PTR"    => Some(TypeTag::Ptr),
            ":WSTR"   | "WSTR"   => Some(TypeTag::Wstr),
            ":CSTR"   | "CSTR"   => Some(TypeTag::Cstr),
            _ => None,
        }
    }

    fn is_signed(self) -> bool {
        matches!(self, TypeTag::I8 | TypeTag::I16 | TypeTag::I32 | TypeTag::I64 | TypeTag::Isize)
    }

    /// Map a packed byte tag back to a TypeTag. Must agree with
    /// the tag numbering in `scripts/generate_win32_pack.py`.
    pub fn from_byte(b: u8) -> Option<TypeTag> {
        Some(match b {
            0 => TypeTag::Void,
            1 => TypeTag::I8,
            2 => TypeTag::U8,
            3 => TypeTag::I16,
            4 => TypeTag::U16,
            5 => TypeTag::I32,
            6 => TypeTag::U32,
            7 => TypeTag::I64,
            8 => TypeTag::U64,
            9 => TypeTag::Isize,
            10 => TypeTag::Usize,
            11 => TypeTag::Bool,
            12 => TypeTag::Handle,
            13 => TypeTag::Ptr,
            14 => TypeTag::Wstr,
            15 => TypeTag::Cstr,
            _ => return None,
        })
    }

    /// The :keyword name used in the Lisp surface (`':I32` etc).
    /// Used by %win32-lookup when building the plist returned to
    /// macro-expansion-time code.
    pub fn keyword_name(self) -> &'static str {
        match self {
            TypeTag::Void   => ":VOID",
            TypeTag::I8     => ":I8",
            TypeTag::U8     => ":U8",
            TypeTag::I16    => ":I16",
            TypeTag::U16    => ":U16",
            TypeTag::I32    => ":I32",
            TypeTag::U32    => ":U32",
            TypeTag::I64    => ":I64",
            TypeTag::U64    => ":U64",
            TypeTag::Isize  => ":ISIZE",
            TypeTag::Usize  => ":USIZE",
            TypeTag::Bool   => ":BOOL",
            TypeTag::Handle => ":HANDLE",
            TypeTag::Ptr    => ":PTR",
            TypeTag::Wstr   => ":WSTR",
            TypeTag::Cstr   => ":CSTR",
        }
    }
}

// ─── DLL / proc caches ────────────────────────────────────────────────

// HMODULE / FARPROC wrap raw pointers; `*mut c_void` is `!Send`.
// The underlying loaded library lives for the process lifetime and
// is thread-safe to call concurrently (LoadLibrary / GetProcAddress
// are documented thread-safe), so a SAFETY-asserted wrapper is the
// idiomatic move.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct SendableHmodule(HMODULE);
#[cfg(windows)]
unsafe impl Send for SendableHmodule {}
#[cfg(windows)]
unsafe impl Sync for SendableHmodule {}

#[cfg(windows)]
#[derive(Clone, Copy)]
struct SendableFarproc(FARPROC);
#[cfg(windows)]
unsafe impl Send for SendableFarproc {}
#[cfg(windows)]
unsafe impl Sync for SendableFarproc {}

#[cfg(windows)]
struct DllCache {
    modules: HashMap<String, SendableHmodule>,
    procs: HashMap<(String, String), SendableFarproc>,
}

#[cfg(windows)]
impl DllCache {
    fn new() -> Self {
        DllCache {
            modules: HashMap::new(),
            procs: HashMap::new(),
        }
    }
}

#[cfg(windows)]
static DLL_CACHE: Mutex<Option<DllCache>> = Mutex::new(None);

#[cfg(windows)]
fn cache() -> std::sync::MutexGuard<'static, Option<DllCache>> {
    let mut guard = DLL_CACHE.lock().unwrap();
    if guard.is_none() {
        *guard = Some(DllCache::new());
    }
    guard
}

#[cfg(windows)]
fn load_dll(name: &str) -> HMODULE {
    {
        let g = cache();
        if let Some(c) = g.as_ref() {
            if let Some(h) = c.modules.get(name) {
                return h.0;
            }
        }
    }
    let utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { LoadLibraryW(PCWSTR(utf16.as_ptr())) }
        .unwrap_or_else(|e| panic!("LoadLibraryW({name:?}) failed: {e}"));
    if let Some(c) = cache().as_mut() {
        c.modules.insert(name.to_string(), SendableHmodule(h));
    }
    h
}

#[cfg(windows)]
fn get_proc(dll: &str, fn_name: &str) -> *const c_void {
    {
        let g = cache();
        if let Some(c) = g.as_ref() {
            if let Some(p) = c.procs.get(&(dll.to_string(), fn_name.to_string())) {
                return p.0.map(|f| f as *const c_void).unwrap_or(std::ptr::null());
            }
        }
    }
    let h = load_dll(dll);
    let asciiz: Vec<u8> = fn_name.bytes().chain(std::iter::once(0)).collect();
    let p = unsafe { GetProcAddress(h, PCSTR(asciiz.as_ptr())) };
    if let Some(c) = cache().as_mut() {
        c.procs.insert((dll.to_string(), fn_name.to_string()), SendableFarproc(p));
    }
    p.map(|f| f as *const c_void).unwrap_or(std::ptr::null())
}

// ─── Arg marshalling ──────────────────────────────────────────────────

/// Lifetime carrier for any owned-side data (`Vec<u16>` for `:wstr`,
/// `Vec<u8>` for `:cstr`) we have to keep alive across the FFI call.
/// The `args` u64 slice we hand to the dispatcher holds pointers
/// into these buffers; if the carrier dropped, we'd dangle.
struct ArgCarrier {
    _wstrs: Vec<Vec<u16>>,
    _cstrs: Vec<Vec<u8>>,
}

impl ArgCarrier {
    fn new() -> Self {
        ArgCarrier {
            _wstrs: Vec::new(),
            _cstrs: Vec::new(),
        }
    }
}

/// Marshal one Lisp Word into a u64 argument slot per the given
/// type tag. Returns the slot value. For `:wstr` / `:cstr`, the
/// underlying buffer is stored in the carrier so it lives until the
/// FFI call returns.
fn marshal_in(w: Word, tag: TypeTag, carrier: &mut ArgCarrier) -> u64 {
    match tag {
        TypeTag::Void => panic!("%ffi-call: :void is not a valid arg type"),
        TypeTag::Bool => {
            // T → 1, NIL → 0. Anything else is an error.
            if w.is_nil() {
                0
            } else if w.raw() == Word::T.raw() {
                1
            } else {
                panic!("%ffi-call :bool arg: expected T or NIL, got {w:?}");
            }
        }
        TypeTag::I8
        | TypeTag::U8
        | TypeTag::I16
        | TypeTag::U16
        | TypeTag::I32
        | TypeTag::U32
        | TypeTag::I64
        | TypeTag::U64
        | TypeTag::Isize
        | TypeTag::Usize
        | TypeTag::Handle
        | TypeTag::Ptr => marshal_integer(w, tag),
        TypeTag::Wstr => {
            // Three legal inputs:
            //   NIL      → null pointer
            //   string   → marshal into a UTF-16 buffer, pass pointer
            //   integer  → treat as already-prepared pointer (the
            //              caller did make-foreign-buffer +
            //              buffer-write-wstring) OR a class atom.
            //              Many Win32 APIs (CreateWindowExW,
            //              FindResourceW, LoadStringW…) accept either
            //              a pointer or an integer atom in an LPCWSTR
            //              parameter — MAKEINTRESOURCE-style usage.
            if w.is_nil() {
                0
            } else if w.tag() == Tag::String {
                let s: String = crate::gc_string::chars_of(w).collect();
                let mut buf: Vec<u16> = s.encode_utf16().collect();
                buf.push(0); // NUL terminator
                let ptr = buf.as_ptr() as u64;
                carrier._wstrs.push(buf);
                ptr
            } else if let Some(n) = w.as_fixnum() {
                n as u64
            } else if crate::bignum::is_bignum(w) {
                marshal_integer(w, TypeTag::U64)
            } else {
                panic!(
                    "%ffi-call :wstr arg: expected string, integer, or NIL; got {w:?}"
                );
            }
        }
        TypeTag::Cstr => {
            // Same three-way input as :wstr (NIL / string / pointer).
            if w.is_nil() {
                0
            } else if w.tag() == Tag::String {
                // ASCII / Latin-1 best-effort encoding. Chars > 0xFF
                // truncate — Phase 5 will surface that as a warning
                // or do proper code-page conversion.
                let s: String = crate::gc_string::chars_of(w).collect();
                let mut buf: Vec<u8> = s.bytes().collect();
                buf.push(0);
                let ptr = buf.as_ptr() as u64;
                carrier._cstrs.push(buf);
                ptr
            } else if let Some(n) = w.as_fixnum() {
                n as u64
            } else if crate::bignum::is_bignum(w) {
                marshal_integer(w, TypeTag::U64)
            } else {
                panic!(
                    "%ffi-call :cstr arg: expected string, integer, or NIL; got {w:?}"
                );
            }
        }
    }
}

fn marshal_integer(w: Word, tag: TypeTag) -> u64 {
    // Fast path for fixnums.
    if let Some(n) = w.as_fixnum() {
        return if tag.is_signed() {
            n as u64 // sign-extend by way of two's complement; the
                     // low N bits of u64 are what x64 reads from
                     // the integer register anyway
        } else {
            n as u64
        };
    }
    // Bignums representing u64/i64 values from earlier ops. We don't
    // support these yet — clean error for now.
    panic!(
        "%ffi-call integer arg: expected a fixnum, got {w:?} (bignum support: Phase 5)"
    );
}

/// Marshal the u64 return value back to a Lisp Word per the return
/// type tag.
fn marshal_out(raw: u64, tag: TypeTag) -> Word {
    match tag {
        TypeTag::Void => Word::NIL,
        TypeTag::Bool => {
            if raw == 0 {
                Word::NIL
            } else {
                Word::T
            }
        }
        TypeTag::I8   => Word::fixnum((raw as u8 as i8) as i64),
        TypeTag::U8   => Word::fixnum((raw as u8) as i64),
        TypeTag::I16  => Word::fixnum((raw as u16 as i16) as i64),
        TypeTag::U16  => Word::fixnum((raw as u16) as i64),
        TypeTag::I32  => Word::fixnum((raw as u32 as i32) as i64),
        TypeTag::U32  => Word::fixnum((raw as u32) as i64),
        TypeTag::I64  => Word::fixnum(raw as i64),
        TypeTag::U64  => {
            // u64 can exceed fixnum range. For Phase 3 we accept
            // truncation to i64; Phase 5 boxes into a bignum.
            Word::fixnum(raw as i64)
        }
        TypeTag::Isize | TypeTag::Usize | TypeTag::Handle | TypeTag::Ptr => {
            Word::fixnum(raw as i64)
        }
        TypeTag::Wstr | TypeTag::Cstr => {
            // Returning strings needs a length policy. Phase 3:
            // return the raw pointer as a fixnum; caller decodes
            // manually. Phase 5: proper string return.
            Word::fixnum(raw as i64)
        }
    }
}

// ─── Dispatcher ───────────────────────────────────────────────────────

/// Call FN_PTR with the given args. All args are treated as u64
/// (the integer register/stack slot); float args (XMM) are not yet
/// supported.
///
/// We support up to 12 args by listing function-pointer types
/// explicitly. Beyond 12 we refuse — every Win32 function we'd
/// reasonably want to bind fits.
#[cfg(windows)]
unsafe fn call_dispatch(fn_ptr: *const c_void, args: &[u64]) -> u64 {
    use std::mem::transmute;
    match args.len() {
        0 => unsafe {
            let f: extern "system" fn() -> u64 = transmute(fn_ptr);
            f()
        },
        1 => unsafe {
            let f: extern "system" fn(u64) -> u64 = transmute(fn_ptr);
            f(args[0])
        },
        2 => unsafe {
            let f: extern "system" fn(u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1])
        },
        3 => unsafe {
            let f: extern "system" fn(u64, u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1], args[2])
        },
        4 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3])
        },
        5 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3], args[4])
        },
        6 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3], args[4], args[5])
        },
        7 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 = transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3], args[4], args[5], args[6])
        },
        8 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7])
        },
        9 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                transmute(fn_ptr);
            f(args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8])
        },
        10 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                transmute(fn_ptr);
            f(
                args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8],
                args[9],
            )
        },
        11 => unsafe {
            let f: extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                transmute(fn_ptr);
            f(
                args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8],
                args[9], args[10],
            )
        },
        12 => unsafe {
            let f: extern "system" fn(
                u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64,
            ) -> u64 = transmute(fn_ptr);
            f(
                args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7], args[8],
                args[9], args[10], args[11],
            )
        },
        n => panic!("%ffi-call: arity {n} exceeds supported maximum of 12"),
    }
}

#[cfg(not(windows))]
unsafe fn call_dispatch(_fn_ptr: *const c_void, _args: &[u64]) -> u64 {
    panic!("call_dispatch invoked on a non-Windows platform");
}

// ─── Lisp shim ────────────────────────────────────────────────────────

/// `(%ffi-call DLL FN ARG-TYPES RETURN-TYPE ARGS…)`
///
/// `DLL` and `FN` are strings. `ARG-TYPES` is a list of keyword
/// type tags. `RETURN-TYPE` is one keyword. `ARGS…` are the
/// remaining positional arguments — there must be exactly one per
/// entry in ARG-TYPES.
///
/// For Phase 3 we don't yet honour a `:route` kwarg — every call
/// is `:any` (runs on the calling thread). Phase 3b will wire the
/// `:ui` route through `WM_NCL_FFI_CALL`.
///
/// Errors:
///   - Bad arity (fewer/more args than ARG-TYPES says)
///   - DLL load failure
///   - Unknown function name (GetProcAddress returned null)
///   - Unknown type tag keyword
///   - Arg arity over 12 (refuse rather than truncate)
pub extern "C-unwind" fn ffi_call_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args < 4 {
        panic!("%ffi-call: expected at least 4 args (DLL FN ARG-TYPES RETURN-TYPE), got {n_args}");
    }

    // Unwrap fixed leading args.
    let read = |i: u64| Word::from_raw(unsafe { *args.add(i as usize) });
    let dll_w = read(0);
    let fn_w = read(1);
    let arg_types_w = read(2);
    let return_type_w = read(3);

    let dll = string_of(dll_w).expect("%ffi-call: DLL arg must be a string");
    let fn_name = string_of(fn_w).expect("%ffi-call: FN arg must be a string");

    let arg_tags = list_of_type_tags(arg_types_w);
    let return_tag = single_type_tag(return_type_w);

    // Check positional-arg count matches type list.
    let expected_args = arg_tags.len() as u64;
    let actual_args = n_args - 4;
    if expected_args != actual_args {
        panic!(
            "%ffi-call: function {fn_name:?} expects {expected_args} args but got {actual_args}"
        );
    }

    #[cfg(windows)]
    {
        // Resolve the function.
        let proc = get_proc(&dll, &fn_name);
        if proc.is_null() {
            panic!("%ffi-call: GetProcAddress({dll:?}, {fn_name:?}) returned null");
        }

        // Marshal args.
        let mut carrier = ArgCarrier::new();
        let mut marshalled: Vec<u64> = Vec::with_capacity(arg_tags.len());
        for (i, &tag) in arg_tags.iter().enumerate() {
            let w = read(4 + i as u64);
            marshalled.push(marshal_in(w, tag, &mut carrier));
        }

        // Call.
        let raw = unsafe { call_dispatch(proc, &marshalled) };

        // Marshal return.
        drop(carrier); // explicit: ensure buffers stay alive past the call
        marshal_out(raw, return_tag).raw()
    }

    #[cfg(not(windows))]
    {
        let _ = (dll, fn_name, arg_tags, return_tag);
        panic!("%ffi-call: not supported on this platform");
    }
}

// ─── Word decoders ─────────────────────────────────────────────────────

fn string_of(w: Word) -> Option<String> {
    if w.tag() != Tag::String {
        return None;
    }
    Some(crate::gc_string::chars_of(w).collect())
}

/// Parse a Lisp list of keyword type tags into a Vec<TypeTag>.
/// NIL → empty vec. Anything else not a proper list of keywords
/// raises a clean error.
fn list_of_type_tags(w: Word) -> Vec<TypeTag> {
    let mut out = Vec::new();
    let mut cur = w;
    while !cur.is_nil() {
        if cur.tag() != Tag::Cons {
            panic!("%ffi-call ARG-TYPES: improper list ending in {cur:?}");
        }
        let car = unsafe {
            let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons ptr");
            Word::from_raw(*p)
        };
        out.push(single_type_tag(car));
        cur = unsafe {
            let p = cur.as_ptr::<u64>(Tag::Cons).expect("cons ptr");
            Word::from_raw(*p.add(1))
        };
    }
    out
}

// ─── Metadata-pack-driven shims ───────────────────────────────────────

/// `(%win32-lookup NAME)` — look up NAME in the loaded metadata
/// pack. Returns NIL if the pack isn't loaded or the name isn't
/// present. Otherwise returns a plist:
///
///   (:dll "USER32.dll" :args (:handle :wstr :wstr :u32)
///    :ret :i32 :sle T :route :ui :aw #\W)
///
/// Used by `(defwin32 …)` at macroexpansion time to bake the
/// signature into a generated `(defun …)`.
pub extern "C-unwind" fn win32_lookup_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%win32-lookup: expected 1 arg (function name string), got {n_args}");
    }
    let name_w = Word::from_raw(unsafe { *args });
    let name = string_of(name_w)
        .unwrap_or_else(|| panic!("%win32-lookup: arg must be a string, got {name_w:?}"));

    let Some(meta) = crate::win_metadata::lookup(&name) else {
        return Word::NIL.raw();
    };
    let m = unsafe { &mut *mutator };
    build_lookup_plist(m, meta).raw()
}

/// `(%win32-call NAME &rest user-args)` — look up NAME in the
/// metadata pack, marshal user-args per its signature, and call.
/// Errors with a clean message if NAME isn't in the pack.
pub extern "C-unwind" fn win32_call_shim(
    mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args == 0 {
        panic!("%win32-call: expected at least 1 arg (function name)");
    }
    let read = |i: u64| Word::from_raw(unsafe { *args.add(i as usize) });
    let name_w = read(0);
    let name = string_of(name_w)
        .unwrap_or_else(|| panic!("%win32-call: name arg must be a string, got {name_w:?}"));

    let Some(meta) = crate::win_metadata::lookup(&name) else {
        panic!("%win32-call: function {name:?} not in metadata pack");
    };

    let user_arg_count = n_args - 1;
    let expected = meta.arg_tags.len() as u64;
    if user_arg_count != expected {
        panic!(
            "%win32-call: function {name:?} expects {expected} args but got {user_arg_count}"
        );
    }

    #[cfg(windows)]
    {
        let proc = get_proc(meta.dll, meta.name);
        if proc.is_null() {
            panic!("%win32-call: GetProcAddress({:?}, {:?}) returned null", meta.dll, meta.name);
        }
        let mut carrier = ArgCarrier::new();
        let mut marshalled: Vec<u64> = Vec::with_capacity(meta.arg_tags.len());
        for (i, &tag_byte) in meta.arg_tags.iter().enumerate() {
            let tag = TypeTag::from_byte(tag_byte)
                .unwrap_or_else(|| panic!("%win32-call: bad tag byte {tag_byte} for {name:?}"));
            let w = read(1 + i as u64);
            // Attach call context so panics from marshal_in identify
            // which API and which positional arg failed — crucial
            // when debugging FFI marshalling (otherwise the user gets
            // a bare "expected fixnum" with no way to find which
            // (win32 …) call upstream is at fault).
            let marshalled_one = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(|| marshal_in(w, tag, &mut carrier)),
            ).unwrap_or_else(|payload| {
                let msg = payload.downcast_ref::<String>().cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<unknown>".to_string());
                panic!(
                    "%win32-call: marshalling arg {i} ({tag:?}) of {name:?} failed: {msg}"
                );
            });
            marshalled.push(marshalled_one);
        }
        let raw = unsafe { call_dispatch(proc, &marshalled) };
        drop(carrier);
        let ret_tag = TypeTag::from_byte(meta.ret_tag)
            .unwrap_or_else(|| panic!("%win32-call: bad ret tag for {name:?}"));
        let _ = mutator;
        marshal_out(raw, ret_tag).raw()
    }

    #[cfg(not(windows))]
    {
        let _ = (mutator, meta);
        panic!("%win32-call: not supported on this platform");
    }
}

/// Build the plist returned by `%win32-lookup`. Allocates several
/// short-lived strings + a few interned keyword symbols; this is
/// macroexpand-time, not hot-path, so cost doesn't matter.
fn build_lookup_plist(m: &mut MutatorState, meta: &'static crate::win_metadata::WinFn) -> Word {
    use crate::abi::ncl_alloc_cons;
    // Build the arg-tags list (each entry a keyword like :i32)
    let mut args_list = Word::NIL;
    for &b in meta.arg_tags.iter().rev() {
        let tag = TypeTag::from_byte(b).unwrap_or(TypeTag::Void);
        let kw = intern_keyword(m, tag.keyword_name());
        let new = unsafe { ncl_alloc_cons(m as *mut _, kw.raw(), args_list.raw()) };
        args_list = Word::from_raw(new);
    }
    let ret_kw = intern_keyword(m,
        TypeTag::from_byte(meta.ret_tag).unwrap_or(TypeTag::Void).keyword_name());
    let dll = crate::gc_string::alloc_string_in_young(m, meta.dll);
    let route_kw = if meta.route_ui {
        intern_keyword(m, ":UI")
    } else {
        intern_keyword(m, ":ANY")
    };
    let sle_w = if meta.set_last_error { Word::T } else { Word::NIL };

    // Build plist right-to-left
    let key_route = intern_keyword(m, ":ROUTE");
    let key_sle = intern_keyword(m, ":SLE");
    let key_ret = intern_keyword(m, ":RET");
    let key_args = intern_keyword(m, ":ARGS");
    let key_dll = intern_keyword(m, ":DLL");

    let mut list = Word::NIL;
    for (k, v) in [
        (key_route, route_kw),
        (key_sle, sle_w),
        (key_ret, ret_kw),
        (key_args, args_list),
        (key_dll, dll),
    ] {
        let c1 = unsafe { ncl_alloc_cons(m as *mut _, v.raw(), list.raw()) };
        list = Word::from_raw(c1);
        let c2 = unsafe { ncl_alloc_cons(m as *mut _, k.raw(), list.raw()) };
        list = Word::from_raw(c2);
    }
    list
}

fn intern_keyword(m: &mut MutatorState, name: &str) -> Word {
    m.coord().intern(name)
}

fn single_type_tag(w: Word) -> TypeTag {
    if w.tag() != Tag::Symbol {
        panic!("%ffi-call: expected a type keyword, got {w:?}");
    }
    // Symbol names live in the process-wide `sym_names` registry,
    // not in the symbol's own name cell (which is NIL at allocation
    // time). See abi::symbol_name_shim for the canonical lookup.
    let name = crate::sym_names::lookup(w.raw())
        .map(|s| s.to_string())
        .unwrap_or_else(|| panic!("%ffi-call: type tag symbol has no name: {w:?}"));
    TypeTag::from_keyword_name(&name)
        .unwrap_or_else(|| panic!("%ffi-call: unknown type tag {name:?}"))
}
