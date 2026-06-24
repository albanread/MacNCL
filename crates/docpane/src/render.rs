//! Direct2D interpreter for the markdown draw list.
//!
//! Lifted (de-chromed) from DocCrate's `render.rs`: the part that walks
//! a `Layout`'s `Vec<DrawCmd>` and issues Direct2D calls into a render
//! target.  Everything DocCrate's `render.rs` wrapped around it —
//! window, sidebar, tabs, scrollbar, find panel, navigation, the event
//! loop — stays behind, because the host (igui's doc-pane, or the
//! standalone exe) provides chrome.  This module is pure rendering:
//! hand it a target, a laid-out document, and a scroll offset.
//!
//! Factories live in process-wide `OnceLock`s; a front-end calls
//! [`init`] once before any draw or measure.  Text formats and
//! measurements are cached per thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use windows::core::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::Vector2;

use crate::layout::{DrawCmd, Layout};
use crate::{mermaid, theme};

// ── process-wide factory singletons ──────────────────────────────────
static G_D2D: OnceLock<ID2D1Factory1> = OnceLock::new();
static G_DW: OnceLock<IDWriteFactory2> = OnceLock::new();

/// Create the Direct2D / DirectWrite factories.  Idempotent — safe to
/// call from each front-end's startup; the first call wins.  Must run
/// before [`draw_document`] or [`measure_text`].
pub fn init() -> Result<()> {
    if G_D2D.get().is_some() {
        return Ok(());
    }
    unsafe {
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dw: IDWriteFactory2 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let _ = G_D2D.set(d2d);
        let _ = G_DW.set(dw);
    }
    Ok(())
}

fn d2d() -> &'static ID2D1Factory1 {
    G_D2D.get().expect("docpane::render::init() not called")
}

/// The shared Direct2D factory.  A host needs it to create its own
/// render target — an `ID2D1HwndRenderTarget` for a live pane, or an
/// `ID2D1RenderTarget` over a WIC bitmap for an offscreen snapshot.
/// Call [`init`] first.
pub fn factory() -> &'static ID2D1Factory1 {
    d2d()
}
fn dw() -> &'static IDWriteFactory2 {
    G_DW.get().expect("docpane::render::init() not called")
}

// ── per-thread caches ────────────────────────────────────────────────
#[derive(PartialEq, Eq, Hash, Clone)]
struct FmtKey {
    family: String,
    size_q: u32,
    bold: bool,
    italic: bool,
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct MeasureKey {
    text: String,
    family: String,
    size_q: u32,
    bold: bool,
    italic: bool,
}

thread_local! {
    static FMT_CACHE: RefCell<HashMap<FmtKey, IDWriteTextFormat>> = RefCell::new(HashMap::new());
    static MEASURE_CACHE: RefCell<HashMap<MeasureKey, f32>> = RefCell::new(HashMap::new());
}

unsafe fn get_fmt(family: &str, size: f32, bold: bool, italic: bool) -> Result<IDWriteTextFormat> {
    let key = FmtKey {
        family: family.to_owned(),
        size_q: (size * 64.0) as u32,
        bold,
        italic,
    };
    FMT_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let fmt = if let Some(f) = cache.get(&key) {
            f.clone()
        } else {
            let weight = if bold {
                DWRITE_FONT_WEIGHT_BOLD
            } else {
                DWRITE_FONT_WEIGHT_REGULAR
            };
            let style = if italic {
                DWRITE_FONT_STYLE_ITALIC
            } else {
                DWRITE_FONT_STYLE_NORMAL
            };
            let fw: Vec<u16> = family.encode_utf16().chain(std::iter::once(0)).collect();
            let fmt = dw().CreateTextFormat(
                PCWSTR(fw.as_ptr()),
                None,
                weight,
                style,
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                w!("en-us"),
            )?;
            cache.insert(key, fmt.clone());
            fmt
        };
        // Reset alignment to defaults — callers wanting otherwise set it
        // themselves; keeps the shared cache safe.
        let _ = fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        Ok(fmt)
    })
}

/// Accurate text width via DirectWrite, cached per thread.  This is the
/// `measure` function `layout::layout` takes — pass it in so layout
/// wraps text exactly as it will render.  Falls back to an approximate
/// width if a format/layout can't be created.
pub fn measure_text(text: &str, font: &str, size: f32, bold: bool, italic: bool) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let key = MeasureKey {
        text: text.to_owned(),
        family: font.to_owned(),
        size_q: (size * 64.0) as u32,
        bold,
        italic,
    };
    MEASURE_CACHE.with(|c| {
        {
            let cache = c.borrow();
            if let Some(&w) = cache.get(&key) {
                return w;
            }
        }
        let w = unsafe {
            let Ok(fmt) = get_fmt(font, size, bold, italic) else {
                return text.chars().count() as f32 * size * 0.52;
            };
            let tw: Vec<u16> = text.encode_utf16().collect();
            let Ok(tl) = dw().CreateTextLayout(&tw, &fmt, f32::MAX, size * 4.0) else {
                return text.chars().count() as f32 * size * 0.52;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            if tl.GetMetrics(&mut m).is_ok() {
                m.widthIncludingTrailingWhitespace
            } else {
                text.chars().count() as f32 * size * 0.52
            }
        };
        c.borrow_mut().insert(key, w);
        w
    })
}

unsafe fn brush(t: &ID2D1RenderTarget, hex: u32) -> Result<ID2D1SolidColorBrush> {
    let c = theme::hex(hex);
    t.CreateSolidColorBrush(std::ptr::addr_of!(c), None)
}

/// Fill a solid rectangle (DIPs) in `hex` colour.  A host uses this for
/// chrome it draws around the document — e.g. a doc-pane's sidebar
/// background and divider.
pub unsafe fn fill_rect(target: &ID2D1RenderTarget, x: f32, y: f32, w: f32, h: f32, hex: u32) {
    if let Ok(br) = brush(target, hex) {
        let r = D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h };
        target.FillRectangle(std::ptr::addr_of!(r), &br);
    }
}

/// Draw a single run of text within a rect (DIPs), clipped.  Left/top
/// aligned, or centred when `center` (used for a doc-pane's toggle
/// glyph).  Host chrome helper — same font cache as the document text.
#[allow(clippy::too_many_arguments)]
pub unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    text: &str,
    font: &str,
    size: f32,
    bold: bool,
    italic: bool,
    hex: u32,
    center: bool,
) {
    let (Ok(fmt), Ok(br)) = (get_fmt(font, size, bold, italic), brush(target, hex)) else {
        return;
    };
    if center {
        let _ = fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let _ = fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    }
    let tw: Vec<u16> = text.encode_utf16().collect();
    let r = D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h };
    target.DrawText(
        &tw,
        &fmt,
        std::ptr::addr_of!(r),
        &br,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
}

/// Render a laid-out document into `target` at a vertical scroll offset.
///
/// `scroll_y` is the number of DIPs scrolled down (0 = top); `viewport_h`
/// is the visible height in DIPs, used to skip commands off-screen.  The
/// command `x` positions are already baked in by `layout::layout`, so
/// the caller positions content by choosing the `x_base`/`width` it
/// passed to layout — not here.
///
/// The caller is responsible for `BeginDraw`/`Clear`/`EndDraw` around
/// this; `draw_document` only issues content draw calls.
pub unsafe fn draw_document(
    target: &ID2D1RenderTarget,
    layout: &Layout,
    scroll_y: f32,
    viewport_h: f32,
) -> Result<()> {
    let oy = -scroll_y;
    for cmd in &layout.cmds {
        match cmd {
            DrawCmd::FillRect { x, y, w, h, color } => {
                let ry = y + oy;
                if ry + h < 0.0 || ry > viewport_h {
                    continue;
                }
                let br = brush(target, *color)?;
                let r = D2D_RECT_F { left: *x, top: ry, right: x + w, bottom: ry + h };
                target.FillRectangle(std::ptr::addr_of!(r), &br);
            }
            DrawCmd::StrokeLine { x0, y0, x1, y1, color } => {
                let ry0 = y0 + oy;
                let ry1 = y1 + oy;
                if ry0 > viewport_h || ry1 < 0.0 {
                    continue;
                }
                let br = brush(target, *color)?;
                target.DrawLine(
                    Vector2 { X: *x0, Y: ry0 },
                    Vector2 { X: *x1, Y: ry1 },
                    &br,
                    1.0,
                    None::<&ID2D1StrokeStyle>,
                );
            }
            DrawCmd::Text {
                x, y, max_w, text, font, size, bold, italic, color, underline,
            } => {
                let ry = y + oy;
                let lh = size * theme::LINE_EXTRA;
                if ry + lh < 0.0 || ry > viewport_h {
                    continue;
                }
                let fmt = get_fmt(font, *size, *bold, *italic)?;
                let br = brush(target, *color)?;
                let r = D2D_RECT_F {
                    left: *x,
                    top: ry,
                    right: x + max_w,
                    bottom: ry + lh * 200.0,
                };
                target.DrawText(
                    text,
                    &fmt,
                    std::ptr::addr_of!(r),
                    &br,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                if *underline {
                    let uy = ry + size * 1.18;
                    let uw = max_w.min(text.len() as f32 * size * 0.52);
                    target.DrawLine(
                        Vector2 { X: *x, Y: uy },
                        Vector2 { X: x + uw, Y: uy },
                        &br,
                        0.8,
                        None::<&ID2D1StrokeStyle>,
                    );
                }
            }
            DrawCmd::Mermaid { x, y, scale, graph } => {
                let ry = y + oy;
                let h = graph.height() * scale;
                if ry + h < 0.0 || ry > viewport_h {
                    continue;
                }
                let _ = mermaid::render::draw_graph(
                    target,
                    d2d(),
                    graph,
                    *x,
                    ry,
                    *scale,
                    |hex| brush(target, hex),
                    |font, size, bold, italic| get_fmt(font, size, bold, italic),
                );
            }
            // Inline raster images need the host's image cache + docs
            // dir (pane-owned state) — wired when the doc-pane lands.
            // Our docs use Mermaid, not embedded bitmaps, so this is a
            // deferred no-op for now.
            DrawCmd::Image { .. } => {}
        }
    }
    Ok(())
}
