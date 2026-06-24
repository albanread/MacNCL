//! MDI child windows + their render-host children.
//!
//! Architecture: each "document" is two HWNDs:
//!
//! 1. **MDI child** — the document window the user sees. Owns the
//!    title bar, MDI activate/close behavior, and lifetime. Created
//!    inside the MDI client via `WM_MDICREATE`. Style:
//!    `WS_OVERLAPPEDWINDOW | WS_VISIBLE`.
//!
//! 2. **Render host** — a borderless `WS_CHILD | WS_VISIBLE |
//!    WS_CLIPSIBLINGS` window inside the MDI child's client area.
//!    Owns the WM_PAINT loop and the active Phase 3b renderer.
//!
//! The current renderer prefers a per-window Direct2D HWND render target.
//! The older GDI path remains as a fallback because it was the first path
//! that produced visible pixels during bring-up and is still useful when
//! diagnosing target-creation or EndDraw failures.

#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;

use windows::core::{w, Error, PCWSTR};
use windows_numerics::Vector2;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_ELLIPSE, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE,
};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, FillRect, FrameRect,
    HBRUSH, LineTo, MoveToEx, PAINTSTRUCT, PS_SOLID, RoundRect, SelectObject,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefMDIChildProcW, DefWindowProcW, DestroyWindow, GetClientRect, GetParent,
    GetWindowLongPtrW, IsWindow, IsWindowVisible, KillTimer, LoadCursorW, RegisterClassExW,
    SendMessageW, SetTimer, SetWindowLongPtrW, SetWindowPos, CREATESTRUCTW,
    GWLP_USERDATA, IDC_ARROW, MDICREATESTRUCTW, SWP_NOACTIVATE, SWP_NOZORDER,
    WHEEL_DELTA, WINDOW_EX_STYLE, WM_CHAR, WM_CLOSE, WM_DPICHANGED_AFTERPARENT, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MDIDESTROY, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SETTEXT,
    WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW, WNDCLASS_STYLES,
    WS_CHILD, WS_CLIPSIBLINGS, WS_VISIBLE,
};

use super::batch as batch_mod;
use super::batch::SurfaceCmd;
use super::channels::{self, IGuiEvent};
use super::registry;
use super::renderer;
use super::window;
use super::IGuiError;

pub(crate) const MDI_CHILD_CLASS: PCWSTR = w!("NewCL.iGui.Child");
pub(crate) const RENDER_HOST_CLASS: PCWSTR = w!("NewCL.iGui.Render");

// ─── ChildState lives on the render host ─────────────────────────────

/// Per-child renderer state, stored in `GWLP_USERDATA` of the visible
/// MDI child HWND. Created in `WM_NCCREATE`, dropped in `WM_NCDESTROY`.
pub(crate) struct ChildState {
    pub(crate) child_id: i64,
    pub(crate) hwnd: HWND,
    pub(crate) target: Option<ID2D1HwndRenderTarget>,
    pub(crate) logged_hwnd_status: bool,
    pub(crate) last_logged_sequence: Option<u64>,
}

impl ChildState {
    fn render(&mut self, hdc: windows::Win32::Graphics::Gdi::HDC) -> Result<(), IGuiError> {
        if !self.logged_hwnd_status {
            if let Some(mdi_hwnd) = registry::mdi_hwnd_of(self.child_id) {
                log_hwnd_monitor("first-paint", self.child_id, mdi_hwnd, self.hwnd);
            } else {
                eprintln!(
                    "[igui-hwnd] first-paint child={} render={:?} missing mdi registry entry",
                    self.child_id, self.hwnd
                );
            }
            self.logged_hwnd_status = true;
        }

        let mut rect = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rect) }
            .map_err(|e| IGuiError::Win32(format!("render-host GetClientRect failed: {e}")))?;
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;
        if width == 0 || height == 0 {
            return Ok(());
        }

        self.ensure_render_target(width, height)?;

        let pending = batch_mod::snapshot(self.child_id);

        match pending.as_ref() {
            Some(batch) if self.last_logged_sequence != Some(batch.sequence) => {
                log_ui_batch(self.child_id, batch);
                self.last_logged_sequence = Some(batch.sequence);
            }
            None if self.last_logged_sequence.is_some() => {
                eprintln!(
                    "[igui-batch-ui] child={} no batch available at paint",
                    self.child_id
                );
                self.last_logged_sequence = None;
            }
            _ => {}
        }

        if let Some(target) = self.target.as_ref() {
            match render_d2d_frame(target, self.child_id, pending.as_deref()) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    eprintln!(
                        "[igui-d2d] child={} render failed, falling back to GDI: {}",
                        self.child_id, err
                    );
                    self.target = None;
                }
            }
        }

        if let Some(batch) = pending.as_ref() {
            execute_gdi_batch(hdc, &rect, batch)?;
        } else {
            let color = phase3a_palette(self.child_id);
            fill_rect_color(hdc, &rect, rgba_to_colorref(color[0], color[1], color[2]))?;
        }
        Ok(())
    }

    fn ensure_render_target(&mut self, width: u32, height: u32) -> Result<(), IGuiError> {
        if let Some(target) = self.target.as_ref() {
            let current = unsafe { target.GetPixelSize() };
            if current.width == width && current.height == height {
                return Ok(());
            }
            unsafe { target.Resize(&D2D_SIZE_U { width, height }) }
                .map_err(|e| IGuiError::D2D(format!("ID2D1HwndRenderTarget::Resize failed: {e}")))?;
            return Ok(());
        }

        let factory = &renderer::ctx().d2d.factory;
        let target = unsafe {
            factory.CreateHwndRenderTarget(
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_IGNORE,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                },
                &D2D1_HWND_RENDER_TARGET_PROPERTIES {
                    hwnd: self.hwnd,
                    pixelSize: D2D_SIZE_U { width, height },
                    presentOptions: D2D1_PRESENT_OPTIONS_NONE,
                },
            )
        }
        .map_err(|e| IGuiError::D2D(format!("CreateHwndRenderTarget failed: {e}")))?;
        self.target = Some(target);
        Ok(())
    }

    fn handle_resize(&mut self, width: u32, height: u32) -> Result<(), IGuiError> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        self.ensure_render_target(width, height)
    }
}

fn render_d2d_frame(
    target: &ID2D1HwndRenderTarget,
    child_id: i64,
    batch: Option<&batch_mod::PaneBatch>,
) -> Result<(), IGuiError> {
    unsafe { target.BeginDraw() };

    match batch {
        Some(batch) => execute_d2d_batch(target, batch)?,
        None => {
            let color = phase3a_palette(child_id);
            unsafe {
                target.Clear(Some(&D2D1_COLOR_F {
                    r: color[0],
                    g: color[1],
                    b: color[2],
                    a: 1.0,
                }))
            };
        }
    }

    unsafe { target.EndDraw(None, None) }
        .map_err(|e| IGuiError::D2D(format!("ID2D1HwndRenderTarget::EndDraw failed: {e}")))?;
    Ok(())
}

fn execute_d2d_batch(
    target: &ID2D1HwndRenderTarget,
    batch: &batch_mod::PaneBatch,
) -> Result<(), IGuiError> {
    for cmd in &batch.cmds {
        match cmd {
            SurfaceCmd::Clear { color } => unsafe {
                target.Clear(Some(&D2D1_COLOR_F {
                    r: color.r,
                    g: color.g,
                    b: color.b,
                    a: color.a,
                }));
            },
            SurfaceCmd::PresentHint => {}
            SurfaceCmd::Blit { x, y, w, h, pixels } => {
                // Canvas fast path: upload the host BGRA32 buffer to a
                // D2D bitmap and draw it 1:1. One CreateBitmap+DrawBitmap
                // per frame — the per-pixel work happened in Lisp via
                // direct memory pokes (see igui::canvas).
                let need = (*w as usize) * (*h as usize);
                if *w > 0 && *h > 0 && pixels.len() >= need {
                    use windows::Win32::Graphics::Direct2D::{
                        D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
                    };
                    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
                    let bitmap = unsafe {
                        target.CreateBitmap(
                            D2D_SIZE_U { width: *w, height: *h },
                            Some(pixels.as_ptr() as *const core::ffi::c_void),
                            *w * 4,
                            &D2D1_BITMAP_PROPERTIES {
                                pixelFormat: D2D1_PIXEL_FORMAT {
                                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                                },
                                dpiX: 96.0,
                                dpiY: 96.0,
                            },
                        )
                    }
                    .map_err(|e| IGuiError::D2D(format!("CreateBitmap (Blit): {e}")))?;
                    let dst = D2D_RECT_F {
                        left: *x,
                        top: *y,
                        right: *x + *w as f32,
                        bottom: *y + *h as f32,
                    };
                    unsafe {
                        target.DrawBitmap(
                            &bitmap,
                            Some(&dst),
                            1.0,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        )
                    };
                }
            }
            SurfaceCmd::FillRect {
                rect,
                corner_radius,
                color,
            } => {
                let brush = unsafe {
                    target.CreateSolidColorBrush(
                        &D2D1_COLOR_F {
                            r: color.r,
                            g: color.g,
                            b: color.b,
                            a: color.a,
                        },
                        None,
                    )
                }
                .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush failed: {e}")))?;
                let d2d_rect = D2D_RECT_F {
                    left: rect.x0,
                    top: rect.y0,
                    right: rect.x1,
                    bottom: rect.y1,
                };
                if *corner_radius <= 0.0 {
                    unsafe { target.FillRectangle(&d2d_rect, &brush) };
                } else {
                    unsafe {
                        target.FillRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: d2d_rect,
                                radiusX: *corner_radius,
                                radiusY: *corner_radius,
                            },
                            &brush,
                        )
                    };
                }
            }
            SurfaceCmd::StrokeRect {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => {
                let brush = unsafe {
                    target.CreateSolidColorBrush(
                        &D2D1_COLOR_F {
                            r: color.r,
                            g: color.g,
                            b: color.b,
                            a: color.a,
                        },
                        None,
                    )
                }
                .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush failed: {e}")))?;
                let d2d_rect = D2D_RECT_F {
                    left: rect.x0,
                    top: rect.y0,
                    right: rect.x1,
                    bottom: rect.y1,
                };
                let stroke_w = (2.0 * half_thickness).max(0.0);
                if *corner_radius <= 0.0 {
                    unsafe { target.DrawRectangle(&d2d_rect, &brush, stroke_w, None) };
                } else {
                    unsafe {
                        target.DrawRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: d2d_rect,
                                radiusX: *corner_radius,
                                radiusY: *corner_radius,
                            },
                            &brush,
                            stroke_w,
                            None,
                        )
                    };
                }
            }
            SurfaceCmd::DrawLine {
                p0,
                p1,
                half_thickness,
                color,
            } => {
                let brush = unsafe {
                    target.CreateSolidColorBrush(
                        &D2D1_COLOR_F {
                            r: color.r,
                            g: color.g,
                            b: color.b,
                            a: color.a,
                        },
                        None,
                    )
                }
                .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush failed: {e}")))?;
                unsafe {
                    target.DrawLine(
                        Vector2 { X: p0.x, Y: p0.y },
                        Vector2 { X: p1.x, Y: p1.y },
                        &brush,
                        (2.0 * half_thickness).max(0.0),
                        None,
                    )
                };
            }
            // ─── Phase 3c geometry primitives (HwndRenderTarget) ────────
            SurfaceCmd::FillOval { rect, color } => {
                let brush = solid_brush(target, *color)?;
                let ellipse = ellipse_from_rect(rect);
                unsafe { target.FillEllipse(&ellipse, &brush) };
            }
            SurfaceCmd::FillCircle {
                center,
                radius,
                color,
            } => {
                let brush = solid_brush(target, *color)?;
                let ellipse = D2D1_ELLIPSE {
                    point: Vector2 { X: center.x, Y: center.y },
                    radiusX: *radius,
                    radiusY: *radius,
                };
                unsafe { target.FillEllipse(&ellipse, &brush) };
            }
            SurfaceCmd::StrokeOval {
                rect,
                half_thickness,
                color,
            } => {
                let brush = solid_brush(target, *color)?;
                let ellipse = ellipse_from_rect(rect);
                let stroke_w = (2.0 * half_thickness).max(0.0);
                unsafe { target.DrawEllipse(&ellipse, &brush, stroke_w, None) };
            }
            SurfaceCmd::StrokeCircle {
                center,
                radius,
                half_thickness,
                color,
            } => {
                let brush = solid_brush(target, *color)?;
                let ellipse = D2D1_ELLIPSE {
                    point: Vector2 { X: center.x, Y: center.y },
                    radiusX: *radius,
                    radiusY: *radius,
                };
                let stroke_w = (2.0 * half_thickness).max(0.0);
                unsafe { target.DrawEllipse(&ellipse, &brush, stroke_w, None) };
            }
            SurfaceCmd::DrawArc {
                center,
                radius,
                rotation_rad,
                half_aperture_rad,
                half_thickness,
                color,
            } => {
                let brush = solid_brush(target, *color)?;
                let stroke_w = (2.0 * half_thickness).max(0.0);
                draw_arc_hwnd(
                    target,
                    *center,
                    *radius,
                    *rotation_rad,
                    *half_aperture_rad,
                    stroke_w,
                    &brush,
                )?;
            }
            // ─── Phase 4: text ─────────────────────────────────────
            SurfaceCmd::DrawTextRun { run } => {
                draw_text_run(target, run)?;
            }
            SurfaceCmd::MeasureTextRun { request_id, run } => {
                run_measure(*request_id, run);
            }
            SurfaceCmd::CharIndexAtPoint { request_id, run, point } => {
                run_hit_test_point(*request_id, run, *point);
            }
            SurfaceCmd::PointAtCharIndex {
                request_id,
                run,
                char_index,
            } => {
                run_hit_test_position(*request_id, run, *char_index);
            }
            // ─── Phase 5: composition ──────────────────────────────
            SurfaceCmd::PushClipRect { rect } => {
                let r2d = D2D_RECT_F {
                    left: rect.x0, top: rect.y0,
                    right: rect.x1, bottom: rect.y1,
                };
                unsafe {
                    target.PushAxisAlignedClip(
                        &r2d,
                        windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    )
                };
            }
            SurfaceCmd::PopClipRect => {
                unsafe { target.PopAxisAlignedClip() };
            }
            SurfaceCmd::PushOffset { dx, dy } => {
                push_offset(target, *dx, *dy);
            }
            SurfaceCmd::PopOffset => {
                pop_offset(target);
            }
            SurfaceCmd::ScrollRect { rect, dx, dy } => {
                // No native intra-buffer copy on ID2D1HwndRenderTarget;
                // scroll requires a full-buffer save / restore. For
                // Phase 5 we leave it as a no-op that documents the
                // intent — most CP-side users will be re-issuing draw
                // batches after a scroll anyway.
                eprintln!(
                    "[igui-d2d] ScrollRect rect=({:.1}, {:.1})-({:.1}, {:.1}) dx={:.1} dy={:.1} — noop (re-submit batch instead)",
                    rect.x0, rect.y0, rect.x1, rect.y1, dx, dy
                );
            }
            SurfaceCmd::SaveRect { slot, rect } => {
                save_rect_slot(target, *slot, *rect)?;
            }
            SurfaceCmd::RestoreRect { slot } => {
                restore_rect_slot(target, *slot)?;
            }
            SurfaceCmd::InstallChildViewBounds { child_view_id, rect } => {
                child_bounds_install(*child_view_id, *rect);
            }
            // ─── Phase 5: overlays ─────────────────────────────────
            SurfaceCmd::MarkRect { rect, mode } => {
                let palette = super::system_colors::lookup;
                let mark_color = match mode {
                    batch_mod::MarkMode::Highlight => {
                        let bg = palette(super::system_colors::kind::SELECTION_BG);
                        batch_mod::Rgba { r: bg.r, g: bg.g, b: bg.b, a: 0.30 }
                    }
                    batch_mod::MarkMode::Invert => {
                        // Simple invert via ~50% white XOR-ish overlay.
                        // ID2D1HwndRenderTarget doesn't expose composite
                        // modes; this is a perceptual approximation.
                        batch_mod::Rgba { r: 1.0, g: 1.0, b: 1.0, a: 0.50 }
                    }
                    batch_mod::MarkMode::Dim25 => {
                        let bg = palette(super::system_colors::kind::WINDOW_BG);
                        batch_mod::Rgba { r: bg.r, g: bg.g, b: bg.b, a: 0.25 }
                    }
                    batch_mod::MarkMode::Dim50 => {
                        let bg = palette(super::system_colors::kind::WINDOW_BG);
                        batch_mod::Rgba { r: bg.r, g: bg.g, b: bg.b, a: 0.50 }
                    }
                    batch_mod::MarkMode::Dim75 => {
                        let bg = palette(super::system_colors::kind::WINDOW_BG);
                        batch_mod::Rgba { r: bg.r, g: bg.g, b: bg.b, a: 0.75 }
                    }
                };
                let brush = solid_brush(target, mark_color)?;
                let r2d = D2D_RECT_F {
                    left: rect.x0, top: rect.y0,
                    right: rect.x1, bottom: rect.y1,
                };
                unsafe { target.FillRectangle(&r2d, &brush) };
            }
            SurfaceCmd::Caret { rect, color } => {
                let brush = solid_brush(target, *color)?;
                let r2d = D2D_RECT_F {
                    left: rect.x0, top: rect.y0,
                    right: rect.x1, bottom: rect.y1,
                };
                unsafe { target.FillRectangle(&r2d, &brush) };
            }
            SurfaceCmd::SelectionRange { rect, color } => {
                let brush = solid_brush(target, *color)?;
                let r2d = D2D_RECT_F {
                    left: rect.x0, top: rect.y0,
                    right: rect.x1, bottom: rect.y1,
                };
                unsafe { target.FillRectangle(&r2d, &brush) };
            }
            SurfaceCmd::FocusRing { rect, corner_radius, half_thickness, color } => {
                let brush = solid_brush(target, *color)?;
                let r2d = D2D_RECT_F {
                    left: rect.x0, top: rect.y0,
                    right: rect.x1, bottom: rect.y1,
                };
                let stroke_w = (2.0 * half_thickness).max(0.0);
                if *corner_radius <= 0.0 {
                    unsafe { target.DrawRectangle(&r2d, &brush, stroke_w, None) };
                } else {
                    unsafe {
                        target.DrawRoundedRectangle(
                            &D2D1_ROUNDED_RECT {
                                rect: r2d,
                                radiusX: *corner_radius,
                                radiusY: *corner_radius,
                            },
                            &brush,
                            stroke_w,
                            None,
                        )
                    };
                }
            }
            SurfaceCmd::DrawPath { commands, fill, stroke } => {
                draw_path(target, commands, fill, stroke.as_ref())?;
            }
        }
    }
    Ok(())
}

// ─── Phase 4 text execution helpers ──────────────────────────────────

fn draw_text_run(
    target: &ID2D1HwndRenderTarget,
    run: &batch_mod::TextRun,
) -> Result<(), IGuiError> {
    let layout = super::dwrite::layout_for(run)?;
    let brush = solid_brush(target, run.color)?;
    let origin = Vector2 {
        X: run.origin.x,
        Y: run.origin.y,
    };
    unsafe {
        target.DrawTextLayout(
            origin,
            &layout,
            &brush,
            windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_NONE,
        )
    };
    Ok(())
}

fn run_measure(request_id: u32, run: &batch_mod::TextRun) {
    let reply = match super::dwrite::layout_for(run) {
        Ok(layout) => {
            let mut m =
                windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
            match unsafe { layout.GetMetrics(&mut m) } {
                Ok(()) => {
                    let ascent = first_line_ascent(&layout).unwrap_or(0.0);
                    super::replies::Reply::Metrics {
                        width: m.width,
                        height: m.height,
                        ascent,
                        line_count: m.lineCount,
                    }
                }
                Err(e) => super::replies::Reply::Failed {
                    message: format!("GetMetrics: {e}"),
                },
            }
        }
        Err(e) => super::replies::Reply::Failed {
            message: format!("{e}"),
        },
    };
    super::replies::deliver(request_id, reply);
}

fn first_line_ascent(layout: &windows::Win32::Graphics::DirectWrite::IDWriteTextLayout) -> Option<f32> {
    let mut count: u32 = 0;
    // First call gets the required count.
    let _ = unsafe {
        layout.GetLineMetrics(None, &mut count)
    };
    if count == 0 {
        return None;
    }
    let mut buf = vec![
        windows::Win32::Graphics::DirectWrite::DWRITE_LINE_METRICS::default();
        count as usize
    ];
    let mut actual: u32 = 0;
    unsafe { layout.GetLineMetrics(Some(&mut buf), &mut actual) }.ok()?;
    Some(buf.first()?.baseline)
}

fn run_hit_test_point(request_id: u32, run: &batch_mod::TextRun, point: batch_mod::Point) {
    let reply = match super::dwrite::layout_for(run) {
        Ok(layout) => {
            let mut is_trailing = windows::core::BOOL(0);
            let mut is_inside = windows::core::BOOL(0);
            let mut metrics =
                windows::Win32::Graphics::DirectWrite::DWRITE_HIT_TEST_METRICS::default();
            match unsafe {
                layout.HitTestPoint(
                    point.x - run.origin.x,
                    point.y - run.origin.y,
                    &mut is_trailing,
                    &mut is_inside,
                    &mut metrics,
                )
            } {
                Ok(()) => super::replies::Reply::HitTestPoint {
                    char_index: metrics.textPosition,
                    is_inside: is_inside.as_bool(),
                    is_trailing: is_trailing.as_bool(),
                },
                Err(e) => super::replies::Reply::Failed {
                    message: format!("HitTestPoint: {e}"),
                },
            }
        }
        Err(e) => super::replies::Reply::Failed {
            message: format!("{e}"),
        },
    };
    super::replies::deliver(request_id, reply);
}

// ─── Phase 5: composition stacks + paths ────────────────────────────

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    static OFFSET_STACK: RefCell<Vec<windows_numerics::Matrix3x2>> =
        RefCell::new(Vec::new());
}

fn push_offset(target: &ID2D1HwndRenderTarget, dx: f32, dy: f32) {
    let mut current = windows_numerics::Matrix3x2::default();
    unsafe { target.GetTransform(&mut current) };
    OFFSET_STACK.with(|st| st.borrow_mut().push(current));
    let translation = windows_numerics::Matrix3x2 {
        M11: 1.0, M12: 0.0,
        M21: 0.0, M22: 1.0,
        M31: dx, M32: dy,
    };
    let new_t = mul_matrix(&current, &translation);
    unsafe { target.SetTransform(&new_t) };
}

fn pop_offset(target: &ID2D1HwndRenderTarget) {
    if let Some(prev) = OFFSET_STACK.with(|st| st.borrow_mut().pop()) {
        unsafe { target.SetTransform(&prev) };
    } else {
        eprintln!("[igui-d2d] PopOffset on empty stack — ignored");
    }
}

fn mul_matrix(
    a: &windows_numerics::Matrix3x2,
    b: &windows_numerics::Matrix3x2,
) -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

// ─── SaveRect / RestoreRect slots ───────────────────────────────────
//
// 8 slots per pane. Each slot stores a (rect, ID2D1Bitmap) pair.
// Lazy-allocated on first save. Slot bitmaps live in CPU-readable
// memory because ID2D1HwndRenderTarget doesn't expose direct GPU
// readback; for Phase 5 we keep this simple and accept the cost.

struct SlotEntry {
    rect: batch_mod::Rect,
    bitmap: windows::Win32::Graphics::Direct2D::ID2D1Bitmap,
}

thread_local! {
    static SLOTS: RefCell<HashMap<u8, SlotEntry>> = RefCell::new(HashMap::new());
}

fn save_rect_slot(
    target: &ID2D1HwndRenderTarget,
    slot: u8,
    rect: batch_mod::Rect,
) -> Result<(), IGuiError> {
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::{
        D2D1_BITMAP_PROPERTIES,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    let width = (rect.x1 - rect.x0).abs().ceil().max(1.0) as u32;
    let height = (rect.y1 - rect.y0).abs().ceil().max(1.0) as u32;

    let bitmap = unsafe {
        target.CreateBitmap(
            D2D_SIZE_U { width, height },
            None,
            0,
            &D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
            },
        )
    }
    .map_err(|e| IGuiError::D2D(format!("CreateBitmap (SaveRect slot): {e}")))?;

    let dst_origin = windows::Win32::Graphics::Direct2D::Common::D2D_POINT_2U { x: 0, y: 0 };
    let src_rect = windows::Win32::Graphics::Direct2D::Common::D2D_RECT_U {
        left: rect.x0.max(0.0) as u32,
        top: rect.y0.max(0.0) as u32,
        right: rect.x1.max(0.0) as u32,
        bottom: rect.y1.max(0.0) as u32,
    };
    unsafe { bitmap.CopyFromRenderTarget(Some(&dst_origin), target, Some(&src_rect)) }
        .map_err(|e| IGuiError::D2D(format!("CopyFromRenderTarget: {e}")))?;

    SLOTS.with(|s| {
        s.borrow_mut().insert(slot, SlotEntry { rect, bitmap });
    });
    Ok(())
}

fn restore_rect_slot(
    target: &ID2D1HwndRenderTarget,
    slot: u8,
) -> Result<(), IGuiError> {
    let entry = SLOTS.with(|s| {
        s.borrow().get(&slot).map(|e| (e.rect, e.bitmap.clone()))
    });
    let Some((rect, bitmap)) = entry else {
        eprintln!("[igui-d2d] RestoreRect slot {slot} empty");
        return Ok(());
    };
    let dst = D2D_RECT_F {
        left: rect.x0, top: rect.y0, right: rect.x1, bottom: rect.y1,
    };
    let size = unsafe { bitmap.GetSize() };
    let src = D2D_RECT_F {
        left: 0.0, top: 0.0, right: size.width, bottom: size.height,
    };
    unsafe {
        target.DrawBitmap(
            &bitmap,
            Some(&dst),
            1.0,
            windows::Win32::Graphics::Direct2D::D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            Some(&src),
        )
    };
    Ok(())
}

// ─── Retained child-view bounds ─────────────────────────────────────

thread_local! {
    static CHILD_BOUNDS: RefCell<HashMap<u32, batch_mod::Rect>> =
        RefCell::new(HashMap::new());
}

fn child_bounds_install(child_view_id: u32, rect: batch_mod::Rect) {
    CHILD_BOUNDS.with(|c| {
        c.borrow_mut().insert(child_view_id, rect);
    });
}

#[allow(dead_code)] // queried by future render-time child composition
pub(crate) fn child_bounds_lookup(child_view_id: u32) -> Option<batch_mod::Rect> {
    CHILD_BOUNDS.with(|c| c.borrow().get(&child_view_id).copied())
}

// ─── DrawPath via ID2D1PathGeometry ─────────────────────────────────

fn draw_path(
    target: &ID2D1HwndRenderTarget,
    commands: &[batch_mod::PathCmd],
    fill: &Option<batch_mod::Rgba>,
    stroke: Option<&(batch_mod::StrokeStyle, batch_mod::Rgba)>,
) -> Result<(), IGuiError> {
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_CLOSED,
        D2D1_FIGURE_END_OPEN,
    };
    use windows::Win32::Graphics::Direct2D::Common::D2D1_BEZIER_SEGMENT;
    use windows::Win32::Graphics::Direct2D::{
        D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
        D2D1_QUADRATIC_BEZIER_SEGMENT, D2D1_STROKE_STYLE_PROPERTIES1,
        D2D1_STROKE_TRANSFORM_TYPE_NORMAL, D2D1_SWEEP_DIRECTION_CLOCKWISE,
        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE,
    };

    let factory = &renderer::ctx().d2d.factory;
    let geometry = unsafe { factory.CreatePathGeometry() }
        .map_err(|e| IGuiError::D2D(format!("CreatePathGeometry: {e}")))?;
    let sink = unsafe { geometry.Open() }
        .map_err(|e| IGuiError::D2D(format!("PathGeometry::Open: {e}")))?;

    let mut figure_open = false;
    let begin_kind = if fill.is_some() {
        D2D1_FIGURE_BEGIN_FILLED
    } else {
        D2D1_FIGURE_BEGIN_HOLLOW
    };

    for cmd in commands {
        match cmd {
            batch_mod::PathCmd::MoveTo(p) => {
                if figure_open {
                    unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
                }
                unsafe {
                    sink.BeginFigure(
                        Vector2 { X: p.x, Y: p.y },
                        begin_kind,
                    )
                };
                figure_open = true;
            }
            batch_mod::PathCmd::LineTo(p) => unsafe {
                sink.AddLine(Vector2 { X: p.x, Y: p.y })
            },
            batch_mod::PathCmd::QuadTo { ctrl, end } => unsafe {
                sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                    point1: Vector2 { X: ctrl.x, Y: ctrl.y },
                    point2: Vector2 { X: end.x, Y: end.y },
                })
            },
            batch_mod::PathCmd::CubicTo { c1, c2, end } => unsafe {
                sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: Vector2 { X: c1.x, Y: c1.y },
                    point2: Vector2 { X: c2.x, Y: c2.y },
                    point3: Vector2 { X: end.x, Y: end.y },
                })
            },
            batch_mod::PathCmd::ArcTo {
                radius,
                rotation_rad,
                large_arc,
                sweep_clockwise,
                end,
            } => unsafe {
                sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: Vector2 { X: end.x, Y: end.y },
                    size: D2D_SIZE_F {
                        width: radius.x,
                        height: radius.y,
                    },
                    rotationAngle: rotation_rad.to_degrees(),
                    sweepDirection: if *sweep_clockwise {
                        D2D1_SWEEP_DIRECTION_CLOCKWISE
                    } else {
                        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                    },
                    arcSize: if *large_arc {
                        D2D1_ARC_SIZE_LARGE
                    } else {
                        D2D1_ARC_SIZE_SMALL
                    },
                })
            },
            batch_mod::PathCmd::Close => {
                if figure_open {
                    unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
                    figure_open = false;
                }
            }
        }
    }
    if figure_open {
        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
    }
    unsafe { sink.Close() }
        .map_err(|e| IGuiError::D2D(format!("GeometrySink::Close: {e}")))?;

    if let Some(fill_color) = fill {
        let brush = solid_brush(target, *fill_color)?;
        unsafe { target.FillGeometry(&geometry, &brush, None) };
    }
    if let Some((style, stroke_color)) = stroke {
        let brush = solid_brush(target, *stroke_color)?;
        let stroke_w = (2.0 * style.half_thickness).max(0.0);
        // Stroke style props
        let cap = match style.line_cap {
            batch_mod::LineCap::Flat => windows::Win32::Graphics::Direct2D::D2D1_CAP_STYLE_FLAT,
            batch_mod::LineCap::Round => windows::Win32::Graphics::Direct2D::D2D1_CAP_STYLE_ROUND,
            batch_mod::LineCap::Square => windows::Win32::Graphics::Direct2D::D2D1_CAP_STYLE_SQUARE,
        };
        let join = match style.line_join {
            batch_mod::LineJoin::Miter => windows::Win32::Graphics::Direct2D::D2D1_LINE_JOIN_MITER,
            batch_mod::LineJoin::Round => windows::Win32::Graphics::Direct2D::D2D1_LINE_JOIN_ROUND,
            batch_mod::LineJoin::Bevel => windows::Win32::Graphics::Direct2D::D2D1_LINE_JOIN_BEVEL,
        };
        let dash_style = if style.dash_pattern.is_some() {
            windows::Win32::Graphics::Direct2D::D2D1_DASH_STYLE_CUSTOM
        } else {
            windows::Win32::Graphics::Direct2D::D2D1_DASH_STYLE_SOLID
        };
        let props = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: cap,
            endCap: cap,
            dashCap: cap,
            lineJoin: join,
            miterLimit: style.miter_limit.max(1.0),
            dashStyle: dash_style,
            dashOffset: 0.0,
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let dashes_slice = style.dash_pattern.as_deref();
        let stroke_style = unsafe {
            factory.CreateStrokeStyle(&props, dashes_slice)
        }
        .map_err(|e| IGuiError::D2D(format!("CreateStrokeStyle: {e}")))?;
        unsafe { target.DrawGeometry(&geometry, &brush, stroke_w, &stroke_style) };
    }
    Ok(())
}

fn run_hit_test_position(request_id: u32, run: &batch_mod::TextRun, char_index: u32) {
    let reply = match super::dwrite::layout_for(run) {
        Ok(layout) => {
            let mut x: f32 = 0.0;
            let mut y: f32 = 0.0;
            let mut metrics =
                windows::Win32::Graphics::DirectWrite::DWRITE_HIT_TEST_METRICS::default();
            match unsafe {
                layout.HitTestTextPosition(char_index, false, &mut x, &mut y, &mut metrics)
            } {
                Ok(()) => super::replies::Reply::HitTestPosition {
                    x: x + run.origin.x,
                    y: y + run.origin.y,
                    height: metrics.height,
                },
                Err(e) => super::replies::Reply::Failed {
                    message: format!("HitTestTextPosition: {e}"),
                },
            }
        }
        Err(e) => super::replies::Reply::Failed {
            message: format!("{e}"),
        },
    };
    super::replies::deliver(request_id, reply);
}

fn solid_brush(
    target: &ID2D1HwndRenderTarget,
    color: batch_mod::Rgba,
) -> Result<windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush, IGuiError> {
    unsafe {
        target.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: color.r,
                g: color.g,
                b: color.b,
                a: color.a,
            },
            None,
        )
    }
    .map_err(|e| IGuiError::D2D(format!("CreateSolidColorBrush failed: {e}")))
}

fn ellipse_from_rect(rect: &batch_mod::Rect) -> D2D1_ELLIPSE {
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

fn draw_arc_hwnd(
    target: &ID2D1HwndRenderTarget,
    center: batch_mod::Point,
    radius: f32,
    rotation_rad: f32,
    half_aperture_rad: f32,
    stroke_w: f32,
    brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
) -> Result<(), IGuiError> {
    let factory = &renderer::ctx().d2d.factory;
    let geometry = unsafe { factory.CreatePathGeometry() }
        .map_err(|e| IGuiError::D2D(format!("CreatePathGeometry: {e}")))?;
    let sink = unsafe { geometry.Open() }
        .map_err(|e| IGuiError::D2D(format!("PathGeometry::Open: {e}")))?;

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
        .map_err(|e| IGuiError::D2D(format!("GeometrySink::Close: {e}")))?;

    unsafe { target.DrawGeometry(&geometry, brush, stroke_w, None) };
    Ok(())
}

fn log_ui_batch(child_id: i64, batch: &batch_mod::PaneBatch) {
    eprintln!(
        "[igui-batch-ui] child={} seq={} flags={} cmds={}",
        child_id,
        batch.sequence,
        batch.flags,
        batch.cmds.len(),
    );
    for (index, cmd) in batch.cmds.iter().enumerate() {
        match cmd {
            SurfaceCmd::Clear { color } => eprintln!(
                "[igui-batch-ui]   #{index} Clear rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::PresentHint => {
                eprintln!("[igui-batch-ui]   #{index} PresentHint");
            }
            SurfaceCmd::Blit { x, y, w, h, .. } => eprintln!(
                "[igui-batch-ui]   #{index} Blit {w}x{h} @ ({x:.1}, {y:.1})"
            ),
            SurfaceCmd::FillRect {
                rect,
                corner_radius,
                color,
            } => eprintln!(
                "[igui-batch-ui]   #{index} FillRect rect=({:.1}, {:.1})-({:.1}, {:.1}) radius={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0,
                rect.y0,
                rect.x1,
                rect.y1,
                corner_radius,
                color.r,
                color.g,
                color.b,
                color.a
            ),
            SurfaceCmd::StrokeRect {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => eprintln!(
                "[igui-batch-ui]   #{index} StrokeRect rect=({:.1}, {:.1})-({:.1}, {:.1}) radius={:.1} half_thickness={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0,
                rect.y0,
                rect.x1,
                rect.y1,
                corner_radius,
                half_thickness,
                color.r,
                color.g,
                color.b,
                color.a
            ),
            SurfaceCmd::DrawLine {
                p0,
                p1,
                half_thickness,
                color,
            } => eprintln!(
                "[igui-batch-ui]   #{index} DrawLine p0=({:.1}, {:.1}) p1=({:.1}, {:.1}) half_thickness={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                p0.x, p0.y, p1.x, p1.y, half_thickness,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::FillOval { rect, color } => eprintln!(
                "[igui-batch-ui]   #{index} FillOval rect=({:.1}, {:.1})-({:.1}, {:.1}) rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0, rect.y0, rect.x1, rect.y1,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::FillCircle { center, radius, color } => eprintln!(
                "[igui-batch-ui]   #{index} FillCircle center=({:.1}, {:.1}) r={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                center.x, center.y, radius,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::StrokeOval { rect, half_thickness, color } => eprintln!(
                "[igui-batch-ui]   #{index} StrokeOval rect=({:.1}, {:.1})-({:.1}, {:.1}) ht={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0, rect.y0, rect.x1, rect.y1, half_thickness,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::StrokeCircle { center, radius, half_thickness, color } => eprintln!(
                "[igui-batch-ui]   #{index} StrokeCircle center=({:.1}, {:.1}) r={:.1} ht={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                center.x, center.y, radius, half_thickness,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::DrawArc { center, radius, rotation_rad, half_aperture_rad, half_thickness, color } => eprintln!(
                "[igui-batch-ui]   #{index} DrawArc center=({:.1}, {:.1}) r={:.1} rot={:.3} half_ap={:.3} ht={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                center.x, center.y, radius, rotation_rad, half_aperture_rad, half_thickness,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::DrawTextRun { run } => eprintln!(
                "[igui-batch-ui]   #{index} DrawTextRun \"{}\" origin=({:.1}, {:.1}) family=\"{}\" size={:.1} weight={} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                run.text, run.origin.x, run.origin.y, run.family, run.size, run.weight,
                run.color.r, run.color.g, run.color.b, run.color.a
            ),
            SurfaceCmd::MeasureTextRun { request_id, run } => eprintln!(
                "[igui-batch-ui]   #{index} MeasureTextRun req={request_id} text=\"{}\"",
                run.text
            ),
            SurfaceCmd::CharIndexAtPoint { request_id, run, point } => eprintln!(
                "[igui-batch-ui]   #{index} CharIndexAtPoint req={request_id} text=\"{}\" pt=({:.1}, {:.1})",
                run.text, point.x, point.y
            ),
            SurfaceCmd::PointAtCharIndex { request_id, run, char_index } => eprintln!(
                "[igui-batch-ui]   #{index} PointAtCharIndex req={request_id} text=\"{}\" char={}",
                run.text, char_index
            ),
            SurfaceCmd::PushClipRect { rect } => eprintln!(
                "[igui-batch-ui]   #{index} PushClipRect rect=({:.1}, {:.1})-({:.1}, {:.1})",
                rect.x0, rect.y0, rect.x1, rect.y1
            ),
            SurfaceCmd::PopClipRect => eprintln!("[igui-batch-ui]   #{index} PopClipRect"),
            SurfaceCmd::PushOffset { dx, dy } => eprintln!(
                "[igui-batch-ui]   #{index} PushOffset dx={dx:.1} dy={dy:.1}"
            ),
            SurfaceCmd::PopOffset => eprintln!("[igui-batch-ui]   #{index} PopOffset"),
            SurfaceCmd::ScrollRect { rect, dx, dy } => eprintln!(
                "[igui-batch-ui]   #{index} ScrollRect rect=({:.1}, {:.1})-({:.1}, {:.1}) dx={:.1} dy={:.1}",
                rect.x0, rect.y0, rect.x1, rect.y1, dx, dy
            ),
            SurfaceCmd::SaveRect { slot, rect } => eprintln!(
                "[igui-batch-ui]   #{index} SaveRect slot={slot} rect=({:.1}, {:.1})-({:.1}, {:.1})",
                rect.x0, rect.y0, rect.x1, rect.y1
            ),
            SurfaceCmd::RestoreRect { slot } => eprintln!(
                "[igui-batch-ui]   #{index} RestoreRect slot={slot}"
            ),
            SurfaceCmd::InstallChildViewBounds { child_view_id, rect } => eprintln!(
                "[igui-batch-ui]   #{index} InstallChildViewBounds id={child_view_id} rect=({:.1}, {:.1})-({:.1}, {:.1})",
                rect.x0, rect.y0, rect.x1, rect.y1
            ),
            SurfaceCmd::MarkRect { rect, mode } => eprintln!(
                "[igui-batch-ui]   #{index} MarkRect rect=({:.1}, {:.1})-({:.1}, {:.1}) mode={:?}",
                rect.x0, rect.y0, rect.x1, rect.y1, mode
            ),
            SurfaceCmd::Caret { rect, color } => eprintln!(
                "[igui-batch-ui]   #{index} Caret rect=({:.1}, {:.1})-({:.1}, {:.1}) rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0, rect.y0, rect.x1, rect.y1, color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::SelectionRange { rect, color } => eprintln!(
                "[igui-batch-ui]   #{index} SelectionRange rect=({:.1}, {:.1})-({:.1}, {:.1}) rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0, rect.y0, rect.x1, rect.y1, color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::FocusRing { rect, corner_radius, half_thickness, color } => eprintln!(
                "[igui-batch-ui]   #{index} FocusRing rect=({:.1}, {:.1})-({:.1}, {:.1}) cr={:.1} ht={:.1} rgba=({:.3}, {:.3}, {:.3}, {:.3})",
                rect.x0, rect.y0, rect.x1, rect.y1, corner_radius, half_thickness,
                color.r, color.g, color.b, color.a
            ),
            SurfaceCmd::DrawPath { commands, fill, stroke } => eprintln!(
                "[igui-batch-ui]   #{index} DrawPath commands={} fill={} stroke={}",
                commands.len(),
                fill.is_some(),
                stroke.is_some()
            ),
        }
    }
}

fn win32_failure(context: &str) -> IGuiError {
    IGuiError::Win32(format!("{context}: {}", Error::from_thread()))
}

fn log_cleanup_failure(context: &str) {
    eprintln!("[igui-win32] {context}: {}", Error::from_thread());
}

fn delete_gdi_object(obj: impl Into<windows::Win32::Graphics::Gdi::HGDIOBJ>, context: &str) {
    if !unsafe { DeleteObject(obj.into()) }.as_bool() {
        log_cleanup_failure(context);
    }
}

fn rgba_channel_to_u8(channel: f32) -> u8 {
    (channel.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn rgba_to_colorref(r: f32, g: f32, b: f32) -> COLORREF {
    let red = rgba_channel_to_u8(r) as u32;
    let green = rgba_channel_to_u8(g) as u32;
    let blue = rgba_channel_to_u8(b) as u32;
    COLORREF(red | (green << 8) | (blue << 16))
}

fn rect_to_win32(rect: &batch_mod::Rect) -> RECT {
    RECT {
        left: rect.x0.floor() as i32,
        top: rect.y0.floor() as i32,
        right: rect.x1.ceil() as i32,
        bottom: rect.y1.ceil() as i32,
    }
}

fn fill_rect_color(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    color: COLORREF,
) -> Result<(), IGuiError> {
    let brush = unsafe { CreateSolidBrush(color) };
    if brush.0.is_null() {
        return Err(win32_failure("CreateSolidBrush failed"));
    }
    if unsafe { FillRect(hdc, rect, brush) } == 0 {
        delete_gdi_object(brush, "DeleteObject after FillRect failure");
        return Err(win32_failure("FillRect failed"));
    }
    delete_gdi_object(brush, "DeleteObject after FillRect");
    Ok(())
}

fn stroke_rect_color(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rect: &RECT,
    color: COLORREF,
    half_thickness: f32,
    corner_radius: f32,
) -> Result<(), IGuiError> {
    let brush = unsafe { CreateSolidBrush(color) };
    if brush.0.is_null() {
        return Err(win32_failure("CreateSolidBrush failed"));
    }
    let thickness = (2.0 * half_thickness).round().max(1.0) as i32;
    if corner_radius <= 0.0 {
        let mut current = *rect;
        for _ in 0..thickness {
            if unsafe { FrameRect(hdc, &current, brush) } == 0 {
                delete_gdi_object(brush, "DeleteObject after FrameRect failure");
                return Err(win32_failure("FrameRect failed"));
            }
            current.left += 1;
            current.top += 1;
            current.right -= 1;
            current.bottom -= 1;
            if current.right <= current.left || current.bottom <= current.top {
                break;
            }
        }
    } else {
        let pen = unsafe { CreatePen(PS_SOLID, thickness, color) };
        if pen.0.is_null() {
            delete_gdi_object(brush, "DeleteObject after CreatePen failure");
            return Err(win32_failure("CreatePen failed"));
        }
        let old_pen = unsafe { SelectObject(hdc, pen.into()) };
        if old_pen.0.is_null() {
            delete_gdi_object(pen, "DeleteObject after SelectObject(pen) failure");
            delete_gdi_object(brush, "DeleteObject after SelectObject(pen) failure");
            return Err(win32_failure("SelectObject(pen) failed"));
        }
        let old_brush = unsafe { SelectObject(hdc, brush.into()) };
        if old_brush.0.is_null() {
            let _ = unsafe { SelectObject(hdc, old_pen) };
            delete_gdi_object(pen, "DeleteObject after SelectObject(brush) failure");
            delete_gdi_object(brush, "DeleteObject after SelectObject(brush) failure");
            return Err(win32_failure("SelectObject(brush) failed"));
        }
        let radius = corner_radius.round().max(1.0) as i32;
        if !unsafe { RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius, radius) }
            .as_bool()
        {
            let _ = unsafe { SelectObject(hdc, old_pen) };
            let _ = unsafe { SelectObject(hdc, old_brush) };
            delete_gdi_object(pen, "DeleteObject after RoundRect failure");
            delete_gdi_object(brush, "DeleteObject after RoundRect failure");
            return Err(win32_failure("RoundRect failed"));
        }
        if unsafe { SelectObject(hdc, old_pen) }.0.is_null() {
            log_cleanup_failure("SelectObject restore pen failed");
        }
        if unsafe { SelectObject(hdc, old_brush) }.0.is_null() {
            log_cleanup_failure("SelectObject restore brush failed");
        }
        delete_gdi_object(pen, "DeleteObject after RoundRect");
        delete_gdi_object(brush, "DeleteObject after RoundRect");
        return Ok(());
    }
    delete_gdi_object(brush, "DeleteObject after FrameRect");
    Ok(())
}

fn draw_line_color(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    p0: &batch_mod::Point,
    p1: &batch_mod::Point,
    color: COLORREF,
    half_thickness: f32,
) -> Result<(), IGuiError> {
    let thickness = (2.0 * half_thickness).round().max(1.0) as i32;
    let pen = unsafe { CreatePen(PS_SOLID, thickness, color) };
    if pen.0.is_null() {
        return Err(win32_failure("CreatePen failed"));
    }
    let old_pen = unsafe { SelectObject(hdc, pen.into()) };
    if old_pen.0.is_null() {
        delete_gdi_object(pen, "DeleteObject after SelectObject(line pen) failure");
        return Err(win32_failure("SelectObject(line pen) failed"));
    }
    if !unsafe { MoveToEx(hdc, p0.x.round() as i32, p0.y.round() as i32, None) }.as_bool() {
        let _ = unsafe { SelectObject(hdc, old_pen) };
        delete_gdi_object(pen, "DeleteObject after MoveToEx failure");
        return Err(win32_failure("MoveToEx failed"));
    }
    if !unsafe { LineTo(hdc, p1.x.round() as i32, p1.y.round() as i32) }.as_bool() {
        let _ = unsafe { SelectObject(hdc, old_pen) };
        delete_gdi_object(pen, "DeleteObject after LineTo failure");
        return Err(win32_failure("LineTo failed"));
    }
    if unsafe { SelectObject(hdc, old_pen) }.0.is_null() {
        log_cleanup_failure("SelectObject restore line pen failed");
    }
    delete_gdi_object(pen, "DeleteObject after LineTo");
    Ok(())
}

fn hwnd_client_size(hwnd: HWND) -> Option<(i32, i32)> {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    Some((rect.right - rect.left, rect.bottom - rect.top))
}

fn log_hwnd_monitor(phase: &str, child_id: i64, mdi_hwnd: HWND, render_hwnd: HWND) {
    let mdi_valid = unsafe { IsWindow(Some(mdi_hwnd)) }.as_bool();
    let render_valid = unsafe { IsWindow(Some(render_hwnd)) }.as_bool();
    let mdi_visible = mdi_valid && unsafe { IsWindowVisible(mdi_hwnd) }.as_bool();
    let render_visible = render_valid && unsafe { IsWindowVisible(render_hwnd) }.as_bool();
    let render_parent = if render_valid {
        unsafe { GetParent(render_hwnd) }.unwrap_or_default()
    } else {
        HWND::default()
    };
    let mdi_registry_match = registry::mdi_hwnd_of(child_id)
        .map(|h| h.0 == mdi_hwnd.0)
        .unwrap_or(false);
    let render_registry_match = registry::render_hwnd_of(child_id)
        .map(|h| h.0 == render_hwnd.0)
        .unwrap_or(false);
    let (mdi_w, mdi_h) = hwnd_client_size(mdi_hwnd).unwrap_or((-1, -1));
    let (render_w, render_h) = hwnd_client_size(render_hwnd).unwrap_or((-1, -1));

    eprintln!(
        "[igui-hwnd] {phase} child={child_id} mdi={:?} valid={} visible={} size={}x{} registry_match={} render={:?} valid={} visible={} size={}x{} registry_match={} parent={:?} parent_matches={}",
        mdi_hwnd,
        mdi_valid,
        mdi_visible,
        mdi_w,
        mdi_h,
        mdi_registry_match,
        render_hwnd,
        render_valid,
        render_visible,
        render_w,
        render_h,
        render_registry_match,
        render_parent,
        render_parent.0 == mdi_hwnd.0,
    );
}

fn execute_gdi_batch(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    client_rect: &RECT,
    batch: &batch_mod::PaneBatch,
) -> Result<(), IGuiError> {
    for cmd in &batch.cmds {
        match cmd {
            SurfaceCmd::Clear { color } => {
                fill_rect_color(
                    hdc,
                    client_rect,
                    rgba_to_colorref(color.r, color.g, color.b),
                )?;
            }
            SurfaceCmd::PresentHint => {}
            SurfaceCmd::FillRect {
                rect,
                corner_radius,
                color,
            } => {
                let rect = rect_to_win32(rect);
                if *corner_radius <= 0.0 {
                    fill_rect_color(hdc, &rect, rgba_to_colorref(color.r, color.g, color.b))?;
                } else {
                    let brush = unsafe { CreateSolidBrush(rgba_to_colorref(color.r, color.g, color.b)) };
                    if brush.0.is_null() {
                        return Err(IGuiError::Win32("CreateSolidBrush failed".into()));
                    }
                    let old_brush = unsafe { SelectObject(hdc, brush.into()) };
                    let radius = corner_radius.round().max(1.0) as i32;
                    let _ = unsafe { RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, radius, radius) };
                    let _ = unsafe { SelectObject(hdc, old_brush) };
                    let _ = unsafe { DeleteObject(brush.into()) };
                }
            }
            SurfaceCmd::StrokeRect {
                rect,
                corner_radius,
                half_thickness,
                color,
            } => {
                let rect = rect_to_win32(rect);
                stroke_rect_color(
                    hdc,
                    &rect,
                    rgba_to_colorref(color.r, color.g, color.b),
                    *half_thickness,
                    *corner_radius,
                )?;
            }
            SurfaceCmd::DrawLine {
                p0,
                p1,
                half_thickness,
                color,
            } => {
                draw_line_color(
                    hdc,
                    p0,
                    p1,
                    rgba_to_colorref(color.r, color.g, color.b),
                    *half_thickness,
                )?;
            }
            // The GDI path is a diagnostic fallback only. Phase 3c
            // ellipse / circle / arc primitives are not implemented
            // here; the user only sees this if the D2D HwndRenderTarget
            // path failed and we logged the error already.
            SurfaceCmd::FillOval { .. }
            | SurfaceCmd::FillCircle { .. }
            | SurfaceCmd::StrokeOval { .. }
            | SurfaceCmd::StrokeCircle { .. }
            | SurfaceCmd::DrawArc { .. } => {
                eprintln!(
                    "[igui-gdi] Phase 3c primitive in GDI fallback — skipped (D2D path is the real one)"
                );
            }
            // Text primitives need DirectWrite; no GDI fallback. The
            // sync queries still need to satisfy any blocked CP caller
            // so we deliver a Failed reply instead of letting them
            // time out.
            SurfaceCmd::DrawTextRun { .. } => {
                eprintln!("[igui-gdi] DrawTextRun in GDI fallback — skipped");
            }
            SurfaceCmd::MeasureTextRun { request_id, .. }
            | SurfaceCmd::CharIndexAtPoint { request_id, .. }
            | SurfaceCmd::PointAtCharIndex { request_id, .. } => {
                super::replies::deliver(
                    *request_id,
                    super::replies::Reply::Failed {
                        message: "text query unsupported on GDI fallback".into(),
                    },
                );
            }
            // Canvas Blit needs the D2D bitmap path; the GDI fallback is
            // transient (used only until the D2D target is created), so
            // we skip it rather than carry a parallel StretchDIBits path.
            SurfaceCmd::Blit { .. } => {
                eprintln!("[igui-gdi] canvas Blit on GDI fallback — skipped (D2D is the canvas path)");
            }
            // Phase 5 commands all skipped in the GDI fallback path —
            // Direct2D is the real path.
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
                eprintln!("[igui-gdi] Phase 5 primitive in GDI fallback — skipped");
            }
        }
    }
    Ok(())
}

/// Deterministic per-child background. Picks a slate-with-tint based
/// on `child_id` so two simultaneously-open children are visually
/// distinct without any CP-side batches yet.
fn phase3a_palette(child_id: i64) -> [f32; 3] {
    let palette: [[f32; 3]; 6] = [
        [0.18, 0.20, 0.23],
        [0.22, 0.18, 0.20],
        [0.18, 0.23, 0.20],
        [0.20, 0.18, 0.23],
        [0.23, 0.22, 0.18],
        [0.18, 0.22, 0.23],
    ];
    palette[((child_id as usize).saturating_sub(2)) % palette.len()]
}

// ─── Class registration ──────────────────────────────────────────────

pub fn register_classes() -> Result<(), IGuiError> {
    let h_instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| IGuiError::Win32(format!("GetModuleHandleW (child): {e}")))?
        .into();
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
        .map_err(|e| IGuiError::Win32(format!("LoadCursorW (child): {e}")))?;

    let mdi = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(mdi_child_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: MDI_CHILD_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&mdi) };

    let render = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(render_host_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: h_instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: RENDER_HOST_CLASS,
        hIconSm: Default::default(),
    };
    let _ = unsafe { RegisterClassExW(&render) };

    super::ledit::register_class()?;
    super::log_view::register_class()?;
    super::text_view::register_class()?;
    super::repl_child::register_class()?;

    Ok(())
}

// ─── MDI child WndProc ───────────────────────────────────────────────

/// `GWLP_USERDATA` on the MDI child stores its `child_id` as a raw
/// `isize`. The render-host HWND is looked up via the registry.
unsafe extern "system" fn mdi_child_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // Recover the bootstrap from MDICREATESTRUCT.lParam.
        let create = lparam.0 as *const CREATESTRUCTW;
        let mdi_create =
            unsafe { (*create).lpCreateParams as *const MDICREATESTRUCTW };
        if mdi_create.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap_ptr =
            unsafe { (*mdi_create).lParam.0 as *mut MdiBootstrap };
        if bootstrap_ptr.is_null() {
            return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap = unsafe { Box::from_raw(bootstrap_ptr) };
        let child_id = bootstrap.child_id;

        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, child_id as isize) };

        let render_bootstrap = Box::into_raw(Box::new(RenderBootstrap { child_id }));
        let h_instance = unsafe { GetModuleHandleW(None) }
            .ok()
            .map(|h| windows::Win32::Foundation::HINSTANCE(h.0));
        let render_hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                RENDER_HOST_CLASS,
                PCWSTR::null(),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
                0,
                0,
                0,
                0,
                Some(hwnd),
                None,
                h_instance,
                Some(render_bootstrap as *mut _),
            )
        };
        let render_hwnd = match render_hwnd {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[igui-mdi] render-host creation failed: {e}");
                let _ = unsafe { Box::from_raw(render_bootstrap) };
                return LRESULT(0);
            }
        };

        registry::register(child_id, hwnd, render_hwnd);
        log_hwnd_monitor("post-create", child_id, hwnd, render_hwnd);

        return unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) };
    }

    let child_id = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as i64;
    let render_hwnd = if child_id != 0 {
        registry::render_hwnd_of(child_id)
    } else {
        None
    };

    match msg {
        WM_SIZE => {
            if let Some(rh) = render_hwnd {
                let w = (lparam.0 & 0xFFFF) as i32;
                let h = ((lparam.0 >> 16) & 0xFFFF) as i32;
                if let Err(err) = unsafe {
                    SetWindowPos(rh, None, 0, 0, w, h, SWP_NOZORDER | SWP_NOACTIVATE)
                } {
                    eprintln!("[igui-win32] SetWindowPos failed for render host {:?}: {err}", rh);
                }
            }
            channels::push(IGuiEvent::Resize {
                child_id,
                width: (lparam.0 & 0xFFFF) as i64,
                height: ((lparam.0 >> 16) & 0xFFFF) as i64,
            });
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CLOSE => {
            // Destroy the MDI child directly instead of delegating to
            // DefMDIChildProcW.  When this is the *last* MDI child,
            // DefMDIChildProcW's WM_CLOSE handler cascades a WM_CLOSE to
            // the parent frame — which closes the entire NCL window.
            // Calling DestroyWindow ourselves skips that cascade while
            // still triggering WM_NCDESTROY so our cleanup runs normally.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if child_id != 0 {
                channels::push(IGuiEvent::Close { child_id });
                batch_mod::forget(child_id);
                registry::unregister(child_id);
            }
            unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefMDIChildProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Render-host WndProc ────────────────────────────────────────────

unsafe extern "system" fn render_host_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if create.is_null() {
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap_ptr =
            unsafe { (*create).lpCreateParams as *mut RenderBootstrap };
        if bootstrap_ptr.is_null() {
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        let bootstrap = unsafe { Box::from_raw(bootstrap_ptr) };

        let state = Box::new(ChildState {
            child_id: bootstrap.child_id,
            hwnd,
            target: None,
            logged_hwnd_status: false,
            last_logged_sequence: None,
        });
        let child_id = state.child_id;
        let raw = Box::into_raw(state);
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize) };
        // Sample the DPI now and emit the initial dpi-change event so
        // the language thread starts with a known DPI for this child.
        super::cursor::refresh_for(child_id, hwnd);

        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ChildState;

    let ensure_ui_thread = |phase: &str| {
        let current = unsafe { GetCurrentThreadId() };
        if let Some(expected) = window::gui_thread_id() {
            if current != expected {
                eprintln!(
                    "[igui-render] {phase} on wrong thread: current={current} expected-ui={expected} hwnd={:?}",
                    hwnd
                );
            }
        } else {
            eprintln!(
                "[igui-render] {phase} without recorded UI thread id: current={current} hwnd={:?}",
                hwnd
            );
        }
    };

    match msg {
        // Suppress GDI background erase. Our render host paints
        // entirely through D2D + DXGI.
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            ensure_ui_thread("WM_PAINT");
            let mut ps = PAINTSTRUCT::default();
            let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
            if hdc.0.is_null() {
                eprintln!("[igui-win32] BeginPaint failed for {:?}: {}", hwnd, Error::from_thread());
                return LRESULT(0);
            }
            eprintln!(
                "[igui-paint] hwnd={:?} rcPaint=({}, {})-({}, {}) fErase={}",
                hwnd,
                ps.rcPaint.left,
                ps.rcPaint.top,
                ps.rcPaint.right,
                ps.rcPaint.bottom,
                ps.fErase.as_bool(),
            );
            if !raw.is_null() {
                let state = unsafe { &mut *raw };
                if let Err(err) = state.render(hdc) {
                    eprintln!("[igui-gdi] render error: {err}");
                }
            }
            if !unsafe { EndPaint(hwnd, &ps) }.as_bool() {
                eprintln!("[igui-win32] EndPaint failed for {:?}: {}", hwnd, Error::from_thread());
            }
            LRESULT(0)
        }
        WM_SIZE => {
            ensure_ui_thread("WM_SIZE");
            if !raw.is_null() {
                let w = (lparam.0 & 0xFFFF) as u32;
                let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
                let state = unsafe { &mut *raw };
                if let Err(err) = state.handle_resize(w, h) {
                    eprintln!("[igui-render] resize error: {err}");
                }
                // TODO: pushing IGuiEvent::Resize from here so the
                // language thread can re-layout against the actual
                // child size triggered a crash during initial paint
                // (probably reentrancy with the in-flight measure-
                // text reply path). For now Lisp uses a polled
                // (child-size id) primitive instead.
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // Apply the per-child cursor only when the cursor is over
            // the client area; otherwise let DefWindowProc handle the
            // non-client cases (resize edges, etc.).
            let hit = (lparam.0 & 0xFFFF) as i16;
            const HTCLIENT: i16 = 1;
            if hit == HTCLIENT && !raw.is_null() {
                let state = unsafe { &*raw };
                super::cursor::apply(state.child_id);
                LRESULT(1)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        x if x == super::window::WM_IGUI_SET_TIMER => {
            // Install or clear the per-child redraw timer.
            let interval = wparam.0;
            if interval == 0 {
                let _ = unsafe { KillTimer(Some(hwnd), super::window::TICK_TIMER_ID) };
            } else {
                let _ = unsafe {
                    SetTimer(
                        Some(hwnd),
                        super::window::TICK_TIMER_ID,
                        interval as u32,
                        None,
                    )
                };
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == super::window::TICK_TIMER_ID => {
            if !raw.is_null() {
                let state = unsafe { &*raw };
                let now = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetMessageTime()
                };
                channels::push(IGuiEvent::Tick {
                    child_id: state.child_id,
                    time_ms: now as i64,
                });
            }
            LRESULT(0)
        }
        WM_DPICHANGED_AFTERPARENT => {
            // The parent (MDI child) just changed DPI; refresh ours.
            if !raw.is_null() {
                let state = unsafe { &*raw };
                super::cursor::refresh_for(state.child_id, hwnd);
            }
            LRESULT(0)
        }
        // ─── Input ─────────────────────────────────────────────────
        WM_MOUSEMOVE => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::MOVE, 0, lparam);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                // Click → focus, so subsequent keystrokes route here.
                let _ = unsafe { SetFocus(Some(hwnd)) };
                window::push_mouse(cid, super::channels::mouse_op::LEFT_DOWN, 1, lparam);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::LEFT_UP, 1, lparam);
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::RIGHT_DOWN, 2, lparam);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::RIGHT_UP, 2, lparam);
            }
            LRESULT(0)
        }
        WM_MBUTTONDOWN => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::MIDDLE_DOWN, 3, lparam);
            }
            LRESULT(0)
        }
        WM_MBUTTONUP => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_mouse(cid, super::channels::mouse_op::MIDDLE_UP, 3, lparam);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i64;
                let lines = delta / WHEEL_DELTA as i64;
                let x = (lparam.0 & 0xFFFF) as i16 as i64;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i64;
                channels::push(IGuiEvent::Mouse {
                    child_id: cid,
                    x,
                    y,
                    op: super::channels::mouse_op::WHEEL,
                    button: 0,
                    mods: window::current_modifiers(),
                    wheel_delta: delta,
                    wheel_lines: lines,
                    time_ms: window::msg_time(),
                });
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_key(cid, true, wparam, lparam);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_KEYUP | WM_SYSKEYUP => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                window::push_key(cid, false, wparam, lparam);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CHAR => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                channels::push(IGuiEvent::Char {
                    child_id: cid,
                    codepoint: wparam.0 as i64,
                    mods: window::current_modifiers(),
                    time_ms: window::msg_time(),
                });
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                channels::push(IGuiEvent::Focus { child_id: cid, gained: true });
            }
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            if !raw.is_null() {
                let cid = unsafe { (*raw).child_id };
                channels::push(IGuiEvent::Focus { child_id: cid, gained: false });
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if !raw.is_null() {
                let state = unsafe { &*raw };
                super::cursor::forget_cursor(state.child_id);
                super::cursor::forget_dpi(state.child_id);
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                let _ = unsafe { Box::from_raw(raw) };
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─── Bootstraps threaded through CreateWindow lpCreateParams ─────────

/// Threaded through `MDICREATESTRUCTW.lParam` for the MDI child window.
pub(crate) struct MdiBootstrap {
    pub(crate) child_id: i64,
}

/// Threaded through `CREATESTRUCTW.lpCreateParams` for the render host.
struct RenderBootstrap {
    child_id: i64,
}

// ─── Helpers used from window.rs ─────────────────────────────────────

/// Send `WM_SETTEXT` to the MDI child to update its title bar.
pub fn set_title(mdi_hwnd: HWND, title_w: &[u16]) {
    unsafe {
        SendMessageW(
            mdi_hwnd,
            WM_SETTEXT,
            Some(WPARAM(0)),
            Some(LPARAM(title_w.as_ptr() as isize)),
        )
    };
}

/// Ask the MDI client to destroy a child via `WM_MDIDESTROY`.
pub fn close_via_mdi(mdi_client: HWND, mdi_child: HWND) {
    unsafe {
        SendMessageW(
            mdi_client,
            WM_MDIDESTROY,
            Some(WPARAM(mdi_child.0 as usize)),
            Some(LPARAM(0)),
        )
    };
}

/// UTF-16 → owned String. Debug only.
#[allow(dead_code)]
pub(crate) fn decode_utf16(buf: &[u16]) -> String {
    let trimmed: Vec<u16> = buf.iter().copied().take_while(|c| *c != 0).collect();
    OsString::from_wide(&trimmed)
        .into_string()
        .unwrap_or_default()
}
