//! Mermaid writeback for supported flowchart subsets.

use std::collections::HashSet;

use super::types::{
    EdgeStroke, FlowEdge, FlowSubGraph, FlowSubGraphItem, FlowTextType, FlowVertex, FlowVertexType,
    FlowchartDb,
};
use super::{FlowAnnotation, FlowAnnotationTarget};

/// Write a flowchart database back to Mermaid source.
///
/// This currently targets a deterministic supported subset:
/// - graph direction
/// - node declarations
/// - subgraphs and their member nodes
/// - edges
/// - annotation comments
pub fn write(db: &FlowchartDb) -> String {
    let mut lines = Vec::new();
    lines.push(format!("flowchart {}", db.get_direction()));

    let grouped_nodes: HashSet<&str> = db
        .subgraphs()
        .iter()
        .flat_map(|subgraph| subgraph.nodes.iter().map(|id| id.as_str()))
        .collect();

    let mut written_top_level_nodes: HashSet<&str> = HashSet::new();
    let mut written_root_subgraphs: HashSet<&str> = HashSet::new();
    let mut written_edges: HashSet<String> = HashSet::new();

    if !db.top_level_items().is_empty() {
        for item in db.top_level_items() {
            match item {
                FlowSubGraphItem::Node(node_id) => {
                    if grouped_nodes.contains(node_id.as_str()) {
                        continue;
                    }
                    if let Some(vertex) = db.vertices().get(node_id.as_str()) {
                        lines.push(write_vertex(vertex));
                        written_top_level_nodes.insert(node_id.as_str());
                    }
                }
                FlowSubGraphItem::Subgraph(subgraph_id) => {
                    if let Some(subgraph) = db.subgraphs().iter().find(|subgraph| {
                        subgraph.id == *subgraph_id && subgraph.parent_id.is_none()
                    }) {
                        lines.extend(write_subgraph(db, subgraph, 0, &mut written_edges));
                        written_root_subgraphs.insert(subgraph.id.as_str());
                    }
                }
                FlowSubGraphItem::Edge(edge_id) => {
                    if let Some(edge) = db
                        .edges()
                        .iter()
                        .find(|edge| edge.id.as_deref() == Some(edge_id.as_str()))
                    {
                        lines.push(write_edge(edge));
                        written_edges.insert(edge_id.clone());
                    }
                }
            }
        }
    }

    for node_id in db.vertex_order() {
        if grouped_nodes.contains(node_id.as_str())
            || written_top_level_nodes.contains(node_id.as_str())
        {
            continue;
        }
        if let Some(vertex) = db.vertices().get(node_id.as_str()) {
            lines.push(write_vertex(vertex));
        }
    }

    for subgraph in db
        .subgraphs()
        .iter()
        .filter(|subgraph| subgraph.parent_id.is_none())
    {
        if written_root_subgraphs.contains(subgraph.id.as_str()) {
            continue;
        }
        lines.extend(write_subgraph(db, subgraph, 0, &mut written_edges));
    }

    for edge in db.edges() {
        if let Some(edge_id) = edge.id.as_deref() {
            if written_edges.contains(edge_id) {
                continue;
            }
        }
        lines.push(write_edge(edge));
    }

    for annotation in db.annotations() {
        lines.push(write_annotation(annotation));
    }

    let mut output = lines.join("\n");
    output.push('\n');
    output
}

fn write_subgraph(
    db: &FlowchartDb,
    subgraph: &FlowSubGraph,
    depth: usize,
    written_edges: &mut HashSet<String>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let indent = "    ".repeat(depth);
    let header = if subgraph.title.is_empty() || subgraph.title == subgraph.id {
        format!("{indent}subgraph {}", subgraph.id)
    } else {
        format!(
            "{indent}subgraph {}[{}]",
            subgraph.id,
            format_text_content(&subgraph.title)
        )
    };
    lines.push(header);

    if let Some(dir) = subgraph.dir.as_deref() {
        lines.push(format!("{indent}    direction {}", dir));
    }

    if !subgraph.items.is_empty() {
        for item in &subgraph.items {
            match item {
                FlowSubGraphItem::Node(node_id) => {
                    if let Some(vertex) = db.vertices().get(node_id.as_str()) {
                        lines.push(format!("{indent}    {}", write_vertex(vertex)));
                    } else {
                        lines.push(format!("{indent}    {}", node_id));
                    }
                }
                FlowSubGraphItem::Edge(edge_id) => {
                    if let Some(edge) = db
                        .edges()
                        .iter()
                        .find(|edge| edge.id.as_deref() == Some(edge_id.as_str()))
                    {
                        lines.push(format!("{indent}    {}", write_edge(edge)));
                        written_edges.insert(edge_id.clone());
                    }
                }
                FlowSubGraphItem::Subgraph(child_id) => {
                    if let Some(child) = db.subgraphs().iter().find(|sg| sg.id == *child_id) {
                        lines.extend(write_subgraph(db, child, depth + 1, written_edges));
                    }
                }
            }
        }
    } else {
        for node_id in &subgraph.nodes {
            if let Some(vertex) = db.vertices().get(node_id.as_str()) {
                lines.push(format!("{indent}    {}", write_vertex(vertex)));
            } else {
                lines.push(format!("{indent}    {}", node_id));
            }
        }
    }

    lines.push(format!("{indent}end"));
    lines
}

fn write_vertex(vertex: &FlowVertex) -> String {
    let label = vertex.text.as_deref().unwrap_or(&vertex.id);
    let formatted_label = format_text_content_with_type(label, &vertex.label_type);
    let body = match vertex
        .vertex_type
        .as_ref()
        .unwrap_or(&FlowVertexType::Square)
    {
        FlowVertexType::Square | FlowVertexType::Rect => {
            if label == vertex.id {
                String::new()
            } else {
                format!("[{}]", formatted_label)
            }
        }
        FlowVertexType::Round => format!("({})", formatted_label),
        FlowVertexType::Circle => format!("(({}))", formatted_label),
        FlowVertexType::DoubleCircle => format!("((({})))", formatted_label),
        FlowVertexType::Ellipse => format!("(-{}-)", formatted_label),
        FlowVertexType::Stadium => format!("([{}])", formatted_label),
        FlowVertexType::Subroutine => format!("[[{}]]", formatted_label),
        FlowVertexType::Cylinder => format!("[({})]", formatted_label),
        FlowVertexType::Diamond => format!("{{{}}}", formatted_label),
        FlowVertexType::Hexagon => format!("{{{{{}}}}}", formatted_label),
        FlowVertexType::Odd => format!(">{}]", formatted_label),
        FlowVertexType::Trapezoid => format!("[/{}\\]", formatted_label),
        FlowVertexType::InvTrapezoid => format!("[\\{}/]", formatted_label),
        FlowVertexType::LeanRight => format!("[/{} /]", formatted_label).replace(" /]", "/]"),
        FlowVertexType::LeanLeft => format!("[\\{}\\]", formatted_label),
        FlowVertexType::Custom(name) => format!("@{{ shape: {}, label: \"{}\" }}", name, formatted_label),
    };

    let mut result = format!("{}{}", vertex.id, body);
    if let Some(class_name) = vertex.classes.first() {
        result.push_str(":::");
        result.push_str(class_name);
    }
    result
}

fn write_edge(edge: &FlowEdge) -> String {
    let mut line = String::new();
    line.push_str(&edge.start);
    line.push(' ');
    if edge.is_user_defined_id {
        if let Some(id) = edge.id.as_deref() {
            line.push_str(id);
            line.push('@');
        }
    }

    let arrow = edge_arrow(edge);
    if edge.text.is_empty() {
        line.push_str(&arrow);
        line.push(' ');
    } else {
        line.push_str(&arrow);
        line.push('|');
        line.push_str(&format_text_content_with_type(&edge.text, &edge.label_type));
        line.push('|');
        line.push(' ');
    }

    line.push_str(&edge.end);
    line
}

fn edge_arrow(edge: &FlowEdge) -> String {
    let edge_type = edge.edge_type.as_deref().unwrap_or("arrow_open");
    let start = match edge_type {
        "double_arrow_point" => "<",
        "double_arrow_circle" => "o",
        "double_arrow_cross" => "x",
        _ => "",
    };
    let end = match edge_type {
        "arrow_point" | "double_arrow_point" => ">",
        "arrow_circle" | "double_arrow_circle" => "o",
        "arrow_cross" | "double_arrow_cross" => "x",
        _ => "",
    };

    match edge.stroke {
        EdgeStroke::Dotted => {
            if end.is_empty() {
                format!("{start}-.-")
            } else {
                format!("{start}-.-{end}")
            }
        }
        EdgeStroke::Thick => {
            let length = edge.length.unwrap_or(1).clamp(1, 10) as usize;
            let count = if start.is_empty() && end.is_empty() {
                length + 2
            } else {
                length + 1
            };
            format!("{start}{}{end}", "=".repeat(count))
        }
        EdgeStroke::Invisible | EdgeStroke::Normal => {
            let length = edge.length.unwrap_or(1).clamp(1, 10) as usize;
            let count = if start.is_empty() && end.is_empty() {
                length + 2
            } else {
                length + 1
            };
            format!("{start}{}{end}", "-".repeat(count))
        }
    }
}

fn write_annotation(annotation: &FlowAnnotation) -> String {
    let target = match &annotation.target {
        FlowAnnotationTarget::Graph => "@graph".to_string(),
        FlowAnnotationTarget::Node { id } => format!("@node {}", id),
        FlowAnnotationTarget::Edge { from, to, ordinal } => {
            if *ordinal <= 1 {
                format!("@edge {}->{}", from, to)
            } else {
                format!("@edge {}->{}#{}", from, to, ordinal)
            }
        }
        FlowAnnotationTarget::Group { id } => format!("@group {}", id),
    };

    let attrs = annotation
        .attrs
        .iter()
        .map(|attr| format!(r#"{}="{}""#, attr.key, escape_annotation_value(&attr.value)))
        .collect::<Vec<_>>()
        .join(" ");

    format!("%% {} {}", target, attrs)
}

fn format_text_content(value: &str) -> String {
    if needs_quotes(value) {
        format!(r#""{}""#, value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn format_text_content_with_type(value: &str, text_type: &FlowTextType) -> String {
    match text_type {
        FlowTextType::Markdown => {
            let escaped = value
                .replace('\\', "\\\\")
                .replace('`', "\\`")
                .replace('"', "\\\"");
            format!(r#""`{}`""#, escaped)
        }
        FlowTextType::Text => format_text_content(value),
    }
}

fn escape_annotation_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn needs_quotes(value: &str) -> bool {
    value.is_empty()
        || value.chars().any(|ch| {
            matches!(
                ch,
                '[' | ']' | '(' | ')' | '{' | '}' | '|' | '"' | '\n' | '\r'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagrams::flowchart::{
        format as format_flowchart, parse, ArrowHead, ConnectionPoint, FlowAnnotationTarget,
        FlowText, FlowTextType, FlowVertexType,
    };

    fn assert_exact_roundtrip(input: &str) {
        let db = parse(input).expect("source should parse");
        let written = db.to_mermaid();
        assert_eq!(
            written, input,
            "unchanged canonical Mermaid should survive exact roundtrip"
        );
    }

    fn assert_formatter_idempotent(input: &str) {
        let once = format_flowchart(input).expect("source should format");
        let twice = format_flowchart(&once).expect("formatted source should stay formattable");
        assert_eq!(
            twice, once,
            "flowchart formatter should be idempotent once source is canonical"
        );
    }

    fn assert_formatter_equivalent_roundtrip(input: &str) {
        let formatted_input = format_flowchart(input).expect("input should format");
        let db = parse(input).expect("source should parse");
        let written = db.to_mermaid();
        let formatted_output = format_flowchart(&written).expect("writer output should format");
        assert_eq!(
            formatted_output, formatted_input,
            "formatted input and formatted writeback should match"
        );
        assert_formatter_idempotent(&formatted_output);
    }

    #[test]
    fn writes_annotations_and_roundtrips_flowchart_subset() {
        let input = r##"flowchart TD
A[Start] --> B[Review]
subgraph Ops[Operations]
    C[Ship]
end
%% @graph width_cm="18" height_cm="10" font_face="Aptos"
%% @node B x="120" y="80" w="140" h="64" fill="#fff4cc"
%% @edge A->B start_arrow="circle" end_arrow="cross" start_connection="right" end_connection="left" path_mode="orthogonal" bend_points="180,65|180,230"
%% @group Ops x="40" y="50" w="540" h="260"
"##;

        let db = parse(input).expect("should parse original source");
        let written = write(&db);
        let reparsed = parse(&written).expect("written source should parse again");

        assert!(written.contains("%% @edge A->B"));
        assert!(written.contains(r#"start_arrow="circle""#));
        assert!(written.contains(r#"end_arrow="cross""#));
        assert!(written.contains(r#"bend_points="180,65|180,230""#));

        let edge = reparsed.edge_annotation_overrides_for(&reparsed.edges()[0]);
        assert_eq!(edge.start_arrow, Some(ArrowHead::Circle));
        assert_eq!(edge.end_arrow, Some(ArrowHead::Cross));
        assert_eq!(edge.start_connection, Some(ConnectionPoint::Right));
        assert_eq!(edge.end_connection, Some(ConnectionPoint::Left));
        assert_eq!(edge.path_mode.as_deref(), Some("orthogonal"));
        assert_eq!(edge.bend_points.len(), 2);

        let graph = reparsed.graph_annotation_overrides();
        assert_eq!(graph.width_cm, Some(18.0));
        assert_eq!(graph.height_cm, Some(10.0));
        assert_eq!(graph.font_face.as_deref(), Some("Aptos"));

        let group = reparsed.group_annotation_overrides("Ops");
        assert_eq!(group.x, Some(40.0));
        assert_eq!(group.width, Some(540.0));
    }

    #[test]
    fn canonical_annotated_flowchart_survives_exact_roundtrip() {
        let input = r##"flowchart TB
A[Start]
subgraph Ops[Operations]
    B[Review]
    C[Ship]
end
A e1@--> B
B o--o C
%% @graph width_cm="18" height_cm="10" font_face="Aptos"
%% @node B x="120" y="80" fill="#fff4cc"
%% @edge A->B end_arrow="circle" path_mode="orthogonal" bend_points="180,65|180,230"
%% @group Ops x="40" y="50" w="540" h="260"
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_shape_and_markdown_flowchart_survives_exact_roundtrip() {
        let input = r#"flowchart LR
A[Square]
B(Round)
C((Circle))
D{Diamond}
E[(Cylinder)]
F([Stadium])
G{{Hexagon}}
H[[Subroutine]]
I[/Lean Right/]
J[\Lean Left\]
K["`Markdown node`"]
"#;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_duplicate_edge_flowchart_survives_exact_roundtrip() {
        let input = r##"flowchart TB
A[Start]
B[Review]
A -->|first| B
A -->|second| B
%% @edge A->B#2 end_arrow="circle" line_color="#3366cc"
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_top_level_vertex_order_survives_exact_roundtrip() {
        let input = r##"flowchart TB
C[Ship]
A[Start]
B[Review]
A --> B
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_subgraph_member_order_survives_exact_roundtrip() {
        let input = r##"flowchart TB
subgraph Ops[Operations]
    direction LR
    C[Ship]
    A[Start]
    B[Review]
end
%% @group Ops x="40" y="50" w="540" h="260"
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_nested_subgraph_survives_exact_roundtrip() {
        let input = r##"flowchart LR
subgraph outer[Outer]
    subgraph inner[Inner]
        A[Start]
        B[Review]
    end
    C[Ship]
end
%% @group outer x="20" y="30" w="500" h="320"
%% @group inner x="80" y="90" w="220" h="160"
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn canonical_top_level_subgraph_then_node_survives_exact_roundtrip() {
        let input = r##"flowchart TB
subgraph Ops[Operations]
    A[Start]
end
C[Ship]
"##;

        assert_exact_roundtrip(input);
    }

    #[test]
    fn formatter_canonicalizes_direction_aliases_for_diff_friendly_output() {
        let input = r##"flowchart TD
A[Start] --> B[Finish]
%% @edge A->B end_arrow="none"
"##;

        let expected = r##"flowchart TB
A[Start]
B[Finish]
A --> B
%% @edge A->B end_arrow="none"
"##;

        let formatted = format_flowchart(input).expect("source should format");
        assert_eq!(formatted, expected);
        assert_formatter_equivalent_roundtrip(input);
    }

    #[test]
    fn formatter_equates_inline_edge_shorthand_roundtrip() {
        let input = r##"flowchart TD
A[Start] --> B[Finish]
C[Ship] --> D[Deliver]
"##;

        assert_formatter_equivalent_roundtrip(input);
    }

    #[test]
    fn formatter_equates_nested_annotated_noncanonical_roundtrip() {
        let input = r##"flowchart TD
subgraph outer[Outer]
    subgraph inner[Inner]
        A[Start] --> B[Review]
        A --> B
    end
    C[Ship]
end
D[Deliver]
%% @graph width_cm="18" height_cm="10" font_face="Aptos"
%% @edge A->B#2 line_color="#3366cc" line_style="dash" end_arrow="circle"
%% @group outer x="20" y="30" w="500" h="320"
%% @group inner x="80" y="90" w="220" h="160"
"##;

        assert_formatter_equivalent_roundtrip(input);
    }

    #[test]
    fn formatter_equates_top_level_mixed_subgraph_node_and_annotations() {
        let input = r##"flowchart TD
subgraph Ops[Operations]
    A[Start]
end
C[Ship] --> D[Deliver]
%% @node D x="240" y="160" fill="#eef6ff"
%% @edge C->D path_mode="straight" end_arrow="cross"
"##;

        assert_formatter_equivalent_roundtrip(input);
    }

    #[test]
    fn formatter_is_idempotent_for_canonical_nested_subgraph() {
        let input = r##"flowchart LR
subgraph outer[Outer]
    subgraph inner[Inner]
        A[Start]
        B[Review]
    end
    C[Ship]
end
%% @group outer x="20" y="30" w="500" h="320"
%% @group inner x="80" y="90" w="220" h="160"
"##;

        assert_formatter_idempotent(input);
    }

    #[test]
    fn flowchart_db_to_mermaid_uses_writer() {
        let input = r##"flowchart TD
A[Start] --> B[Finish]
%% @edge A->B end_arrow="none"
"##;
        let db = parse(input).expect("should parse source");
        let written = db.to_mermaid();

        assert!(written.starts_with("flowchart TB"));
        assert!(written.contains(r#"%% @edge A->B end_arrow="none""#));
    }

    #[test]
    fn writer_roundtrips_vertex_shapes_and_markdown_labels() {
        let input = r#"flowchart LR
A[Square]
B(Round)
C((Circle))
D{Diamond}
E[(Cylinder)]
F([Stadium])
G{{Hexagon}}
H[[Subroutine]]
I[/Lean Right/]
J[\Lean Left\]
K["`Markdown node`"]
"#;

        let db = parse(input).expect("should parse original shape source");
        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("written shape source should parse");

        let expected = [
            ("A", FlowVertexType::Square),
            ("B", FlowVertexType::Round),
            ("C", FlowVertexType::Circle),
            ("D", FlowVertexType::Diamond),
            ("E", FlowVertexType::Cylinder),
            ("F", FlowVertexType::Stadium),
            ("G", FlowVertexType::Hexagon),
            ("H", FlowVertexType::Subroutine),
            ("I", FlowVertexType::LeanRight),
            ("J", FlowVertexType::LeanLeft),
            ("K", FlowVertexType::Square),
        ];

        for (id, shape) in expected {
            let vertex = reparsed.vertices().get(id).expect("vertex should exist");
            assert_eq!(
                vertex.vertex_type,
                Some(shape.clone()),
                "writer should preserve shape for {id}. source:\n{written}"
            );
        }

        let markdown = reparsed
            .vertices()
            .get("K")
            .expect("markdown node should exist");
        assert_eq!(markdown.label_type, FlowTextType::Markdown);
        assert!(written.contains(r#"K["`Markdown node`"]"#));
    }

    #[test]
    fn writer_roundtrips_edge_ids_and_arrow_variants() {
        let input = r##"flowchart LR
A e1@--> B
B o--o C
C x--x D
D -.-> E
"##;

        let db = parse(input).expect("should parse edge source");
        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("written edge source should parse");

        assert!(written.contains("e1@-->"));

        let edges = reparsed.edges();
        assert_eq!(edges.len(), 4);
        assert_eq!(edges[0].id.as_deref(), Some("e1"));
        assert!(edges[0].is_user_defined_id);
        assert_eq!(edges[0].edge_type.as_deref(), Some("arrow_point"));
        assert_eq!(edges[1].edge_type.as_deref(), Some("double_arrow_circle"));
        assert_eq!(edges[2].edge_type.as_deref(), Some("double_arrow_cross"));
        assert_eq!(edges[3].stroke, EdgeStroke::Dotted);
        assert_eq!(edges[3].edge_type.as_deref(), Some("arrow_point"));
    }

    #[test]
    fn writer_roundtrips_subgraph_direction_and_members() {
        let input = r#"flowchart TB
subgraph Ops[Operations]
    direction LR
    A[Start]
    B[Review]
end
A --> B
"#;

        let db = parse(input).expect("should parse subgraph source");
        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("written subgraph source should parse");

        assert!(written.contains("subgraph Ops[Operations]"));
        assert!(written.contains("    direction LR"));
        assert_eq!(reparsed.subgraphs().len(), 1);
        assert_eq!(reparsed.subgraphs()[0].dir.as_deref(), Some("LR"));
        assert!(reparsed.subgraphs()[0].nodes.contains(&"A".to_string()));
        assert!(reparsed.subgraphs()[0].nodes.contains(&"B".to_string()));
    }

    #[test]
    fn edit_write_parse_render_roundtrip_updates_result() {
        let input = r##"flowchart TD
A[Start] --> B[Review]
%% @node B x="120" y="80" fill="#fff4cc"
%% @edge A->B end_arrow="cross"
"##;

        let mut db = parse(input).expect("should parse editable source");

        assert!(db.set_vertex_text_obj("B", FlowText::markdown("Updated Review")));
        db.set_annotation_attr(
            FlowAnnotationTarget::Node {
                id: "B".to_string(),
            },
            "fill",
            "#cfe8ff",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Node {
                id: "B".to_string(),
            },
            "x",
            "210",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "A".to_string(),
                to: "B".to_string(),
                ordinal: 1,
            },
            "end_arrow",
            "circle",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "A".to_string(),
                to: "B".to_string(),
                ordinal: 1,
            },
            "path_mode",
            "orthogonal",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "A".to_string(),
                to: "B".to_string(),
                ordinal: 1,
            },
            "bend_points",
            "180,65|180,140",
        );

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("edited Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("edited Mermaid should render");

        let vertex = reparsed.vertices().get("B").expect("B should exist");
        assert_eq!(vertex.label_type, FlowTextType::Markdown);
        assert_eq!(vertex.text.as_deref(), Some("Updated Review"));

        let node = reparsed.node_annotation_overrides("B");
        assert_eq!(node.fill.as_deref(), Some("#cfe8ff"));
        assert_eq!(node.x, Some(210.0));

        let edge = reparsed.edge_annotation_overrides_for(&reparsed.edges()[0]);
        assert_eq!(edge.end_arrow, Some(ArrowHead::Circle));
        assert_eq!(edge.path_mode.as_deref(), Some("orthogonal"));
        assert_eq!(edge.bend_points.len(), 2);

        assert!(written.contains(r#"B["`Updated Review`"]"#));
        assert!(written.contains(r#"%% @edge A->B end_arrow="circle""#));
        assert!(written.contains(r#"bend_points="180,65|180,140""#));

        assert!(
            rerendered.contains("Updated Review"),
            "Rendered SVG should reflect the edited label. SVG:\n{}",
            rerendered
        );
        assert!(
            rerendered.contains("marker-end=\"url(#arrow_circle)\""),
            "Rendered SVG should reflect the edited end arrow. SVG:\n{}",
            rerendered
        );
        assert!(
            rerendered.contains("fill: #cfe8ff") || rerendered.contains("fill:#cfe8ff"),
            "Rendered SVG should reflect the edited node fill. SVG:\n{}",
            rerendered
        );
    }

    #[test]
    fn edit_roundtrip_preserves_group_membership_changes() {
        let input = r##"flowchart TD
A[Start]
B[Review]
subgraph Ops[Operations]
    C[Ship]
end
%% @node B x="35" y="55" fill="#eef6ff"
"##;

        let mut db = parse(input).expect("should parse editable group source");
        assert!(db.move_vertex_to_subgraph("B", Some("Ops")));
        db.set_annotation_attr(
            FlowAnnotationTarget::Group {
                id: "Ops".to_string(),
            },
            "x",
            "40",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Group {
                id: "Ops".to_string(),
            },
            "y",
            "60",
        );

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("edited Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("edited Mermaid should render");

        let ops = reparsed
            .subgraphs()
            .iter()
            .find(|sg| sg.id == "Ops")
            .expect("Ops subgraph should exist");
        assert!(ops.nodes.contains(&"B".to_string()));
        assert!(ops.nodes.contains(&"C".to_string()));
        assert!(!written.contains("\nB[Review]\n"));
        assert!(written.contains("subgraph Ops[Operations]"));
        assert!(written.contains("    B[Review]"));

        let group = reparsed.group_annotation_overrides("Ops");
        assert_eq!(group.x, Some(40.0));
        assert_eq!(group.y, Some(60.0));
        assert!(
            rerendered.contains("subgraph-Ops"),
            "Rendered SVG should still contain the edited subgraph. SVG:\n{}",
            rerendered
        );
    }

    #[test]
    fn edit_roundtrip_preserves_edge_topology_changes() {
        let input = r##"flowchart TD
A[Start] --> B[Review]
%% @edge A->B end_arrow="cross" line_color="#3366cc"
"##;

        let mut db = parse(input).expect("should parse editable topology source");
        let removed = db.remove_edge_at(0).expect("edge should be removed");
        assert_eq!(removed.start, "A");
        assert_eq!(removed.end, "B");

        db.add_vertex_simple("C", Some("Ship"), Some(FlowVertexType::Rect));
        db.add_edge("B", "C", "-->", Some("next"), None);
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "B".to_string(),
                to: "C".to_string(),
                ordinal: 1,
            },
            "end_arrow",
            "circle",
        );
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "B".to_string(),
                to: "C".to_string(),
                ordinal: 1,
            },
            "line_color",
            "#228833",
        );

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("edited topology Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("edited topology Mermaid should render");

        assert!(!written.contains(r#"%% @edge A->B end_arrow="cross""#));
        assert!(written.contains("B -->|next| C"));
        assert!(written.contains(r#"%% @edge B->C end_arrow="circle""#));

        assert_eq!(reparsed.edges().len(), 1);
        assert_eq!(reparsed.edges()[0].start, "B");
        assert_eq!(reparsed.edges()[0].end, "C");

        let edge = reparsed.edge_annotation_overrides_for(&reparsed.edges()[0]);
        assert_eq!(edge.end_arrow, Some(ArrowHead::Circle));
        assert_eq!(edge.line_color.as_deref(), Some("#228833"));

        assert!(
            rerendered.contains("marker-end=\"url(#arrow_circle)\""),
            "Rendered SVG should reflect the new topology edge annotation. SVG:\n{}",
            rerendered
        );
        assert!(
            rerendered.contains("next"),
            "Rendered SVG should reflect the replacement edge label. SVG:\n{}",
            rerendered
        );
    }

    #[test]
    fn edit_roundtrip_retargets_duplicate_edge_annotations_after_removal() {
        let input = r##"flowchart TD
A[Start]
B[Review]
A -->|first| B
A -->|second| B
%% @edge A->B#2 end_arrow="circle" line_color="#3366cc"
"##;

        let mut db = parse(input).expect("should parse duplicate-edge source");
        let removed = db
            .remove_edge_at(0)
            .expect("first duplicate edge should be removed");
        assert_eq!(removed.text, "first");

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("retargeted Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("retargeted Mermaid should render");

        assert_eq!(reparsed.edges().len(), 1);
        assert_eq!(reparsed.edges()[0].text, "second");
        assert!(written.contains("A -->|second| B"));
        assert!(!written.contains("A -->|first| B"));
        assert!(written.contains(r##"%% @edge A->B end_arrow="circle" line_color="#3366cc""##));
        assert!(!written.contains("%% @edge A->B#2"));

        let edge = reparsed.edge_annotation_overrides_for(&reparsed.edges()[0]);
        assert_eq!(edge.end_arrow, Some(ArrowHead::Circle));
        assert_eq!(edge.line_color.as_deref(), Some("#3366cc"));

        assert!(
            rerendered.contains("marker-end=\"url(#arrow_circle)\""),
            "Rendered SVG should keep the retargeted duplicate-edge marker. SVG:\n{}",
            rerendered
        );
        assert!(
            rerendered.contains("second"),
            "Rendered SVG should reflect the surviving duplicate edge label. SVG:\n{}",
            rerendered
        );
    }

    #[test]
    fn edit_roundtrip_preserves_nested_group_membership_changes() {
        let input = r##"flowchart TB
subgraph outer[Outer]
    subgraph inner[Inner]
        A[Start]
        B[Review]
    end
    C[Ship]
end
%% @group outer x="20" y="30" w="500" h="320"
%% @group inner x="80" y="90" w="220" h="160"
"##;

        let mut db = parse(input).expect("should parse nested editable source");
        assert!(db.move_vertex_to_subgraph("C", Some("inner")));
        db.set_annotation_attr(
            FlowAnnotationTarget::Node {
                id: "C".to_string(),
            },
            "fill",
            "#d9f2d9",
        );

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("edited nested Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("edited nested Mermaid should render");

        let inner = reparsed
            .subgraphs()
            .iter()
            .find(|sg| sg.id == "inner")
            .expect("inner subgraph should exist");
        assert!(inner.nodes.contains(&"A".to_string()));
        assert!(inner.nodes.contains(&"B".to_string()));
        assert!(inner.nodes.contains(&"C".to_string()));
        assert!(matches!(
            inner.items.last(),
            Some(FlowSubGraphItem::Node(id)) if id == "C"
        ));

        let node = reparsed.node_annotation_overrides("C");
        assert_eq!(node.fill.as_deref(), Some("#d9f2d9"));

        assert!(written.contains("subgraph inner[Inner]"));
        assert!(written.contains("        C[Ship]"));
        assert!(!written.contains("\n    C[Ship]\nend"));
        assert!(
            rerendered.contains("subgraph-inner"),
            "Rendered SVG should still contain the nested subgraph. SVG:\n{}",
            rerendered
        );
    }

    #[test]
    fn edit_roundtrip_preserves_nested_edge_placement_changes() {
        let input = r##"flowchart TB
subgraph outer[Outer]
    subgraph inner[Inner]
        A[Start]
        B[Review]
    end
    C[Ship]
end
"##;

        let mut db = parse(input).expect("should parse nested edge-edit source");
        db.add_edge("A", "B", "-->", Some("retry"), None);
        let new_edge_index = db.edges().len() - 1;
        assert!(db.move_edge_to_subgraph(new_edge_index, Some("inner")));
        db.set_annotation_attr(
            FlowAnnotationTarget::Edge {
                from: "A".to_string(),
                to: "B".to_string(),
                ordinal: 1,
            },
            "end_arrow",
            "circle",
        );

        let written = db.to_mermaid();
        let reparsed = parse(&written).expect("edited nested-edge Mermaid should parse");
        let rerendered =
            crate::render::render_text(&written).expect("edited nested-edge Mermaid should render");

        let inner = reparsed
            .subgraphs()
            .iter()
            .find(|sg| sg.id == "inner")
            .expect("inner subgraph should exist");
        assert!(matches!(
            inner.items.last(),
            Some(FlowSubGraphItem::Edge(id)) if id.starts_with("L-A-B-")
        ));
        assert!(written.contains("        A -->|retry| B"));

        let retry_edge = reparsed
            .edges()
            .iter()
            .find(|edge| edge.text == "retry")
            .expect("retry edge should exist");
        let edge = reparsed.edge_annotation_overrides_for(retry_edge);
        assert_eq!(edge.end_arrow, Some(ArrowHead::Circle));

        assert!(
            rerendered.contains("marker-end=\"url(#arrow_circle)\""),
            "Rendered SVG should reflect the nested edge annotation. SVG:\n{}",
            rerendered
        );
        assert!(
            rerendered.contains("retry"),
            "Rendered SVG should reflect the nested edge label. SVG:\n{}",
            rerendered
        );
    }
}
