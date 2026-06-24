# Changelog

## v0.3.0

ASCII text rendering for all diagram types, legible text color auto-selection, and rendering fixes across the board. This release adds a complete ASCII output mode — every diagram type can now be rendered as plain text for terminal display, accessibility, and embedding in contexts where SVG is unavailable.

### ASCII Rendering (New)

- **All diagram types**: ASCII renderers for flowcharts, sequence, class, ER, state, architecture, requirement, gantt, mindmap, pie, xychart, sankey, kanban, treemap, quadrant, radar, block, and C4 diagrams
- **Public API**: `render_ascii` exposed in the library API for programmatic use
- **Max width constraint**: configurable `max_width` parameter for ASCII output
- **Pie charts**: rendered as circular ASCII art
- **Mindmap**: visual ASCII shapes for nodes
- **Kanban**: columns rendered side-by-side
- **Sankey**: rewritten as flow diagram layout
- **Treemap**: nested rectangle rendering
- **Radar**: spider chart layout instead of bar chart
- **XYChart**: unified vertical grid for bars and lines
- **State diagrams**: fork/join bars and composite containers
- **Architecture**: dedicated layout for ASCII mode
- **Block diagrams**: proper nesting, shapes, and spacing
- **Class diagrams**: content-accurate layout to prevent box overlap
- **ER diagrams**: fixed label truncation and off-screen edge routing

### New Features

- **Text legibility**: auto-select legible text color (black/white) over custom node backgrounds based on luminance contrast
- **Edge label truncation detection**: eval system detects when edge labels are clipped

### Rendering Fixes

- **Layout**: improved BK position algorithm for narrower layouts
- **ER**: intersect-rect algorithm for edge routing instead of heuristic
- **Requirement**: declaration order and source pulling for better layout
- **Edge labels**: prevented truncation near occupied cells
- **Quadrant**: warning when `find_free_cell` exhausts search radius; points no longer overlap labels and axes
- **Coordinate handling**: fixed `saturating_sub` bug in `render_ascii_impl` and `render_er_ascii`
- **C4**: matched mermaid.js colors and added symbol definitions

### Eval Improvements

- **34 new channel diagrams** added to eval set
- **Architecture node bounds** detection in SVG structure parser
- **Subgraph boundary chars** included in `ascii_text_similarity` scoring

### Refactoring

- Renamed `tui` to `ascii` across the codebase
- Extracted shared architecture edge routing into architecture module

## v0.2.0

First point release of selkie-rs. Focused on rendering fidelity across all diagram types — closing the gap between selkie's SVG output and the mermaid.js reference.

See [v0.2.0 release notes](https://github.com/btucker/selkie/releases/tag/v0.2.0) for details.

## v0.1.0

Initial release with parser and SVG renderer for all mermaid diagram types.
