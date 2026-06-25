//! macOS native Lisp shims for iGui — the bridge that lets Lisp drive the
//! Cocoa side windows.
//!
//! These mirror the Windows shims in `igui::lisp_shims` (same names, same
//! `extern "C-unwind"` ABI), but route window management through
//! `igui_mac::window` (one `NSWindow` per child) and drawing through a
//! thread-local `SurfaceCmd` batch builder that presents to the right
//! window. The arg-decoding helpers and `event_to_plist` are ported from
//! the Windows shims (that logic is platform-neutral). Installed by
//! `ncl-compiler`'s `install_igui` on macOS.

use std::cell::RefCell;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::gc_string;
use crate::igui_events::{self, mouse_op, IGuiEvent};
use crate::igui_mac::render::CgCanvas;
use crate::igui_mac::window;
use crate::igui_paint::{
    FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
};
use crate::mutator::{GcCoordinator, MutatorState};
use crate::word::{Tag, Word};

// ── arg decoding (ported, platform-neutral) ───────────────────────────

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
fn kw(coord: &GcCoordinator, name: &str) -> Word {
    let mut buf = String::with_capacity(name.len() + 1);
    buf.push(':');
    buf.push_str(name);
    coord.intern(&buf)
}
fn unpack_rgba(packed: i64) -> Rgba {
    let bits = packed as u64;
    Rgba {
        r: ((bits >> 24) & 0xFF) as f32 / 255.0,
        g: ((bits >> 16) & 0xFF) as f32 / 255.0,
        b: ((bits >> 8) & 0xFF) as f32 / 255.0,
        a: (bits & 0xFF) as f32 / 255.0,
    }
}
fn mouse_op_name(op: i64) -> &'static str {
    match op {
        mouse_op::LEFT_DOWN => "LEFT-DOWN",
        mouse_op::LEFT_UP => "LEFT-UP",
        mouse_op::RIGHT_DOWN => "RIGHT-DOWN",
        mouse_op::RIGHT_UP => "RIGHT-UP",
        mouse_op::MIDDLE_DOWN => "MIDDLE-DOWN",
        mouse_op::MIDDLE_UP => "MIDDLE-UP",
        mouse_op::WHEEL => "WHEEL",
        mouse_op::DRAG => "DRAG",
        _ => "MOVE",
    }
}

// ── child-id allocation + thread-local batch builder ──────────────────

static NEXT_CHILD: AtomicI64 = AtomicI64::new(2); // 1 = IDE/main window

fn alloc_child_id() -> i64 {
    NEXT_CHILD.fetch_add(1, Ordering::Relaxed)
}

#[derive(Default)]
struct Builder {
    child_id: i64,
    cmds: Vec<SurfaceCmd>,
}
thread_local! {
    static BUILDER: RefCell<Builder> = RefCell::new(Builder::default());
}
fn batch_begin(id: i64) {
    BUILDER.with(|b| {
        let mut b = b.borrow_mut();
        b.child_id = id;
        b.cmds.clear();
    });
}
fn batch_push(cmd: SurfaceCmd) {
    BUILDER.with(|b| b.borrow_mut().cmds.push(cmd));
}
fn batch_submit() -> bool {
    BUILDER.with(|b| {
        let mut b = b.borrow_mut();
        if b.child_id == 0 {
            return false;
        }
        let id = b.child_id;
        let cmds = std::mem::take(&mut b.cmds);
        window::present(id, cmds);
        true
    })
}

// ── lifecycle / window management ─────────────────────────────────────

macro_rules! shim {
    ($name:ident, $body:expr) => {
        pub extern "C-unwind" fn $name(
            _mutator: *mut MutatorState,
            _env: u64,
            args: *const u64,
            n_args: u64,
        ) -> u64 {
            let _ = (args, n_args);
            $body
        }
    };
}

/// `(igui-start)` — on macOS the AppKit app is already running (the driver
/// brought it up on the main thread before the worker), so this just
/// confirms readiness.
shim!(igui_start_shim, Word::T.raw());
shim!(igui_wait_shim, Word::T.raw());
shim!(igui_quit_shim, {
    window::close_window(window::MAIN_ID);
    Word::T.raw()
});

const DEFAULT_W: f64 = 480.0;
const DEFAULT_H: f64 = 360.0;

pub extern "C-unwind" fn open_child_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 1 {
        panic!("open-child: expected 1 arg (title), got {n}");
    }
    let title = arg_string(args, 0).unwrap_or_default();
    let id = alloc_child_id();
    window::open_window(id, DEFAULT_W, DEFAULT_H, &title);
    Word::fixnum(id).raw()
}

pub extern "C-unwind" fn open_child_sized_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 3 {
        panic!("open-child-sized: expected 3 args, got {n}");
    }
    let title = arg_string(args, 0).unwrap_or_default();
    let w = arg_fixnum(args, 1).filter(|v| *v > 0).map(|v| v as f64).unwrap_or(DEFAULT_W);
    let h = arg_fixnum(args, 2).filter(|v| *v > 0).map(|v| v as f64).unwrap_or(DEFAULT_H);
    let id = alloc_child_id();
    window::open_window(id, w, h, &title);
    Word::fixnum(id).raw()
}

pub extern "C-unwind" fn close_child_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 1 {
        panic!("close-child: expected 1 arg, got {n}");
    }
    match arg_fixnum(args, 0) {
        Some(id) => {
            window::close_window(id);
            Word::T.raw()
        }
        None => Word::NIL.raw(),
    }
}

pub extern "C-unwind" fn set_title_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 2 {
        panic!("set-title: expected 2 args, got {n}");
    }
    if let (Some(id), Some(title)) = (arg_fixnum(args, 0), arg_string(args, 1)) {
        window::set_window_title(id, &title);
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
}

/// `(set-redraw-rate child-id ms)` — schedule `:TICK` events for the child
/// every `ms` milliseconds. Spawns a timer thread that posts ticks into the
/// mailbox.
pub extern "C-unwind" fn set_redraw_rate_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 2 {
        panic!("set-redraw-rate: expected 2 args, got {n}");
    }
    let (Some(id), Some(ms)) = (arg_fixnum(args, 0), arg_fixnum(args, 1)) else {
        return Word::NIL.raw();
    };
    if ms <= 0 {
        return Word::NIL.raw();
    }
    std::thread::Builder::new()
        .name(format!("igui-tick-{id}"))
        .spawn(move || {
            let dur = std::time::Duration::from_millis(ms as u64);
            loop {
                std::thread::sleep(dur);
                igui_events::push(IGuiEvent::Tick { child_id: id, time_ms: 0 });
            }
        })
        .ok();
    Word::T.raw()
}

// ── events ─────────────────────────────────────────────────────────────

fn event_to_plist(m: &mut MutatorState, coord: &GcCoordinator, ev: IGuiEvent) -> Word {
    let mut pairs: Vec<(Word, Word)> = Vec::new();
    match ev {
        IGuiEvent::Key { child_id, vkey, scancode, mods, repeat, down, time_ms } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "KEY")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            pairs.push((kw(coord, "VKEY"), Word::fixnum(vkey)));
            pairs.push((kw(coord, "SCANCODE"), Word::fixnum(scancode)));
            pairs.push((kw(coord, "MODS"), Word::fixnum(mods)));
            pairs.push((kw(coord, "REPEAT"), Word::fixnum(repeat)));
            pairs.push((kw(coord, "DOWN"), if down { Word::T } else { Word::NIL }));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::Char { child_id, codepoint, mods, time_ms } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "CHAR")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
            let ch = char::from_u32(codepoint as u32).map(Word::char).unwrap_or(Word::NIL);
            pairs.push((kw(coord, "CHAR"), ch));
            pairs.push((kw(coord, "CODEPOINT"), Word::fixnum(codepoint)));
            pairs.push((kw(coord, "MODS"), Word::fixnum(mods)));
            pairs.push((kw(coord, "TIME-MS"), Word::fixnum(time_ms)));
        }
        IGuiEvent::Mouse { child_id, x, y, op, button, mods, wheel_delta, wheel_lines, time_ms } => {
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
        IGuiEvent::FrameClose => pairs.push((kw(coord, "KIND"), kw(coord, "FRAME-CLOSE"))),
        IGuiEvent::ThemeChange => pairs.push((kw(coord, "KIND"), kw(coord, "THEME-CHANGE"))),
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
            let s = gc_string::alloc_string_in_young(m, source.as_str());
            pairs.push((kw(coord, "SOURCE"), s));
        }
        IGuiEvent::ReplSubmit { child_id } => {
            pairs.push((kw(coord, "KIND"), kw(coord, "REPL-SUBMIT")));
            pairs.push((kw(coord, "CHILD-ID"), Word::fixnum(child_id)));
        }
    }
    let mut acc = Word::NIL;
    for (k, v) in pairs.into_iter().rev() {
        acc = m.alloc_cons(v, acc);
        acc = m.alloc_cons(k, acc);
    }
    acc
}

pub extern "C-unwind" fn next_event_shim(
    mutator: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 1 {
        panic!("next-event: expected 1 arg, got {n}");
    }
    let timeout = arg_fixnum(args, 0).unwrap_or(-1);
    let m = unsafe { &mut *mutator };
    let do_park = timeout != 0;
    if do_park {
        m.enter_blocked();
    }
    let ev_opt = igui_events::next_event(timeout);
    if do_park {
        m.leave_blocked();
    }
    let Some(ev) = ev_opt else { return Word::NIL.raw() };
    let coord = std::sync::Arc::clone(m.coord());
    event_to_plist(m, &coord, ev).raw()
}

pub extern "C-unwind" fn next_event_for_shim(
    mutator: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 2 {
        panic!("next-event-for: expected 2 args, got {n}");
    }
    let (Some(id), Some(timeout)) = (arg_fixnum(args, 0), arg_fixnum(args, 1)) else {
        panic!("next-event-for: child-id and timeout must be fixnums");
    };
    let m = unsafe { &mut *mutator };
    let do_park = timeout != 0;
    if do_park {
        m.enter_blocked();
    }
    let ev_opt = igui_events::next_event_for(id, timeout);
    if do_park {
        m.leave_blocked();
    }
    let Some(ev) = ev_opt else { return Word::NIL.raw() };
    let coord = std::sync::Arc::clone(m.coord());
    event_to_plist(m, &coord, ev).raw()
}

pub extern "C-unwind" fn filter_on_window_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n == 1 {
        if let Some(id) = arg_fixnum(args, 0) {
            igui_events::filter_on_window(id);
        }
    }
    Word::T.raw()
}
pub extern "C-unwind" fn unfilter_window_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n == 1 {
        if let Some(id) = arg_fixnum(args, 0) {
            igui_events::unfilter_window(id);
        }
    }
    Word::T.raw()
}
shim!(clear_event_filter_shim, {
    igui_events::clear_filter();
    Word::T.raw()
});
shim!(discard_stashed_events_shim, {
    igui_events::discard_stashed_events();
    Word::T.raw()
});

// ── batch builder + drawing primitives ────────────────────────────────

pub extern "C-unwind" fn begin_batch_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 1 {
        panic!("%begin-batch: expected 1 arg, got {n}");
    }
    match arg_fixnum(args, 0) {
        Some(id) => {
            batch_begin(id);
            Word::T.raw()
        }
        None => Word::NIL.raw(),
    }
}
shim!(submit_batch_shim, {
    if batch_submit() {
        Word::T.raw()
    } else {
        Word::NIL.raw()
    }
});

fn fx(args: *const u64, i: u64) -> f32 {
    arg_fixnum(args, i).unwrap_or(0) as f32
}

pub extern "C-unwind" fn emit_clear_shim(
    _m: *mut MutatorState,
    _e: u64,
    args: *const u64,
    n: u64,
) -> u64 {
    if n != 1 {
        panic!("%emit-clear: 1 arg");
    }
    batch_push(SurfaceCmd::Clear { color: unpack_rgba(arg_fixnum(args, 0).unwrap_or(0)) });
    Word::T.raw()
}

fn rect_xywh(args: *const u64) -> Rect {
    let (x, y, w, h) = (fx(args, 0), fx(args, 1), fx(args, 2), fx(args, 3));
    Rect { x0: x, y0: y, x1: x + w, y1: y + h }
}

pub extern "C-unwind" fn emit_fill_rect_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 5 { panic!("%emit-fill-rect: 5 args"); }
    batch_push(SurfaceCmd::FillRect { rect: rect_xywh(args), corner_radius: 0.0, color: unpack_rgba(arg_fixnum(args, 4).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_stroke_rect_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 6 { panic!("%emit-stroke-rect: 6 args"); }
    batch_push(SurfaceCmd::StrokeRect { rect: rect_xywh(args), corner_radius: 0.0, half_thickness: fx(args, 4) * 0.5, color: unpack_rgba(arg_fixnum(args, 5).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_fill_oval_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 5 { panic!("%emit-fill-oval: 5 args"); }
    batch_push(SurfaceCmd::FillOval { rect: rect_xywh(args), color: unpack_rgba(arg_fixnum(args, 4).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_stroke_oval_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 6 { panic!("%emit-stroke-oval: 6 args"); }
    batch_push(SurfaceCmd::StrokeOval { rect: rect_xywh(args), half_thickness: fx(args, 4) * 0.5, color: unpack_rgba(arg_fixnum(args, 5).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_fill_circle_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 4 { panic!("%emit-fill-circle: 4 args"); }
    batch_push(SurfaceCmd::FillCircle { center: Point { x: fx(args, 0), y: fx(args, 1) }, radius: fx(args, 2), color: unpack_rgba(arg_fixnum(args, 3).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_stroke_circle_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 5 { panic!("%emit-stroke-circle: 5 args"); }
    batch_push(SurfaceCmd::StrokeCircle { center: Point { x: fx(args, 0), y: fx(args, 1) }, radius: fx(args, 2), half_thickness: fx(args, 3) * 0.5, color: unpack_rgba(arg_fixnum(args, 4).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_draw_line_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 6 { panic!("%emit-draw-line: 6 args"); }
    batch_push(SurfaceCmd::DrawLine { p0: Point { x: fx(args, 0), y: fx(args, 1) }, p1: Point { x: fx(args, 2), y: fx(args, 3) }, half_thickness: fx(args, 4) * 0.5, color: unpack_rgba(arg_fixnum(args, 5).unwrap_or(0)) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_draw_arc_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 7 { panic!("%emit-draw-arc: 7 args"); }
    // x y radius start-deg sweep-deg thickness color  → center/rotation/aperture
    let cx = fx(args, 0); let cy = fx(args, 1); let r = fx(args, 2);
    let start = fx(args, 3).to_radians(); let sweep = fx(args, 4).to_radians();
    batch_push(SurfaceCmd::DrawArc { center: Point { x: cx, y: cy }, radius: r, rotation_rad: start + sweep * 0.5, half_aperture_rad: (sweep * 0.5).abs(), half_thickness: fx(args, 5) * 0.5, color: unpack_rgba(arg_fixnum(args, 6).unwrap_or(0)) });
    Word::T.raw()
}

fn text_run(text: String, x: f32, y: f32, size: f32, family: &str, weight: u16, color: Rgba) -> TextRun {
    TextRun {
        text, origin: Point { x, y }, family: family.into(), size, weight,
        style: FontStyle::Normal, stretch: FontStretch::Normal, locale: "en-us".into(),
        color, max_width: None, alignment: TextAlign::Leading, trimming: TextTrimming::None,
    }
}

pub extern "C-unwind" fn emit_draw_text_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 5 { panic!("%emit-draw-text: 5 args (x y text size color)"); }
    let text = arg_string(args, 2).unwrap_or_default();
    batch_push(SurfaceCmd::DrawTextRun { run: text_run(text, fx(args, 0), fx(args, 1), fx(args, 3), "Helvetica", 400, unpack_rgba(arg_fixnum(args, 4).unwrap_or(0))) });
    Word::T.raw()
}
pub extern "C-unwind" fn emit_draw_text_styled_shim(_m: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    // x y text size weight color  (simplified: ignore the styled plist for now)
    if n < 5 { panic!("%emit-draw-text-styled: >=5 args"); }
    let text = arg_string(args, 2).unwrap_or_default();
    let color_idx = if n >= 6 { 5 } else { 4 };
    batch_push(SurfaceCmd::DrawTextRun { run: text_run(text, fx(args, 0), fx(args, 1), fx(args, 3), "Helvetica", 400, unpack_rgba(arg_fixnum(args, color_idx).unwrap_or(0))) });
    Word::T.raw()
}

/// `(%measure-text text family size weight)` → plist (:width :height :ascent).
pub extern "C-unwind" fn measure_text_shim(mutator: *mut MutatorState, _e: u64, args: *const u64, n: u64) -> u64 {
    if n != 4 { panic!("%measure-text: 4 args"); }
    let text = arg_string(args, 0).unwrap_or_default();
    let family = arg_string(args, 1).unwrap_or_else(|| "Helvetica".into());
    let size = fx(args, 2);
    let m = unsafe { &mut *mutator };
    let coord = std::sync::Arc::clone(m.coord());
    let run = text_run(text, 0.0, 0.0, size, &family, 400, Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 });
    let (w, h, asc) = match CgCanvas::measure_text_run(&run) {
        Some(tm) => (tm.width, tm.height, tm.ascent),
        None => (0.0, size, size),
    };
    let pairs = [
        (kw(&coord, "WIDTH"), Word::fixnum(w as i64)),
        (kw(&coord, "HEIGHT"), Word::fixnum(h as i64)),
        (kw(&coord, "ASCENT"), Word::fixnum(asc as i64)),
    ];
    let mut acc = Word::NIL;
    for (k, v) in pairs.into_iter().rev() {
        acc = m.alloc_cons(v, acc);
        acc = m.alloc_cons(k, acc);
    }
    acc.raw()
}
