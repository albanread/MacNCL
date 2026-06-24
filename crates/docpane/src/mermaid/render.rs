//! Direct2D rendering for a doccrate mermaid [`Graph`].
//!
//! Walks the IR and emits primitive D2D calls. The renderer is decoupled from
//! `render::App` via two callbacks (one for solid brushes keyed by `u32` hex,
//! one for cached `IDWriteTextFormat`s) plus a borrowed `ID2D1Factory1`. This
//! keeps `App`'s private API contained in `render.rs` and lets the diagram
//! renderer be unit-tested with stub callbacks if we want.
//!
//! Positions are baked through scale manually rather than via `SetTransform`,
//! so stroke widths and font sizes stay constant on screen regardless of how
//! much the diagram is shrunk to fit the content width.

use std::sync::OnceLock;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use crate::mermaid::ir::*;
use crate::theme;

// ── Cached stroke styles for dashed / dotted edges ─────────────────────────
// `ID2D1StrokeStyle` is process-wide; once created we re-use forever.
static DASH_STYLE: OnceLock<ID2D1StrokeStyle> = OnceLock::new();
static DOT_STYLE: OnceLock<ID2D1StrokeStyle> = OnceLock::new();

unsafe fn dash_style(factory: &ID2D1Factory1) -> &'static ID2D1StrokeStyle {
    DASH_STYLE.get_or_init(|| {
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_FLAT,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_DASH,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let s1: ID2D1StrokeStyle1 = factory
            .CreateStrokeStyle(std::ptr::addr_of!(props), None)
            .expect("dash stroke style");
        s1.into()
    })
}

unsafe fn dot_style(factory: &ID2D1Factory1) -> &'static ID2D1StrokeStyle {
    DOT_STYLE.get_or_init(|| {
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_DOT,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let s1: ID2D1StrokeStyle1 = factory
            .CreateStrokeStyle(std::ptr::addr_of!(props), None)
            .expect("dot stroke style");
        s1.into()
    })
}

// ── Cached stroke styles shared with the sequence renderer ─────────────────

static SEQ_LIFELINE_STYLE: OnceLock<ID2D1StrokeStyle> = OnceLock::new();
static SEQ_DOT_STYLE: OnceLock<ID2D1StrokeStyle> = OnceLock::new();

/// Lifeline / dashed-line style for sequence diagrams. Different cadence to
/// the flowchart `dash_style` so they're visually distinguishable.
pub(crate) unsafe fn sequence_dash_style(factory: &ID2D1Factory1) -> &'static ID2D1StrokeStyle {
    SEQ_LIFELINE_STYLE.get_or_init(|| {
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_FLAT,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_DASH,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let s1: ID2D1StrokeStyle1 = factory
            .CreateStrokeStyle(std::ptr::addr_of!(props), None)
            .expect("sequence dash stroke style");
        s1.into()
    })
}

/// Dotted style for sequence messages.
pub(crate) unsafe fn sequence_dot_style(factory: &ID2D1Factory1) -> &'static ID2D1StrokeStyle {
    SEQ_DOT_STYLE.get_or_init(|| {
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_DOT,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let s1: ID2D1StrokeStyle1 = factory
            .CreateStrokeStyle(std::ptr::addr_of!(props), None)
            .expect("sequence dot stroke style");
        s1.into()
    })
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Draw any [`Graph`] into `target` with its top-left at `(ox, oy)`, scaled by
/// `scale`. Stroke widths and font sizes are interpreted as on-screen pixels.
/// Dispatches on the diagram variant.
pub unsafe fn draw_graph(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &Graph,
    ox: f32,
    oy: f32,
    scale: f32,
    brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    match graph {
        Graph::Architecture(g) => {
            crate::mermaid::architecture::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
        Graph::Flowchart(g) => draw_flowchart(target, factory, g, ox, oy, scale, brush, fmt),
        Graph::C4(g) => crate::mermaid::c4::draw(target, factory, g, ox, oy, scale, brush, fmt),
        Graph::Class(g) => {
            crate::mermaid::class::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
        Graph::Er(g) => crate::mermaid::er::draw(target, factory, g, ox, oy, scale, brush, fmt),
        Graph::Gantt(g) => {
            crate::mermaid::gantt::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
        Graph::Git(g) => crate::mermaid::git::draw(target, factory, g, ox, oy, scale, brush, fmt),
        Graph::Journey(g) => {
            crate::mermaid::journey::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
        Graph::Sequence(g) => {
            crate::mermaid::sequence::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
        Graph::Timeline(g) => {
            crate::mermaid::timeline::draw(target, factory, g, ox, oy, scale, brush, fmt)
        }
    }
}

/// Public re-export of [`build_polygon`] so sibling modules (sequence renderer)
/// can build their own triangle / cross / arrowhead geometries.
pub(crate) unsafe fn build_polygon_pub(
    factory: &ID2D1Factory1,
    pts: &[(f32, f32)],
) -> Result<ID2D1PathGeometry> {
    build_polygon(factory, pts)
}

unsafe fn draw_flowchart(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &FlowchartGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale; // for sizes (width/height) only

    // Canvas background
    if let Some(bg) = graph.background {
        let br = brush(bg)?;
        let r = D2D_RECT_F {
            left: ox,
            top: oy,
            right: ox + graph.width * scale,
            bottom: oy + graph.height * scale,
        };
        target.FillRectangle(std::ptr::addr_of!(r), &br);
    }

    // ── Groups (subgraphs) ────────────────────────────────────────────────
    for g in &graph.groups {
        let r = D2D_RECT_F {
            left: tx(g.x),
            top: ty(g.y),
            right: tx(g.x) + ts(g.w),
            bottom: ty(g.y) + ts(g.h),
        };
        let rr = D2D1_ROUNDED_RECT {
            rect: r,
            radiusX: 6.0,
            radiusY: 6.0,
        };
        let fill_br = brush(g.fill)?;
        target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
        let stroke_br = brush(g.stroke)?;
        target.DrawRoundedRectangle(
            std::ptr::addr_of!(rr),
            &stroke_br,
            g.stroke_w,
            None::<&ID2D1StrokeStyle>,
        );

        if let Some(title) = &g.title {
            let f = fmt(theme::BODY_FONT, g.title_font_size, true, false)?;
            let title_br = brush(g.title_color)?;
            let buf: Vec<u16> = title.encode_utf16().collect();
            let title_rect = D2D_RECT_F {
                left: r.left + 8.0,
                top: r.top + 4.0,
                right: r.right - 8.0,
                bottom: r.top + 4.0 + g.title_font_size * 1.4,
            };
            target.DrawText(
                &buf,
                &f,
                std::ptr::addr_of!(title_rect),
                &title_br,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    // ── Edges ─────────────────────────────────────────────────────────────
    // Edges draw below nodes so the arrowheads tuck under node borders cleanly.
    for e in &graph.edges {
        draw_edge(target, factory, e, &tx, &ty, &mut brush, &mut fmt)?;
    }

    // ── Nodes ─────────────────────────────────────────────────────────────
    for n in &graph.nodes {
        draw_node(target, factory, n, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }

    Ok(())
}

// ── Node ───────────────────────────────────────────────────────────────────

unsafe fn draw_node(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    n: &Node,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(n.x);
    let y = ty(n.y);
    let w = ts(n.w);
    let h = ts(n.h);
    let fill_br = brush(n.fill)?;
    let stroke_br = brush(n.stroke)?;

    match n.shape {
        Shape::Rect => {
            let r = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            target.FillRectangle(std::ptr::addr_of!(r), &fill_br);
            target.DrawRectangle(
                std::ptr::addr_of!(r),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::RoundedRect => {
            let r = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: 8.0,
                radiusY: 8.0,
            };
            target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
            target.DrawRoundedRectangle(
                std::ptr::addr_of!(rr),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::Stadium => {
            let r = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let radius = (h / 2.0).max(1.0);
            let rr = D2D1_ROUNDED_RECT {
                rect: r,
                radiusX: radius,
                radiusY: radius,
            };
            target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
            target.DrawRoundedRectangle(
                std::ptr::addr_of!(rr),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::Circle | Shape::Ellipse => {
            let cx = x + w / 2.0;
            let cy = y + h / 2.0;
            let (rx, ry) = if matches!(n.shape, Shape::Circle) {
                let r = w.max(h) / 2.0;
                (r, r)
            } else {
                (w / 2.0, h / 2.0)
            };
            let e = D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: rx,
                radiusY: ry,
            };
            target.FillEllipse(std::ptr::addr_of!(e), &fill_br);
            target.DrawEllipse(
                std::ptr::addr_of!(e),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::Diamond => {
            let cx = x + w / 2.0;
            let cy = y + h / 2.0;
            let pts = [
                (cx, y),     // top
                (x + w, cy), // right
                (cx, y + h), // bottom
                (x, cy),     // left
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::Hexagon => {
            // Pointy-top hexagon inscribed in the rect.
            let inset = (h / 2.0).min(w * 0.25);
            let pts = [
                (x + inset, y),
                (x + w - inset, y),
                (x + w, y + h / 2.0),
                (x + w - inset, y + h),
                (x + inset, y + h),
                (x, y + h / 2.0),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::DoubleCircle => {
            // Two concentric ellipses; gap proportional to size so it scales.
            let cx = x + w / 2.0;
            let cy = y + h / 2.0;
            let rx_outer = w / 2.0;
            let ry_outer = h / 2.0;
            let gap = (w.min(h) * 0.08).max(3.0);
            let rx_inner = (rx_outer - gap).max(1.0);
            let ry_inner = (ry_outer - gap).max(1.0);

            let outer = D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: rx_outer,
                radiusY: ry_outer,
            };
            target.FillEllipse(std::ptr::addr_of!(outer), &fill_br);
            target.DrawEllipse(
                std::ptr::addr_of!(outer),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
            let inner = D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: rx_inner,
                radiusY: ry_inner,
            };
            target.DrawEllipse(
                std::ptr::addr_of!(inner),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::Cylinder => {
            // Cap height: ~15% of total, clamped so very short cylinders still look right.
            let cap_h = (h * 0.18).max(6.0).min(h * 0.45);
            let geo = build_cylinder_silhouette(factory, x, y, w, h, cap_h)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
            // The "open lip" — visible inside-front edge of the top cap.
            let lip = build_cylinder_top_lip(factory, x, y, w, cap_h)?;
            target.DrawGeometry(&lip, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::Subroutine => {
            // Outer rectangle + two vertical inner bars near each short edge.
            let r = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            target.FillRectangle(std::ptr::addr_of!(r), &fill_br);
            target.DrawRectangle(
                std::ptr::addr_of!(r),
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
            let inset = (w * 0.06).max(6.0).min(w * 0.2);
            target.DrawLine(
                Vector2 { X: x + inset, Y: y },
                Vector2 {
                    X: x + inset,
                    Y: y + h,
                },
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
            target.DrawLine(
                Vector2 {
                    X: x + w - inset,
                    Y: y,
                },
                Vector2 {
                    X: x + w - inset,
                    Y: y + h,
                },
                &stroke_br,
                n.stroke_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Shape::Trapezoid => {
            // Wider at the bottom.
            let inset = (w * 0.15).min(h * 0.7);
            let pts = [
                (x + inset, y),
                (x + w - inset, y),
                (x + w, y + h),
                (x, y + h),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::InvTrapezoid => {
            // Wider at the top.
            let inset = (w * 0.15).min(h * 0.7);
            let pts = [
                (x, y),
                (x + w, y),
                (x + w - inset, y + h),
                (x + inset, y + h),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::LeanRight => {
            // Parallelogram leaning right (top-left and bottom-right pushed outward).
            let shear = (w * 0.15).min(h * 0.7);
            let pts = [
                (x + shear, y),
                (x + w, y),
                (x + w - shear, y + h),
                (x, y + h),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::LeanLeft => {
            // Parallelogram leaning left (mirror of LeanRight).
            let shear = (w * 0.15).min(h * 0.7);
            let pts = [
                (x, y),
                (x + w - shear, y),
                (x + w, y + h),
                (x + shear, y + h),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::Odd => {
            // Flag / asymmetric pentagon (rectangle with a notched left side).
            let notch = (w * 0.18).min(h * 0.5);
            let pts = [
                (x, y),
                (x + w, y),
                (x + w, y + h),
                (x, y + h),
                (x + notch, y + h / 2.0),
            ];
            let geo = build_polygon(factory, &pts)?;
            target.FillGeometry(&geo, &fill_br, None);
            target.DrawGeometry(&geo, &stroke_br, n.stroke_w, None::<&ID2D1StrokeStyle>);
        }
        Shape::HorizontalBar => {
            // Thin filled bar used for fork/join. No label.
            let r = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            target.FillRectangle(std::ptr::addr_of!(r), &stroke_br);
        }
        Shape::Custom(idx) => {
            if let Some(def) = crate::mermaid::shape_def::registry().get(idx) {
                let geo = build_custom_geometry(factory, def, x, y, w, h)?;
                target.FillGeometry(&geo, &fill_br, None);
                target.DrawGeometry(
                    &geo,
                    &stroke_br,
                    n.stroke_w * def.stroke_mult,
                    None::<&ID2D1StrokeStyle>,
                );
            } else {
                // Registry miss — defensive fallback so the node still appears.
                let r = D2D_RECT_F {
                    left: x,
                    top: y,
                    right: x + w,
                    bottom: y + h,
                };
                target.FillRectangle(std::ptr::addr_of!(r), &fill_br);
                target.DrawRectangle(
                    std::ptr::addr_of!(r),
                    &stroke_br,
                    n.stroke_w,
                    None::<&ID2D1StrokeStyle>,
                );
            }
        }
    }

    // HorizontalBar is a structural marker, not a labelled node.
    if matches!(n.shape, Shape::HorizontalBar) {
        return Ok(());
    }
    // For a cylinder, push the label below the top cap so it isn't clipped
    // by the curved lip. `h` is already the on-screen height, matching the
    // `cap_h` formula used by the cylinder geometry above.
    let label_top_pad = if matches!(n.shape, Shape::Cylinder) {
        (h * 0.18).max(6.0).min(h * 0.45)
    } else {
        0.0
    };

    // Label — single-line, centred vertically inside the node, horizontal
    // alignment per `n.label_align`. Padding scales down at very small scales
    // so the text doesn't get pinched against borders.
    if !n.label.is_empty() {
        let pad = 6.0_f32.min(w * 0.1);
        // Custom shapes can specify a tighter label bounding box via
        // `text-area`; otherwise fall back to the node rect with padding.
        let label_rect = if let Shape::Custom(idx) = n.shape {
            match crate::mermaid::shape_def::registry().get(idx) {
                Some(def) => {
                    let (lx0, ly0, lx1, ly1) = def.label_rect();
                    D2D_RECT_F {
                        left: x + lx0 * w,
                        top: y + ly0 * h,
                        right: x + lx1 * w,
                        bottom: y + ly1 * h,
                    }
                }
                None => D2D_RECT_F {
                    left: x + pad,
                    top: y + label_top_pad,
                    right: x + w - pad,
                    bottom: y + h,
                },
            }
        } else {
            D2D_RECT_F {
                left: x + pad,
                top: y + label_top_pad,
                right: x + w - pad,
                bottom: y + h,
            }
        };
        let f = fmt(theme::BODY_FONT, n.font_size, n.bold, false)?;
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = f.SetTextAlignment(match n.label_align {
            Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
            Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
        });
        let buf: Vec<u16> = n.label.encode_utf16().collect();
        let label_br = brush(n.text_color)?;
        target.DrawText(
            &buf,
            &f,
            std::ptr::addr_of!(label_rect),
            &label_br,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        // Restore defaults — the format cache is shared with body-text rendering,
        // so leaving CENTER alignment behind would silently mis-render paragraphs.
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    }
    Ok(())
}

// ── Edge ───────────────────────────────────────────────────────────────────

unsafe fn draw_edge(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    e: &Edge,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if e.points.len() < 2 {
        return Ok(());
    }
    let pts: Vec<(f32, f32)> = e.points.iter().map(|(x, y)| (tx(*x), ty(*y))).collect();
    let line_br = brush(e.line_color)?;
    let style: Option<&ID2D1StrokeStyle> = match e.line_style {
        LineStyle::Solid => None,
        LineStyle::Dash => Some(dash_style(factory)),
        LineStyle::Dot => Some(dot_style(factory)),
    };

    // Polyline: a series of DrawLine segments. Cheap, no path geometry needed.
    for w in pts.windows(2) {
        target.DrawLine(
            Vector2 {
                X: w[0].0,
                Y: w[0].1,
            },
            Vector2 {
                X: w[1].0,
                Y: w[1].1,
            },
            &line_br,
            e.line_w,
            style,
        );
    }

    // Arrowheads
    if !matches!(e.end_arrow, Arrow::None) {
        let (b, a) = (pts[pts.len() - 2], pts[pts.len() - 1]);
        draw_arrow(target, factory, b, a, e.end_arrow, e.line_w, &line_br)?;
    }
    if !matches!(e.start_arrow, Arrow::None) {
        let (b, a) = (pts[1], pts[0]);
        draw_arrow(target, factory, b, a, e.start_arrow, e.line_w, &line_br)?;
    }

    // Label
    if let Some(lbl) = &e.label {
        let scaled_lx = tx(lbl.x);
        let scaled_ly = ty(lbl.y);
        let scaled_w = (tx(lbl.x + lbl.w) - scaled_lx).max(1.0);
        let scaled_h = (ty(lbl.y + lbl.h) - scaled_ly).max(1.0);
        let min_w = lbl.text.chars().count() as f32 * lbl.font_size * 0.62 + 10.0;
        let min_h = lbl.font_size * theme::LINE_EXTRA + 4.0;
        let lw = scaled_w.max(min_w);
        let lh = scaled_h.max(min_h);
        let cx = tx(lbl.x + lbl.w / 2.0);
        let cy = ty(lbl.y + lbl.h / 2.0);
        let lx = cx - lw / 2.0;
        let ly = cy - lh / 2.0;

        // Tiny pill behind the text so it's readable when crossing edge lines.
        let bg_br = brush(theme::BG)?;
        let r = D2D_RECT_F {
            left: lx - 2.0,
            top: ly,
            right: lx + lw + 2.0,
            bottom: ly + lh,
        };
        target.FillRectangle(std::ptr::addr_of!(r), &bg_br);

        let f = fmt(theme::BODY_FONT, lbl.font_size, false, false)?;
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let buf: Vec<u16> = lbl.text.encode_utf16().collect();
        let text_br = brush(lbl.text_color)?;
        let lr = D2D_RECT_F {
            left: lx,
            top: ly,
            right: lx + lw,
            bottom: ly + lh,
        };
        target.DrawText(
            &buf,
            &f,
            std::ptr::addr_of!(lr),
            &text_br,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    }
    Ok(())
}

/// Draw an arrowhead at `a`, pointing along the segment from `b` to `a`.
unsafe fn draw_arrow(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    b: (f32, f32),
    a: (f32, f32),
    kind: Arrow,
    line_w: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<()> {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    // Perpendicular unit vector
    let px = -uy;
    let py = ux;

    let size = (line_w * 5.0).max(7.0);

    match kind {
        Arrow::None => {}
        Arrow::Triangle => {
            // Tip at `a`, base behind it
            let tip = (a.0, a.1);
            let back = (a.0 - ux * size, a.1 - uy * size);
            let half = size * 0.6;
            let l = (back.0 + px * half, back.1 + py * half);
            let r = (back.0 - px * half, back.1 - py * half);
            let geo = build_polygon(factory, &[tip, l, r])?;
            target.FillGeometry(&geo, brush, None);
        }
        Arrow::Circle => {
            let radius = size * 0.45;
            let cx = a.0 - ux * radius;
            let cy = a.1 - uy * radius;
            let e = D2D1_ELLIPSE {
                point: Vector2 { X: cx, Y: cy },
                radiusX: radius,
                radiusY: radius,
            };
            target.FillEllipse(std::ptr::addr_of!(e), brush);
        }
        Arrow::Cross => {
            // Two short perpendicular strokes through the tip.
            let half = size * 0.5;
            let v0 = (a.0 + px * half, a.1 + py * half);
            let v1 = (a.0 - px * half, a.1 - py * half);
            let h0 = (a.0 + ux * half, a.1 + uy * half);
            let h1 = (a.0 - ux * half, a.1 - uy * half);
            target.DrawLine(
                Vector2 { X: v0.0, Y: v0.1 },
                Vector2 { X: v1.0, Y: v1.1 },
                brush,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
            target.DrawLine(
                Vector2 { X: h0.0, Y: h0.1 },
                Vector2 { X: h1.0, Y: h1.1 },
                brush,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }
    Ok(())
}

/// Build the closed silhouette of a database-style cylinder: top arc, right
/// side, bottom arc, left side. Drawn filled and stroked.
///
/// In Direct2D the y axis grows downward, so `SWEEP_DIRECTION_CLOCKWISE` from
/// the left endpoint of an ellipse to the right endpoint sweeps via the *top*
/// of the ellipse (visually upward). That's what we want for the top cap.
unsafe fn build_cylinder_silhouette(
    factory: &ID2D1Factory1,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    cap_h: f32,
) -> Result<ID2D1PathGeometry> {
    let rx = w / 2.0;
    let ry = cap_h / 2.0;
    let ymid_top = y + ry;
    let ymid_bot = y + h - ry;

    let geo1: ID2D1PathGeometry1 = factory.CreatePathGeometry()?;
    let geo: ID2D1PathGeometry = geo1.into();
    let sink: ID2D1GeometrySink = geo.Open()?;

    sink.BeginFigure(Vector2 { X: x, Y: ymid_top }, D2D1_FIGURE_BEGIN_FILLED);
    sink.AddArc(&D2D1_ARC_SEGMENT {
        point: Vector2 {
            X: x + w,
            Y: ymid_top,
        },
        size: D2D_SIZE_F {
            width: rx,
            height: ry,
        },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    });
    sink.AddLine(Vector2 {
        X: x + w,
        Y: ymid_bot,
    });
    sink.AddArc(&D2D1_ARC_SEGMENT {
        point: Vector2 { X: x, Y: ymid_bot },
        size: D2D_SIZE_F {
            width: rx,
            height: ry,
        },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    });
    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    sink.Close()?;
    Ok(geo)
}

/// The visible inside-lip of a cylinder's top cap — an open arc curving
/// downward through the bottom of the top ellipse, indicating depth.
unsafe fn build_cylinder_top_lip(
    factory: &ID2D1Factory1,
    x: f32,
    y: f32,
    w: f32,
    cap_h: f32,
) -> Result<ID2D1PathGeometry> {
    let rx = w / 2.0;
    let ry = cap_h / 2.0;
    let ymid_top = y + ry;

    let geo1: ID2D1PathGeometry1 = factory.CreatePathGeometry()?;
    let geo: ID2D1PathGeometry = geo1.into();
    let sink: ID2D1GeometrySink = geo.Open()?;

    sink.BeginFigure(Vector2 { X: x, Y: ymid_top }, D2D1_FIGURE_BEGIN_HOLLOW);
    // Counter-clockwise from left endpoint → right endpoint passes through the
    // bottom of the ellipse (because of the y-down convention).
    sink.AddArc(&D2D1_ARC_SEGMENT {
        point: Vector2 {
            X: x + w,
            Y: ymid_top,
        },
        size: D2D_SIZE_F {
            width: rx,
            height: ry,
        },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    });
    sink.EndFigure(D2D1_FIGURE_END_OPEN);
    sink.Close()?;
    Ok(geo)
}

/// Build a filled polygon path geometry from a vertex list (≥ 3 points).
/// Re-export of `build_custom_geometry` for the sequence-diagram renderer.
/// Same arguments; lives in this module because the rest of the path /
/// geometry helpers do.
pub(crate) unsafe fn build_custom_geometry_pub(
    factory: &ID2D1Factory1,
    def: &crate::mermaid::shape_def::ShapeDef,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Result<ID2D1PathGeometry> {
    build_custom_geometry(factory, def, x, y, w, h)
}

/// Build a D2D path geometry from a [`ShapeDef`]. Normalised 0..1 coords are
/// mapped onto the node rect `(x, y, w, h)` already in screen space.
unsafe fn build_custom_geometry(
    factory: &ID2D1Factory1,
    def: &crate::mermaid::shape_def::ShapeDef,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Result<ID2D1PathGeometry> {
    use crate::mermaid::shape_def::PathCmd;
    let geo1: ID2D1PathGeometry1 = factory.CreatePathGeometry()?;
    let geo: ID2D1PathGeometry = geo1.into();
    let sink: ID2D1GeometrySink = geo.Open()?;

    let mut open = false;
    let tx = |nx: f32| x + nx * w;
    let ty = |ny: f32| y + ny * h;
    for cmd in &def.commands {
        match cmd {
            PathCmd::MoveTo(nx, ny) => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                }
                sink.BeginFigure(
                    Vector2 {
                        X: tx(*nx),
                        Y: ty(*ny),
                    },
                    D2D1_FIGURE_BEGIN_FILLED,
                );
                open = true;
            }
            PathCmd::LineTo(nx, ny) => {
                sink.AddLine(Vector2 {
                    X: tx(*nx),
                    Y: ty(*ny),
                });
            }
            PathCmd::CurveTo(c1x, c1y, c2x, c2y, nx, ny) => {
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: Vector2 {
                        X: tx(*c1x),
                        Y: ty(*c1y),
                    },
                    point2: Vector2 {
                        X: tx(*c2x),
                        Y: ty(*c2y),
                    },
                    point3: Vector2 {
                        X: tx(*nx),
                        Y: ty(*ny),
                    },
                });
            }
            PathCmd::QuadTo(cx, cy, nx, ny) => {
                sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                    point1: Vector2 {
                        X: tx(*cx),
                        Y: ty(*cy),
                    },
                    point2: Vector2 {
                        X: tx(*nx),
                        Y: ty(*ny),
                    },
                });
            }
            PathCmd::Polygon(pts) => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    open = false;
                }
                if let Some(&(nx, ny)) = pts.first() {
                    sink.BeginFigure(
                        Vector2 {
                            X: tx(nx),
                            Y: ty(ny),
                        },
                        D2D1_FIGURE_BEGIN_FILLED,
                    );
                    let rest: Vec<Vector2> = pts[1..]
                        .iter()
                        .map(|p| Vector2 {
                            X: tx(p.0),
                            Y: ty(p.1),
                        })
                        .collect();
                    sink.AddLines(&rest);
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                }
            }
            PathCmd::Circle(cx, cy, r) => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    open = false;
                }
                // Approximate full ellipse with two 180° arcs.
                let rw = r * w;
                let rh = r * h;
                let cxv = tx(*cx);
                let cyv = ty(*cy);
                sink.BeginFigure(
                    Vector2 {
                        X: cxv - rw,
                        Y: cyv,
                    },
                    D2D1_FIGURE_BEGIN_FILLED,
                );
                sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: Vector2 {
                        X: cxv + rw,
                        Y: cyv,
                    },
                    size: D2D_SIZE_F {
                        width: rw,
                        height: rh,
                    },
                    rotationAngle: 0.0,
                    sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                    arcSize: D2D1_ARC_SIZE_SMALL,
                });
                sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: Vector2 {
                        X: cxv - rw,
                        Y: cyv,
                    },
                    size: D2D_SIZE_F {
                        width: rw,
                        height: rh,
                    },
                    rotationAngle: 0.0,
                    sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                    arcSize: D2D1_ARC_SIZE_SMALL,
                });
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            }
            PathCmd::Close => {
                if open {
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    open = false;
                }
            }
        }
    }
    if open {
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    }
    sink.Close()?;
    Ok(geo)
}

unsafe fn build_polygon(factory: &ID2D1Factory1, pts: &[(f32, f32)]) -> Result<ID2D1PathGeometry> {
    let geo1: ID2D1PathGeometry1 = factory.CreatePathGeometry()?;
    let geo: ID2D1PathGeometry = geo1.into();
    let sink: ID2D1GeometrySink = geo.Open()?;
    sink.BeginFigure(
        Vector2 {
            X: pts[0].0,
            Y: pts[0].1,
        },
        D2D1_FIGURE_BEGIN_FILLED,
    );
    let rest: Vec<Vector2> = pts[1..]
        .iter()
        .map(|p| Vector2 { X: p.0, Y: p.1 })
        .collect();
    sink.AddLines(&rest);
    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    sink.Close()?;
    Ok(geo)
}
