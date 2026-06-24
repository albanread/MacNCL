//! DocCrate-owned manual layout comments for Mermaid diagrams.
//!
//! These comments are intentionally outside Mermaid's grammar. Selkie still
//! parses and lays out the diagram first; DocCrate then applies these overrides
//! to the resolved IR for diagram types that opt in.

use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManualLayoutOverrides {
    pub graph: GraphOverride,
    objects: HashMap<String, BoxOverride>,
    groups: HashMap<String, BoxOverride>,
    edges: HashMap<String, EdgeOverride>,
}

impl ManualLayoutOverrides {
    pub fn object(&self, id: &str) -> Option<&BoxOverride> {
        self.objects.get(id)
    }

    pub fn objects(&self) -> impl Iterator<Item = (&String, &BoxOverride)> {
        self.objects.iter()
    }

    pub fn group(&self, id: &str) -> Option<&BoxOverride> {
        self.groups.get(id)
    }

    pub fn edge(&self, from: &str, to: &str) -> Option<&EdgeOverride> {
        self.edges.get(&edge_key(from, to))
    }

    fn merge_object(&mut self, id: String, ov: BoxOverride) {
        self.objects.entry(id).or_default().merge(ov);
    }

    fn merge_group(&mut self, id: String, ov: BoxOverride) {
        self.groups.entry(id).or_default().merge(ov);
    }

    fn merge_edge(&mut self, ov: EdgeOverride) {
        self.edges.insert(edge_key(&ov.from, &ov.to), ov);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GraphOverride {
    pub w: Option<f32>,
    pub h: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BoxOverride {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub w: Option<f32>,
    pub h: Option<f32>,
}

impl BoxOverride {
    fn merge(&mut self, other: Self) {
        if other.x.is_some() {
            self.x = other.x;
        }
        if other.y.is_some() {
            self.y = other.y;
        }
        if other.w.is_some() {
            self.w = other.w;
        }
        if other.h.is_some() {
            self.h = other.h;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeOverride {
    pub from: String,
    pub to: String,
    /// Complete polyline in diagram coordinates. When set, this replaces the
    /// renderer's route entirely.
    pub points: Option<Vec<(f32, f32)>>,
    /// Interior bend points. When set without `points`, DocCrate keeps the
    /// normal source/target ports and inserts these bends between them.
    pub bend_points: Vec<(f32, f32)>,
    /// Absolute label center in diagram coordinates.
    pub label_pos: Option<(f32, f32)>,
    /// Relative label nudge from the route midpoint.
    pub label_offset: Option<(f32, f32)>,
}

pub fn parse(source: &str) -> Result<ManualLayoutOverrides, String> {
    let mut out = ManualLayoutOverrides::default();
    for (idx, line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        let Some(body) = trimmed.strip_prefix("%%").map(str::trim) else {
            continue;
        };
        if !body.starts_with('@') {
            continue;
        }
        parse_comment_body(line_no, body, &mut out)?;
    }
    Ok(out)
}

fn parse_comment_body(
    line_no: usize,
    body: &str,
    out: &mut ManualLayoutOverrides,
) -> Result<(), String> {
    let tokens = tokenize(body).map_err(|e| ferr(line_no, &e))?;
    if tokens.is_empty() {
        return Ok(());
    }

    let target = tokens[0].trim_start_matches('@').to_ascii_lowercase();
    match target.as_str() {
        "service" | "junction" | "node" | "object" | "note" => {
            let id = required_token(line_no, &tokens, 1, "@service needs an id")?;
            let attrs = parse_attrs(line_no, &tokens[2..])?;
            out.merge_object(id.to_string(), box_override(line_no, &attrs)?);
        }
        "group" => {
            let id = required_token(line_no, &tokens, 1, "@group needs an id")?;
            let attrs = parse_attrs(line_no, &tokens[2..])?;
            out.merge_group(id.to_string(), box_override(line_no, &attrs)?);
        }
        "edge" | "rel" | "relationship" => {
            let spec = required_token(line_no, &tokens, 1, "@edge needs from->to")?;
            let (from, to) = spec
                .split_once("->")
                .ok_or_else(|| ferr(line_no, "@edge target must look like from->to"))?;
            let attrs = parse_attrs(line_no, &tokens[2..])?;
            out.merge_edge(edge_override(line_no, from, to, &attrs)?);
        }
        "graph" => {
            let attrs = parse_attrs(line_no, &tokens[1..])?;
            out.graph.w = first_f32(line_no, &attrs, &["w", "width"])?;
            out.graph.h = first_f32(line_no, &attrs, &["h", "height"])?;
        }
        other => {
            return Err(ferr(
                line_no,
                &format!("unknown manual layout target `@{other}`"),
            ))
        }
    }
    Ok(())
}

fn tokenize(body: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in body.chars() {
        if let Some(q) = quote {
            if escaped {
                token.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            } else {
                token.push(ch);
            }
            continue;
        }

        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch.is_ascii_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }

    if quote.is_some() {
        return Err("unterminated quoted value".to_string());
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn parse_attrs(line_no: usize, tokens: &[String]) -> Result<HashMap<String, String>, String> {
    let mut attrs = HashMap::new();
    for token in tokens {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| ferr(line_no, &format!("attribute `{token}` needs key=value")))?;
        if key.trim().is_empty() {
            return Err(ferr(line_no, "attribute key cannot be empty"));
        }
        attrs.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    Ok(attrs)
}

fn box_override(line_no: usize, attrs: &HashMap<String, String>) -> Result<BoxOverride, String> {
    Ok(BoxOverride {
        x: first_f32(line_no, attrs, &["x"])?,
        y: first_f32(line_no, attrs, &["y"])?,
        w: first_f32(line_no, attrs, &["w", "width"])?,
        h: first_f32(line_no, attrs, &["h", "height"])?,
    })
}

fn edge_override(
    line_no: usize,
    from: &str,
    to: &str,
    attrs: &HashMap<String, String>,
) -> Result<EdgeOverride, String> {
    let points = first_string(attrs, &["points", "path"])
        .map(|value| parse_points(line_no, "points", value))
        .transpose()?;
    let bend_points = first_string(attrs, &["bend_points", "bends"])
        .map(|value| parse_points(line_no, "bend_points", value))
        .transpose()?
        .unwrap_or_default();
    let label_pos = point_from_pair_or_axes(
        line_no,
        attrs,
        &["label_pos", "label_position"],
        &["label_x"],
        &["label_y"],
        false,
    )?;
    let label_offset = point_from_pair_or_axes(
        line_no,
        attrs,
        &["label_offset", "label_delta"],
        &["label_offset_x", "label_dx"],
        &["label_offset_y", "label_dy"],
        true,
    )?;

    if let Some(points) = &points {
        if points.len() < 2 {
            return Err(ferr(line_no, "`points` needs at least two x,y pairs"));
        }
    }

    Ok(EdgeOverride {
        from: from.trim().to_string(),
        to: to.trim().to_string(),
        points,
        bend_points,
        label_pos,
        label_offset,
    })
}

fn point_from_pair_or_axes(
    line_no: usize,
    attrs: &HashMap<String, String>,
    pair_keys: &[&str],
    x_keys: &[&str],
    y_keys: &[&str],
    default_missing_axis_to_zero: bool,
) -> Result<Option<(f32, f32)>, String> {
    if let Some(value) = first_string(attrs, pair_keys) {
        return parse_point_pair(line_no, pair_keys[0], value).map(Some);
    }

    let x = first_f32(line_no, attrs, x_keys)?;
    let y = first_f32(line_no, attrs, y_keys)?;
    match (x, y, default_missing_axis_to_zero) {
        (Some(x), Some(y), _) => Ok(Some((x, y))),
        (Some(x), None, true) => Ok(Some((x, 0.0))),
        (None, Some(y), true) => Ok(Some((0.0, y))),
        (None, None, _) => Ok(None),
        _ => Err(ferr(
            line_no,
            &format!("`{}` and `{}` must be used together", x_keys[0], y_keys[0]),
        )),
    }
}

fn parse_point_pair(line_no: usize, key: &str, value: &str) -> Result<(f32, f32), String> {
    let (x, y) = value
        .split_once(',')
        .ok_or_else(|| ferr(line_no, &format!("`{key}` value `{value}` needs x,y")))?;
    Ok((parse_f32(line_no, key, x)?, parse_f32(line_no, key, y)?))
}

fn parse_points(line_no: usize, key: &str, value: &str) -> Result<Vec<(f32, f32)>, String> {
    let mut points = Vec::new();
    for raw in value
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ';')
        .filter(|part| !part.trim().is_empty())
    {
        points.push(parse_point_pair(line_no, key, raw)?);
    }
    if points.is_empty() {
        return Err(ferr(
            line_no,
            &format!("`{key}` needs at least one x,y pair"),
        ));
    }
    Ok(points)
}

fn first_f32(
    line_no: usize,
    attrs: &HashMap<String, String>,
    keys: &[&str],
) -> Result<Option<f32>, String> {
    first_string(attrs, keys)
        .map(|value| parse_f32(line_no, keys[0], value))
        .transpose()
}

fn first_string<'a>(attrs: &'a HashMap<String, String>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| attrs.get(*key).map(String::as_str))
}

fn parse_f32(line_no: usize, key: &str, value: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .map_err(|_| ferr(line_no, &format!("`{key}` value `{value}` is not a number")))
}

fn required_token<'a>(
    line_no: usize,
    tokens: &'a [String],
    idx: usize,
    msg: &str,
) -> Result<&'a str, String> {
    tokens
        .get(idx)
        .map(String::as_str)
        .ok_or_else(|| ferr(line_no, msg))
}

fn edge_key(from: &str, to: &str) -> String {
    format!("{}->{}", from.trim(), to.trim())
}

fn ferr(line_no: usize, msg: &str) -> String {
    format!("manual layout line {line_no}: {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_box_and_edge_overrides() {
        let source = r#"
architecture-beta
%% @service api x=120 y=80 w=150 h=110
%% @group runtime x=90 y=50 width=260 height=200
%% @edge gateway->api points="20,40 80,40 80,100" label_offset="12,-8"
%% @edge api->db bend_points="220,160;260,160" label_pos="240,120"
%% @graph w=640 h=360
"#;

        let ov = parse(source).unwrap();
        assert_eq!(ov.object("api").unwrap().x, Some(120.0));
        assert_eq!(ov.group("runtime").unwrap().w, Some(260.0));
        assert_eq!(
            ov.edge("gateway", "api").unwrap().points.as_ref().unwrap(),
            &vec![(20.0, 40.0), (80.0, 40.0), (80.0, 100.0)]
        );
        assert_eq!(
            ov.edge("api", "db").unwrap().bend_points,
            vec![(220.0, 160.0), (260.0, 160.0)]
        );
        assert_eq!(
            ov.edge("gateway", "api").unwrap().label_offset,
            Some((12.0, -8.0))
        );
        assert_eq!(
            ov.edge("api", "db").unwrap().label_pos,
            Some((240.0, 120.0))
        );
        assert_eq!(ov.graph.w, Some(640.0));
        assert_eq!(ov.graph.h, Some(360.0));
    }
}
