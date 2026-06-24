//! Flowchart types

use std::collections::HashMap;
use std::str::FromStr;

use super::annotations::{
    annotation_arrow_head, annotation_connection_point, annotation_f64, annotation_label_align,
    annotation_line_style, annotation_point_pair, annotation_points, annotation_string,
    EdgeAnnotationOverrides, FlowAnnotation, FlowAnnotationAttr, FlowAnnotationTarget,
    GraphAnnotationOverrides, GroupAnnotationOverrides, NodeAnnotationOverrides,
};
use crate::common::CommonDb;
pub use crate::diagrams::direction::Direction;

/// Valid vertex types in flowcharts
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FlowVertexType {
    #[default]
    Square,
    DoubleCircle,
    Circle,
    Ellipse,
    Stadium,
    Subroutine,
    Rect,
    Cylinder,
    Round,
    Diamond,
    Hexagon,
    Odd,
    Trapezoid,
    InvTrapezoid,
    LeanRight,
    LeanLeft,
    /// Extension shape — the inner string is the shape name supplied via
    /// `@{ shape: name }` metadata. Renderer-defined.
    Custom(String),
}

impl FromStr for FlowVertexType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "square" => Ok(Self::Square),
            "doublecircle" => Ok(Self::DoubleCircle),
            "circle" => Ok(Self::Circle),
            "ellipse" => Ok(Self::Ellipse),
            "stadium" => Ok(Self::Stadium),
            "subroutine" => Ok(Self::Subroutine),
            "rect" => Ok(Self::Rect),
            "cylinder" => Ok(Self::Cylinder),
            "round" => Ok(Self::Round),
            "diamond" => Ok(Self::Diamond),
            "hexagon" => Ok(Self::Hexagon),
            "odd" => Ok(Self::Odd),
            "trapezoid" => Ok(Self::Trapezoid),
            "inv_trapezoid" => Ok(Self::InvTrapezoid),
            "lean_right" => Ok(Self::LeanRight),
            "lean_left" => Ok(Self::LeanLeft),
            _ => Err(()),
        }
    }
}

/// Text type for flowchart labels
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FlowTextType {
    #[default]
    Text,
    Markdown,
}

/// Text content for a flowchart element
#[derive(Debug, Clone, Default)]
pub struct FlowText {
    pub text: String,
    pub text_type: FlowTextType,
}

impl FlowText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_type: FlowTextType::Text,
        }
    }

    pub fn markdown(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_type: FlowTextType::Markdown,
        }
    }
}

/// A vertex (node) in a flowchart
#[derive(Debug, Clone)]
pub struct FlowVertex {
    pub id: String,
    pub dom_id: String,
    pub text: Option<String>,
    pub label_type: FlowTextType,
    pub vertex_type: Option<FlowVertexType>,
    pub styles: Vec<String>,
    pub classes: Vec<String>,
    pub dir: Option<String>,
    pub link: Option<String>,
    pub link_target: Option<String>,
    pub have_callback: bool,
    pub icon: Option<String>,
    pub form: Option<String>,
    pub pos: Option<String>,
    pub img: Option<String>,
    pub constraint: Option<String>,
}

impl FlowVertex {
    pub fn new(id: impl Into<String>, dom_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            dom_id: dom_id.into(),
            text: None,
            label_type: FlowTextType::Text,
            vertex_type: None,
            styles: Vec::new(),
            classes: Vec::new(),
            dir: None,
            link: None,
            link_target: None,
            have_callback: false,
            icon: None,
            form: None,
            pos: None,
            img: None,
            constraint: None,
        }
    }
}

/// Edge stroke types
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EdgeStroke {
    #[default]
    Normal,
    Thick,
    Invisible,
    Dotted,
}

/// An edge (link) between nodes in a flowchart
#[derive(Debug, Clone)]
pub struct FlowEdge {
    pub id: Option<String>,
    pub is_user_defined_id: bool,
    pub start: String,
    pub end: String,
    pub interpolate: Option<String>,
    pub edge_type: Option<String>,
    pub stroke: EdgeStroke,
    pub style: Vec<String>,
    pub length: Option<u32>,
    pub text: String,
    pub label_type: FlowTextType,
    pub classes: Vec<String>,
    pub animation: Option<String>,
    pub animate: Option<bool>,
}

impl FlowEdge {
    pub fn new(start: impl Into<String>, end: impl Into<String>) -> Self {
        Self {
            id: None,
            is_user_defined_id: false,
            start: start.into(),
            end: end.into(),
            interpolate: None,
            edge_type: None,
            stroke: EdgeStroke::Normal,
            style: Vec::new(),
            length: None,
            text: String::new(),
            label_type: FlowTextType::Text,
            classes: Vec::new(),
            animation: None,
            animate: None,
        }
    }
}

/// A class definition for styling
#[derive(Debug, Clone, Default)]
pub struct FlowClass {
    pub id: String,
    pub styles: Vec<String>,
    pub text_styles: Vec<String>,
}

impl FlowClass {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            styles: Vec::new(),
            text_styles: Vec::new(),
        }
    }
}

/// A subgraph (container for nodes)
#[derive(Debug, Clone, Default)]
pub struct FlowSubGraph {
    pub id: String,
    pub title: String,
    pub label_type: String,
    pub nodes: Vec<String>,
    pub items: Vec<FlowSubGraphItem>,
    pub classes: Vec<String>,
    pub dir: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowSubGraphItem {
    Node(String),
    Edge(String),
    Subgraph(String),
}

/// Link type information from parser
#[derive(Debug, Clone, Default)]
pub struct FlowLink {
    pub link_type: Option<String>,
    pub stroke: EdgeStroke,
    pub length: Option<u32>,
    pub text: Option<FlowText>,
    pub id: Option<String>,
}

/// Data returned from get_data()
#[derive(Debug, Clone)]
pub struct FlowData {
    pub vertices: HashMap<String, FlowVertex>,
    pub edges: Vec<FlowEdge>,
    pub classes: HashMap<String, FlowClass>,
    pub subgraphs: Vec<FlowSubGraph>,
    pub annotations: Vec<FlowAnnotation>,
}

/// The flowchart database
#[derive(Debug, Clone)]
pub struct FlowchartDb {
    common: CommonDb,
    vertex_counter: u32,
    vertices: HashMap<String, FlowVertex>,
    vertex_order: Vec<String>,
    top_level_items: Vec<FlowSubGraphItem>,
    edges: Vec<FlowEdge>,
    default_interpolate: Option<String>,
    default_style: Option<Vec<String>>,
    classes: HashMap<String, FlowClass>,
    subgraphs: Vec<FlowSubGraph>,
    subgraph_lookup: HashMap<String, usize>,
    tooltips: HashMap<String, String>,
    direction: Direction,
    annotations: Vec<FlowAnnotation>,
}

impl Default for FlowchartDb {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an arrow string to extract edge type, stroke style, and length.
/// Returns (edge_type, stroke, length).
///
/// Arrow formats:
/// - Normal: `-->`, `---`, `-->`
/// - Thick: `==>`, `===`, `==>`
/// - Dotted: `-.->`, `-.-`, `-..->`
/// - With starts: `<-->`, `x--x`, `o--o`
fn parse_arrow(arrow: &str) -> (String, EdgeStroke, u32) {
    let arrow = arrow.trim();

    // Determine stroke type based on characters
    let stroke = if arrow.contains("-.") || arrow.contains(".-") {
        EdgeStroke::Dotted
    } else if arrow.contains('=') {
        EdgeStroke::Thick
    } else if arrow.starts_with('~') {
        EdgeStroke::Invisible
    } else {
        EdgeStroke::Normal
    };

    // Determine edge type based on start/end markers
    let has_start_arrow = arrow.starts_with('<')
        || arrow.starts_with("x-")
        || arrow.starts_with("o-")
        || arrow.starts_with("x=")
        || arrow.starts_with("o=")
        || arrow.starts_with("<-")
        || arrow.starts_with("<=");
    let has_end_arrow = arrow.ends_with('>');
    let has_end_cross = arrow.ends_with('x') && !arrow.starts_with('x');
    let has_end_circle = arrow.ends_with('o') && !arrow.starts_with('o');
    let has_start_cross = arrow.starts_with("x-") || arrow.starts_with("x=");
    let has_start_circle = arrow.starts_with("o-") || arrow.starts_with("o=");
    let has_end_cross_double = arrow.ends_with('x') && has_start_cross;
    let has_end_circle_double = arrow.ends_with('o') && has_start_circle;

    let edge_type = if has_start_arrow && has_end_arrow {
        "double_arrow_point".to_string()
    } else if has_end_cross_double || (has_start_cross && arrow.ends_with('x')) {
        "double_arrow_cross".to_string()
    } else if has_end_circle_double || (has_start_circle && arrow.ends_with('o')) {
        "double_arrow_circle".to_string()
    } else if has_end_arrow {
        "arrow_point".to_string()
    } else if has_end_cross {
        "arrow_cross".to_string()
    } else if has_end_circle {
        "arrow_circle".to_string()
    } else {
        "arrow_open".to_string()
    };

    // Calculate length based on repeated characters
    // Mermaid's algorithm: for open edges (no arrow head), remove last char then subtract 1
    // For arrows with heads, just subtract 1 from the dash count
    let is_open_edge = edge_type == "arrow_open";
    let length = match stroke {
        EdgeStroke::Normal | EdgeStroke::Invisible => {
            // Count consecutive dashes or tildes
            let dash_count = arrow.chars().filter(|&c| c == '-' || c == '~').count();
            // Open edges subtract 2 (like mermaid's slice(-1) then length-1)
            // Arrow edges subtract 1
            let subtract = if is_open_edge { 2 } else { 1 };
            dash_count.saturating_sub(subtract).clamp(1, 10) as u32
        }
        EdgeStroke::Thick => {
            let eq_count = arrow.chars().filter(|&c| c == '=').count();
            let subtract = if is_open_edge { 2 } else { 1 };
            eq_count.saturating_sub(subtract).clamp(1, 10) as u32
        }
        EdgeStroke::Dotted => {
            let dot_count = arrow.chars().filter(|&c| c == '.').count();
            dot_count.clamp(1, 10) as u32
        }
    };

    (edge_type, stroke, length)
}

impl FlowchartDb {
    const DOM_ID_PREFIX: &'static str = "flowchart-";

    pub fn new() -> Self {
        Self {
            common: CommonDb::new(),
            vertex_counter: 0,
            vertices: HashMap::new(),
            vertex_order: Vec::new(),
            top_level_items: Vec::new(),
            edges: Vec::new(),
            default_interpolate: None,
            default_style: None,
            classes: HashMap::new(),
            subgraphs: Vec::new(),
            subgraph_lookup: HashMap::new(),
            tooltips: HashMap::new(),
            direction: Direction::default(),
            annotations: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.common.clear();
        self.vertex_counter = 0;
        self.vertices.clear();
        self.vertex_order.clear();
        self.top_level_items.clear();
        self.edges.clear();
        self.default_interpolate = None;
        self.default_style = None;
        self.classes.clear();
        self.subgraphs.clear();
        self.subgraph_lookup.clear();
        self.tooltips.clear();
        self.direction = Direction::default();
        self.annotations.clear();
    }

    pub fn add_annotation(&mut self, annotation: FlowAnnotation) {
        self.annotations.push(annotation);
    }

    pub fn annotations(&self) -> &[FlowAnnotation] {
        &self.annotations
    }

    /// Replace or create a single annotation attribute on a target.
    ///
    /// Selkie stores annotations as Mermaid comment directives, but edits should work
    /// against a typed target/key/value model. This helper keeps the annotation list
    /// normalized by ensuring there is at most one value for a given target+key pair.
    pub fn set_annotation_attr(
        &mut self,
        target: FlowAnnotationTarget,
        key: impl Into<String>,
        value: impl Into<String>,
    ) {
        let key = key.into();
        let value = value.into();

        for annotation in &mut self.annotations {
            if annotation.target == target {
                annotation.attrs.retain(|attr| attr.key != key);
                annotation.attrs.push(FlowAnnotationAttr {
                    key: key.clone(),
                    value,
                });
                return;
            }
        }

        self.annotations.push(FlowAnnotation {
            target,
            attrs: vec![FlowAnnotationAttr { key, value }],
        });
    }

    /// Remove a single annotation attribute from a target.
    pub fn remove_annotation_attr(&mut self, target: &FlowAnnotationTarget, key: &str) {
        self.annotations.retain_mut(|annotation| {
            if &annotation.target == target {
                annotation.attrs.retain(|attr| attr.key != key);
                !annotation.attrs.is_empty()
            } else {
                true
            }
        });
    }

    /// Remove all annotations for a specific target.
    pub fn remove_annotations_for_target(&mut self, target: &FlowAnnotationTarget) {
        self.annotations
            .retain(|annotation| &annotation.target != target);
    }

    /// Update a vertex label while preserving the desired text type.
    pub fn set_vertex_text_obj(&mut self, id: &str, text: FlowText) -> bool {
        if let Some(vertex) = self.vertices.get_mut(id) {
            vertex.text = Some(text.text);
            vertex.label_type = text.text_type;
            true
        } else {
            false
        }
    }

    /// Move a vertex into a subgraph, or out to the top level when `target_subgraph_id` is `None`.
    pub fn move_vertex_to_subgraph(
        &mut self,
        vertex_id: &str,
        target_subgraph_id: Option<&str>,
    ) -> bool {
        if !self.vertices.contains_key(vertex_id) {
            return false;
        }

        self.top_level_items
            .retain(|item| !matches!(item, FlowSubGraphItem::Node(id) if id == vertex_id));

        let mut found_target = target_subgraph_id.is_none();
        for subgraph in &mut self.subgraphs {
            subgraph.nodes.retain(|id| id != vertex_id);
            subgraph
                .items
                .retain(|item| !matches!(item, FlowSubGraphItem::Node(id) if id == vertex_id));
            if Some(subgraph.id.as_str()) == target_subgraph_id {
                found_target = true;
                if !subgraph.nodes.iter().any(|id| id == vertex_id) {
                    subgraph.nodes.push(vertex_id.to_string());
                }
                if !subgraph
                    .items
                    .iter()
                    .any(|item| matches!(item, FlowSubGraphItem::Node(id) if id == vertex_id))
                {
                    subgraph
                        .items
                        .push(FlowSubGraphItem::Node(vertex_id.to_string()));
                }
            }
        }

        if target_subgraph_id.is_none()
            && !self
                .top_level_items
                .iter()
                .any(|item| matches!(item, FlowSubGraphItem::Node(id) if id == vertex_id))
        {
            self.top_level_items
                .push(FlowSubGraphItem::Node(vertex_id.to_string()));
        }

        found_target
    }

    /// Move an edge into a subgraph, or out to the top level when `target_subgraph_id` is `None`.
    pub fn move_edge_to_subgraph(
        &mut self,
        edge_index: usize,
        target_subgraph_id: Option<&str>,
    ) -> bool {
        let Some(edge_id) = self.edges.get(edge_index).and_then(|edge| edge.id.clone()) else {
            return false;
        };

        self.top_level_items
            .retain(|item| !matches!(item, FlowSubGraphItem::Edge(id) if id == &edge_id));

        let mut found_target = target_subgraph_id.is_none();
        for subgraph in &mut self.subgraphs {
            subgraph
                .items
                .retain(|item| !matches!(item, FlowSubGraphItem::Edge(id) if id == &edge_id));
            if Some(subgraph.id.as_str()) == target_subgraph_id {
                found_target = true;
                if !subgraph
                    .items
                    .iter()
                    .any(|item| matches!(item, FlowSubGraphItem::Edge(id) if id == &edge_id))
                {
                    subgraph.items.push(FlowSubGraphItem::Edge(edge_id.clone()));
                }
            }
        }

        if target_subgraph_id.is_none()
            && !self
                .top_level_items
                .iter()
                .any(|item| matches!(item, FlowSubGraphItem::Edge(id) if id == &edge_id))
        {
            self.top_level_items.push(FlowSubGraphItem::Edge(edge_id));
        }

        found_target
    }

    /// Remove an edge by index, also removing annotations targeted at that edge ordinal.
    pub fn remove_edge_at(&mut self, index: usize) -> Option<FlowEdge> {
        if index >= self.edges.len() {
            return None;
        }

        let edge = self.edges.remove(index);
        if let Some(edge_id) = edge.id.as_ref() {
            self.top_level_items
                .retain(|item| !matches!(item, FlowSubGraphItem::Edge(id) if id == edge_id));
            for subgraph in &mut self.subgraphs {
                subgraph
                    .items
                    .retain(|item| !matches!(item, FlowSubGraphItem::Edge(id) if id == edge_id));
            }
        }
        let mut ordinal = 1_u32;
        for candidate in &self.edges[..index] {
            if candidate.start == edge.start && candidate.end == edge.end {
                ordinal += 1;
            }
        }
        self.remove_annotations_for_target(&FlowAnnotationTarget::Edge {
            from: edge.start.clone(),
            to: edge.end.clone(),
            ordinal,
        });
        self.shift_edge_annotation_ordinals_after_removal(&edge.start, &edge.end, ordinal);

        Some(edge)
    }

    pub fn graph_annotation_overrides(&self) -> GraphAnnotationOverrides {
        let attrs =
            self.collect_annotation_attrs(|target| matches!(target, FlowAnnotationTarget::Graph));
        GraphAnnotationOverrides {
            width_cm: annotation_f64(&attrs, "width_cm"),
            height_cm: annotation_f64(&attrs, "height_cm"),
            canvas_fill: annotation_string(&attrs, "canvas_fill"),
            font_face: annotation_string(&attrs, "font_face"),
            node_label_font_size: annotation_f64(&attrs, "node_label_font_size"),
            group_label_font_size: annotation_f64(&attrs, "group_label_font_size"),
            edge_label_font_size: annotation_f64(&attrs, "edge_label_font_size"),
            node_label_align: annotation_label_align(&attrs, "node_label_align"),
            group_label_align: annotation_label_align(&attrs, "group_label_align"),
            edge_label_align: annotation_label_align(&attrs, "edge_label_align"),
        }
    }

    pub fn node_annotation_overrides(&self, node_id: &str) -> NodeAnnotationOverrides {
        let attrs = self.collect_annotation_attrs(
            |target| matches!(target, FlowAnnotationTarget::Node { id } if id == node_id),
        );
        NodeAnnotationOverrides {
            x: annotation_f64(&attrs, "x"),
            y: annotation_f64(&attrs, "y"),
            width: annotation_f64(&attrs, "w"),
            height: annotation_f64(&attrs, "h"),
            fill: annotation_string(&attrs, "fill"),
            stroke: annotation_string(&attrs, "stroke"),
            line_width: annotation_f64(&attrs, "line_width"),
            label_align: annotation_label_align(&attrs, "label_align"),
        }
    }

    pub fn edge_annotation_overrides_for(&self, edge: &FlowEdge) -> EdgeAnnotationOverrides {
        let ordinal = self.edge_ordinal(edge);
        let attrs = self.collect_annotation_attrs(|target| {
            matches!(
                target,
                FlowAnnotationTarget::Edge { from, to, ordinal: target_ordinal }
                    if from == &edge.start && to == &edge.end && *target_ordinal == ordinal
            )
        });
        EdgeAnnotationOverrides {
            line_color: annotation_string(&attrs, "line_color"),
            line_width: annotation_f64(&attrs, "line_width"),
            line_style: annotation_line_style(&attrs, "line_style")
                .or_else(|| annotation_line_style(&attrs, "line_type")),
            start_arrow: annotation_arrow_head(&attrs, "start_arrow"),
            end_arrow: annotation_arrow_head(&attrs, "end_arrow"),
            start_connection: annotation_connection_point(&attrs, "start_connection"),
            end_connection: annotation_connection_point(&attrs, "end_connection"),
            path_mode: annotation_string(&attrs, "path_mode"),
            bend_points: annotation_points(&attrs, "bend_points").unwrap_or_default(),
            label_offset: annotation_point_pair(
                annotation_f64(&attrs, "label_offset_x"),
                annotation_f64(&attrs, "label_offset_y"),
            ),
        }
    }

    pub fn group_annotation_overrides(&self, group_id: &str) -> GroupAnnotationOverrides {
        let attrs = self.collect_annotation_attrs(
            |target| matches!(target, FlowAnnotationTarget::Group { id } if id == group_id),
        );
        GroupAnnotationOverrides {
            x: annotation_f64(&attrs, "x"),
            y: annotation_f64(&attrs, "y"),
            width: annotation_f64(&attrs, "w"),
            height: annotation_f64(&attrs, "h"),
            fill: annotation_string(&attrs, "fill"),
            stroke: annotation_string(&attrs, "stroke"),
            label_align: annotation_label_align(&attrs, "label_align"),
        }
    }

    fn collect_annotation_attrs(
        &self,
        matches_target: impl Fn(&FlowAnnotationTarget) -> bool,
    ) -> Vec<FlowAnnotationAttr> {
        let mut attrs = Vec::new();
        for annotation in &self.annotations {
            if matches_target(&annotation.target) {
                attrs.extend(annotation.attrs.clone());
            }
        }
        attrs
    }

    fn edge_ordinal(&self, edge: &FlowEdge) -> u32 {
        let mut ordinal = 0;
        for candidate in &self.edges {
            if candidate.start == edge.start && candidate.end == edge.end {
                ordinal += 1;
            }
            if std::ptr::eq(candidate, edge) {
                return ordinal.max(1);
            }
        }
        ordinal.max(1)
    }

    fn shift_edge_annotation_ordinals_after_removal(
        &mut self,
        start: &str,
        end: &str,
        removed_ordinal: u32,
    ) {
        for annotation in &mut self.annotations {
            if let FlowAnnotationTarget::Edge { from, to, ordinal } = &mut annotation.target {
                if from == start && to == end && *ordinal > removed_ordinal {
                    *ordinal -= 1;
                }
            }
        }
    }

    /// Check if a node exists in any of the given subgraphs
    pub fn exists(&self, subgraphs: &[FlowSubGraph], node_id: &str) -> bool {
        subgraphs
            .iter()
            .any(|sg| sg.nodes.iter().any(|n| n == node_id))
    }

    /// Remove nodes from a subgraph that already exist in other subgraphs
    pub fn make_uniq(&self, subgraph: &mut FlowSubGraph, existing: &[FlowSubGraph]) {
        subgraph.nodes.retain(|node| !self.exists(existing, node));
    }

    /// Add a vertex to the flowchart
    #[allow(clippy::too_many_arguments)]
    pub fn add_vertex(
        &mut self,
        id: &str,
        text_obj: Option<FlowText>,
        vertex_type: Option<FlowVertexType>,
        styles: Vec<String>,
        classes: Vec<String>,
        dir: Option<&str>,
        _metadata: Option<&str>,
    ) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }

        let is_new = !self.vertices.contains_key(id);
        let vertex = self.vertices.entry(id.to_string()).or_insert_with(|| {
            let dom_id = format!("{}{}-{}", Self::DOM_ID_PREFIX, id, self.vertex_counter);
            FlowVertex::new(id, dom_id)
        });
        if is_new {
            self.vertex_order.push(id.to_string());
        }

        self.vertex_counter += 1;

        if let Some(text_obj) = text_obj {
            let txt = text_obj.text.trim();
            // Strip surrounding quotes
            let txt = txt
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(txt);
            vertex.text = Some(txt.to_string());
            vertex.label_type = text_obj.text_type;
        } else if vertex.text.is_none() {
            vertex.text = Some(id.to_string());
        }

        if let Some(vt) = vertex_type {
            vertex.vertex_type = Some(vt);
        }

        vertex.styles.extend(styles);
        vertex.classes.extend(classes);

        if let Some(d) = dir {
            vertex.dir = Some(d.to_string());
        }
    }

    /// Add an edge between nodes
    fn add_single_link(
        &mut self,
        start: &str,
        end: &str,
        link_data: Option<&FlowLink>,
        id: Option<&str>,
    ) {
        let mut edge = FlowEdge::new(start, end);
        edge.interpolate.clone_from(&self.default_interpolate);

        if let Some(link) = link_data {
            if let Some(text) = &link.text {
                let txt = text.text.trim();
                let txt = txt
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(txt);
                edge.text = txt.to_string();
                edge.label_type = text.text_type.clone();
            }
            edge.edge_type.clone_from(&link.link_type);
            edge.stroke = link.stroke.clone();
            edge.length = link.length.map(|l| l.min(10));
        }

        if let Some(user_id) = id {
            let id_exists = self.edges.iter().any(|e| e.id.as_deref() == Some(user_id));
            if !id_exists {
                edge.id = Some(user_id.to_string());
                edge.is_user_defined_id = true;
            }
        }

        if edge.id.is_none() {
            let existing_count = self
                .edges
                .iter()
                .filter(|e| e.start == start && e.end == end)
                .count();
            edge.id = Some(format!("L-{}-{}-{}", start, end, existing_count));
        }

        self.edges.push(edge);
    }

    /// Add links between multiple start and end nodes
    pub fn add_link(&mut self, starts: &[&str], ends: &[&str], link_data: Option<&FlowLink>) {
        let id = link_data.and_then(|l| l.id.as_deref());
        let last_start_idx = starts.len().saturating_sub(1);

        for (si, start) in starts.iter().enumerate() {
            for (ei, end) in ends.iter().enumerate() {
                // Only use ID for last start and first end
                let use_id = si == last_start_idx && ei == 0;
                self.add_single_link(start, end, link_data, if use_id { id } else { None });
            }
        }
    }

    /// Update link interpolation
    pub fn update_link_interpolate(&mut self, positions: &[String], interpolate: &str) {
        let interpolate = interpolate.to_string();

        for pos in positions {
            if pos == "default" {
                self.default_interpolate = Some(interpolate.clone());
                // Apply to existing edges without explicit interpolate
                for edge in &mut self.edges {
                    if edge.interpolate.is_none() {
                        edge.interpolate = Some(interpolate.clone());
                    }
                }
            } else if let Ok(idx) = pos.parse::<usize>() {
                if let Some(edge) = self.edges.get_mut(idx) {
                    edge.interpolate = Some(interpolate.clone());
                }
            }
        }
    }

    /// Update link style
    pub fn update_link(&mut self, positions: &[usize], style: &[String]) {
        for &pos in positions {
            if pos == usize::MAX {
                self.default_style = Some(style.to_vec());
            } else if let Some(edge) = self.edges.get_mut(pos) {
                edge.style = style.to_vec();
                // Add fill:none if not already present
                let has_fill = edge.style.iter().any(|s| s.starts_with("fill"));
                if !has_fill {
                    edge.style.push("fill:none".to_string());
                }
            }
        }
    }

    /// Add a CSS class definition
    pub fn add_class(&mut self, ids: &str, styles: &[String]) {
        // Process styles: handle escaped commas, convert commas to semicolons
        let processed: Vec<String> = styles
            .join(",")
            .replace("\\,", "\x00") // Temporary placeholder
            .replace(',', ";")
            .replace('\x00', ",")
            .split(';')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        for id in ids.split(',').map(str::trim) {
            let class_node = self
                .classes
                .entry(id.to_string())
                .or_insert_with(|| FlowClass::new(id));

            for style in &processed {
                if style.contains("color") {
                    class_node.text_styles.push(style.replace("fill", "bgFill"));
                }
                class_node.styles.push(style.clone());
            }
        }
    }

    /// Set the direction of the flowchart
    pub fn set_direction(&mut self, dir: &str) {
        self.direction = Direction::parse(dir);
    }

    /// Get the direction of the flowchart as a string
    pub fn get_direction(&self) -> &'static str {
        self.direction.as_str()
    }

    /// Set class on elements
    pub fn set_class(&mut self, ids: &str, class_name: &str) {
        for id in ids.split(',').map(str::trim) {
            if let Some(vertex) = self.vertices.get_mut(id) {
                vertex.classes.push(class_name.to_string());
            }

            for edge in &mut self.edges {
                if edge.id.as_deref() == Some(id) {
                    edge.classes.push(class_name.to_string());
                }
            }

            if let Some(&idx) = self.subgraph_lookup.get(id) {
                if let Some(sg) = self.subgraphs.get_mut(idx) {
                    sg.classes.push(class_name.to_string());
                }
            }
        }
    }

    /// Add a subgraph
    pub fn add_sub_graph(&mut self, nodes: Vec<String>, id: &str, title: &str, dir: &str) {
        let subgraph = FlowSubGraph {
            id: id.to_string(),
            title: title.to_string(),
            label_type: "text".to_string(),
            nodes,
            items: Vec::new(),
            classes: Vec::new(),
            dir: if dir.is_empty() {
                None
            } else {
                Some(dir.to_string())
            },
            parent_id: None,
        };

        let idx = self.subgraphs.len();
        self.subgraph_lookup.insert(id.to_string(), idx);
        self.subgraphs.push(subgraph);
    }

    /// Get all vertices
    pub fn vertices(&self) -> &HashMap<String, FlowVertex> {
        &self.vertices
    }

    /// Get vertex ids in declaration order.
    pub fn vertex_order(&self) -> &[String] {
        &self.vertex_order
    }

    pub fn top_level_items(&self) -> &[FlowSubGraphItem] {
        &self.top_level_items
    }

    pub fn push_top_level_item(&mut self, item: FlowSubGraphItem) {
        self.top_level_items.push(item);
    }

    /// Get vertices (alias for compatibility with parser)
    pub fn get_vertices(&self) -> &HashMap<String, FlowVertex> {
        &self.vertices
    }

    /// Get a mutable vertex by ID
    pub fn get_vertex_mut(&mut self, id: &str) -> Option<&mut FlowVertex> {
        self.vertices.get_mut(id)
    }

    /// Get all edges
    pub fn edges(&self) -> &[FlowEdge] {
        &self.edges
    }

    /// Get edges (alias for compatibility with parser)
    pub fn get_edges(&self) -> &[FlowEdge] {
        &self.edges
    }

    /// Get all classes
    pub fn get_classes(&self) -> &HashMap<String, FlowClass> {
        &self.classes
    }

    /// Get compiled styles for a vertex
    ///
    /// Compiles styles from the vertex's assigned classes and any inline styles
    /// into a single CSS style string for use as an inline style attribute.
    pub fn get_compiled_styles(&self, vertex: &FlowVertex) -> Option<String> {
        let mut styles: Vec<String> = Vec::new();

        // First, add styles from assigned classes
        for class_name in &vertex.classes {
            if let Some(class_def) = self.classes.get(class_name) {
                styles.extend(class_def.styles.clone());
            }
        }

        // Then add inline styles (these take precedence)
        styles.extend(vertex.styles.clone());

        if styles.is_empty() {
            None
        } else {
            // Join styles with semicolons and add !important to override theme styles
            let style_str = styles
                .iter()
                .map(|s| {
                    let s = s.trim();
                    if s.ends_with("!important") {
                        s.to_string()
                    } else {
                        format!("{} !important", s)
                    }
                })
                .collect::<Vec<_>>()
                .join(";");
            Some(style_str)
        }
    }

    /// Get all subgraphs
    pub fn subgraphs(&self) -> &[FlowSubGraph] {
        &self.subgraphs
    }

    pub fn get_subgraph_mut(&mut self, id: &str) -> Option<&mut FlowSubGraph> {
        self.subgraph_lookup
            .get(id)
            .and_then(|&idx| self.subgraphs.get_mut(idx))
    }

    /// Simplified add_vertex for parser - just id, optional text and type
    pub fn add_vertex_simple(
        &mut self,
        id: &str,
        text: Option<&str>,
        vertex_type: Option<FlowVertexType>,
    ) {
        let text_obj = text.map(FlowText::new);
        self.add_vertex(
            id,
            text_obj,
            vertex_type,
            Vec::new(),
            Vec::new(),
            None,
            None,
        );
    }

    /// Add an edge between two nodes (simplified for parser)
    pub fn add_edge(
        &mut self,
        start: &str,
        end: &str,
        arrow: &str,
        text: Option<&str>,
        link_id: Option<&str>,
    ) {
        // Ensure vertices exist
        if !self.vertices.contains_key(start) {
            self.add_vertex_simple(start, None, None);
        }
        if !self.vertices.contains_key(end) {
            self.add_vertex_simple(end, None, None);
        }

        // Parse arrow string to extract edge type, stroke, and length
        let (edge_type, stroke, length) = parse_arrow(arrow);

        let flow_link = FlowLink {
            text: text.map(FlowText::new),
            id: link_id.map(String::from),
            link_type: Some(edge_type),
            stroke,
            length: Some(length),
        };

        self.add_single_link(start, end, Some(&flow_link), link_id);
    }

    /// Add a subgraph (simplified for parser)
    pub fn add_subgraph(&mut self, id: &str, title: &str) {
        self.add_sub_graph(Vec::new(), id, title, "");
    }

    /// Add a subgraph with member nodes
    pub fn add_subgraph_with_nodes(&mut self, id: &str, title: &str, nodes: Vec<String>) {
        self.add_sub_graph(nodes, id, title, "");
    }

    /// Add a subgraph with member nodes and optional direction
    pub fn add_subgraph_with_dir(
        &mut self,
        id: &str,
        title: &str,
        nodes: Vec<String>,
        dir: Option<String>,
    ) {
        self.add_sub_graph(nodes, id, title, dir.as_deref().unwrap_or(""));
    }

    /// Set link on a vertex (for click handler)
    pub fn set_link(&mut self, id: &str, link: &str, target: Option<&str>) {
        if let Some(vertex) = self.vertices.get_mut(id) {
            vertex.link = Some(link.to_string());
            vertex.link_target = target.map(String::from);
        }
    }

    /// Set click event on a vertex
    pub fn set_click_event(&mut self, id: &str, callback: &str) {
        if let Some(vertex) = self.vertices.get_mut(id) {
            vertex.have_callback = true;
            // Store callback name (would need additional field)
        }
        let _ = callback; // TODO: store callback
    }

    /// Set tooltip on a vertex
    pub fn set_tooltip(&mut self, id: &str, tooltip: &str) {
        self.tooltips.insert(id.to_string(), tooltip.to_string());
    }

    /// Set default link style
    pub fn set_default_link_style(&mut self, styles: &[String]) {
        self.default_style = Some(styles.to_vec());
    }

    /// Set link style by index
    pub fn set_link_style(&mut self, idx: usize, styles: &[String]) {
        if let Some(edge) = self.edges.get_mut(idx) {
            edge.style = styles.to_vec();
        }
    }

    /// Set default link interpolate
    pub fn set_default_link_interpolate(&mut self, interpolate: &str) {
        self.default_interpolate = Some(interpolate.to_string());
    }

    /// Get data for rendering
    pub fn get_data(&self) -> FlowData {
        FlowData {
            vertices: self.vertices.clone(),
            edges: self.edges.clone(),
            classes: self.classes.clone(),
            subgraphs: self.subgraphs.clone(),
            annotations: self.annotations.clone(),
        }
    }

    /// Write the flowchart back to Mermaid source for the supported subset.
    pub fn to_mermaid(&self) -> String {
        super::writer::write(self)
    }

    // Common DB delegation
    pub fn set_acc_title(&mut self, title: impl Into<String>) {
        self.common.set_acc_title(title);
    }

    pub fn get_acc_title(&self) -> Option<&str> {
        self.common.get_acc_title()
    }

    pub fn set_acc_description(&mut self, desc: impl Into<String>) {
        self.common.set_acc_description(desc);
    }

    pub fn get_acc_description(&self) -> Option<&str> {
        self.common.get_acc_description()
    }

    pub fn set_diagram_title(&mut self, title: impl Into<String>) {
        self.common.set_diagram_title(title);
    }

    pub fn get_diagram_title(&self) -> Option<&str> {
        self.common.get_diagram_title()
    }
}
