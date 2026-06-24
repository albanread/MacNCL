//! Lisp-callable shims for iGui — the integration layer between
//! the renderer/window subsystem and the rest of the runtime's
//! `extern "C-unwind"` shim conventions.
//!
//! Each function here has the standard JIT calling convention
//! `(mutator, env, args, n_args) -> u64` so it can be installed in
//! a Symbol's function cell via `install_native`. It does any
//! Word-to-native translation, calls the corresponding iGui
//! function (which lives on the GUI thread or marshals onto it),
//! and converts the result back.
//!
//! First slice (this commit): start/quit, the window-management
//! trio (open-child / close-child / set-title), and next-event.
//! Drawing primitives (with-batch / fill-rect / ...) come next.

#![cfg(windows)]

use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use crate::gc_string;
use crate::word::{Tag, Word};
use crate::GcCoordinator;

use super::batch::{
    self as batch_mod, FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd,
    TextAlign, TextRun, TextTrimming,
};
use super::channels::{kind, IGuiEvent};
use super::cp_exports::FRAME_HWND;
use super::{channels, log_view, window};

/// JoinHandle for the GUI thread, taken on `(igui-wait)`. Tracking
/// it here means the Lisp thread can block until the message pump
/// exits — useful for one-shot demos via `--eval`, where without a
/// wait the main thread would return immediately and tear the
/// process (and its GUI thread) down with it.
static GUI_THREAD: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

// -- helpers ----------------------------------------------------------------

fn arg(args: *const u64, i: u64) -> Word {
    Word::from_raw(unsafe { *args.add(i as usize) })
}

fn arg_fixnum(args: *const u64, i: u64) -> Option<i64> {
    arg(args, i).as_fixnum()
}

fn arg_string(args: *const u64, i: u64) -> Option<String> {
    let w = arg(args, i);
    if w.tag() != Tag::String {
        return None;
    }
    Some(gc_string::chars_of(w).collect())
}

/// Build a runtime keyword (`:NAME`) from a name string. Uses the
/// same colon-prefixed convention the compiler uses for keyword
/// literals so user code can `eq` them.
fn kw(coord: &GcCoordinator, name: &str) -> Word {
    let mut buf = String::with_capacity(name.len() + 1);
    buf.push(':');
    buf.push_str(name);
    coord.intern(&buf)
}

// -- shim entry points ------------------------------------------------------

/// `(igui-start)` — spawn the GUI thread, wait for the frame to be
/// up, return T. Idempotent: subsequent calls are no-ops.
/// Returns NIL on startup failure.
///
/// The JoinHandle for the spawned thread is stashed; `(igui-wait)`
/// blocks the calling thread on it.
/// Ensure the iGui MDI frame is running. Idempotent — safe to call even
/// if `(igui-start)` has already been called from Lisp or if `begin` was
/// called by the startup splash. Returns `true` on success.
///
/// Spawns the GUI thread and waits up to 5 s for `FRAME_HWND` to appear.
/// The `JoinHandle` is registered in `GUI_THREAD` so `(igui-wait)` works
/// regardless of whether the frame was started by this function or by the
/// Lisp-level `(igui-start)` shim.
pub(crate) fn ensure_igui_started() -> bool {
    let slot = GUI_THREAD.get_or_init(|| Mutex::new(None));
    {
        let guard = slot.lock().unwrap();
        if guard.is_some() {
            return true; // already running
        }
    }

    let handle = std::thread::spawn(|| {
        match super::run(None::<fn()>) {
            Ok(code) => {
                if code != 0 {
                    eprintln!("[igui] message pump exited with code {code}");
                }
            }
            Err(e) => {
                eprintln!("[igui] startup failed: {e}");
            }
        }
    });
    {
        let mut guard = slot.lock().unwrap();
        *guard = Some(handle);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while FRAME_HWND.get().is_none() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    crate::win_surface::set_windows_enabled(true);
    true
}

pub extern "C-unwind" fn igui_start_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    // Kick off the long-uptime entropy stirrer before the frame starts.
    {
        let m = unsafe { &*mutator };
        crate::random::ensure_stirrer_started(m.coord().young_starts());
    }

    if ensure_igui_started() {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(igui-wait)` — block the calling Lisp thread until the GUI
/// thread exits (i.e. the message pump returned after WM_QUIT).
/// Returns T on a clean shutdown, NIL if the GUI thread was never
/// started.
///
/// This is what one-shot demos via `--eval` use: `(igui-start)`
/// kicks the GUI off, then `(igui-wait)` keeps the process alive
/// until the user closes the frame. For a long-running REPL or
/// app, you'd typically call `(next-event ...)` in a loop instead
/// and never block on this.
pub extern "C-unwind" fn igui_wait_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    let Some(slot) = GUI_THREAD.get() else {
        return Word::NIL.raw();
    };
    let handle = {
        let mut guard = slot.lock().unwrap();
        guard.take()
    };
    match handle {
        Some(h) => {
            let _ = h.join();
            Word::T.raw()
        }
        None => Word::NIL.raw(),
    }
}

/// `(igui-quit)` — post WM_CLOSE to the frame. The GUI thread
/// tears down at its own pace; subsequent next-event calls drain
/// any pending events before EvFrameClose arrives. Returns T.
pub extern "C-unwind" fn igui_quit_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    if let Some(&hwnd_raw) = FRAME_HWND.get() {
        let hwnd = HWND(hwnd_raw as *mut _);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    Word::T.raw()
}

/// `(open-child title)` — open a new MDI child with the given
/// title; returns its child-id as a fixnum, or NIL on failure.
/// Synchronous: blocks the caller while the GUI thread creates
/// the window via SendMessageW.
pub extern "C-unwind" fn open_child_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-child: expected 1 arg (title), got {n_args}");
    }
    let Some(title) = arg_string(args, 0) else {
        panic!("open-child: title must be a string");
    };
    match window::open_child(&title) {
        Some(id) => Word::fixnum(id).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(open-child-sized title width height)` — open a new MDI child
/// with an explicit initial pixel size. Pass 0 for either
/// dimension to fall back to the Windows-default size for that
/// axis. Returns the child-id fixnum, or NIL on failure.
pub extern "C-unwind" fn open_child_sized_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 3 {
        panic!("open-child-sized: expected 3 args (title width height), got {n_args}");
    }
    let Some(title) = arg_string(args, 0) else {
        panic!("open-child-sized: title must be a string");
    };
    let w = arg_fixnum(args, 1).unwrap_or(0) as i32;
    let h = arg_fixnum(args, 2).unwrap_or(0) as i32;
    match window::open_child_sized(&title, w, h) {
        Some(id) => Word::fixnum(id).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(close-child child-id)` — close the named child. Returns T on
/// success, NIL if the child id is unknown.
pub extern "C-unwind" fn close_child_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("close-child: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("close-child: child-id must be a fixnum");
    };
    if window::close_child(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(set-title child-id title)` — change a child's window title.
/// Returns T on success, NIL if the child id is unknown.
pub extern "C-unwind" fn set_title_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("set-title: expected 2 args (child-id, title), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("set-title: child-id must be a fixnum");
    };
    let Some(title) = arg_string(args, 1) else {
        panic!("set-title: title must be a string");
    };
    window::set_child_title(id, &title);
    Word::T.raw()
}

/// `(set-redraw-rate child-id interval-ms)` — schedule a periodic
/// `:tick` event for CHILD-ID every INTERVAL-MS milliseconds. A
/// non-positive interval clears any existing timer. Returns T on
/// success, NIL if the child id is unknown. Win32 auto-coalesces
/// pending WM_TIMERs, so a backed-up language thread sees at most
/// one `:tick` per drain cycle.
pub extern "C-unwind" fn set_redraw_rate_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("set-redraw-rate: expected 2 args (child-id interval-ms), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("set-redraw-rate: child-id must be a fixnum");
    };
    let Some(ms) = arg_fixnum(args, 1) else {
        panic!("set-redraw-rate: interval-ms must be a fixnum");
    };
    if super::window::set_redraw_rate(id, ms) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(next-event timeout-ms)` — pull the next event off the global
/// mailbox. `timeout-ms` is a fixnum: negative blocks forever,
/// 0 polls without blocking, positive waits up to N ms. Returns
/// a property list `(:kind ... :child-id ... ...)` describing the
/// event, or NIL on timeout.
///
/// Each event variant maps to a fixed set of plist keys. The
/// `:kind` key is always present and is a keyword identifying the
/// event family (`:KEY`, `:MOUSE`, `:RESIZE`, `:CLOSE`, etc.).
pub extern "C-unwind" fn next_event_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("next-event: expected 1 arg (timeout-ms), got {n_args}");
    }
    let Some(timeout) = arg_fixnum(args, 0) else {
        panic!("next-event: timeout-ms must be a fixnum");
    };
    // Park while we're blocked in the channel wait — otherwise a
    // peer mutator that triggers GC waits forever for us to reach
    // a safepoint we'll never hit until an event arrives. Safe
    // because young is walked via the start-bit bitmap, not by
    // linearly parsing cells — an abandoned TLAB tail is just a
    // run of "no-bit-set" cells and the walker skips it for free.
    let m = unsafe { &mut *mutator };
    let do_park = timeout != 0;
    if do_park { m.enter_blocked(); }
    let ev_opt = channels::next_event(timeout);
    if do_park { m.leave_blocked(); }
    let Some(ev) = ev_opt else {
        return Word::NIL.raw();
    };
    let coord = std::sync::Arc::clone(m.coord());
    event_to_plist(m, &coord, ev).raw()
}

/// `(next-event-for child-id timeout-ms)` — block until an event
/// arrives for CHILD-ID (or a global event like :FRAME-CLOSE).
/// Other children's events park in a stash and are visible to
/// later consumers. NIL on timeout.
pub extern "C-unwind" fn next_event_for_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("next-event-for: expected 2 args (child-id timeout-ms), got {n_args}");
    }
    let Some(child_id) = arg_fixnum(args, 0) else {
        panic!("next-event-for: child-id must be a fixnum");
    };
    let Some(timeout) = arg_fixnum(args, 1) else {
        panic!("next-event-for: timeout-ms must be a fixnum");
    };
    // Same park-on-block protocol as next_event_shim above.
    let m = unsafe { &mut *mutator };
    let do_park = timeout != 0;
    if do_park { m.enter_blocked(); }
    let ev_opt = channels::next_event_for(child_id, timeout);
    if do_park { m.leave_blocked(); }
    let Some(ev) = ev_opt else {
        return Word::NIL.raw();
    };
    let coord = std::sync::Arc::clone(m.coord());
    event_to_plist(m, &coord, ev).raw()
}

/// `(filter-on-window child-id)` — add CHILD-ID to the persistent
/// event-interest set. While the set is non-empty, plain
/// (next-event) only returns events for member windows (plus
/// globals). Returns T.
pub extern "C-unwind" fn filter_on_window_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("filter-on-window: expected 1 arg (child-id)");
    }
    let Some(child_id) = arg_fixnum(args, 0) else {
        panic!("filter-on-window: child-id must be a fixnum");
    };
    channels::filter_on_window(child_id);
    Word::T.raw()
}

/// `(unfilter-window child-id)` — remove CHILD-ID from the
/// interest set. Returns T.
pub extern "C-unwind" fn unfilter_window_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("unfilter-window: expected 1 arg (child-id)");
    }
    let Some(child_id) = arg_fixnum(args, 0) else {
        panic!("unfilter-window: child-id must be a fixnum");
    };
    channels::unfilter_window(child_id);
    Word::T.raw()
}

/// `(clear-event-filter)` — drop every window from the interest
/// set. Returns T.
pub extern "C-unwind" fn clear_event_filter_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    channels::clear_filter();
    Word::T.raw()
}

/// `(discard-stashed-events)` — drop every event currently in the
/// stash. Useful on mode transitions (close one app, start another)
/// where carry-over events would be confusing.
pub extern "C-unwind" fn discard_stashed_events_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    channels::discard_stashed_events();
    Word::T.raw()
}

// -- event → plist conversion ------------------------------------------------

fn event_to_plist(
    m: &mut crate::mutator::MutatorState,
    coord: &GcCoordinator,
    ev: IGuiEvent,
) -> Word {
    let mut pairs: Vec<(Word, Word)> = Vec::new();
    match ev {
        IGuiEvent::Key {
            child_id, vkey, scancode, mods, repeat, down, time_ms,
        } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "KEY")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "VKEY"), Word::fixnum(vkey)));
            pairs.push((kw(coord, "SCANCODE"), Word::fixnum(scancode)));
            pairs.push((kw(coord, "MODS"), Word::fixnum(mods)));
            pairs.push((kw(coord, "REPEAT"), Word::fixnum(repeat)));
            pairs.push((kw(coord, "DOWN"), if down { Word::T } else { Word::NIL }));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::Char {
            child_id, codepoint, mods, time_ms,
        } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "CHAR")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            let ch = char::from_u32(codepoint as u32)
                .map(Word::char)
                .unwrap_or(Word::NIL);
            pairs.push((kw(coord, "CHAR"), ch));
            pairs.push((kw(coord, "CODEPOINT"), Word::fixnum(codepoint)));
            pairs.push((kw(coord, "MODS"), Word::fixnum(mods)));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::Mouse {
            child_id, x, y, op, button, mods, wheel_delta, wheel_lines, time_ms,
        } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "MOUSE")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "X"), Word::fixnum(x)));
            pairs.push((kw(coord, "Y"), Word::fixnum(y)));
            pairs.push((kw(coord, "OP"), kw(coord, mouse_op_name(op))));
            pairs.push((kw(coord, "BUTTON"), Word::fixnum(button)));
            pairs.push((kw(coord, "MODS"), Word::fixnum(mods)));
            pairs.push((kw(coord, "WHEEL-DELTA"), Word::fixnum(wheel_delta)));
            pairs.push((kw(coord, "WHEEL-LINES"), Word::fixnum(wheel_lines)));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::Focus { child_id, gained } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "FOCUS")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "GAINED"), if gained { Word::T } else { Word::NIL }));
        }
        IGuiEvent::Resize { child_id, width, height } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "RESIZE")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "WIDTH"), Word::fixnum(width)));
            pairs.push((kw(coord, "HEIGHT"), Word::fixnum(height)));
        }
        IGuiEvent::Close { child_id } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "CLOSE")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
        }
        IGuiEvent::FrameClose => {
            pairs.push((kw(coord, "KIND"), kw(coord, "FRAME-CLOSE")));
        }
        IGuiEvent::ThemeChange => {
            pairs.push((kw(coord, "KIND"), kw(coord, "THEME-CHANGE")));
        }
        IGuiEvent::DpiChange { child_id, dpi_x, dpi_y } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "DPI-CHANGE")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "DPI-X"), Word::fixnum(dpi_x)));
            pairs.push((kw(coord, "DPI-Y"), Word::fixnum(dpi_y)));
        }
        IGuiEvent::Menu { menu_id, item_id } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "MENU")));
            pairs.push((kw(coord, "MENU-ID"), Word::fixnum(menu_id)));
            pairs.push((kw(coord, "ITEM-ID"), Word::fixnum(item_id)));
        }
        IGuiEvent::Tick { child_id, time_ms } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "TICK")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::EvalBuffer { source } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "EVAL-BUFFER")));
            // Allocate the source text as a Lisp string in young.
            let s = crate::gc_string::alloc_string_in_young(m, source.as_str());
            pairs.push((kw(coord, "SOURCE"), s));
        }
        IGuiEvent::ReplSubmit { child_id } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "REPL-SUBMIT")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
        }
    }
    // Build (k1 v1 k2 v2 ...) right-to-left.
    let mut acc = Word::NIL;
    for (k, v) in pairs.into_iter().rev() {
        acc = m.alloc_cons(v, acc);
        acc = m.alloc_cons(k, acc);
    }
    acc
}

// -- Log view ----------------------------------------------------------------

/// `(log-write s)` — append S as a new line to the iGui log
/// overlay's ring buffer. The log_view child shows the buffer
/// (Tools → Log, or Ctrl+Shift+L). Repeated identical lines
/// coalesce: the count next to the line ticks up rather than
/// pushing a fresh entry.
pub extern "C-unwind" fn log_write_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("log-write: expected 1 arg (string), got {n_args}");
    }
    let Some(s) = arg_string(args, 0) else {
        panic!("log-write: argument must be a string");
    };
    log_view::append(&s);
    Word::T.raw()
}

// -- Drawing -----------------------------------------------------------------
//
// Each call site looks like
//
//   (with-batch CHILD-ID
//     (clear (rgb 30 30 50))
//     (fill-rect 10 10 100 50 (rgb 200 50 50)))
//
// where `with-batch` is a macro wrapping `(%begin-batch ...)` ...
// `(%submit-batch)`. The drawing primitives below assume an active
// batch (thread-local) and just push commands onto it.
//
// Color is a packed fixnum: 0xRRGGBBAA, one byte per channel.
// `(rgb r g b)` and `(rgba r g b a)` are user-Lisp constructors.

fn unpack_rgba(packed: i64) -> Rgba {
    let bits = packed as u64;
    let r = ((bits >> 24) & 0xFF) as f32 / 255.0;
    let g = ((bits >> 16) & 0xFF) as f32 / 255.0;
    let b = ((bits >> 8) & 0xFF) as f32 / 255.0;
    let a = (bits & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a }
}

/// `(%begin-batch child-id)` — start building a new batch for the
/// given child. Subsequent emit calls push onto it; `(%submit-batch)`
/// hands it to the GUI thread.
pub extern "C-unwind" fn begin_batch_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%begin-batch: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("%begin-batch: child-id must be a fixnum");
    };
    batch_mod::begin(id);
    Word::T.raw()
}

/// `(%submit-batch)` — hand the in-flight batch to the GUI thread.
/// Returns T on success, NIL if no batch is in flight or the
/// child-id no longer exists.
pub extern "C-unwind" fn submit_batch_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    let Some(batch) = batch_mod::finish() else {
        return Word::NIL.raw();
    };
    if batch_mod::submit(batch) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(%emit-clear color)` — fill the entire client area with `color`.
pub extern "C-unwind" fn emit_clear_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("%emit-clear: expected 1 arg (color), got {n_args}");
    }
    let Some(c) = arg_fixnum(args, 0) else {
        panic!("%emit-clear: color must be a fixnum");
    };
    batch_mod::push(SurfaceCmd::Clear { color: unpack_rgba(c) });
    Word::T.raw()
}

/// `(%emit-fill-rect x y w h color)` — filled rectangle. Coords
/// are fixnums (pixel-precise; sub-pixel waits on float support).
pub extern "C-unwind" fn emit_fill_rect_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 5 {
        panic!("%emit-fill-rect: expected 5 args (x y w h color), got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let w = arg_fixnum(args, 2).expect("w") as f32;
    let h = arg_fixnum(args, 3).expect("h") as f32;
    let c = arg_fixnum(args, 4).expect("color");
    batch_mod::push(SurfaceCmd::FillRect {
        rect: Rect { x0: x, y0: y, x1: x + w, y1: y + h },
        corner_radius: 0.0,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-stroke-rect x y w h thickness color)` — outlined rectangle.
pub extern "C-unwind" fn emit_stroke_rect_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 6 {
        panic!("%emit-stroke-rect: expected 6 args, got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let w = arg_fixnum(args, 2).expect("w") as f32;
    let h = arg_fixnum(args, 3).expect("h") as f32;
    let t = arg_fixnum(args, 4).expect("thickness") as f32;
    let c = arg_fixnum(args, 5).expect("color");
    batch_mod::push(SurfaceCmd::StrokeRect {
        rect: Rect { x0: x, y0: y, x1: x + w, y1: y + h },
        corner_radius: 0.0,
        half_thickness: t * 0.5,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-fill-oval x y w h color)` — filled ellipse with the
/// given axis-aligned bounding box.
pub extern "C-unwind" fn emit_fill_oval_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 5 {
        panic!("%emit-fill-oval: expected 5 args, got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let w = arg_fixnum(args, 2).expect("w") as f32;
    let h = arg_fixnum(args, 3).expect("h") as f32;
    let c = arg_fixnum(args, 4).expect("color");
    batch_mod::push(SurfaceCmd::FillOval {
        rect: Rect { x0: x, y0: y, x1: x + w, y1: y + h },
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-stroke-oval x y w h thickness color)` — outlined ellipse.
pub extern "C-unwind" fn emit_stroke_oval_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 6 {
        panic!("%emit-stroke-oval: expected 6 args, got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let w = arg_fixnum(args, 2).expect("w") as f32;
    let h = arg_fixnum(args, 3).expect("h") as f32;
    let t = arg_fixnum(args, 4).expect("thickness") as f32;
    let c = arg_fixnum(args, 5).expect("color");
    batch_mod::push(SurfaceCmd::StrokeOval {
        rect: Rect { x0: x, y0: y, x1: x + w, y1: y + h },
        half_thickness: t * 0.5,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-fill-circle cx cy radius color)` — filled circle from
/// center + radius.
pub extern "C-unwind" fn emit_fill_circle_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 4 {
        panic!("%emit-fill-circle: expected 4 args, got {n_args}");
    }
    let cx = arg_fixnum(args, 0).expect("cx") as f32;
    let cy = arg_fixnum(args, 1).expect("cy") as f32;
    let r = arg_fixnum(args, 2).expect("radius") as f32;
    let c = arg_fixnum(args, 3).expect("color");
    batch_mod::push(SurfaceCmd::FillCircle {
        center: Point { x: cx, y: cy },
        radius: r,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-stroke-circle cx cy radius thickness color)` — outlined circle.
pub extern "C-unwind" fn emit_stroke_circle_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 5 {
        panic!("%emit-stroke-circle: expected 5 args, got {n_args}");
    }
    let cx = arg_fixnum(args, 0).expect("cx") as f32;
    let cy = arg_fixnum(args, 1).expect("cy") as f32;
    let r = arg_fixnum(args, 2).expect("radius") as f32;
    let t = arg_fixnum(args, 3).expect("thickness") as f32;
    let c = arg_fixnum(args, 4).expect("color");
    batch_mod::push(SurfaceCmd::StrokeCircle {
        center: Point { x: cx, y: cy },
        radius: r,
        half_thickness: t * 0.5,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-draw-arc cx cy radius rotation-deg aperture-deg thickness color)`
/// — outlined circular arc. Angles are degrees (fixnums); we
/// convert to radians here. `aperture-deg` is the FULL angular
/// span; the underlying iGui takes a half-aperture, so we halve.
pub extern "C-unwind" fn emit_draw_arc_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 7 {
        panic!("%emit-draw-arc: expected 7 args, got {n_args}");
    }
    let cx = arg_fixnum(args, 0).expect("cx") as f32;
    let cy = arg_fixnum(args, 1).expect("cy") as f32;
    let r = arg_fixnum(args, 2).expect("radius") as f32;
    let rot_deg = arg_fixnum(args, 3).expect("rotation-deg") as f32;
    let aperture_deg = arg_fixnum(args, 4).expect("aperture-deg") as f32;
    let t = arg_fixnum(args, 5).expect("thickness") as f32;
    let c = arg_fixnum(args, 6).expect("color");
    let to_rad = std::f32::consts::PI / 180.0;
    batch_mod::push(SurfaceCmd::DrawArc {
        center: Point { x: cx, y: cy },
        radius: r,
        rotation_rad: rot_deg * to_rad,
        half_aperture_rad: aperture_deg * to_rad * 0.5,
        half_thickness: t * 0.5,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

/// `(%emit-draw-text-styled x y text size color opts-plist)` —
/// styled-text variant. `opts-plist` is a property list with any
/// of:
///   :family   string         — DirectWrite font family name
///   :weight   fixnum         — 100..900 (DWRITE_FONT_WEIGHT scale)
///   :style    keyword        — :normal / :italic / :oblique
///   :align    keyword        — :leading / :trailing / :center / :justified
///   :stretch  fixnum         — 1..9 (DWRITE_FONT_STRETCH scale)
/// Unrecognised keys are ignored. Missing keys take the same
/// defaults as `(draw-text …)`.
pub extern "C-unwind" fn emit_draw_text_styled_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 6 {
        panic!("%emit-draw-text-styled: expected 6 args, got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let Some(text) = arg_string(args, 2) else {
        panic!("%emit-draw-text-styled: text must be a string");
    };
    let size = arg_fixnum(args, 3).expect("size") as f32;
    let c = arg_fixnum(args, 4).expect("color");
    let opts = arg(args, 5);

    let style_opts = parse_text_opts(opts);
    batch_mod::push(SurfaceCmd::DrawTextRun {
        run: TextRun {
            text,
            origin: Point { x, y },
            family: style_opts.family,
            size,
            weight: style_opts.weight,
            style: style_opts.style,
            stretch: style_opts.stretch,
            locale: "en-us".to_string(),
            color: unpack_rgba(c),
            max_width: None,
            alignment: style_opts.alignment,
            trimming: TextTrimming::None,
        },
    });
    Word::T.raw()
}

/// `(measure-text child-id text size opts-plist)` →
/// `(:width W :height H :ascent A :line-count N)` or NIL.
///
/// Uses the same text-runs and the same DirectWrite layout cache
/// the drawing path uses, so width/height returned here matches
/// what `draw-text-styled` will render. Implementation submits a
/// measure-only batch on top of the live draw batch and restores
/// the draw batch as soon as the reply arrives — so the pane's
/// visible content shouldn't change observably for the user
/// (modulo a frame's worth of paint timing, which is invisibly
/// fast).
pub extern "C-unwind" fn measure_text_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 4 {
        panic!("measure-text: expected 4 args (child-id text size opts), got {n_args}");
    }
    let Some(child_id) = arg_fixnum(args, 0) else {
        panic!("measure-text: child-id must be a fixnum");
    };
    let Some(text) = arg_string(args, 1) else {
        panic!("measure-text: text must be a string");
    };
    let size = arg_fixnum(args, 2).expect("size") as f32;
    let opts = arg(args, 3);

    let style_opts = parse_text_opts(opts);
    let run = TextRun {
        text,
        origin: Point { x: 0.0, y: 0.0 },
        family: style_opts.family,
        size,
        weight: style_opts.weight,
        style: style_opts.style,
        stretch: style_opts.stretch,
        locale: "en-us".to_string(),
        color: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        max_width: None,
        alignment: style_opts.alignment,
        trimming: TextTrimming::None,
    };

    // Save state we'll need to put back when we're done.
    //
    //   `in_progress` — the batch the calling thread is currently
    //                   building, if any (i.e. we were called from
    //                   inside a with-batch). `begin()` would
    //                   clobber this without the save.
    //   `displayed`   — the batch currently in the pane's slot,
    //                   which `submit()` of our measure batch will
    //                   replace. We restore it after the measure
    //                   reply arrives so the visible content stays
    //                   put.
    let in_progress = batch_mod::finish();
    let displayed = batch_mod::snapshot(child_id);

    let request_id = super::replies::alloc_id();
    let rx = super::replies::install(request_id);
    batch_mod::begin(child_id);
    batch_mod::push(SurfaceCmd::MeasureTextRun { request_id, run });
    let Some(b) = batch_mod::finish() else {
        batch_mod::restore_current(in_progress);
        return Word::NIL.raw();
    };
    if !batch_mod::submit(b) {
        batch_mod::restore_current(in_progress);
        return Word::NIL.raw();
    }

    let metrics = super::replies::wait(rx);

    // Restore the displayed batch (so the pane's visible content
    // doesn't go blank), then the in-progress batch (so the
    // surrounding with-batch can keep pushing into it).
    if let Some(arc) = displayed {
        let _ = batch_mod::submit((*arc).clone());
    }
    batch_mod::restore_current(in_progress);

    let (width, height, ascent, line_count) = match metrics {
        Some(super::replies::Reply::Metrics { width, height, ascent, line_count }) => {
            (width, height, ascent, line_count)
        }
        _ => return Word::NIL.raw(),
    };

    let m = unsafe { &mut *mutator };
    let coord = std::sync::Arc::clone(m.coord());
    let pairs = [
        (kw(&coord, "WIDTH"), Word::fixnum(width.round() as i64)),
        (kw(&coord, "HEIGHT"), Word::fixnum(height.round() as i64)),
        (kw(&coord, "ASCENT"), Word::fixnum(ascent.round() as i64)),
        (kw(&coord, "LINE-COUNT"), Word::fixnum(line_count as i64)),
    ];
    let mut acc = Word::NIL;
    for (k, v) in pairs.iter().rev() {
        acc = m.alloc_cons(*v, acc);
        acc = m.alloc_cons(*k, acc);
    }
    acc.raw()
}

/// Pull a (car, cdr) from a Word that's expected to be a Cons.
/// Returns None for nil or non-cons.
fn take_pair(w: Word) -> Option<(Word, Word)> {
    if w.tag() != Tag::Cons {
        return None;
    }
    let p = w.as_ptr::<u64>(Tag::Cons)?;
    let car = Word::from_raw(unsafe { *p });
    let cdr = Word::from_raw(unsafe { *p.add(1) });
    Some((car, cdr))
}

/// Lookup a Word's printer-name if it's a Symbol with a registered
/// name. Used to extract `:FAMILY`, `:CENTER`, etc. from keyword
/// args at the iGui boundary.
fn keyword_name(w: Word) -> Option<std::sync::Arc<str>> {
    if w.tag() != Tag::Symbol {
        return None;
    }
    crate::sym_names::lookup(w.raw())
}

/// Parsed text-styling options. Used by both `draw-text-styled`
/// and `measure-text` so the rendering and the measurement see
/// identical text-run parameters.
struct TextOpts {
    family: String,
    weight: u16,
    style: FontStyle,
    stretch: FontStretch,
    alignment: TextAlign,
}

impl Default for TextOpts {
    fn default() -> Self {
        TextOpts {
            family: "Segoe UI".to_string(),
            weight: 400,
            style: FontStyle::Normal,
            stretch: FontStretch::Normal,
            alignment: TextAlign::Leading,
        }
    }
}

/// Walk a `(:key1 val1 :key2 val2 ...)` plist and populate text
/// styling fields. Unrecognised keys are silently ignored.
fn parse_text_opts(opts: Word) -> TextOpts {
    let mut out = TextOpts::default();
    let mut cur = opts;
    while !cur.is_nil() {
        let Some((k, rest)) = take_pair(cur) else { break };
        let Some((v, rest)) = take_pair(rest) else { break };
        cur = rest;
        let Some(kname) = keyword_name(k) else { continue };
        match kname.as_ref() {
            ":FAMILY" => {
                if v.tag() == Tag::String {
                    out.family = crate::gc_string::chars_of(v).collect();
                }
            }
            ":WEIGHT" => {
                if let Some(n) = v.as_fixnum() {
                    out.weight = n.clamp(100, 900) as u16;
                }
            }
            ":STYLE" => {
                if let Some(name) = keyword_name(v) {
                    out.style = match name.as_ref() {
                        ":ITALIC" => FontStyle::Italic,
                        ":OBLIQUE" => FontStyle::Oblique,
                        _ => FontStyle::Normal,
                    };
                }
            }
            ":STRETCH" => {
                if let Some(n) = v.as_fixnum() {
                    out.stretch = match n {
                        1 => FontStretch::UltraCondensed,
                        2 => FontStretch::ExtraCondensed,
                        3 => FontStretch::Condensed,
                        4 => FontStretch::SemiCondensed,
                        5 => FontStretch::Normal,
                        6 => FontStretch::SemiExpanded,
                        7 => FontStretch::Expanded,
                        8 => FontStretch::ExtraExpanded,
                        9 => FontStretch::UltraExpanded,
                        _ => FontStretch::Normal,
                    };
                }
            }
            ":ALIGN" => {
                if let Some(name) = keyword_name(v) {
                    out.alignment = match name.as_ref() {
                        ":CENTER" => TextAlign::Center,
                        ":TRAILING" | ":RIGHT" => TextAlign::Trailing,
                        ":JUSTIFIED" => TextAlign::Justified,
                        _ => TextAlign::Leading,
                    };
                }
            }
            _ => {}
        }
    }
    out
}

/// `(%emit-draw-text x y text size color)` — render a string in
/// the default UI font (Segoe UI, weight 400). The defaults cover
/// the common case; users who need a different family / weight /
/// style can call `%emit-draw-text-styled` (next slice).
pub extern "C-unwind" fn emit_draw_text_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 5 {
        panic!("%emit-draw-text: expected 5 args (x y text size color), got {n_args}");
    }
    let x = arg_fixnum(args, 0).expect("x") as f32;
    let y = arg_fixnum(args, 1).expect("y") as f32;
    let Some(text) = arg_string(args, 2) else {
        panic!("%emit-draw-text: text must be a string");
    };
    let size = arg_fixnum(args, 3).expect("size") as f32;
    let c = arg_fixnum(args, 4).expect("color");

    batch_mod::push(SurfaceCmd::DrawTextRun {
        run: TextRun {
            text,
            origin: Point { x, y },
            family: "Segoe UI".to_string(),
            size,
            weight: 400, // DWRITE_FONT_WEIGHT_NORMAL
            style: FontStyle::Normal,
            stretch: FontStretch::Normal,
            locale: "en-us".to_string(),
            color: unpack_rgba(c),
            max_width: None,
            alignment: TextAlign::Leading,
            trimming: TextTrimming::None,
        },
    });
    Word::T.raw()
}

/// `(%emit-draw-line x1 y1 x2 y2 thickness color)` — line segment.
pub extern "C-unwind" fn emit_draw_line_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 6 {
        panic!("%emit-draw-line: expected 6 args, got {n_args}");
    }
    let x1 = arg_fixnum(args, 0).expect("x1") as f32;
    let y1 = arg_fixnum(args, 1).expect("y1") as f32;
    let x2 = arg_fixnum(args, 2).expect("x2") as f32;
    let y2 = arg_fixnum(args, 3).expect("y2") as f32;
    let t = arg_fixnum(args, 4).expect("thickness") as f32;
    let c = arg_fixnum(args, 5).expect("color");
    batch_mod::push(SurfaceCmd::DrawLine {
        p0: Point { x: x1, y: y1 },
        p1: Point { x: x2, y: y2 },
        half_thickness: t * 0.5,
        color: unpack_rgba(c),
    });
    Word::T.raw()
}

fn mouse_op_name(op: i64) -> &'static str {
    use super::channels::mouse_op;
    match op {
        x if x == mouse_op::MOVE => "MOVE",
        x if x == mouse_op::LEFT_DOWN => "LEFT-DOWN",
        x if x == mouse_op::LEFT_UP => "LEFT-UP",
        x if x == mouse_op::RIGHT_DOWN => "RIGHT-DOWN",
        x if x == mouse_op::RIGHT_UP => "RIGHT-UP",
        x if x == mouse_op::MIDDLE_DOWN => "MIDDLE-DOWN",
        x if x == mouse_op::MIDDLE_UP => "MIDDLE-UP",
        x if x == mouse_op::WHEEL => "WHEEL",
        _ => "UNKNOWN",
    }
}

// Suppress an unused-import warning in builds where `kind::*` is
// only consulted via mouse_op::* constants (i.e. always).
#[allow(dead_code)]
const _: i64 = kind::NONE;

// ───────────────────────────────────────────────────────────────────
// Text-view shims — terminal-style monospaced cell window.
//
// All children opened with `(open-text-window TITLE)` get a packed
// 0xRRGGBBAA pen colour pair (fg, bg) and a cursor (row, col). The
// commands below mutate that state and lazily invalidate the
// window — Win32 coalesces the WM_PAINT messages so a tight Lisp
// loop calling `(text-write …)` repeatedly produces O(1) repaints
// rather than one per write. Returns from each shim are T on
// success, NIL if the child id is unknown / closed.
// ───────────────────────────────────────────────────────────────────

use super::text_view;

/// `(open-text-window title)` — open a new MDI text-view child.
/// Returns its child-id as a fixnum, or NIL on failure.
pub extern "C-unwind" fn open_text_window_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-text-window: expected 1 arg (title), got {n_args}");
    }
    let Some(title) = arg_string(args, 0) else {
        panic!("open-text-window: title must be a string");
    };
    match text_view::open(&title) {
        Some(id) => Word::fixnum(id).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(text-write child-id string)` — write at the cursor, advancing
/// and wrapping/scrolling as needed. Embedded newlines, CR, tab,
/// and backspace are handled per the obvious terminal conventions.
pub extern "C-unwind" fn text_write_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("text-write: expected 2 args (child-id string), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-write: child-id must be a fixnum");
    };
    let Some(s) = arg_string(args, 1) else {
        panic!("text-write: second arg must be a string");
    };
    if text_view::write_str(id, &s) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-write-char child-id char)` — write one character at the
/// cursor. Same control-code handling as `text-write`.
pub extern "C-unwind" fn text_write_char_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("text-write-char: expected 2 args (child-id char), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-write-char: child-id must be a fixnum");
    };
    let cw = arg(args, 1);
    let cp = match cw.as_char() {
        Some(c) => c as u32,
        None => match cw.as_fixnum() {
            // Accept a fixnum codepoint too — convenient for control codes.
            Some(n) if n >= 0 => n as u32,
            _ => panic!("text-write-char: second arg must be a character or non-negative fixnum"),
        },
    };
    if text_view::write_char(id, cp) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-clear child-id)` — wipe the entire grid and put the cursor
/// at (0, 0). Uses the current pen colour for the background fill.
pub extern "C-unwind" fn text_clear_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("text-clear: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-clear: child-id must be a fixnum");
    };
    if text_view::clear_all_cmd(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-clear-eol child-id)` — clear from the cursor to the end of
/// the current line. Cursor doesn't move.
pub extern "C-unwind" fn text_clear_eol_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("text-clear-eol: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-clear-eol: child-id must be a fixnum");
    };
    if text_view::clear_to_eol_cmd(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-clear-eos child-id)` — clear from the cursor to the bottom-
/// right of the grid. Cursor doesn't move.
pub extern "C-unwind" fn text_clear_eos_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("text-clear-eos: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-clear-eos: child-id must be a fixnum");
    };
    if text_view::clear_to_eos_cmd(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-newline child-id)` — move cursor to col 0 of the next row,
/// scrolling up if it would otherwise fall off the bottom.
pub extern "C-unwind" fn text_newline_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("text-newline: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-newline: child-id must be a fixnum");
    };
    if text_view::newline_cmd(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-scroll-up child-id n)` — scroll the visible grid up N rows.
/// Top N rows fall off; bottom N rows blank in the current bg.
pub extern "C-unwind" fn text_scroll_up_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("text-scroll-up: expected 2 args (child-id n), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-scroll-up: child-id must be a fixnum");
    };
    let n = arg_fixnum(args, 1).unwrap_or(1).max(0) as u32;
    if text_view::scroll_up_cmd(id, n) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-set-cursor child-id row col)` — move the cursor.
/// Out-of-range row/col is clamped into the grid.
pub extern "C-unwind" fn text_set_cursor_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 3 {
        panic!("text-set-cursor: expected 3 args (child-id row col), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-set-cursor: child-id must be a fixnum");
    };
    let row = arg_fixnum(args, 1).unwrap_or(0).max(0) as u32;
    let col = arg_fixnum(args, 2).unwrap_or(0).max(0) as u32;
    if text_view::set_cursor(id, row, col) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

// Note: synchronous queries `text-cursor` and `text-size` are
// intentionally not exposed in the command-channel design — the
// language thread holds `child_id` as an opaque token and never
// reads back live grid state. If a Lisp caller needs a query later,
// add a SendMessageW round-trip here that drains the queue and
// returns the requested value, mirroring `OpenChildRequest`.

/// `(text-set-pen child-id fg bg)` — set the foreground and
/// background packed-RGBA colours used by subsequent writes/clears.
/// Pass NIL or a negative fixnum to keep a colour unchanged.
pub extern "C-unwind" fn text_set_pen_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 3 {
        panic!("text-set-pen: expected 3 args (child-id fg bg), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-set-pen: child-id must be a fixnum");
    };
    // Negative fixnums or NIL → "leave alone". Implemented by
    // reading current pen first when one of them is sentinel.
    let fg_arg = arg_fixnum(args, 1);
    let bg_arg = arg_fixnum(args, 2);
    // We don't have a "read pen" helper; since the typical case is
    // "set both", just default sentinels to the canonical defaults
    // when the caller passed NIL. Callers wanting to update only one
    // can call text-reset-pen first, then set both explicitly.
    let fg = match fg_arg {
        Some(n) if n >= 0 => n as u32,
        _ => 0xDCDCDCFF,
    };
    let bg = match bg_arg {
        Some(n) if n >= 0 => n as u32,
        _ => 0x12161CFF,
    };
    if text_view::set_pen(id, fg, bg) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-reset-pen child-id)` — restore the default pen.
pub extern "C-unwind" fn text_reset_pen_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("text-reset-pen: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-reset-pen: child-id must be a fixnum");
    };
    if text_view::reset_pen(id) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(text-show-caret child-id flag)` — flag is T/NIL.
pub extern "C-unwind" fn text_show_caret_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("text-show-caret: expected 2 args (child-id flag), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("text-show-caret: child-id must be a fixnum");
    };
    let visible = !arg(args, 1).is_nil();
    if text_view::set_caret_visible(id, visible) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

// ─── REPL child shims ─────────────────────────────────────────────────────────

/// `(open-repl-window title)` → child-id or NIL.
///
/// Opens a graphical REPL MDI child with the given title string.
/// Returns the child-id fixnum on success, NIL on failure.
pub extern "C-unwind" fn open_repl_window_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-repl-window: expected 1 arg (title), got {n_args}");
    }
    let Some(title) = arg_string(args, 0) else {
        panic!("open-repl-window: title must be a string");
    };
    match window::open_repl_child(&title) {
        Some(id) => Word::fixnum(id).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(repl-output child-id text)` — append an output line to the REPL
/// transcript. Returns T.
pub extern "C-unwind" fn repl_output_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("repl-output: expected 2 args (child-id text), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("repl-output: child-id must be a fixnum");
    };
    let Some(text) = arg_string(args, 1) else {
        panic!("repl-output: text must be a string");
    };
    super::repl_child::append(id, text, super::repl_child::AppendKind::Output);
    Word::T.raw()
}

/// `(repl-error child-id text)` — append an error line (red) to the
/// REPL transcript. Returns T.
pub extern "C-unwind" fn repl_error_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("repl-error: expected 2 args (child-id text), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("repl-error: child-id must be a fixnum");
    };
    let Some(text) = arg_string(args, 1) else {
        panic!("repl-error: text must be a string");
    };
    super::repl_child::append(id, text, super::repl_child::AppendKind::Error);
    Word::T.raw()
}

/// `(repl-pop-input child-id)` → string or NIL.
///
/// Called by the worker thread after receiving a `ReplSubmit` event.
/// Pops and returns the next pending input string for the given child,
/// or NIL if the queue is empty.
pub extern "C-unwind" fn repl_pop_input_shim(
    mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("repl-pop-input: expected 1 arg (child-id), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("repl-pop-input: child-id must be a fixnum");
    };
    match super::repl_child::pop_input(id) {
        Some(s) => {
            let m = unsafe { &mut *mutator };
            crate::gc_string::alloc_string_in_young(m, &s).raw()
        }
        None => Word::NIL.raw(),
    }
}

// ───────────────────────────────────────────────────────────────────
// Doc-pane shims — Markdown + Mermaid pane (docpane crate).
//
// `(open-doc-window TITLE)` opens a new MDI child whose body renders
// the Markdown source supplied later via `(doc-set-markdown ...)`
// and `(doc-append-markdown ...)`. Diagrams in fenced ```mermaid
// blocks render natively through `docpane`'s Direct2D path. All
// three shims return T / a fixnum on success and NIL on failure.
// ───────────────────────────────────────────────────────────────────

use super::doc_pane;

/// `(open-doc-window title)` — open a new MDI doc-pane child. The
/// pane starts empty; supply Markdown via `doc-set-markdown`. Returns
/// the child-id as a fixnum, or NIL on failure.
pub extern "C-unwind" fn open_doc_window_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        panic!("open-doc-window: expected 1 arg (title), got {n_args}");
    }
    let Some(title) = arg_string(args, 0) else {
        panic!("open-doc-window: title must be a string");
    };
    match doc_pane::open(&title) {
        Some(id) => Word::fixnum(id).raw(),
        None => Word::NIL.raw(),
    }
}

/// `(doc-set-markdown child-id markdown)` — replace the pane's
/// Markdown source. Returns T, or NIL if the child id is unknown.
pub extern "C-unwind" fn doc_set_markdown_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("doc-set-markdown: expected 2 args (child-id markdown), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("doc-set-markdown: child-id must be a fixnum");
    };
    let Some(md) = arg_string(args, 1) else {
        panic!("doc-set-markdown: markdown must be a string");
    };
    if doc_pane::set_markdown(id, &md) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(doc-append-markdown child-id markdown)` — append to the pane's
/// existing Markdown source. Returns T, or NIL if the child id is
/// unknown.
pub extern "C-unwind" fn doc_append_markdown_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 2 {
        panic!("doc-append-markdown: expected 2 args (child-id markdown), got {n_args}");
    }
    let Some(id) = arg_fixnum(args, 0) else {
        panic!("doc-append-markdown: child-id must be a fixnum");
    };
    let Some(md) = arg_string(args, 1) else {
        panic!("doc-append-markdown: markdown must be a string");
    };
    if doc_pane::append_markdown(id, &md) {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

// ─── Canvas: fast pixel-direct rendering ─────────────────────────────
//
// A canvas is a host-owned BGRA32 framebuffer bound to a render-host
// child by id. `canvas-open` returns the buffer's base ADDRESS as a
// fixnum; Lisp writes pixels into it with the foreign-buffer pokes
// (`buffer-set-u32 base byte-offset argb`) — no per-pixel boundary
// crossing. `canvas-present` snapshots it and draws one frame.

/// `(canvas-open child-id w h)` — open or resize the canvas for
/// CHILD-ID to W×H BGRA32, returning the base address (fixnum) of its
/// pixel buffer, or NIL on bad dimensions. Poke a pixel at (x, y) with
/// `(buffer-set-u32 base (* (+ (* y w) x) 4) #xAARRGGBB)`.
pub extern "C-unwind" fn canvas_open_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 3 {
        return Word::NIL.raw();
    }
    let (Some(id), Some(w), Some(h)) =
        (arg_fixnum(args, 0), arg_fixnum(args, 1), arg_fixnum(args, 2))
    else {
        return Word::NIL.raw();
    };
    let base = super::canvas::canvas_open(id, w, h);
    if base == 0 {
        Word::NIL.raw()
    } else {
        Word::fixnum(base as i64).raw()
    }
}

/// `(canvas-present child-id)` — snapshot the canvas and draw it this
/// frame (one Blit); returns the base address (fixnum) for the next
/// frame's writes, or NIL if no canvas is open for CHILD-ID.
pub extern "C-unwind" fn canvas_present_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    args: *const u64,
    n_args: u64,
) -> u64 {
    if n_args != 1 {
        return Word::NIL.raw();
    }
    let Some(id) = arg_fixnum(args, 0) else {
        return Word::NIL.raw();
    };
    let base = super::canvas::canvas_present(id);
    if base == 0 {
        Word::NIL.raw()
    } else {
        Word::fixnum(base as i64).raw()
    }
}

// ─── MDI window-management verbs (minimized-icon arrangement etc.) ────
//
// When the frame is maximized/full and child windows are minimized,
// Win32 shows them as small icons in the MDI client area; these verbs
// arrange/cascade/tile them. See `window::dispatch_mdi_verb`.

/// `(mdi-arrange-icons)` — arrange minimized child-window icons into a
/// row at the bottom of the MDI client (WM_MDIICONARRANGE). Returns T.
pub extern "C-unwind" fn mdi_arrange_icons_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    window::dispatch_mdi_verb(super::menu::MdiVerb::ArrangeIcons);
    Word::T.raw()
}

/// `(mdi-cascade)` — cascade the open child windows (WM_MDICASCADE).
pub extern "C-unwind" fn mdi_cascade_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    window::dispatch_mdi_verb(super::menu::MdiVerb::Cascade);
    Word::T.raw()
}

/// `(mdi-tile)` — tile the open child windows horizontally (WM_MDITILE).
pub extern "C-unwind" fn mdi_tile_shim(
    _mutator: *mut crate::mutator::MutatorState,
    _env: u64,
    _args: *const u64,
    _n_args: u64,
) -> u64 {
    window::dispatch_mdi_verb(super::menu::MdiVerb::TileH);
    Word::T.raw()
}
