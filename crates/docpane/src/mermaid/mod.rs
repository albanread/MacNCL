//! Mermaid diagram rendering.
//!
//! Pipeline:
//!
//! 1. `build::build(source)` runs selkie. For a flowchart it does parse +
//!    layout + annotation overrides and converts the result into
//!    [`ir::FlowchartGraph`]. For a sequence diagram, we compute layout
//!    ourselves from `SequenceDb` (selkie has no `LayoutGraph` adapter for
//!    sequences). Either way the output is a top-level [`ir::Graph`] enum.
//!
//! 2. `parser::Block::Mermaid` caches an `Arc<ir::Graph>` per fenced
//!    ```mermaid block. Re-parse only happens on doc navigation.
//!
//! 3. `layout::DrawCmd::Mermaid` carries `(x, y, scale, Arc<ir::Graph>)`.
//!    `scale = (content_width / graph.width()).min(1.0)`.
//!
//! 4. `render::draw_graph(target, ...)` dispatches on the [`ir::Graph`]
//!    variant.

pub mod architecture;
pub mod build;
pub mod c4;
pub mod class;
pub mod er;
pub mod gantt;
pub mod git;
pub mod ir;
pub mod journey;
pub mod manual_layout;
pub mod render;
pub mod sequence;
pub mod shape_def;
pub mod timeline;

pub use build::build;
pub use ir::Graph;
