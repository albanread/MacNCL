//! Journey diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::journey::{JourneyDb, JourneyTask};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const PAD_X: f32 = 34.0;
const PAD_BOTTOM: f32 = 34.0;
const AXIS_W: f32 = 34.0;
const TITLE_H: f32 = 34.0;
const SECTION_H: f32 = 38.0;
const SECTION_GAP: f32 = 14.0;
const CHART_H: f32 = 128.0;
const CARD_GAP: f32 = 32.0;
const LANE_GAP: f32 = 44.0;
const COL_W: f32 = 160.0;
const CARD_W: f32 = 138.0;
const MAX_COLS: usize = 3;
const TITLE_FONT: f32 = 15.0;
const SECTION_FONT: f32 = 11.5;
const TASK_FONT: f32 = 10.6;
const ACTOR_FONT: f32 = 9.8;
const SCORE_FONT: f32 = 10.5;

const SECTION_COLORS: [(u32, u32, u32); 8] = [
    (0x202020, 0x4FC1FF, 0xFFFFFF),
    (0x202020, 0x6A9955, 0xFFFFFF),
    (0x202020, 0xDCDCAA, 0xFFFFFF),
    (0x202020, 0xC586C0, 0xFFFFFF),
    (0x202020, 0xCE9178, 0xFFFFFF),
    (0x202020, 0x4EC9B0, 0xFFFFFF),
    (0x202020, 0x9CDCFE, 0xFFFFFF),
    (0x202020, 0xD7BA7D, 0xFFFFFF),
];

pub fn build(db: &JourneyDb) -> JourneyGraph {
    let tasks = db.get_tasks();
    let has_sections = !db.get_sections().is_empty();
    let title = db.title.clone();
    let title_h = if title.is_empty() { 0.0 } else { TITLE_H };
    let chart_x = PAD_X + AXIS_W;
    let first_lane_top = title_h + 14.0;

    if tasks.is_empty() {
        let chart_y = first_lane_top + if has_sections { SECTION_H + 24.0 } else { 0.0 };
        return JourneyGraph {
            width: 520.0,
            height: chart_y + CHART_H + PAD_BOTTOM,
            title,
            lanes: vec![JourneyLane {
                chart_x,
                chart_y,
                chart_w: 380.0,
                chart_h: CHART_H,
                task_start: 0,
                task_count: 0,
            }],
            sections: Vec::new(),
            tasks: Vec::new(),
        };
    }

    let mut section_index = HashMap::new();
    for (idx, section) in db.get_sections().iter().enumerate() {
        section_index.insert(section.as_str(), idx);
    }

    let max_cols = tasks.len().min(MAX_COLS);
    let chart_w = max_cols as f32 * COL_W;
    let mut lanes = Vec::new();
    let mut sections = Vec::new();
    let mut out_tasks = Vec::new();
    let mut max_bottom = 0.0_f32;

    let mut lane_top = first_lane_top;
    let lane_count = tasks.len().div_ceil(MAX_COLS);
    for lane_idx in 0..lane_count {
        let start = lane_idx * MAX_COLS;
        let end = (start + MAX_COLS).min(tasks.len());
        let cols = end - start;
        let section_y = lane_top;
        let chart_y = lane_top + if has_sections { SECTION_H + 24.0 } else { 0.0 };
        let card_y = chart_y + CHART_H + CARD_GAP;
        let lane_chart_w = cols as f32 * COL_W;

        lanes.push(JourneyLane {
            chart_x,
            chart_y,
            chart_w: lane_chart_w,
            chart_h: CHART_H,
            task_start: start,
            task_count: cols,
        });

        if has_sections {
            sections.extend(build_sections(
                db.get_sections(),
                tasks,
                start,
                end,
                section_y,
                chart_x,
                &section_index,
            ));
        }

        let mut lane_bottom = card_y;
        for (col, task) in tasks[start..end].iter().enumerate() {
            let point_x = chart_x + col as f32 * COL_W + COL_W / 2.0;
            let point_y = score_y(task.score, chart_y, CHART_H);
            let x = point_x - CARD_W / 2.0;
            let h = card_height(task);
            let (fill, stroke, text_color) = score_colors(task.score);
            lane_bottom = lane_bottom.max(card_y + h);
            out_tasks.push(JourneyTaskBox {
                x,
                y: card_y,
                w: CARD_W,
                h,
                point_x,
                point_y,
                score: task.score,
                label: task.task.clone(),
                actors: task.people.join(", "),
                fill,
                stroke,
                text_color,
            });
        }

        max_bottom = max_bottom.max(lane_bottom);
        lane_top = lane_bottom + LANE_GAP;
    }

    JourneyGraph {
        width: chart_x + chart_w + PAD_X,
        height: max_bottom + PAD_BOTTOM,
        title,
        lanes,
        sections,
        tasks: out_tasks,
    }
}

pub fn build_with_overrides(db: &JourneyDb, overrides: &ManualLayoutOverrides) -> JourneyGraph {
    let mut graph = build(db);
    apply_manual_overrides(&mut graph, overrides);
    graph
}

fn apply_manual_overrides(graph: &mut JourneyGraph, overrides: &ManualLayoutOverrides) {
    for section in &mut graph.sections {
        if let Some(ov) = overrides.group(&section.label) {
            apply_box_override(
                &mut section.x,
                &mut section.y,
                &mut section.w,
                &mut section.h,
                ov,
            );
        }
    }

    for task in &mut graph.tasks {
        if let Some(ov) = overrides.object(&task.label) {
            apply_box_override(&mut task.x, &mut task.y, &mut task.w, &mut task.h, ov);
            task.point_x = task.x + task.w / 2.0;
        }
    }

    grow_bounds(graph);
    if let Some(w) = overrides.graph.w {
        graph.width = w;
    }
    if let Some(h) = overrides.graph.h {
        graph.height = h;
    }
}

fn apply_box_override(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, ov: &BoxOverride) {
    if let Some(v) = ov.x {
        *x = v;
    }
    if let Some(v) = ov.y {
        *y = v;
    }
    if let Some(v) = ov.w {
        *w = v;
    }
    if let Some(v) = ov.h {
        *h = v;
    }
}

fn grow_bounds(graph: &mut JourneyGraph) {
    let mut width = graph.width.max(1.0);
    let mut height = graph.height.max(1.0);
    for lane in &graph.lanes {
        width = width.max(lane.chart_x + lane.chart_w + PAD_X);
        height = height.max(lane.chart_y + lane.chart_h + PAD_BOTTOM);
    }
    for section in &graph.sections {
        width = width.max(section.x + section.w + PAD_X);
        height = height.max(section.y + section.h + PAD_BOTTOM);
    }
    for task in &graph.tasks {
        width = width.max(task.x + task.w + PAD_X);
        height = height.max(task.y + task.h + PAD_BOTTOM);
    }
    graph.width = width;
    graph.height = height;
}

fn build_sections(
    sections: &[String],
    tasks: &[JourneyTask],
    start: usize,
    end: usize,
    section_y: f32,
    chart_x: f32,
    section_index: &HashMap<&str, usize>,
) -> Vec<JourneySectionBox> {
    let mut out = Vec::new();
    for (idx, section) in sections.iter().enumerate() {
        let positions: Vec<usize> = tasks
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
            .filter_map(|(task_idx, task)| (task.section == *section).then_some(task_idx))
            .collect();
        if positions.is_empty() {
            continue;
        }

        let first = *positions.first().unwrap();
        let last = *positions.last().unwrap();
        let x = chart_x + (first - start) as f32 * COL_W + SECTION_GAP / 2.0;
        let w = (last - first + 1) as f32 * COL_W - SECTION_GAP;
        let color_idx = section_index.get(section.as_str()).copied().unwrap_or(idx);
        let (fill, stroke, text_color) = SECTION_COLORS[color_idx % SECTION_COLORS.len()];
        out.push(JourneySectionBox {
            x,
            y: section_y,
            w,
            h: SECTION_H,
            label: section.clone(),
            fill,
            stroke,
            text_color,
        });
    }
    out
}

fn score_y(score: i32, chart_y: f32, chart_h: f32) -> f32 {
    let s = score.clamp(1, 5) as f32;
    chart_y + (5.0 - s) / 4.0 * chart_h
}

fn score_colors(score: i32) -> (u32, u32, u32) {
    match score.clamp(1, 5) {
        1 => (0x4A2525, 0xF87171, 0xFFFFFF),
        2 => (0x5A3327, 0xCE9178, 0xFFFFFF),
        3 => (0x5C511F, 0xDCDCAA, 0xFFFFFF),
        4 => (0x385723, 0x6A9955, 0xFFFFFF),
        _ => (0x246A61, 0x4EC9B0, 0xFFFFFF),
    }
}

fn card_height(task: &JourneyTask) -> f32 {
    let actor_text = task.people.join(", ");
    let task_lines = wrap_count(&task.task, 20);
    let actor_lines = if actor_text.is_empty() {
        0
    } else {
        wrap_count(&actor_text, 22)
    };
    (68.0 + task_lines as f32 * 14.0 + actor_lines as f32 * 13.0).clamp(98.0, 136.0)
}

fn wrap_count(text: &str, chars_per_line: usize) -> usize {
    text.split_whitespace()
        .fold((1usize, 0usize), |(lines, cur), word| {
            let next = if cur == 0 {
                word.len()
            } else {
                cur + 1 + word.len()
            };
            if next > chars_per_line && cur > 0 {
                (lines + 1, word.len())
            } else {
                (lines, next)
            }
        })
        .0
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &JourneyGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale;

    if !graph.title.is_empty() {
        let rect = D2D_RECT_F {
            left: tx(0.0),
            top: ty(0.0),
            right: tx(graph.width),
            bottom: ty(TITLE_H),
        };
        draw_text(
            target,
            &graph.title,
            rect,
            title_font(scale),
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_BRIGHT,
            false,
            &mut brush,
            &mut fmt,
        )?;
    }

    for section in &graph.sections {
        draw_section(target, section, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }

    for lane in &graph.lanes {
        draw_chart(target, graph, lane, &tx, &ty, &ts, &mut brush, &mut fmt)?;
        draw_connections(target, graph, lane, scale, &tx, &ty, &mut brush)?;
    }

    for task in &graph.tasks {
        draw_task(
            target, factory, task, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_section(
    target: &ID2D1RenderTarget,
    section: &JourneySectionBox,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(section.x),
        top: ty(section.y),
        right: tx(section.x) + ts(section.w),
        bottom: ty(section.y) + ts(section.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: ts(5.0).max(3.0),
        radiusY: ts(5.0).max(3.0),
    };
    let fill = brush(section.fill)?;
    let stroke = brush(section.stroke)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.1 * ts(1.0)).max(0.7),
        None::<&ID2D1StrokeStyle>,
    );
    draw_text(
        target,
        &section.label,
        rect,
        section_font(scale_from_rect(rect, section.h)),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        section.text_color,
        false,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_chart(
    target: &ID2D1RenderTarget,
    graph: &JourneyGraph,
    lane: &JourneyLane,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(lane.chart_x),
        top: ty(lane.chart_y),
        right: tx(lane.chart_x + lane.chart_w),
        bottom: ty(lane.chart_y + lane.chart_h),
    };
    let bg = brush(0x202020)?;
    let border = brush(theme::BORDER)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &border,
        (1.0 * ts(1.0)).max(0.7),
        None::<&ID2D1StrokeStyle>,
    );

    let grid = brush(0x343434)?;
    for score in 1..=5 {
        let y = ty(score_y(score, lane.chart_y, lane.chart_h));
        target.DrawLine(
            Vector2 {
                X: tx(lane.chart_x),
                Y: y,
            },
            Vector2 {
                X: tx(lane.chart_x + lane.chart_w),
                Y: y,
            },
            &grid,
            (1.0 * ts(1.0)).max(0.6),
            None::<&ID2D1StrokeStyle>,
        );

        let label_rect = D2D_RECT_F {
            left: tx(PAD_X),
            top: y - ts(10.0),
            right: tx(lane.chart_x - 8.0),
            bottom: y + ts(10.0),
        };
        draw_text(
            target,
            &score.to_string(),
            label_rect,
            score_font(scale_from_rect(rect, lane.chart_h)),
            true,
            DWRITE_TEXT_ALIGNMENT_TRAILING,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_DIM,
            false,
            brush,
            fmt,
        )?;
    }

    let end = lane.task_start + lane.task_count;
    for task in &graph.tasks[lane.task_start..end] {
        let x = tx(task.point_x);
        target.DrawLine(
            Vector2 {
                X: x,
                Y: ty(lane.chart_y),
            },
            Vector2 {
                X: x,
                Y: ty(lane.chart_y + lane.chart_h),
            },
            &grid,
            (1.0 * ts(1.0)).max(0.5),
            None::<&ID2D1StrokeStyle>,
        );
    }

    Ok(())
}

unsafe fn draw_connections(
    target: &ID2D1RenderTarget,
    graph: &JourneyGraph,
    lane: &JourneyLane,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    if lane.task_count < 2 {
        return Ok(());
    }

    let line = brush(theme::MERMAID_EDGE)?;
    let end = lane.task_start + lane.task_count;
    for pair in graph.tasks[lane.task_start..end].windows(2) {
        target.DrawLine(
            Vector2 {
                X: tx(pair[0].point_x),
                Y: ty(pair[0].point_y),
            },
            Vector2 {
                X: tx(pair[1].point_x),
                Y: ty(pair[1].point_y),
            },
            &line,
            (2.0 * scale).max(1.0),
            None::<&ID2D1StrokeStyle>,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_task(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    task: &JourneyTaskBox,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let stroke = brush(task.stroke)?;
    target.DrawLine(
        Vector2 {
            X: tx(task.point_x),
            Y: ty(task.point_y),
        },
        Vector2 {
            X: tx(task.point_x),
            Y: ty(task.y),
        },
        &stroke,
        (1.1 * scale).max(0.7),
        Some(crate::mermaid::render::sequence_dash_style(factory)),
    );

    let dot = D2D1_ELLIPSE {
        point: Vector2 {
            X: tx(task.point_x),
            Y: ty(task.point_y),
        },
        radiusX: (6.2 * scale).max(3.5),
        radiusY: (6.2 * scale).max(3.5),
    };
    target.FillEllipse(std::ptr::addr_of!(dot), &stroke);

    let rect = D2D_RECT_F {
        left: tx(task.x),
        top: ty(task.y),
        right: tx(task.x) + ts(task.w),
        bottom: ty(task.y) + ts(task.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: ts(6.0).max(3.0),
        radiusY: ts(6.0).max(3.0),
    };
    let fill = brush(task.fill)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.1 * scale).max(0.7),
        None::<&ID2D1StrokeStyle>,
    );

    let inner_pad_x = ts(8.0).min((rect.right - rect.left) * 0.12);
    let score_top = rect.bottom - ts(30.0).max(24.0);
    let task_rect = D2D_RECT_F {
        left: rect.left + inner_pad_x,
        top: rect.top + ts(8.0),
        right: rect.right - inner_pad_x,
        bottom: rect.top + ts(48.0).min((score_top - rect.top) * 0.58),
    };
    draw_text(
        target,
        &task.label,
        task_rect,
        task_font(scale),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
        task.text_color,
        true,
        brush,
        fmt,
    )?;

    let actor_rect = D2D_RECT_F {
        left: task_rect.left,
        top: task_rect.bottom + ts(3.0),
        right: task_rect.right,
        bottom: score_top - ts(5.0),
    };
    if !task.actors.is_empty() {
        draw_text(
            target,
            &task.actors,
            actor_rect,
            actor_font(scale),
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            task.text_color,
            true,
            brush,
            fmt,
        )?;
    }

    let separator = brush(0x1E1E1E)?;
    target.DrawLine(
        Vector2 {
            X: rect.left + inner_pad_x,
            Y: score_top,
        },
        Vector2 {
            X: rect.right - inner_pad_x,
            Y: score_top,
        },
        &separator,
        (1.0 * scale).max(0.7),
        None::<&ID2D1StrokeStyle>,
    );

    draw_score_dots(target, task, rect, score_top, scale, brush, fmt)
}

unsafe fn draw_score_dots(
    target: &ID2D1RenderTarget,
    task: &JourneyTaskBox,
    rect: D2D_RECT_F,
    score_top: f32,
    scale: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let filled = brush(task.stroke)?;
    let empty = brush(0x5A5A5A)?;
    let y = (score_top + rect.bottom) * 0.5;
    let start_x = rect.left + (13.0 * scale).max(8.0);
    let gap = (11.0 * scale).max(8.0);
    let radius = (3.2 * scale).max(2.4);
    for idx in 0..5 {
        let ellipse = D2D1_ELLIPSE {
            point: Vector2 {
                X: start_x + idx as f32 * gap,
                Y: y,
            },
            radiusX: radius,
            radiusY: radius,
        };
        if idx < task.score.clamp(0, 5) as usize {
            target.FillEllipse(std::ptr::addr_of!(ellipse), &filled);
        } else {
            target.DrawEllipse(
                std::ptr::addr_of!(ellipse),
                &empty,
                (1.0 * scale).max(0.7),
                None::<&ID2D1StrokeStyle>,
            );
        }
    }

    let score = format!("{}/5", task.score.clamp(0, 5));
    let score_rect = D2D_RECT_F {
        left: rect.right - (43.0 * scale).max(34.0),
        top: rect.bottom - (24.0 * scale).max(19.0),
        right: rect.right - (8.0 * scale).max(6.0),
        bottom: rect.bottom - (4.0 * scale).max(3.0),
    };
    draw_text(
        target,
        &score,
        score_rect,
        score_font(scale),
        true,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        task.text_color,
        false,
        brush,
        fmt,
    )
}

fn scale_from_rect(rect: D2D_RECT_F, original_h: f32) -> f32 {
    ((rect.bottom - rect.top) / original_h.max(1.0)).max(0.01)
}

fn title_font(scale: f32) -> f32 {
    (TITLE_FONT * scale).max(10.0)
}

fn section_font(scale: f32) -> f32 {
    (SECTION_FONT * scale).max(8.0)
}

fn task_font(scale: f32) -> f32 {
    (TASK_FONT * scale).max(7.8)
}

fn actor_font(scale: f32) -> f32 {
    (ACTOR_FONT * scale).max(7.2)
}

fn score_font(scale: f32) -> f32 {
    (SCORE_FONT * scale).max(7.5)
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    text: &str,
    rect: D2D_RECT_F,
    size: f32,
    bold: bool,
    align: DWRITE_TEXT_ALIGNMENT,
    paragraph: DWRITE_PARAGRAPH_ALIGNMENT,
    color: u32,
    wrap: bool,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let f = fmt(theme::BODY_FONT, size, bold, false)?;
    let _ = f.SetTextAlignment(align);
    let _ = f.SetParagraphAlignment(paragraph);
    let _ = f.SetWordWrapping(if wrap {
        DWRITE_WORD_WRAPPING_WRAP
    } else {
        DWRITE_WORD_WRAPPING_NO_WRAP
    });
    let br = brush(color)?;
    let buf: Vec<u16> = text.encode_utf16().collect();
    target.DrawText(
        &buf,
        &f,
        std::ptr::addr_of!(rect),
        &br,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_journal_style_journey() {
        let source = r#"journey
title Developer First PR Journal
section Setup
Clone repo: 5: Developer
Run release build: 3: Developer
section Change
Find renderer path: 4: Developer, Reviewer
Capture testsnap: 5: Developer
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Journey(db) = diagram else {
            panic!("expected journey");
        };
        let graph = build(&db);
        assert!(graph.sections.iter().any(|s| s.label == "Setup"));
        assert!(graph.sections.iter().any(|s| s.label == "Change"));
        assert_eq!(graph.tasks.len(), 4);
        assert!(graph.tasks.iter().any(|task| task.score == 3));
        assert_eq!(graph.lanes.len(), 2);
        assert!(graph.width > 500.0);
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let source = r#"journey
title Incident Response Journal
section Detect
Alert fires: 3: On-call
Confirm impact: 2: On-call, Support
section Mitigate
Rollback service: 4: Developer, SRE
Validate recovery: 5: SRE, Support

%% @group Detect x=72 y=48 w=250 h=38
%% @node "Alert fires" x=82 y=245 w=150 h=106
%% @node "Confirm impact" x=252 y=245 w=154 h=112
%% @node "Rollback service" x=82 y=515 w=164 h=108
%% @graph w=620 h=680
"#;
        let stripped = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("%% @"))
            .collect::<Vec<_>>()
            .join("\n");
        let diagram = selkie::parse(&stripped).unwrap();
        let selkie::diagrams::Diagram::Journey(db) = diagram else {
            panic!("expected journey");
        };
        let overrides = crate::mermaid::manual_layout::parse(source).unwrap();
        let graph = build_with_overrides(&db, &overrides);

        let section = graph.sections.iter().find(|s| s.label == "Detect").unwrap();
        assert_eq!(
            (section.x, section.y, section.w, section.h),
            (72.0, 48.0, 250.0, 38.0)
        );
        let alert = graph
            .tasks
            .iter()
            .find(|task| task.label == "Alert fires")
            .unwrap();
        assert_eq!(
            (alert.x, alert.y, alert.w, alert.h),
            (82.0, 245.0, 150.0, 106.0)
        );
        assert_eq!(alert.point_x, 157.0);
        assert_eq!((graph.width, graph.height), (620.0, 680.0));
    }
}
