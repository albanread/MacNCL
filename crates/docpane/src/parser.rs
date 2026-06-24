use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::mermaid;

#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    BoldItalic(String),
    Code(String),
    Link { text: String, href: String },
    Image { alt: String, src: String },
    SoftBreak,
    HardBreak,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColAlign {
    None,
    Left,
    Center,
    Right,
}

/// A single item in a toolbar: an icon image + a navigation link.
#[derive(Debug, Clone)]
pub struct ToolbarItem {
    pub image_path: String,
    pub image_alt: String,
    pub label: String,
    pub href: String,
}

#[derive(Debug, Clone)]
pub enum Block {
    Located {
        line: usize,
        block: Box<Block>,
    },
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    CodeBlock {
        lang: String,
        code: String,
    },
    Blockquote(Vec<Block>),
    BulletList(Vec<Vec<Inline>>),
    OrderedList {
        start: u64,
        items: Vec<Vec<Inline>>,
    },
    ThematicBreak,
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        alignments: Vec<ColAlign>,
    },
    /// A toolbar row: detected from a 2-row table where all header cells are
    /// images and the single body row contains all links.
    Toolbar(Vec<ToolbarItem>),
    /// A ```mermaid fenced code block. Pre-parsed and laid out at markdown
    /// parse time, so re-layout on resize is just a transform on the cached
    /// `Arc<Graph>`. `error` is set (and `graph` is `None`) when selkie fails
    /// to parse the source — the renderer falls back to showing the original
    /// text as a code block in that case.
    Mermaid {
        source: String,
        graph: Option<Arc<mermaid::Graph>>,
        error: Option<String>,
    },
}

pub fn parse(md: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let line_starts = line_starts(md);
    let events: Vec<LocatedEvent> = Parser::new_ext(md, opts)
        .into_offset_iter()
        .map(|(event, range)| LocatedEvent {
            line: line_for_offset(&line_starts, range.start),
            event,
        })
        .collect();
    let mut pos = 0;
    parse_blocks(&events, &mut pos, None)
}

// `parse_loaded(docs::LoadedDoc)` lived here in DocCrate, bridging the
// file-loader (and its large-doc rope buffer) to `parse`.  In the
// render-core fork that's the wrong layer: docpane is rope-agnostic and
// renders from text, while file-loading + large-doc rope streaming stay
// at the igui layer, which already owns the shared rope buffer.  Call
// `parse(&str)` directly.

struct LocatedEvent<'a> {
    event: Event<'a>,
    line: usize,
}

fn located(line: usize, block: Block) -> Block {
    Block::Located {
        line,
        block: Box::new(block),
    }
}

fn line_starts(md: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, b) in md.bytes().enumerate() {
        if b == b'\n' {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_for_offset(starts: &[usize], offset: usize) -> usize {
    match starts.binary_search(&offset) {
        Ok(idx) => idx + 1,
        Err(idx) => idx.max(1),
    }
}

fn parse_blocks(events: &[LocatedEvent], pos: &mut usize, end_tag: Option<TagEnd>) -> Vec<Block> {
    let mut blocks = Vec::new();
    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(t) if Some(t.clone()) == end_tag => {
                *pos += 1;
                return blocks;
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let line = events[*pos].line;
                *pos += 1;
                let level = hl(*level);
                let inlines = parse_inlines(events, pos, TagEnd::Heading(heading_level(level)));
                blocks.push(located(line, Block::Heading { level, inlines }));
            }
            Event::Start(Tag::Paragraph) => {
                let line = events[*pos].line;
                *pos += 1;
                let inlines = parse_inlines(events, pos, TagEnd::Paragraph);
                if !inlines.is_empty() {
                    blocks.push(located(line, Block::Paragraph(inlines)));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let line = events[*pos].line;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                *pos += 1;
                let mut code = String::new();
                while *pos < events.len() {
                    match &events[*pos].event {
                        Event::Text(t) => {
                            code.push_str(t);
                            *pos += 1;
                        }
                        Event::End(TagEnd::CodeBlock) => {
                            *pos += 1;
                            break;
                        }
                        _ => {
                            *pos += 1;
                        }
                    }
                }
                if code.ends_with('\n') {
                    code.pop();
                }
                if lang.eq_ignore_ascii_case("mermaid") {
                    match mermaid::build(&code) {
                        Ok(g) => blocks.push(located(
                            line,
                            Block::Mermaid {
                                source: code,
                                graph: Some(Arc::new(g)),
                                error: None,
                            },
                        )),
                        Err(e) => blocks.push(located(
                            line,
                            Block::Mermaid {
                                source: code,
                                graph: None,
                                error: Some(e),
                            },
                        )),
                    }
                } else {
                    blocks.push(located(line, Block::CodeBlock { lang, code }));
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                let line = events[*pos].line;
                *pos += 1;
                let inner = parse_blocks(events, pos, Some(TagEnd::BlockQuote(None)));
                blocks.push(located(line, Block::Blockquote(inner)));
            }
            Event::Start(Tag::List(start_num)) => {
                let line = events[*pos].line;
                let ordered = start_num.is_some();
                let start = start_num.unwrap_or(1);
                *pos += 1;
                let items = parse_list_items(events, pos);
                if ordered {
                    blocks.push(located(line, Block::OrderedList { start, items }));
                } else {
                    blocks.push(located(line, Block::BulletList(items)));
                }
            }
            Event::Start(Tag::Table(aligns)) => {
                let line = events[*pos].line;
                let alignments = aligns
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::Left => ColAlign::Left,
                        pulldown_cmark::Alignment::Center => ColAlign::Center,
                        pulldown_cmark::Alignment::Right => ColAlign::Right,
                        pulldown_cmark::Alignment::None => ColAlign::None,
                    })
                    .collect();
                *pos += 1;
                blocks.push(located(line, parse_table(events, pos, alignments)));
            }
            Event::Rule => {
                let line = events[*pos].line;
                blocks.push(located(line, Block::ThematicBreak));
                *pos += 1;
            }
            Event::End(_) => {
                *pos += 1;
            }
            _ => {
                *pos += 1;
            }
        }
    }
    blocks
}

// ── Table / toolbar parsing ────────────────────────────────────────────────

fn parse_table(events: &[LocatedEvent], pos: &mut usize, alignments: Vec<ColAlign>) -> Block {
    let mut header_cells: Vec<Vec<Inline>> = Vec::new();
    let mut row_cells: Vec<Vec<Vec<Inline>>> = Vec::new();

    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(TagEnd::Table) => {
                *pos += 1;
                break;
            }
            Event::Start(Tag::TableHead) => {
                *pos += 1;
                header_cells = parse_table_row_inlines(events, pos, TagEnd::TableHead);
            }
            Event::Start(Tag::TableRow) => {
                *pos += 1;
                row_cells.push(parse_table_row_inlines(events, pos, TagEnd::TableRow));
            }
            _ => {
                *pos += 1;
            }
        }
    }

    // Toolbar detection: exactly 1 body row, all headers = lone image,
    // all body cells = lone link.
    if !header_cells.is_empty() && row_cells.len() == 1 {
        let all_img = header_cells
            .iter()
            .all(|c| matches!(c.as_slice(), [Inline::Image { .. }]));
        let all_link = row_cells[0]
            .iter()
            .all(|c| matches!(c.as_slice(), [Inline::Link { .. }]));
        if all_img && all_link {
            let items = header_cells
                .iter()
                .zip(row_cells[0].iter())
                .map(|(hc, rc)| {
                    let (image_path, image_alt) = match &hc[0] {
                        Inline::Image { src, alt } => (src.clone(), alt.clone()),
                        _ => unreachable!(),
                    };
                    let (label, href) = match &rc[0] {
                        Inline::Link { text, href } => (text.clone(), href.clone()),
                        _ => unreachable!(),
                    };
                    ToolbarItem {
                        image_path,
                        image_alt,
                        label,
                        href,
                    }
                })
                .collect();
            return Block::Toolbar(items);
        }
    }

    // Regular table — convert inlines to plain text per cell.
    let headers = header_cells
        .iter()
        .map(|c| collect_inline_text(c))
        .collect();
    let rows = row_cells
        .iter()
        .map(|row| row.iter().map(|c| collect_inline_text(c)).collect())
        .collect();
    Block::Table {
        headers,
        rows,
        alignments,
    }
}

/// Parse a table row, returning one `Vec<Inline>` per cell.
fn parse_table_row_inlines(
    events: &[LocatedEvent],
    pos: &mut usize,
    end: TagEnd,
) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();
    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(t) if *t == end => {
                *pos += 1;
                break;
            }
            Event::Start(Tag::TableCell) => {
                *pos += 1;
                cells.push(parse_inlines(events, pos, TagEnd::TableCell));
            }
            _ => {
                *pos += 1;
            }
        }
    }
    cells
}

// ── List parsing ───────────────────────────────────────────────────────────

fn parse_list_items(events: &[LocatedEvent], pos: &mut usize) -> Vec<Vec<Inline>> {
    let mut items = Vec::new();
    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(TagEnd::List(_)) => {
                *pos += 1;
                break;
            }
            Event::Start(Tag::Item) => {
                *pos += 1;
                items.push(collect_item_inlines(events, pos));
            }
            _ => {
                *pos += 1;
            }
        }
    }
    items
}

fn collect_item_inlines(events: &[LocatedEvent], pos: &mut usize) -> Vec<Inline> {
    let mut result = Vec::new();
    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(TagEnd::Item) => {
                *pos += 1;
                break;
            }
            Event::Start(Tag::Paragraph) => {
                *pos += 1;
                let mut inner = parse_inlines(events, pos, TagEnd::Paragraph);
                result.append(&mut inner);
            }
            _ => {
                if let Some(i) = parse_one_inline(events, pos) {
                    result.push(i);
                }
            }
        }
    }
    result
}

// ── Inline parsing ─────────────────────────────────────────────────────────

fn parse_inlines(events: &[LocatedEvent], pos: &mut usize, end: TagEnd) -> Vec<Inline> {
    let mut inlines = Vec::new();
    while *pos < events.len() {
        match &events[*pos].event {
            Event::End(t) if *t == end => {
                *pos += 1;
                break;
            }
            _ => {
                if let Some(i) = parse_one_inline(events, pos) {
                    inlines.push(i);
                }
            }
        }
    }
    inlines
}

fn parse_one_inline(events: &[LocatedEvent], pos: &mut usize) -> Option<Inline> {
    match &events[*pos].event {
        Event::Text(t) => {
            let s = t.to_string();
            *pos += 1;
            Some(Inline::Text(s))
        }
        Event::Code(t) => {
            let s = t.to_string();
            *pos += 1;
            Some(Inline::Code(s))
        }
        Event::SoftBreak => {
            *pos += 1;
            Some(Inline::SoftBreak)
        }
        Event::HardBreak => {
            *pos += 1;
            Some(Inline::HardBreak)
        }
        Event::Start(Tag::Strong) => {
            *pos += 1;
            let inlines = parse_inlines(events, pos, TagEnd::Strong);
            if inlines.len() == 1 {
                if let Inline::Italic(t) = &inlines[0] {
                    return Some(Inline::BoldItalic(t.clone()));
                }
            }
            Some(Inline::Bold(collect_inline_text(&inlines)))
        }
        Event::Start(Tag::Emphasis) => {
            *pos += 1;
            let inlines = parse_inlines(events, pos, TagEnd::Emphasis);
            if inlines.len() == 1 {
                if let Inline::Bold(t) = &inlines[0] {
                    return Some(Inline::BoldItalic(t.clone()));
                }
            }
            Some(Inline::Italic(collect_inline_text(&inlines)))
        }
        Event::Start(Tag::Link { dest_url, .. }) => {
            let href = dest_url.to_string();
            *pos += 1;
            let inlines = parse_inlines(events, pos, TagEnd::Link);
            let text = collect_inline_text(&inlines);
            Some(Inline::Link { text, href })
        }
        Event::Start(Tag::Image { dest_url, .. }) => {
            let src = dest_url.to_string();
            *pos += 1;
            let inlines = parse_inlines(events, pos, TagEnd::Image);
            let alt = collect_inline_text(&inlines);
            Some(Inline::Image { alt, src })
        }
        _ => {
            *pos += 1;
            None
        }
    }
}

pub fn collect_inline_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        match i {
            Inline::Text(t)
            | Inline::Bold(t)
            | Inline::Italic(t)
            | Inline::BoldItalic(t)
            | Inline::Code(t) => s.push_str(t),
            Inline::Link { text, .. } => s.push_str(text),
            Inline::Image { alt, .. } => s.push_str(alt),
            Inline::SoftBreak | Inline::HardBreak => s.push(' '),
        }
    }
    s
}

fn hl(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn heading_level(n: u8) -> HeadingLevel {
    match n {
        1 => HeadingLevel::H1,
        2 => HeadingLevel::H2,
        3 => HeadingLevel::H3,
        4 => HeadingLevel::H4,
        5 => HeadingLevel::H5,
        _ => HeadingLevel::H6,
    }
}
