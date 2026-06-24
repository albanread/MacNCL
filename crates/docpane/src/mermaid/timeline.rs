//! Timeline diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::timeline::{TimelineDb, TimelineTask};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const PAD_X: f32 = 34.0;
const PAD_BOTTOM: f32 = 34.0;
const TITLE_H: f32 = 34.0;
const SECTION_Y: f32 = 44.0;
const SECTION_H: f32 = 42.0;
const SECTION_GAP: f32 = 16.0;
const TASK_Y_WITH_SECTIONS: f32 = 112.0;
const TASK_Y_NO_SECTIONS: f32 = 62.0;
const LINE_GAP: f32 = 38.0;
const EVENT_GAP: f32 = 10.0;
const COL_W: f32 = 178.0;
const NODE_W: f32 = 150.0;
const TASK_FONT: f32 = 11.0;
const EVENT_FONT: f32 = 10.5;
const TITLE_FONT: f32 = 15.0;
const SECTION_FONT: f32 = 11.5;

const COLORS: [(u32, u32, u32); 8] = [
    (0x1F4E79, 0x4FC1FF, 0xFFFFFF),
    (0x385723, 0x6A9955, 0xFFFFFF),
    (0x6B5B1D, 0xDCDCAA, 0xFFFFFF),
    (0x5A3D67, 0xC586C0, 0xFFFFFF),
    (0x6B3B2A, 0xCE9178, 0xFFFFFF),
    (0x246A61, 0x4EC9B0, 0xFFFFFF),
    (0x3E5570, 0x9CDCFE, 0xFFFFFF),
    (0x5D4F26, 0xD7BA7D, 0xFFFFFF),
];

pub fn build(db: &TimelineDb) -> TimelineGraph {
    let tasks = db.get_tasks();
    let has_sections = !db.get_sections().is_empty();
    let title = db.get_title().to_string();
    let title_h = if title.is_empty() { 0.0 } else { TITLE_H };
    let task_y = title_h
        + if has_sections {
            TASK_Y_WITH_SECTIONS
        } else {
            TASK_Y_NO_SECTIONS
        };

    if tasks.is_empty() {
        return TimelineGraph {
            width: 420.0,
            height: task_y + 120.0,
            title,
            line_y: task_y + 70.0,
            sections: Vec::new(),
            items: Vec::new(),
        };
    }

    let mut section_index = HashMap::new();
    for (idx, section) in db.get_sections().iter().enumerate() {
        section_index.insert(section.as_str(), idx);
    }

    let mut items = Vec::new();
    let mut max_bottom = 0.0_f32;
    let line_y = task_y + max_task_height(tasks) + LINE_GAP;
    for (idx, task) in tasks.iter().enumerate() {
        let color_idx = if has_sections {
            section_index
                .get(task.section.as_str())
                .copied()
                .unwrap_or(idx)
        } else {
            idx
        };
        let (fill, stroke, text_color) = COLORS[color_idx % COLORS.len()];
        let x = PAD_X + idx as f32 * COL_W + (COL_W - NODE_W) / 2.0;
        let h = task_height(task);
        let cx = x + NODE_W / 2.0;

        let mut events = Vec::new();
        let mut next_y = line_y + 32.0;
        for event in &task.events {
            let event_h = event_height(event);
            events.push(TimelineEventBox {
                x,
                y: next_y,
                w: NODE_W,
                h: event_h,
                label: event.clone(),
                fill: lighten(fill),
                stroke,
                text_color,
            });
            next_y += event_h + EVENT_GAP;
        }
        max_bottom = max_bottom.max(next_y);

        items.push(TimelineItemBox {
            x,
            y: task_y,
            w: NODE_W,
            h,
            cx,
            label: task.task.clone(),
            fill,
            stroke,
            text_color,
            events,
        });
    }

    let sections = if has_sections {
        build_sections(db.get_sections(), tasks, title_h, &section_index)
    } else {
        Vec::new()
    };

    TimelineGraph {
        width: PAD_X * 2.0 + tasks.len() as f32 * COL_W,
        height: max_bottom.max(line_y + 70.0) + PAD_BOTTOM,
        title,
        line_y,
        sections,
        items,
    }
}

pub fn build_with_overrides(db: &TimelineDb, overrides: &ManualLayoutOverrides) -> TimelineGraph {
    let mut graph = build(db);
    apply_manual_overrides(&mut graph, overrides);
    graph
}

fn apply_manual_overrides(graph: &mut TimelineGraph, overrides: &ManualLayoutOverrides) {
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

    for item in &mut graph.items {
        if let Some(ov) = overrides.object(&item.label) {
            apply_box_override(&mut item.x, &mut item.y, &mut item.w, &mut item.h, ov);
            item.cx = item.x + item.w / 2.0;
        }
        for event in &mut item.events {
            if let Some(ov) = overrides.object(&event.label) {
                apply_box_override(&mut event.x, &mut event.y, &mut event.w, &mut event.h, ov);
            }
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

fn grow_bounds(graph: &mut TimelineGraph) {
    let mut width = graph.width.max(1.0);
    let mut height = graph.height.max(1.0);
    for section in &graph.sections {
        width = width.max(section.x + section.w + PAD_X);
        height = height.max(section.y + section.h + PAD_BOTTOM);
    }
    for item in &graph.items {
        width = width.max(item.x + item.w + PAD_X);
        height = height.max(item.y + item.h + PAD_BOTTOM);
        for event in &item.events {
            width = width.max(event.x + event.w + PAD_X);
            height = height.max(event.y + event.h + PAD_BOTTOM);
        }
    }
    graph.width = width;
    graph.height = height;
}

fn build_sections(
    sections: &[String],
    tasks: &[TimelineTask],
    title_h: f32,
    section_index: &HashMap<&str, usize>,
) -> Vec<TimelineSectionBox> {
    let mut out = Vec::new();
    for (idx, section) in sections.iter().enumerate() {
        let positions: Vec<usize> = tasks
            .iter()
            .enumerate()
            .filter_map(|(task_idx, task)| (task.section == *section).then_some(task_idx))
            .collect();
        if positions.is_empty() {
            continue;
        }
        let first = *positions.first().unwrap();
        let last = *positions.last().unwrap();
        let left = PAD_X + first as f32 * COL_W + (COL_W - NODE_W) / 2.0 - SECTION_GAP / 2.0;
        let right =
            PAD_X + last as f32 * COL_W + (COL_W - NODE_W) / 2.0 + NODE_W + SECTION_GAP / 2.0;
        let color_idx = section_index.get(section.as_str()).copied().unwrap_or(idx);
        let (_, stroke, _) = COLORS[color_idx % COLORS.len()];
        out.push(TimelineSectionBox {
            x: left,
            y: title_h + SECTION_Y,
            w: right - left,
            h: SECTION_H,
            label: section.clone(),
            fill: 0x202020,
            stroke,
            text_color: theme::TEXT_BRIGHT,
        });
    }
    out
}

fn max_task_height(tasks: &[TimelineTask]) -> f32 {
    tasks
        .iter()
        .map(task_height)
        .fold(0.0_f32, f32::max)
        .max(48.0)
}

fn task_height(task: &TimelineTask) -> f32 {
    text_box_height(&task.task, 22, 52.0, 86.0)
}

fn event_height(text: &str) -> f32 {
    text_box_height(text, 18, 46.0, 100.0)
}

fn text_box_height(text: &str, chars_per_line: usize, min_h: f32, max_h: f32) -> f32 {
    let line_count = text
        .split_whitespace()
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
        .0;
    (24.0 + line_count as f32 * 14.0).clamp(min_h, max_h)
}

fn lighten(color: u32) -> u32 {
    let r = ((color >> 16) & 0xFF) as f32;
    let g = ((color >> 8) & 0xFF) as f32;
    let b = (color & 0xFF) as f32;
    let mix = |v: f32| (v + (255.0 - v) * 0.16).min(255.0) as u32;
    (mix(r) << 16) | (mix(g) << 8) | mix(b)
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &TimelineGraph,
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

    draw_timeline_line(target, factory, graph, scale, &tx, &ty, &mut brush)?;

    for item in &graph.items {
        draw_item(
            target, factory, item, graph, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_section(
    target: &ID2D1RenderTarget,
    section: &TimelineSectionBox,
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
        radiusX: 5.0,
        radiusY: 5.0,
    };
    let fill = brush(section.fill)?;
    let stroke = brush(section.stroke)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        1.1,
        None::<&ID2D1StrokeStyle>,
    );
    draw_text(
        target,
        &section.label,
        rect,
        section_font((rect.bottom - rect.top) / section.h.max(1.0)),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        section.text_color,
        false,
        brush,
        fmt,
    )
}

unsafe fn draw_timeline_line(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &TimelineGraph,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let Some(first) = graph.items.first() else {
        return Ok(());
    };
    let Some(last) = graph.items.last() else {
        return Ok(());
    };
    let line = brush(theme::MERMAID_EDGE)?;
    let y = ty(graph.line_y);
    let start = tx(first.cx);
    let end = tx(last.cx + 40.0);
    target.DrawLine(
        Vector2 { X: start, Y: y },
        Vector2 { X: end, Y: y },
        &line,
        (2.0 * scale).max(1.0),
        None::<&ID2D1StrokeStyle>,
    );
    draw_arrow(
        target,
        factory,
        (end - 30.0 * scale, y),
        (end, y),
        scale,
        &line,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_item(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    item: &TimelineItemBox,
    graph: &TimelineGraph,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let line = brush(item.stroke)?;
    let x = tx(item.cx);
    let task_bottom = ty(item.y + item.h);
    let line_y = ty(graph.line_y);
    target.DrawLine(
        Vector2 {
            X: x,
            Y: task_bottom,
        },
        Vector2 { X: x, Y: line_y },
        &line,
        (1.2 * scale).max(0.7),
        Some(crate::mermaid::render::sequence_dash_style(factory)),
    );
    if let Some(last_event) = item.events.last() {
        target.DrawLine(
            Vector2 { X: x, Y: line_y },
            Vector2 {
                X: x,
                Y: ty(last_event.y + last_event.h),
            },
            &line,
            (1.2 * scale).max(0.7),
            Some(crate::mermaid::render::sequence_dash_style(factory)),
        );
    }

    let dot = D2D1_ELLIPSE {
        point: Vector2 { X: x, Y: line_y },
        radiusX: (5.0 * scale).max(3.0),
        radiusY: (5.0 * scale).max(3.0),
    };
    target.FillEllipse(std::ptr::addr_of!(dot), &line);

    draw_box(
        target,
        item.x,
        item.y,
        item.w,
        item.h,
        &item.label,
        item.fill,
        item.stroke,
        item.text_color,
        task_font(scale),
        true,
        tx,
        ty,
        ts,
        brush,
        fmt,
    )?;

    for event in &item.events {
        draw_box(
            target,
            event.x,
            event.y,
            event.w,
            event.h,
            &event.label,
            event.fill,
            event.stroke,
            event.text_color,
            event_font(scale),
            false,
            tx,
            ty,
            ts,
            brush,
            fmt,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_box(
    target: &ID2D1RenderTarget,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: &str,
    fill: u32,
    stroke: u32,
    text_color: u32,
    font_size: f32,
    bold: bool,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(x),
        top: ty(y),
        right: tx(x) + ts(w),
        bottom: ty(y) + ts(h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: ts(6.0).max(3.0),
        radiusY: ts(6.0).max(3.0),
    };
    let fill_br = brush(fill)?;
    let stroke_br = brush(stroke)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke_br,
        1.1,
        None::<&ID2D1StrokeStyle>,
    );

    let box_w = (rect.right - rect.left).max(1.0);
    let box_h = (rect.bottom - rect.top).max(1.0);
    let pad_x = ts(7.0).min(box_w * 0.14);
    let pad_y = ts(4.0).min(box_h * 0.12);
    let text_rect = D2D_RECT_F {
        left: rect.left + pad_x,
        top: rect.top + pad_y,
        right: rect.right - pad_x,
        bottom: rect.bottom - pad_y,
    };
    draw_text(
        target,
        label,
        text_rect,
        font_size,
        bold,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        text_color,
        true,
        brush,
        fmt,
    )
}

unsafe fn draw_arrow(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    from: (f32, f32),
    to: (f32, f32),
    scale: f32,
    brush: &ID2D1SolidColorBrush,
) -> Result<()> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let size = (10.0 * scale).max(7.0);
    let back = (to.0 - ux * size, to.1 - uy * size);
    let half = size * 0.58;
    let pts = [
        to,
        (back.0 + px * half, back.1 + py * half),
        (back.0 - px * half, back.1 - py * half),
    ];
    let geo = crate::mermaid::render::build_polygon_pub(factory, &pts)?;
    target.FillGeometry(&geo, brush, None);
    Ok(())
}

fn title_font(scale: f32) -> f32 {
    (TITLE_FONT * scale).max(10.0)
}

fn section_font(scale: f32) -> f32 {
    (SECTION_FONT * scale).max(8.0)
}

fn task_font(scale: f32) -> f32 {
    (TASK_FONT * scale).max(8.0)
}

fn event_font(scale: f32) -> f32 {
    (EVENT_FONT * scale).max(7.5)
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
    fn builds_sectioned_timeline() {
        let source = r#"timeline
title Incident Timeline
section Detection
Alert fired : SLO burn rate triggered
Triage : Service owner paged : Impact confirmed
section Recovery
Mitigation : Cache disabled
Resolved : Error rate back to normal
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Timeline(db) = diagram else {
            panic!("expected timeline");
        };
        let graph = build(&db);
        assert_eq!(graph.sections.len(), 2);
        assert_eq!(graph.items.len(), 4);
        assert!(graph.items.iter().any(|item| item.events.len() == 2));
        assert!(graph.width > 500.0);
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let source = r#"timeline
title Release Timeline
section Design
API freeze : ADR accepted : Review complete
section Ship
Release candidate : Smoke test : Publish notes

%% @group Design x=54 y=48 w=250 h=44
%% @node "API freeze" x=78 y=128 w=168 h=58
%% @node "ADR accepted" x=78 y=252 w=168 h=54
%% @node "Release candidate" x=358 y=128 w=178 h=58
%% @graph w=640 h=390
"#;
        let diagram = selkie::parse(
            &source
                .lines()
                .filter(|line| !line.trim_start().starts_with("%% @"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let selkie::diagrams::Diagram::Timeline(db) = diagram else {
            panic!("expected timeline");
        };
        let overrides = crate::mermaid::manual_layout::parse(source).unwrap();
        let graph = build_with_overrides(&db, &overrides);

        let section = graph.sections.iter().find(|s| s.label == "Design").unwrap();
        assert_eq!(
            (section.x, section.y, section.w, section.h),
            (54.0, 48.0, 250.0, 44.0)
        );
        let task = graph
            .items
            .iter()
            .find(|item| item.label == "API freeze")
            .unwrap();
        assert_eq!((task.x, task.y, task.w, task.h), (78.0, 128.0, 168.0, 58.0));
        assert_eq!(task.cx, 162.0);
        let event = task
            .events
            .iter()
            .find(|event| event.label == "ADR accepted")
            .unwrap();
        assert_eq!(
            (event.x, event.y, event.w, event.h),
            (78.0, 252.0, 168.0, 54.0)
        );
        assert_eq!((graph.width, graph.height), (640.0, 390.0));
    }
}
