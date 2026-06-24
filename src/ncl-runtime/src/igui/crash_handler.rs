//! VEH-based crash handler for the NewCormanLisp worker thread.
//!
//! When the Lisp worker thread takes a Windows SEH exception (access
//! violation, illegal instruction, divide-by-zero, privileged
//! instruction), this handler:
//!
//!   1. Snapshots register state + RIP + 16 stack qwords into a
//!      pre-allocated static buffer.  No allocation, no Mutex.
//!   2. Rewrites the CONTEXT record's `Rip` to a tiny
//!      `crash_recovery_thunk` whose body is `ExitThread(2)`.
//!   3. Returns `EXCEPTION_CONTINUE_EXECUTION`.
//!
//! The OS resumes the worker at the thunk; the thread exits cleanly.
//! The supervisor (in ncl-driver's run_with_windows_surface) is
//! parked on the worker's `JoinHandle`; when it returns, the supervisor
//! calls `take_dump()`.  If populated, it formats the dump, pushes it
//! to the crash view, and leaves the frame alive so the user can read
//! the report.
//!
//! Ported from WF64's crash_handler.rs; the only NCL-specific change is
//! the end-of-dump message (no session reboot here — the Lisp session
//! is gone).

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO, EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_STACK_OVERFLOW,
};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_POINTERS, EXCEPTION_RECORD,
    EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
};
use windows::Win32::System::Threading::{ExitThread, GetCurrentThreadId};

/// Thread ID of the registered worker thread.  Set by
/// `register_worker_thread` from inside the worker right before
/// it starts running Lisp code.  VEH compares against this to
/// decide whether to intercept (we don't want to recover SEH on
/// the UI thread — UI-thread bugs should surface, not be retried).
static WORKER_TID: AtomicU32 = AtomicU32::new(0);

/// One-shot flag: `true` iff the VEH has filled CAPTURED.  Cleared
/// by `take_dump`.  SeqCst for clarity; the path is not hot.
static CAPTURED: AtomicBool = AtomicBool::new(false);

/// Lock-free capture buffer.  Written exactly once per crash
/// (by the VEH on the worker thread), read exactly once (by the
/// supervisor after the worker exits).
#[repr(C)]
pub struct CapturedDump {
    pub code: u32,
    pub flags: u32,
    pub rip: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub stack: [u64; 16],
    pub access_addr: u64,
    pub access_kind: u32,
    pub thread_id: u32,
}

impl CapturedDump {
    const fn zero() -> Self {
        Self {
            code: 0, flags: 0, rip: 0, rsp: 0, rbp: 0,
            rax: 0, rbx: 0, rcx: 0, rdx: 0, rsi: 0, rdi: 0,
            r8: 0, r9: 0, r10: 0, r11: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            stack: [0u64; 16],
            access_addr: 0,
            access_kind: 0,
            thread_id: 0,
        }
    }
}

/// Static capture buffer.  The `CAPTURED` AtomicBool serialises
/// the VEH writer vs the supervisor reader.
static mut CAPTURED_DUMP: CapturedDump = CapturedDump::zero();

/// Install the VEH.  Call once at process startup before the worker
/// thread starts.  Idempotent (subsequent calls no-op).
pub fn install() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = unsafe { AddVectoredExceptionHandler(1, Some(veh_callback)) };
    if handle.is_null() {
        eprintln!("[crash_handler] AddVectoredExceptionHandler failed");
    }
}

/// Worker thread calls this right after spawn so the VEH knows which
/// thread to intercept.
pub fn register_worker_thread() {
    let tid = unsafe { GetCurrentThreadId() };
    WORKER_TID.store(tid, Ordering::Release);
}

pub fn unregister_worker_thread() {
    WORKER_TID.store(0, Ordering::Release);
}

/// Returns a fresh copy of the captured dump and clears the flag.
/// Call from the supervisor after the worker's `JoinHandle::join`
/// returns.  `Some` → the worker died via a caught SEH.  `None` →
/// clean exit or Rust-panic-handled exit.
pub fn take_dump() -> Option<CapturedDump> {
    if !CAPTURED.swap(false, Ordering::Acquire) {
        return None;
    }
    // SAFETY: VEH is the only writer; AtomicBool ensures we read after.
    let dump = unsafe { std::ptr::read(&raw const CAPTURED_DUMP) };
    Some(dump)
}

/// Format the dump as a multi-line text block.  Runs on the
/// supervisor thread; allocates freely.
pub fn format_dump(d: &CapturedDump) -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(&format!("kind:           SEH exception (Lisp worker thread)\n"));
    s.push_str(&format!("exception code: {}  ({})\n", format_code(d.code), exception_name(d.code)));
    if d.code == EXCEPTION_ACCESS_VIOLATION.0 as u32 {
        let kind = match d.access_kind {
            0 => "read",
            1 => "write",
            8 => "execute",
            _ => "?",
        };
        s.push_str(&format!("access:         {kind} at {:#018x}\n", d.access_addr));
    }
    s.push_str(&format!("thread id:      {}\n", d.thread_id));
    s.push_str(&format!("\n"));
    s.push_str(&format!("rip = {:#018x}   flags = {:#010x}\n", d.rip, d.flags));
    s.push_str(&format!("rax = {:#018x}   rbx = {:#018x}\n", d.rax, d.rbx));
    s.push_str(&format!("rcx = {:#018x}   rdx = {:#018x}\n", d.rcx, d.rdx));
    s.push_str(&format!("rsi = {:#018x}   rdi = {:#018x}\n", d.rsi, d.rdi));
    s.push_str(&format!("rbp = {:#018x}   rsp = {:#018x}\n", d.rbp, d.rsp));
    s.push_str(&format!("r8  = {:#018x}   r9  = {:#018x}\n", d.r8, d.r9));
    s.push_str(&format!("r10 = {:#018x}   r11 = {:#018x}\n", d.r10, d.r11));
    s.push_str(&format!("r12 = {:#018x}   r13 = {:#018x}\n", d.r12, d.r13));
    s.push_str(&format!("r14 = {:#018x}   r15 = {:#018x}\n", d.r14, d.r15));
    s.push_str(&format!("\n"));
    s.push_str(&format!("stack (16 qwords from rsp):\n"));
    for (i, qw) in d.stack.iter().enumerate() {
        s.push_str(&format!("  [rsp+{:>3}] {:#018x}\n", i * 8, qw));
    }
    s.push_str(&format!("\n"));
    s.push_str(&format!("The Lisp worker thread has been terminated.  The session\n"));
    s.push_str(&format!("is no longer available — close the window and restart.\n"));
    s
}

fn format_code(c: u32) -> String {
    format!("0x{c:08x}")
}

fn exception_name(c: u32) -> &'static str {
    match c {
        x if x == EXCEPTION_ACCESS_VIOLATION.0 as u32     => "ACCESS_VIOLATION",
        x if x == EXCEPTION_ILLEGAL_INSTRUCTION.0 as u32  => "ILLEGAL_INSTRUCTION",
        x if x == EXCEPTION_INT_DIVIDE_BY_ZERO.0 as u32   => "INT_DIVIDE_BY_ZERO",
        x if x == EXCEPTION_PRIV_INSTRUCTION.0 as u32     => "PRIV_INSTRUCTION",
        x if x == EXCEPTION_STACK_OVERFLOW.0 as u32       => "STACK_OVERFLOW",
        _                                                  => "unknown SEH code",
    }
}

/// VEH callback.  Runs in the context of the faulting thread.
/// Minimal: one mut-static write, two atomic stores, one CONTEXT
/// field mutation — no allocation, no I/O, no Mutex.
unsafe extern "system" fn veh_callback(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let ep: &EXCEPTION_POINTERS = unsafe { &*info };
    let er_ptr: *const EXCEPTION_RECORD = ep.ExceptionRecord;
    if er_ptr.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let er: &EXCEPTION_RECORD = unsafe { &*er_ptr };
    let code = er.ExceptionCode.0 as u32;

    // Only handle fatal SEH codes we want to recover from.
    let recoverable = matches!(
        code,
        x if x == EXCEPTION_ACCESS_VIOLATION.0 as u32
          || x == EXCEPTION_ILLEGAL_INSTRUCTION.0 as u32
          || x == EXCEPTION_INT_DIVIDE_BY_ZERO.0 as u32
          || x == EXCEPTION_PRIV_INSTRUCTION.0 as u32
    );
    if !recoverable {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    // Only intercept on the registered worker thread.
    let current_tid = unsafe { GetCurrentThreadId() };
    let worker_tid = WORKER_TID.load(Ordering::Acquire);
    if worker_tid == 0 || current_tid != worker_tid {
        return EXCEPTION_CONTINUE_SEARCH;
    }

    let ctx_ptr: *mut CONTEXT = ep.ContextRecord;
    if ctx_ptr.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let ctx: &mut CONTEXT = unsafe { &mut *ctx_ptr };

    // Capture into the static buffer.
    unsafe {
        let d = &mut *(&raw mut CAPTURED_DUMP);
        d.code  = code;
        d.flags = er.ExceptionFlags;
        d.rip   = ctx.Rip;
        d.rsp   = ctx.Rsp;
        d.rbp   = ctx.Rbp;
        d.rax   = ctx.Rax;
        d.rbx   = ctx.Rbx;
        d.rcx   = ctx.Rcx;
        d.rdx   = ctx.Rdx;
        d.rsi   = ctx.Rsi;
        d.rdi   = ctx.Rdi;
        d.r8    = ctx.R8;
        d.r9    = ctx.R9;
        d.r10   = ctx.R10;
        d.r11   = ctx.R11;
        d.r12   = ctx.R12;
        d.r13   = ctx.R13;
        d.r14   = ctx.R14;
        d.r15   = ctx.R15;
        d.thread_id = current_tid;
        if code == EXCEPTION_ACCESS_VIOLATION.0 as u32 && er.NumberParameters >= 2 {
            d.access_kind = er.ExceptionInformation[0] as u32;
            d.access_addr = er.ExceptionInformation[1] as u64;
        } else {
            d.access_kind = 0;
            d.access_addr = 0;
        }
        copy_stack_safely(ctx.Rsp as *const u64, &mut d.stack);
    }

    CAPTURED.store(true, Ordering::Release);

    // Redirect resumption to the thunk.
    ctx.Rip = crash_recovery_thunk as usize as u64;

    EXCEPTION_CONTINUE_EXECUTION
}

/// Safe stack copy via ReadProcessMemory so a bogus RSP doesn't AV
/// us recursively.
unsafe fn copy_stack_safely(src: *const u64, dst: &mut [u64; 16]) {
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::GetCurrentProcess;
    let proc = unsafe { GetCurrentProcess() };
    let mut bytes_read: usize = 0;
    let dst_bytes = unsafe {
        std::slice::from_raw_parts_mut(
            dst.as_mut_ptr() as *mut u8,
            std::mem::size_of_val(dst),
        )
    };
    let _ = unsafe {
        ReadProcessMemory(
            proc,
            src as *const _,
            dst_bytes.as_mut_ptr() as *mut _,
            dst_bytes.len(),
            Some(&mut bytes_read),
        )
    };
}

/// CPU resumes here after the VEH rewrites RIP.  Exits the thread
/// cleanly so the supervisor's `JoinHandle::join` unblocks.
unsafe extern "system" fn crash_recovery_thunk() -> ! {
    unsafe { ExitThread(2) };
}
