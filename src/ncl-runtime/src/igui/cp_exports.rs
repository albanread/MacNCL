//! Native shims for iGui. Borrowed from the sister NewCP repo and
//! adapted: NewCP exposed these via its module-system's
//! NativeModuleArtifact; here we hold them as plain Rust functions
//! and the ncl-compiler crate wires them into Lisp symbol function
//! cells via `install_native`.
//!
//! The functions still use the same C-ABI signatures as in NewCP
//! (some take CP-style `(ptr, len)` open-array pairs) because the
//! work happens inside them — they call straight into the iGui
//! sub-modules (`window`, `channels`, `batch`, `dwrite`). Lisp-
//! facing wrappers with our Word-tagged ABI live in
//! ncl-runtime::abi alongside the other shim functions.

#![cfg(windows)]

use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use super::batch::{self as batch_mod, Point, Rect, Rgba, SurfaceCmd, TextRun};
use super::channels::{self, kind, IGuiEvent};
use super::dwrite as dwrite_mod;
use super::replies;

/// HWND of the iGui frame, set by `window::run` once the window
/// exists. Used by `iGui.Quit` to post WM_CLOSE.
pub static FRAME_HWND: OnceLock<isize> = OnceLock::new();

/// `iGui.NextEvent(VAR kind, childId, timeMs, p1, p2, p3, p4: INTEGER;
///                 timeoutMs: INTEGER): INTSHORT`.
///
/// Returns 1 if an event was delivered, 0 on timeout.
///
/// Field semantics by kind (all values written to the corresponding
/// VAR pointer):
///
/// | kind         | childId          | timeMs    | p1        | p2          | p3          | p4              |
/// |--------------|------------------|-----------|-----------|-------------|-------------|-----------------|
/// | EvKey        | child window id  | GetMsgTime| vkey      | scancode    | mods        | down(1)/up(0)\|repeat<<16 |
/// | EvChar       | child window id  | GetMsgTime| codepoint | mods        | 0           | 0               |
/// | EvMouse      | child window id  | GetMsgTime| x         | y           | mods\|button<<8\|op<<16 | wheel_delta\|wheel_lines<<16 |
/// | EvFocus      | child window id  | 0         | gained    | 0           | 0           | 0               |
/// | EvResize     | child window id  | 0         | width     | height      | 0           | 0               |
/// | EvClose      | child window id  | 0         | 0         | 0           | 0           | 0               |
/// | EvFrameClose | 0                | 0         | 0         | 0           | 0           | 0               |
/// | EvThemeChange| 0                | 0         | 0         | 0           | 0           | 0               |
/// | EvDpiChange  | child window id  | 0         | dpi_x×100 | dpi_y×100   | 0           | 0               |
/// | EvMenu       | 0                | 0         | menu_id   | item_id     | 0           | 0               |
#[unsafe(export_name = "iGui.NextEvent")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_next_event(
    out_kind: *mut i64,
    out_child: *mut i64,
    out_time: *mut i64,
    out_p1: *mut i64,
    out_p2: *mut i64,
    out_p3: *mut i64,
    out_p4: *mut i64,
    timeout_ms: i64,
) -> i32 {
    let Some(ev) = channels::next_event(timeout_ms) else {
        return 0;
    };
    write_event(ev, out_kind, out_child, out_time, out_p1, out_p2, out_p3, out_p4);
    1
}

/// `iGui.Quit`. Posts WM_CLOSE to the frame; the GUI thread tears down
/// in its own time.
#[unsafe(export_name = "iGui.Quit")]
pub extern "C" fn igui_quit() {
    if let Some(&hwnd_raw) = FRAME_HWND.get() {
        let hwnd = HWND(hwnd_raw as *mut _);
        let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

/// `iGui.OpenChild(title: ARRAY OF SHORTCHAR; VAR childId: INTEGER): INTSHORT`.
///
/// NewCP's ABI appends a hidden `$len: i64` argument after every open-
/// array pointer. We accept it here even though we scan for the
/// SHORTCHAR NUL terminator ourselves; ignoring it would shift all
/// later arguments by one register/stack slot.
#[unsafe(export_name = "iGui.OpenChild")]
pub extern "C" fn igui_open_child(
    title: *const u8,
    _title_len: i64,
    out_child: *mut i64,
) -> i32 {
    if title.is_null() || out_child.is_null() {
        return 0;
    }
    let title_str = unsafe { read_cp_shortstr(title) };
    match super::window::open_child(&title_str) {
        Some(id) => {
            unsafe { *out_child = id };
            1
        }
        None => 0,
    }
}

/// `iGui.CloseChild(childId: INTEGER): INTSHORT`. Returns 1 on success,
/// 0 if the child id is unknown.
#[unsafe(export_name = "iGui.CloseChild")]
pub extern "C" fn igui_close_child(child_id: i64) -> i32 {
    if super::window::close_child(child_id) {
        1
    } else {
        0
    }
}

/// `iGui.SetTitle(childId: INTEGER; title: ARRAY OF SHORTCHAR)`.
#[unsafe(export_name = "iGui.SetTitle")]
pub extern "C" fn igui_set_title(child_id: i64, title: *const u8, _title_len: i64) {
    if title.is_null() {
        return;
    }
    let title_str = unsafe { read_cp_shortstr(title) };
    super::window::set_child_title(child_id, &title_str);
}

// ─── Phase 3b: batch builder + first geometry primitives ─────────────

#[unsafe(export_name = "iGui.BeginBatch")]
pub extern "C" fn igui_begin_batch(child_id: i64) {
    batch_mod::begin(child_id);
}

#[unsafe(export_name = "iGui.SubmitBatch")]
pub extern "C" fn igui_submit_batch() -> i32 {
    let Some(batch) = batch_mod::finish() else {
        return 0;
    };
    if batch_mod::submit(batch) {
        1
    } else {
        0
    }
}

#[unsafe(export_name = "iGui.EmitClear")]
pub extern "C" fn igui_emit_clear(r: f64, g: f64, b: f64, a: f64) {
    eprintln!(
        "[igui-export] EmitClear rgba=({:.3}, {:.3}, {:.3}, {:.3})",
        r, g, b, a
    );
    batch_mod::push(SurfaceCmd::Clear {
        color: Rgba {
            r: r as f32,
            g: g as f32,
            b: b as f32,
            a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitPresentHint")]
pub extern "C" fn igui_emit_present_hint() {
    batch_mod::push(SurfaceCmd::PresentHint);
}

#[unsafe(export_name = "iGui.EmitFillRect")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_fill_rect(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    corner_radius: f64,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
) {
    eprintln!(
        "[igui-export] EmitFillRect rect=({:.1}, {:.1})-({:.1}, {:.1}) radius={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
        x0, y0, x1, y1, corner_radius, r, g, b, a
    );
    batch_mod::push(SurfaceCmd::FillRect {
        rect: Rect {
            x0: x0 as f32,
            y0: y0 as f32,
            x1: x1 as f32,
            y1: y1 as f32,
        },
        corner_radius: corner_radius as f32,
        color: Rgba {
            r: r as f32,
            g: g as f32,
            b: b as f32,
            a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitStrokeRect")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_stroke_rect(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    corner_radius: f64,
    half_thickness: f64,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
) {
    eprintln!(
        "[igui-export] EmitStrokeRect rect=({:.1}, {:.1})-({:.1}, {:.1}) radius={:.1} half_thickness={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
        x0, y0, x1, y1, corner_radius, half_thickness, r, g, b, a
    );
    batch_mod::push(SurfaceCmd::StrokeRect {
        rect: Rect {
            x0: x0 as f32,
            y0: y0 as f32,
            x1: x1 as f32,
            y1: y1 as f32,
        },
        corner_radius: corner_radius as f32,
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32,
            g: g as f32,
            b: b as f32,
            a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitDrawLine")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_draw_line(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    half_thickness: f64,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
) {
    eprintln!(
        "[igui-export] EmitDrawLine p0=({:.1}, {:.1}) p1=({:.1}, {:.1}) half_thickness={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
        x0, y0, x1, y1, half_thickness, r, g, b, a
    );
    batch_mod::push(SurfaceCmd::DrawLine {
        p0: Point {
            x: x0 as f32,
            y: y0 as f32,
        },
        p1: Point {
            x: x1 as f32,
            y: y1 as f32,
        },
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32,
            g: g as f32,
            b: b as f32,
            a: a as f32,
        },
    });
}

// ─── Phase 3c geometry primitives ────────────────────────────────────

#[unsafe(export_name = "iGui.EmitFillOval")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_fill_oval(
    x0: f64, y0: f64, x1: f64, y1: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::FillOval {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitFillCircle")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_fill_circle(
    cx: f64, cy: f64, radius: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::FillCircle {
        center: Point { x: cx as f32, y: cy as f32 },
        radius: radius as f32,
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitStrokeOval")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_stroke_oval(
    x0: f64, y0: f64, x1: f64, y1: f64,
    half_thickness: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::StrokeOval {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitStrokeCircle")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_stroke_circle(
    cx: f64, cy: f64, radius: f64,
    half_thickness: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::StrokeCircle {
        center: Point { x: cx as f32, y: cy as f32 },
        radius: radius as f32,
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitDrawArc")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_draw_arc(
    cx: f64, cy: f64, radius: f64,
    rotation_rad: f64, half_aperture_rad: f64,
    half_thickness: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::DrawArc {
        center: Point { x: cx as f32, y: cy as f32 },
        radius: radius as f32,
        rotation_rad: rotation_rad as f32,
        half_aperture_rad: half_aperture_rad as f32,
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

// ─── Diagnostics: DirectWrite layout cache stats ─────────────────────

/// `iGui.LayoutCacheStats(VAR hits, misses, size: INTEGER)`. Reads
/// the GUI thread's text-layout cache counters. Used by tests and
/// the log-view demo to confirm the cache is doing useful work.
/// Returns 1 always.
#[unsafe(export_name = "iGui.LayoutCacheStats")]
pub extern "C" fn igui_layout_cache_stats(
    out_hits: *mut i64,
    out_misses: *mut i64,
    out_size: *mut i64,
) -> i32 {
    let (hits, misses, size) = super::dwrite::layout_cache_stats();
    unsafe {
        if !out_hits.is_null() { *out_hits = hits as i64; }
        if !out_misses.is_null() { *out_misses = misses as i64; }
        if !out_size.is_null() { *out_size = size as i64; }
    }
    1
}

// ─── Animation tick (deferred design open-question #3) ───────────────

/// `iGui.SetRedrawRate(childId: INTEGER; intervalMs: INTEGER): INTSHORT`.
///
/// Schedules an `EvTick` event for `childId` every `intervalMs`
/// milliseconds. `intervalMs <= 0` disables the timer for that
/// child. Win32 auto-coalesces queued WM_TIMERs, so a backed-up
/// language thread sees at most one tick per drain cycle.
///
/// Returns 1 on success, 0 if `childId` is unknown.
#[unsafe(export_name = "iGui.SetRedrawRate")]
pub extern "C" fn igui_set_redraw_rate(child_id: i64, interval_ms: i64) -> i32 {
    if super::window::set_redraw_rate(child_id, interval_ms) {
        1
    } else {
        0
    }
}

// ─── Phase 6: menu + MDI verbs ───────────────────────────────────────

/// `iGui.SetMenu(spec: ARRAY OF SHORTCHAR): INTSHORT`. See
/// [`super::menu`] for the spec format.
#[unsafe(export_name = "iGui.SetMenu")]
pub extern "C" fn igui_set_menu(spec: *const u8, _spec_len: i64) -> i32 {
    let spec_str = unsafe { read_cp_shortstr(spec) };
    if super::window::set_menu(&spec_str) {
        1
    } else {
        0
    }
}

/// `iGui.MeasureFont(family, size, weight, italic; OUT ascent, descent,
///                   lineHeight, advanceM): INTSHORT`.
///
/// Synchronous, no childId, no batch — calls into `font_metrics`
/// directly. Returns 1 on success, 0 if DirectWrite refused the
/// typeface (caller should retry with a fallback family).
#[unsafe(export_name = "iGui.MeasureFont")]
pub extern "C" fn igui_measure_font(
    family: *const u8,
    _family_len: i64,
    size: f64,
    weight: i64,
    italic: i32,
    out_ascent: *mut f64,
    out_descent: *mut f64,
    out_line_height: *mut f64,
    out_advance_m: *mut f64,
) -> i32 {
    let family_str = unsafe { read_cp_shortstr(family) };
    let result = super::font_metrics::measure_font(
        &family_str,
        size as f32,
        weight as u16,
        italic != 0,
    );
    match result {
        Some(m) => {
            unsafe {
                if !out_ascent.is_null() {
                    *out_ascent = m.ascent as f64;
                }
                if !out_descent.is_null() {
                    *out_descent = m.descent as f64;
                }
                if !out_line_height.is_null() {
                    *out_line_height = m.line_height as f64;
                }
                if !out_advance_m.is_null() {
                    *out_advance_m = m.advance_m as f64;
                }
            }
            1
        }
        None => 0,
    }
}

/// `iGui.MeasureString(s, family, size, weight, italic; OUT width):
///                    INTSHORT`. Width is in DIPs.
#[unsafe(export_name = "iGui.MeasureString")]
pub extern "C" fn igui_measure_string(
    text: *const u8,
    _text_len: i64,
    family: *const u8,
    _family_len: i64,
    size: f64,
    weight: i64,
    italic: i32,
    out_width: *mut f64,
) -> i32 {
    let text_str = unsafe { read_cp_shortstr(text) };
    let family_str = unsafe { read_cp_shortstr(family) };
    let result = super::font_metrics::measure_string(
        &text_str,
        &family_str,
        size as f32,
        weight as u16,
        italic != 0,
    );
    match result {
        Some(w) => {
            unsafe {
                if !out_width.is_null() {
                    *out_width = w as f64;
                }
            }
            1
        }
        None => 0,
    }
}

/// `iGui.LogAppend(s: ARRAY OF SHORTCHAR)` — push one line into the
/// process-wide Rust log ring buffer. Identical adjacent lines
/// coalesce into a count badge instead of producing duplicate
/// entries. The log view (Tools > Log / Ctrl+Shift+L) repaints
/// automatically when it's open. Safe to call from any thread.
#[unsafe(export_name = "iGui.LogAppend")]
pub extern "C" fn igui_log_append(s: *const u8, _s_len: i64) {
    let line = unsafe { read_cp_shortstr(s) };
    super::log_view::append(&line);
}

#[unsafe(export_name = "iGui.MdiCascade")]
pub extern "C" fn igui_mdi_cascade() {
    super::window::dispatch_mdi_verb(super::menu::MdiVerb::Cascade);
}

#[unsafe(export_name = "iGui.MdiTileH")]
pub extern "C" fn igui_mdi_tile_h() {
    super::window::dispatch_mdi_verb(super::menu::MdiVerb::TileH);
}

#[unsafe(export_name = "iGui.MdiTileV")]
pub extern "C" fn igui_mdi_tile_v() {
    super::window::dispatch_mdi_verb(super::menu::MdiVerb::TileV);
}

#[unsafe(export_name = "iGui.MdiCloseAll")]
pub extern "C" fn igui_mdi_close_all() {
    super::window::dispatch_mdi_verb(super::menu::MdiVerb::CloseAll);
}

#[unsafe(export_name = "iGui.MdiArrangeIcons")]
pub extern "C" fn igui_mdi_arrange_icons() {
    super::window::dispatch_mdi_verb(super::menu::MdiVerb::ArrangeIcons);
}

// ─── Phase 5: composition + overlays + paths + system colors ─────

#[unsafe(export_name = "iGui.EmitPushClipRect")]
pub extern "C" fn igui_emit_push_clip_rect(x0: f64, y0: f64, x1: f64, y1: f64) {
    batch_mod::push(SurfaceCmd::PushClipRect {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitPopClipRect")]
pub extern "C" fn igui_emit_pop_clip_rect() {
    batch_mod::push(SurfaceCmd::PopClipRect);
}

#[unsafe(export_name = "iGui.EmitPushOffset")]
pub extern "C" fn igui_emit_push_offset(dx: f64, dy: f64) {
    batch_mod::push(SurfaceCmd::PushOffset {
        dx: dx as f32,
        dy: dy as f32,
    });
}

#[unsafe(export_name = "iGui.EmitPopOffset")]
pub extern "C" fn igui_emit_pop_offset() {
    batch_mod::push(SurfaceCmd::PopOffset);
}

#[unsafe(export_name = "iGui.EmitScrollRect")]
pub extern "C" fn igui_emit_scroll_rect(
    x0: f64, y0: f64, x1: f64, y1: f64, dx: f64, dy: f64,
) {
    batch_mod::push(SurfaceCmd::ScrollRect {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        dx: dx as f32,
        dy: dy as f32,
    });
}

#[unsafe(export_name = "iGui.EmitSaveRect")]
pub extern "C" fn igui_emit_save_rect(slot: i32, x0: f64, y0: f64, x1: f64, y1: f64) {
    batch_mod::push(SurfaceCmd::SaveRect {
        slot: slot.clamp(0, 7) as u8,
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitRestoreRect")]
pub extern "C" fn igui_emit_restore_rect(slot: i32) {
    batch_mod::push(SurfaceCmd::RestoreRect {
        slot: slot.clamp(0, 7) as u8,
    });
}

#[unsafe(export_name = "iGui.EmitInstallChildViewBounds")]
pub extern "C" fn igui_emit_install_child_view_bounds(
    child_view_id: i32, x0: f64, y0: f64, x1: f64, y1: f64,
) {
    batch_mod::push(SurfaceCmd::InstallChildViewBounds {
        child_view_id: child_view_id.max(0) as u32,
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitMarkRect")]
pub extern "C" fn igui_emit_mark_rect(x0: f64, y0: f64, x1: f64, y1: f64, mode: i32) {
    use batch_mod::MarkMode;
    let mode = match mode {
        1 => MarkMode::Invert,
        2 => MarkMode::Dim25,
        3 => MarkMode::Dim50,
        4 => MarkMode::Dim75,
        _ => MarkMode::Highlight,
    };
    batch_mod::push(SurfaceCmd::MarkRect {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        mode,
    });
}

#[unsafe(export_name = "iGui.EmitCaret")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_caret(
    x0: f64, y0: f64, x1: f64, y1: f64, r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::Caret {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitSelectionRange")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_selection_range(
    x0: f64, y0: f64, x1: f64, y1: f64, r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::SelectionRange {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

#[unsafe(export_name = "iGui.EmitFocusRing")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_focus_ring(
    x0: f64, y0: f64, x1: f64, y1: f64,
    corner_radius: f64, half_thickness: f64,
    r: f64, g: f64, b: f64, a: f64,
) {
    batch_mod::push(SurfaceCmd::FocusRing {
        rect: Rect {
            x0: x0 as f32, y0: y0 as f32, x1: x1 as f32, y1: y1 as f32,
        },
        corner_radius: corner_radius as f32,
        half_thickness: half_thickness as f32,
        color: Rgba {
            r: r as f32, g: g as f32, b: b as f32, a: a as f32,
        },
    });
}

// ─── Path builder ────────────────────────────────────────────────────

#[unsafe(export_name = "iGui.PathBegin")]
pub extern "C" fn igui_path_begin() {
    batch_mod::path_begin();
}

#[unsafe(export_name = "iGui.PathMoveTo")]
pub extern "C" fn igui_path_move_to(x: f64, y: f64) {
    batch_mod::path_push(batch_mod::PathCmd::MoveTo(batch_mod::Point {
        x: x as f32, y: y as f32,
    }));
}

#[unsafe(export_name = "iGui.PathLineTo")]
pub extern "C" fn igui_path_line_to(x: f64, y: f64) {
    batch_mod::path_push(batch_mod::PathCmd::LineTo(batch_mod::Point {
        x: x as f32, y: y as f32,
    }));
}

#[unsafe(export_name = "iGui.PathQuadTo")]
pub extern "C" fn igui_path_quad_to(cx: f64, cy: f64, ex: f64, ey: f64) {
    batch_mod::path_push(batch_mod::PathCmd::QuadTo {
        ctrl: batch_mod::Point { x: cx as f32, y: cy as f32 },
        end: batch_mod::Point { x: ex as f32, y: ey as f32 },
    });
}

#[unsafe(export_name = "iGui.PathCubicTo")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_path_cubic_to(
    c1x: f64, c1y: f64, c2x: f64, c2y: f64, ex: f64, ey: f64,
) {
    batch_mod::path_push(batch_mod::PathCmd::CubicTo {
        c1: batch_mod::Point { x: c1x as f32, y: c1y as f32 },
        c2: batch_mod::Point { x: c2x as f32, y: c2y as f32 },
        end: batch_mod::Point { x: ex as f32, y: ey as f32 },
    });
}

#[unsafe(export_name = "iGui.PathArcTo")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_path_arc_to(
    rx: f64, ry: f64, rotation_rad: f64,
    large_arc: i32, sweep_clockwise: i32,
    ex: f64, ey: f64,
) {
    batch_mod::path_push(batch_mod::PathCmd::ArcTo {
        radius: batch_mod::Point { x: rx as f32, y: ry as f32 },
        rotation_rad: rotation_rad as f32,
        large_arc: large_arc != 0,
        sweep_clockwise: sweep_clockwise != 0,
        end: batch_mod::Point { x: ex as f32, y: ey as f32 },
    });
}

#[unsafe(export_name = "iGui.PathClose")]
pub extern "C" fn igui_path_close() {
    batch_mod::path_push(batch_mod::PathCmd::Close);
}

/// Finish the current path and emit a DrawPath command into the
/// active batch. `fillMode` and `strokeMode` are 0/1 flags. When
/// stroking, the cap/join enums use the values defined in
/// Mod/iGui.cp (Cap*/Join*); dash pattern is unsupported in this
/// shim — pass any non-zero `dashLen` to enable a default
/// equal-segment dash pattern of length 4.
#[unsafe(export_name = "iGui.EmitPath")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_path(
    fill_mode: i32,
    fill_r: f64, fill_g: f64, fill_b: f64, fill_a: f64,
    stroke_mode: i32,
    stroke_half_thickness: f64,
    stroke_cap: i32,
    stroke_join: i32,
    stroke_miter: f64,
    stroke_dash_len: i32,
    stroke_r: f64, stroke_g: f64, stroke_b: f64, stroke_a: f64,
) -> i32 {
    use batch_mod::{LineCap, LineJoin, Rgba, StrokeStyle};
    let fill = if fill_mode != 0 {
        Some(Rgba {
            r: fill_r as f32, g: fill_g as f32, b: fill_b as f32, a: fill_a as f32,
        })
    } else {
        None
    };
    let stroke = if stroke_mode != 0 {
        let cap = match stroke_cap {
            1 => LineCap::Round,
            2 => LineCap::Square,
            _ => LineCap::Flat,
        };
        let join = match stroke_join {
            1 => LineJoin::Round,
            2 => LineJoin::Bevel,
            _ => LineJoin::Miter,
        };
        let dash = if stroke_dash_len > 0 {
            Some(vec![4.0, 4.0])
        } else {
            None
        };
        Some((
            StrokeStyle {
                half_thickness: stroke_half_thickness as f32,
                line_cap: cap,
                line_join: join,
                miter_limit: stroke_miter as f32,
                dash_pattern: dash,
            },
            Rgba {
                r: stroke_r as f32,
                g: stroke_g as f32,
                b: stroke_b as f32,
                a: stroke_a as f32,
            },
        ))
    } else {
        None
    };
    if batch_mod::path_finish(fill, stroke) { 1 } else { 0 }
}

// ─── System colors ───────────────────────────────────────────────────

#[unsafe(export_name = "iGui.SystemColor")]
pub extern "C" fn igui_system_color(
    kind: i32,
    out_r: *mut f64,
    out_g: *mut f64,
    out_b: *mut f64,
    out_a: *mut f64,
) -> i32 {
    let c = super::system_colors::lookup(kind);
    unsafe {
        if !out_r.is_null() { *out_r = c.r as f64 }
        if !out_g.is_null() { *out_g = c.g as f64 }
        if !out_b.is_null() { *out_b = c.b as f64 }
        if !out_a.is_null() { *out_a = c.a as f64 }
    }
    1
}

// ─── Phase 4: text ───────────────────────────────────────────────────

/// Build a `TextRun` from the wide list of CP-passed scalars.
/// Open-array params each carry a hidden `$len: i64` after the
/// pointer, matching the project's CP ABI rule. We scan to NUL
/// ourselves and treat the lengths as upper bounds.
#[allow(clippy::too_many_arguments)]
fn build_text_run(
    text: *const u8,
    origin_x: f64,
    origin_y: f64,
    family: *const u8,
    size: f64,
    weight: i32,
    style: i32,
    stretch: i32,
    locale: *const u8,
    color_r: f64,
    color_g: f64,
    color_b: f64,
    color_a: f64,
    max_width: f64,
    alignment: i32,
    trimming: i32,
) -> TextRun {
    let text_str = unsafe { read_cp_shortstr(text) };
    let family_str = unsafe { read_cp_shortstr(family) };
    let locale_str = unsafe { read_cp_shortstr(locale) };
    let max_width = if max_width > 0.0 {
        Some(max_width as f32)
    } else {
        None
    };
    TextRun {
        text: text_str,
        origin: Point {
            x: origin_x as f32,
            y: origin_y as f32,
        },
        family: if family_str.is_empty() {
            "Segoe UI".to_string()
        } else {
            family_str
        },
        size: size as f32,
        weight: weight.clamp(100, 900) as u16,
        style: dwrite_mod::cp_style(style),
        stretch: dwrite_mod::cp_stretch(stretch),
        locale: if locale_str.is_empty() {
            "en-us".to_string()
        } else {
            locale_str
        },
        color: Rgba {
            r: color_r as f32,
            g: color_g as f32,
            b: color_b as f32,
            a: color_a as f32,
        },
        max_width,
        alignment: dwrite_mod::cp_align(alignment),
        trimming: dwrite_mod::cp_trimming(trimming),
    }
}

/// `iGui.EmitDrawTextRun(text, x, y, fontSize, family, weight, style,
/// stretch, locale, maxWidth, alignment, trimming, r, g, b, a)`.
///
/// CP open arrays each contribute (`*const u8`, `i64` length); the
/// length is the buffer capacity, not the meaningful string length —
/// we still scan for NUL.
#[unsafe(export_name = "iGui.EmitDrawTextRun")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_emit_draw_text_run(
    text: *const u8,
    _text_len: i64,
    origin_x: f64,
    origin_y: f64,
    font_size: f64,
    family: *const u8,
    _family_len: i64,
    weight: i32,
    style: i32,
    stretch: i32,
    locale: *const u8,
    _locale_len: i64,
    max_width: f64,
    alignment: i32,
    trimming: i32,
    r: f64,
    g: f64,
    b: f64,
    a: f64,
) {
    let run = build_text_run(
        text, origin_x, origin_y, family, font_size, weight, style, stretch, locale,
        r, g, b, a, max_width, alignment, trimming,
    );
    batch_mod::push(SurfaceCmd::DrawTextRun { run });
}

/// `iGui.MeasureTextRun(childId, text, fontSize, family, weight, style,
/// stretch, locale, maxWidth, alignment, trimming,
/// VAR width, height, ascent: REAL; VAR lineCount: INTEGER): INTSHORT`.
///
/// Submits a measure batch for `child_id`, blocks up to 5s on the
/// reply channel. Returns 1 on success, 0 on failure / timeout.
#[unsafe(export_name = "iGui.MeasureTextRun")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_measure_text_run(
    child_id: i64,
    text: *const u8,
    _text_len: i64,
    font_size: f64,
    family: *const u8,
    _family_len: i64,
    weight: i32,
    style: i32,
    stretch: i32,
    locale: *const u8,
    _locale_len: i64,
    max_width: f64,
    alignment: i32,
    trimming: i32,
    out_width: *mut f64,
    out_height: *mut f64,
    out_ascent: *mut f64,
    out_line_count: *mut i64,
) -> i32 {
    let run = build_text_run(
        text, 0.0, 0.0, family, font_size, weight, style, stretch, locale,
        0.0, 0.0, 0.0, 1.0, max_width, alignment, trimming,
    );
    let request_id = replies::alloc_id();
    let rx = replies::install(request_id);
    batch_mod::begin(child_id);
    batch_mod::push(SurfaceCmd::MeasureTextRun {
        request_id,
        run,
    });
    if batch_mod::finish().and_then(|b| {
        // Submit the batch directly without going through submit() so
        // we don't lose the ordering with a concurrent draw batch.
        Some(batch_mod::submit(b))
    }) != Some(true)
    {
        return 0;
    }
    match replies::wait(rx) {
        Some(replies::Reply::Metrics {
            width,
            height,
            ascent,
            line_count,
        }) => {
            unsafe {
                if !out_width.is_null() { *out_width = width as f64 }
                if !out_height.is_null() { *out_height = height as f64 }
                if !out_ascent.is_null() { *out_ascent = ascent as f64 }
                if !out_line_count.is_null() { *out_line_count = line_count as i64 }
            }
            1
        }
        _ => 0,
    }
}

/// `iGui.CharIndexAtPoint(childId, text, fontSize, family, weight, style,
/// stretch, locale, x, y,
/// VAR charIndex: INTEGER; VAR isInside, isTrailing: INTSHORT): INTSHORT`.
#[unsafe(export_name = "iGui.CharIndexAtPoint")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_char_index_at_point(
    child_id: i64,
    text: *const u8,
    _text_len: i64,
    font_size: f64,
    family: *const u8,
    _family_len: i64,
    weight: i32,
    style: i32,
    stretch: i32,
    locale: *const u8,
    _locale_len: i64,
    x: f64,
    y: f64,
    out_char_index: *mut i64,
    out_is_inside: *mut i32,
    out_is_trailing: *mut i32,
) -> i32 {
    let run = build_text_run(
        text, 0.0, 0.0, family, font_size, weight, style, stretch, locale,
        0.0, 0.0, 0.0, 1.0, -1.0, 0, 0,
    );
    let request_id = replies::alloc_id();
    let rx = replies::install(request_id);
    batch_mod::begin(child_id);
    batch_mod::push(SurfaceCmd::CharIndexAtPoint {
        request_id,
        run,
        point: Point {
            x: x as f32,
            y: y as f32,
        },
    });
    if batch_mod::finish().map(batch_mod::submit) != Some(true) {
        return 0;
    }
    match replies::wait(rx) {
        Some(replies::Reply::HitTestPoint {
            char_index,
            is_inside,
            is_trailing,
        }) => {
            unsafe {
                if !out_char_index.is_null() { *out_char_index = char_index as i64 }
                if !out_is_inside.is_null() { *out_is_inside = if is_inside { 1 } else { 0 } }
                if !out_is_trailing.is_null() { *out_is_trailing = if is_trailing { 1 } else { 0 } }
            }
            1
        }
        _ => 0,
    }
}

/// `iGui.PointAtCharIndex(childId, text, fontSize, family, weight, style,
/// charIndex, VAR x, y, height: REAL): INTSHORT`.
#[unsafe(export_name = "iGui.PointAtCharIndex")]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn igui_point_at_char_index(
    child_id: i64,
    text: *const u8,
    _text_len: i64,
    font_size: f64,
    family: *const u8,
    _family_len: i64,
    weight: i32,
    style: i32,
    char_index: i64,
    out_x: *mut f64,
    out_y: *mut f64,
    out_height: *mut f64,
) -> i32 {
    let run = build_text_run(
        text, 0.0, 0.0, family, font_size, weight, style, 5, std::ptr::null(),
        0.0, 0.0, 0.0, 1.0, -1.0, 0, 0,
    );
    let request_id = replies::alloc_id();
    let rx = replies::install(request_id);
    batch_mod::begin(child_id);
    batch_mod::push(SurfaceCmd::PointAtCharIndex {
        request_id,
        run,
        char_index: char_index.max(0) as u32,
    });
    if batch_mod::finish().map(batch_mod::submit) != Some(true) {
        return 0;
    }
    match replies::wait(rx) {
        Some(replies::Reply::HitTestPosition { x, y, height }) => {
            unsafe {
                if !out_x.is_null() { *out_x = x as f64 }
                if !out_y.is_null() { *out_y = y as f64 }
                if !out_height.is_null() { *out_height = height as f64 }
            }
            1
        }
        _ => 0,
    }
}

// ─── Phase 3c: DPI + cursor ──────────────────────────────────────────

/// `iGui.GetDpi(childId: INTEGER; VAR dpiX, dpiY: REAL): INTSHORT`.
/// Returns 1 on success, 0 if the child id is unknown.
#[unsafe(export_name = "iGui.GetDpi")]
pub extern "C" fn igui_get_dpi(
    child_id: i64,
    out_dpi_x: *mut f64,
    out_dpi_y: *mut f64,
) -> i32 {
    if out_dpi_x.is_null() || out_dpi_y.is_null() {
        return 0;
    }
    match super::cursor::get_dpi(child_id) {
        Some((x, y)) => {
            unsafe {
                *out_dpi_x = x as f64;
                *out_dpi_y = y as f64;
            }
            1
        }
        None => 0,
    }
}

/// `iGui.SetCursor(childId: INTEGER; kind: INTSHORT)`.
#[unsafe(export_name = "iGui.SetCursor")]
pub extern "C" fn igui_set_cursor(child_id: i64, kind: i32) {
    super::cursor::set_kind(child_id, kind);
}

/// CP `ARRAY OF SHORTCHAR` is passed as a bare pointer to a sequence
/// of bytes terminated by `0X`. This helper reads up to 4096 bytes,
/// stops at the first NUL, and returns the lossy UTF-8 decoding.
/// Null pointer returns the empty string so internal callers that
/// substitute defaults can pass `null()` safely.
unsafe fn read_cp_shortstr(ptr: *const u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    const MAX: usize = 4096;
    let mut len = 0usize;
    while len < MAX {
        let b = unsafe { *ptr.add(len) };
        if b == 0 {
            break;
        }
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(slice).into_owned()
}

#[allow(clippy::too_many_arguments)]
#[allow(unused_assignments)] // initial defaults overwritten by every match arm
fn write_event(
    ev: IGuiEvent,
    out_kind: *mut i64,
    out_child: *mut i64,
    out_time: *mut i64,
    out_p1: *mut i64,
    out_p2: *mut i64,
    out_p3: *mut i64,
    out_p4: *mut i64,
) {
    let mut k = kind::NONE;
    let mut child = 0i64;
    let mut t = 0i64;
    let mut p1 = 0i64;
    let mut p2 = 0i64;
    let mut p3 = 0i64;
    let mut p4 = 0i64;

    match ev {
        IGuiEvent::Key {
            child_id,
            vkey,
            scancode,
            mods,
            repeat,
            down,
            time_ms,
        } => {
            k = kind::KEY;
            child = child_id;
            t = time_ms;
            p1 = vkey;
            p2 = scancode;
            p3 = mods;
            p4 = (if down { 1 } else { 0 }) | (repeat << 16);
        }
        IGuiEvent::Char {
            child_id,
            codepoint,
            mods,
            time_ms,
        } => {
            k = kind::CHAR;
            child = child_id;
            t = time_ms;
            p1 = codepoint;
            p2 = mods;
        }
        IGuiEvent::Mouse {
            child_id,
            x,
            y,
            op,
            button,
            mods,
            wheel_delta,
            wheel_lines,
            time_ms,
        } => {
            k = kind::MOUSE;
            child = child_id;
            t = time_ms;
            p1 = x;
            p2 = y;
            p3 = mods | (button << 8) | (op << 16);
            p4 = (wheel_delta & 0xFFFF) | (wheel_lines << 16);
        }
        IGuiEvent::Focus { child_id, gained } => {
            k = kind::FOCUS;
            child = child_id;
            p1 = if gained { 1 } else { 0 };
        }
        IGuiEvent::Resize {
            child_id,
            width,
            height,
        } => {
            k = kind::RESIZE;
            child = child_id;
            p1 = width;
            p2 = height;
        }
        IGuiEvent::Close { child_id } => {
            k = kind::CLOSE;
            child = child_id;
        }
        IGuiEvent::FrameClose => {
            k = kind::FRAME_CLOSE;
        }
        IGuiEvent::ThemeChange => {
            k = kind::THEME_CHANGE;
        }
        IGuiEvent::DpiChange {
            child_id,
            dpi_x,
            dpi_y,
        } => {
            k = kind::DPI_CHANGE;
            child = child_id;
            p1 = dpi_x;
            p2 = dpi_y;
        }
        IGuiEvent::Menu { menu_id, item_id } => {
            k = kind::MENU;
            p1 = menu_id;
            p2 = item_id;
        }
        IGuiEvent::Tick { child_id, time_ms } => {
            k = kind::TICK;
            child = child_id;
            t = time_ms;
        }
        IGuiEvent::EvalBuffer { .. } => {
            // The C-export ABI doesn't carry the source string —
            // EvalBuffer is consumed via the Rust-native path
            // (event_to_plist allocates a Lisp string). C consumers
            // see only the kind tag.
            k = kind::EVAL_BUFFER;
        }
        IGuiEvent::ReplSubmit { child_id } => {
            k = kind::REPL_SUBMIT;
            child = child_id;
        }
    }

    unsafe {
        if !out_kind.is_null() {
            *out_kind = k;
        }
        if !out_child.is_null() {
            *out_child = child;
        }
        if !out_time.is_null() {
            *out_time = t;
        }
        if !out_p1.is_null() {
            *out_p1 = p1;
        }
        if !out_p2.is_null() {
            *out_p2 = p2;
        }
        if !out_p3.is_null() {
            *out_p3 = p3;
        }
        if !out_p4.is_null() {
            *out_p4 = p4;
        }
    }
}

