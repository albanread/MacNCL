//! Surface executor: drains a `PaneBatch` and translates each
//! `SurfaceCmd` into Direct2D draw calls. Phase 3b implements the
//! lifecycle and basic geometry primitives; the rest land in 3c / 5.

#![cfg(windows)]

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::Interface;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN, D2D_RECT_F, D2D_SIZE_F,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1Brush, ID2D1DeviceContext, ID2D1SolidColorBrush, ID2D1StrokeStyle, D2D1_ARC_SEGMENT,
    D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_ELLIPSE, D2D1_ROUNDED_RECT,
    D2D1_SWEEP_DIRECTION_CLOCKWISE,
};
use windows_numerics::Vector2;

use super::batch::{PaneBatch, Rgba, SurfaceCmd};
use super::renderer;
use super::IGuiError;

/// Process-wide solid-color brush cache. Brushes are bound to the D2D
/// device context, which is itself process-wide (one per `iGui::run`),
/// so brushes outlive any individual swap chain.
struct BrushCache {
    map: HashMap<u128, ID2D1SolidColorBrush>,
}

impl BrushCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    fn get(&mut self, ctx: &ID2D1DeviceContext, color: Rgba) -> Result<ID2D1Brush, IGuiError> {
        let key = pack_color(color);
        if !self.map.contains_key(&key) {
            let d2d_color = D2D1_COLOR_F {
                r: color.r,
                g: color.g,
                b: color.b,
                a: color.a,
            };
            let brush = unsafe { ctx.CreateSolidColorBrush(&d2d_color, None) }
                .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush: {e}")))?;
            self.map.insert(key, brush);
        }
        Ok(self.map.get(&key).unwrap().cast::<ID2D1Brush>().unwrap())
    }
}

fn pack_color(c: Rgba) -> u128 {
    let r = c.r.to_bits() as u128;
    let g = c.g.to_bits() as u128;
    let b = c.b.to_bits() as u128;
    let a = c.a.to_bits() as u128;
    r | (g << 32) | (b << 64) | (a << 96)
}

thread_local! {
    static BRUSHES: RefCell<BrushCache> = RefCell::new(BrushCache::new());
}

/// Execute every command in `batch` against the currently bound D2D
/// render target. Caller is responsible for `BeginDraw` / `EndDraw` /
/// `Present`. Returns `Ok(present_hint)` — true if the batch wants an
/// explicit Present beyond the default.
pub fn execute(batch: &PaneBatch) -> Result<bool, IGuiError> {
    let r = renderer::ctx();
    let ctx = &r.d2d.context;
    let mut want_present = false;

    let no_stroke: Option<&ID2D1StrokeStyle> = None;

    for cmd in &batch.cmds {
        match cmd {
            SurfaceCmd::Clear { color } => unsafe {
                ctx.Clear(Some(&D2D1_COLOR_F {
                    r: color.r,
                    g: color.g,
                    b: color.b,
                    a: color.a,
                }));
            },
            SurfaceCmd::PresentHint => {
                want_present = true;
            }
            SurfaceCmd::FillRect {
                rect,
                corner_radius,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let r2d = D2D_RECT_F {
                    left: rect.x0,
                    top: rect.y0,
                    right: rect.x1,
                    bottom: rect.y1,
                };
                if *corner_radius <= 0.0 {
                    unsafe { ctx.FillRectangle(&r2d, &brush) };
                } else {
                    let rr = D2D1_ROUNDED_RECT {
                        rect: r2d,
                        radiusX: *corner_radius,
                        radiusY: *corner_radius,
                    };
                    unsafe { ctx.FillRoundedRectangle(&rr, &brush) };
                }
            }
            SurfaceCmd::StrokeRect {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let r2d = D2D_RECT_F {
                    left: rect.x0,
                    top: rect.y0,
                    right: rect.x1,
                    bottom: rect.y1,
                };
                let stroke_w = (2.0 * half_thickness).max(0.0);
                if *corner_radius <= 0.0 {
                    unsafe { ctx.DrawRectangle(&r2d, &brush, stroke_w, no_stroke) };
                } else {
                    let rr = D2D1_ROUNDED_RECT {
                        rect: r2d,
                        radiusX: *corner_radius,
                        radiusY: *corner_radius,
                    };
                    unsafe { ctx.DrawRoundedRectangle(&rr, &brush, stroke_w, no_stroke) };
                }
            }
            SurfaceCmd::DrawLine {
                p0,
                p1,
                half_thickness,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let stroke_w = (2.0 * half_thickness).max(0.0);
                unsafe {
                    ctx.DrawLine(
                        Vector2 { X: p0.x, Y: p0.y },
                        Vector2 { X: p1.x, Y: p1.y },
                        &brush,
                        stroke_w,
                        no_stroke,
                    )
                };
            }
            SurfaceCmd::FillOval { rect, color } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let ellipse = ellipse_from_rect(rect);
                unsafe { ctx.FillEllipse(&ellipse, &brush) };
            }
            SurfaceCmd::FillCircle {
                center,
                radius,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let ellipse = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: center.x,
                        Y: center.y,
                    },
                    radiusX: *radius,
                    radiusY: *radius,
                };
                unsafe { ctx.FillEllipse(&ellipse, &brush) };
            }
            SurfaceCmd::StrokeOval {
                rect,
                half_thickness,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let ellipse = ellipse_from_rect(rect);
                let stroke_w = (2.0 * half_thickness).max(0.0);
                unsafe { ctx.DrawEllipse(&ellipse, &brush, stroke_w, no_stroke) };
            }
            SurfaceCmd::StrokeCircle {
                center,
                radius,
                half_thickness,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let ellipse = D2D1_ELLIPSE {
                    point: Vector2 {
                        X: center.x,
                        Y: center.y,
                    },
                    radiusX: *radius,
                    radiusY: *radius,
                };
                let stroke_w = (2.0 * half_thickness).max(0.0);
                unsafe { ctx.DrawEllipse(&ellipse, &brush, stroke_w, no_stroke) };
            }
            SurfaceCmd::DrawArc {
                center,
                radius,
                rotation_rad,
                half_aperture_rad,
                half_thickness,
                color,
            } => {
                let brush = BRUSHES.with(|c| c.borrow_mut().get(ctx, *color))?;
                let stroke_w = (2.0 * half_thickness).max(0.0);
                draw_arc(ctx, *center, *radius, *rotation_rad, *half_aperture_rad,
                         stroke_w, &brush, no_stroke)?;
            }
            // The device-context executor is currently dormant; the
            // active text path is in child.rs::execute_d2d_batch via
            // the HwndRenderTarget. Keep these arms exhaustive so this
            // file builds.
            SurfaceCmd::DrawTextRun { .. }
            | SurfaceCmd::MeasureTextRun { .. }
            | SurfaceCmd::CharIndexAtPoint { .. }
            | SurfaceCmd::PointAtCharIndex { .. } => {
                eprintln!("[igui-executor] text command on dormant DC path — ignored");
            }
            // Phase 5 — same dormant story.
            SurfaceCmd::PushClipRect { .. }
            | SurfaceCmd::PopClipRect
            | SurfaceCmd::PushOffset { .. }
            | SurfaceCmd::PopOffset
            | SurfaceCmd::ScrollRect { .. }
            | SurfaceCmd::SaveRect { .. }
            | SurfaceCmd::RestoreRect { .. }
            | SurfaceCmd::InstallChildViewBounds { .. }
            | SurfaceCmd::MarkRect { .. }
            | SurfaceCmd::Caret { .. }
            | SurfaceCmd::SelectionRange { .. }
            | SurfaceCmd::FocusRing { .. }
            | SurfaceCmd::DrawPath { .. } => {
                eprintln!("[igui-executor] Phase 5 command on dormant DC path — ignored");
            }
            SurfaceCmd::Blit { .. } => {
                eprintln!("[igui-executor] canvas Blit on dormant DC path — ignored");
            }
        }
    }

    Ok(want_present)
}

/// Build a `D2D1_ELLIPSE` from an axis-aligned bounding rect. The
/// ellipse fits exactly inside the rect.
fn ellipse_from_rect(rect: &super::batch::Rect) -> D2D1_ELLIPSE {
    let cx = 0.5 * (rect.x0 + rect.x1);
    let cy = 0.5 * (rect.y0 + rect.y1);
    let rx = 0.5 * (rect.x1 - rect.x0).abs();
    let ry = 0.5 * (rect.y1 - rect.y0).abs();
    D2D1_ELLIPSE {
        point: Vector2 { X: cx, Y: cy },
        radiusX: rx,
        radiusY: ry,
    }
}

/// Stroke a circular arc spanning
/// `[rotation_rad - half_aperture_rad, rotation_rad + half_aperture_rad]`.
/// Builds a transient `ID2D1PathGeometry` with one figure containing
/// a single arc segment, then `DrawGeometry`.
fn draw_arc(
    ctx: &ID2D1DeviceContext,
    center: super::batch::Point,
    radius: f32,
    rotation_rad: f32,
    half_aperture_rad: f32,
    stroke_w: f32,
    brush: &ID2D1Brush,
    no_stroke: Option<&ID2D1StrokeStyle>,
) -> Result<(), IGuiError> {
    let factory = &renderer::ctx().d2d.factory;
    let geometry = unsafe { factory.CreatePathGeometry() }
        .map_err(|e| IGuiError::D2D(format!("CreatePathGeometry: {e}")))?;
    let sink = unsafe { geometry.Open() }
        .map_err(|e| IGuiError::D2D(format!("ID2D1PathGeometry::Open: {e}")))?;

    let start_angle = rotation_rad - half_aperture_rad;
    let end_angle = rotation_rad + half_aperture_rad;
    let start = Vector2 {
        X: center.x + radius * start_angle.cos(),
        Y: center.y + radius * start_angle.sin(),
    };
    let end = Vector2 {
        X: center.x + radius * end_angle.cos(),
        Y: center.y + radius * end_angle.sin(),
    };

    // half_aperture_rad > π/2 ⇒ large arc.
    let total_sweep = (2.0 * half_aperture_rad).abs();
    let arc_size = if total_sweep > std::f32::consts::PI {
        D2D1_ARC_SIZE_LARGE
    } else {
        D2D1_ARC_SIZE_SMALL
    };

    let arc_segment = D2D1_ARC_SEGMENT {
        point: end,
        size: D2D_SIZE_F {
            width: radius,
            height: radius,
        },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: arc_size,
    };

    unsafe {
        sink.BeginFigure(start, D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddArc(&arc_segment);
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
    }
    unsafe { sink.Close() }
        .map_err(|e| IGuiError::D2D(format!("ID2D1GeometrySink::Close: {e}")))?;

    unsafe { ctx.DrawGeometry(&geometry, brush, stroke_w, no_stroke) };
    Ok(())
}
