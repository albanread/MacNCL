//! Gantt diagram build + Direct2D renderer.

use chrono::{Duration, NaiveDateTime};
use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::gantt::{GanttDb, Task};

use crate::mermaid::ir::*;
use crate::theme;

const LEFT_W: f32 = 320.0;
const SECTION_W: f32 = 104.0;
const CHART_W: f32 = 620.0;
const RIGHT_PAD: f32 = 28.0;
const TOP_PAD: f32 = 58.0;
const TITLE_H: f32 = 30.0;
const ROW_H: f32 = 28.0;
const BAR_H: f32 = 16.0;
const BOTTOM_PAD: f32 = 48.0;
const TASK_FONT: f32 = 11.5;
const AXIS_FONT: f32 = 10.5;
const TITLE_FONT: f32 = 15.0;

pub fn build(db: &mut GanttDb) -> GanttGraph {
    let tasks = db.get_tasks();
    let resolved: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.start_time.is_some() && task_end(task).is_some())
        .collect();

    let title = db.get_diagram_title().to_string();
    if resolved.is_empty() {
        return GanttGraph {
            width: LEFT_W + CHART_W + RIGHT_PAD,
            height: 160.0,
            title,
            chart_x: LEFT_W,
            chart_y: TOP_PAD + TITLE_H,
            chart_w: CHART_W,
            chart_h: ROW_H,
            ticks: Vec::new(),
            sections: Vec::new(),
            tasks: Vec::new(),
        };
    }

    let mut min_time = resolved[0].start_time.unwrap();
    let mut max_time = task_end(resolved[0]).unwrap();
    for task in &resolved {
        let start = task.start_time.unwrap();
        let end = task_end(task).unwrap();
        min_time = min_time.min(start);
        max_time = max_time.max(end.max(start));
    }
    if max_time <= min_time {
        max_time = min_time + Duration::days(1);
    }

    let chart_x = LEFT_W;
    let chart_y = TOP_PAD + if title.is_empty() { 0.0 } else { TITLE_H };
    let range_secs = (max_time - min_time).num_seconds().max(1) as f32;

    let mut out_tasks = Vec::new();
    let mut sections = Vec::new();
    let mut current_section = String::new();
    let mut section_start_row = 0usize;
    let mut row = 0usize;

    for task in &resolved {
        if task.flags.vert {
            let x = chart_x + time_x(task.start_time.unwrap(), min_time, range_secs);
            let (_, stroke, text) = task_colors(task);
            out_tasks.push(GanttTask {
                x,
                y: chart_y,
                w: 1.5,
                h: 0.0,
                label: task.task.clone(),
                start_label: format_date(task.start_time.unwrap()),
                milestone: false,
                vertical: true,
                fill: stroke,
                stroke,
                text_color: text,
            });
            continue;
        }

        if task.section != current_section {
            if !current_section.is_empty() && row > section_start_row {
                sections.push(section_box(
                    &current_section,
                    section_start_row,
                    row,
                    sections.len(),
                    chart_y,
                ));
            }
            current_section = task.section.clone();
            section_start_row = row;
        }

        let start = task.start_time.unwrap();
        let end = task_end(task).unwrap_or(start);
        let start_x = chart_x + time_x(start, min_time, range_secs);
        let end_x = chart_x + time_x(end, min_time, range_secs);
        let y = chart_y + row as f32 * ROW_H + (ROW_H - BAR_H) / 2.0;
        let (fill, stroke, text_color) = task_colors(task);
        let (x, w) = if task.flags.milestone {
            (start_x - BAR_H / 2.0, BAR_H)
        } else {
            (start_x, (end_x - start_x).max(3.0))
        };

        out_tasks.push(GanttTask {
            x,
            y,
            w,
            h: BAR_H,
            label: task.task.clone(),
            start_label: format_date(start),
            milestone: task.flags.milestone,
            vertical: false,
            fill,
            stroke,
            text_color,
        });
        row += 1;
    }

    if !current_section.is_empty() && row > section_start_row {
        sections.push(section_box(
            &current_section,
            section_start_row,
            row,
            sections.len(),
            chart_y,
        ));
    }

    let row_count = row.max(1);
    let chart_h = row_count as f32 * ROW_H;
    for task in &mut out_tasks {
        if task.vertical {
            task.h = chart_h;
        }
    }

    let ticks = build_ticks(min_time, max_time, range_secs, CHART_W);
    GanttGraph {
        width: LEFT_W + CHART_W + RIGHT_PAD,
        height: chart_y + chart_h + BOTTOM_PAD,
        title,
        chart_x,
        chart_y,
        chart_w: CHART_W,
        chart_h,
        ticks,
        sections,
        tasks: out_tasks,
    }
}

fn task_end(task: &Task) -> Option<NaiveDateTime> {
    task.render_end_time.or(task.end_time)
}

fn time_x(time: NaiveDateTime, min_time: NaiveDateTime, range_secs: f32) -> f32 {
    let secs = (time - min_time).num_seconds() as f32;
    (secs / range_secs).clamp(0.0, 1.0) * CHART_W
}

fn section_box(
    label: &str,
    start_row: usize,
    end_row: usize,
    section_idx: usize,
    chart_y: f32,
) -> GanttSection {
    GanttSection {
        y: chart_y + start_row as f32 * ROW_H,
        h: (end_row - start_row) as f32 * ROW_H,
        label: label.replace("<br/>", " ").replace("<br>", " "),
        fill: if section_idx.is_multiple_of(2) {
            0x252526
        } else {
            0x202020
        },
    }
}

fn task_colors(task: &Task) -> (u32, u32, u32) {
    if task.flags.critical && task.flags.done {
        (0x6F5454, 0xF14C4C, 0xFFFFFF)
    } else if task.flags.critical && task.flags.active {
        (0xCE9178, 0xF14C4C, 0x1E1E1E)
    } else if task.flags.critical {
        (0xB73A3A, 0xF14C4C, 0xFFFFFF)
    } else if task.flags.done {
        (0x4E7A3E, 0x6A9955, 0xFFFFFF)
    } else if task.flags.active {
        (0xDCDCAA, 0xB5A642, 0x1E1E1E)
    } else {
        (0x438DD5, 0x6FA8DC, 0xFFFFFF)
    }
}

fn build_ticks(
    min_time: NaiveDateTime,
    max_time: NaiveDateTime,
    range_secs: f32,
    chart_w: f32,
) -> Vec<GanttTick> {
    let range_days = (max_time - min_time).num_days().max(1);
    let tick_count = if range_days <= 7 {
        (range_days + 1).min(8) as usize
    } else if range_days <= 28 {
        7
    } else {
        6
    };

    let mut ticks = Vec::new();
    let denom = tick_count.saturating_sub(1).max(1) as i64;
    for idx in 0..tick_count {
        let secs = (range_secs as i64 * idx as i64) / denom;
        let time = min_time + Duration::seconds(secs);
        ticks.push(GanttTick {
            x: (idx as f32 / denom as f32) * chart_w,
            label: format_tick(time, range_days),
        });
    }
    ticks
}

fn format_tick(time: NaiveDateTime, range_days: i64) -> String {
    if range_days > 370 {
        time.format("%b %Y").to_string()
    } else if range_days > 1 {
        time.format("%b %d").to_string()
    } else {
        time.format("%H:%M").to_string()
    }
}

fn format_date(time: NaiveDateTime) -> String {
    time.format("%Y-%m-%d").to_string()
}

fn title_font(scale: f32) -> f32 {
    (TITLE_FONT * scale).max(10.0)
}

fn task_font(scale: f32) -> f32 {
    (TASK_FONT * scale).max(8.5)
}

fn axis_font(scale: f32) -> f32 {
    (AXIS_FONT * scale).max(8.0)
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &GanttGraph,
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

    draw_grid(target, graph, scale, &tx, &ty, &mut brush, &mut fmt)?;

    for section in &graph.sections {
        draw_section(
            target, section, graph, scale, &tx, &ty, &mut brush, &mut fmt,
        )?;
    }
    for task in &graph.tasks {
        draw_task(
            target, factory, task, graph, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_grid(
    target: &ID2D1RenderTarget,
    graph: &GanttGraph,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let chart = D2D_RECT_F {
        left: tx(graph.chart_x),
        top: ty(graph.chart_y),
        right: tx(graph.chart_x + graph.chart_w),
        bottom: ty(graph.chart_y + graph.chart_h),
    };
    let bg = brush(0x1B1B1B)?;
    let border = brush(theme::BORDER)?;
    target.FillRectangle(std::ptr::addr_of!(chart), &bg);
    target.DrawRectangle(
        std::ptr::addr_of!(chart),
        &border,
        (1.0 * scale).max(0.75),
        None::<&ID2D1StrokeStyle>,
    );

    let grid_br = brush(0x343434)?;
    for tick in &graph.ticks {
        let x = tx(graph.chart_x + tick.x);
        target.DrawLine(
            Vector2 { X: x, Y: chart.top },
            Vector2 {
                X: x,
                Y: chart.bottom,
            },
            &grid_br,
            (1.0 * scale).max(0.6),
            None::<&ID2D1StrokeStyle>,
        );
        let tick_rect = D2D_RECT_F {
            left: x - 38.0 * scale,
            top: chart.top - 26.0 * scale,
            right: x + 38.0 * scale,
            bottom: chart.top - 6.0 * scale,
        };
        draw_text(
            target,
            &tick.label,
            tick_rect,
            axis_font(scale),
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_DIM,
            false,
            brush,
            fmt,
        )?;
    }

    let rows = (graph.chart_h / ROW_H).round() as usize;
    for row in 0..=rows {
        let y = ty(graph.chart_y + row as f32 * ROW_H);
        target.DrawLine(
            Vector2 { X: tx(0.0), Y: y },
            Vector2 {
                X: tx(graph.chart_x + graph.chart_w),
                Y: y,
            },
            &grid_br,
            (1.0 * scale).max(0.5),
            None::<&ID2D1StrokeStyle>,
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_section(
    target: &ID2D1RenderTarget,
    section: &GanttSection,
    graph: &GanttGraph,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(0.0),
        top: ty(section.y),
        right: tx(graph.chart_x + graph.chart_w),
        bottom: ty(section.y + section.h),
    };
    let fill = brush(section.fill)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &fill);

    let label_rect = D2D_RECT_F {
        left: tx(10.0),
        top: rect.top,
        right: tx(SECTION_W - 6.0),
        bottom: rect.bottom,
    };
    draw_text(
        target,
        &section.label,
        label_rect,
        task_font(scale),
        true,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        theme::MERMAID_GROUP_TITLE,
        false,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_task(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    task: &GanttTask,
    graph: &GanttGraph,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if task.vertical {
        let x = tx(task.x);
        let line = brush(task.stroke)?;
        target.DrawLine(
            Vector2 {
                X: x,
                Y: ty(graph.chart_y),
            },
            Vector2 {
                X: x,
                Y: ty(graph.chart_y + graph.chart_h),
            },
            &line,
            (1.5 * scale).max(0.8),
            Some(crate::mermaid::render::sequence_dash_style(factory)),
        );
        let label_rect = D2D_RECT_F {
            left: x - 54.0 * scale,
            top: ty(graph.chart_y + graph.chart_h + 8.0),
            right: x + 54.0 * scale,
            bottom: ty(graph.chart_y + graph.chart_h + 28.0),
        };
        return draw_text(
            target,
            &task.label,
            label_rect,
            axis_font(scale),
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            task.stroke,
            false,
            brush,
            fmt,
        );
    }

    let label_rect = D2D_RECT_F {
        left: tx(SECTION_W + 8.0),
        top: ty(task.y - 4.0),
        right: tx(graph.chart_x - 12.0),
        bottom: ty(task.y + task.h + 4.0),
    };
    draw_text(
        target,
        &task.label,
        label_rect,
        task_font(scale),
        false,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        theme::TEXT,
        false,
        brush,
        fmt,
    )?;

    if task.milestone {
        draw_milestone(target, factory, task, scale, tx, ty, ts, brush)?;
        let right = tx(task.x + task.w + 6.0);
        let label_w = 118.0 * scale;
        let graph_right = tx(graph.width);
        let (left, right, align) = if right + label_w > graph_right {
            (
                tx(task.x) - label_w - 6.0 * scale,
                tx(task.x) - 6.0 * scale,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
            )
        } else {
            (right, right + label_w, DWRITE_TEXT_ALIGNMENT_LEADING)
        };
        let rect = D2D_RECT_F {
            left,
            top: ty(task.y - 2.0),
            right,
            bottom: ty(task.y + task.h + 2.0),
        };
        draw_text(
            target,
            &task.start_label,
            rect,
            axis_font(scale),
            false,
            align,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_DIM,
            false,
            brush,
            fmt,
        )?;
        return Ok(());
    }

    let rect = D2D_RECT_F {
        left: tx(task.x),
        top: ty(task.y),
        right: tx(task.x + task.w),
        bottom: ty(task.y + task.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 4.0 * scale,
        radiusY: 4.0 * scale,
    };
    let fill = brush(task.fill)?;
    let stroke = brush(task.stroke)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.2 * scale).max(0.8),
        None::<&ID2D1StrokeStyle>,
    );

    let text_w = task.label.chars().count() as f32 * task_font(scale) * 0.55;
    if rect.right - rect.left > text_w + 16.0 * scale {
        draw_text(
            target,
            &task.label,
            rect,
            task_font(scale),
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            task.text_color,
            false,
            brush,
            fmt,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_milestone(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    task: &GanttTask,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let x = tx(task.x);
    let y = ty(task.y);
    let w = ts(task.w);
    let h = ts(task.h);
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let pts = [(cx, y), (x + w, cy), (cx, y + h), (x, cy)];
    let geo = crate::mermaid::render::build_polygon_pub(factory, &pts)?;
    let fill = brush(task.fill)?;
    let stroke = brush(task.stroke)?;
    target.FillGeometry(&geo, &fill, None);
    target.DrawGeometry(
        &geo,
        &stroke,
        (1.2 * scale).max(0.8),
        None::<&ID2D1StrokeStyle>,
    );
    Ok(())
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
    fn builds_release_plan() {
        let source = r#"gantt
title Release Plan
dateFormat YYYY-MM-DD
section Planning
Requirements :done, req, 2026-05-25, 2d
Design :active, design, after req, 2d
section Delivery
Implementation :crit, impl, after design, 4d
Release :milestone, ship, after impl, 0d
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Gantt(mut db) = diagram else {
            panic!("expected gantt");
        };
        let graph = build(&mut db);
        assert_eq!(graph.sections.len(), 2);
        assert_eq!(graph.tasks.len(), 4);
        assert!(graph.tasks.iter().any(|task| task.milestone));
        assert!(!graph.ticks.is_empty());
    }
}
