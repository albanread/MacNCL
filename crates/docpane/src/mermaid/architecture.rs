//! Architecture-beta diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::architecture::{
    ArchitectureDb, ArchitectureDirection as SDirection, ArchitectureEdge, ArchitectureGroup,
};
use selkie::layout::{self, CharacterSizeEstimator, LayoutGraph, LayoutNode, ToLayoutGraph};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const MARGIN: f32 = 36.0;
const TITLE_H: f32 = 38.0;
const SERVICE_W: f32 = 148.0;
const SERVICE_H: f32 = 132.0;
const JUNCTION_SIZE: f32 = 22.0;
const GROUP_LABEL_H: f32 = 34.0;
const GROUP_PAD: f32 = 18.0;
const ICON_SIZE: f32 = 42.0;

const TITLE_FONT: f32 = 15.0;
const GROUP_FONT: f32 = 12.4;
const SERVICE_FONT: f32 = 12.0;
const ICON_FONT: f32 = 10.0;
const EDGE_FONT: f32 = 10.8;

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    junction: bool,
}

#[cfg(test)]
fn build(db: &ArchitectureDb) -> std::result::Result<ArchitectureGraph, String> {
    build_with_overrides(db, &ManualLayoutOverrides::default())
}

pub fn build_with_overrides(
    db: &ArchitectureDb,
    overrides: &ManualLayoutOverrides,
) -> std::result::Result<ArchitectureGraph, String> {
    let estimator = CharacterSizeEstimator::default();
    let lg = db
        .to_layout_graph(&estimator)
        .map_err(|e| format!("layout-graph build: {e}"))?;
    let lg = layout::layout(lg).map_err(|e| format!("layout: {e}"))?;
    Ok(convert(db, &lg, overrides))
}

fn convert(
    db: &ArchitectureDb,
    lg: &LayoutGraph,
    overrides: &ManualLayoutOverrides,
) -> ArchitectureGraph {
    let bx = lg.bounds_x.unwrap_or(0.0) as f32;
    let by = lg.bounds_y.unwrap_or(0.0) as f32;
    let title = db.get_title().to_string();
    let title_h = if title.is_empty() { 0.0 } else { TITLE_H };
    let oy = MARGIN + title_h;

    let mut rects: HashMap<String, Rect> = HashMap::new();
    let mut services = Vec::new();
    let mut service_indices = HashMap::new();
    let mut object_indices = HashMap::new();

    let mut service_refs = db.get_services();
    service_refs.sort_by_key(|service| service.id.as_str());
    for service in service_refs {
        let Some(node) = lg.get_node(&service.id) else {
            continue;
        };
        let (nx, ny) = node_origin(node, bx, by, oy);
        let cx = nx + node.width as f32 / 2.0;
        let cy = ny + node.height as f32 / 2.0;
        let rect = Rect {
            x: cx - SERVICE_W / 2.0,
            y: cy - SERVICE_H / 2.0,
            w: SERVICE_W,
            h: SERVICE_H,
            junction: false,
        };
        rects.insert(service.id.clone(), rect);
        object_indices.insert(service.id.clone(), services.len());
        service_indices.insert(service.id.clone(), services.len());
        services.push(ArchitectureServiceBox {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            title: service.title.clone().unwrap_or_else(|| service.id.clone()),
            icon: service.icon.clone().unwrap_or_default(),
            icon_text: service.icon_text.clone().unwrap_or_default(),
            junction: false,
        });
    }

    let mut junction_refs = db.get_junctions();
    junction_refs.sort_by_key(|junction| junction.id.as_str());
    for junction in junction_refs {
        let Some(node) = lg.get_node(&junction.id) else {
            continue;
        };
        let (nx, ny) = node_origin(node, bx, by, oy);
        let cx = nx + node.width as f32 / 2.0;
        let cy = ny + node.height as f32 / 2.0;
        let rect = Rect {
            x: cx - JUNCTION_SIZE / 2.0,
            y: cy - JUNCTION_SIZE / 2.0,
            w: JUNCTION_SIZE,
            h: JUNCTION_SIZE,
            junction: true,
        };
        rects.insert(junction.id.clone(), rect);
        object_indices.insert(junction.id.clone(), services.len());
        services.push(ArchitectureServiceBox {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            title: String::new(),
            icon: String::new(),
            icon_text: String::new(),
            junction: true,
        });
    }

    spread_overlapping_siblings(db, &mut services, &service_indices, &mut rects);
    apply_object_overrides(overrides, &object_indices, &mut services, &mut rects);

    let groups = build_groups(db, lg, bx, by, oy, &mut rects, overrides);

    let mut edges = Vec::new();
    for edge in db.get_edges() {
        if let Some(line) = convert_edge(edge, &rects, overrides) {
            edges.push(line);
        }
    }

    let mut width = lg.width.unwrap_or(0.0) as f32 + MARGIN * 2.0;
    let mut height = lg.height.unwrap_or(0.0) as f32 + MARGIN * 2.0 + title_h + SERVICE_H * 0.35;
    for group in &groups {
        width = width.max(group.x + group.w + MARGIN);
        height = height.max(group.y + group.h + MARGIN);
    }
    for service in &services {
        width = width.max(service.x + service.w + MARGIN);
        height = height.max(service.y + service.h + MARGIN);
    }
    for edge in &edges {
        for (x, y) in &edge.points {
            width = width.max(*x + MARGIN);
            height = height.max(*y + MARGIN);
        }
    }
    if let Some(w) = overrides.graph.w {
        width = w.max(1.0);
    }
    if let Some(h) = overrides.graph.h {
        height = h.max(1.0);
    }

    let min_width = if overrides.graph.w.is_some() {
        1.0
    } else {
        420.0
    };
    let min_height = if overrides.graph.h.is_some() {
        1.0
    } else {
        180.0
    };

    ArchitectureGraph {
        width: width.max(min_width),
        height: height.max(min_height),
        title,
        groups,
        edges,
        services,
    }
}

fn node_origin(node: &LayoutNode, bx: f32, by: f32, oy: f32) -> (f32, f32) {
    (
        node.x.unwrap_or(0.0) as f32 - bx + MARGIN,
        node.y.unwrap_or(0.0) as f32 - by + oy,
    )
}

fn spread_overlapping_siblings(
    db: &ArchitectureDb,
    services: &mut [ArchitectureServiceBox],
    service_indices: &HashMap<String, usize>,
    rects: &mut HashMap<String, Rect>,
) {
    let mut buckets: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for service in db.get_services() {
        buckets
            .entry(service.parent.clone())
            .or_default()
            .push(service.id.clone());
    }

    for ids in buckets.values_mut() {
        if ids.len() < 2 {
            continue;
        }
        ids.sort_by(|a, b| {
            let ra = rects.get(a).copied().unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
                junction: false,
            });
            let rb = rects.get(b).copied().unwrap_or(Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
                junction: false,
            });
            (ra.y, ra.x)
                .partial_cmp(&(rb.y, rb.x))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if !has_overlap(ids, rects) {
            continue;
        }

        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        for id in ids.iter() {
            if let Some(rect) = rects.get(id) {
                min_x = min_x.min(rect.x);
                min_y = min_y.min(rect.y);
                max_x = max_x.max(rect.x + rect.w);
            }
        }
        if min_x == f32::MAX {
            continue;
        }

        let cols = if ids.len() <= 2 { ids.len() } else { 2 };
        let col_gap = 54.0;
        let row_gap = 46.0;
        let total_w = cols as f32 * SERVICE_W + (cols.saturating_sub(1)) as f32 * col_gap;
        let center_x = (min_x + max_x) * 0.5;
        let start_x = (center_x - total_w * 0.5).max(MARGIN);

        for (idx, id) in ids.iter().enumerate() {
            let col = idx % cols;
            let row = idx / cols;
            let x = start_x + col as f32 * (SERVICE_W + col_gap);
            let y = min_y + row as f32 * (SERVICE_H + row_gap);
            if let Some(rect) = rects.get_mut(id) {
                rect.x = x;
                rect.y = y;
            }
            if let Some(service_idx) = service_indices.get(id).copied() {
                services[service_idx].x = x;
                services[service_idx].y = y;
            }
        }
    }
}

fn has_overlap(ids: &[String], rects: &HashMap<String, Rect>) -> bool {
    for (idx, a_id) in ids.iter().enumerate() {
        let Some(a) = rects.get(a_id) else {
            continue;
        };
        for b_id in ids.iter().skip(idx + 1) {
            let Some(b) = rects.get(b_id) else {
                continue;
            };
            let separated = a.x + a.w + 10.0 <= b.x
                || b.x + b.w + 10.0 <= a.x
                || a.y + a.h + 10.0 <= b.y
                || b.y + b.h + 10.0 <= a.y;
            if !separated {
                return true;
            }
        }
    }
    false
}

fn apply_object_overrides(
    overrides: &ManualLayoutOverrides,
    object_indices: &HashMap<String, usize>,
    services: &mut [ArchitectureServiceBox],
    rects: &mut HashMap<String, Rect>,
) {
    for (id, ov) in overrides.objects() {
        let Some(rect) = rects.get_mut(id) else {
            continue;
        };
        apply_box_override(rect, ov);
        if let Some(idx) = object_indices.get(id).copied() {
            services[idx].x = rect.x;
            services[idx].y = rect.y;
            services[idx].w = rect.w;
            services[idx].h = rect.h;
        }
    }
}

fn apply_box_override(rect: &mut Rect, ov: &BoxOverride) {
    if let Some(x) = ov.x {
        rect.x = x;
    }
    if let Some(y) = ov.y {
        rect.y = y;
    }
    if let Some(w) = ov.w {
        rect.w = w.max(1.0);
    }
    if let Some(h) = ov.h {
        rect.h = h.max(1.0);
    }
}

fn build_groups(
    db: &ArchitectureDb,
    lg: &LayoutGraph,
    bx: f32,
    by: f32,
    oy: f32,
    rects: &mut HashMap<String, Rect>,
    overrides: &ManualLayoutOverrides,
) -> Vec<ArchitectureGroupBox> {
    let mut group_refs = db.get_groups();
    group_refs.sort_by_key(|group| std::cmp::Reverse(group_depth(group, db)));

    let mut local: HashMap<String, ArchitectureGroupBox> = HashMap::new();
    for group in &group_refs {
        let fallback = lg.get_node(&group.id).map(|node| {
            let (x, y) = node_origin(node, bx, by, oy);
            Rect {
                x,
                y,
                w: node.width as f32,
                h: node.height as f32 + GROUP_LABEL_H,
                junction: false,
            }
        });

        let rect = group_content_rect(group, db, rects, &local).or(fallback);
        let Some(mut rect) = rect else {
            continue;
        };
        if let Some(ov) = overrides.group(&group.id) {
            apply_box_override(&mut rect, ov);
        }

        rects.insert(group.id.clone(), rect);
        local.insert(
            group.id.clone(),
            ArchitectureGroupBox {
                x: rect.x,
                y: rect.y,
                w: rect.w,
                h: rect.h,
                title: group.title.clone().unwrap_or_else(|| group.id.clone()),
            },
        );
    }

    let mut groups: Vec<ArchitectureGroupBox> = local.into_values().collect();
    groups.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));
    groups
}

fn group_content_rect(
    group: &ArchitectureGroup,
    db: &ArchitectureDb,
    rects: &HashMap<String, Rect>,
    local_groups: &HashMap<String, ArchitectureGroupBox>,
) -> Option<Rect> {
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;

    let mut absorb = |rect: Rect| {
        min_x = min_x.min(rect.x);
        min_y = min_y.min(rect.y);
        max_x = max_x.max(rect.x + rect.w);
        max_y = max_y.max(rect.y + rect.h);
    };

    for service in db.get_services() {
        if service.parent.as_deref() == Some(group.id.as_str()) {
            if let Some(rect) = rects.get(&service.id) {
                absorb(*rect);
            }
        }
    }
    for junction in db.get_junctions() {
        if junction.parent.as_deref() == Some(group.id.as_str()) {
            if let Some(rect) = rects.get(&junction.id) {
                absorb(*rect);
            }
        }
    }
    for child in db.get_groups() {
        if child.parent.as_deref() == Some(group.id.as_str()) {
            if let Some(child_box) = local_groups.get(&child.id) {
                absorb(Rect {
                    x: child_box.x,
                    y: child_box.y,
                    w: child_box.w,
                    h: child_box.h,
                    junction: false,
                });
            }
        }
    }

    if min_x == f32::MAX {
        return None;
    }

    let x = min_x - GROUP_PAD;
    let y = min_y - GROUP_LABEL_H - GROUP_PAD;
    Some(Rect {
        x,
        y,
        w: (max_x - min_x) + GROUP_PAD * 2.0,
        h: (max_y - y) + GROUP_PAD,
        junction: false,
    })
}

fn group_depth(group: &ArchitectureGroup, db: &ArchitectureDb) -> usize {
    let mut depth = 0;
    let mut parent = group.parent.as_deref();
    while let Some(parent_id) = parent {
        depth += 1;
        parent = db
            .get_groups()
            .into_iter()
            .find(|candidate| candidate.id == parent_id)
            .and_then(|candidate| candidate.parent.as_deref());
    }
    depth
}

fn convert_edge(
    edge: &ArchitectureEdge,
    rects: &HashMap<String, Rect>,
    overrides: &ManualLayoutOverrides,
) -> Option<ArchitectureEdgeLine> {
    let lhs = rects.get(&edge.lhs_id)?;
    let rhs = rects.get(&edge.rhs_id)?;
    let start_dir = convert_dir(edge.lhs_dir);
    let end_dir = convert_dir(edge.rhs_dir);
    let start = port(*lhs, start_dir);
    let end = port(*rhs, end_dir);
    let edge_override = overrides.edge(&edge.lhs_id, &edge.rhs_id);
    let points = if let Some(ov) = edge_override {
        if let Some(points) = &ov.points {
            points.clone()
        } else if ov.bend_points.is_empty() {
            edge_points(start, end, start_dir, end_dir)
        } else {
            let mut points = Vec::with_capacity(ov.bend_points.len() + 2);
            points.push(start);
            points.extend(ov.bend_points.iter().copied());
            points.push(end);
            points
        }
    } else {
        edge_points(start, end, start_dir, end_dir)
    };
    Some(ArchitectureEdgeLine {
        points,
        label: edge.title.clone().unwrap_or_default(),
        label_pos: edge_override.and_then(|ov| ov.label_pos),
        label_offset: edge_override.and_then(|ov| ov.label_offset),
        start_arrow: edge.lhs_into,
        end_arrow: edge.rhs_into,
    })
}

fn convert_dir(dir: SDirection) -> ArchitectureDirection {
    match dir {
        SDirection::Left => ArchitectureDirection::Left,
        SDirection::Right => ArchitectureDirection::Right,
        SDirection::Top => ArchitectureDirection::Top,
        SDirection::Bottom => ArchitectureDirection::Bottom,
    }
}

fn port(rect: Rect, dir: ArchitectureDirection) -> (f32, f32) {
    if rect.junction {
        return (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
    }
    match dir {
        ArchitectureDirection::Left => (rect.x, rect.y + rect.h / 2.0),
        ArchitectureDirection::Right => (rect.x + rect.w, rect.y + rect.h / 2.0),
        ArchitectureDirection::Top => (rect.x + rect.w / 2.0, rect.y),
        ArchitectureDirection::Bottom => (rect.x + rect.w / 2.0, rect.y + rect.h),
    }
}

fn edge_points(
    start: (f32, f32),
    end: (f32, f32),
    start_dir: ArchitectureDirection,
    end_dir: ArchitectureDirection,
) -> Vec<(f32, f32)> {
    let cross_axis = matches!(
        (start_dir, end_dir),
        (
            ArchitectureDirection::Left | ArchitectureDirection::Right,
            ArchitectureDirection::Top | ArchitectureDirection::Bottom
        ) | (
            ArchitectureDirection::Top | ArchitectureDirection::Bottom,
            ArchitectureDirection::Left | ArchitectureDirection::Right
        )
    );
    if cross_axis {
        let bend = if matches!(
            start_dir,
            ArchitectureDirection::Top | ArchitectureDirection::Bottom
        ) {
            (start.0, end.1)
        } else {
            (end.0, start.1)
        };
        vec![start, bend, end]
    } else {
        vec![start, end]
    }
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &ArchitectureGraph,
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

    for group in &graph.groups {
        draw_group(target, group, scale, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    for edge in &graph.edges {
        draw_edge(target, factory, edge, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    for service in &graph.services {
        draw_service(
            target, factory, service, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_group(
    target: &ID2D1RenderTarget,
    group: &ArchitectureGroupBox,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(group.x),
        top: ty(group.y),
        right: tx(group.x) + ts(group.w),
        bottom: ty(group.y) + ts(group.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: ts(8.0).max(4.0),
        radiusY: ts(8.0).max(4.0),
    };
    let fill = brush(theme::MERMAID_GROUP_FILL)?;
    let stroke = brush(theme::MERMAID_GROUP_STROKE)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.2 * scale).max(0.8),
        None::<&ID2D1StrokeStyle>,
    );
    let header = D2D_RECT_F {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: (rect.top + ts(GROUP_LABEL_H)).min(rect.bottom),
    };
    let header_fill = brush(0x202020)?;
    target.FillRectangle(std::ptr::addr_of!(header), &header_fill);

    draw_text(
        target,
        &group.title,
        D2D_RECT_F {
            left: header.left + ts(10.0),
            top: header.top,
            right: header.right - ts(10.0),
            bottom: header.bottom,
        },
        group_font(scale),
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
unsafe fn draw_service(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    service: &ArchitectureServiceBox,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if service.junction {
        let center = Vector2 {
            X: tx(service.x + service.w / 2.0),
            Y: ty(service.y + service.h / 2.0),
        };
        let dot = D2D1_ELLIPSE {
            point: center,
            radiusX: ts(service.w / 2.0).max(4.0),
            radiusY: ts(service.h / 2.0).max(4.0),
        };
        let fill = brush(theme::MERMAID_EDGE)?;
        target.FillEllipse(std::ptr::addr_of!(dot), &fill);
        return Ok(());
    }

    let rect = D2D_RECT_F {
        left: tx(service.x),
        top: ty(service.y),
        right: tx(service.x) + ts(service.w),
        bottom: ty(service.y) + ts(service.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: ts(8.0).max(4.0),
        radiusY: ts(8.0).max(4.0),
    };
    let fill = brush(theme::MERMAID_NODE_FILL)?;
    let stroke = brush(theme::MERMAID_NODE_STROKE)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.2 * scale).max(0.8),
        None::<&ID2D1StrokeStyle>,
    );

    let icon_rect = D2D_RECT_F {
        left: rect.left + (rect.right - rect.left - ts(ICON_SIZE)) / 2.0,
        top: rect.top + ts(9.0),
        right: rect.left + (rect.right - rect.left + ts(ICON_SIZE)) / 2.0,
        bottom: rect.top + ts(9.0 + ICON_SIZE),
    };
    let icon_name = if service.icon.is_empty() {
        service.icon_text.as_str()
    } else {
        service.icon.as_str()
    };
    draw_icon(target, factory, icon_rect, icon_name, scale, brush, fmt)?;

    let text_rect = D2D_RECT_F {
        left: rect.left + ts(9.0),
        top: icon_rect.bottom + ts(5.0),
        right: rect.right - ts(9.0),
        bottom: rect.bottom - ts(6.0),
    };
    draw_text(
        target,
        &service.title,
        text_rect,
        service_font(scale),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
        theme::MERMAID_NODE_TEXT,
        true,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_icon(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    rect: D2D_RECT_F,
    icon: &str,
    scale: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let stroke = brush(theme::MERMAID_NODE_STROKE)?;
    let fill = brush(0x1E2D36)?;

    if let Some(def) = icon_shape(icon) {
        let (x, y, w, h) = fitted_shape_rect(rect, def);
        let geo = crate::mermaid::render::build_custom_geometry_pub(factory, def, x, y, w, h)?;
        target.FillGeometry(&geo, &fill, None);
        target.DrawGeometry(
            &geo,
            &stroke,
            (1.1 * scale).max(0.8) * def.stroke_mult,
            None::<&ID2D1StrokeStyle>,
        );
        return Ok(());
    }

    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: (6.0 * scale).max(3.0),
        radiusY: (6.0 * scale).max(3.0),
    };
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.1 * scale).max(0.8),
        None::<&ID2D1StrokeStyle>,
    );
    let label = icon_label(icon);
    draw_text(
        target,
        &label,
        rect,
        icon_font(scale),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        theme::TEXT_BRIGHT,
        false,
        brush,
        fmt,
    )
}

fn icon_shape(icon: &str) -> Option<&'static crate::mermaid::shape_def::ShapeDef> {
    let registry = crate::mermaid::shape_def::registry();
    let icon = icon.trim().to_ascii_lowercase();
    if icon.is_empty() {
        return None;
    }

    for candidate in [
        icon.as_str(),
        icon.rsplit_once(':').map(|(_, name)| name).unwrap_or(""),
    ] {
        if candidate.is_empty() {
            continue;
        }
        if let Some(idx) = registry.lookup(candidate) {
            return registry.get(idx);
        }
    }
    None
}

fn fitted_shape_rect(
    rect: D2D_RECT_F,
    def: &crate::mermaid::shape_def::ShapeDef,
) -> (f32, f32, f32, f32) {
    let box_w = (rect.right - rect.left).max(0.0);
    let box_h = (rect.bottom - rect.top).max(0.0);
    let mut w = box_w;
    let mut h = box_h;

    if let Some(aspect) = def.aspect.filter(|aspect| *aspect > 0.0) {
        let box_aspect = if box_h > 0.0 { box_w / box_h } else { aspect };
        if box_aspect > aspect {
            w = h * aspect;
        } else {
            h = w / aspect;
        }
    }

    (
        rect.left + (box_w - w) * 0.5,
        rect.top + (box_h - h) * 0.5,
        w,
        h,
    )
}

fn icon_label(icon: &str) -> String {
    let mut label: String = icon
        .split(['-', '_', ':'])
        .filter_map(|part| part.chars().next())
        .take(4)
        .collect();
    if label.is_empty() {
        label = icon.chars().take(4).collect();
    }
    label.to_ascii_uppercase()
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_edge(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    edge: &ArchitectureEdgeLine,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if edge.points.len() < 2 {
        return Ok(());
    }
    let pts: Vec<(f32, f32)> = edge.points.iter().map(|(x, y)| (tx(*x), ty(*y))).collect();
    let line = brush(theme::MERMAID_EDGE)?;
    let line_w = (1.6 * scale).max(1.0);
    for segment in pts.windows(2) {
        target.DrawLine(
            Vector2 {
                X: segment[0].0,
                Y: segment[0].1,
            },
            Vector2 {
                X: segment[1].0,
                Y: segment[1].1,
            },
            &line,
            line_w,
            None::<&ID2D1StrokeStyle>,
        );
    }
    if edge.end_arrow {
        draw_arrow(
            target,
            factory,
            pts[pts.len() - 2],
            pts[pts.len() - 1],
            line_w,
            scale,
            &line,
        )?;
    }
    if edge.start_arrow {
        draw_arrow(target, factory, pts[1], pts[0], line_w, scale, &line)?;
    }
    let to_screen = |(x, y): (f32, f32)| (tx(x), ty(y));
    draw_edge_label(target, edge, &pts, scale, &to_screen, brush, fmt)
}

unsafe fn draw_arrow(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    from: (f32, f32),
    to: (f32, f32),
    line_w: f32,
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
    let left = (back.0 + px * half, back.1 + py * half);
    let right = (back.0 - px * half, back.1 - py * half);
    let geo = crate::mermaid::render::build_polygon_pub(factory, &[to, left, right])?;
    target.FillGeometry(&geo, brush, None);
    target.DrawGeometry(&geo, brush, line_w, None::<&ID2D1StrokeStyle>);
    Ok(())
}

unsafe fn draw_edge_label(
    target: &ID2D1RenderTarget,
    edge: &ArchitectureEdgeLine,
    pts: &[(f32, f32)],
    scale: f32,
    to_screen: &impl Fn((f32, f32)) -> (f32, f32),
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if edge.label.is_empty() {
        return Ok(());
    }
    let (cx, cy) = if let Some(pos) = edge.label_pos {
        to_screen(pos)
    } else {
        let (mx, my) = midpoint(pts);
        let (dx, dy) = edge.label_offset.unwrap_or((0.0, 0.0));
        (mx + dx * scale, my + dy * scale)
    };
    let font = edge_font(scale);
    let w = (edge.label.chars().count() as f32 * font * 0.58).max(42.0) + 12.0 * scale;
    let h = font * 1.45 + 7.0 * scale;
    let rect = D2D_RECT_F {
        left: cx - w / 2.0,
        top: cy - h / 2.0,
        right: cx + w / 2.0,
        bottom: cy + h / 2.0,
    };
    let bg = brush(theme::BG)?;
    let stroke = brush(theme::BORDER)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        (0.8 * scale).max(0.6),
        None::<&ID2D1StrokeStyle>,
    );
    draw_text(
        target,
        &edge.label,
        rect,
        font,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        theme::MERMAID_EDGE_LABEL,
        false,
        brush,
        fmt,
    )
}

fn midpoint(pts: &[(f32, f32)]) -> (f32, f32) {
    if pts.len() < 2 {
        return pts.first().copied().unwrap_or((0.0, 0.0));
    }
    let mid = pts.len() / 2;
    if pts.len().is_multiple_of(2) && mid > 0 {
        (
            (pts[mid - 1].0 + pts[mid].0) * 0.5,
            (pts[mid - 1].1 + pts[mid].1) * 0.5,
        )
    } else {
        pts[mid]
    }
}

fn title_font(scale: f32) -> f32 {
    (TITLE_FONT * scale).max(10.0)
}

fn group_font(scale: f32) -> f32 {
    (GROUP_FONT * scale).max(6.8)
}

fn service_font(scale: f32) -> f32 {
    (SERVICE_FONT * scale).max(6.0)
}

fn icon_font(scale: f32) -> f32 {
    (ICON_FONT * scale).max(6.2)
}

fn edge_font(scale: f32) -> f32 {
    (EDGE_FONT * scale).max(6.5)
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
    fn builds_service_map() {
        let source = r#"architecture-beta
title DocCrate Render Path
group app(cloud)[DocCrate App]
service docs(disk)[Markdown Docs] in app
service parser(server)[Parser] in app
service layout(server)[Layout Engine] in app
service d2d(server)[Direct2D Renderer] in app
docs:R --> L:parser
parser:R --> L:layout
layout:R --> L:d2d
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Architecture(db) = diagram else {
            panic!("expected architecture");
        };
        let graph = build(&db).unwrap();
        assert_eq!(graph.services.len(), 4);
        assert_eq!(graph.groups.len(), 1);
        assert_eq!(graph.edges.len(), 3);
        assert!(graph.width > 300.0);
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let source = r#"architecture-beta
title Manual Layout
group runtime(service)[Runtime]
service gateway(gateway)[Gateway] in runtime
service api(api)[API] in runtime
service db(database)[Database] in runtime
gateway:R --> L:api
api:R --> L:db
%% @service gateway x=40 y=70 w=120 h=96
%% @service api x=230 y=80 w=132 h=104
%% @service db x=430 y=74 w=126 h=110
%% @group runtime x=24 y=28 w=560 h=210
%% @edge gateway->api points="160,118 195,118 195,132 230,132"
%% @edge api->db bend_points="390,132 390,129"
%% @graph w=620 h=280
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Architecture(db) = diagram else {
            panic!("expected architecture");
        };
        let overrides = crate::mermaid::manual_layout::parse(source).unwrap();
        let graph = build_with_overrides(&db, &overrides).unwrap();

        let gateway = graph
            .services
            .iter()
            .find(|service| service.title == "Gateway")
            .unwrap();
        assert_eq!(
            (gateway.x, gateway.y, gateway.w, gateway.h),
            (40.0, 70.0, 120.0, 96.0)
        );

        let runtime = graph
            .groups
            .iter()
            .find(|group| group.title == "Runtime")
            .unwrap();
        assert_eq!(
            (runtime.x, runtime.y, runtime.w, runtime.h),
            (24.0, 28.0, 560.0, 210.0)
        );

        assert_eq!(
            graph.edges[0].points,
            vec![
                (160.0, 118.0),
                (195.0, 118.0),
                (195.0, 132.0),
                (230.0, 132.0)
            ]
        );
        assert_eq!(graph.edges[1].points.len(), 4);
        assert_eq!((graph.width, graph.height), (620.0, 280.0));
    }
}
