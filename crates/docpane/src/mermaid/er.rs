//! ER diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::er::{
    AttributeKey, Cardinality as SCardinality, Entity, ErDb, Identification,
};
use selkie::layout::{
    self, CharacterSizeEstimator, LayoutEdge, LayoutGraph, LayoutNode, Point, ToLayoutGraph,
};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const ER_MARGIN: f32 = 24.0;
const HEADER_H: f32 = 42.75;
const ROW_H: f32 = 42.75;
const ATTR_FONT_SIZE: f32 = 12.0;
const TITLE_FONT_SIZE: f32 = 14.0;
const TEXT_W: f32 = 0.65;

#[derive(Debug, Clone)]
struct EntityDimensions {
    width: f32,
    height: f32,
    col_widths: [f32; 3],
}

pub fn build_with_overrides(
    db: &ErDb,
    overrides: &ManualLayoutOverrides,
) -> std::result::Result<ErGraph, String> {
    let estimator = CharacterSizeEstimator::default();
    let lg = db
        .to_layout_graph(&estimator)
        .map_err(|e| format!("layout-graph build: {e}"))?;
    let lg = layout::layout(lg).map_err(|e| format!("layout: {e}"))?;
    Ok(convert(db, &lg, overrides))
}

fn convert(db: &ErDb, lg: &LayoutGraph, overrides: &ManualLayoutOverrides) -> ErGraph {
    let bx = lg.bounds_x.unwrap_or(0.0) as f32;
    let by = lg.bounds_y.unwrap_or(0.0) as f32;
    let mut width = lg.width.unwrap_or(0.0) as f32 + ER_MARGIN * 2.0;
    let mut height = lg.height.unwrap_or(0.0) as f32 + ER_MARGIN * 2.0;

    let entities = db.get_entities();
    let id_to_name: HashMap<String, String> = entities
        .iter()
        .map(|(name, entity)| (entity.id.clone(), name.clone()))
        .collect();

    let mut dimensions: HashMap<String, EntityDimensions> = HashMap::new();
    for (name, entity) in entities {
        dimensions.insert(name.clone(), calculate_entity_dimensions(entity));
    }

    let mut positions: HashMap<String, (f32, f32)> = HashMap::new();
    for node in &lg.nodes {
        if node.is_dummy {
            continue;
        }
        if let Some(name) = id_to_name.get(&node.id) {
            positions.insert(
                name.clone(),
                (node.x.unwrap_or(0.0) as f32, node.y.unwrap_or(0.0) as f32),
            );
        }
    }

    let mut entity_names: Vec<&String> = entities.keys().collect();
    entity_names.sort();

    let mut entity_boxes = Vec::new();
    for name in entity_names {
        let Some(entity) = entities.get(name) else {
            continue;
        };
        let Some(node) = lg.get_node(&entity.id) else {
            continue;
        };
        let dims = dimensions
            .get(name)
            .cloned()
            .unwrap_or_else(|| calculate_entity_dimensions(entity));
        let (x, y) = node_origin(node, bx, by);
        let (fill, stroke, text_color) = entity_colors(db, entity);
        entity_boxes.push(ErEntityBox {
            name: name.clone(),
            x,
            y,
            w: node.width as f32,
            h: node.height as f32,
            title: display_name(entity).to_string(),
            attrs: entity.attributes.iter().map(convert_attr).collect(),
            col_widths: dims.col_widths,
            fill,
            stroke,
            text_color,
        });
    }
    apply_entity_overrides(&mut entity_boxes, overrides);
    let entity_lookup: HashMap<&str, &ErEntityBox> = entity_boxes
        .iter()
        .map(|entity| (entity.name.as_str(), entity))
        .collect();

    let mut edges = Vec::new();
    for (idx, rel) in db.get_relationships().iter().enumerate() {
        let edge_id = format!("relationship-{idx}");
        let a_name = id_to_name.get(&rel.entity_a).map(String::as_str);
        let b_name = id_to_name.get(&rel.entity_b).map(String::as_str);
        let edge_override = a_name
            .zip(b_name)
            .and_then(|(a, b)| overrides.edge(a, b).or_else(|| overrides.edge(b, a)));
        let entity_moved = a_name.and_then(|name| overrides.object(name)).is_some()
            || b_name.and_then(|name| overrides.object(name)).is_some();
        let edge = lg.edges.iter().find(|edge| edge.id == edge_id);
        let auto_points =
            if entity_moved || edge_override.is_some_and(|ov| !ov.bend_points.is_empty()) {
                direct_edge_points_for_boxes(rel, &id_to_name, &entity_lookup)
            } else {
                edge.and_then(|edge| {
                    edge_points(edge, bx, by, rel, &id_to_name, &positions, &dimensions)
                })
                .or_else(|| direct_edge_points_for_boxes(rel, &id_to_name, &entity_lookup))
                .or_else(|| direct_edge_points(rel, &id_to_name, &positions, &dimensions, bx, by))
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
        if points.len() < 2 {
            continue;
        }

        edges.push(ErEdge {
            points,
            label: rel.role_a.clone(),
            label_pos: edge_override.and_then(|ov| ov.label_pos),
            label_offset: edge_override.and_then(|ov| ov.label_offset),
            line_style: match rel.rel_spec.rel_type {
                Identification::Identifying => LineStyle::Solid,
                Identification::NonIdentifying => LineStyle::Dot,
            },
            start_card: convert_cardinality(rel.rel_spec.card_b),
            end_card: convert_cardinality(rel.rel_spec.card_a),
            color: theme::MERMAID_EDGE,
        });
    }

    grow_bounds(&mut width, &mut height, &entity_boxes, &edges);
    if let Some(w) = overrides.graph.w {
        width = w;
    }
    if let Some(h) = overrides.graph.h {
        height = h;
    }

    ErGraph {
        width: width.max(1.0),
        height: height.max(1.0),
        entities: entity_boxes,
        edges,
    }
}

fn apply_entity_overrides(entities: &mut [ErEntityBox], overrides: &ManualLayoutOverrides) {
    for entity in entities {
        if let Some(ov) = overrides.object(&entity.name) {
            apply_box_override(
                &mut entity.x,
                &mut entity.y,
                &mut entity.w,
                &mut entity.h,
                ov,
            );
        }
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

fn grow_bounds(width: &mut f32, height: &mut f32, entities: &[ErEntityBox], edges: &[ErEdge]) {
    for entity in entities {
        *width = (*width).max(entity.x + entity.w + ER_MARGIN);
        *height = (*height).max(entity.y + entity.h + ER_MARGIN);
    }
    for edge in edges {
        for (x, y) in &edge.points {
            *width = (*width).max(*x + ER_MARGIN);
            *height = (*height).max(*y + ER_MARGIN);
        }
        if let Some((x, y)) = edge.label_pos {
            *width = (*width).max(x + ER_MARGIN);
            *height = (*height).max(y + ER_MARGIN);
        }
    }
}

fn node_origin(node: &LayoutNode, bx: f32, by: f32) -> (f32, f32) {
    (
        node.x.unwrap_or(0.0) as f32 - bx + ER_MARGIN,
        node.y.unwrap_or(0.0) as f32 - by + ER_MARGIN,
    )
}

fn edge_points(
    edge: &LayoutEdge,
    bx: f32,
    by: f32,
    rel: &selkie::diagrams::er::Relationship,
    id_to_name: &HashMap<String, String>,
    positions: &HashMap<String, (f32, f32)>,
    dimensions: &HashMap<String, EntityDimensions>,
) -> Option<Vec<(f32, f32)>> {
    if edge.bend_points.len() < 2 {
        return None;
    }

    let points = match (id_to_name.get(&rel.entity_a), id_to_name.get(&rel.entity_b)) {
        (Some(a), Some(b)) => {
            adjust_bend_points_for_intersection(&edge.bend_points, a, b, positions, dimensions)
        }
        _ => edge.bend_points.clone(),
    };

    Some(
        points
            .iter()
            .map(|p| (p.x as f32 - bx + ER_MARGIN, p.y as f32 - by + ER_MARGIN))
            .collect(),
    )
}

fn direct_edge_points(
    rel: &selkie::diagrams::er::Relationship,
    id_to_name: &HashMap<String, String>,
    positions: &HashMap<String, (f32, f32)>,
    dimensions: &HashMap<String, EntityDimensions>,
    bx: f32,
    by: f32,
) -> Option<Vec<(f32, f32)>> {
    let a_name = id_to_name.get(&rel.entity_a)?;
    let b_name = id_to_name.get(&rel.entity_b)?;
    let &(ax, ay) = positions.get(a_name)?;
    let &(bx0, by0) = positions.get(b_name)?;
    let ad = dimensions.get(a_name)?;
    let bd = dimensions.get(b_name)?;

    let a_center = Point::new((ax + ad.width / 2.0) as f64, (ay + ad.height / 2.0) as f64);
    let b_center = Point::new(
        (bx0 + bd.width / 2.0) as f64,
        (by0 + bd.height / 2.0) as f64,
    );
    let (sx, sy) = intersect_rect(
        ax as f64,
        ay as f64,
        ad.width as f64,
        ad.height as f64,
        b_center.x,
        b_center.y,
    );
    let (ex, ey) = intersect_rect(
        bx0 as f64,
        by0 as f64,
        bd.width as f64,
        bd.height as f64,
        a_center.x,
        a_center.y,
    );

    Some(vec![
        (sx as f32 - bx + ER_MARGIN, sy as f32 - by + ER_MARGIN),
        (ex as f32 - bx + ER_MARGIN, ey as f32 - by + ER_MARGIN),
    ])
}

fn direct_edge_points_for_boxes(
    rel: &selkie::diagrams::er::Relationship,
    id_to_name: &HashMap<String, String>,
    entities: &HashMap<&str, &ErEntityBox>,
) -> Option<Vec<(f32, f32)>> {
    let a = *entities.get(id_to_name.get(&rel.entity_a)?.as_str())?;
    let b = *entities.get(id_to_name.get(&rel.entity_b)?.as_str())?;
    let a_center = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let b_center = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let (sx, sy) = intersect_rect(
        a.x as f64,
        a.y as f64,
        a.w as f64,
        a.h as f64,
        b_center.0 as f64,
        b_center.1 as f64,
    );
    let (ex, ey) = intersect_rect(
        b.x as f64,
        b.y as f64,
        b.w as f64,
        b.h as f64,
        a_center.0 as f64,
        a_center.1 as f64,
    );
    Some(vec![(sx as f32, sy as f32), (ex as f32, ey as f32)])
}

fn calculate_entity_dimensions(entity: &Entity) -> EntityDimensions {
    let mut max_type_width = 0.0_f32;
    let mut max_name_width = 0.0_f32;
    let mut max_keys_width = 0.0_f32;
    let char_w = ATTR_FONT_SIZE * TEXT_W;

    for attr in &entity.attributes {
        max_type_width = max_type_width.max(attr.attr_type.chars().count() as f32 * char_w);
        max_name_width = max_name_width.max(attr.name.chars().count() as f32 * char_w);
        let keys_width = attr
            .keys
            .iter()
            .map(|k| key_name(*k))
            .collect::<Vec<_>>()
            .join(",")
            .chars()
            .count() as f32
            * char_w;
        max_keys_width = max_keys_width.max(keys_width);
    }

    let col_padding = 22.0;
    let type_col = max_type_width + col_padding;
    let name_col = max_name_width + col_padding;
    let keys_col = if max_keys_width > 0.0 {
        max_keys_width + 46.0
    } else {
        46.0
    };
    let content_w = type_col + name_col + keys_col;
    let header_w = display_name(entity).chars().count() as f32 * TITLE_FONT_SIZE * TEXT_W + 48.0;
    let width = content_w.max(header_w).max(100.0);
    let height = if entity.attributes.is_empty() {
        HEADER_H + 16.0
    } else {
        HEADER_H + entity.attributes.len() as f32 * ROW_H + 16.0
    };

    EntityDimensions {
        width,
        height,
        col_widths: [type_col, name_col, keys_col],
    }
}

fn display_name(entity: &Entity) -> &str {
    if entity.alias.is_empty() {
        &entity.label
    } else {
        &entity.alias
    }
}

fn convert_attr(attr: &selkie::diagrams::er::Attribute) -> ErAttribute {
    ErAttribute {
        attr_type: attr.attr_type.clone(),
        name: attr.name.clone(),
        keys: attr
            .keys
            .iter()
            .map(|k| key_name(*k))
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn key_name(key: AttributeKey) -> &'static str {
    match key {
        AttributeKey::PrimaryKey => "PK",
        AttributeKey::ForeignKey => "FK",
        AttributeKey::UniqueKey => "UK",
    }
}

fn convert_cardinality(card: SCardinality) -> ErCardinality {
    match card {
        SCardinality::ZeroOrOne => ErCardinality::ZeroOrOne,
        SCardinality::ZeroOrMore => ErCardinality::ZeroOrMore,
        SCardinality::OneOrMore => ErCardinality::OneOrMore,
        SCardinality::OnlyOne | SCardinality::MdParent => ErCardinality::OnlyOne,
    }
}

fn entity_colors(db: &ErDb, entity: &Entity) -> (u32, u32, u32) {
    let mut fill = theme::MERMAID_NODE_FILL;
    let mut stroke = theme::MERMAID_NODE_STROKE;
    let mut text = theme::MERMAID_NODE_TEXT;

    let classes: Vec<&str> = entity
        .css_classes
        .split_whitespace()
        .filter(|name| *name != "default")
        .collect();
    for style in db.get_compiled_styles(&classes) {
        apply_style_decl(&style, &mut fill, &mut stroke, &mut text);
    }
    for style in &entity.css_styles {
        apply_style_decl(style, &mut fill, &mut stroke, &mut text);
    }
    (fill, stroke, text)
}

fn apply_style_decl(style: &str, fill: &mut u32, stroke: &mut u32, text: &mut u32) {
    for decl in style.split([';', ',']) {
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

fn adjust_bend_points_for_intersection(
    bend_points: &[Point],
    entity_a_name: &str,
    entity_b_name: &str,
    positions: &HashMap<String, (f32, f32)>,
    dimensions: &HashMap<String, EntityDimensions>,
) -> Vec<Point> {
    if bend_points.len() < 2 {
        return bend_points.to_vec();
    }

    let Some(&(ax, ay)) = positions.get(entity_a_name) else {
        return bend_points.to_vec();
    };
    let Some(&(bx, by)) = positions.get(entity_b_name) else {
        return bend_points.to_vec();
    };
    let Some(a_dims) = dimensions.get(entity_a_name) else {
        return bend_points.to_vec();
    };
    let Some(b_dims) = dimensions.get(entity_b_name) else {
        return bend_points.to_vec();
    };

    let mut adjusted = bend_points.to_vec();
    let last_idx = adjusted.len() - 1;
    let a_target = if bend_points.len() > 2 {
        bend_points[1]
    } else {
        Point::new(
            (bx + b_dims.width / 2.0) as f64,
            (by + b_dims.height / 2.0) as f64,
        )
    };
    let (start_x, start_y) = intersect_rect(
        ax as f64,
        ay as f64,
        a_dims.width as f64,
        a_dims.height as f64,
        a_target.x,
        a_target.y,
    );
    adjusted[0] = Point::new(start_x, start_y);

    let b_source = if bend_points.len() > 3 {
        bend_points[last_idx - 1]
    } else {
        Point::new(start_x, start_y)
    };
    let (end_x, end_y) = intersect_rect(
        bx as f64,
        by as f64,
        b_dims.width as f64,
        b_dims.height as f64,
        b_source.x,
        b_source.y,
    );
    adjusted[last_idx] = Point::new(end_x, end_y);
    adjusted
}

fn intersect_rect(rx: f64, ry: f64, w: f64, h: f64, px: f64, py: f64) -> (f64, f64) {
    let cx = rx + w / 2.0;
    let cy = ry + h / 2.0;
    let dx = px - cx;
    let dy = py - cy;

    if dx.abs() < 0.001 && dy.abs() < 0.001 {
        return (cx, cy);
    }

    let sx = if dx.abs() > 0.001 {
        (w / 2.0) / dx.abs()
    } else {
        f64::INFINITY
    };
    let sy = if dy.abs() > 0.001 {
        (h / 2.0) / dy.abs()
    } else {
        f64::INFINITY
    };
    let t = sx.min(sy);
    (cx + t * dx, cy + t * dy)
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &ErGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale;

    for edge in &graph.edges {
        draw_edge(target, factory, edge, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    for entity in &graph.entities {
        draw_entity(target, entity, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_entity(
    target: &ID2D1RenderTarget,
    entity: &ErEntityBox,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(entity.x);
    let y = ty(entity.y);
    let w = ts(entity.w);
    let h = ts(entity.h);
    let header_h = ts(HEADER_H);
    let row_h = ts(ROW_H);
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let fill = brush(entity.fill)?;
    let stroke = brush(entity.stroke)?;

    target.FillRectangle(std::ptr::addr_of!(rect), &fill);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        (theme::MERMAID_NODE_STROKE_W * ts(1.0)).max(0.75),
        None::<&ID2D1StrokeStyle>,
    );

    if entity.attrs.is_empty() {
        draw_text(
            target,
            &entity.title,
            rect,
            ts(TITLE_FONT_SIZE),
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            entity.text_color,
            brush,
            fmt,
        )?;
        return Ok(());
    }

    for (idx, _) in entity.attrs.iter().enumerate() {
        let row_y = y + header_h + idx as f32 * row_h;
        let row_rect = D2D_RECT_F {
            left: x,
            top: row_y,
            right: x + w,
            bottom: (row_y + row_h).min(y + h),
        };
        let color = if idx % 2 == 0 { theme::BG } else { entity.fill };
        let row_br = brush(color)?;
        target.FillRectangle(std::ptr::addr_of!(row_rect), &row_br);
    }

    let type_end = x + ts(entity.col_widths[0]);
    let name_end = type_end + ts(entity.col_widths[1]);
    let content_y = y + header_h;
    target.DrawLine(
        Vector2 { X: x, Y: content_y },
        Vector2 {
            X: x + w,
            Y: content_y,
        },
        &stroke,
        1.0,
        None::<&ID2D1StrokeStyle>,
    );
    for x_line in [type_end, name_end] {
        if x_line < x + w {
            target.DrawLine(
                Vector2 {
                    X: x_line,
                    Y: content_y,
                },
                Vector2 {
                    X: x_line,
                    Y: y + h,
                },
                &stroke,
                1.0,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        (theme::MERMAID_NODE_STROKE_W * ts(1.0)).max(0.75),
        None::<&ID2D1StrokeStyle>,
    );

    let title_rect = D2D_RECT_F {
        left: x + 8.0,
        top: y,
        right: x + w - 8.0,
        bottom: y + header_h,
    };
    draw_text(
        target,
        &entity.title,
        title_rect,
        ts(TITLE_FONT_SIZE),
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        entity.text_color,
        brush,
        fmt,
    )?;

    let font_size = ts(ATTR_FONT_SIZE);
    for (idx, attr) in entity.attrs.iter().enumerate() {
        let row_y = y + header_h + idx as f32 * row_h;
        let top = row_y;
        let bottom = (row_y + row_h).min(y + h);
        draw_text(
            target,
            &attr.attr_type,
            D2D_RECT_F {
                left: x + 8.0,
                top,
                right: type_end - 6.0,
                bottom,
            },
            font_size,
            false,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            entity.text_color,
            brush,
            fmt,
        )?;
        draw_text(
            target,
            &attr.name,
            D2D_RECT_F {
                left: type_end + 8.0,
                top,
                right: name_end - 6.0,
                bottom,
            },
            font_size,
            false,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            entity.text_color,
            brush,
            fmt,
        )?;
        if !attr.keys.is_empty() && name_end < x + w {
            draw_text(
                target,
                &attr.keys,
                D2D_RECT_F {
                    left: name_end + 3.0,
                    top,
                    right: x + w - 2.0,
                    bottom,
                },
                font_size,
                false,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                theme::MERMAID_GROUP_TITLE,
                brush,
                fmt,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_edge(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    edge: &ErEdge,
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
    let line = brush(edge.color)?;
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
            &line,
            line_w,
            style,
        );
    }

    draw_cardinality_marker(
        target,
        pts[1],
        pts[0],
        edge.start_card,
        line_w,
        scale,
        &line,
        brush,
    )?;
    draw_cardinality_marker(
        target,
        pts[pts.len() - 2],
        pts[pts.len() - 1],
        edge.end_card,
        line_w,
        scale,
        &line,
        brush,
    )?;

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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_cardinality_marker(
    target: &ID2D1RenderTarget,
    from: (f32, f32),
    to: (f32, f32),
    card: ErCardinality,
    line_w: f32,
    scale: f32,
    line: &ID2D1SolidColorBrush,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let s = (10.0 * scale).max(8.0);

    let point_at = |d: f32| (to.0 - ux * d, to.1 - uy * d);
    let draw_bar = |target: &ID2D1RenderTarget, center: (f32, f32)| {
        target.DrawLine(
            Vector2 {
                X: center.0 + px * s * 0.55,
                Y: center.1 + py * s * 0.55,
            },
            Vector2 {
                X: center.0 - px * s * 0.55,
                Y: center.1 - py * s * 0.55,
            },
            line,
            line_w,
            None::<&ID2D1StrokeStyle>,
        );
    };
    let draw_crow = |target: &ID2D1RenderTarget| {
        let tip = point_at(s * 0.45);
        let back = point_at(s * 1.8);
        let left = (back.0 + px * s * 0.85, back.1 + py * s * 0.85);
        let right = (back.0 - px * s * 0.85, back.1 - py * s * 0.85);
        for end in [left, back, right] {
            target.DrawLine(
                Vector2 { X: tip.0, Y: tip.1 },
                Vector2 { X: end.0, Y: end.1 },
                line,
                line_w,
                None::<&ID2D1StrokeStyle>,
            );
        }
    };

    match card {
        ErCardinality::OnlyOne => {
            draw_bar(target, point_at(s * 0.65));
            draw_bar(target, point_at(s * 1.25));
        }
        ErCardinality::ZeroOrOne => {
            draw_bar(target, point_at(s * 0.65));
            draw_marker_circle(target, point_at(s * 1.45), s * 0.42, line_w, line, brush)?;
        }
        ErCardinality::OneOrMore => {
            draw_bar(target, point_at(s * 2.4));
            draw_crow(target);
        }
        ErCardinality::ZeroOrMore => {
            draw_marker_circle(target, point_at(s * 2.45), s * 0.42, line_w, line, brush)?;
            draw_crow(target);
        }
    }
    Ok(())
}

unsafe fn draw_marker_circle(
    target: &ID2D1RenderTarget,
    center: (f32, f32),
    radius: f32,
    line_w: f32,
    line: &ID2D1SolidColorBrush,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let bg = brush(theme::BG)?;
    let e = D2D1_ELLIPSE {
        point: Vector2 {
            X: center.0,
            Y: center.1,
        },
        radiusX: radius,
        radiusY: radius,
    };
    target.FillEllipse(std::ptr::addr_of!(e), &bg);
    target.DrawEllipse(
        std::ptr::addr_of!(e),
        line,
        line_w,
        None::<&ID2D1StrokeStyle>,
    );
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
    let target = total / 2.0;
    let mut walked = 0.0;
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
        DWRITE_TEXT_ALIGNMENT_CENTER,
        theme::MERMAID_EDGE_LABEL,
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
    align: DWRITE_TEXT_ALIGNMENT,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let f = fmt(theme::BODY_FONT, size, bold, false)?;
    let _ = f.SetTextAlignment(align);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
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
    fn builds_entities_and_relationships() {
        let source = r#"erDiagram
    CUSTOMER ||--o{ ORDER : places
    CUSTOMER {
        string id PK
    }
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::Er(db) = diagram else {
            panic!("expected ER diagram");
        };
        let graph = build_with_overrides(&db, &ManualLayoutOverrides::default()).unwrap();
        assert_eq!(graph.entities.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].label, "places");
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let source = r#"erDiagram
    CUSTOMER ||--o{ ORDER : places
    ORDER ||--|{ LINE_ITEM : contains
    CUSTOMER {
        string id PK
        string name
    }
    ORDER {
        string id PK
        string customer_id FK
    }
    LINE_ITEM {
        string order_id FK
        int quantity
    }
%% @node CUSTOMER x=40 y=72 w=190 h=140
%% @node ORDER x=320 y=72 w=210 h=140
%% @node LINE_ITEM x=320 y=272 w=210 h=140
%% @edge CUSTOMER->ORDER points="230,142 320,142" label_offset="0,-14"
%% @edge ORDER->LINE_ITEM bend_points="425,235" label_pos="486,235"
%% @graph w=590 h=460
"#;
        let graph = match crate::mermaid::build(source).unwrap() {
            Graph::Er(graph) => graph,
            other => panic!("expected ER graph, got {other:?}"),
        };

        let customer = graph
            .entities
            .iter()
            .find(|e| e.name == "CUSTOMER")
            .unwrap();
        let order = graph.entities.iter().find(|e| e.name == "ORDER").unwrap();
        let line_item = graph
            .entities
            .iter()
            .find(|e| e.name == "LINE_ITEM")
            .unwrap();
        assert_eq!(
            (customer.x, customer.y, customer.w, customer.h),
            (40.0, 72.0, 190.0, 140.0)
        );
        assert_eq!(
            (order.x, order.y, order.w, order.h),
            (320.0, 72.0, 210.0, 140.0)
        );
        assert_eq!(
            (line_item.x, line_item.y, line_item.w, line_item.h),
            (320.0, 272.0, 210.0, 140.0)
        );
        assert_eq!(graph.edges[0].points, vec![(230.0, 142.0), (320.0, 142.0)]);
        assert_eq!(graph.edges[0].label_offset, Some((0.0, -14.0)));
        assert_eq!(graph.edges[1].points[1], (425.0, 235.0));
        assert_eq!(graph.edges[1].label_pos, Some((486.0, 235.0)));
        assert_eq!((graph.width, graph.height), (590.0, 460.0));
    }
}
