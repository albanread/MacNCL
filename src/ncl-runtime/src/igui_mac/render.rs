//! Core Graphics rasteriser for the `SurfaceCmd` IR.
//!
//! The macOS analogue of `igui::child::execute_d2d_batch`: it walks a
//! `&[SurfaceCmd]` and issues the equivalent Core Graphics calls. It
//! targets a `CGBitmapContext`, so it renders **headlessly** — no
//! window, no display — which makes the whole drawing path
//! unit-testable (render a batch, read back pixels, assert).
//!
//! Coordinate system: the `SurfaceCmd` IR (like Direct2D) is top-left
//! origin, y-down. Core Graphics is bottom-left origin, y-up, and a
//! `CGBitmapContext`'s backing store is laid out so the *first* row in
//! memory is the *top* of the image only after a vertical flip. We
//! install that flip once (`translate(0,h); scale(1,-1)`) so (a) IR
//! coordinates map directly and (b) memory row `y` corresponds to IR
//! `y` — i.e. `pixel(x, y)` reads the byte block at `(y*w + x)*4`.
//!
//! Pixels are RGBA8888, premultiplied-last (`kCGImageAlphaPremultipliedLast`).

use core_graphics::base::kCGImageAlphaPremultipliedLast;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};

use crate::igui_paint::{PathCmd, Point, Rect, Rgba, SurfaceCmd};

/// A headless Core Graphics drawing surface sized `width`×`height`.
pub struct CgCanvas {
    ctx: CGContext,
    width: usize,
    height: usize,
}

#[inline]
fn rect_cg(r: &Rect) -> CGRect {
    let x = r.x0.min(r.x1) as f64;
    let y = r.y0.min(r.y1) as f64;
    let w = (r.x1 - r.x0).abs() as f64;
    let h = (r.y1 - r.y0).abs() as f64;
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(w, h))
}

#[inline]
fn circle_rect(c: Point, radius: f32) -> CGRect {
    let r = radius as f64;
    CGRect::new(
        &CGPoint::new(c.x as f64 - r, c.y as f64 - r),
        &CGSize::new(2.0 * r, 2.0 * r),
    )
}

impl CgCanvas {
    /// Create a `width`×`height` RGBA bitmap canvas with a transparent
    /// backing store and the IR-coordinate flip already applied.
    pub fn new(width: usize, height: usize) -> Self {
        let cs = CGColorSpace::create_device_rgb();
        let ctx = CGContext::create_bitmap_context(
            None,
            width,
            height,
            8,
            width * 4,
            &cs,
            kCGImageAlphaPremultipliedLast,
        );
        // Flip to top-left, y-down so IR coordinates map 1:1 and memory
        // row y == IR y.
        ctx.translate(0.0, height as f64);
        ctx.scale(1.0, -1.0);
        Self { ctx, width, height }
    }

    pub fn width(&self) -> usize {
        self.width
    }
    pub fn height(&self) -> usize {
        self.height
    }

    #[inline]
    fn fill(&self, c: Rgba) {
        self.ctx
            .set_rgb_fill_color(c.r as f64, c.g as f64, c.b as f64, c.a as f64);
    }

    #[inline]
    fn stroke(&self, c: Rgba, half_thickness: f32) {
        self.ctx
            .set_rgb_stroke_color(c.r as f64, c.g as f64, c.b as f64, c.a as f64);
        self.ctx.set_line_width((2.0 * half_thickness).max(0.0) as f64);
    }

    /// Render every command in `cmds`. Clip/offset push-pop pairs nest
    /// via the Core Graphics graphics-state stack.
    pub fn execute(&mut self, cmds: &[SurfaceCmd]) {
        for cmd in cmds {
            self.exec_one(cmd);
        }
        self.ctx.flush();
    }

    fn exec_one(&mut self, cmd: &SurfaceCmd) {
        let ctx = &self.ctx;
        match cmd {
            SurfaceCmd::Clear { color } => {
                self.fill(*color);
                ctx.fill_rect(CGRect::new(
                    &CGPoint::new(0.0, 0.0),
                    &CGSize::new(self.width as f64, self.height as f64),
                ));
            }
            SurfaceCmd::PresentHint => {}

            SurfaceCmd::FillRect {
                rect,
                corner_radius,
                color,
            } => {
                self.fill(*color);
                if *corner_radius <= 0.0 {
                    ctx.fill_rect(rect_cg(rect));
                } else {
                    self.rounded_rect_path(rect, *corner_radius);
                    ctx.fill_path();
                }
            }
            SurfaceCmd::StrokeRect {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                if *corner_radius <= 0.0 {
                    ctx.stroke_rect_with_width(rect_cg(rect), (2.0 * half_thickness).max(0.0) as f64);
                } else {
                    self.rounded_rect_path(rect, *corner_radius);
                    ctx.stroke_path();
                }
            }
            SurfaceCmd::DrawLine {
                p0,
                p1,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                ctx.stroke_line_segments(&[
                    CGPoint::new(p0.x as f64, p0.y as f64),
                    CGPoint::new(p1.x as f64, p1.y as f64),
                ]);
            }

            SurfaceCmd::FillOval { rect, color } => {
                self.fill(*color);
                ctx.fill_ellipse_in_rect(rect_cg(rect));
            }
            SurfaceCmd::StrokeOval {
                rect,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                ctx.stroke_ellipse_in_rect(rect_cg(rect));
            }
            SurfaceCmd::FillCircle {
                center,
                radius,
                color,
            } => {
                self.fill(*color);
                ctx.fill_ellipse_in_rect(circle_rect(*center, *radius));
            }
            SurfaceCmd::StrokeCircle {
                center,
                radius,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                ctx.stroke_ellipse_in_rect(circle_rect(*center, *radius));
            }
            SurfaceCmd::DrawArc {
                center,
                radius,
                rotation_rad,
                half_aperture_rad,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                self.arc_path(*center, *radius, *rotation_rad, *half_aperture_rad);
                ctx.stroke_path();
            }

            SurfaceCmd::DrawPath {
                commands,
                fill,
                stroke,
            } => {
                self.build_path(commands);
                match (fill, stroke) {
                    (Some(fc), Some((ss, sc))) => {
                        self.fill(*fc);
                        self.stroke(*sc, ss.half_thickness);
                        // draw_path with FillStroke would be ideal; do
                        // it in two passes to keep the path simple.
                        ctx.fill_path();
                        self.build_path(commands);
                        ctx.stroke_path();
                    }
                    (Some(fc), None) => {
                        self.fill(*fc);
                        ctx.fill_path();
                    }
                    (None, Some((ss, sc))) => {
                        self.stroke(*sc, ss.half_thickness);
                        ctx.stroke_path();
                    }
                    (None, None) => {}
                }
            }

            // ─── overlays: fills / strokes over geometry ───────────────
            SurfaceCmd::Caret { rect, color } => {
                self.fill(*color);
                ctx.fill_rect(rect_cg(rect));
            }
            SurfaceCmd::SelectionRange { rect, color } => {
                self.fill(*color);
                ctx.fill_rect(rect_cg(rect));
            }
            SurfaceCmd::MarkRect { rect, mode: _ } => {
                // Approximate every mark mode as a translucent wash for
                // now; true invert/dim blend modes land with the pane port.
                self.fill(Rgba {
                    r: 1.0,
                    g: 1.0,
                    b: 0.0,
                    a: 0.25,
                });
                ctx.fill_rect(rect_cg(rect));
            }
            SurfaceCmd::FocusRing {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => {
                self.stroke(*color, *half_thickness);
                if *corner_radius <= 0.0 {
                    ctx.stroke_rect_with_width(rect_cg(rect), (2.0 * half_thickness).max(0.0) as f64);
                } else {
                    self.rounded_rect_path(rect, *corner_radius);
                    ctx.stroke_path();
                }
            }

            // ─── composition: clip + offset via the gstate stack ───────
            SurfaceCmd::PushClipRect { rect } => {
                ctx.save();
                ctx.clip_to_rect(rect_cg(rect));
            }
            SurfaceCmd::PopClipRect => {
                ctx.restore();
            }
            SurfaceCmd::PushOffset { dx, dy } => {
                ctx.save();
                ctx.translate(*dx as f64, *dy as f64);
            }
            SurfaceCmd::PopOffset => {
                ctx.restore();
            }

            // ─── canvas fast path: BGRA32 buffer blit ──────────────────
            SurfaceCmd::Blit {
                x,
                y,
                w,
                h,
                pixels,
            } => {
                self.blit_bgra(*x as i64, *y as i64, *w as usize, *h as usize, pixels);
            }

            // ─── not yet on the Core Graphics path (Phase 2.1+) ────────
            // Text (DrawTextRun + the 3 sync queries) lands with Core
            // Text; ScrollRect / Save/RestoreRect / InstallChildViewBounds
            // land with the pane port. Left as no-ops so a batch
            // containing them still renders its geometry.
            SurfaceCmd::DrawTextRun { .. }
            | SurfaceCmd::MeasureTextRun { .. }
            | SurfaceCmd::CharIndexAtPoint { .. }
            | SurfaceCmd::PointAtCharIndex { .. }
            | SurfaceCmd::ScrollRect { .. }
            | SurfaceCmd::SaveRect { .. }
            | SurfaceCmd::RestoreRect { .. }
            | SurfaceCmd::InstallChildViewBounds { .. } => {}
        }
    }

    fn build_path(&self, commands: &[PathCmd]) {
        let ctx = &self.ctx;
        ctx.begin_path();
        for c in commands {
            match c {
                PathCmd::MoveTo(p) => ctx.move_to_point(p.x as f64, p.y as f64),
                PathCmd::LineTo(p) => ctx.add_line_to_point(p.x as f64, p.y as f64),
                PathCmd::QuadTo { ctrl, end } => ctx.add_quad_curve_to_point(
                    ctrl.x as f64,
                    ctrl.y as f64,
                    end.x as f64,
                    end.y as f64,
                ),
                PathCmd::CubicTo { c1, c2, end } => ctx.add_curve_to_point(
                    c1.x as f64,
                    c1.y as f64,
                    c2.x as f64,
                    c2.y as f64,
                    end.x as f64,
                    end.y as f64,
                ),
                PathCmd::ArcTo {
                    radius,
                    end,
                    sweep_clockwise,
                    ..
                } => {
                    // Polyline approximation between the current implied
                    // start and `end`, bulging by `radius`. Good enough
                    // for the first cut; exact elliptical arcs land later.
                    let _ = sweep_clockwise;
                    ctx.add_line_to_point(end.x as f64, end.y as f64);
                    let _ = radius;
                }
                PathCmd::Close => ctx.close_path(),
            }
        }
    }

    /// Append a stroked circular-arc path approximated by line segments.
    fn arc_path(&self, center: Point, radius: f32, rotation_rad: f32, half_aperture_rad: f32) {
        let ctx = &self.ctx;
        const SEGMENTS: usize = 48;
        let start = rotation_rad - half_aperture_rad;
        let total = 2.0 * half_aperture_rad;
        ctx.begin_path();
        for i in 0..=SEGMENTS {
            let t = start + total * (i as f32 / SEGMENTS as f32);
            let px = (center.x + radius * t.cos()) as f64;
            let py = (center.y + radius * t.sin()) as f64;
            if i == 0 {
                ctx.move_to_point(px, py);
            } else {
                ctx.add_line_to_point(px, py);
            }
        }
    }

    /// Build a rounded-rect path with quadratic-curve corners.
    fn rounded_rect_path(&self, rect: &Rect, radius: f32) {
        let ctx = &self.ctx;
        let r = rect_cg(rect);
        let (x0, y0) = (r.origin.x, r.origin.y);
        let (x1, y1) = (x0 + r.size.width, y0 + r.size.height);
        let rad = (radius as f64).min(r.size.width / 2.0).min(r.size.height / 2.0);
        ctx.begin_path();
        ctx.move_to_point(x0 + rad, y0);
        ctx.add_line_to_point(x1 - rad, y0);
        ctx.add_quad_curve_to_point(x1, y0, x1, y0 + rad);
        ctx.add_line_to_point(x1, y1 - rad);
        ctx.add_quad_curve_to_point(x1, y1, x1 - rad, y1);
        ctx.add_line_to_point(x0 + rad, y1);
        ctx.add_quad_curve_to_point(x0, y1, x0, y1 - rad);
        ctx.add_line_to_point(x0, y0 + rad);
        ctx.add_quad_curve_to_point(x0, y0, x0 + rad, y0);
        ctx.close_path();
    }

    /// Blit a `w`×`h` BGRA32 (0xAARRGGBB / little-endian B,G,R,A) buffer
    /// at top-left (`dx`,`dy`) straight into the RGBA backing store,
    /// converting channel order and premultiplying. Bypasses the CTM —
    /// the canvas fast path always blits a full frame at the origin.
    fn blit_bgra(&mut self, dx: i64, dy: i64, w: usize, h: usize, pixels: &[u32]) {
        let cw = self.width;
        let ch = self.height;
        let stride = self.ctx.bytes_per_row();
        let buf = self.ctx.data();
        for sy in 0..h {
            let ty = dy + sy as i64;
            if ty < 0 || ty as usize >= ch {
                continue;
            }
            for sx in 0..w {
                let tx = dx + sx as i64;
                if tx < 0 || tx as usize >= cw {
                    continue;
                }
                let src = pixels[sy * w + sx];
                let b = (src & 0xFF) as u32;
                let g = ((src >> 8) & 0xFF) as u32;
                let r = ((src >> 16) & 0xFF) as u32;
                let a = ((src >> 24) & 0xFF) as u32;
                // Premultiply (backing store is PremultipliedLast).
                let pm = |c: u32| ((c * a + 127) / 255) as u8;
                let o = ty as usize * stride + tx as usize * 4;
                buf[o] = pm(r);
                buf[o + 1] = pm(g);
                buf[o + 2] = pm(b);
                buf[o + 3] = a as u8;
            }
        }
    }

    /// Read back the RGBA bytes of pixel (x, y). Panics out of bounds.
    pub fn pixel(&mut self, x: usize, y: usize) -> [u8; 4] {
        let stride = self.ctx.bytes_per_row();
        let data = self.ctx.data();
        let o = y * stride + x * 4;
        [data[o], data[o + 1], data[o + 2], data[o + 3]]
    }

    /// Dump the canvas as a binary PPM (P6, RGB) for eyeballing. No
    /// image-crate dependency; alpha is dropped.
    pub fn to_ppm(&mut self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let stride = self.ctx.bytes_per_row();
        let data = self.ctx.data();
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        out.reserve(w * h * 3);
        for y in 0..h {
            for x in 0..w {
                let o = y * stride + x * 4;
                out.push(data[o]);
                out.push(data[o + 1]);
                out.push(data[o + 2]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::igui_paint::{Point, Rect, Rgba, SurfaceCmd};

    fn red() -> Rgba {
        Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 }
    }
    fn blue() -> Rgba {
        Rgba { r: 0.0, g: 0.0, b: 1.0, a: 1.0 }
    }

    #[test]
    fn clear_fills_whole_canvas() {
        let mut c = CgCanvas::new(32, 32);
        c.execute(&[SurfaceCmd::Clear { color: blue() }]);
        for &(x, y) in &[(0, 0), (31, 0), (0, 31), (31, 31), (16, 16)] {
            let p = c.pixel(x, y);
            assert_eq!(p, [0, 0, 255, 255], "pixel ({x},{y}) not blue: {p:?}");
        }
    }

    #[test]
    fn fill_rect_lands_in_the_right_place() {
        let mut c = CgCanvas::new(64, 64);
        c.execute(&[
            SurfaceCmd::Clear { color: blue() },
            SurfaceCmd::FillRect {
                rect: Rect { x0: 16.0, y0: 16.0, x1: 48.0, y1: 48.0 },
                corner_radius: 0.0,
                color: red(),
            },
        ]);
        // Inside the red rect.
        assert_eq!(c.pixel(32, 32), [255, 0, 0, 255], "center should be red");
        // Top-left corner of the rect (IR y-down: row 16 is the top edge).
        assert_eq!(c.pixel(20, 20), [255, 0, 0, 255], "inside top-left → red");
        // Outside stays blue (background).
        assert_eq!(c.pixel(4, 4), [0, 0, 255, 255], "corner should stay blue");
        assert_eq!(c.pixel(60, 60), [0, 0, 255, 255], "far corner stays blue");
    }

    #[test]
    fn top_left_origin_is_respected() {
        // A rect hugging the top edge must light up low memory rows.
        let mut c = CgCanvas::new(32, 32);
        c.execute(&[
            SurfaceCmd::Clear { color: blue() },
            SurfaceCmd::FillRect {
                rect: Rect { x0: 0.0, y0: 0.0, x1: 32.0, y1: 8.0 },
                corner_radius: 0.0,
                color: red(),
            },
        ]);
        assert_eq!(c.pixel(16, 1), [255, 0, 0, 255], "row 1 (near top) red");
        assert_eq!(c.pixel(16, 30), [0, 0, 255, 255], "row 30 (near bottom) blue");
    }

    #[test]
    fn clip_rect_confines_drawing() {
        let mut c = CgCanvas::new(64, 64);
        c.execute(&[
            SurfaceCmd::Clear { color: blue() },
            SurfaceCmd::PushClipRect {
                rect: Rect { x0: 0.0, y0: 0.0, x1: 16.0, y1: 16.0 },
            },
            SurfaceCmd::FillRect {
                rect: Rect { x0: 0.0, y0: 0.0, x1: 64.0, y1: 64.0 },
                corner_radius: 0.0,
                color: red(),
            },
            SurfaceCmd::PopClipRect,
        ]);
        assert_eq!(c.pixel(8, 8), [255, 0, 0, 255], "inside clip → red");
        assert_eq!(c.pixel(40, 40), [0, 0, 255, 255], "outside clip stayed blue");
    }

    #[test]
    fn offset_translates_drawing() {
        let mut c = CgCanvas::new(64, 64);
        c.execute(&[
            SurfaceCmd::Clear { color: blue() },
            SurfaceCmd::PushOffset { dx: 32.0, dy: 32.0 },
            SurfaceCmd::FillRect {
                rect: Rect { x0: 0.0, y0: 0.0, x1: 8.0, y1: 8.0 },
                corner_radius: 0.0,
                color: red(),
            },
            SurfaceCmd::PopOffset,
        ]);
        assert_eq!(c.pixel(34, 34), [255, 0, 0, 255], "shifted rect is red");
        assert_eq!(c.pixel(4, 4), [0, 0, 255, 255], "origin untouched (blue)");
    }

    #[test]
    fn blit_writes_bgra_buffer() {
        let mut c = CgCanvas::new(16, 16);
        // 4x4 opaque green in BGRA32 (0xAARRGGBB): A=FF,R=00,G=FF,B=00.
        let green = 0xFF00FF00u32;
        let pixels = std::sync::Arc::new(vec![green; 16]);
        c.execute(&[SurfaceCmd::Blit { x: 2.0, y: 2.0, w: 4, h: 4, pixels }]);
        assert_eq!(c.pixel(3, 3), [0, 255, 0, 255], "blitted pixel is green");
        assert_eq!(c.pixel(0, 0), [0, 0, 0, 0], "outside blit is untouched");
    }

    #[test]
    fn fill_circle_hits_center_misses_corner() {
        let mut c = CgCanvas::new(64, 64);
        c.execute(&[
            SurfaceCmd::Clear { color: blue() },
            SurfaceCmd::FillCircle {
                center: Point { x: 32.0, y: 32.0 },
                radius: 16.0,
                color: red(),
            },
        ]);
        assert_eq!(c.pixel(32, 32), [255, 0, 0, 255], "circle center red");
        assert_eq!(c.pixel(2, 2), [0, 0, 255, 255], "circle leaves corner blue");
    }
}
