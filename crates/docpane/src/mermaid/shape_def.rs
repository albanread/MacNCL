//! `.shape` DSL parser and registry for renderer-defined shapes.
//!
//! The DSL is intentionally tiny — a thin wrapper over the D2D
//! `ID2D1GeometrySink` API in normalized 0..1 coordinates. Each shape file
//! defines exactly one shape:
//!
//! ```text
//! shape cloud
//!     aspect      1.6                  # width / height ratio (hard)
//!     label       0.5 0.5              # label center
//!     text-area   0.15 0.3 0.85 0.7    # optional label bounding box
//!     stroke-mult 1.0                  # optional stroke-width multiplier
//!
//!     moveto   0.10 0.60
//!     curveto  0.00 0.45  0.05 0.25  0.20 0.25
//!     ...
//!     close
//! end
//! ```
//!
//! Supported geometry commands:
//! * `moveto x y`
//! * `lineto x y`
//! * `curveto cx1 cy1  cx2 cy2  x y`     – cubic Bézier
//! * `quadto cx cy  x y`                  – quadratic Bézier
//! * `circle cx cy r`                     – additive ellipse subpath
//! * `polygon x1,y1 x2,y2 ...`            – closed polyline shorthand
//! * `close`

use std::collections::HashMap;
use std::sync::OnceLock;

/// Process-wide shape registry. Built once at startup from bundled defaults
/// + `docs/.shapes/*.shape`, then read-only — no locks needed on the read
/// path. If `init` is never called (e.g. unit tests), `registry()` returns
/// an empty registry.
static REGISTRY: OnceLock<ShapeRegistry> = OnceLock::new();

/// Install the global registry. Subsequent calls are silently ignored
/// (first-writer-wins). Safe to call before any mermaid block is parsed.
pub fn init(reg: ShapeRegistry) {
    let _ = REGISTRY.set(reg);
}

pub fn registry() -> &'static ShapeRegistry {
    REGISTRY.get_or_init(ShapeRegistry::new)
}

#[derive(Debug, Clone)]
pub enum PathCmd {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CurveTo(f32, f32, f32, f32, f32, f32),
    QuadTo(f32, f32, f32, f32),
    /// Additive sub-path: a full ellipse (`cx`, `cy`, `r`) included in the
    /// shape's filled outline.
    Circle(f32, f32, f32),
    /// Closed polyline as a single sub-path. Used for shapes with no curves.
    Polygon(Vec<(f32, f32)>),
    Close,
}

#[derive(Debug, Clone)]
pub struct ShapeDef {
    pub name: String,
    /// Hard aspect ratio (w / h). When `Some`, doccrate overrides the node
    /// dimensions before layout so the shape renders at its intended ratio.
    pub aspect: Option<f32>,
    pub label_cx: f32,
    pub label_cy: f32,
    /// Optional tighter label bounding box `(x0, y0, x1, y1)`. Defaults to
    /// the full node rectangle.
    pub text_area: Option<(f32, f32, f32, f32)>,
    pub stroke_mult: f32,
    pub commands: Vec<PathCmd>,
}

impl ShapeDef {
    pub fn label_rect(&self) -> (f32, f32, f32, f32) {
        self.text_area.unwrap_or((0.0, 0.0, 1.0, 1.0))
    }
}

#[derive(Debug, Default)]
pub struct ShapeRegistry {
    defs: Vec<ShapeDef>,
    index: HashMap<String, u32>,
}

impl ShapeRegistry {
    pub fn new() -> Self {
        Self {
            defs: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Insert or replace a shape by name. Returns the index for use in
    /// `Shape::Custom(idx)`.
    pub fn insert(&mut self, def: ShapeDef) -> u32 {
        if let Some(&idx) = self.index.get(&def.name) {
            self.defs[idx as usize] = def;
            return idx;
        }
        let idx = self.defs.len() as u32;
        self.index.insert(def.name.clone(), idx);
        self.defs.push(def);
        idx
    }

    pub fn lookup(&self, name: &str) -> Option<u32> {
        self.index.get(name).copied()
    }

    pub fn get(&self, idx: u32) -> Option<&ShapeDef> {
        self.defs.get(idx as usize)
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Convenience: parse `source` as a shape file and insert the result.
    pub fn load_text(&mut self, source: &str) -> Result<u32, String> {
        let def = parse(source)?;
        Ok(self.insert(def))
    }
}

/// Parse a single shape file. Returns the shape def or a human-readable
/// error string (line number included).
pub fn parse(source: &str) -> Result<ShapeDef, String> {
    let mut name: Option<String> = None;
    let mut aspect: Option<f32> = None;
    let mut label_cx: f32 = 0.5;
    let mut label_cy: f32 = 0.5;
    let mut text_area: Option<(f32, f32, f32, f32)> = None;
    let mut stroke_mult: f32 = 1.0;
    let mut cmds: Vec<PathCmd> = Vec::new();
    let mut saw_end = false;

    for (ln, raw_line) in source.lines().enumerate() {
        let line_no = ln + 1;
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        let mut it = line.split_ascii_whitespace();
        let head = it.next().unwrap();
        match head {
            "shape" => {
                let n = it
                    .next()
                    .ok_or_else(|| ferr(line_no, "shape needs a name"))?;
                if name.is_some() {
                    return Err(ferr(line_no, "multiple `shape` headers"));
                }
                name = Some(n.to_string());
            }
            "end" => {
                saw_end = true;
                break;
            }
            "aspect" => {
                aspect = Some(num(line_no, it.next())?);
            }
            "label" => {
                label_cx = num(line_no, it.next())?;
                label_cy = num(line_no, it.next())?;
            }
            "text-area" => {
                let x0 = num(line_no, it.next())?;
                let y0 = num(line_no, it.next())?;
                let x1 = num(line_no, it.next())?;
                let y1 = num(line_no, it.next())?;
                text_area = Some((x0, y0, x1, y1));
            }
            "stroke-mult" => {
                stroke_mult = num(line_no, it.next())?;
            }
            "moveto" => cmds.push(PathCmd::MoveTo(
                num(line_no, it.next())?,
                num(line_no, it.next())?,
            )),
            "lineto" => cmds.push(PathCmd::LineTo(
                num(line_no, it.next())?,
                num(line_no, it.next())?,
            )),
            "curveto" => cmds.push(PathCmd::CurveTo(
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
            )),
            "quadto" => cmds.push(PathCmd::QuadTo(
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
            )),
            "circle" => cmds.push(PathCmd::Circle(
                num(line_no, it.next())?,
                num(line_no, it.next())?,
                num(line_no, it.next())?,
            )),
            "polygon" => {
                let mut pts: Vec<(f32, f32)> = Vec::new();
                for tok in it {
                    let mut s = tok.splitn(2, ',');
                    let x = s
                        .next()
                        .and_then(|t| t.parse::<f32>().ok())
                        .ok_or_else(|| ferr(line_no, "polygon: expected x,y"))?;
                    let y = s
                        .next()
                        .and_then(|t| t.parse::<f32>().ok())
                        .ok_or_else(|| ferr(line_no, "polygon: expected x,y"))?;
                    pts.push((x, y));
                }
                if pts.len() < 3 {
                    return Err(ferr(line_no, "polygon needs >= 3 points"));
                }
                cmds.push(PathCmd::Polygon(pts));
            }
            "close" => cmds.push(PathCmd::Close),
            other => return Err(ferr(line_no, &format!("unknown directive `{}`", other))),
        }
    }

    let name = name.ok_or_else(|| "missing `shape <name>` header".to_string())?;
    if !saw_end {
        return Err("missing `end` terminator".to_string());
    }
    if cmds.is_empty() {
        return Err(format!("shape `{}` has no geometry commands", name));
    }
    Ok(ShapeDef {
        name,
        aspect,
        label_cx,
        label_cy,
        text_area,
        stroke_mult,
        commands: cmds,
    })
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
}

fn num(line_no: usize, tok: Option<&str>) -> Result<f32, String> {
    let t = tok.ok_or_else(|| ferr(line_no, "expected number"))?;
    t.parse::<f32>()
        .map_err(|_| ferr(line_no, &format!("`{}` is not a number", t)))
}

fn ferr(line_no: usize, msg: &str) -> String {
    format!("line {}: {}", line_no, msg)
}
