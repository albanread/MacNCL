//! Doccrate-owned intermediate representation for laid-out mermaid diagrams.
//!
//! Currently supports two diagram families. They share the [`Graph`] enum but
//! have completely different internal layouts:
//!
//! * [`FlowchartGraph`] — nodes + edges + subgraph groups, positioned by selkie's
//!   dagre-style layout engine, then refined by `@annotation` overrides.
//! * [`SequenceGraph`] — actors + lifelines + time-ordered messages, with
//!   layout computed by us (selkie has no LayoutGraph adapter for sequences).
//!
//! Everything in both subtrees is fully resolved: colours are `u32` (RGB in
//! low 24 bits), positions are `f32` DIPs in the graph's natural coordinate
//! space starting at `(0, 0)` and bounded by `width × height`.

// ===========================================================================
// Top-level enum
// ===========================================================================

#[derive(Debug, Clone)]
pub enum Graph {
    Architecture(ArchitectureGraph),
    Flowchart(FlowchartGraph),
    C4(C4Graph),
    Class(ClassGraph),
    Er(ErGraph),
    Gantt(GanttGraph),
    Git(GitGraph),
    Journey(JourneyGraph),
    Sequence(SequenceGraph),
    Timeline(TimelineGraph),
}

impl Graph {
    pub fn width(&self) -> f32 {
        match self {
            Graph::Architecture(g) => g.width,
            Graph::Flowchart(g) => g.width,
            Graph::C4(g) => g.width,
            Graph::Class(g) => g.width,
            Graph::Er(g) => g.width,
            Graph::Gantt(g) => g.width,
            Graph::Git(g) => g.width,
            Graph::Journey(g) => g.width,
            Graph::Sequence(g) => g.width,
            Graph::Timeline(g) => g.width,
        }
    }
    pub fn height(&self) -> f32 {
        match self {
            Graph::Architecture(g) => g.height,
            Graph::Flowchart(g) => g.height,
            Graph::C4(g) => g.height,
            Graph::Class(g) => g.height,
            Graph::Er(g) => g.height,
            Graph::Gantt(g) => g.height,
            Graph::Git(g) => g.height,
            Graph::Journey(g) => g.height,
            Graph::Sequence(g) => g.height,
            Graph::Timeline(g) => g.height,
        }
    }
}

// ===========================================================================
// Architecture diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct ArchitectureGraph {
    pub width: f32,
    pub height: f32,
    pub title: String,
    pub groups: Vec<ArchitectureGroupBox>,
    pub edges: Vec<ArchitectureEdgeLine>,
    pub services: Vec<ArchitectureServiceBox>,
}

#[derive(Debug, Clone)]
pub struct ArchitectureGroupBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct ArchitectureServiceBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: String,
    pub icon: String,
    pub icon_text: String,
    pub junction: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureDirection {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
pub struct ArchitectureEdgeLine {
    pub points: Vec<(f32, f32)>,
    pub label: String,
    pub label_pos: Option<(f32, f32)>,
    pub label_offset: Option<(f32, f32)>,
    pub start_arrow: bool,
    pub end_arrow: bool,
}

// ===========================================================================
// Flowchart
// ===========================================================================

#[derive(Debug, Clone)]
pub struct FlowchartGraph {
    pub width: f32,
    pub height: f32,
    /// `@graph canvas_fill` if set; otherwise `None` (transparent).
    pub background: Option<u32>,
    pub groups: Vec<Group>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Node shape. Anything mermaid supports that we don't yet handle natively
/// falls back to [`Shape::Rect`] at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Rect,
    RoundedRect,
    Stadium,
    Circle,
    /// `(((text)))` — concentric circles
    DoubleCircle,
    Ellipse,
    Diamond,
    Hexagon,
    /// `[(text)]` — classic database / data-store cylinder
    Cylinder,
    /// `[[text]]` — rectangle with inner vertical bars
    Subroutine,
    /// `[/text\]` — wider at the bottom
    Trapezoid,
    /// `[\text/]` — wider at the top
    InvTrapezoid,
    /// `[/text/]` — parallelogram leaning right
    LeanRight,
    /// `[\text\]` — parallelogram leaning left
    LeanLeft,
    /// `>text]` — asymmetric pentagonal (flag) shape
    Odd,
    /// Used for fork/join bars in state diagrams. Thin filled bar, no label.
    HorizontalBar,
    /// Renderer-defined extension shape — index into the App-wide shape
    /// registry (built-ins + `docs/.shapes/*.shape`). Kept as a `u32` so
    /// `Shape` stays `Copy` and `DrawCmd::Mermaid` doesn't have to clone
    /// strings each frame.
    Custom(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub shape: Shape,
    pub label: String,
    pub label_align: Align,
    pub fill: u32,
    pub stroke: u32,
    pub stroke_w: f32,
    pub text_color: u32,
    pub font_size: f32,
    pub bold: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Solid,
    Dash,
    Dot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrow {
    None,
    Triangle,
    Circle,
    Cross,
}

#[derive(Debug, Clone)]
pub struct EdgeLabel {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    pub text_color: u32,
    pub font_size: f32,
}

#[derive(Debug, Clone)]
pub struct Edge {
    /// Polyline path. Always ≥ 2 points (start, end).
    pub points: Vec<(f32, f32)>,
    pub line_color: u32,
    pub line_w: f32,
    pub line_style: LineStyle,
    pub start_arrow: Arrow,
    pub end_arrow: Arrow,
    pub label: Option<EdgeLabel>,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: Option<String>,
    pub fill: u32,
    pub stroke: u32,
    pub stroke_w: f32,
    pub title_font_size: f32,
    pub title_color: u32,
}

// ===========================================================================
// C4 diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct C4Graph {
    pub width: f32,
    pub height: f32,
    pub title: Option<String>,
    pub boundaries: Vec<C4BoundaryBox>,
    pub relationships: Vec<C4Edge>,
    pub elements: Vec<C4ElementBox>,
}

#[derive(Debug, Clone)]
pub struct C4BoundaryBox {
    pub alias: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub kind: String,
    pub solid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum C4Shape {
    Person,
    Rect,
    Database,
    Queue,
}

#[derive(Debug, Clone)]
pub struct C4ElementBox {
    pub alias: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub kind_label: String,
    pub technology: String,
    pub description: String,
    pub shape: C4Shape,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct C4Edge {
    pub points: Vec<(f32, f32)>,
    pub label: String,
    pub technology: String,
    pub label_pos: Option<(f32, f32)>,
    pub label_offset: Option<(f32, f32)>,
    pub bidirectional: bool,
    pub color: u32,
}

// ===========================================================================
// Class diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct ClassGraph {
    pub width: f32,
    pub height: f32,
    pub groups: Vec<ClassGroup>,
    pub nodes: Vec<ClassBox>,
    pub edges: Vec<ClassEdge>,
    pub notes: Vec<ClassNoteBox>,
}

#[derive(Debug, Clone)]
pub struct ClassGroup {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct ClassBox {
    pub id: String,
    pub namespace: Option<String>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: String,
    pub annotations: Vec<String>,
    pub members: Vec<ClassMemberLine>,
    pub methods: Vec<ClassMemberLine>,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct ClassMemberLine {
    pub text: String,
    pub italic: bool,
    pub underline: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassMarker {
    None,
    Aggregation,
    Extension,
    Composition,
    Dependency,
    Lollipop,
}

#[derive(Debug, Clone)]
pub struct ClassEdge {
    pub points: Vec<(f32, f32)>,
    pub line_style: LineStyle,
    pub start_marker: ClassMarker,
    pub end_marker: ClassMarker,
    pub label: String,
    pub label_pos: Option<(f32, f32)>,
    pub label_offset: Option<(f32, f32)>,
    pub card_start: String,
    pub card_end: String,
    pub color: u32,
}

#[derive(Debug, Clone)]
pub struct ClassNoteBox {
    pub id: String,
    pub class: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
}

// ===========================================================================
// ER diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct ErGraph {
    pub width: f32,
    pub height: f32,
    pub entities: Vec<ErEntityBox>,
    pub edges: Vec<ErEdge>,
}

#[derive(Debug, Clone)]
pub struct ErEntityBox {
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub title: String,
    pub attrs: Vec<ErAttribute>,
    pub col_widths: [f32; 3],
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct ErAttribute {
    pub attr_type: String,
    pub name: String,
    pub keys: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErCardinality {
    ZeroOrOne,
    ZeroOrMore,
    OneOrMore,
    OnlyOne,
}

#[derive(Debug, Clone)]
pub struct ErEdge {
    pub points: Vec<(f32, f32)>,
    pub label: String,
    pub label_pos: Option<(f32, f32)>,
    pub label_offset: Option<(f32, f32)>,
    pub line_style: LineStyle,
    pub start_card: ErCardinality,
    pub end_card: ErCardinality,
    pub color: u32,
}

// ===========================================================================
// Gantt diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct GanttGraph {
    pub width: f32,
    pub height: f32,
    pub title: String,
    pub chart_x: f32,
    pub chart_y: f32,
    pub chart_w: f32,
    pub chart_h: f32,
    pub ticks: Vec<GanttTick>,
    pub sections: Vec<GanttSection>,
    pub tasks: Vec<GanttTask>,
}

#[derive(Debug, Clone)]
pub struct GanttTick {
    pub x: f32,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct GanttSection {
    pub y: f32,
    pub h: f32,
    pub label: String,
    pub fill: u32,
}

#[derive(Debug, Clone)]
pub struct GanttTask {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub start_label: String,
    pub milestone: bool,
    pub vertical: bool,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

// ===========================================================================
// Timeline diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct TimelineGraph {
    pub width: f32,
    pub height: f32,
    pub title: String,
    pub line_y: f32,
    pub sections: Vec<TimelineSectionBox>,
    pub items: Vec<TimelineItemBox>,
}

#[derive(Debug, Clone)]
pub struct TimelineSectionBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct TimelineItemBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub cx: f32,
    pub label: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
    pub events: Vec<TimelineEventBox>,
}

#[derive(Debug, Clone)]
pub struct TimelineEventBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

// ===========================================================================
// Journey diagrams
// ===========================================================================

#[derive(Debug, Clone)]
pub struct JourneyGraph {
    pub width: f32,
    pub height: f32,
    pub title: String,
    pub lanes: Vec<JourneyLane>,
    pub sections: Vec<JourneySectionBox>,
    pub tasks: Vec<JourneyTaskBox>,
}

#[derive(Debug, Clone)]
pub struct JourneyLane {
    pub chart_x: f32,
    pub chart_y: f32,
    pub chart_w: f32,
    pub chart_h: f32,
    pub task_start: usize,
    pub task_count: usize,
}

#[derive(Debug, Clone)]
pub struct JourneySectionBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub label: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

#[derive(Debug, Clone)]
pub struct JourneyTaskBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub point_x: f32,
    pub point_y: f32,
    pub score: i32,
    pub label: String,
    pub actors: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
}

// ===========================================================================
// Git graphs
// ===========================================================================

#[derive(Debug, Clone)]
pub struct GitGraph {
    pub width: f32,
    pub height: f32,
    pub branches: Vec<GitBranch>,
    pub edges: Vec<GitEdge>,
    pub commits: Vec<GitCommit>,
}

#[derive(Debug, Clone)]
pub struct GitBranch {
    pub name: String,
    pub color: u32,
    pub line: Vec<(f32, f32)>,
    pub label_x: f32,
    pub label_y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCommitKind {
    Normal,
    Reverse,
    Highlight,
    Merge,
    CherryPick,
}

#[derive(Debug, Clone)]
pub struct GitCommit {
    pub x: f32,
    pub y: f32,
    pub label: String,
    pub tags: Vec<String>,
    pub kind: GitCommitKind,
    pub color: u32,
}

#[derive(Debug, Clone)]
pub struct GitEdge {
    pub points: Vec<(f32, f32)>,
    pub color: u32,
    pub line_style: LineStyle,
}

// ===========================================================================
// Sequence
// ===========================================================================

#[derive(Debug, Clone)]
pub struct SequenceGraph {
    pub width: f32,
    pub height: f32,
    pub actors: Vec<SeqActor>,
    pub messages: Vec<SeqMessage>,
    pub notes: Vec<SeqNote>,
}

#[derive(Debug, Clone)]
pub struct SeqActor {
    /// Top participant box.
    pub box_x: f32,
    pub box_y: f32,
    pub box_w: f32,
    pub box_h: f32,
    /// Vertical lifeline beneath the box.
    pub lifeline_x: f32,
    pub lifeline_y0: f32,
    pub lifeline_y1: f32,
    pub label: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
    pub font_size: f32,
    /// Optional renderer-defined shape for the participant box.
    /// `None` → the default rounded rectangle.
    pub shape: Option<Shape>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageStyle {
    Solid,
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageArrow {
    /// Filled triangle — sync request
    Filled,
    /// Open (stick) arrow — async / return
    Open,
    /// "X" mark — destroy / lost
    Cross,
    None,
}

#[derive(Debug, Clone)]
pub struct SeqMessage {
    pub from_x: f32,
    pub to_x: f32,
    pub y: f32,
    pub label: String,
    pub style: MessageStyle,
    pub start_arrow: MessageArrow,
    pub end_arrow: MessageArrow,
    /// `true` when the sender and receiver are the same actor; rendered as a
    /// short rectangular loop on the right side of the lifeline.
    pub self_loop: bool,
    pub color: u32,
    pub label_color: u32,
    pub font_size: f32,
}

#[derive(Debug, Clone)]
pub struct SeqNote {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub text: String,
    pub fill: u32,
    pub stroke: u32,
    pub text_color: u32,
    pub font_size: f32,
}
