//! docpane — the Direct2D markdown + Mermaid render core.
//!
//! Forked from DocCrate (the standalone `doc-crate.exe`), this crate is
//! the *shared* rendering core that will back two front-ends:
//!
//!   * the in-window **igui doc-pane type** (the manual, in the
//!     Factor4th MDI), and
//!   * the standalone **doc-crate.exe** doc-testing harness.
//!
//! ## Layering (the seam that made the fork clean)
//!
//! `layout` turns a parsed document into a `Vec<DrawCmd>` + hit regions
//! that is entirely Direct2D-free — colours are `u32`, text is
//! pre-encoded `Vec<u16>`, and text measurement is injected as a `fn`.
//! So `parser` / `layout` / `theme` are pure; only the `mermaid`
//! diagram renderer (and, once extracted, the markdown DrawCmd
//! interpreter) touch Direct2D.
//!
//! Status: the model pipeline (parse → layout → mermaid IR) and the
//! mermaid Direct2D renderer are in.  The markdown DrawCmd → Direct2D
//! interpreter is being lifted out of DocCrate's `render.rs` (its
//! window/sidebar/tab chrome stays behind — igui provides chrome) as
//! the next step.

pub mod parser;
pub mod layout;
pub mod theme;
pub mod mermaid;
pub mod render;

#[cfg(test)]
mod tests {
    // A stub measure (no DirectWrite) — the layout pipeline is pure, so
    // this exercises parse → layout without a window or render target.
    fn approx(text: &str, _font: &str, size: f32, _bold: bool, _italic: bool) -> f32 {
        text.chars().count() as f32 * size * 0.5
    }

    #[test]
    fn parse_then_layout_produces_commands() {
        let md = "# Title\n\nSome **bold** text and `code`.\n\n- one\n- two\n";
        let blocks = crate::parser::parse(md);
        assert!(!blocks.is_empty(), "parser produced blocks");
        let ly = crate::layout::layout(&blocks, 0.0, 600.0, 0.0, approx);
        assert!(!ly.cmds.is_empty(), "layout produced draw commands");
        assert!(ly.total_h > 0.0, "layout has positive height");
        // The heading text should surface as a recorded heading.
        assert!(ly.headings.iter().any(|(h, _)| h.contains("Title")),
            "heading recorded: {:?}", ly.headings);
    }
}
