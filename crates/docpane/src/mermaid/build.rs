//! `mermaid::build` — run selkie's parse + layout + annotation overrides on a
//! mermaid source string and produce a doccrate-owned [`Graph`].
//!
//! After this step, the IR holds every property the renderer needs, including
//! colours / strokes / arrow heads / line styles resolved from `@annotation`
//! comments. Selkie's own types are not exposed beyond this module.

use crate::mermaid::architecture as architecture_diagram;
use crate::mermaid::c4 as c4_diagram;
use crate::mermaid::class as class_diagram;
use crate::mermaid::er as er_diagram;
use crate::mermaid::gantt as gantt_diagram;
use crate::mermaid::git as git_diagram;
use crate::mermaid::ir::*;
use crate::mermaid::journey as journey_diagram;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::mermaid::sequence;
use crate::mermaid::timeline as timeline_diagram;
use crate::theme;

use std::collections::HashMap;

use selkie::diagrams::flowchart::{
    ArrowHead as SArrow, FlowchartDb, LabelAlign as SLabelAlign, LineStyle as SLineStyle,
};
use selkie::diagrams::state::StateDb;
use selkie::diagrams::Diagram;
use selkie::layout::{
    self, CharacterSizeEstimator, LayoutEdge, LayoutGraph, LayoutNode, NodeShape as SNodeShape,
    ToLayoutGraph,
};
use selkie::render::apply_flowchart_annotation_layout_overrides;

/// Parse a mermaid source string and produce a rendered-ready [`Graph`].
///
/// Dispatches on diagram type:
/// * `flowchart` / `graph` → selkie parse + layout + annotation overrides →
///   [`FlowchartGraph`]
/// * `sequenceDiagram` → selkie parse only (no LayoutGraph for sequences);
///   layout is computed in [`crate::mermaid::sequence::build`] →
///   [`SequenceGraph`]
/// * everything else → `Err(...)` so the caller can fall back to showing the
///   raw fenced source as a code block.
pub fn build(source: &str) -> Result<Graph, String> {
    let diagram = parse_with_doccrate_comments(source)?;
    match &diagram {
        Diagram::Architecture(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Architecture(
                architecture_diagram::build_with_overrides(db, &overrides)?,
            ))
        }
        Diagram::Flowchart(db) => Ok(Graph::Flowchart(build_flowchart(db)?)),
        Diagram::C4(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::C4(c4_diagram::build_with_overrides(db, &overrides)))
        }
        Diagram::Class(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Class(class_diagram::build_with_overrides(
                db, &overrides,
            )?))
        }
        Diagram::Er(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Er(er_diagram::build_with_overrides(db, &overrides)?))
        }
        Diagram::Gantt(db)     => {
            let mut db = db.clone();
            Ok(Graph::Gantt(gantt_diagram::build(&mut db)))
        }
        Diagram::Git(db)       => Ok(Graph::Git(git_diagram::build(db))),
        Diagram::Journey(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Journey(journey_diagram::build_with_overrides(
                db, &overrides,
            )))
        }
        Diagram::Sequence(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Sequence(sequence::build_with_overrides(
                db, &overrides,
            )))
        }
        Diagram::State(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Flowchart(build_state(db, &overrides)?))
        }
        Diagram::Timeline(db) => {
            let overrides = crate::mermaid::manual_layout::parse(source)?;
            Ok(Graph::Timeline(timeline_diagram::build_with_overrides(
                db, &overrides,
            )))
        }
        _ => Err(
            "unsupported diagram type (only flowchart, sequenceDiagram, stateDiagram, classDiagram, erDiagram, gitGraph, gantt, journey, timeline, architecture-beta, and C4 supported)"
                .to_string(),
        ),
    }
}

fn parse_with_doccrate_comments(source: &str) -> Result<Diagram, String> {
    match selkie::parse(source) {
        Ok(diagram) => Ok(diagram),
        Err(first_err) => {
            let stripped = strip_doccrate_manual_comments(source);
            if stripped == source {
                return Err(format!("parse error: {first_err}"));
            }
            selkie::parse(&stripped).map_err(|second_err| format!("parse error: {second_err}"))
        }
    }
}

fn strip_doccrate_manual_comments(source: &str) -> String {
    source
        .lines()
        .filter(|line| !is_doccrate_manual_comment(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_doccrate_manual_comment(line: &str) -> bool {
    let Some(body) = line.trim().strip_prefix("%%").map(str::trim) else {
        return false;
    };
    let Some(target) = body.strip_prefix('@') else {
        return false;
    };
    let target = target
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        target.as_str(),
        "service"
            | "junction"
            | "node"
            | "object"
            | "note"
            | "group"
            | "edge"
            | "rel"
            | "relationship"
            | "graph"
    )
}

fn build_flowchart(db: &FlowchartDb) -> Result<FlowchartGraph, String> {
    let estimator = CharacterSizeEstimator::default();
    let mut lg = db
        .to_layout_graph(&estimator)
        .map_err(|e| format!("layout-graph build: {e}"))?;
    // Hard aspect-ratio enforcement for custom shapes — must run BEFORE
    // selkie's `layout()` so edge routing sees the final node dimensions.
    enforce_custom_aspect(&mut lg);
    let mut lg = layout::layout(lg).map_err(|e| format!("layout: {e}"))?;
    apply_flowchart_annotation_layout_overrides(db, &mut lg);
    Ok(convert(db, &lg))
}

/// State diagrams (`stateDiagram-v2`). Selkie already maps every
/// `StateType` to an appropriate `NodeShape` (Start → Circle, End →
/// DoubleCircle, Fork/Join → HorizontalBar, Choice → Diamond, default →
/// RoundedRect), so we reuse the `FlowchartGraph` IR and the same renderer.
/// `@annotation` overrides are flowchart-only at the selkie layer, so
/// state diagrams render with theme defaults.
fn build_state(db: &StateDb, overrides: &ManualLayoutOverrides) -> Result<FlowchartGraph, String> {
    let estimator = CharacterSizeEstimator::default();
    let mut lg = db
        .to_layout_graph(&estimator)
        .map_err(|e| format!("layout-graph build: {e}"))?;
    enforce_custom_aspect(&mut lg);
    let lg = layout::layout(lg).map_err(|e| format!("layout: {e}"))?;
    Ok(convert_state(&lg, overrides))
}

/// Annotation-free LayoutGraph → FlowchartGraph conversion. Used by
/// state diagrams (and any future diagram type that produces a vanilla
/// LayoutGraph without an annotation database).
fn convert_state(lg: &LayoutGraph, overrides: &ManualLayoutOverrides) -> FlowchartGraph {
    let bx = lg.bounds_x.unwrap_or(0.0) as f32;
    let by = lg.bounds_y.unwrap_or(0.0) as f32;
    let mut w = lg.width.unwrap_or(0.0) as f32;
    let mut h = lg.height.unwrap_or(0.0) as f32;

    let mut groups: Vec<Group> = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut rects = HashMap::new();
    collect_state_nodes(
        &lg.nodes,
        bx,
        by,
        overrides,
        &mut rects,
        &mut groups,
        &mut nodes,
    );

    let mut edges = Vec::new();
    for edge in &lg.edges {
        if let Some(e) = convert_state_edge(edge, bx, by, overrides, &rects) {
            edges.push(e);
        }
    }
    grow_state_bounds(&mut w, &mut h, &groups, &nodes, &edges);
    if let Some(ow) = overrides.graph.w {
        w = ow;
    }
    if let Some(oh) = overrides.graph.h {
        h = oh;
    }

    FlowchartGraph {
        width: w.max(1.0),
        height: h.max(1.0),
        background: None,
        groups,
        nodes,
        edges,
    }
}

/// Walk LayoutGraph nodes. Selkie's state adapter keeps everything flat in
/// `lg.nodes` (composite parents and their children all at the top level)
/// and tags composites via `metadata["is_group"] = "true"`. We honour that
/// tag here. The `children` array on `LayoutNode` is empty for state
/// graphs, but we still recurse into it defensively in case some future
/// adapter nests them.
fn collect_state_nodes(
    nodes: &[LayoutNode],
    bx: f32,
    by: f32,
    overrides: &ManualLayoutOverrides,
    rects: &mut HashMap<String, StateRect>,
    groups: &mut Vec<Group>,
    out: &mut Vec<Node>,
) {
    for n in nodes {
        if n.is_dummy {
            continue;
        }
        let (x, y) = node_origin(n, bx, by);
        let is_group = n
            .metadata
            .get("is_group")
            .map(|s| s == "true")
            .unwrap_or(false);
        let mut x = x;
        let mut y = y;
        let mut w = n.width as f32;
        let mut h = n.height as f32;
        if let Some(ov) = state_box_override(n, is_group, overrides) {
            apply_state_box_override(&mut x, &mut y, &mut w, &mut h, ov);
        }
        rects.insert(n.id.clone(), StateRect { x, y, w, h });
        if is_group {
            groups.push(Group {
                x,
                y,
                w,
                h,
                title: n.label.clone(),
                fill: theme::MERMAID_GROUP_FILL,
                stroke: theme::MERMAID_GROUP_STROKE,
                stroke_w: theme::MERMAID_GROUP_STROKE_W,
                title_font_size: theme::MERMAID_GROUP_FONT_SIZE,
                title_color: theme::MERMAID_GROUP_TITLE,
            });
        } else {
            // Leaf state. Start markers paint as a solid dot
            // (`fill = stroke`) per mermaid convention. End markers keep
            // the default theme fill — the inner circle of `DoubleCircle`
            // gives the visual cue.
            let state_type = n
                .metadata
                .get("state_type")
                .map(|s| s.as_str())
                .unwrap_or("");
            let fill = if state_type == "Start" {
                theme::MERMAID_NODE_STROKE
            } else {
                theme::MERMAID_NODE_FILL
            };
            out.push(Node {
                x,
                y,
                w,
                h,
                shape: convert_shape(n.shape, &n.metadata),
                label: n.label.clone().unwrap_or_default(),
                label_align: Align::Center,
                fill,
                stroke: theme::MERMAID_NODE_STROKE,
                stroke_w: theme::MERMAID_NODE_STROKE_W,
                text_color: theme::MERMAID_NODE_TEXT,
                font_size: theme::MERMAID_NODE_FONT_SIZE,
                bold: false,
            });
        }
        if !n.children.is_empty() {
            collect_state_nodes(&n.children, bx, by, overrides, rects, groups, out);
        }
    }
}

/// LayoutEdge → IR Edge with theme defaults. No annotation lookup.
fn convert_state_edge(
    edge: &LayoutEdge,
    bx: f32,
    by: f32,
    overrides: &ManualLayoutOverrides,
    rects: &HashMap<String, StateRect>,
) -> Option<Edge> {
    if edge.bend_points.len() < 2 {
        return None;
    }
    let source = edge.source().unwrap_or("");
    let target = edge.target().unwrap_or("");
    let edge_override = overrides.edge(source, target);
    let layout_points: Vec<(f32, f32)> = edge
        .bend_points
        .iter()
        .map(|p| (p.x as f32 - bx, p.y as f32 - by))
        .collect();
    let auto_points = if state_endpoint_was_overridden(source, target, overrides) {
        state_direct_edge_points(source, target, rects).unwrap_or_else(|| layout_points.clone())
    } else {
        layout_points.clone()
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
    let mut label = match (&edge.label, edge.label_position) {
        (Some(text), Some(pos)) if !text.is_empty() => {
            let (w, h) = edge_label_box(text, edge.label_width as f32, edge.label_height as f32);
            Some(EdgeLabel {
                x: (pos.x as f32 - bx) - w / 2.0,
                y: (pos.y as f32 - by) - h / 2.0,
                w,
                h,
                text: text.clone(),
                text_color: theme::MERMAID_EDGE_LABEL,
                font_size: theme::MERMAID_EDGE_FONT_SIZE,
            })
        }
        _ => None,
    };
    if let (Some(label), Some(ov)) = (&mut label, edge_override) {
        if let Some((x, y)) = ov.label_pos {
            label.x = x - label.w / 2.0;
            label.y = y - label.h / 2.0;
        } else if ov.label_offset.is_some() || ov.points.is_some() || !ov.bend_points.is_empty() {
            let (cx, cy) = state_polyline_midpoint(&points);
            let (dx, dy) = ov.label_offset.unwrap_or((0.0, 0.0));
            label.x = cx - label.w / 2.0 + dx;
            label.y = cy - label.h / 2.0 + dy;
        }
    }
    Some(Edge {
        points,
        line_color: theme::MERMAID_EDGE,
        line_w: theme::MERMAID_EDGE_W,
        line_style: LineStyle::Solid,
        start_arrow: Arrow::None,
        end_arrow: Arrow::Triangle,
        label,
    })
}

#[derive(Debug, Clone, Copy)]
struct StateRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn state_box_override<'a>(
    node: &LayoutNode,
    is_group: bool,
    overrides: &'a ManualLayoutOverrides,
) -> Option<&'a BoxOverride> {
    if is_group {
        overrides.group(&node.id).or_else(|| {
            node.label
                .as_deref()
                .and_then(|label| overrides.group(label))
        })
    } else {
        overrides.object(&node.id).or_else(|| {
            node.label
                .as_deref()
                .and_then(|label| overrides.object(label))
        })
    }
}

fn apply_state_box_override(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, ov: &BoxOverride) {
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

fn state_endpoint_was_overridden(
    source: &str,
    target: &str,
    overrides: &ManualLayoutOverrides,
) -> bool {
    overrides.object(source).is_some()
        || overrides.object(target).is_some()
        || overrides.group(source).is_some()
        || overrides.group(target).is_some()
}

fn state_direct_edge_points(
    source: &str,
    target: &str,
    rects: &HashMap<String, StateRect>,
) -> Option<Vec<(f32, f32)>> {
    let a = *rects.get(source)?;
    let b = *rects.get(target)?;
    let ac = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let bc = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    Some(vec![
        state_rect_intersection(a, bc),
        state_rect_intersection(b, ac),
    ])
}

fn state_rect_intersection(rect: StateRect, toward: (f32, f32)) -> (f32, f32) {
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

fn state_polyline_midpoint(points: &[(f32, f32)]) -> (f32, f32) {
    if points.len() < 2 {
        return points.first().copied().unwrap_or((0.0, 0.0));
    }
    let mut total = 0.0;
    for pair in points.windows(2) {
        total += state_distance(pair[0], pair[1]);
    }
    if total <= 0.0 {
        return points[0];
    }
    let target = total / 2.0;
    let mut walked = 0.0;
    for pair in points.windows(2) {
        let len = state_distance(pair[0], pair[1]);
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

fn state_distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    (dx * dx + dy * dy).sqrt()
}

fn edge_label_box(text: &str, width: f32, height: f32) -> (f32, f32) {
    let min_w = text.chars().count() as f32 * theme::MERMAID_EDGE_FONT_SIZE * 0.62 + 10.0;
    let min_h = theme::MERMAID_EDGE_FONT_SIZE * theme::LINE_EXTRA + 4.0;
    (width.max(min_w), height.max(min_h))
}

fn grow_state_bounds(
    width: &mut f32,
    height: &mut f32,
    groups: &[Group],
    nodes: &[Node],
    edges: &[Edge],
) {
    for group in groups {
        *width = (*width).max(group.x + group.w);
        *height = (*height).max(group.y + group.h);
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
        if let Some(label) = &edge.label {
            *width = (*width).max(label.x + label.w);
            *height = (*height).max(label.y + label.h);
        }
    }
}

/// Resize `NodeShape::Custom` nodes so their width/height match the
/// `aspect` declared in the shape file. Grows the smaller dimension so we
/// never shrink under the size estimator's text-based minimum.
fn enforce_custom_aspect(lg: &mut LayoutGraph) {
    let reg = crate::mermaid::shape_def::registry();
    lg.traverse_nodes_mut(|n| {
        if !matches!(n.shape, SNodeShape::Custom) {
            return;
        }
        let name = match n.metadata.get("shape") {
            Some(s) => s.as_str(),
            None => return,
        };
        let aspect = match reg
            .lookup(name)
            .and_then(|i| reg.get(i))
            .and_then(|d| d.aspect)
        {
            Some(a) => a as f64,
            None => return,
        };
        if n.height <= 0.0 {
            return;
        }
        let cur = n.width / n.height;
        if cur < aspect {
            n.width = n.height * aspect;
        } else if cur > aspect {
            n.height = n.width / aspect;
        }
    });
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

fn convert(db: &FlowchartDb, lg: &LayoutGraph) -> FlowchartGraph {
    let graph_overrides = db.graph_annotation_overrides();
    let background = graph_overrides.canvas_fill.as_deref().and_then(parse_hex);

    let bx = lg.bounds_x.unwrap_or(0.0) as f32;
    let by = lg.bounds_y.unwrap_or(0.0) as f32;
    let w = lg.width.unwrap_or(0.0) as f32;
    let h = lg.height.unwrap_or(0.0) as f32;

    // Default font sizes from `@graph` if present.
    let node_font = graph_overrides
        .node_label_font_size
        .map(|v| v as f32)
        .unwrap_or(theme::MERMAID_NODE_FONT_SIZE);
    let edge_font = graph_overrides
        .edge_label_font_size
        .map(|v| v as f32)
        .unwrap_or(theme::MERMAID_EDGE_FONT_SIZE);
    let group_font = graph_overrides
        .group_label_font_size
        .map(|v| v as f32)
        .unwrap_or(theme::MERMAID_GROUP_FONT_SIZE);
    let default_node_align = graph_overrides
        .node_label_align
        .map(convert_align)
        .unwrap_or(Align::Center);

    // Subgraphs / groups come first so the renderer can paint them under the nodes.
    let mut groups = Vec::new();
    for sg in db.subgraphs() {
        if let Some(node) = lg.get_node(&sg.id) {
            let (x, y) = node_origin(node, bx, by);
            let ov = db.group_annotation_overrides(&sg.id);
            let fill = ov
                .fill
                .as_deref()
                .and_then(parse_hex)
                .unwrap_or(theme::MERMAID_GROUP_FILL);
            let stroke = ov
                .stroke
                .as_deref()
                .and_then(parse_hex)
                .unwrap_or(theme::MERMAID_GROUP_STROKE);
            let title = if sg.title.is_empty() {
                None
            } else {
                Some(sg.title.clone())
            };
            groups.push(Group {
                x,
                y,
                w: node.width as f32,
                h: node.height as f32,
                title,
                fill,
                stroke,
                stroke_w: theme::MERMAID_GROUP_STROKE_W,
                title_font_size: group_font,
                title_color: theme::MERMAID_GROUP_TITLE,
            });
        }
    }

    // Walk all visible (non-dummy, non-subgraph) nodes recursively.
    let mut nodes = Vec::new();
    let subgraph_ids: std::collections::HashSet<&str> =
        db.subgraphs().iter().map(|s| s.id.as_str()).collect();
    collect_nodes(
        &lg.nodes,
        &subgraph_ids,
        db,
        bx,
        by,
        node_font,
        default_node_align,
        &mut nodes,
    );

    // Edges.
    let mut edges = Vec::new();
    for edge in &lg.edges {
        if let Some(e) = convert_edge(edge, db, bx, by, edge_font) {
            edges.push(e);
        }
    }

    FlowchartGraph {
        width: w.max(1.0),
        height: h.max(1.0),
        background,
        groups,
        nodes,
        edges,
    }
}

fn collect_nodes(
    nodes: &[LayoutNode],
    subgraph_ids: &std::collections::HashSet<&str>,
    db: &FlowchartDb,
    bx: f32,
    by: f32,
    default_font: f32,
    default_align: Align,
    out: &mut Vec<Node>,
) {
    for node in nodes {
        if !node.is_dummy && !subgraph_ids.contains(node.id.as_str()) {
            out.push(convert_node(node, db, bx, by, default_font, default_align));
        }
        if !node.children.is_empty() {
            collect_nodes(
                &node.children,
                subgraph_ids,
                db,
                bx,
                by,
                default_font,
                default_align,
                out,
            );
        }
    }
}

fn convert_node(
    node: &LayoutNode,
    db: &FlowchartDb,
    bx: f32,
    by: f32,
    default_font: f32,
    default_align: Align,
) -> Node {
    let (x, y) = node_origin(node, bx, by);
    let ov = db.node_annotation_overrides(&node.id);

    let fill = ov
        .fill
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(theme::MERMAID_NODE_FILL);
    let stroke = ov
        .stroke
        .as_deref()
        .and_then(parse_hex)
        .unwrap_or(theme::MERMAID_NODE_STROKE);
    let stroke_w = ov
        .line_width
        .map(|v| v as f32)
        .unwrap_or(theme::MERMAID_NODE_STROKE_W);
    let label_align = ov.label_align.map(convert_align).unwrap_or(default_align);

    Node {
        x,
        y,
        w: node.width as f32,
        h: node.height as f32,
        shape: convert_shape(node.shape, &node.metadata),
        label: node.label.clone().unwrap_or_default(),
        label_align,
        fill,
        stroke,
        stroke_w,
        text_color: theme::MERMAID_NODE_TEXT,
        font_size: default_font,
        bold: false,
    }
}

fn convert_edge(
    edge: &LayoutEdge,
    db: &FlowchartDb,
    bx: f32,
    by: f32,
    edge_font: f32,
) -> Option<Edge> {
    // Skip degenerate edges that the layout couldn't route.
    if edge.bend_points.len() < 2 {
        return None;
    }
    let points: Vec<(f32, f32)> = edge
        .bend_points
        .iter()
        .map(|p| (p.x as f32 - bx, p.y as f32 - by))
        .collect();

    // Locate the originating flow-edge to query its annotation overrides.
    let flow_edge = db.edges().iter().find(|fe| {
        fe.id.as_deref() == Some(edge.id.as_str())
            || (fe.start == edge.source().unwrap_or("") && fe.end == edge.target().unwrap_or(""))
    });
    let ov = flow_edge.map(|fe| db.edge_annotation_overrides_for(fe));

    let line_color = ov
        .as_ref()
        .and_then(|o| o.line_color.as_deref().and_then(parse_hex))
        .unwrap_or(theme::MERMAID_EDGE);
    let line_w = ov
        .as_ref()
        .and_then(|o| o.line_width.map(|v| v as f32))
        .unwrap_or(theme::MERMAID_EDGE_W);
    let line_style = ov
        .as_ref()
        .and_then(|o| o.line_style.map(convert_line_style))
        .unwrap_or(LineStyle::Solid);
    let start_arrow = ov
        .as_ref()
        .and_then(|o| o.start_arrow.map(convert_arrow))
        .unwrap_or(Arrow::None);
    let end_arrow = ov
        .as_ref()
        .and_then(|o| o.end_arrow.map(convert_arrow))
        .unwrap_or(Arrow::Triangle);

    let label = match (&edge.label, edge.label_position) {
        (Some(text), Some(pos)) if !text.is_empty() => Some(EdgeLabel {
            x: (pos.x as f32 - bx) - (edge.label_width as f32) / 2.0,
            y: (pos.y as f32 - by) - (edge.label_height as f32) / 2.0,
            w: edge.label_width as f32,
            h: edge.label_height as f32,
            text: text.clone(),
            text_color: theme::MERMAID_EDGE_LABEL,
            font_size: edge_font,
        }),
        _ => None,
    };

    Some(Edge {
        points,
        line_color,
        line_w,
        line_style,
        start_arrow,
        end_arrow,
        label,
    })
}

// ---------------------------------------------------------------------------
// Small mapping helpers
// ---------------------------------------------------------------------------

fn node_origin(node: &LayoutNode, bx: f32, by: f32) -> (f32, f32) {
    let x = node.x.unwrap_or(0.0) as f32 - bx;
    let y = node.y.unwrap_or(0.0) as f32 - by;
    (x, y)
}

fn convert_shape(s: SNodeShape, metadata: &std::collections::HashMap<String, String>) -> Shape {
    match s {
        SNodeShape::Rectangle => Shape::Rect,
        SNodeShape::RoundedRect => Shape::RoundedRect,
        SNodeShape::Stadium => Shape::Stadium,
        SNodeShape::Circle => Shape::Circle,
        SNodeShape::DoubleCircle => Shape::DoubleCircle,
        SNodeShape::Ellipse => Shape::Ellipse,
        SNodeShape::Diamond => Shape::Diamond,
        SNodeShape::Hexagon => Shape::Hexagon,
        SNodeShape::Cylinder => Shape::Cylinder,
        SNodeShape::Subroutine => Shape::Subroutine,
        SNodeShape::Trapezoid => Shape::Trapezoid,
        SNodeShape::InvTrapezoid => Shape::InvTrapezoid,
        SNodeShape::LeanRight => Shape::LeanRight,
        SNodeShape::LeanLeft => Shape::LeanLeft,
        SNodeShape::Odd => Shape::Odd,
        SNodeShape::HorizontalBar => Shape::HorizontalBar,
        SNodeShape::Custom => {
            // Resolve @{ shape: name } against the registry. If the name is
            // unknown (typo or shape not shipped), fall back to a rectangle
            // so the node still appears on screen.
            let name = metadata.get("shape").map(|s| s.as_str()).unwrap_or("");
            match crate::mermaid::shape_def::registry().lookup(name) {
                Some(idx) => Shape::Custom(idx),
                None => Shape::Rect,
            }
        }
    }
}

fn convert_align(a: SLabelAlign) -> Align {
    match a {
        SLabelAlign::Left => Align::Left,
        SLabelAlign::Center => Align::Center,
        SLabelAlign::Right => Align::Right,
    }
}

fn convert_line_style(s: SLineStyle) -> LineStyle {
    match s {
        SLineStyle::Solid => LineStyle::Solid,
        SLineStyle::Dash => LineStyle::Dash,
        SLineStyle::Dot => LineStyle::Dot,
    }
}

fn convert_arrow(a: SArrow) -> Arrow {
    match a {
        SArrow::None => Arrow::None,
        SArrow::Point => Arrow::Triangle,
        SArrow::Circle => Arrow::Circle,
        SArrow::Cross => Arrow::Cross,
    }
}

/// Parse a CSS-like hex colour. Accepts `#RGB`, `#RRGGBB`, or the same without
/// the leading `#`. Returns `None` on any malformed input — the caller falls
/// back to a theme default.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_forms() {
        assert_eq!(parse_hex("#abc"), Some(0xAABBCC));
        assert_eq!(parse_hex("aabbcc"), Some(0xAABBCC));
        assert_eq!(parse_hex("#AABBCC"), Some(0xAABBCC));
        assert_eq!(parse_hex("zzz"), None);
    }

    #[test]
    fn builds_state_with_manual_layout_overrides() {
        let source = r#"stateDiagram-v2
    [*] --> Idle
    Idle --> Loading : fetch
    Loading --> Success : 200
    Loading --> Error : failed
    %% @node Idle x=60 y=80 w=120 h=56
    %% @node Loading x=245 y=80 w=130 h=56
    %% @node Success x=450 y=40 w=130 h=56
    %% @node Error x=450 y=140 w=130 h=56
    %% @edge Idle->Loading points="180,108 245,108" label_offset="0,-10"
    %% @edge Loading->Error bend_points="415,108 415,168" label_pos="410,140"
    %% @graph w=640 h=260
"#;
        let graph = match build(source).unwrap() {
            Graph::Flowchart(graph) => graph,
            other => panic!("expected flowchart graph, got {other:?}"),
        };

        let idle = graph.nodes.iter().find(|n| n.label == "Idle").unwrap();
        let loading = graph.nodes.iter().find(|n| n.label == "Loading").unwrap();
        assert_eq!((idle.x, idle.y, idle.w, idle.h), (60.0, 80.0, 120.0, 56.0));
        assert_eq!(
            (loading.x, loading.y, loading.w, loading.h),
            (245.0, 80.0, 130.0, 56.0)
        );
        let fetch = graph
            .edges
            .iter()
            .find(|edge| {
                edge.label
                    .as_ref()
                    .is_some_and(|label| label.text == "fetch")
            })
            .unwrap();
        assert_eq!(fetch.points, vec![(180.0, 108.0), (245.0, 108.0)]);
        let label = fetch.label.as_ref().unwrap();
        assert!(((label.y + label.h / 2.0) - 98.0).abs() < 0.1);
        assert_eq!((graph.width, graph.height), (640.0, 260.0));
    }
}
