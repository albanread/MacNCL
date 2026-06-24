//! Rendering engine for mermaid diagrams
//!
//! This module provides SVG rendering for positioned diagram elements.

mod architecture;
pub mod ascii;
mod block;
mod c4;
mod class;
mod er;
mod flowchart;
mod gantt;
mod git;
mod journey;
mod kanban;
mod mindmap;
mod packet;
mod pie;
mod quadrant;
mod radar;
mod requirement;
mod sankey;
mod sequence;
mod state;
pub mod svg;
pub(crate) mod text_utils;
mod timeline;
mod treemap;
mod xychart;

use crate::diagrams::{detect_init, detect_type, parse, remove_directives, Diagram};
use crate::error::{MermaidError, Result};
use crate::layout::{
    self, geometric_midpoint, CharacterSizeEstimator, LayoutGraph, Point, ToLayoutGraph,
};
use std::collections::{HashMap, HashSet};

pub use svg::{RenderConfig, SvgRenderer, Theme};

/// Render a diagram to SVG
pub fn render(diagram: &Diagram) -> Result<String> {
    render_with_config(diagram, &RenderConfig::default())
}

/// Render a diagram to PNG bytes.
///
/// This uses the same annotation-aware SVG renderer as [`render`] and then
/// rasterizes the resulting SVG with default intrinsic sizing.
#[cfg(feature = "png")]
pub fn render_png(diagram: &Diagram) -> Result<Vec<u8>> {
    render_png_with_config(diagram, &RenderConfig::default())
}

/// Render diagram text to SVG with automatic directive processing
///
/// This function:
/// 1. Detects and parses `%%{init: ...}%%` directives
/// 2. Extracts theme configuration from directives
/// 3. Detects the diagram type
/// 4. Parses the diagram
/// 5. Renders with directive-derived theme configuration
///
/// # Example
///
/// ```
/// use selkie::render::render_text;
///
/// let svg = render_text(r#"%%{init: {"theme": "dark"}}%%
/// flowchart TD
///     A[Start] --> B[End]
/// "#).unwrap();
/// assert!(svg.contains("<svg"));
/// ```
pub fn render_text(text: &str) -> Result<String> {
    // Extract directive configuration
    let directive_config = detect_init(text);

    // Build render config with directive theme and themeCSS
    let config = if let Some(ref dc) = directive_config {
        RenderConfig {
            theme: Theme::from_directive(dc),
            theme_css: dc.theme_css.clone(),
            ..RenderConfig::default()
        }
    } else {
        RenderConfig::default()
    };

    // Remove directives from text before parsing
    let clean_text = remove_directives(text);

    // Detect diagram type and parse
    let diagram_type = detect_type(&clean_text)?;
    let diagram = parse(diagram_type, &clean_text)?;

    // Render with config
    render_with_config(&diagram, &config)
}

/// Render diagram text directly to PNG bytes with automatic directive processing.
///
/// This mirrors [`render_text`] and uses the same directive/theme handling before
/// rasterizing the rendered SVG.
#[cfg(feature = "png")]
pub fn render_text_png(text: &str) -> Result<Vec<u8>> {
    let svg = render_text(text)?;
    svg_to_png_with_size(&svg, None, None)
}

/// Render a diagram to SVG with custom configuration
pub fn render_with_config(diagram: &Diagram, config: &RenderConfig) -> Result<String> {
    match diagram {
        Diagram::Architecture(db) => render_architecture(db, config),
        Diagram::Block(db) => block::render_block(db, config),
        Diagram::C4(db) => c4::render_c4(db, config),
        Diagram::Flowchart(db) => render_flowchart(db, config),
        Diagram::Git(db) => git::render_git(db, config),
        Diagram::Pie(db) => pie::render_pie(db, config),
        Diagram::Sequence(db) => sequence::render_sequence(db, config),
        Diagram::Class(db) => class::render_class(db, config),
        Diagram::State(db) => state::render_state(db, config),
        Diagram::Er(db) => er::render_er(db, config),
        Diagram::Gantt(db) => {
            let mut db_clone = db.clone();
            gantt::render_gantt(&mut db_clone, config)
        }
        Diagram::Mindmap(db) => mindmap::render_mindmap(db, config),
        Diagram::Timeline(db) => timeline::render_timeline(db, config),
        Diagram::Requirement(db) => requirement::render_requirement(db, config),
        Diagram::Sankey(db) => sankey::render_sankey(db, config),
        Diagram::Radar(db) => radar::render_radar(db, config),
        Diagram::Packet(db) => packet::render_packet(db, config),
        Diagram::XyChart(db) => xychart::render_xychart(db, config),
        Diagram::Quadrant(db) => quadrant::render_quadrant(db, config),
        Diagram::Treemap(db) => treemap::render_treemap(db, config),
        Diagram::Journey(db) => journey::render_journey(db, config),
        Diagram::Kanban(db) => kanban::render_kanban(db, config),
        _ => Err(MermaidError::RenderError(format!(
            "Diagram type {:?} not yet supported for rendering",
            diagram_type_name(diagram)
        ))),
    }
}

/// Render a diagram to PNG bytes with custom configuration.
///
/// This uses the annotation-aware SVG renderer and rasterizes the generated SVG
/// at its intrinsic size.
#[cfg(feature = "png")]
pub fn render_png_with_config(diagram: &Diagram, config: &RenderConfig) -> Result<Vec<u8>> {
    let svg = render_with_config(diagram, config)?;
    svg_to_png_with_size(&svg, None, None)
}

/// Rasterize SVG into PNG bytes, optionally forcing the output dimensions.
///
/// When only one dimension is provided, the other is derived from the SVG's
/// intrinsic aspect ratio.
#[cfg(feature = "png")]
pub fn svg_to_png_with_size(svg: &str, width: Option<u32>, height: Option<u32>) -> Result<Vec<u8>> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let mut opt = usvg::Options::default();
    let fontdb = opt.fontdb_mut();
    fontdb.load_system_fonts();
    fontdb.set_sans_serif_family("Arial");
    fontdb.set_serif_family("Times New Roman");
    fontdb.set_monospace_family("Courier New");

    let tree = usvg::Tree::from_str(svg, &opt)
        .map_err(|e| MermaidError::RenderError(format!("Failed to parse SVG: {}", e)))?;

    let svg_size = tree.size();
    let (target_width, target_height) = match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => {
            let scale = w as f32 / svg_size.width();
            (w, (svg_size.height() * scale) as u32)
        }
        (None, Some(h)) => {
            let scale = h as f32 / svg_size.height();
            ((svg_size.width() * scale) as u32, h)
        }
        (None, None) => (svg_size.width() as u32, svg_size.height() as u32),
    };

    let mut pixmap = tiny_skia::Pixmap::new(target_width, target_height)
        .ok_or_else(|| MermaidError::RenderError("Failed to create pixmap".to_string()))?;

    let scale_x = target_width as f32 / svg_size.width();
    let scale_y = target_height as f32 / svg_size.height();
    let transform = tiny_skia::Transform::from_scale(scale_x, scale_y);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    pixmap
        .encode_png()
        .map_err(|e| MermaidError::RenderError(format!("Failed to encode PNG: {}", e)))
}

/// Get the name of the diagram type for error messages
fn diagram_type_name(diagram: &Diagram) -> &'static str {
    match diagram {
        Diagram::Architecture(_) => "Architecture",
        Diagram::Block(_) => "Block",
        Diagram::C4(_) => "C4",
        Diagram::Class(_) => "Class",
        Diagram::Er(_) => "ER",
        Diagram::Flowchart(_) => "Flowchart",
        Diagram::Gantt(_) => "Gantt",
        Diagram::Git(_) => "Git",
        Diagram::Info(_) => "Info",
        Diagram::Journey(_) => "Journey",
        Diagram::Kanban(_) => "Kanban",
        Diagram::Mindmap(_) => "Mindmap",
        Diagram::Packet(_) => "Packet",
        Diagram::Pie(_) => "Pie",
        Diagram::Quadrant(_) => "Quadrant",
        Diagram::Radar(_) => "Radar",
        Diagram::Requirement(_) => "Requirement",
        Diagram::Sankey(_) => "Sankey",
        Diagram::Sequence(_) => "Sequence",
        Diagram::State(_) => "State",
        Diagram::Timeline(_) => "Timeline",
        Diagram::Treemap(_) => "Treemap",
        Diagram::XyChart(_) => "XyChart",
    }
}

/// Render a flowchart diagram
fn render_flowchart(
    db: &crate::diagrams::flowchart::FlowchartDb,
    config: &RenderConfig,
) -> Result<String> {
    let size_estimator = CharacterSizeEstimator::default();

    // Convert to layout graph
    let graph = db.to_layout_graph(&size_estimator)?;

    // Run layout algorithm
    let mut graph = layout::layout(graph)?;
    apply_flowchart_annotation_layout_overrides(db, &mut graph);

    // Render to SVG
    let renderer = SvgRenderer::new(config.clone());
    renderer.render_flowchart(db, &graph)
}

const FLOWCHART_GROUP_PADDING: f64 = 20.0;
const FLOWCHART_GROUP_TITLE_HEIGHT: f64 = 25.0;

/// Apply flowchart `@annotation` overrides (positions, sizes, edge routing) to a
/// previously laid-out [`LayoutGraph`]. Public so external renderers (e.g. a Direct2D
/// renderer in another binary) can re-use selkie's parse + layout pipeline and then
/// pick up the annotation-resolved geometry without going through SVG.
pub fn apply_flowchart_annotation_layout_overrides(
    db: &crate::diagrams::flowchart::FlowchartDb,
    graph: &mut LayoutGraph,
) {
    let mut moved_nodes = HashSet::new();
    let group_boxes_before: HashMap<String, (f64, f64, f64, f64)> = db
        .subgraphs()
        .iter()
        .filter_map(|subgraph| {
            flowchart_group_box(graph, subgraph).map(|bounds| (subgraph.id.clone(), bounds))
        })
        .collect();

    for subgraph in db.subgraphs() {
        let overrides = db.group_annotation_overrides(&subgraph.id);
        if let Some(group_node) = graph.get_node_mut(&subgraph.id) {
            if let Some(width) = overrides.width {
                group_node.width = width.max(1.0);
            }
            if let Some(height) = overrides.height {
                group_node.height = height.max(1.0);
            }
        }
    }

    for node_id in db.vertices().keys() {
        let overrides = db.node_annotation_overrides(node_id);
        if let Some(node) = graph.get_node_mut(node_id) {
            if let Some(width) = overrides.width {
                node.width = width.max(1.0);
                moved_nodes.insert(node_id.clone());
            }
            if let Some(height) = overrides.height {
                node.height = height.max(1.0);
                moved_nodes.insert(node_id.clone());
            }
        }
    }

    for subgraph in db.subgraphs() {
        let Some((before_x, before_y, _, _)) = group_boxes_before.get(&subgraph.id).copied() else {
            continue;
        };
        let overrides = db.group_annotation_overrides(&subgraph.id);
        let target_group_x = overrides.x.unwrap_or(before_x);
        let target_group_y = overrides.y.unwrap_or(before_y);

        if let Some(group_node) = graph.get_node_mut(&subgraph.id) {
            if overrides.x.is_some() {
                group_node.x = Some(target_group_x);
            }
            if overrides.y.is_some() {
                group_node.y = Some(target_group_y);
            }
        }

        let current_group_x = graph
            .get_node(&subgraph.id)
            .and_then(|node| node.x)
            .unwrap_or(target_group_x);
        let current_group_y = graph
            .get_node(&subgraph.id)
            .and_then(|node| node.y)
            .unwrap_or(target_group_y);
        let before_content_origin = flowchart_group_content_origin(before_x, before_y);
        let target_content_origin =
            flowchart_group_content_origin(current_group_x, current_group_y);
        let group_shift = Point::new(
            target_content_origin.x - before_content_origin.x,
            target_content_origin.y - before_content_origin.y,
        );
        let group_moved = group_shift.x.abs() > 0.001 || group_shift.y.abs() > 0.001;

        for node_id in &subgraph.nodes {
            let node_overrides = db.node_annotation_overrides(node_id);
            let Some(node) = graph.get_node_mut(node_id) else {
                continue;
            };

            let current_x = node.x.unwrap_or(target_content_origin.x);
            let current_y = node.y.unwrap_or(target_content_origin.y);
            let current_local_x = current_x - before_content_origin.x;
            let current_local_y = current_y - before_content_origin.y;

            if node_overrides.x.is_some() || node_overrides.y.is_some() || group_moved {
                let next_local_x = node_overrides.x.unwrap_or(current_local_x);
                let next_local_y = node_overrides.y.unwrap_or(current_local_y);
                node.x = Some(target_content_origin.x + next_local_x);
                node.y = Some(target_content_origin.y + next_local_y);
                moved_nodes.insert(node_id.clone());
            }
        }
    }

    for node_id in db.vertices().keys() {
        let overrides = db.node_annotation_overrides(node_id);
        if overrides.x.is_none() && overrides.y.is_none() {
            continue;
        }
        let Some(node) = graph.get_node_mut(node_id) else {
            continue;
        };
        if node.parent_id.is_some() {
            continue;
        }
        if let Some(x) = overrides.x {
            node.x = Some(x);
        }
        if let Some(y) = overrides.y {
            node.y = Some(y);
        }
        moved_nodes.insert(node_id.clone());
    }

    let rerouted_edges: Vec<(usize, Vec<Point>, Option<Point>, String)> = graph
        .edges
        .iter()
        .enumerate()
        .filter_map(|(idx, edge)| {
            let flow_edge = find_flow_edge_for_layout_edge(db, edge)?;
            let source_id = edge.source()?;
            let target_id = edge.target()?;
            let overrides = db.edge_annotation_overrides_for(flow_edge);
            let needs_reroute = moved_nodes.contains(source_id)
                || moved_nodes.contains(target_id)
                || !overrides.bend_points.is_empty()
                || overrides.start_connection.is_some()
                || overrides.end_connection.is_some()
                || overrides.path_mode.is_some();

            if !needs_reroute {
                return None;
            }

            let (points, route_mode) =
                build_flowchart_override_edge_points(graph, edge, &overrides)?;
            let label_position = if edge.label.is_some() {
                geometric_midpoint(&points)
            } else {
                None
            };
            Some((idx, points, label_position, route_mode))
        })
        .collect();

    for (idx, points, label_position, route_mode) in rerouted_edges {
        if let Some(edge) = graph.edges.get_mut(idx) {
            edge.bend_points = points;
            edge.label_position = label_position;
            edge.metadata
                .insert("annotation_route_mode".to_string(), route_mode);
        }
    }

    graph.compute_bounds();
}

fn find_flow_edge_for_layout_edge<'a>(
    db: &'a crate::diagrams::flowchart::FlowchartDb,
    layout_edge: &crate::layout::LayoutEdge,
) -> Option<&'a crate::diagrams::flowchart::FlowEdge> {
    db.edges()
        .iter()
        .find(|edge| edge.id.as_deref() == Some(layout_edge.id.as_str()))
        .or_else(|| {
            db.edges().iter().find(|edge| {
                edge.start == layout_edge.source().unwrap_or("")
                    && edge.end == layout_edge.target().unwrap_or("")
            })
        })
}

fn build_flowchart_override_edge_points(
    graph: &LayoutGraph,
    edge: &crate::layout::LayoutEdge,
    overrides: &crate::diagrams::flowchart::EdgeAnnotationOverrides,
) -> Option<(Vec<Point>, String)> {
    let source = graph.get_node(edge.source()?)?;
    let target = graph.get_node(edge.target()?)?;

    let start = flowchart_connection_anchor(source, target, overrides.start_connection, true)?;
    let end = flowchart_connection_anchor(target, source, overrides.end_connection, false)?;

    if !overrides.bend_points.is_empty() {
        let mut points = Vec::with_capacity(overrides.bend_points.len() + 2);
        points.push(start);
        points.extend(overrides.bend_points.iter().copied());
        points.push(end);
        let route_mode = overrides
            .path_mode
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "manual".to_string());
        return Some((points, route_mode));
    }

    let path_mode = overrides
        .path_mode
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "auto".to_string());

    let (points, route_mode) = if path_mode == "straight" {
        (vec![start, end], "straight".to_string())
    } else if matches!(path_mode.as_str(), "orthogonal" | "orth" | "right-angle") {
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let points = if dx.abs() >= dy.abs() {
            let mid_x = start.x + dx / 2.0;
            vec![
                start,
                Point::new(mid_x, start.y),
                Point::new(mid_x, end.y),
                end,
            ]
        } else {
            let mid_y = start.y + dy / 2.0;
            vec![
                start,
                Point::new(start.x, mid_y),
                Point::new(end.x, mid_y),
                end,
            ]
        };
        (points, "orthogonal".to_string())
    } else if (start.x - end.x).abs() < 0.001 || (start.y - end.y).abs() < 0.001 {
        (vec![start, end], "straight".to_string())
    } else {
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        let mut points = vec![start];
        if dx.abs() >= dy.abs() {
            let mid_x = start.x + dx / 2.0;
            points.push(Point::new(mid_x, start.y));
            points.push(Point::new(mid_x, end.y));
        } else {
            let mid_y = start.y + dy / 2.0;
            points.push(Point::new(start.x, mid_y));
            points.push(Point::new(end.x, mid_y));
        }
        points.push(end);
        (points, "curved".to_string())
    };

    Some((points, route_mode))
}

fn flowchart_connection_anchor(
    node: &crate::layout::LayoutNode,
    other: &crate::layout::LayoutNode,
    explicit: Option<crate::diagrams::flowchart::ConnectionPoint>,
    is_source: bool,
) -> Option<Point> {
    let x = node.x?;
    let y = node.y?;
    let center = node.center()?;

    let anchor = explicit.unwrap_or_else(|| {
        let other_center = other.center().unwrap_or(center);
        let dx = other_center.x - center.x;
        let dy = other_center.y - center.y;
        if dx.abs() >= dy.abs() {
            if dx >= 0.0 {
                if is_source {
                    crate::diagrams::flowchart::ConnectionPoint::Right
                } else {
                    crate::diagrams::flowchart::ConnectionPoint::Left
                }
            } else if is_source {
                crate::diagrams::flowchart::ConnectionPoint::Left
            } else {
                crate::diagrams::flowchart::ConnectionPoint::Right
            }
        } else if dy >= 0.0 {
            if is_source {
                crate::diagrams::flowchart::ConnectionPoint::Bottom
            } else {
                crate::diagrams::flowchart::ConnectionPoint::Top
            }
        } else if is_source {
            crate::diagrams::flowchart::ConnectionPoint::Top
        } else {
            crate::diagrams::flowchart::ConnectionPoint::Bottom
        }
    });

    Some(match anchor {
        crate::diagrams::flowchart::ConnectionPoint::Top => Point::new(center.x, y),
        crate::diagrams::flowchart::ConnectionPoint::Right => Point::new(x + node.width, center.y),
        crate::diagrams::flowchart::ConnectionPoint::Bottom => {
            Point::new(center.x, y + node.height)
        }
        crate::diagrams::flowchart::ConnectionPoint::Left => Point::new(x, center.y),
        crate::diagrams::flowchart::ConnectionPoint::Center => center,
    })
}

fn flowchart_group_box(
    graph: &LayoutGraph,
    subgraph: &crate::diagrams::flowchart::FlowSubGraph,
) -> Option<(f64, f64, f64, f64)> {
    if let Some(group_node) = graph.get_node(&subgraph.id) {
        if let (Some(x), Some(y)) = (group_node.x, group_node.y) {
            if group_node.width > 0.0 && group_node.height > 0.0 {
                return Some((x, y, group_node.width, group_node.height));
            }
        }
    }

    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    let mut found_nodes = false;

    for node_id in &subgraph.nodes {
        if let Some(node) = graph.get_node(node_id) {
            if let (Some(x), Some(y)) = (node.x, node.y) {
                found_nodes = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x + node.width);
                max_y = max_y.max(y + node.height);
            }
        }
    }

    if !found_nodes {
        return None;
    }

    let x = min_x - FLOWCHART_GROUP_PADDING;
    let y = min_y - FLOWCHART_GROUP_PADDING - FLOWCHART_GROUP_TITLE_HEIGHT;
    let width = (max_x - min_x) + FLOWCHART_GROUP_PADDING * 2.0;
    let height = (max_y - min_y) + FLOWCHART_GROUP_PADDING * 2.0 + FLOWCHART_GROUP_TITLE_HEIGHT;
    Some((x, y, width, height))
}

fn flowchart_group_content_origin(group_x: f64, group_y: f64) -> Point {
    Point::new(
        group_x + FLOWCHART_GROUP_PADDING,
        group_y + FLOWCHART_GROUP_PADDING + FLOWCHART_GROUP_TITLE_HEIGHT,
    )
}

/// Render a diagram to ASCII character art.
///
/// This is the primary entry point for ASCII rendering. It accepts any parsed
/// `Diagram` and dispatches to the appropriate type-specific ASCII renderer.
///
/// # Example
///
/// ```
/// let diagram = selkie::parse("flowchart TD\n    A[Start] --> B[End]").unwrap();
/// let ascii = selkie::render::render_ascii(&diagram).unwrap();
/// assert!(ascii.contains("Start"));
/// ```
pub fn render_ascii(diagram: &Diagram) -> Result<String> {
    render_ascii_with_config(diagram, &ascii::AsciiRenderConfig::default())
}

/// Render a diagram to ASCII character art with configuration.
///
/// Like [`render_ascii`], but accepts an [`AsciiRenderConfig`](ascii::AsciiRenderConfig)
/// to control output constraints such as maximum width.
///
/// # Example
///
/// ```
/// use selkie::render::ascii::AsciiRenderConfig;
///
/// let diagram = selkie::parse("flowchart TD\n    A[Start] --> B[End]").unwrap();
/// let config = AsciiRenderConfig { max_width: Some(60), ..Default::default() };
/// let ascii = selkie::render::render_ascii_with_config(&diagram, &config).unwrap();
/// assert!(ascii.lines().all(|line| line.len() <= 60));
/// ```
pub fn render_ascii_with_config(
    diagram: &Diagram,
    config: &ascii::AsciiRenderConfig,
) -> Result<String> {
    use crate::layout::{self, CharacterSizeEstimator, ToLayoutGraph};

    let estimator = CharacterSizeEstimator::default();

    let result = match diagram {
        Diagram::Flowchart(db) => {
            let graph = db.to_layout_graph(&estimator)?;
            let graph = layout::layout(graph)?;
            Ok(ascii::render_flowchart_ascii_with_config(
                db, &graph, config,
            )?)
        }
        Diagram::Sequence(db) => Ok(ascii::render_sequence_ascii(db)?),
        Diagram::Class(db) => {
            let graph = db.to_layout_graph(&estimator)?;
            let graph = layout::layout(graph)?;
            Ok(ascii::render_class_ascii(db, &graph)?)
        }
        Diagram::State(db) => {
            let graph = db.to_layout_graph(&estimator)?;
            let graph = layout::layout(graph)?;
            Ok(ascii::render_graph_ascii_with_config(&graph, config)?)
        }
        Diagram::Er(db) => {
            let graph = db.to_layout_graph(&estimator)?;
            let graph = layout::layout(graph)?;
            Ok(ascii::render_er_ascii(db, &graph)?)
        }
        Diagram::Architecture(db) => {
            let graph = architecture::layout_architecture(db, &estimator)?;
            Ok(ascii::render_graph_ascii_with_config(&graph, config)?)
        }
        Diagram::Requirement(db) => {
            let graph = db.to_layout_graph(&estimator)?;
            let graph = layout::layout(graph)?;
            Ok(ascii::render_graph_ascii_with_config(&graph, config)?)
        }
        Diagram::Pie(db) => Ok(ascii::pie::render_pie_ascii(db)?),
        Diagram::Gantt(db) => {
            let mut db_clone = db.clone();
            Ok(ascii::gantt::render_gantt_ascii(&mut db_clone)?)
        }
        Diagram::Mindmap(db) => Ok(ascii::mindmap::render_mindmap_ascii(db)?),
        Diagram::Journey(db) => Ok(ascii::journey::render_journey_ascii(db)?),
        Diagram::Timeline(db) => Ok(ascii::timeline::render_timeline_ascii(db)?),
        Diagram::Kanban(db) => Ok(ascii::kanban::render_kanban_ascii(db)?),
        Diagram::Packet(db) => Ok(ascii::packet::render_packet_ascii(db)?),
        Diagram::XyChart(db) => Ok(ascii::xychart::render_xychart_ascii(db)?),
        Diagram::Quadrant(db) => Ok(ascii::quadrant::render_quadrant_ascii(db)?),
        Diagram::Radar(db) => Ok(ascii::radar::render_radar_ascii(db)?),
        Diagram::Git(db) => Ok(ascii::gitgraph::render_gitgraph_ascii(db)?),
        Diagram::Sankey(db) => Ok(ascii::sankey::render_sankey_ascii(db)?),
        Diagram::Block(db) => Ok(ascii::block::render_block_ascii(db)?),
        Diagram::C4(db) => Ok(ascii::c4::render_c4_ascii(db)?),
        Diagram::Treemap(db) => Ok(ascii::treemap::render_treemap_ascii(db)?),
        _ => Err(MermaidError::RenderError(
            "ASCII format not yet supported for this diagram type".to_string(),
        )),
    }?;

    // For diagram types that don't yet thread config internally,
    // apply max_width truncation at the output level.
    Ok(truncate_ascii_width(&result, config))
}

/// Truncate each line of ASCII output to the configured max_width.
fn truncate_ascii_width(output: &str, config: &ascii::AsciiRenderConfig) -> String {
    match config.max_width {
        Some(max_w) if max_w > 0 => {
            let mut result = String::with_capacity(output.len());
            for line in output.split('\n') {
                let char_count = line.chars().count();
                if char_count > max_w {
                    let truncated: String = line.chars().take(max_w).collect();
                    result.push_str(&truncated);
                } else {
                    result.push_str(line);
                }
                result.push('\n');
            }
            // Remove trailing extra newline if original didn't end with double newline
            if !output.ends_with("\n\n") && result.ends_with("\n\n") {
                result.pop();
            }
            if output.is_empty() {
                result.clear();
            }
            result
        }
        _ => output.to_string(),
    }
}

/// Render mermaid text directly to ASCII character art.
///
/// This is a convenience function that parses the input text and renders it
/// to ASCII in one step, similar to how [`render_text`] works for SVG.
///
/// # Example
///
/// ```
/// let ascii = selkie::render::render_text_ascii("flowchart TD\n    A[Start] --> B[End]").unwrap();
/// assert!(ascii.contains("Start"));
/// ```
pub fn render_text_ascii(text: &str) -> Result<String> {
    render_text_ascii_with_config(text, &ascii::AsciiRenderConfig::default())
}

/// Render mermaid text directly to ASCII character art with configuration.
///
/// Like [`render_text_ascii`], but accepts an [`AsciiRenderConfig`](ascii::AsciiRenderConfig)
/// for output constraints.
///
/// # Example
///
/// ```
/// use selkie::render::ascii::AsciiRenderConfig;
///
/// let config = AsciiRenderConfig { max_width: Some(80), ..Default::default() };
/// let ascii = selkie::render::render_text_ascii_with_config(
///     "flowchart TD\n    A[Start] --> B[End]",
///     &config,
/// ).unwrap();
/// assert!(ascii.lines().all(|line| line.len() <= 80));
/// ```
pub fn render_text_ascii_with_config(
    text: &str,
    config: &ascii::AsciiRenderConfig,
) -> Result<String> {
    let clean_text = remove_directives(text);
    let diagram_type = detect_type(&clean_text)?;
    let diagram = parse(diagram_type, &clean_text)?;
    render_ascii_with_config(&diagram, config)
}

/// Render an architecture diagram
fn render_architecture(
    db: &crate::diagrams::architecture::ArchitectureDb,
    config: &RenderConfig,
) -> Result<String> {
    let size_estimator = CharacterSizeEstimator::default();

    let graph = architecture::layout_architecture(db, &size_estimator)?;

    let renderer = SvgRenderer::new(config.clone());
    renderer.render_architecture(db, &graph)
}
