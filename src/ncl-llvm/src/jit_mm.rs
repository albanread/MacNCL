//! Custom MCJIT memory manager that captures section addresses
//! by name and registers Windows SEH unwind tables for the JIT'd
//! code with `RtlAddFunctionTable`.
//!
//! Why this exists
//! ---------------
//! LLVM emits `.pdata` and `.xdata` sections for every function that
//! carries the `uwtable` attribute (every JIT'd Lisp function in our
//! case — see `build_lisp_function`). Those sections describe how
//! the Windows SEH unwinder should walk the function's frame. But
//! the OS unwinder doesn't scan memory for these — it consults a
//! registry. Statically-linked PE images are auto-registered. JIT'd
//! code in `VirtualAlloc`-ed memory is not.
//!
//! Without registration, a Rust `panic!` raised inside a runtime
//! helper (`error_shim`, `condvar_wait_shim`, etc.) reaches the JIT
//! frame above it, finds no unwind info known to the OS, and gets
//! reported as `0xE06D7363` ("C++ exception not caught") which on
//! Windows aborts the process. With registration, that same panic
//! unwinds cleanly back through the JIT frame to whatever
//! `catch_unwind` boundary lives in the host (the `Session::eval`
//! loop, the spawn thunk for created threads, the condition
//! `handler-case` machinery).
//!
//! Design
//! ------
//! Per JIT'd module we allocate sections via `VirtualAlloc` and
//! capture every `.pdata` / `.xdata` / `.text` we see by exact name
//! match. On finalize we:
//!   1. Flip code sections to `PAGE_EXECUTE_READ`.
//!   2. Use the lowest `.text` address as the SEH BaseAddress.
//!   3. Sanity-check: `BaseAddress + first_entry.BeginAddress` must
//!      land inside the captured `.text` range, and the range must
//!      fit within u32 RVA reach.
//!   4. Call `RtlAddFunctionTable` for each captured `.pdata`.
//!
//! Memory is leaked for the process lifetime — matches the existing
//! `keep_forever(engine)` contract. When module retirement lands we
//! pair `RtlDeleteFunctionTable` + `VirtualFree` in `destroy`.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_void, CStr};
use std::sync::Mutex;

use llvm_sys::execution_engine::{
    LLVMCreateSimpleMCJITMemoryManager, LLVMMCJITMemoryManagerRef,
};

// ── Windows API shim ──────────────────────────────────────────────────

#[cfg(windows)]
mod win {
    use super::*;

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const PAGE_NOACCESS: u32 = 0x01;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const PAGE_EXECUTE_READ: u32 = 0x20;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct RUNTIME_FUNCTION {
        pub BeginAddress: u32,
        pub EndAddress: u32,
        pub UnwindData: u32,
    }

    unsafe extern "system" {
        pub fn VirtualAlloc(
            lpAddress: *mut c_void,
            dwSize: usize,
            flAllocationType: u32,
            flProtect: u32,
        ) -> *mut c_void;
        pub fn VirtualProtect(
            lpAddress: *mut c_void,
            dwSize: usize,
            flNewProtect: u32,
            lpflOldProtect: *mut u32,
        ) -> i32;
        pub fn RtlAddFunctionTable(
            FunctionTable: *const RUNTIME_FUNCTION,
            EntryCount: u32,
            BaseAddress: u64,
        ) -> u8;
    }
}

// ── Per-allocation record ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Section {
    name_tag: NameTag,
    ptr: *mut u8,
    size: usize,
    is_code: bool,
}

unsafe impl Send for Section {}
unsafe impl Sync for Section {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NameTag {
    Text,
    Pdata,
    Xdata,
    Other,
}

impl NameTag {
    fn from_name(name: *const c_char) -> NameTag {
        if name.is_null() {
            return NameTag::Other;
        }
        let s = unsafe { CStr::from_ptr(name) };
        match s.to_bytes() {
            b".text" => NameTag::Text,
            b".pdata" => NameTag::Pdata,
            b".xdata" => NameTag::Xdata,
            _ => NameTag::Other,
        }
    }
}

// ── Per-engine memory manager state ───────────────────────────────────

const PAGE: usize = 4096;
/// One reservation per JIT'd module. Has to be big enough for all
/// sections, since `IMAGE_REL_AMD64_ADDR32NB` relocations in `.pdata`
/// reference `.text` and `.xdata` as 32-bit RVAs — all sections must
/// sit within 4 GiB of each other. A 4 MiB reservation is plenty for
/// any individual Lisp module we emit (typical: a few hundred bytes
/// of `.text`, double-digit bytes of `.pdata`/`.xdata`); we'd revisit
/// only if a single module ever needed more than that.
const MODULE_RESERVE: usize = 4 * 1024 * 1024;

struct Bump {
    base: *mut u8,
    size: usize,
    used: usize,
}

unsafe impl Send for Bump {}

/// State carried as the `opaque` field of the LLVM C MM callbacks.
/// One instance per JIT'd module (per ExecutionEngine). Owns one
/// contiguous virtual reservation; sections bump-allocate inside it
/// so RVAs in `.pdata` reach `.text` and `.xdata` within u32 range.
pub struct JitMm {
    bump: Mutex<Bump>,
    sections: Mutex<Vec<Section>>,
}

impl JitMm {
    fn new() -> Box<JitMm> {
        let base = reserve_module_region();
        Box::new(JitMm {
            bump: Mutex::new(Bump {
                base,
                size: MODULE_RESERVE,
                used: 0,
            }),
            sections: Mutex::new(Vec::new()),
        })
    }

    fn alloc(&self, size: usize, align: u32) -> *mut u8 {
        if size == 0 {
            return std::ptr::null_mut();
        }
        let mut b = self.bump.lock().unwrap();
        if b.base.is_null() {
            return std::ptr::null_mut();
        }
        // Each section starts on its own page. Page-rounding makes
        // VirtualProtect on finalize trivial — we flip whole pages
        // to EXECUTE_READ without dragging adjacent data along.
        let start = (b.used + (PAGE - 1)) & !(PAGE - 1);
        let user_align = (align as usize).max(8);
        let start = (start + user_align - 1) & !(user_align - 1);
        let end = start + size;
        let end_rounded = (end + (PAGE - 1)) & !(PAGE - 1);
        if end_rounded > b.size {
            return std::ptr::null_mut();
        }
        let p = unsafe { b.base.add(start) };
        // Commit only the pages we're about to use. The reservation
        // is pre-reserved (no physical memory committed), and commit
        // happens on demand so resident memory tracks actual section
        // size rather than the 4 MiB envelope.
        if !commit_pages(p, end_rounded - start) {
            return std::ptr::null_mut();
        }
        b.used = end_rounded;
        p
    }

    fn track(&self, sec: Section) {
        self.sections.lock().unwrap().push(sec);
    }
}

// ── Reserve + commit primitives ───────────────────────────────────────

#[cfg(windows)]
fn reserve_module_region() -> *mut u8 {
    unsafe {
        win::VirtualAlloc(
            std::ptr::null_mut(),
            MODULE_RESERVE,
            win::MEM_RESERVE,
            win::PAGE_NOACCESS,
        ) as *mut u8
    }
}

#[cfg(not(windows))]
fn reserve_module_region() -> *mut u8 {
    unimplemented!("jit_mm: non-Windows reservation not yet implemented")
}

#[cfg(windows)]
fn commit_pages(p: *mut u8, size: usize) -> bool {
    let r = unsafe {
        win::VirtualAlloc(
            p as *mut c_void,
            size,
            win::MEM_COMMIT,
            win::PAGE_READWRITE,
        )
    };
    !r.is_null()
}

#[cfg(not(windows))]
fn commit_pages(_p: *mut u8, _size: usize) -> bool {
    unimplemented!("jit_mm: non-Windows commit not yet implemented")
}

extern "C" fn allocate_code_section(
    opaque: *mut c_void,
    size: usize,
    alignment: u32,
    _section_id: u32,
    section_name: *const c_char,
) -> *mut u8 {
    let mm = unsafe { &*(opaque as *const JitMm) };
    let p = mm.alloc(size, alignment);
    if p.is_null() {
        return p;
    }
    mm.track(Section {
        name_tag: NameTag::from_name(section_name),
        ptr: p,
        size,
        is_code: true,
    });
    p
}

extern "C" fn allocate_data_section(
    opaque: *mut c_void,
    size: usize,
    alignment: u32,
    _section_id: u32,
    section_name: *const c_char,
    _is_readonly: i32,
) -> *mut u8 {
    let mm = unsafe { &*(opaque as *const JitMm) };
    let p = mm.alloc(size, alignment);
    if p.is_null() {
        return p;
    }
    mm.track(Section {
        name_tag: NameTag::from_name(section_name),
        ptr: p,
        size,
        is_code: false,
    });
    p
}

// ── Finalize: protect code, register SEH ──────────────────────────────

extern "C" fn finalize_memory(
    opaque: *mut c_void,
    err_msg: *mut *mut c_char,
) -> i32 {
    let mm = unsafe { &*(opaque as *const JitMm) };
    let sections = mm.sections.lock().unwrap().clone();

    // 1. Flip every code section to PAGE_EXECUTE_READ.
    #[cfg(windows)]
    {
        for sec in sections.iter().filter(|s| s.is_code) {
            let mut old: u32 = 0;
            let ok = unsafe {
                win::VirtualProtect(
                    sec.ptr as *mut c_void,
                    sec.size,
                    win::PAGE_EXECUTE_READ,
                    &mut old,
                )
            };
            if ok == 0 {
                let msg = b"VirtualProtect failed for code section\0";
                unsafe { *err_msg = msg.as_ptr() as *mut c_char };
                return 1;
            }
        }
    }

    // 2. Register Windows SEH unwind tables.
    #[cfg(windows)]
    {
        register_seh_for_module(&sections);
    }

    // Silence non-windows unused warnings.
    #[cfg(not(windows))]
    {
        let _ = (sections, err_msg);
    }

    0
}

#[cfg(windows)]
fn register_seh_for_module(sections: &[Section]) {
    // Find the (single) .text base — every JIT'd Lisp module we
    // emit currently has exactly one .text section. We assume it
    // for simplicity; multi-text-section support means picking the
    // lowest address and asserting the RUNTIME_FUNCTION RVA range
    // fits within u32 from there.
    let Some(text) = sections.iter().find(|s| s.name_tag == NameTag::Text) else {
        return;
    };
    let base = text.ptr as u64;
    if text.size > u32::MAX as usize {
        eprintln!(
            "[jit_mm] .text larger than u32 RVA window ({} bytes); SEH \
             registration skipped",
            text.size
        );
        return;
    }

    // Each .pdata section gets its own registration. (Almost always
    // exactly one; we don't assume.) Windows requires the entries
    // be sorted ascending by BeginAddress — `RtlAddFunctionTable`
    // does NOT validate this, and the unwinder binary-searches the
    // table. Unsorted entries return the wrong RUNTIME_FUNCTION,
    // applying another function's UNWIND_INFO to the panicking
    // frame, corrupting RSP and tripping the next /GS canary
    // (STATUS_STACK_BUFFER_OVERRUN, 0xC0000409). We sort in place.
    for pdata in sections.iter().filter(|s| s.name_tag == NameTag::Pdata) {
        let raw_count =
            (pdata.size / std::mem::size_of::<win::RUNTIME_FUNCTION>()) as usize;
        if raw_count == 0 {
            continue;
        }

        // The COFF .pdata section the linker allocates for us is
        // often LARGER than the actually-populated entry count —
        // LLVM/RuntimeDyld pads it (e.g. to alignment, or just
        // over-reserves). The unused tail is zero-filled, which
        // means BeginAddress=EndAddress=0 RUNTIME_FUNCTIONs sit
        // next to the real ones. We must not hand those to
        // RtlAddFunctionTable: they all share BeginAddress=0, and
        // RtlLookupFunctionEntry's binary search can land on one
        // of them, see EndAddress=0, conclude "PC not in this
        // function," and skip the JIT frame's unwind — leaving
        // RSP unrestored. The next /GS canary check then fast-fails
        // (STATUS_STACK_BUFFER_OVERRUN, 0xC0000409).
        //
        // Pack the live entries (Begin<End) into the front of the
        // slice, sort, and register only those. Trailing zero
        // entries stay in memory but the registered table doesn't
        // include them.
        let entries = unsafe {
            std::slice::from_raw_parts_mut(
                pdata.ptr as *mut win::RUNTIME_FUNCTION,
                raw_count,
            )
        };
        // In-place partition: live entries to the front.
        let mut live = 0usize;
        for i in 0..raw_count {
            if entries[i].BeginAddress < entries[i].EndAddress {
                if i != live {
                    entries[live] = entries[i];
                }
                live += 1;
            }
        }
        // Zero out the tail so we never accidentally re-register it.
        for slot in &mut entries[live..raw_count] {
            *slot = win::RUNTIME_FUNCTION {
                BeginAddress: 0,
                EndAddress: 0,
                UnwindData: 0,
            };
        }
        if live == 0 {
            continue;
        }
        let live_entries = &mut entries[..live];
        live_entries.sort_by_key(|e| e.BeginAddress);

        // Sanity-check the smallest entry: BaseAddress + Begin must
        // land in .text.
        let first = &live_entries[0];
        let computed = base.wrapping_add(first.BeginAddress as u64);
        let text_lo = base;
        let text_hi = base + text.size as u64;
        if computed < text_lo || computed >= text_hi {
            eprintln!(
                "[jit_mm] SEH base-address sanity check failed: \
                 base={base:#x} first.BeginAddress={:#x} computed={computed:#x} \
                 text=[{text_lo:#x}, {text_hi:#x}). Skipping registration.",
                first.BeginAddress
            );
            continue;
        }

        let ok = unsafe {
            win::RtlAddFunctionTable(
                pdata.ptr as *const win::RUNTIME_FUNCTION,
                live as u32,
                base,
            )
        };
        if ok == 0 {
            eprintln!(
                "[jit_mm] RtlAddFunctionTable failed for {live} entries at \
                 base={base:#x}; panics through JIT frames will abort"
            );
        } else if std::env::var_os("NCL_TRACE_SEH").is_some() {
            let last = &live_entries[live - 1];
            eprintln!(
                "[jit_mm] registered {live} live SEH entries (of {raw_count} slots) \
                 at base={base:#x}; first=[{:#x},{:#x}) last=[{:#x},{:#x})",
                base + first.BeginAddress as u64,
                base + first.EndAddress as u64,
                base + last.BeginAddress as u64,
                base + last.EndAddress as u64,
            );
        }
    }
}

extern "C" fn destroy(opaque: *mut c_void) {
    // Match the existing keep_forever contract: leak. When module
    // retirement lands, this is the place to call
    // RtlDeleteFunctionTable for every registered .pdata and
    // VirtualFree every captured allocation. Until then the loader
    // never reaches a quiescent epoch, so dropping the JitMm would
    // leave dangling pointers in the OS unwinder.
    let _ = unsafe { Box::from_raw(opaque as *mut JitMm) };
    // (Actually drop the Box so we don't accumulate JitMms forever
    // — the sections inside are leaked, but the bookkeeping isn't
    // needed once SEH is registered.)
}

// ── Public constructor: build the LLVM C-API MM ref ───────────────────

/// Construct an LLVM `LLVMMCJITMemoryManagerRef` that captures
/// `.pdata`/`.xdata`/`.text` and registers SEH tables on finalize.
/// The returned ref is consumed by `LLVMCreateMCJITCompilerForModule`
/// — it owns the ref and will call `destroy` at engine drop time.
///
/// MacNCL: off-Windows we return a **null** MM. The custom manager only
/// exists to register Windows SEH unwind tables (`RtlAddFunctionTable`)
/// and to flip pages executable via `VirtualProtect` on finalize —
/// neither of which applies on macOS, where Apple Silicon additionally
/// requires `MAP_JIT` / `pthread_jit_write_protect_np` handling that
/// LLVM's own default `SectionMemoryManager` already implements
/// correctly. A null `MCJMM` makes `LLVMCreateMCJITCompilerForModule`
/// fall back to that default manager. See docs/PORTING_DESIGN.md §3.4.
#[cfg(not(windows))]
pub unsafe fn make_mm() -> LLVMMCJITMemoryManagerRef {
    std::ptr::null_mut()
}

#[cfg(windows)]
pub unsafe fn make_mm() -> LLVMMCJITMemoryManagerRef {
    let mm = JitMm::new();
    let opaque = Box::into_raw(mm) as *mut c_void;
    // llvm-sys's signature: AllocateCode/Data/Finalize are bare
    // `extern "C" fn(...)` pointers; Destroy is `Option<extern "C"
    // fn(...)>` (presumably because it's the only one LLVM itself
    // treats as optional).
    unsafe {
        LLVMCreateSimpleMCJITMemoryManager(
            opaque,
            allocate_code_section,
            allocate_data_section,
            finalize_memory,
            Some(destroy),
        )
    }
}
