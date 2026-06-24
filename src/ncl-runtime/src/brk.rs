//! Crash-reporting handler for NewCormanLisp.
//!
//! Installed once at process startup via [`install_crash_handler`].
//! When a Windows structured exception (access violation, stack
//! overflow, illegal instruction, …) propagates past every other
//! handler, the filter writes a structured BRK-style report to stderr
//! covering the exception code + faulting address, the captured
//! register state, and a stack walk via `RtlVirtualUnwind`.
//!
//! Ported from `E:\NewBCPL\src\newbcpl-runtime\src\brk.rs` and
//! `E:\NewFB\src\newfb-runtime\src\brk.rs`. The format is intentionally
//! identical so a developer who knows one knows the other.
//!
//! ### Safety contract
//!
//! The handler can fire when the process is in any state — including
//! a half-collected heap or a partially-rewritten object graph. It
//! must not make things worse:
//!
//! * **No `format!` / `println!`.** Numbers are formatted by hand
//!   into stack buffers.
//! * **No Rust heap allocation.** Every buffer is a fixed-size stack
//!   array.
//! * **Direct WriteFile** on `STD_ERROR_HANDLE` — no stdio locking,
//!   no UTF-8 validation, just bytes to the OS handle.
//! * **Best-effort stack walk.** Any unwind step that fails just
//!   terminates the walk; we never retry or recurse.
//! * **Never re-enters.** The filter returns
//!   `EXCEPTION_EXECUTE_HANDLER` so the OS terminates the process
//!   afterwards.
//!
//! Non-Windows hosts get a no-op shim so callers don't have to
//! `cfg`-gate every call site.

use std::sync::RwLock;
#[cfg(windows)]
use std::sync::Once;

/// Install a process-wide last-resort exception filter. Idempotent
/// across repeated calls; only the first call wires the handler.
///
/// Should be called early in `main()` before any work that could
/// fault.
#[cfg(windows)]
pub fn install_crash_handler() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        unsafe {
            win::SetUnhandledExceptionFilter(Some(unhandled_exception_filter));
        }
    });
}

/// Non-Windows shim: NCL runs on Windows for now, but the
/// no-op lets cross-platform builds compile.
#[cfg(not(windows))]
pub fn install_crash_handler() {}

// ─── JIT symbol registry ────────────────────────────────────────────
//
// Populated by `ncl-llvm` after each `LLVMGetFunctionAddress`. The
// crash handler's stack walk consults this to translate JIT'd RIPs
// back to Lisp function names. Without it, stack traces are raw
// hex addresses that take a `dumpbin` round-trip to interpret.

static JIT_SYMBOLS: RwLock<Vec<(u64, String)>> = RwLock::new(Vec::new());

/// Register a JIT-emitted function for stack-trace resolution.
/// Called from `ncl-llvm` after `LLVMGetFunctionAddress` returns a
/// stable address. The registry stays sorted by `start_addr` so
/// lookups can binary-search.
///
/// `name` is borrowed for the duration of the call; the registry
/// copies the bytes into an owned `String`.
pub fn register_jit_symbol(start_addr: u64, name: &str) {
    let mut guard = match JIT_SYMBOLS.write() {
        Ok(g) => g,
        Err(_) => return, // poisoned; quietly skip
    };
    let pos = guard.partition_point(|(s, _)| *s < start_addr);
    guard.insert(pos, (start_addr, name.to_string()));
}

/// Reasonable upper bound on a JIT'd Lisp function's machine-code
/// size. Any RIP that sits more than this far above its nearest
/// registered start address is almost certainly host / OS code, not
/// JIT code, and gets reported as un-named so we don't mis-attribute
/// it to the highest-address JIT function.
const MAX_REASONABLE_JIT_ROUTINE_SIZE: u64 = 1024 * 1024;

fn lookup_jit_symbol(rip: u64) -> Option<String> {
    let guard = JIT_SYMBOLS.read().ok()?;
    if guard.is_empty() {
        return None;
    }
    let after = guard.partition_point(|(s, _)| *s <= rip);
    if after == 0 {
        return None;
    }
    let (start, name) = &guard[after - 1];
    if rip.saturating_sub(*start) > MAX_REASONABLE_JIT_ROUTINE_SIZE {
        return None;
    }
    Some(name.clone())
}

// ─── GC coordinator handle for crash-time state dump ───────────────
//
// Set once from `GcCoordinator::new`. The crash handler reads from
// this to emit a post-mortem section listing the GC's cumulative
// stats and the heap's per-generation page counts. The process is
// already dying, so we lock the heap mutex outright — a deadlock is
// preferable to skipping the dump silently.

static GC_COORD: RwLock<Option<std::sync::Arc<crate::mutator::GcCoordinator>>> =
    RwLock::new(None);

/// Register the process-wide GC coordinator with the crash handler.
/// Called once from `GcCoordinator::new` so the SEH filter can dump
/// GC + heap state on a fatal exception.
pub fn install_gc_coordinator(coord: std::sync::Arc<crate::mutator::GcCoordinator>) {
    if let Ok(mut g) = GC_COORD.write() {
        *g = Some(coord);
    }
}

/// The process-wide GC coordinator, if one has been installed. Used by the
/// JIT (at compile time) to reach the static area for compile-time constant
/// allocation — e.g. eliding a no-capture closure's per-evaluation alloc.
/// `None` in unit-test contexts that compile without a running coordinator.
pub fn gc_coordinator() -> Option<std::sync::Arc<crate::mutator::GcCoordinator>> {
    GC_COORD.read().ok().and_then(|g| g.clone())
}

// ─── Win32 FFI ──────────────────────────────────────────────────────
//
// Hand-rolled rather than going through the `windows` crate so the
// crash handler doesn't pull in extra feature flags / compile time
// for `Win32_System_Diagnostics_Debug`. The structures are documented
// in MSDN under `WinNT.h` and `errhandlingapi.h`.

#[cfg(windows)]
mod win {
    use core::ffi::c_void;

    pub type HANDLE = *mut c_void;
    pub const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4; // -12 as u32
    pub const INVALID_HANDLE_VALUE: HANDLE = usize::MAX as HANDLE;

    /// AMD64 `CONTEXT`. The full record is ~1232 bytes; we pad the
    /// trailing bytes since `RtlCaptureContext` writes the whole
    /// thing. Alignment must be 16 (the embedded XMM block requires
    /// it, otherwise `RtlCaptureContext` faults).
    #[repr(C, align(16))]
    pub struct Context {
        pub p1_home:       u64,
        pub p2_home:       u64,
        pub p3_home:       u64,
        pub p4_home:       u64,
        pub p5_home:       u64,
        pub p6_home:       u64,
        pub context_flags: u32,
        pub mx_csr:        u32,
        pub seg_cs:        u16,
        pub seg_ds:        u16,
        pub seg_es:        u16,
        pub seg_fs:        u16,
        pub seg_gs:        u16,
        pub seg_ss:        u16,
        pub eflags:        u32,
        pub dr0:           u64,
        pub dr1:           u64,
        pub dr2:           u64,
        pub dr3:           u64,
        pub dr6:           u64,
        pub dr7:           u64,
        pub rax:           u64,
        pub rcx:           u64,
        pub rdx:           u64,
        pub rbx:           u64,
        pub rsp:           u64,
        pub rbp:           u64,
        pub rsi:           u64,
        pub rdi:           u64,
        pub r8:            u64,
        pub r9:            u64,
        pub r10:           u64,
        pub r11:           u64,
        pub r12:           u64,
        pub r13:           u64,
        pub r14:           u64,
        pub r15:           u64,
        pub rip:           u64,
        // FLOATING_SAVE_AREA + 256 bytes of XMM regs + debug
        // registers + segment fill. We never read these, but the
        // OS writes through the pointer so we MUST allocate the
        // full size.
        pub _trailing: [u8; 1232 - 0x100],
    }

    pub const CONTEXT_ALL_AMD64: u32 = 0x10003F;
    pub const UNW_FLAG_NHANDLER: u32 = 0;

    /// Opaque to us; `RtlVirtualUnwind` reads / writes through the
    /// pointer. 600 bytes is the documented size on x86-64.
    #[repr(C)]
    pub struct UnwindHistoryTable {
        _opaque: [u8; 600],
    }

    impl UnwindHistoryTable {
        pub fn zeroed() -> Self { Self { _opaque: [0; 600] } }
    }

    /// Win32 `EXCEPTION_RECORD`. 15-parameter form (we read at most
    /// `exception_info[0..2]`, but the OS writes the full thing).
    #[repr(C)]
    pub struct ExceptionRecord {
        pub exception_code:    u32,
        pub exception_flags:   u32,
        pub exception_record:  *mut ExceptionRecord,
        pub exception_address: *mut c_void,
        pub number_parameters: u32,
        _pad:                  u32,
        pub exception_info:    [u64; 15],
    }

    #[repr(C)]
    pub struct ExceptionPointers {
        pub exception_record: *mut ExceptionRecord,
        pub context_record:   *mut Context,
    }

    /// Filter return values. We return `EXCEPTION_EXECUTE_HANDLER`
    /// (1) after dumping so the OS terminates the process.
    pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

    pub const EXCEPTION_ACCESS_VIOLATION:      u32 = 0xC0000005;
    pub const EXCEPTION_STACK_OVERFLOW:        u32 = 0xC00000FD;
    pub const EXCEPTION_ILLEGAL_INSTRUCTION:   u32 = 0xC000001D;
    pub const EXCEPTION_INT_DIVIDE_BY_ZERO:    u32 = 0xC0000094;
    pub const EXCEPTION_FLT_DIVIDE_BY_ZERO:    u32 = 0xC000008E;
    pub const EXCEPTION_PRIV_INSTRUCTION:      u32 = 0xC0000096;
    pub const EXCEPTION_FASTFAIL:              u32 = 0xC0000409;
    pub const EXCEPTION_BREAKPOINT:            u32 = 0x80000003;

    pub type TopLevelExceptionFilter = unsafe extern "system" fn(
        info: *mut ExceptionPointers,
    ) -> i32;

    unsafe extern "system" {
        pub fn GetStdHandle(nStdHandle: u32) -> HANDLE;
        pub fn WriteFile(
            hFile: HANDLE,
            lpBuffer: *const u8,
            nNumberOfBytesToWrite: u32,
            lpNumberOfBytesWritten: *mut u32,
            lpOverlapped: *mut c_void,
        ) -> i32;
        pub fn RtlCaptureContext(ctx: *mut Context);
        pub fn RtlLookupFunctionEntry(
            ControlPc:    u64,
            ImageBase:    *mut u64,
            HistoryTable: *mut UnwindHistoryTable,
        ) -> *mut c_void;
        pub fn RtlVirtualUnwind(
            HandlerType:      u32,
            ImageBase:        u64,
            ControlPc:        u64,
            FunctionEntry:    *mut c_void,
            ContextRecord:    *mut Context,
            HandlerData:      *mut *mut c_void,
            EstablisherFrame: *mut u64,
            ContextPointers:  *mut c_void,
        ) -> *mut c_void;
        pub fn SetUnhandledExceptionFilter(
            filter: Option<TopLevelExceptionFilter>,
        ) -> Option<TopLevelExceptionFilter>;
    }
}

// ─── Alloc-free writer ──────────────────────────────────────────────

#[cfg(windows)]
const BRK_BUFFER_BYTES: usize = 4096;

#[cfg(windows)]
struct BrkWriter {
    buf:    [u8; BRK_BUFFER_BYTES],
    pos:    usize,
    handle: win::HANDLE,
}

#[cfg(windows)]
impl BrkWriter {
    fn new() -> Self {
        let handle = unsafe { win::GetStdHandle(win::STD_ERROR_HANDLE) };
        Self { buf: [0; BRK_BUFFER_BYTES], pos: 0, handle }
    }

    fn flush(&mut self) {
        if self.pos == 0
            || self.handle.is_null()
            || self.handle == win::INVALID_HANDLE_VALUE
        {
            self.pos = 0;
            return;
        }
        let mut written: u32 = 0;
        let _ = unsafe {
            win::WriteFile(
                self.handle,
                self.buf.as_ptr(),
                self.pos as u32,
                &mut written,
                core::ptr::null_mut(),
            )
        };
        self.pos = 0;
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let mut start = 0;
        while start < bytes.len() {
            let space = BRK_BUFFER_BYTES - self.pos;
            let take = (bytes.len() - start).min(space);
            self.buf[self.pos..self.pos + take]
                .copy_from_slice(&bytes[start..start + take]);
            self.pos += take;
            start += take;
            if self.pos == BRK_BUFFER_BYTES {
                self.flush();
            }
        }
    }

    fn write_str(&mut self, s: &str) {
        self.write_bytes(s.as_bytes());
    }

    /// Write `n` as a fixed-width 16-hex-digit unsigned hex number
    /// (no `0x` prefix, capitals). Lines up nicely under one another
    /// in a register dump or stack walk.
    fn write_hex16(&mut self, n: u64) {
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut tmp = [0u8; 16];
        for i in 0..16 {
            let shift = (15 - i) * 4;
            tmp[i] = HEX[((n >> shift) & 0xF) as usize];
        }
        self.write_bytes(&tmp);
    }

    fn write_dec_u64(&mut self, n: u64) {
        let mut tmp = [0u8; 24];
        let mut len = 0;
        let mut v = n;
        if v == 0 {
            tmp[len] = b'0';
            len += 1;
        } else {
            while v > 0 {
                tmp[len] = b'0' + (v % 10) as u8;
                len += 1;
                v /= 10;
            }
        }
        tmp[..len].reverse();
        self.write_bytes(&tmp[..len]);
    }
}

// ─── Crash filter ───────────────────────────────────────────────────

#[cfg(windows)]
unsafe extern "system" fn unhandled_exception_filter(
    info: *mut win::ExceptionPointers,
) -> i32 {
    // The filter runs on the faulting thread with the original stack
    // still live. Validate `info` and the pointers it carries; if any
    // are bad, defer to the OS default handler.
    if info.is_null() {
        return win::EXCEPTION_EXECUTE_HANDLER;
    }
    let ep = unsafe { &*info };
    if ep.exception_record.is_null() || ep.context_record.is_null() {
        return win::EXCEPTION_EXECUTE_HANDLER;
    }
    let record = unsafe { &*ep.exception_record };
    // Copy the context so the stack-walk's mutation doesn't disturb
    // the OS's view of the fault. `read` is a memcpy — safe on any
    // properly-aligned 16-byte-aligned source, which a CONTEXT
    // record is.
    let ctx_copy = unsafe { core::ptr::read(ep.context_record) };

    unsafe { crash_impl(record, &ctx_copy) };

    win::EXCEPTION_EXECUTE_HANDLER
}

#[cfg(windows)]
unsafe fn crash_impl(record: &win::ExceptionRecord, ctx: &win::Context) {
    let mut w = BrkWriter::new();

    // Banner — exception code + name + faulting address + faulting
    // RIP. The bar text matches NewBCPL/NewFB's `=== CRASH:` so log
    // readers can grep across projects.
    w.write_str("\n=== CRASH: ");
    let code = record.exception_code;
    w.write_str(exception_name(code));
    w.write_str("  code=0x");
    w.write_hex16(code as u64);
    w.write_str("  at=0x");
    w.write_hex16(record.exception_address as u64);
    if let Some(name) = lookup_jit_symbol(record.exception_address as u64) {
        w.write_str("  in ");
        w.write_bytes(name.as_bytes());
    }
    w.write_str(" ===\n");

    // ACCESS_VIOLATION carries two extra `exception_info` words:
    //   [0] = 0 (read) / 1 (write) / 8 (DEP)
    //   [1] = inaccessible address
    if code == win::EXCEPTION_ACCESS_VIOLATION
        && record.number_parameters >= 2
    {
        let kind = match record.exception_info[0] {
            0 => "read",
            1 => "write",
            8 => "DEP",
            _ => "?",
        };
        w.write_str("  ");
        w.write_str(kind);
        w.write_str(" at 0x");
        w.write_hex16(record.exception_info[1]);
        w.write_str("\n");
    }
    w.flush();

    write_context(&mut w, ctx);
    w.flush();
    unsafe { write_stack_walk_from(&mut w, core::ptr::read(ctx)) };
    w.flush();
    write_gc_state(&mut w);
    w.flush();

    w.write_str("=== END CRASH ===\n\n");
    w.flush();
}

/// Dump the GC coordinator's cumulative stats and the heap's per-
/// generation page counts. The process is already terminating, so we
/// lock the heap mutex outright — a deadlock is preferable to a
/// silent partial dump.
#[cfg(windows)]
fn write_gc_state(w: &mut BrkWriter) {
    let guard = match GC_COORD.read() {
        Ok(g) => g,
        Err(_) => {
            w.write_str("gc-state: <coordinator lock poisoned>\n");
            return;
        }
    };
    let coord = match guard.as_ref() {
        Some(c) => c,
        None => {
            w.write_str("gc-state: <coordinator not installed>\n");
            return;
        }
    };

    use core::sync::atomic::Ordering;
    let s = &coord.stats;

    w.write_str("gc-stats:\n");
    w.write_str("  minor-gcs              = ");
    w.write_dec_u64(s.minor_gcs.load(Ordering::Relaxed));
    w.write_str("\n  full-gcs               = ");
    w.write_dec_u64(s.full_gcs.load(Ordering::Relaxed));
    w.write_str("\n  bytes-promoted-total   = ");
    w.write_dec_u64(s.bytes_promoted_total.load(Ordering::Relaxed));
    w.write_str("\n  objects-pinned-total   = ");
    w.write_dec_u64(s.objects_pinned_total.load(Ordering::Relaxed));
    w.write_str("\n  pinned-residual-cells  = ");
    w.write_dec_u64(s.pinned_residual_cells.load(Ordering::Relaxed));
    w.write_str("\n  peak-young-bytes       = ");
    w.write_dec_u64(s.peak_young_used_bytes.load(Ordering::Relaxed));
    w.write_str("\n  last-minor-pause-us    = ");
    w.write_dec_u64(s.last_minor_pause_us.load(Ordering::Relaxed));
    w.write_str("\n  max-minor-pause-us     = ");
    w.write_dec_u64(s.max_minor_pause_us.load(Ordering::Relaxed));
    w.write_str("\n  total-minor-pause-us   = ");
    w.write_dec_u64(s.total_minor_pause_us.load(Ordering::Relaxed));
    w.write_str("\n  last-full-pause-us     = ");
    w.write_dec_u64(s.last_full_pause_us.load(Ordering::Relaxed));
    w.write_str("\n");

    // Static area (atomics, no lock).
    let static_area = coord.static_area();
    w.write_str("static-area:\n  used-bytes      = ");
    w.write_dec_u64(static_area.used_cells() as u64 * 8);
    w.write_str("\n  committed-bytes = ");
    w.write_dec_u64(static_area.committed_bytes() as u64);
    w.write_str("\n");

    // Heap state — lock outright. If we deadlock here the process is
    // already dying; the user can kill it manually.
    write_heap_state(w, coord);
}

#[cfg(all(windows, feature = "gc-page-heap"))]
fn write_heap_state(
    w: &mut BrkWriter,
    coord: &crate::mutator::GcCoordinator,
) {
    use newgc_core::Generation;
    // newgc-core owns the heap mutex internally; `with_heap` locks it
    // for the duration of the closure. The process is already dying,
    // so blocking here is acceptable (a deadlock is preferable to a
    // silent partial dump — same rationale as the old try_lock path,
    // minus the contended-skip branch).
    coord.with_heap(|heap| {
        w.write_str("heap (page-heap):\n");
        w.write_str("  total-pages = ");
        w.write_dec_u64(heap.page_count() as u64);
        w.write_str("\n  free        = ");
        w.write_dec_u64(heap.count_pages_in_gen(Generation::Free) as u64);
        w.write_str("\n  g0          = ");
        w.write_dec_u64(heap.count_pages_in_gen(Generation::G0) as u64);
        w.write_str("\n  g1          = ");
        w.write_dec_u64(heap.count_pages_in_gen(Generation::G1) as u64);
        w.write_str("\n  tenured     = ");
        w.write_dec_u64(heap.count_pages_in_gen(Generation::Tenured) as u64);
        w.write_str("\n  last-mark-live-bytes        = ");
        w.write_dec_u64(heap.last_mark_live_bytes() as u64);
        w.write_str("\n  last-mark-live-pages        = ");
        w.write_dec_u64(heap.last_mark_live_pages() as u64);
        w.write_str("\n  last-zero-live-pages-released = ");
        w.write_dec_u64(heap.last_zero_live_pages_released() as u64);
        let (pin_objs, pin_cells) = heap.last_pin_summary();
        w.write_str("\n  last-pin-objects  = ");
        w.write_dec_u64(pin_objs as u64);
        w.write_str("\n  last-pin-cells    = ");
        w.write_dec_u64(pin_cells as u64);
        w.write_str("\n  pinned-now        = ");
        w.write_dec_u64(heap.pinned_count() as u64);
        w.write_str("\n  minors-since-g0-promote = ");
        w.write_dec_u64(heap.minors_since_g0_promote() as u64);
        w.write_str("\n  g0-promotes-since-g1-promote = ");
        w.write_dec_u64(heap.g0_promotes_since_g1_promote() as u64);
        w.write_str("\n");
    });
}

#[cfg(all(windows, not(feature = "gc-page-heap")))]
fn write_heap_state(
    w: &mut BrkWriter,
    _coord: &crate::mutator::GcCoordinator,
) {
    w.write_str("heap (semispace): <dump not wired for semispace>\n");
}

#[cfg(windows)]
fn exception_name(code: u32) -> &'static str {
    match code {
        win::EXCEPTION_ACCESS_VIOLATION    => "ACCESS_VIOLATION",
        win::EXCEPTION_STACK_OVERFLOW      => "STACK_OVERFLOW",
        win::EXCEPTION_ILLEGAL_INSTRUCTION => "ILLEGAL_INSTRUCTION",
        win::EXCEPTION_INT_DIVIDE_BY_ZERO  => "INT_DIVIDE_BY_ZERO",
        win::EXCEPTION_FLT_DIVIDE_BY_ZERO  => "FLT_DIVIDE_BY_ZERO",
        win::EXCEPTION_PRIV_INSTRUCTION    => "PRIV_INSTRUCTION",
        win::EXCEPTION_FASTFAIL            => "FASTFAIL",
        win::EXCEPTION_BREAKPOINT          => "BREAKPOINT",
        _                                  => "EXCEPTION",
    }
}

#[cfg(windows)]
fn write_context(w: &mut BrkWriter, ctx: &win::Context) {
    w.write_str("context: rip=");
    w.write_hex16(ctx.rip);
    w.write_str("  rsp=");
    w.write_hex16(ctx.rsp);
    w.write_str("  rbp=");
    w.write_hex16(ctx.rbp);
    w.write_str("\n         rax=");
    w.write_hex16(ctx.rax);
    w.write_str("  rbx=");
    w.write_hex16(ctx.rbx);
    w.write_str("  rcx=");
    w.write_hex16(ctx.rcx);
    w.write_str("\n         rdx=");
    w.write_hex16(ctx.rdx);
    w.write_str("  rsi=");
    w.write_hex16(ctx.rsi);
    w.write_str("  rdi=");
    w.write_hex16(ctx.rdi);
    w.write_str("\n         r8 =");
    w.write_hex16(ctx.r8);
    w.write_str("  r9 =");
    w.write_hex16(ctx.r9);
    w.write_str("  r10=");
    w.write_hex16(ctx.r10);
    w.write_str("\n         r11=");
    w.write_hex16(ctx.r11);
    w.write_str("  r12=");
    w.write_hex16(ctx.r12);
    w.write_str("  r13=");
    w.write_hex16(ctx.r13);
    w.write_str("\n         r14=");
    w.write_hex16(ctx.r14);
    w.write_str("  r15=");
    w.write_hex16(ctx.r15);
    w.write_str("  flags=");
    w.write_hex16(ctx.eflags as u64);
    w.write_str("\n");
}

/// Walk the stack starting from the captured fault context.
///
/// Mutates a working copy of the context (caller already copied it
/// out of the OS's record). Each iteration consumes one frame:
///
/// * `RtlLookupFunctionEntry` finds the function table entry that
///   owns the current `rip`; null means a leaf frame with no unwind
///   data, so we pop the saved `rip` manually off `rsp` and try once
///   more.
/// * `RtlVirtualUnwind` advances `ctx` to the caller's frame.
/// * If `rip` and `rsp` are unchanged across an unwind step, we've
///   stopped making progress and bail.
///
/// All reads are best-effort: any failure terminates the walk. Caps
/// at 32 frames so a pathological loop in the unwind tables doesn't
/// spin forever.
#[cfg(windows)]
unsafe fn write_stack_walk_from(w: &mut BrkWriter, mut ctx: win::Context) {
    const MAX_FRAMES: usize = 32;

    w.write_str("stack:\n");
    let mut history = win::UnwindHistoryTable::zeroed();

    for frame_index in 0..MAX_FRAMES {
        let rip = ctx.rip;
        if rip == 0 {
            break;
        }

        w.write_str("  #");
        w.write_dec_u64(frame_index as u64);
        w.write_str("  rip=");
        w.write_hex16(rip);
        if let Some(name) = lookup_jit_symbol(rip) {
            w.write_str("  in ");
            w.write_bytes(name.as_bytes());
        }
        w.write_str("\n");

        let mut image_base: u64 = 0;
        let func_entry = unsafe {
            win::RtlLookupFunctionEntry(rip, &mut image_base, &mut history)
        };
        if func_entry.is_null() {
            // Leaf with no unwind data. Pop saved RIP manually off
            // RSP and try one more iteration. A failure here ends
            // the walk.
            let saved_rip_ptr = ctx.rsp as *const u64;
            if saved_rip_ptr.is_null() {
                break;
            }
            let new_rip =
                unsafe { core::ptr::read_volatile(saved_rip_ptr) };
            if new_rip == 0 || new_rip == ctx.rip {
                break;
            }
            ctx.rip = new_rip;
            ctx.rsp = ctx.rsp.wrapping_add(8);
            continue;
        }

        let prev_rip = ctx.rip;
        let prev_rsp = ctx.rsp;
        let mut handler_data: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut establisher_frame: u64 = 0;
        let _handler = unsafe {
            win::RtlVirtualUnwind(
                win::UNW_FLAG_NHANDLER,
                image_base,
                rip,
                func_entry,
                &mut ctx,
                &mut handler_data,
                &mut establisher_frame,
                core::ptr::null_mut(),
            )
        };
        if ctx.rip == prev_rip && ctx.rsp == prev_rsp {
            break;
        }
    }
}
