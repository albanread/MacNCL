//! Per-pane render state and the typed surface command enum.
//!
//! Phase 3b semantics:
//! - The CP-side batch builder is a thread-local `Vec<SurfaceCmd>` — CP
//!   calls `BeginBatch(childId)` / `Emit*` / `SubmitBatch` and the
//!   submit step hands off ownership to the per-pane "current" slot.
//! - The per-pane current batch is the latest fully-built batch for
//!   that child. WM_PAINT renders from it. Submitting a new batch
//!   replaces the previous one (newer sequence wins) and posts a
//!   `WM_PAINT` to the child via `InvalidateRect`.

#![cfg(windows)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::InvalidateRect;

use super::registry;

#[derive(Debug, Clone, Copy)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

// ─── Phase 5: marks, paths, strokes ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkMode {
    Highlight,
    Invert,
    Dim25,
    Dim50,
    Dim75,
}

#[derive(Debug, Clone)]
pub enum PathCmd {
    MoveTo(Point),
    LineTo(Point),
    QuadTo { ctrl: Point, end: Point },
    CubicTo { c1: Point, c2: Point, end: Point },
    /// Arc segment ending at `end`. Matches `D2D1_ARC_SEGMENT`
    /// fields one-to-one. `radius` is per-axis to support elliptical
    /// arcs; for a circular arc, use the same value for both.
    ArcTo {
        radius: Point,
        rotation_rad: f32,
        large_arc: bool,
        sweep_clockwise: bool,
        end: Point,
    },
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCap {
    Flat,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

#[derive(Debug, Clone)]
pub struct StrokeStyle {
    pub half_thickness: f32,
    pub line_cap: LineCap,
    pub line_join: LineJoin,
    pub miter_limit: f32,
    pub dash_pattern: Option<Vec<f32>>,
}

// ─── Phase 4: text descriptors ───────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Normal,
    Italic,
    Oblique,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontStretch {
    UltraCondensed,
    ExtraCondensed,
    Condensed,
    SemiCondensed,
    Normal,
    SemiExpanded,
    Expanded,
    ExtraExpanded,
    UltraExpanded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextAlign {
    Leading,
    Trailing,
    Center,
    Justified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextTrimming {
    None,
    EllipsisChar,
    EllipsisWord,
}

/// Full text-run descriptor passed across the CP / Rust boundary by
/// every text command (DrawTextRun + the three synchronous queries).
/// Draw, measure, and hit-test must resolve against the same
/// `IDWriteTextLayout` for results to agree, so all four commands
/// carry exactly the same fields.
#[derive(Debug, Clone)]
pub struct TextRun {
    pub text: String,
    pub origin: Point,
    pub family: String,
    pub size: f32,        // DIPs
    pub weight: u16,      // DWRITE_FONT_WEIGHT (100..900)
    pub style: FontStyle,
    pub stretch: FontStretch,
    pub locale: String,   // BCP-47, e.g. "en-us"
    pub color: Rgba,
    pub max_width: Option<f32>, // None = no wrap
    pub alignment: TextAlign,
    pub trimming: TextTrimming,
}

#[derive(Debug, Clone)]
pub enum SurfaceCmd {
    Clear {
        color: Rgba,
    },
    PresentHint,
    FillRect {
        rect: Rect,
        corner_radius: f32,
        color: Rgba,
    },
    StrokeRect {
        rect: Rect,
        corner_radius: f32,
        half_thickness: f32,
        color: Rgba,
    },
    DrawLine {
        p0: Point,
        p1: Point,
        half_thickness: f32,
        color: Rgba,
    },
    // ─── Phase 3c additions ────────────────────────────────────────
    FillOval {
        rect: Rect,
        color: Rgba,
    },
    FillCircle {
        center: Point,
        radius: f32,
        color: Rgba,
    },
    StrokeOval {
        rect: Rect,
        half_thickness: f32,
        color: Rgba,
    },
    StrokeCircle {
        center: Point,
        radius: f32,
        half_thickness: f32,
        color: Rgba,
    },
    DrawArc {
        center: Point,
        radius: f32,
        rotation_rad: f32,
        half_aperture_rad: f32,
        half_thickness: f32,
        color: Rgba,
    },
    // ─── Phase 4: text ─────────────────────────────────────────────
    DrawTextRun {
        run: TextRun,
    },
    /// GUI thread answers via `replies::deliver_metrics`, keyed on
    /// `request_id`. The originating CP call blocks on its reply slot.
    MeasureTextRun {
        request_id: u32,
        run: TextRun,
    },
    CharIndexAtPoint {
        request_id: u32,
        run: TextRun,
        point: Point,
    },
    PointAtCharIndex {
        request_id: u32,
        run: TextRun,
        char_index: u32,
    },
    // ─── Phase 5: composition + overlays + paths ───────────────────
    PushClipRect {
        rect: Rect,
    },
    PopClipRect,
    PushOffset {
        dx: f32,
        dy: f32,
    },
    PopOffset,
    ScrollRect {
        rect: Rect,
        dx: f32,
        dy: f32,
    },
    /// 8 transient slots per pane. SaveRect captures the pane's
    /// pixels under `rect` into `slot`; RestoreRect paints them back.
    SaveRect {
        slot: u8,
        rect: Rect,
    },
    RestoreRect {
        slot: u8,
    },
    InstallChildViewBounds {
        child_view_id: u32,
        rect: Rect,
    },
    MarkRect {
        rect: Rect,
        mode: MarkMode,
    },
    Caret {
        rect: Rect,
        color: Rgba,
    },
    SelectionRange {
        rect: Rect,
        color: Rgba,
    },
    FocusRing {
        rect: Rect,
        corner_radius: f32,
        half_thickness: f32,
        color: Rgba,
    },
    DrawPath {
        commands: Vec<PathCmd>,
        fill: Option<Rgba>,
        stroke: Option<(StrokeStyle, Rgba)>,
    },
    /// Blit a host-owned BGRA32 pixel buffer (one `u32` per pixel, in
    /// 0xAARRGGBB form / little-endian B,G,R,A bytes) at (x, y), sized
    /// `w`×`h`. This is the canvas fast path: a Lisp program pokes
    /// pixels directly into a host buffer (see `igui::canvas`) and the
    /// `present` step emits exactly one `Blit` per frame. `pixels` is an
    /// independent per-frame snapshot, so the GUI thread can own it for
    /// the batch's lifetime while Lisp keeps writing the live buffer.
    Blit {
        x: f32,
        y: f32,
        w: u32,
        h: u32,
        pixels: Arc<Vec<u32>>,
    },
}

#[derive(Debug, Clone)]
pub struct PaneBatch {
    pub child_id: i64,
    pub sequence: u64,
    pub flags: u32,
    pub cmds: Vec<SurfaceCmd>,
}

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_sequence() -> u64 {
    SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

// ─── Per-pane "current display batch" registry ────────────────────────

static PANE_STATES: Mutex<Option<HashMap<i64, Arc<PaneBatch>>>> = Mutex::new(None);

/// Hand `batch` to the GUI thread for child `child_id`. Replaces any
/// previously-submitted batch for the same child. Triggers a redraw by
/// invalidating the **render host** HWND (the borderless inner child
/// that owns the swap chain and WM_PAINT loop).
pub fn submit(batch: PaneBatch) -> bool {
    let child_id = batch.child_id;
    let arc = Arc::new(batch);
    {
        let mut guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
        let map = guard.get_or_insert_with(HashMap::new);
        map.insert(child_id, arc);
    }
    if let Some(hwnd) = registry::render_hwnd_of(child_id) {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        true
    } else {
        false
    }
}

pub fn snapshot(child_id: i64) -> Option<Arc<PaneBatch>> {
    let guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
    guard.as_ref().and_then(|m| m.get(&child_id).cloned())
}

/// Replace child `child_id`'s display batch with a single `Blit` of
/// `pixels` (w×h BGRA32) at the origin, and trigger a repaint. The
/// canvas `present` step calls this once per frame; `pixels` is a
/// fresh per-frame snapshot owned by the resulting batch.
pub fn present_pixels(child_id: i64, w: u32, h: u32, pixels: Arc<Vec<u32>>) -> bool {
    submit(PaneBatch {
        child_id,
        sequence: next_sequence(),
        flags: 0,
        cmds: vec![SurfaceCmd::Blit {
            x: 0.0,
            y: 0.0,
            w,
            h,
            pixels,
        }],
    })
}

#[allow(dead_code)] // used when child windows close
pub fn forget(child_id: i64) {
    let mut guard = PANE_STATES.lock().expect("PANE_STATES poisoned");
    if let Some(map) = guard.as_mut() {
        map.remove(&child_id);
    }
}

// ─── CP-thread batch builder ─────────────────────────────────────────

thread_local! {
    static CURRENT: RefCell<Option<PaneBatch>> = const { RefCell::new(None) };
}

pub fn begin(child_id: i64) {
    CURRENT.with(|slot| {
        *slot.borrow_mut() = Some(PaneBatch {
            child_id,
            sequence: next_sequence(),
            flags: 0,
            cmds: Vec::new(),
        });
    });
}

pub fn push(cmd: SurfaceCmd) -> bool {
    CURRENT.with(|slot| {
        if let Some(batch) = slot.borrow_mut().as_mut() {
            batch.cmds.push(cmd);
            true
        } else {
            false
        }
    })
}

pub fn finish() -> Option<PaneBatch> {
    CURRENT.with(|slot| slot.borrow_mut().take())
}

/// Restore an earlier in-progress batch into the thread-local
/// CURRENT slot. Used by `measure-text` so that calling it from
/// inside a `with-batch` doesn't clobber the user's draw work in
/// progress: take_current()-do-measure-restore-current(saved).
pub fn restore_current(saved: Option<PaneBatch>) {
    CURRENT.with(|slot| *slot.borrow_mut() = saved);
}

// ─── Phase 5: path builder ──────────────────────────────────────────

thread_local! {
    /// In-progress path commands accumulated by `path_*` calls until
    /// the matching `path_finish_*` call wraps them in a
    /// `SurfaceCmd::DrawPath` and pushes it onto the active batch.
    static PATH: RefCell<Vec<PathCmd>> = const { RefCell::new(Vec::new()) };
}

pub fn path_begin() {
    PATH.with(|c| c.borrow_mut().clear());
}

pub fn path_push(cmd: PathCmd) {
    PATH.with(|c| c.borrow_mut().push(cmd));
}

/// Take the current path command stream and emit a DrawPath into the
/// active batch with the given fill / stroke options.
pub fn path_finish(
    fill: Option<Rgba>,
    stroke: Option<(StrokeStyle, Rgba)>,
) -> bool {
    let commands = PATH.with(|c| std::mem::take(&mut *c.borrow_mut()));
    if commands.is_empty() {
        return false;
    }
    push(SurfaceCmd::DrawPath {
        commands,
        fill,
        stroke,
    })
}

#[allow(dead_code)] // used by the GUI thread when a child window closes
pub(crate) fn invalidate(hwnd: HWND) {
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
}
