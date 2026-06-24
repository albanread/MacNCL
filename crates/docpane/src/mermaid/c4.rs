//! C4 diagram build + Direct2D renderer.

use std::collections::HashMap;

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::c4::{C4Boundary, C4Db, C4Element, C4Relationship, C4ShapeType};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

const SHAPES_PER_ROW: usize = 3;
const ELEMENT_W: f32 = 216.0;
const ELEMENT_H: f32 = 119.0;
const PERSON_H: f32 = 167.0;
const ELEMENT_MARGIN: f32 = 50.0;
const DIAGRAM_MARGIN: f32 = 38.0;
const BOUNDARY_PADDING: f32 = 22.0;
const BOUNDARY_LABEL_H: f32 = 42.0;
const MAX_ROW_W: f32 = 800.0;
const TITLE_H: f32 = 40.0;

const TYPE_FONT: f32 = 10.5;
const LABEL_FONT: f32 = 13.5;
const TECH_FONT: f32 = 10.5;
const DESC_FONT: f32 = 10.5;
const BOUNDARY_FONT: f32 = 13.0;

pub fn build_with_overrides(db: &C4Db, overrides: &ManualLayoutOverrides) -> C4Graph {
    let title = db.get_title().map(str::to_string);
    let title_h = if title.is_some() { TITLE_H } else { 0.0 };

    let mut elements_by_boundary: HashMap<&str, Vec<&C4Element>> = HashMap::new();
    for element in db.get_elements() {
        elements_by_boundary
            .entry(element.parent_boundary.as_str())
            .or_default()
            .push(element);
    }

    let mut root = Bounds::new(DIAGRAM_MARGIN, DIAGRAM_MARGIN + title_h, MAX_ROW_W);
    let mut elements = Vec::new();
    let mut boundaries = Vec::new();

    if let Some(root_elements) = elements_by_boundary.get("") {
        for element in root_elements {
            let h = element_height(&element.shape_type);
            let (x, y) = root.insert(ELEMENT_W, h);
            elements.push(convert_element(element, x, y, ELEMENT_W, h));
        }
    }

    root.break_row();
    for boundary in db
        .get_boundaries()
        .iter()
        .filter(|boundary| boundary.parent_boundary.is_empty())
    {
        process_boundary(
            boundary,
            db,
            &elements_by_boundary,
            &mut root,
            &mut elements,
            &mut boundaries,
        );
    }

    apply_element_overrides(&mut elements, overrides);
    apply_boundary_overrides(&mut boundaries, overrides);
    boundaries.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));

    let lookup: HashMap<&str, &C4ElementBox> = elements
        .iter()
        .map(|element| (element.alias.as_str(), element))
        .collect();
    let mut relationships = Vec::new();
    for relationship in db.get_relationships() {
        if let Some(edge) = convert_relationship(relationship, &lookup, overrides) {
            relationships.push(edge);
        }
    }

    let mut width = (root.stop_x + DIAGRAM_MARGIN).max(400.0);
    let mut height = (root.stop_y + DIAGRAM_MARGIN).max(160.0 + title_h);
    for element in &elements {
        width = width.max(element.x + element.w + DIAGRAM_MARGIN);
        height = height.max(element.y + element.h + DIAGRAM_MARGIN);
    }
    for boundary in &boundaries {
        width = width.max(boundary.x + boundary.w + DIAGRAM_MARGIN);
        height = height.max(boundary.y + boundary.h + DIAGRAM_MARGIN);
    }
    for edge in &relationships {
        for (x, y) in &edge.points {
            width = width.max(*x + DIAGRAM_MARGIN);
            height = height.max(*y + DIAGRAM_MARGIN);
        }
        if let Some((x, y)) = edge.label_pos {
            width = width.max(x + DIAGRAM_MARGIN);
            height = height.max(y + DIAGRAM_MARGIN);
        }
    }
    if let Some(w) = overrides.graph.w {
        width = w;
    }
    if let Some(h) = overrides.graph.h {
        height = h;
    }

    C4Graph {
        width,
        height,
        title,
        boundaries,
        relationships,
        elements,
    }
}

fn apply_element_overrides(elements: &mut [C4ElementBox], overrides: &ManualLayoutOverrides) {
    for element in elements {
        if let Some(ov) = overrides.object(&element.alias) {
            apply_box_override(
                &mut element.x,
                &mut element.y,
                &mut element.w,
                &mut element.h,
                ov,
            );
        }
    }
}

fn apply_boundary_overrides(boundaries: &mut [C4BoundaryBox], overrides: &ManualLayoutOverrides) {
    for boundary in boundaries {
        if let Some(ov) = overrides.group(&boundary.alias) {
            apply_box_override(
                &mut boundary.x,
                &mut boundary.y,
                &mut boundary.w,
                &mut boundary.h,
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

#[derive(Debug, Clone)]
struct Bounds {
    start_x: f32,
    start_y: f32,
    next_x: f32,
    next_y: f32,
    stop_x: f32,
    stop_y: f32,
    row_count: usize,
    row_max_h: f32,
    width_limit: f32,
}

impl Bounds {
    fn new(x: f32, y: f32, width_limit: f32) -> Self {
        Self {
            start_x: x,
            start_y: y,
            next_x: x,
            next_y: y,
            stop_x: x,
            stop_y: y,
            row_count: 0,
            row_max_h: 0.0,
            width_limit,
        }
    }

    fn insert(&mut self, w: f32, h: f32) -> (f32, f32) {
        let would_wrap = self.row_count > 0
            && (self.row_count >= SHAPES_PER_ROW
                || self.next_x + w > self.start_x + self.width_limit);
        if would_wrap {
            self.break_row();
        }

        let x = self.next_x;
        let y = self.next_y;
        self.next_x = x + w + ELEMENT_MARGIN;
        self.row_count += 1;
        self.row_max_h = self.row_max_h.max(h);
        self.stop_x = self.stop_x.max(x + w);
        self.stop_y = self.stop_y.max(y + self.row_max_h);
        (x, y)
    }

    fn break_row(&mut self) {
        if self.row_count == 0 {
            return;
        }
        self.next_x = self.start_x;
        self.next_y = self.stop_y + ELEMENT_MARGIN;
        self.row_count = 0;
        self.row_max_h = 0.0;
    }

    fn absorb(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.stop_x = self.stop_x.max(x + w);
        self.stop_y = self.stop_y.max(y + h);
    }

    fn width(&self) -> f32 {
        (self.stop_x - self.start_x).max(0.0)
    }

    fn height(&self) -> f32 {
        (self.stop_y - self.start_y).max(0.0)
    }
}

fn process_boundary(
    boundary: &C4Boundary,
    db: &C4Db,
    elements_by_boundary: &HashMap<&str, Vec<&C4Element>>,
    parent: &mut Bounds,
    elements: &mut Vec<C4ElementBox>,
    boundaries: &mut Vec<C4BoundaryBox>,
) {
    parent.break_row();

    let boundary_x = parent.start_x;
    let boundary_y = parent.next_y;
    let mut content = Bounds::new(
        boundary_x + BOUNDARY_PADDING,
        boundary_y + BOUNDARY_PADDING + BOUNDARY_LABEL_H,
        (parent.width_limit - BOUNDARY_PADDING * 2.0).max(ELEMENT_W),
    );

    if let Some(boundary_elements) = elements_by_boundary.get(boundary.alias.as_str()) {
        for element in boundary_elements {
            let h = element_height(&element.shape_type);
            let (x, y) = content.insert(ELEMENT_W, h);
            elements.push(convert_element(element, x, y, ELEMENT_W, h));
        }
    }

    content.break_row();
    for nested in db
        .get_boundaries()
        .iter()
        .filter(|nested| nested.parent_boundary == boundary.alias)
    {
        process_boundary(
            nested,
            db,
            elements_by_boundary,
            &mut content,
            elements,
            boundaries,
        );
    }

    let content_w = content.width().max(ELEMENT_W);
    let content_h = content.height().max(ELEMENT_H * 0.55);
    let w = content_w + BOUNDARY_PADDING * 2.0;
    let h = content_h + BOUNDARY_PADDING * 2.0 + BOUNDARY_LABEL_H;

    boundaries.push(C4BoundaryBox {
        alias: boundary.alias.clone(),
        x: boundary_x,
        y: boundary_y,
        w,
        h,
        label: if boundary.label.is_empty() {
            boundary.alias.clone()
        } else {
            boundary.label.clone()
        },
        kind: boundary.boundary_type.clone(),
        solid: boundary.boundary_type.starts_with("deployment"),
    });

    parent.absorb(boundary_x, boundary_y, w, h);
    parent.next_x = parent.start_x;
    parent.next_y = parent.stop_y + ELEMENT_MARGIN;
    parent.row_count = 0;
    parent.row_max_h = 0.0;
}

fn convert_element(element: &C4Element, x: f32, y: f32, w: f32, h: f32) -> C4ElementBox {
    let (fill, stroke, text_color) = element_colors(&element.shape_type);
    C4ElementBox {
        alias: element.alias.clone(),
        x,
        y,
        w,
        h,
        label: if element.label.is_empty() {
            element.alias.clone()
        } else {
            element.label.clone()
        },
        kind_label: shape_type_label(&element.shape_type).to_string(),
        technology: element.technology.clone(),
        description: element.description.clone(),
        shape: element_shape(&element.shape_type),
        fill,
        stroke,
        text_color,
    }
}

fn convert_relationship(
    rel: &C4Relationship,
    lookup: &HashMap<&str, &C4ElementBox>,
    overrides: &ManualLayoutOverrides,
) -> Option<C4Edge> {
    let raw_from = rel.from.as_str();
    let raw_to = rel.to.as_str();
    let mut from = raw_from;
    let mut to = raw_to;
    if rel.rel_type.contains("Back") {
        std::mem::swap(&mut from, &mut to);
    }

    let source = *lookup.get(from)?;
    let target = *lookup.get(to)?;
    let edge_override = overrides
        .edge(from, to)
        .or_else(|| overrides.edge(raw_from, raw_to));
    let auto_points = if source.alias == target.alias {
        self_loop_points(source)
    } else {
        let (start, end) = connection_points(source, target);
        route_edge(source, target, start, end)
    };
    let points = if let Some(ov) = edge_override {
        if let Some(points) = &ov.points {
            points.clone()
        } else if ov.bend_points.is_empty() {
            auto_points
        } else {
            let mut points = Vec::with_capacity(ov.bend_points.len() + 2);
            points.push(auto_points[0]);
            points.extend(ov.bend_points.iter().copied());
            points.push(*auto_points.last().unwrap_or(&auto_points[0]));
            points
        }
    } else {
        auto_points
    };

    Some(C4Edge {
        points,
        label: rel.label.clone(),
        technology: rel.technology.clone(),
        label_pos: edge_override.and_then(|ov| ov.label_pos),
        label_offset: edge_override.and_then(|ov| ov.label_offset),
        bidirectional: rel.rel_type == "BiRel",
        color: 0xA8A8A8,
    })
}

fn element_height(shape_type: &C4ShapeType) -> f32 {
    match shape_type {
        C4ShapeType::Person | C4ShapeType::PersonExt => PERSON_H,
        _ => ELEMENT_H,
    }
}

fn element_shape(shape_type: &C4ShapeType) -> C4Shape {
    match shape_type {
        C4ShapeType::Person | C4ShapeType::PersonExt => C4Shape::Person,
        C4ShapeType::SystemDb
        | C4ShapeType::SystemDbExt
        | C4ShapeType::ContainerDb
        | C4ShapeType::ContainerDbExt
        | C4ShapeType::ComponentDb
        | C4ShapeType::ComponentDbExt => C4Shape::Database,
        C4ShapeType::SystemQueue
        | C4ShapeType::SystemQueueExt
        | C4ShapeType::ContainerQueue
        | C4ShapeType::ContainerQueueExt
        | C4ShapeType::ComponentQueue
        | C4ShapeType::ComponentQueueExt => C4Shape::Queue,
        _ => C4Shape::Rect,
    }
}

fn shape_type_label(shape_type: &C4ShapeType) -> &'static str {
    match shape_type {
        C4ShapeType::Person => "person",
        C4ShapeType::PersonExt => "external_person",
        C4ShapeType::System => "system",
        C4ShapeType::SystemExt => "external_system",
        C4ShapeType::SystemDb => "system_db",
        C4ShapeType::SystemDbExt => "external_system_db",
        C4ShapeType::SystemQueue => "system_queue",
        C4ShapeType::SystemQueueExt => "external_system_queue",
        C4ShapeType::Container => "container",
        C4ShapeType::ContainerExt => "external_container",
        C4ShapeType::ContainerDb => "container_db",
        C4ShapeType::ContainerDbExt => "external_container_db",
        C4ShapeType::ContainerQueue => "container_queue",
        C4ShapeType::ContainerQueueExt => "external_container_queue",
        C4ShapeType::Component => "component",
        C4ShapeType::ComponentExt => "external_component",
        C4ShapeType::ComponentDb => "component_db",
        C4ShapeType::ComponentDbExt => "external_component_db",
        C4ShapeType::ComponentQueue => "component_queue",
        C4ShapeType::ComponentQueueExt => "external_component_queue",
    }
}

fn element_colors(shape_type: &C4ShapeType) -> (u32, u32, u32) {
    match shape_type {
        C4ShapeType::Person => (0x08427B, 0x073B6F, 0xFFFFFF),
        C4ShapeType::PersonExt => (0x62717C, 0x4E5B63, 0xFFFFFF),
        C4ShapeType::System | C4ShapeType::SystemDb | C4ShapeType::SystemQueue => {
            (0x1168BD, 0x0E5EA8, 0xFFFFFF)
        }
        C4ShapeType::SystemExt | C4ShapeType::SystemDbExt | C4ShapeType::SystemQueueExt => {
            (0x777777, 0x666666, 0xFFFFFF)
        }
        C4ShapeType::Container | C4ShapeType::ContainerDb | C4ShapeType::ContainerQueue => {
            (0x438DD5, 0x3477B7, 0xFFFFFF)
        }
        C4ShapeType::ContainerExt
        | C4ShapeType::ContainerDbExt
        | C4ShapeType::ContainerQueueExt => (0x888888, 0x777777, 0xFFFFFF),
        C4ShapeType::Component | C4ShapeType::ComponentDb | C4ShapeType::ComponentQueue => {
            (0x85BBF0, 0x6FA8DC, 0x1E1E1E)
        }
        C4ShapeType::ComponentExt
        | C4ShapeType::ComponentDbExt
        | C4ShapeType::ComponentQueueExt => (0xCCCCCC, 0xA6A6A6, 0x1E1E1E),
    }
}

fn connection_points(a: &C4ElementBox, b: &C4ElementBox) -> ((f32, f32), (f32, f32)) {
    let ac = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let bc = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    (rect_intersection(a, bc), rect_intersection(b, ac))
}

fn rect_intersection(rect: &C4ElementBox, toward: (f32, f32)) -> (f32, f32) {
    let cx = rect.x + rect.w / 2.0;
    let cy = rect.y + rect.h / 2.0;
    let dx = toward.0 - cx;
    let dy = toward.1 - cy;
    if dx.abs() < 0.001 && dy.abs() < 0.001 {
        return (cx, cy);
    }
    let sx = if dx.abs() > 0.001 {
        (rect.w / 2.0) / dx.abs()
    } else {
        f32::INFINITY
    };
    let sy = if dy.abs() > 0.001 {
        (rect.h / 2.0) / dy.abs()
    } else {
        f32::INFINITY
    };
    let t = sx.min(sy);
    (cx + dx * t, cy + dy * t)
}

fn route_edge(
    source: &C4ElementBox,
    target: &C4ElementBox,
    start: (f32, f32),
    end: (f32, f32),
) -> Vec<(f32, f32)> {
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let sc = (source.x + source.w / 2.0, source.y + source.h / 2.0);
    let tc = (target.x + target.w / 2.0, target.y + target.h / 2.0);

    if dx.abs() > dy.abs() * 1.15 {
        let lane_y = source.y.min(target.y) - 22.0;
        if lane_y > 4.0 {
            return vec![start, (start.0, lane_y), (end.0, lane_y), end];
        }
    }

    if dy.abs() > dx.abs() * 1.15 {
        let lane_x = (source.x + source.w).max(target.x + target.w) + 42.0;
        return vec![start, (lane_x, start.1), (lane_x, end.1), end];
    }

    if (sc.1 - tc.1).abs() < 24.0 && dx.abs() > 1.0 {
        let lane_y = source.y.min(target.y) - 22.0;
        if lane_y > 4.0 {
            return vec![start, (start.0, lane_y), (end.0, lane_y), end];
        }
    }

    if (sc.0 - tc.0).abs() < 48.0 && dy.abs() > 1.0 {
        let lane_x = (source.x + source.w).max(target.x + target.w) + 42.0;
        return vec![start, (lane_x, start.1), (lane_x, end.1), end];
    }

    if dx.abs() < 1.0 {
        let lane_x = (source.x + source.w).max(target.x + target.w) + 42.0;
        return vec![start, (lane_x, start.1), (lane_x, end.1), end];
    }
    if dy.abs() < 1.0 {
        let lane_y = source.y.min(target.y) - 22.0;
        if lane_y > 4.0 {
            return vec![start, (start.0, lane_y), (end.0, lane_y), end];
        }
        return vec![start, end];
    }
    if dx.abs() > dy.abs() {
        let mid_x = (start.0 + end.0) / 2.0;
        vec![start, (mid_x, start.1), (mid_x, end.1), end]
    } else {
        let mid_y = (start.1 + end.1) / 2.0;
        vec![start, (start.0, mid_y), (end.0, mid_y), end]
    }
}

fn self_loop_points(element: &C4ElementBox) -> Vec<(f32, f32)> {
    let x = element.x + element.w;
    let y0 = element.y + element.h * 0.35;
    let y1 = element.y + element.h * 0.68;
    let right = x + 44.0;
    vec![(x, y0), (right, y0), (right, y1), (x, y1)]
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &C4Graph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale;

    if let Some(title) = &graph.title {
        let rect = D2D_RECT_F {
            left: ox,
            top: oy,
            right: ox + graph.width * scale,
            bottom: oy + TITLE_H * scale,
        };
        draw_text(
            target,
            title,
            rect,
            15.0 * scale,
            true,
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_BRIGHT,
            false,
            &mut brush,
            &mut fmt,
        )?;
    }

    for boundary in &graph.boundaries {
        draw_boundary(
            target, factory, boundary, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }
    for edge in &graph.relationships {
        draw_edge_line(target, factory, edge, scale, &tx, &ty, &mut brush)?;
    }
    for element in &graph.elements {
        draw_element(
            target, factory, element, scale, &tx, &ty, &ts, &mut brush, &mut fmt,
        )?;
    }
    for edge in &graph.relationships {
        draw_edge_label(target, edge, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_boundary(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    boundary: &C4BoundaryBox,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let rect = D2D_RECT_F {
        left: tx(boundary.x),
        top: ty(boundary.y),
        right: tx(boundary.x) + ts(boundary.w),
        bottom: ty(boundary.y) + ts(boundary.h),
    };
    let rr = D2D1_ROUNDED_RECT {
        rect,
        radiusX: 5.0 * scale,
        radiusY: 5.0 * scale,
    };
    let fill = brush(theme::MERMAID_GROUP_FILL)?;
    let stroke = brush(0x666666)?;
    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill);
    let style = if boundary.solid {
        None
    } else {
        Some(crate::mermaid::render::sequence_dash_style(factory))
    };
    target.DrawRoundedRectangle(
        std::ptr::addr_of!(rr),
        &stroke,
        (1.0 * scale).max(0.8),
        style,
    );

    let label_rect = D2D_RECT_F {
        left: rect.left + 10.0 * scale,
        top: rect.top + 4.0 * scale,
        right: rect.right - 10.0 * scale,
        bottom: rect.top + 24.0 * scale,
    };
    draw_text(
        target,
        &boundary.label,
        label_rect,
        BOUNDARY_FONT * scale,
        true,
        false,
        DWRITE_TEXT_ALIGNMENT_LEADING,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        theme::MERMAID_GROUP_TITLE,
        false,
        brush,
        fmt,
    )?;

    if !boundary.kind.is_empty() && !boundary.solid {
        let kind = format!("[{}]", boundary.kind.to_uppercase());
        let type_rect = D2D_RECT_F {
            left: rect.left + 10.0 * scale,
            top: rect.top + 23.0 * scale,
            right: rect.right - 10.0 * scale,
            bottom: rect.top + 40.0 * scale,
        };
        draw_text(
            target,
            &kind,
            type_rect,
            10.5 * scale,
            false,
            false,
            DWRITE_TEXT_ALIGNMENT_LEADING,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT_DIM,
            false,
            brush,
            fmt,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_element(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    element: &C4ElementBox,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(element.x);
    let y = ty(element.y);
    let w = ts(element.w);
    let h = ts(element.h);
    let fill = brush(element.fill)?;
    let stroke = brush(element.stroke)?;
    draw_element_shape(
        target,
        factory,
        element.shape,
        x,
        y,
        w,
        h,
        scale,
        &fill,
        &stroke,
    )?;

    if matches!(element.shape, C4Shape::Person) {
        draw_person_icon(target, x, y, w, scale, element.text_color, brush)?;
    }

    let pad = 10.0 * scale;
    let type_rect = D2D_RECT_F {
        left: x + pad,
        top: y + 6.0 * scale,
        right: x + w - pad,
        bottom: y + 25.0 * scale,
    };
    let type_label = format!("<<{}>>", element.kind_label);
    draw_text(
        target,
        &type_label,
        type_rect,
        TYPE_FONT * scale,
        false,
        true,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        element.text_color,
        false,
        brush,
        fmt,
    )?;

    let label_top = if matches!(element.shape, C4Shape::Person) {
        y + 78.0 * scale
    } else {
        y + 29.0 * scale
    };
    let label_rect = D2D_RECT_F {
        left: x + pad,
        top: label_top,
        right: x + w - pad,
        bottom: label_top + 22.0 * scale,
    };
    draw_text(
        target,
        &element.label,
        label_rect,
        LABEL_FONT * scale,
        true,
        false,
        DWRITE_TEXT_ALIGNMENT_CENTER,
        DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        element.text_color,
        false,
        brush,
        fmt,
    )?;

    let mut body_top = label_top + 23.0 * scale;
    if !element.technology.is_empty() {
        let tech = format!("[{}]", element.technology);
        let tech_rect = D2D_RECT_F {
            left: x + pad,
            top: body_top,
            right: x + w - pad,
            bottom: body_top + 17.0 * scale,
        };
        draw_text(
            target,
            &tech,
            tech_rect,
            TECH_FONT * scale,
            false,
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            element.text_color,
            false,
            brush,
            fmt,
        )?;
        body_top += 17.0 * scale;
    }

    if !element.description.is_empty() && body_top < y + h - 8.0 * scale {
        let desc_rect = D2D_RECT_F {
            left: x + 14.0 * scale,
            top: body_top + 5.0 * scale,
            right: x + w - 14.0 * scale,
            bottom: y + h - 8.0 * scale,
        };
        draw_text(
            target,
            &element.description,
            desc_rect,
            DESC_FONT * scale,
            false,
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
            element.text_color,
            true,
            brush,
            fmt,
        )?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_element_shape(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    shape: C4Shape,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    scale: f32,
    fill: &ID2D1SolidColorBrush,
    stroke: &ID2D1SolidColorBrush,
) -> Result<()> {
    match shape {
        C4Shape::Database => {
            let cap_h = (h * 0.18).max(6.0 * scale).min(h * 0.45);
            let geo = build_cylinder_silhouette(factory, x, y, w, h, cap_h)?;
            target.FillGeometry(&geo, fill, None);
            target.DrawGeometry(
                &geo,
                stroke,
                (1.2 * scale).max(0.8),
                None::<&ID2D1StrokeStyle>,
            );
            let lip = build_cylinder_top_lip(factory, x, y, w, cap_h)?;
            target.DrawGeometry(
                &lip,
                stroke,
                (1.0 * scale).max(0.75),
                None::<&ID2D1StrokeStyle>,
            );
        }
        C4Shape::Queue => {
            let rect = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect,
                radiusX: 7.0 * scale,
                radiusY: 7.0 * scale,
            };
            target.FillRoundedRectangle(std::ptr::addr_of!(rr), fill);
            target.DrawRoundedRectangle(
                std::ptr::addr_of!(rr),
                stroke,
                (1.2 * scale).max(0.8),
                None::<&ID2D1StrokeStyle>,
            );
            let side = build_queue_side(factory, x, y, w, h, 14.0 * scale)?;
            target.DrawGeometry(
                &side,
                stroke,
                (1.0 * scale).max(0.75),
                None::<&ID2D1StrokeStyle>,
            );
        }
        C4Shape::Person | C4Shape::Rect => {
            let rect = D2D_RECT_F {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let rr = D2D1_ROUNDED_RECT {
                rect,
                radiusX: 5.0 * scale,
                radiusY: 5.0 * scale,
            };
            target.FillRoundedRectangle(std::ptr::addr_of!(rr), fill);
            target.DrawRoundedRectangle(
                std::ptr::addr_of!(rr),
                stroke,
                (1.2 * scale).max(0.8),
                None::<&ID2D1StrokeStyle>,
            );
        }
    }
    Ok(())
}

unsafe fn draw_person_icon(
    target: &ID2D1RenderTarget,
    x: f32,
    y: f32,
    w: f32,
    scale: f32,
    color: u32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    let br = brush(color)?;
    let cx = x + w / 2.0;
    let head_y = y + 42.0 * scale;
    let r = (10.0 * scale).max(4.0);
    let head = D2D1_ELLIPSE {
        point: Vector2 { X: cx, Y: head_y },
        radiusX: r,
        radiusY: r,
    };
    target.DrawEllipse(
        std::ptr::addr_of!(head),
        &br,
        (2.0 * scale).max(1.1),
        None::<&ID2D1StrokeStyle>,
    );
    let shoulder_y = y + 61.0 * scale;
    let body_y = y + 71.0 * scale;
    target.DrawLine(
        Vector2 {
            X: cx,
            Y: head_y + r,
        },
        Vector2 { X: cx, Y: body_y },
        &br,
        (2.0 * scale).max(1.1),
        None::<&ID2D1StrokeStyle>,
    );
    target.DrawLine(
        Vector2 {
            X: cx - 20.0 * scale,
            Y: shoulder_y,
        },
        Vector2 {
            X: cx + 20.0 * scale,
            Y: shoulder_y,
        },
        &br,
        (2.0 * scale).max(1.1),
        None::<&ID2D1StrokeStyle>,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_edge_line(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    edge: &C4Edge,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
) -> Result<()> {
    if edge.points.len() < 2 {
        return Ok(());
    }
    let pts: Vec<(f32, f32)> = edge.points.iter().map(|(x, y)| (tx(*x), ty(*y))).collect();
    let line = brush(edge.color)?;
    let line_w = (theme::MERMAID_EDGE_W * scale).max(0.8);
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
    draw_arrow(
        target,
        factory,
        pts[pts.len() - 2],
        pts[pts.len() - 1],
        line_w,
        scale,
        &line,
    )?;
    if edge.bidirectional {
        draw_arrow(target, factory, pts[1], pts[0], line_w, scale, &line)?;
    }

    Ok(())
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
    let tip = to;
    let back = (to.0 - ux * size, to.1 - uy * size);
    let half = size * 0.58;
    let left = (back.0 + px * half, back.1 + py * half);
    let right = (back.0 - px * half, back.1 - py * half);
    let geo = crate::mermaid::render::build_polygon_pub(factory, &[tip, left, right])?;
    target.FillGeometry(&geo, brush, None);
    target.DrawGeometry(&geo, brush, line_w, None::<&ID2D1StrokeStyle>);
    Ok(())
}

unsafe fn draw_edge_label(
    target: &ID2D1RenderTarget,
    edge: &C4Edge,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    if edge.label.is_empty() && edge.technology.is_empty() {
        return Ok(());
    }
    if edge.points.len() < 2 {
        return Ok(());
    }
    let pts: Vec<(f32, f32)> = edge.points.iter().map(|(x, y)| (tx(*x), ty(*y))).collect();
    let (cx, cy) = if let Some((x, y)) = edge.label_pos {
        (tx(x), ty(y))
    } else {
        let (mx, my) = polyline_midpoint(&pts);
        let (dx, dy) = edge.label_offset.unwrap_or((0.0, 0.0));
        (mx + dx * scale, my + dy * scale)
    };
    draw_edge_label_at(target, edge, cx, cy, scale, brush, fmt)
}

unsafe fn draw_edge_label_at(
    target: &ID2D1RenderTarget,
    edge: &C4Edge,
    cx: f32,
    cy: f32,
    scale: f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let font = (theme::MERMAID_EDGE_FONT_SIZE * scale).max(7.0);
    let tech = if edge.technology.is_empty() {
        String::new()
    } else {
        format!("[{}]", edge.technology)
    };
    let max_chars = edge.label.chars().count().max(tech.chars().count()) as f32;
    let w = (max_chars * font * 0.58).max(34.0) + 12.0 * scale;
    let lines = if edge.label.is_empty() || tech.is_empty() {
        1.0
    } else {
        2.0
    };
    let h = font * (lines * 1.3) + 8.0 * scale;
    let rect = D2D_RECT_F {
        left: cx - w / 2.0,
        top: cy - h / 2.0,
        right: cx + w / 2.0,
        bottom: cy + h / 2.0,
    };
    let bg = brush(theme::BG)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &bg);

    if !edge.label.is_empty() {
        let label_rect = D2D_RECT_F {
            left: rect.left + 3.0 * scale,
            top: rect.top,
            right: rect.right - 3.0 * scale,
            bottom: if tech.is_empty() {
                rect.bottom
            } else {
                rect.top + h / 2.0
            },
        };
        draw_text(
            target,
            &edge.label,
            label_rect,
            font,
            false,
            false,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::MERMAID_EDGE_LABEL,
            false,
            brush,
            fmt,
        )?;
    }
    if !tech.is_empty() {
        let tech_rect = D2D_RECT_F {
            left: rect.left + 3.0 * scale,
            top: if edge.label.is_empty() {
                rect.top
            } else {
                rect.top + h / 2.0 - 1.0 * scale
            },
            right: rect.right - 3.0 * scale,
            bottom: rect.bottom,
        };
        draw_text(
            target,
            &tech,
            tech_rect,
            font,
            false,
            true,
            DWRITE_TEXT_ALIGNMENT_CENTER,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
            theme::TEXT,
            false,
            brush,
            fmt,
        )?;
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

#[allow(clippy::too_many_arguments)]
unsafe fn draw_text(
    target: &ID2D1RenderTarget,
    text: &str,
    rect: D2D_RECT_F,
    size: f32,
    bold: bool,
    italic: bool,
    align: DWRITE_TEXT_ALIGNMENT,
    paragraph: DWRITE_PARAGRAPH_ALIGNMENT,
    color: u32,
    wrap: bool,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let f = fmt(theme::BODY_FONT, size, bold, italic)?;
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

unsafe fn build_queue_side(
    factory: &ID2D1Factory1,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    inset: f32,
) -> Result<ID2D1PathGeometry> {
    let geo1: ID2D1PathGeometry1 = factory.CreatePathGeometry()?;
    let geo: ID2D1PathGeometry = geo1.into();
    let sink: ID2D1GeometrySink = geo.Open()?;
    let sx = x + w - inset;
    sink.BeginFigure(Vector2 { X: sx, Y: y }, D2D1_FIGURE_BEGIN_HOLLOW);
    sink.AddBezier(&D2D1_BEZIER_SEGMENT {
        point1: Vector2 {
            X: x + w - inset * 0.25,
            Y: y + h * 0.25,
        },
        point2: Vector2 {
            X: x + w - inset * 0.25,
            Y: y + h * 0.75,
        },
        point3: Vector2 { X: sx, Y: y + h },
    });
    sink.EndFigure(D2D1_FIGURE_END_OPEN);
    sink.Close()?;
    Ok(geo)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_context_elements_and_relationships() {
        let source = r#"C4Context
title DocCrate Context
Person(reader, "Reader", "Uses local docs")
System(doccrate, "DocCrate", "Native Windows Markdown viewer")
BiRel(reader, doccrate, "Reads")
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::C4(db) = diagram else {
            panic!("expected C4 diagram");
        };
        let graph = build_with_overrides(&db, &ManualLayoutOverrides::default());
        assert_eq!(graph.elements.len(), 2);
        assert_eq!(graph.relationships.len(), 1);
        assert!(graph.relationships[0].bidirectional);
        assert!(graph.width > 300.0);
    }

    #[test]
    fn builds_boundary_layout() {
        let source = r#"C4Container
Container_Boundary(app, "DocCrate") {
  Container(parser, "Parser", "pulldown-cmark", "Builds blocks")
  ContainerDb(cache, "Layout Cache", "Memory", "Stores parsed layouts")
}
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::C4(db) = diagram else {
            panic!("expected C4 diagram");
        };
        let graph = build_with_overrides(&db, &ManualLayoutOverrides::default());
        assert_eq!(graph.boundaries.len(), 1);
        assert_eq!(graph.elements.len(), 2);
        assert!(graph.elements.iter().any(|e| e.shape == C4Shape::Database));
    }

    #[test]
    fn applies_manual_layout_overrides() {
        let source = r#"C4Container
title Manual C4
Container_Boundary(app, "Runtime") {
  Container(api, "API", "Rust", "Handles requests")
  ContainerDb(db, "Store", "SQLite", "Persists data")
}
Rel(api, db, "Reads and writes", "SQL")
%% @node api x=60 y=90 w=190 h=105
%% @node db x=360 y=92 w=190 h=105
%% @group app x=30 y=40 w=560 h=210
%% @edge api->db bend_points="285,142" label_pos="305,112"
%% @graph w=650 h=300
"#;
        let diagram = selkie::parse(source).unwrap();
        let selkie::diagrams::Diagram::C4(db) = diagram else {
            panic!("expected C4 diagram");
        };
        let overrides = crate::mermaid::manual_layout::parse(source).unwrap();
        let graph = build_with_overrides(&db, &overrides);

        let api = graph.elements.iter().find(|e| e.alias == "api").unwrap();
        let db = graph.elements.iter().find(|e| e.alias == "db").unwrap();
        let app = graph.boundaries.iter().find(|b| b.alias == "app").unwrap();
        assert_eq!((api.x, api.y, api.w, api.h), (60.0, 90.0, 190.0, 105.0));
        assert_eq!((db.x, db.y, db.w, db.h), (360.0, 92.0, 190.0, 105.0));
        assert_eq!((app.x, app.y, app.w, app.h), (30.0, 40.0, 560.0, 210.0));
        assert_eq!(graph.relationships[0].points[1], (285.0, 142.0));
        assert_eq!(graph.relationships[0].label_pos, Some((305.0, 112.0)));
        assert_eq!((graph.width, graph.height), (650.0, 300.0));
    }
}
