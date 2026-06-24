//! Platform-neutral drawing IR for iGui.
//!
//! These types were factored out of `igui::batch` so that both the
//! Windows Direct2D executor (`igui::child::execute_d2d_batch`) and the
//! macOS Core Graphics renderer (`igui_mac`) consume the *same*
//! `SurfaceCmd` vocabulary. Nothing here is platform-specific: it is
//! pure geometry, colour, text, and path data. `igui::batch` re-exports
//! everything in this module (`pub use crate::igui_paint::*`), so the
//! existing Windows code paths (`super::batch::SurfaceCmd`, …) keep
//! resolving unchanged.
//!
//! See docs/PORTING_DESIGN.md §4.5 — `SurfaceCmd` is the cross-platform
//! drawing IR; the platform backends differ only in how they interpret it.

use std::sync::Arc;

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
/// text layout for results to agree, so all four commands carry
/// exactly the same fields.
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
