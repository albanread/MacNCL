//! Windows surface bootstrap. Phases 1 and 2 of `docs/WINDOWS_FFI.md`.
//!
//! The driver decides at startup whether the Windows surface is on
//! (the `--windows` flag). If it is:
//!
//!   * thread 0 (the process main) becomes the UI thread; it creates
//!     a hidden `HWND_MESSAGE` window and runs a Win32 message pump
//!     via `run_message_pump`
//!   * a worker thread runs the Lisp eval / REPL — what `main()`
//!     does today
//!   * `(windows-enabled-p)` returns T from Lisp, opening up the
//!     conditional `(when (windows-enabled-p) (require 'win32-threading))`
//!     branch in init.lisp
//!   * `(on-ui-thread BODY)` from Lisp marshals a closure to thread 0
//!     via `SendMessageW(hwnd, WM_NCL_EXECUTE, …)`, blocks until it
//!     returns, propagates the result back
//!
//! If it isn't:
//!
//!   * thread 0 runs Lisp directly (today's behaviour)
//!   * `(windows-enabled-p)` returns NIL
//!   * the UI-thread predicates return NIL
//!   * `(on-ui-thread BODY)` errors with "Windows surface not enabled"
//!
//! All state here is process-global `OnceLock`. The driver sets the
//! flag once, very early, before any worker can race; everything
//! else reads.
//!
//! Phase 3 will layer the libffi-based `%ffi-call` (with a
//! `WM_NCL_FFI_CALL` arm) on top of the same dispatcher.

use std::cell::Cell;
use std::sync::{Arc, OnceLock};

use crate::mutator::{GcCoordinator, MutatorState};
use crate::word::Word;

#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
#[cfg(windows)]
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
#[cfg(windows)]
use windows::Win32::System::Threading::GetCurrentThreadId;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostThreadMessageW,
    RegisterClassExW, SendMessageW, TranslateMessage, HMENU, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_QUIT, WM_USER, WNDCLASSEXW,
};

// ─── Global flags ─────────────────────────────────────────────────────

/// True iff the driver was started with `--windows`. Set once by
/// `set_windows_enabled` before the Lisp worker boots. Defaults to
/// false (i.e. unset OnceLock reads as false) so embedders and tests
/// that don't go through the driver get the conservative answer.
static WINDOWS_ENABLED: OnceLock<bool> = OnceLock::new();

/// The OS thread ID of the UI thread (thread 0 in `--windows` mode).
/// Set once by `register_ui_thread` from thread 0 itself. None
/// outside `--windows` mode.
static UI_THREAD_ID: OnceLock<u32> = OnceLock::new();

/// The hidden dispatcher HWND. Created on thread 0 by
/// `init_ui_dispatch` after `register_ui_thread`. Receives
/// `WM_NCL_EXECUTE` (Phase 2) and `WM_NCL_FFI_CALL` (Phase 3).
#[cfg(windows)]
static UI_DISPATCH_HWND: OnceLock<UiHwndPtr> = OnceLock::new();

/// The Lisp worker's coordinator, published so thread 0 can register
/// itself as a mutator on the same heap. The worker calls
/// `publish_coordinator` immediately after creating its Session and
/// before any `(on-ui-thread …)` could possibly run.
static GC_COORDINATOR: OnceLock<Arc<GcCoordinator>> = OnceLock::new();

/// Sendable wrapper for HWND so it can live in a OnceLock<T> where
/// T: Send + Sync. HWND is a raw pointer (`*mut c_void`); the
/// underlying window object is owned by Windows itself and is
/// thread-affine via the message pump, not via Rust borrowing rules.
#[cfg(windows)]
#[derive(Clone, Copy)]
struct UiHwndPtr(pub HWND);
#[cfg(windows)]
unsafe impl Send for UiHwndPtr {}
#[cfg(windows)]
unsafe impl Sync for UiHwndPtr {}

/// Idempotently set the "Windows surface enabled" flag. Called once
/// by the driver, before any worker thread is spawned. Subsequent
/// calls are silently ignored (OnceLock semantics).
pub fn set_windows_enabled(enabled: bool) {
    let _ = WINDOWS_ENABLED.set(enabled);
}

/// True iff the Windows surface is active for this process.
pub fn windows_enabled() -> bool {
    WINDOWS_ENABLED.get().copied().unwrap_or(false)
}

/// Worker thread publishes its GcCoordinator so thread 0 can register
/// itself as a mutator and run Lisp funcalls in `WM_NCL_EXECUTE`
/// handlers. Idempotent; first call wins.
pub fn publish_coordinator(coord: Arc<GcCoordinator>) {
    let _ = GC_COORDINATOR.set(coord);
}

// ─── UI thread identity + dispatch HWND ───────────────────────────────

#[cfg(windows)]
mod platform {
    use super::*;

    /// Private window messages. WM_USER = 0x0400; we pick offsets
    /// well above iGui's WM_USER+1..+8 to keep ranges disjoint if
    /// iGui's frame ever happens to handle these too.
    pub(super) const WM_NCL_EXECUTE: u32 = WM_USER + 99;
    #[allow(dead_code)] // Phase 3 will populate
    pub(super) const WM_NCL_FFI_CALL: u32 = WM_USER + 100;

    /// Record the calling thread as the UI (dispatcher) thread.
    /// Driver-only; called from thread 0 immediately after deciding
    /// `--windows` is on, before spawning the Lisp worker.
    pub fn register_ui_thread() {
        let tid = unsafe { GetCurrentThreadId() };
        let _ = UI_THREAD_ID.set(tid);
    }

    /// The OS thread ID of the UI thread, or None if not registered.
    pub fn ui_thread_id() -> Option<u32> {
        UI_THREAD_ID.get().copied()
    }

    /// True iff the calling thread IS the UI thread. False if no UI
    /// thread is registered (i.e. `--windows` wasn't passed).
    pub fn am_i_ui_thread() -> bool {
        match ui_thread_id() {
            Some(want) => (unsafe { GetCurrentThreadId() }) == want,
            None => false,
        }
    }

    /// Create the hidden HWND_MESSAGE dispatch window. Called from
    /// thread 0 by the driver after `register_ui_thread`. The window
    /// is never displayed; it exists only as a target for
    /// `SendMessageW(WM_NCL_EXECUTE, …)` from worker threads.
    pub fn init_ui_dispatch() {
        if UI_DISPATCH_HWND.get().is_some() {
            return;
        }
        // UTF-16 NUL-terminated class name.
        let class_name: Vec<u16> = "ncl_ui_dispatch\0".encode_utf16().collect();
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()).unwrap_or_default() };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dispatch_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = unsafe { RegisterClassExW(&wc) };
        if atom == 0 {
            // Class already registered (idempotency in tests that
            // re-run init) — keep going, CreateWindowExW will reuse it.
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0, 0, 0, 0,
                Some(HWND_MESSAGE),
                Option::<HMENU>::None,
                Some(hinstance.into()),
                None,
            )
        };
        let hwnd = hwnd.expect("CreateWindowExW(HWND_MESSAGE) failed");
        let _ = UI_DISPATCH_HWND.set(UiHwndPtr(hwnd));
    }

    /// Return the dispatch HWND, or None if not initialised.
    pub(super) fn dispatch_hwnd() -> Option<HWND> {
        UI_DISPATCH_HWND.get().map(|p| p.0)
    }

    /// Run the Win32 message pump on the calling thread. Blocks
    /// until a `WM_QUIT` arrives (typically posted by the worker
    /// thread via `post_quit_to_ui_thread` when Lisp finishes).
    ///
    /// `GetMessageW(hWnd=NULL)` retrieves thread messages too (those
    /// posted by `PostThreadMessageW`), so it picks up both window
    /// messages directed at the dispatch HWND and the WM_QUIT we
    /// post via `PostThreadMessageW`.
    pub fn run_message_pump() {
        let mut msg = MSG::default();
        unsafe {
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 <= 0 {
                    // 0 = WM_QUIT, -1 = error. Either way we're done.
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Post WM_QUIT to the UI thread, unblocking its message pump.
    /// Worker thread calls this when it's done with all Lisp work.
    /// No-op if no UI thread is registered.
    pub fn post_quit_to_ui_thread() {
        if let Some(tid) = ui_thread_id() {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Send an ExecuteRequest to the UI thread. Called by the
    /// `%ui-execute` shim on the worker thread. Blocks until the
    /// UI thread's WndProc handler returns.
    pub(super) fn send_execute(req: &mut ExecuteRequest) {
        let hwnd = dispatch_hwnd().expect(
            "send_execute: dispatch HWND not initialised (Windows surface off?)",
        );
        let lparam = LPARAM(req as *mut _ as isize);
        unsafe {
            // SendMessageW blocks until WndProc returns.
            let _ = SendMessageW(hwnd, WM_NCL_EXECUTE, Some(WPARAM(0)), Some(lparam));
        }
    }

    /// WndProc for the dispatch window. Handles `WM_NCL_EXECUTE`
    /// (Phase 2) and `WM_NCL_FFI_CALL` (Phase 3 — not yet
    /// populated). Everything else falls through to DefWindowProcW.
    unsafe extern "system" fn dispatch_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            m if m == WM_NCL_EXECUTE => {
                let req = unsafe { &mut *(lparam.0 as *mut ExecuteRequest) };
                handle_execute(req);
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    /// Run the closure carried by an ExecuteRequest on the UI
    /// thread. Catches panics so a runaway Lisp error doesn't kill
    /// the message pump; the panic message comes back to the worker
    /// as `out_error_word` (Phase 2 stub: returns NIL on panic).
    fn handle_execute(req: &mut ExecuteRequest) {
        let mutator = ensure_ui_mutator();
        let closure_word = req.closure_word;

        // catch_unwind so a Lisp panic doesn't bring down the pump.
        // We pass the closure with zero args (it's a thunk).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            crate::abi::ncl_funcall(mutator, closure_word, std::ptr::null(), 0)
        }));
        match result {
            Ok(w) => {
                req.out_result = w;
                req.out_error_word = Word::NIL.raw();
            }
            Err(_panic) => {
                // Phase 2: stash a NIL error. Phase 3+ can box the
                // panic payload into a condition Word.
                req.out_result = Word::NIL.raw();
                req.out_error_word = Word::T.raw();
            }
        }
    }
}

// On non-Windows platforms (we're Windows-only today, but the
// runtime crate still has to build on other hosts for tooling) the
// surface is a no-op.
#[cfg(not(windows))]
mod platform {
    use super::*;
    pub fn register_ui_thread() {}
    pub fn ui_thread_id() -> Option<u32> { None }
    pub fn am_i_ui_thread() -> bool { false }
    pub fn init_ui_dispatch() {}
    pub fn run_message_pump() {
        panic!("run_message_pump called on a non-Windows platform");
    }
    pub fn post_quit_to_ui_thread() {}
    pub(super) fn send_execute(_req: &mut ExecuteRequest) {
        panic!("send_execute called on a non-Windows platform");
    }
}

pub use platform::{
    am_i_ui_thread, init_ui_dispatch, post_quit_to_ui_thread, register_ui_thread,
    run_message_pump, ui_thread_id,
};

// ─── ExecuteRequest: closure dispatch ─────────────────────────────────

/// Payload for `WM_NCL_EXECUTE`. The worker fills `closure_word`,
/// the UI thread fills `out_result` / `out_error_word`. The struct
/// lives on the worker's stack while `SendMessageW` blocks, so no
/// allocation needed.
#[repr(C)]
pub struct ExecuteRequest {
    pub closure_word: u64,
    pub out_result: u64,
    pub out_error_word: u64,
}

// ─── UI thread mutator (lazy init) ────────────────────────────────────

thread_local! {
    /// The UI thread's own MutatorState pointer. Leaked Box, lives
    /// for the lifetime of the thread (i.e. process). Lazy-initialised
    /// the first time a `WM_NCL_EXECUTE` handler runs.
    static UI_MUTATOR_PTR: Cell<*mut MutatorState> = const { Cell::new(std::ptr::null_mut()) };
}

/// Public façade for `ensure_ui_mutator` — used by callers
/// outside this module (Phase 6 callback dispatcher, etc.) that
/// need the UI thread's mutator state from inside a Win32 callback.
/// Panics with a clear message rather than returning Option so the
/// failure mode is obvious in stack traces.
pub fn ui_mutator_or_panic() -> *mut MutatorState {
    #[cfg(windows)]
    {
        ensure_ui_mutator()
    }
    #[cfg(not(windows))]
    {
        panic!("ui_mutator_or_panic: Windows surface not available on this platform")
    }
}

/// Get the UI thread's mutator state pointer, creating it on first
/// use. Must be called FROM the UI thread (it calls
/// `coord.register_mutator()` which introspects the calling thread's
/// stack). Panics if no coordinator has been published (i.e. the
/// worker hasn't initialised Lisp yet — shouldn't happen because
/// the worker doesn't send `WM_NCL_EXECUTE` until after it's up).
#[cfg(windows)]
fn ensure_ui_mutator() -> *mut MutatorState {
    UI_MUTATOR_PTR.with(|cell| {
        let ptr = cell.get();
        if !ptr.is_null() {
            return ptr;
        }
        let coord = GC_COORDINATOR
            .get()
            .expect("ensure_ui_mutator: worker hasn't published coordinator yet")
            .clone();
        let mutator = Box::new(coord.register_mutator());
        let raw = Box::into_raw(mutator);
        cell.set(raw);
        raw
    })
}

// ─── Lisp shims ───────────────────────────────────────────────────────
//
// All shims are installed unconditionally — they're tiny and they
// return the truth (NIL) when the surface is off. That keeps Lisp
// code uniformly able to ask `(windows-enabled-p)` and branch on it
// in init.lisp without an "is this shim installed?" dance.

/// `(windows-enabled-p) → t / nil`
pub extern "C-unwind" fn windows_enabled_p_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    if windows_enabled() {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(ui-thread-id) → fixnum / nil`
pub extern "C-unwind" fn ui_thread_id_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    match ui_thread_id() {
        Some(tid) => Word::fixnum(tid as i64).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(ui-thread-p) → t / nil`
pub extern "C-unwind" fn ui_thread_p_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    if am_i_ui_thread() {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(%ui-execute closure) → value`
///
/// Marshal a 0-arg Lisp closure to the UI thread, block until it
/// returns, return its primary value. The `(on-ui-thread BODY)`
/// macro wraps BODY in a `(lambda () BODY)` and calls this.
///
/// Errors with a runtime condition if:
///   - the Windows surface isn't enabled
///   - the dispatch HWND wasn't initialised
///   - the closure's call panicked on the UI thread
pub extern "C-unwind" fn ui_execute_shim(
    _mutator: *mut MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%ui-execute: expected 1 argument (closure), got {n_args}");
    }
    if !windows_enabled() {
        panic!("%ui-execute: Windows surface not enabled — start ncl with --windows");
    }
    let closure_word = unsafe { *args };
    let mut req = ExecuteRequest {
        closure_word,
        out_result: Word::NIL.raw(),
        out_error_word: Word::NIL.raw(),
    };
    #[cfg(windows)]
    {
        // If we're already on the UI thread, short-circuit: call the
        // closure directly with no SendMessage round-trip. This
        // makes nested `(on-ui-thread (on-ui-thread …))` free.
        if am_i_ui_thread() {
            let mutator = ensure_ui_mutator();
            return unsafe {
                crate::abi::ncl_funcall(mutator, closure_word, std::ptr::null(), 0)
            };
        }
        platform::send_execute(&mut req);
    }
    if req.out_error_word != Word::NIL.raw() {
        panic!("%ui-execute: closure on UI thread panicked");
    }
    req.out_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_enabled_defaults_false() {
        // OnceLock — if any earlier test in this binary set it true,
        // we can't reset. So this only checks the unset case is
        // false. The actual reset semantics are tested by running
        // `cargo test --test ...` in a fresh process.
        if WINDOWS_ENABLED.get().is_none() {
            assert!(!windows_enabled());
        }
    }

    #[test]
    fn ui_thread_id_unset_means_none() {
        if UI_THREAD_ID.get().is_none() {
            assert_eq!(ui_thread_id(), None);
            assert!(!am_i_ui_thread());
        }
    }
}
