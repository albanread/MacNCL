//! Class diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::class::{
    ClassDb, ClassNode as SClassNode, Classifier, LineType as SLineType,
    RelationType as SRelationType,
};
use selkie::layout::{
    self, CharacterSizeEstimator, LayoutEdge, LayoutGraph, LayoutNode, ToLayoutGraph,
};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const CLASS_PAD: f32 = 24.0;
const CLASS_HEADER_H: f32 = 48.0;
const CLASS_ANNOTATION_H: f32 = 20.0;
const CLASS_ROW_H: f32 = 24.0;
const CLASS_TITLE_SIZE: f32 = 13.0;
const CLASS_MEMBER_SIZE: f32 = 12.0;
const CLASS_NOTE_W: f32 = 150.0;
const CLASS_NOTE_MIN_H: f32 = 48.0;

pub fn build_with_overrides(
    db: &ClassDb,
    overrides: &ManualLayoutOverrides,
) -> std::result::Result<ClassGraph, String> {
    let estimator = CharacterSizeEstimator::default();
    let lg = db
        .to_layout_graph(&estimator)
        .map_err(|e| format!("layout-graph build: {e}"))?;
    let lg = layout::layout(lg).map_err(|e| format!("layout: {e}"))?;
    Ok(convert(db, &lg, overrides))
}

fn convert(db: &ClassDb, lg: &LayoutGraph, overrides: &ManualLayoutOverrides) -> ClassGraph {
    let bx = lg.bounds_x.unwrap_or(0.0) as f32;
    let by = lg.bounds_y.unwrap_or(0.0) as f32;
    let mut width = lg.width.unwrap_or(0.0) as f32;
    let mut height = lg.height.unwrap_or(0.0) as f32;

    let mut nodes = Vec::new();
    let mut class_ids: Vec<&String> = db.classes.keys().collect();
    class_ids.sort();
    for id in class_ids {
        let Some(class) = db.classes.get(id) else {
            continue;
        };
        let Some(node) = lg.get_node(id) else {
            continue;
        };
        let (x, y) = node_origin(node, bx, by);
        let (fill, stroke, text_color) = class_colors(db, class);
        let title = class_title(class);

        nodes.push(ClassBox {
            id: id.clone(),
            namespace: class.parent.clone(),
            x,
            y,
            w: node.width as f32,
            h: node.height as f32,
            title,
            annotations: class.annotations.clone(),
            members: class.members.iter().map(convert_member).collect(),
            methods: class.methods.iter().map(convert_member).collect(),
            fill,
            stroke,
            text_color,
        });
    }
    apply_class_overrides(&mut nodes, overrides);

    let node_lookup: HashMap<&str, &ClassBox> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut edges = Vec::new();
    for (idx, relation) in db.relations.iter().enumerate() {
        let edge_id = format!("rel-{}-{}-{}", relation.id1, relation.id2, idx);
        let edge_override = overrides.edge(&relation.id1, &relation.id2);
        let auto_points = if class_was_overridden(overrides, &relation.id1)
            || class_was_overridden(overrides, &relation.id2)
            || edge_override.is_some_and(|ov| !ov.bend_points.is_empty())
        {
            direct_edge_points(&node_lookup, &relation.id1, &relation.id2)
        } else {
            lg.edges
                .iter()
                .find(|edge| edge.id == edge_id)
                .and_then(|edge| edge_points(edge, bx, by))
                .or_else(|| direct_edge_points(&node_lookup, &relation.id1, &relation.id2))
        };
        let points = if let Some(ov) = edge_override {
            if let Some(points) = &ov.points {
                Some(points.clone())
            } else if ov.bend_points.is_empty() {
                auto_points
            } else {
                auto_points.map(|auto| {
                    let mut points = Vec::with_capacity(ov.bend_points.len() + 2);
                    points.push(auto[0]);
                    points.extend(ov.bend_points.iter().copied());
                    points.push(*auto.last().unwrap_or(&auto[0]));
                    points
                })
            }
        } else {
            auto_points
        };

        let Some(points) = points else {
            continue;
        };
        edges.push(ClassEdge {
            points,
            line_style: match relation.relation.line_type {
                SLineType::Solid => LineStyle::Solid,
                SLineType::Dotted => LineStyle::Dash,
            },
            start_marker: convert_marker(relation.relation.type1),
            end_marker: convert_marker(relation.relation.type2),
            label: relation.title.clone(),
            label_pos: edge_override.and_then(|ov| ov.label_pos),
            label_offset: edge_override.and_then(|ov| ov.label_offset),
            card_start: relation.relation_title1.clone(),
            card_end: relation.relation_title2.clone(),
            color: theme::MERMAID_EDGE,
        });
    }

    let mut groups = build_namespace_groups(&nodes);
    apply_group_overrides(&mut groups, overrides);

    let mut notes = Vec::new();
    let mut note_values: Vec<_> = db.notes.values().collect();
    note_values.sort_by_key(|note| note.index);
    for note in note_values {
        let Some(class) = node_lookup.get(note.class.as_str()) else {
            continue;
        };
        let line_count = note.text.lines().count().max(1) as f32;
        let h = CLASS_NOTE_MIN_H.max(20.0 + line_count * 15.0);
        let mut n = ClassNoteBox {
            id: note.id.clone(),
            class: note.class.clone(),
            x: class.x + class.w + 18.0,
            y: class.y,
            w: CLASS_NOTE_W,
            h,
            text: note.text.clone(),
        };
        apply_note_override(&mut n, overrides);
        notes.push(n);
    }

    grow_bounds(&mut width, &mut height, &groups, &nodes, &edges, &notes);
    if let Some(w) = overrides.graph.w {
        width = w;
    }
    if let Some(h) = overrides.graph.h {
        height = h;
    }

    ClassGraph {
        width: width.max(1.0),
        height: height.max(1.0),
        groups,
        nodes,
        edges,
        notes,
    }
}

fn apply_class_overrides(nodes: &mut [ClassBox], overrides: &ManualLayoutOverrides) {
    for node in nodes {
        if let Some(ov) = overrides.object(&node.id) {
            apply_box_override(&mut node.x, &mut node.y, &mut node.w, &mut node.h, ov);
        }
    }
}

fn apply_group_overrides(groups: &mut [ClassGroup], overrides: &ManualLayoutOverrides) {
    for group in groups {
        if let Some(ov) = overrides.group(&group.title) {
            apply_box_override(&mut group.x, &mut group.y, &mut group.w, &mut group.h, ov);
        }
    }
}

fn apply_note_override(note: &mut ClassNoteBox, overrides: &ManualLayoutOverrides) {
    let note_key = format!("note:{}", note.class);
    let override_for_note = overrides
        .object(&note.id)
        .or_else(|| overrides.object(&note_key));
    if let Some(ov) = override_for_note {
        apply_box_override(&mut note.x, &mut note.y, &mut note.w, &mut note.h, ov);
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

fn class_was_overridden(overrides: &ManualLayoutOverrides, id: &str) -> bool {
    overrides.object(id).is_some()
}

fn grow_bounds(
    width: &mut f32,
    height: &mut f32,
    groups: &[ClassGroup],
    nodes: &[ClassBox],
    edges: &[ClassEdge],
    notes: &[ClassNoteBox],
) {
    for g in groups {
        *width = (*width).max(g.x + g.w);
        *height = (*height).max(g.y + g.h);
    }
    for node in nodes {
        *width = (*width).max(node.x + node.w);
        *height = (*height).max(node.y + node.h);
    }
    for edge in edges {
        for (x, y) in &edge.points {
            *width = (*width).max(*x);
            *height = (*height).max(*y);
        }
        if let Some((x, y)) = edge.label_pos {
            *width = (*width).max(x);
            *height = (*height).max(y);
        }
    }
    for note in notes {
        *width = (*width).max(note.x + note.w);
        *height = (*height).max(note.y + note.h);
    }
}

fn node_origin(node: &LayoutNode, bx: f32, by: f32) -> (f32, f32) {
    let x = node.x.unwrap_or(0.0) as f32 - bx;
    let y = node.y.unwrap_or(0.0) as f32 - by;
    (x, y)
}

fn edge_points(edge: &LayoutEdge, bx: f32, by: f32) -> Option<Vec<(f32, f32)>> {
    if edge.bend_points.len() < 2 {
        return None;
    }
    Some(
        edge.bend_points
            .iter()
            .map(|p| (p.x as f32 - bx, p.y as f32 - by))
            .collect(),
    )
}

fn direct_edge_points(
    nodes: &HashMap<&str, &ClassBox>,
    start: &str,
    end: &str,
) -> Option<Vec<(f32, f32)>> {
    let a = *nodes.get(start)?;
    let b = *nodes.get(end)?;
    let (sx, sy, ex, ey) = connection_points(a, b);
    Some(vec![(sx, sy), (ex, ey)])
}

fn connection_points(a: &ClassBox, b: &ClassBox) -> (f32, f32, f32, f32) {
    let acx = a.x + a.w / 2.0;
    let acy = a.y + a.h / 2.0;
    let bcx = b.x + b.w / 2.0;
    let bcy = b.y + b.h / 2.0;
    let dx = bcx - acx;
    let dy = bcy - acy;
    if dx.abs() > dy.abs() {
        if dx > 0.0 {
            (a.x + a.w, acy, b.x, bcy)
        } else {
            (a.x, acy, b.x + b.w, bcy)
        }
    } else if dy > 0.0 {
        (acx, a.y + a.h, bcx, b.y)
    } else {
        (acx, a.y, bcx, b.y + b.h)
    }
}

fn class_title(class: &SClassNode) -> String {
    let label = if class.label.is_empty() {
        class.id.as_str()
    } else {
        class.label.as_str()
    };
    if class.type_param.is_empty() {
        label.to_string()
    } else {
        format!("{label}<{}>", class.type_param)
    }
}

fn convert_member(member: &selkie::diagrams::class::ClassMember) -> ClassMemberLine {
    let details = member.get_display_details();
    ClassMemberLine {
        text: details.display_text,
        italic: member.classifier == Classifier::Abstract,
        underline: member.classifier == Classifier::Static,
    }
}

fn convert_marker(value: i32) -> ClassMarker {
    if value == SRelationType::Aggregation as i32 {
        ClassMarker::Aggregation
    } else if value == SRelationType::Extension as i32 {
        ClassMarker::Extension
    } else if value == SRelationType::Composition as i32 {
        ClassMarker::Composition
    } else if value == SRelationType::Dependency as i32 {
        ClassMarker::Dependency
    } else if value == SRelationType::Lollipop as i32 {
        ClassMarker::Lollipop
    } else {
        ClassMarker::None
    }
}

fn class_colors(db: &ClassDb, class: &SClassNode) -> (u32, u32, u32) {
    let mut fill = theme::MERMAID_NODE_FILL;
    let mut stroke = theme::MERMAID_NODE_STROKE;
    let mut text = theme::MERMAID_NODE_TEXT;

    for class_name in class.css_classes.split_whitespace() {
        if class_name == "default" {
            continue;
        }
        if let Some(def) = db.style_classes.get(class_name) {
            apply_style_list(&def.styles, &mut fill, &mut stroke, &mut text);
            apply_style_list(&def.text_styles, &mut fill, &mut stroke, &mut text);
        }
    }
    apply_style_list(&class.styles, &mut fill, &mut stroke, &mut text);
    (fill, stroke, text)
}

fn apply_style_list(styles: &[String], fill: &mut u32, stroke: &mut u32, text: &mut u32) {
    for item in styles {
        for decl in item.split([';', ',']) {
            let Some((key, value)) = decl.split_once(':') else {
                continue;
            };
            let Some(color) = parse_hex(value.trim()) else {
                continue;
            };
            match key.trim() {
                "fill" => *fill = color,
                "stroke" => *stroke = color,
                "color" => *text = color,
                _ => {}
            }
        }
    }
}

fn parse_hex(s: &str) -> Option<u32> {
    let raw = s.trim().trim_start_matches('#');
    let v = match raw.len() {
        3 => {
            let r = u32::from_str_radix(&raw[0..1], 16).ok()?;
            let g = u32::from_str_radix(&raw[1..2], 16).ok()?;
            let b = u32::from_str_radix(&raw[2..3], 16).ok()?;
            ((r * 0x11) << 16) | ((g * 0x11) << 8) | (b * 0x11)
        }
        6 => u32::from_str_radix(raw, 16).ok()?,
        _ => return None,
    };
    Some(v & 0x00FF_FFFF)
}

fn build_namespace_groups(nodes: &[ClassBox]) -> Vec<ClassGroup> {
    #[derive(Clone, Copy)]
    struct Bounds {
        min_x: f32,
        min_y: f32,
        max_x: f32,
        max_y: f32,
    }

    let mut bounds: HashMap<String, Bounds> = HashMap::new();
    for node in nodes {
        if node.id.is_empty() {
            continue;
        }
        let ns = node
            .namespace
            .as_deref()
            .or_else(|| node.id.rsplit_once('.').map(|(ns, _)| ns));
        let Some(ns) = ns else {
            continue;
        };
        bounds
            .entry(ns.to_string())
            .and_modify(|b| {
                b.min_x = b.min_x.min(node.x);
                b.min_y = b.min_y.min(node.y);
                b.max_x = b.max_x.max(node.x + node.w);
                b.max_y = b.max_y.max(node.y + node.h);
            })
            .or_insert(Bounds {
                min_x: node.x,
                min_y: node.y,
                max_x: node.x + node.w,
                max_y: node.y + node.h,
            });
    }

    let mut groups: Vec<_> = bounds
        .into_iter()
        .map(|(title, b)| ClassGroup {
            x: b.min_x - 18.0,
            y: b.min_y - 34.0,
            w: (b.max_x - b.min_x) + 36.0,
            h: (b.max_y - b.min_y) + 52.0,
            title,
        })
        .collect();
    groups.sort_by(|a, b| a.title.cmp(&b.title));
    groups
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &ClassGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale;

    for group in &graph.groups {
        draw_group(target, group, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    for edge in &graph.edges {
        draw_edge(target, factory, edge, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    for node in &graph.nodes {
        draw_class_box(target, node, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    for note in &graph.notes {
        draw_note(target, factory, note, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    Ok(())
}

unsafe fn draw_group(
    target: &ID2D1RenderTarget,
    group: &ClassGroup,
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
        radiusX: 6.0,
        radiusY: 6.0,
    };
    let fill = brush(theme::MERMAID_GROUP_FILL)?;
    let stroke = brush(theme::MERMAID_GROUP_STROKE)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        theme::MERMAID_GROUP_STROKE_W,
        None::<&ID2D1StrokeStyle>,
    );

    let title_rect = D2D_RECT_F {
        left: rect.left + 8.0,
        top: rect.top + 4.0,
        right: rect.right - 8.0,
        bottom: rect.top + 24.0,
    };
    draw_text(
        target,
        &group.title,
        title_rect,
        ts(theme::MERMAID_GROUP_FONT_SIZE),
        true,
        false,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        theme::MERMAID_GROUP_TITLE,
        brush,
        fmt,
    )
}

unsafe fn draw_class_box(
    target: &ID2D1RenderTarget,
    node: &ClassBox,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(node.x);
    let y = ty(node.y);
    let w = ts(node.w);
    let h = ts(node.h);
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 4.0,
        radiusY: 4.0,
    };
    let fill = brush(node.fill)?;
    let stroke = brush(node.stroke)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        theme::MERMAID_NODE_STROKE_W,
        None::<&ID2D1StrokeStyle>,
    );

    let has_members = !node.members.is_empty();
    let has_methods = !node.methods.is_empty();
    let has_body = has_members || has_methods;
    let header_h = if has_body {
        ts(CLASS_HEADER_H + node.annotations.len() as f32 * CLASS_ANNOTATION_H).min(h)
    } else {
        h
    };
    let divider1 = (y + header_h).min(y + h);

    if has_body {
        target.DrawLine(
            Vector2 { X: x, Y: divider1 },
            Vector2 {
                X: x + w,
                Y: divider1,
            },
            &stroke,
            1.0,
            None::<&ID2D1StrokeStyle>,
        );
    }

    let divider2 = if has_members && has_methods {
        let attr_h = ts(node.members.len() as f32 * CLASS_ROW_H + CLASS_PAD);
        let divider2 = (divider1 + attr_h).min(y + h);
        target.DrawLine(
            Vector2 { X: x, Y: divider2 },
            Vector2 {
                X: x + w,
                Y: divider2,
            },
            &stroke,
            1.0,
            None::<&ID2D1StrokeStyle>,
        );
        divider2
    } else {
        divider1
    };

    let header_lines = node.annotations.len() + 1;
    let line_gap = header_h / (header_lines as f32 + 1.0);
    let mut text_y = y + line_gap;
    for annotation in &node.annotations {
        let text = format!("\u{00AB}{annotation}\u{00BB}");
        let rect = D2D_RECT_F {
            left: x + 6.0,
            top: text_y - 10.0,
            right: x + w - 6.0,
            bottom: text_y + 10.0,
        };
        draw_text(
            target,
            &text,
            rect,
            ts(CLASS_TITLE_SIZE),
            false,
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            node.text_color,
            brush,
            fmt,
        )?;
        text_y += line_gap;
    }

    let title_rect = D2D_RECT_F {
        left: x + 6.0,
        top: text_y - 11.0,
        right: x + w - 6.0,
        bottom: text_y + 11.0,
    };
    draw_text(
        target,
        &node.title,
        title_rect,
        ts(CLASS_TITLE_SIZE),
        true,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        node.text_color,
        brush,
        fmt,
    )?;

    let left = x + ts(CLASS_PAD * 0.55);
    let row_h = ts(CLASS_ROW_H);
    let mut row_y = divider1;
    for member in &node.members {
        row_y += row_h / 2.0;
        draw_member_line(
            target,
            member,
            left,
            row_y,
            x + w - 8.0,
            ts(CLASS_MEMBER_SIZE),
            node.text_color,
            brush,
            fmt,
        )?;
        row_y += row_h / 2.0;
    }

    row_y = divider2;
    for method in &node.methods {
        row_y += row_h / 2.0;
        draw_member_line(
            target,
            method,
            left,
            row_y,
            x + w - 8.0,
            ts(CLASS_MEMBER_SIZE),
            node.text_color,
            brush,
            fmt,
        )?;
        row_y += row_h / 2.0;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_member_line(
    target: &ID2D1RenderTarget,
    member: &ClassMemberLine,
    left: f32,
    center_y: f32,
    right: f32,
    font_size: f32,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left,
        top: center_y - 9.0,
        right,
        bottom: center_y + 9.0,
    };
    draw_text(
        target,
        &member.text,
        rect,
        font_size,
        false,
        member.italic,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        color,
        brush,
        fmt,
    )?;
    if member.underline {
        let br = brush(color)?;
        let width =
            (member.text.chars().count() as f32 * font_size * 0.52).min((right - left).max(0.0));
        target.DrawLine(
            Vector2 {
                X: left,
                Y: center_y + 7.0,
            },
            Vector2 {
                X: left + width,
                Y: center_y + 7.0,
            },
            &br,
            1.0,
            None::<&ID2D1StrokeStyle>,
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_edge(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    edge: &ClassEdge,
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
    let line_br = brush(edge.color)?;
    let line_w = (theme::MERMAID_EDGE_W * scale).max(0.75);
    let style = match edge.line_style {
        LineStyle::Solid => None,
        LineStyle::Dash => Some(crate::mermaid::render::sequence_dash_style(factory)),
        LineStyle::Dot => Some(crate::mermaid::render::sequence_dot_style(factory)),
    };
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
            &line_br,
            line_w,
            style,
        );
    }

    if !matches!(edge.end_marker, ClassMarker::None) {
        let (from, to) = (pts[pts.len() - 2], pts[pts.len() - 1]);
        draw_marker(target, factory, from, to, edge.end_marker, line_w, brush)?;
    }
    if !matches!(edge.start_marker, ClassMarker::None) {
        let (from, to) = (pts[1], pts[0]);
        draw_marker(target, factory, from, to, edge.start_marker, line_w, brush)?;
    }

    if !edge.label.is_empty() {
        let (x, y) = if let Some((x, y)) = edge.label_pos {
            (tx(x), ty(y))
        } else {
            let (mx, my) = polyline_midpoint(&pts);
            let (dx, dy) = edge.label_offset.unwrap_or((0.0, 0.0));
            (mx + dx * scale, my + dy * scale)
        };
        draw_center_label(
            target,
            &edge.label,
            x,
            y,
            theme::MERMAID_EDGE_FONT_SIZE * scale,
            brush,
            fmt,
        )?;
    }
    if !edge.card_start.is_empty() {
        let (x, y) = endpoint_label_pos(&pts, true);
        draw_center_label(target, &edge.card_start, x, y, 11.0 * scale, brush, fmt)?;
    }
    if !edge.card_end.is_empty() {
        let (x, y) = endpoint_label_pos(&pts, false);
        draw_center_label(target, &edge.card_end, x, y, 11.0 * scale, brush, fmt)?;
    }
    Ok(())
}

unsafe fn draw_marker(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    from: (f32, f32),
    to: (f32, f32),
    marker: ClassMarker,
    line_w: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let size = (line_w * 6.0).max(9.0);
    let stroke = brush(theme::MERMAID_EDGE)?;
    let bg = brush(theme::BG)?;

    match marker {
        ClassMarker::None => {}
        ClassMarker::Dependency => {
            let back = (to.0 - ux * size, to.1 - uy * size);
            let half = size * 0.55;
            let left = (back.0 + px * half, back.1 + py * half);
            let right = (back.0 - px * half, back.1 - py * half);
            target.DrawLine(
                Vector2 { X: to.0, Y: to.1 },
                Vector2 {
                    X: left.0,
                    Y: left.1,
                },
                &stroke,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
            target.DrawLine(
                Vector2 { X: to.0, Y: to.1 },
                Vector2 {
                    X: right.0,
                    Y: right.1,
                },
                &stroke,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
        ClassMarker::Extension => {
            let back = (to.0 - ux * size, to.1 - uy * size);
            let half = size * 0.62;
            let left = (back.0 + px * half, back.1 + py * half);
            let right = (back.0 - px * half, back.1 - py * half);
            let geo = crate::mermaid::render::build_polygon_pub(factory, &[to, left, right])?;
            target.FillGeometry(&geo, &bg, None);
            target.DrawGeometry(&geo, &stroke, line_w, None::<&ID2D1StrokeStyle>);
        }
        ClassMarker::Aggregation | ClassMarker::Composition => {
            let mid = (to.0 - ux * size * 0.75, to.1 - uy * size * 0.75);
            let back = (to.0 - ux * size * 1.5, to.1 - uy * size * 1.5);
            let half = size * 0.55;
            let left = (mid.0 + px * half, mid.1 + py * half);
            let right = (mid.0 - px * half, mid.1 - py * half);
            let geo = crate::mermaid::render::build_polygon_pub(factory, &[to, left, back, right])?;
            let fill = if matches!(marker, ClassMarker::Composition) {
                &stroke
            } else {
                &bg
            };
            target.FillGeometry(&geo, fill, None);
            target.DrawGeometry(&geo, &stroke, line_w, None::<&ID2D1StrokeStyle>);
        }
        ClassMarker::Lollipop => {
            let radius = size * 0.48;
            let center = (to.0 - ux * radius, to.1 - uy * radius);
            let ellipse = D2D1_ELLIPSE {
                point: Vector2 {
                    X: center.0,
                    Y: center.1,
                },
                radiusX: radius,
                radiusY: radius,
            };
            target.FillEllipse(std::ptr::addr_of!(ellipse), &bg);
            target.DrawEllipse(
                std::ptr::addr_of!(ellipse),
                &stroke,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }
    Ok(())
}

fn polyline_midpoint(points: &[(f32, f32)]) -> (f32, f32) {
    let mut total = 0.0;
    for pair in points.windows(2) {
        total += distance(pair[0], pair[1]);
    }
    if total <= 0.0 {
        return points[0];
    }
    let mut walked = 0.0;
    let target = total / 2.0;
    for pair in points.windows(2) {
        let len = distance(pair[0], pair[1]);
        if walked + len >= target {
            let t = (target - walked) / len.max(0.0001);
            return (
                pair[0].0 + (pair[1].0 - pair[0].0) * t,
                pair[0].1 + (pair[1].1 - pair[0].1) * t,
            );
        }
        walked += len;
    }
    *points.last().unwrap_or(&points[0])
}

fn endpoint_label_pos(points: &[(f32, f32)], start: bool) -> (f32, f32) {
    let (a, b) = if start {
        (points[0], points[1])
    } else {
        (points[points.len() - 1], points[points.len() - 2])
    };
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    (a.0 + ux * 22.0 + px * 12.0, a.1 + uy * 22.0 + py * 12.0)
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    (dx * dx + dy * dy).sqrt()
}

unsafe fn draw_center_label(
    target: &ID2D1RenderTarget,
    text: &str,
    cx: f32,
    cy: f32,
    size: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let w = (text.chars().count() as f32 * size * 0.58).max(24.0) + 8.0;
    let h = size + 8.0;
    let bg = brush(theme::BG)?;
    let rect = D2D_RECT_F {
        left: cx - w / 2.0,
        top: cy - h / 2.0,
        right: cx + w / 2.0,
        bottom: cy + h / 2.0,
    };
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);
    draw_text(
        target,
        text,
        rect,
        size,
        false,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        theme::MERMAID_EDGE_LABEL,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_note(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    note: &ClassNoteBox,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(note.x);
    let y = ty(note.y);
    let w = ts(note.w);
    let h = ts(note.h);
    let fold = 10.0;
    let fill = brush(0x3A3320)?;
    let stroke = brush(theme::MERMAID_GROUP_TITLE)?;
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    target.FillRectangle(std::ptr::addr_of!(rect), &fill);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        1.0,
        None::<&ID2D1StrokeStyle>,
    );
    let fold_geo = crate::mermaid::render::build_polygon_pub(
        factory,
        &[
            (x + w - fold, y),
            (x + w, y + fold),
            (x + w - fold, y + fold),
        ],
    )?;
    let fold_br = brush(0x51482C)?;
    target.FillGeometry(&fold_geo, &fold_br, None);
    target.DrawGeometry(&fold_geo, &stroke, 1.0, None::<&ID2D1StrokeStyle>);

    let text_rect = D2D_RECT_F {
        left: x + 8.0,
        top: y + 8.0,
        right: x + w - 8.0,
        bottom: y + h - 8.0,
    };
    draw_text(
        target,
        &note.text,
        text_rect,
        ts(11.0),
        false,
        false,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        theme::TEXT,
        brush,
        fmt,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    text: &str,
    rect: D2D_RECT_F,
    size: f32,
    bold: bool,
    italic: bool,
    align: DWRITE_TEXT_ALIGNMENT,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let f = fmt(theme::BODY_FONT, size, bold, italic)?;
    let _ = f.SetTextAlignment(align);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_class_graph_with_members_and_markers() {
        let src = r#"classDiagram
    class Animal {
      <<interface>>
      +String name
      +eat()*
    }
    Animal <|-- Duck : implements
    Duck : +quack()$
"#;
        let graph = match crate::mermaid::build(src).unwrap() {
            Graph::Class(g) => g,
            other => panic!("expected class graph, got {other:?}"),
        };
        assert_eq!(graph.nodes.len(), 2);
        let animal = graph.nodes.iter().find(|n| n.id == "Animal").unwrap();
        assert_eq!(animal.annotations, vec!["interface"]);
        assert_eq!(animal.members.len(), 1);
        assert_eq!(animal.methods.len(), 1);
        assert!(animal.methods[0].italic);
        let duck = graph.nodes.iter().find(|n| n.id == "Duck").unwrap();
        assert!(duck.methods[0].underline);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].start_marker, ClassMarker::Extension);
        assert_eq!(graph.edges[0].label, "implements");
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let src = r#"classDiagram
namespace Core {
  class DocumentStore {
    <<interface>>
    +load(path)
  }
  class MarkdownParser {
    +parse(doc)
  }
}
DocumentStore <|.. MarkdownParser : consumes
note for MarkdownParser "Hot path: keep parse allocation-free"
%% @node DocumentStore x=40 y=82 w=190 h=128
%% @node MarkdownParser x=330 y=82 w=210 h=128
%% @node note:MarkdownParser x=345 y=238 w=210 h=60
%% @group Core x=20 y=36 w=555 h=290
%% @edge DocumentStore->MarkdownParser bend_points="245,146" label_pos="285,116"
%% @graph w=620 h=350
"#;
        let diagram = selkie::parse(src).unwrap();
        let selkie::diagrams::Diagram::Class(db) = diagram else {
            panic!("expected class diagram");
        };
        let overrides = crate::mermaid::manual_layout::parse(src).unwrap();
        let graph = build_with_overrides(&db, &overrides).unwrap();

        let store = graph
            .nodes
            .iter()
            .find(|n| n.id == "DocumentStore")
            .unwrap();
        let parser = graph
            .nodes
            .iter()
            .find(|n| n.id == "MarkdownParser")
            .unwrap();
        let core = graph.groups.iter().find(|g| g.title == "Core").unwrap();
        let note = graph
            .notes
            .iter()
            .find(|n| n.class == "MarkdownParser")
            .unwrap();
        assert_eq!(
            (store.x, store.y, store.w, store.h),
            (40.0, 82.0, 190.0, 128.0)
        );
        assert_eq!(
            (parser.x, parser.y, parser.w, parser.h),
            (330.0, 82.0, 210.0, 128.0)
        );
        assert_eq!((core.x, core.y, core.w, core.h), (20.0, 36.0, 555.0, 290.0));
        assert_eq!(
            (note.x, note.y, note.w, note.h),
            (345.0, 238.0, 210.0, 60.0)
        );
        assert_eq!(graph.edges[0].points[1], (245.0, 146.0));
        assert_eq!(graph.edges[0].label_pos, Some((285.0, 116.0)));
        assert_eq!((graph.width, graph.height), (620.0, 350.0));
    }
}
