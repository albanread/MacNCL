//! Flowchart annotations and typed override helpers

use crate::layout::Point;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowAnnotationTarget {
    Graph,
    Node {
        id: String,
    },
    Edge {
        from: String,
        to: String,
        ordinal: u32,
    },
    Group {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowAnnotationAttr {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowAnnotation {
    pub target: FlowAnnotationTarget,
    pub attrs: Vec<FlowAnnotationAttr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelAlign {
    Left,
    Center,
    Right,
}

impl LabelAlign {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "centre" | "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPoint {
    Top,
    Right,
    Bottom,
    Left,
    Center,
}

impl ConnectionPoint {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "top" => Some(Self::Top),
            "right" => Some(Self::Right),
            "bottom" => Some(Self::Bottom),
            "left" => Some(Self::Left),
            "center" | "centre" => Some(Self::Center),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Solid,
    Dash,
    Dot,
}

impl LineStyle {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "solid" => Some(Self::Solid),
            "dash" | "dashed" => Some(Self::Dash),
            "dot" | "dotted" => Some(Self::Dot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowHead {
    None,
    Point,
    Circle,
    Cross,
}

impl ArrowHead {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "open" => Some(Self::None),
            "point" | "arrow" | "triangle" => Some(Self::Point),
            "circle" => Some(Self::Circle),
            "cross" | "x" => Some(Self::Cross),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphAnnotationOverrides {
    pub width_cm: Option<f64>,
    pub height_cm: Option<f64>,
    pub canvas_fill: Option<String>,
    pub font_face: Option<String>,
    pub node_label_font_size: Option<f64>,
    pub group_label_font_size: Option<f64>,
    pub edge_label_font_size: Option<f64>,
    pub node_label_align: Option<LabelAlign>,
    pub group_label_align: Option<LabelAlign>,
    pub edge_label_align: Option<LabelAlign>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeAnnotationOverrides {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub fill: Option<String>,
    pub stroke: Option<String>,
    pub line_width: Option<f64>,
    pub label_align: Option<LabelAlign>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EdgeAnnotationOverrides {
    pub line_color: Option<String>,
    pub line_width: Option<f64>,
    pub line_style: Option<LineStyle>,
    pub start_arrow: Option<ArrowHead>,
    pub end_arrow: Option<ArrowHead>,
    pub start_connection: Option<ConnectionPoint>,
    pub end_connection: Option<ConnectionPoint>,
    pub path_mode: Option<String>,
    pub bend_points: Vec<Point>,
    pub label_offset: Option<Point>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GroupAnnotationOverrides {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub fill: Option<String>,
    pub stroke: Option<String>,
    pub label_align: Option<LabelAlign>,
}

pub fn parse_annotation_comment(comment: &str) -> Result<Option<FlowAnnotation>, String> {
    let body = comment
        .trim()
        .strip_prefix("%%")
        .map(str::trim)
        .ok_or_else(|| "annotation comments must start with %%".to_string())?;

    if !body.starts_with('@') {
        return Ok(None);
    }

    let tokens = tokenize_annotation_body(body)?;
    if tokens.is_empty() {
        return Ok(None);
    }

    let (target, attr_start) = match tokens[0].as_str() {
        "@graph" => (FlowAnnotationTarget::Graph, 1),
        "@node" => {
            let id = tokens
                .get(1)
                .ok_or_else(|| "node annotation is missing a target id".to_string())?;
            (FlowAnnotationTarget::Node { id: id.clone() }, 2)
        }
        "@group" => {
            let id = tokens
                .get(1)
                .ok_or_else(|| "group annotation is missing a target id".to_string())?;
            (FlowAnnotationTarget::Group { id: id.clone() }, 2)
        }
        "@edge" => {
            let edge_target = tokens
                .get(1)
                .ok_or_else(|| "edge annotation is missing a target reference".to_string())?;
            (parse_edge_target(edge_target)?, 2)
        }
        _ => return Ok(None),
    };

    let attrs = parse_attrs(&tokens[attr_start..])?;
    if attrs.is_empty() {
        return Err(format!(
            "annotation {:?} must contain at least one key=value pair",
            target
        ));
    }

    Ok(Some(FlowAnnotation { target, attrs }))
}

pub(crate) fn annotation_f64(attrs: &[FlowAnnotationAttr], key: &str) -> Option<f64> {
    annotation_string(attrs, key).and_then(|value| value.parse::<f64>().ok())
}

pub(crate) fn annotation_string(attrs: &[FlowAnnotationAttr], key: &str) -> Option<String> {
    attrs
        .iter()
        .rev()
        .find(|attr| attr.key == key)
        .map(|attr| attr.value.clone())
}

pub(crate) fn annotation_label_align(
    attrs: &[FlowAnnotationAttr],
    key: &str,
) -> Option<LabelAlign> {
    annotation_string(attrs, key).and_then(|value| LabelAlign::parse(&value))
}

pub(crate) fn annotation_connection_point(
    attrs: &[FlowAnnotationAttr],
    key: &str,
) -> Option<ConnectionPoint> {
    annotation_string(attrs, key).and_then(|value| ConnectionPoint::parse(&value))
}

pub(crate) fn annotation_line_style(attrs: &[FlowAnnotationAttr], key: &str) -> Option<LineStyle> {
    annotation_string(attrs, key).and_then(|value| LineStyle::parse(&value))
}

pub(crate) fn annotation_arrow_head(attrs: &[FlowAnnotationAttr], key: &str) -> Option<ArrowHead> {
    annotation_string(attrs, key).and_then(|value| ArrowHead::parse(&value))
}

pub(crate) fn annotation_point_pair(x: Option<f64>, y: Option<f64>) -> Option<Point> {
    match (x, y) {
        (Some(x), Some(y)) => Some(Point::new(x, y)),
        _ => None,
    }
}

pub(crate) fn annotation_points(attrs: &[FlowAnnotationAttr], key: &str) -> Option<Vec<Point>> {
    let raw = annotation_string(attrs, key)?;
    parse_points(&raw)
}

fn parse_points(raw: &str) -> Option<Vec<Point>> {
    let mut points = Vec::new();
    for pair in raw
        .split([';', '|'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let mut coords = pair.split(',').map(str::trim);
        let x = coords.next()?.parse::<f64>().ok()?;
        let y = coords.next()?.parse::<f64>().ok()?;
        if coords.next().is_some() {
            return None;
        }
        points.push(Point::new(x, y));
    }
    Some(points)
}

fn tokenize_annotation_body(body: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = body.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                current.push(ch);
                in_quotes = !in_quotes;
            }
            '\\' if in_quotes => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if in_quotes {
        return Err("unterminated quoted annotation value".to_string());
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

fn parse_edge_target(token: &str) -> Result<FlowAnnotationTarget, String> {
    let (base, ordinal) = match token.split_once('#') {
        Some((base, ordinal)) => {
            let parsed = ordinal
                .parse::<u32>()
                .map_err(|_| format!("invalid edge ordinal in annotation target `{token}`"))?;
            (base, parsed)
        }
        None => (token, 1),
    };

    let (from, to) = base
        .split_once("->")
        .ok_or_else(|| format!("edge annotation target `{token}` must look like A->B or A->B#2"))?;

    if from.trim().is_empty() || to.trim().is_empty() {
        return Err(format!(
            "edge annotation target `{token}` must include both start and end node ids"
        ));
    }

    Ok(FlowAnnotationTarget::Edge {
        from: from.trim().to_string(),
        to: to.trim().to_string(),
        ordinal,
    })
}

fn parse_attrs(tokens: &[String]) -> Result<Vec<FlowAnnotationAttr>, String> {
    let mut attrs = Vec::new();
    for token in tokens {
        let (key, raw_value) = token
            .split_once('=')
            .ok_or_else(|| format!("annotation token `{token}` is not a key=value pair"))?;
        if key.trim().is_empty() {
            return Err(format!("annotation token `{token}` is missing a key"));
        }
        attrs.push(FlowAnnotationAttr {
            key: key.trim().to_string(),
            value: unquote_value(raw_value.trim())?,
        });
    }
    Ok(attrs)
}

fn unquote_value(value: &str) -> Result<String, String> {
    if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return Err(format!("annotation value `{value}` has mismatched quotes"));
        }
        let inner = &value[1..value.len() - 1];
        Ok(inner.replace("\\\"", "\""))
    } else {
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_annotation_comment() {
        let parsed = parse_annotation_comment(r##"%% @node Review x="120" y="80" fill="#fff4cc""##)
            .unwrap()
            .unwrap();

        assert_eq!(
            parsed.target,
            FlowAnnotationTarget::Node {
                id: "Review".to_string()
            }
        );
        assert_eq!(annotation_f64(&parsed.attrs, "x"), Some(120.0));
        assert_eq!(annotation_f64(&parsed.attrs, "y"), Some(80.0));
        assert_eq!(
            annotation_string(&parsed.attrs, "fill"),
            Some("#fff4cc".to_string())
        );
    }

    #[test]
    fn parses_edge_annotation_comment_with_ordinal() {
        let parsed =
            parse_annotation_comment(r##"%% @edge A->B#2 line_color="#3366cc" line_style="dash""##)
                .unwrap()
                .unwrap();

        assert_eq!(
            parsed.target,
            FlowAnnotationTarget::Edge {
                from: "A".to_string(),
                to: "B".to_string(),
                ordinal: 2
            }
        );
        assert_eq!(
            annotation_string(&parsed.attrs, "line_color"),
            Some("#3366cc".to_string())
        );
        assert_eq!(
            annotation_line_style(&parsed.attrs, "line_style"),
            Some(LineStyle::Dash)
        );
    }

    #[test]
    fn parses_arrow_head_annotation_values() {
        let parsed =
            parse_annotation_comment(r#"%% @edge A->B start_arrow="circle" end_arrow="cross""#)
                .unwrap()
                .unwrap();

        assert_eq!(
            annotation_arrow_head(&parsed.attrs, "start_arrow"),
            Some(ArrowHead::Circle)
        );
        assert_eq!(
            annotation_arrow_head(&parsed.attrs, "end_arrow"),
            Some(ArrowHead::Cross)
        );
    }

    #[test]
    fn parses_bend_points_and_label_offset() {
        let parsed = parse_annotation_comment(
            r#"%% @edge A->B bend_points="10,20;30,40" label_offset_x="12" label_offset_y="-8""#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            annotation_points(&parsed.attrs, "bend_points"),
            Some(vec![Point::new(10.0, 20.0), Point::new(30.0, 40.0)])
        );
        assert_eq!(
            annotation_point_pair(
                annotation_f64(&parsed.attrs, "label_offset_x"),
                annotation_f64(&parsed.attrs, "label_offset_y")
            ),
            Some(Point::new(12.0, -8.0))
        );
    }

    #[test]
    fn parses_bend_points_with_pipe_separator() {
        let parsed = parse_annotation_comment(r#"%% @edge A->B bend_points="10,20|30,40|50,60""#)
            .unwrap()
            .unwrap();

        assert_eq!(
            annotation_points(&parsed.attrs, "bend_points"),
            Some(vec![
                Point::new(10.0, 20.0),
                Point::new(30.0, 40.0),
                Point::new(50.0, 60.0),
            ])
        );
    }
}
