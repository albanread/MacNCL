//! Sequence-diagram pipeline.
//!
//! Selkie parses sequence diagrams into [`SequenceDb`] but, unlike flowcharts,
//! it has no `LayoutGraph` adapter — sequence layout is positional/incremental
//! rather than graph-routed. This module does the layout pass ourselves and
//! the Direct2D draw pass.
//!
//! Phase 1 scope:
//! * actors (boxes at top, lifelines beneath)
//! * messages: solid / dotted, with filled / open / cross arrowheads
//! * self-messages (rectangular loop on the right of the lifeline)
//! * notes: left/right/over participant spans
//!
//! Out of scope (follow-ups): activation boxes, fragments
//! (loop / alt / opt / par / critical / break), participant kinds (actor stick
//! figure, database, queue, etc.), autonumber.

use windows::{
    core::*,
    Win32::Graphics::Direct2D::{Common::*, *},
    Win32::Graphics::DirectWrite::*,
};
use windows_numerics::Vector2;

use selkie::diagrams::sequence::{LineType, Placement, SequenceDb};

use crate::mermaid::ir::*;
use crate::mermaid::manual_layout::{BoxOverride, ManualLayoutOverrides};
use crate::theme;

// ---------------------------------------------------------------------------
// Layout constants (DIPs, natural scale)
// ---------------------------------------------------------------------------

const TOP_MARGIN: f32 = 16.0;
const SIDE_MARGIN: f32 = 12.0;
const BOTTOM_MARGIN: f32 = 16.0;

const ACTOR_BOX_H: f32 = 36.0;
const ACTOR_BOX_MIN_W: f32 = 80.0;
const ACTOR_BOX_H_PAD: f32 = 16.0;
const ACTOR_H_GAP: f32 = 60.0; // gap between adjacent actor-box edges

const FIRST_MSG_OFFSET: f32 = 28.0;
const MSG_V_GAP: f32 = 36.0;
const SELF_LOOP_V: f32 = 30.0; // extra vertical space for a self-message
const SELF_LOOP_W: f32 = 50.0; // horizontal extent of the self-loop rectangle
const NOTE_W: f32 = 170.0;
const NOTE_MIN_H: f32 = 42.0;
const NOTE_GAP: f32 = 14.0;

const LABEL_FONT_W_RATIO: f32 = 0.55; // rough glyph-width fraction of font size

// ---------------------------------------------------------------------------
// Layout (build IR from SequenceDb)
// ---------------------------------------------------------------------------

pub fn build_with_overrides(db: &SequenceDb, overrides: &ManualLayoutOverrides) -> SequenceGraph {
    let reg = crate::mermaid::shape_def::registry();
    let mut actors_in: Vec<ActorIn> = db
        .get_actors_in_order()
        .into_iter()
        .map(|a| {
            // Optional custom shape declared via `participant Foo@{ shape: name }`.
            // Selkie's sequence parser stashes the metadata k/v pairs in
            // `actor.properties`.
            let shape = a
                .properties
                .get("shape")
                .and_then(|name| reg.lookup(name))
                .map(crate::mermaid::ir::Shape::Custom);
            let label_override = a.properties.get("label").cloned();
            ActorIn {
                name: a.name.clone(),
                label: label_override.unwrap_or_else(|| {
                    if a.description.is_empty() {
                        a.name.clone()
                    } else {
                        a.description.clone()
                    }
                }),
                box_x: 0.0,
                box_y: TOP_MARGIN,
                box_w: 0.0,
                box_h: ACTOR_BOX_H,
                shape,
            }
        })
        .collect();

    // No actors → bail with a tiny placeholder graph so the layout pipeline
    // doesn't divide by zero.
    if actors_in.is_empty() {
        return SequenceGraph {
            width: 100.0,
            height: 40.0,
            actors: Vec::new(),
            messages: Vec::new(),
            notes: Vec::new(),
        };
    }

    // Actor box widths (measured from label length; full DirectWrite metrics
    // happen at render time but this approximation is close enough for layout).
    let font_size = theme::MERMAID_NODE_FONT_SIZE;
    for a in &mut actors_in {
        let text_w = a.label.chars().count() as f32 * font_size * LABEL_FONT_W_RATIO;
        a.box_w = (text_w + ACTOR_BOX_H_PAD * 2.0).max(ACTOR_BOX_MIN_W);
        // Hard aspect-ratio enforcement for custom-shape actors: widen the
        // box to match the shape's declared ratio (ACTOR_BOX_H is fixed).
        if let Some(crate::mermaid::ir::Shape::Custom(idx)) = a.shape {
            if let Some(def) = reg.get(idx) {
                if let Some(aspect) = def.aspect {
                    a.box_w = a.box_w.max(ACTOR_BOX_H * aspect);
                }
            }
        }
    }

    // X positions: left-edge of each actor box.
    let mut x_cursor = SIDE_MARGIN;
    for a in &mut actors_in {
        a.box_x = x_cursor;
        x_cursor = a.box_x + a.box_w + ACTOR_H_GAP;
    }
    apply_actor_overrides(&mut actors_in, overrides);

    // Y positions: actor boxes at TOP_MARGIN, lifelines start beneath.
    let lifeline_y0 = actors_in
        .iter()
        .map(|actor| actor.box_y + actor.box_h)
        .fold(TOP_MARGIN + ACTOR_BOX_H, f32::max);
    let mut msg_y_cursor = lifeline_y0 + FIRST_MSG_OFFSET;

    // Walk messages and notes in their original event order.
    let mut events = Vec::new();
    events.extend(
        db.get_messages()
            .iter()
            .map(|message| (message.order, SequenceEvent::Message(message))),
    );
    events.extend(
        db.get_notes()
            .iter()
            .enumerate()
            .map(|(idx, note)| (note.order, SequenceEvent::Note(idx, note))),
    );
    events.sort_by_key(|(order, _)| *order);

    let mut messages_out: Vec<SeqMessage> = Vec::new();
    let mut notes_out: Vec<SeqNote> = Vec::new();
    for (_, event) in events {
        match event {
            SequenceEvent::Message(m) => {
                if !is_drawable_line(m.message_type) {
                    continue;
                }
                let from_idx = m
                    .from
                    .as_ref()
                    .and_then(|n| actors_in.iter().position(|a| &a.name == n));
                let to_idx =
                    m.to.as_ref()
                        .and_then(|n| actors_in.iter().position(|a| &a.name == n));
                let (Some(fi), Some(ti)) = (from_idx, to_idx) else {
                    continue;
                };

                let from_x = actors_in[fi].center_x();
                let to_x = actors_in[ti].center_x();
                let self_loop = fi == ti;

                let (style, start_arrow, end_arrow) = line_kind_to_style_arrows(m.message_type);
                messages_out.push(SeqMessage {
                    from_x,
                    to_x,
                    y: msg_y_cursor,
                    label: m.message.clone(),
                    style,
                    start_arrow,
                    end_arrow,
                    self_loop,
                    color: theme::MERMAID_EDGE,
                    label_color: theme::MERMAID_EDGE_LABEL,
                    font_size: theme::MERMAID_EDGE_FONT_SIZE,
                });
                msg_y_cursor += if self_loop {
                    MSG_V_GAP + SELF_LOOP_V
                } else {
                    MSG_V_GAP
                };
            }
            SequenceEvent::Note(idx, note) => {
                if let Some(note_out) = layout_note(idx, note, &actors_in, msg_y_cursor, overrides)
                {
                    msg_y_cursor = msg_y_cursor.max(note_out.y + note_out.h + NOTE_GAP);
                    notes_out.push(note_out);
                }
            }
        }
    }

    let note_bottom = notes_out
        .iter()
        .map(|note| note.y + note.h)
        .fold(0.0, f32::max);
    let lifeline_y1 = msg_y_cursor
        .max(note_bottom + NOTE_GAP)
        .max(lifeline_y0 + 20.0);
    let mut total_w = actors_in
        .iter()
        .map(|a| a.box_x + a.box_w)
        .fold(0.0, f32::max)
        + SIDE_MARGIN;
    let mut total_h = lifeline_y1 + BOTTOM_MARGIN;
    for note in &notes_out {
        total_w = total_w.max(note.x + note.w + SIDE_MARGIN);
        total_h = total_h.max(note.y + note.h + BOTTOM_MARGIN);
    }
    if let Some(w) = overrides.graph.w {
        total_w = w;
    }
    if let Some(h) = overrides.graph.h {
        total_h = h;
    }

    let actors_out: Vec<SeqActor> = actors_in
        .into_iter()
        .map(|a| {
            let lifeline_x = a.center_x();
            SeqActor {
                box_x: a.box_x,
                box_y: a.box_y,
                box_w: a.box_w,
                box_h: a.box_h,
                lifeline_x,
                lifeline_y0: a.box_y + a.box_h,
                lifeline_y1,
                shape: a.shape,
                label: a.label,
                fill: theme::MERMAID_NODE_FILL,
                stroke: theme::MERMAID_NODE_STROKE,
                text_color: theme::MERMAID_NODE_TEXT,
                font_size,
            }
        })
        .collect();

    SequenceGraph {
        width: total_w.max(1.0),
        height: total_h.max(1.0),
        actors: actors_out,
        messages: messages_out,
        notes: notes_out,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct ActorIn {
    name: String,
    label: String,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
    /// Resolved custom shape, if the participant carried
    /// `@{ shape: name }` metadata that matched a registry entry.
    shape: Option<crate::mermaid::ir::Shape>,
}
impl ActorIn {
    fn new_unset(_n: usize) -> Self {
        Self {
            name: String::new(),
            label: String::new(),
            box_x: 0.0,
            box_y: TOP_MARGIN,
            box_w: 0.0,
            box_h: ACTOR_BOX_H,
            shape: None,
        }
    }
    fn center_x(&self) -> f32 {
        self.box_x + self.box_w / 2.0
    }
}

enum SequenceEvent<'a> {
    Message(&'a selkie::diagrams::sequence::Message),
    Note(usize, &'a selkie::diagrams::sequence::Note),
}

fn apply_actor_overrides(actors: &mut [ActorIn], overrides: &ManualLayoutOverrides) {
    for actor in actors {
        if let Some(ov) = overrides.object(&actor.name) {
            apply_box_override(
                &mut actor.box_x,
                &mut actor.box_y,
                &mut actor.box_w,
                &mut actor.box_h,
                ov,
            );
        }
    }
}

fn layout_note(
    idx: usize,
    note: &selkie::diagrams::sequence::Note,
    actors: &[ActorIn],
    y: f32,
    overrides: &ManualLayoutOverrides,
) -> Option<SeqNote> {
    let actor = actors.iter().find(|actor| actor.name == note.actor)?;
    let actor_to = note
        .actor_to
        .as_ref()
        .and_then(|name| actors.iter().find(|actor| actor.name == *name));
    let lines = note
        .message
        .replace("<br/>", "\n")
        .replace("<br>", "\n")
        .lines()
        .count()
        .max(1) as f32;
    let mut w = NOTE_W;
    let mut h = NOTE_MIN_H.max(18.0 + lines * 16.0);
    let (mut x, mut y) = match note.placement {
        Placement::LeftOf => (actor.center_x() - NOTE_W - 18.0, y),
        Placement::RightOf => (actor.center_x() + 18.0, y),
        Placement::Over => {
            let left = actor
                .center_x()
                .min(actor_to.map(ActorIn::center_x).unwrap_or(actor.center_x()));
            let right = actor
                .center_x()
                .max(actor_to.map(ActorIn::center_x).unwrap_or(actor.center_x()));
            w = w.max((right - left) + 72.0);
            (left - 36.0, y)
        }
    };
    if let Some(ov) = sequence_note_override(overrides, idx, note) {
        apply_box_override(&mut x, &mut y, &mut w, &mut h, ov);
    }
    Some(SeqNote {
        x,
        y,
        w,
        h,
        text: note.message.replace("<br/>", "\n").replace("<br>", "\n"),
        fill: 0x3A3320,
        stroke: theme::MERMAID_GROUP_TITLE,
        text_color: theme::TEXT,
        font_size: 11.0,
    })
}

fn sequence_note_override<'a>(
    overrides: &'a ManualLayoutOverrides,
    idx: usize,
    note: &selkie::diagrams::sequence::Note,
) -> Option<&'a BoxOverride> {
    let by_index = format!("note:{idx}");
    let by_actor = format!("note:{}", note.actor);
    let by_span = note
        .actor_to
        .as_ref()
        .map(|to| format!("note:{}->{}", note.actor, to));
    overrides
        .object(&by_index)
        .or_else(|| overrides.object(&idx.to_string()))
        .or_else(|| overrides.object(&by_actor))
        .or_else(|| by_span.as_deref().and_then(|key| overrides.object(key)))
}

fn apply_box_override(x: &mut f32, y: &mut f32, w: &mut f32, h: &mut f32, ov: &BoxOverride) {
    if let Some(v) = ov.x {
        *x = v;
    }
    if let Some(v) = ov.y {
        *y = v;
    }
    if let Some(v) = ov.w {
        *w = v;
    }
    if let Some(v) = ov.h {
        *h = v;
    }
}
impl Default for ActorIn {
    fn default() -> Self {
        Self::new_unset(0)
    }
}

/// True when this `LineType` represents a real message we want to draw.
/// Fragment markers and activation toggles return false and are dropped in
/// phase 1 (will be honoured in phase 2).
fn is_drawable_line(t: LineType) -> bool {
    use LineType::*;
    matches!(
        t,
        Solid
            | Dotted
            | SolidCross
            | DottedCross
            | SolidOpen
            | DottedOpen
            | SolidPoint
            | DottedPoint
            | BidirectionalSolid
            | BidirectionalDotted
    )
}

fn line_kind_to_style_arrows(t: LineType) -> (MessageStyle, MessageArrow, MessageArrow) {
    use LineType::*;
    use MessageArrow::*;
    match t {
        Solid => (MessageStyle::Solid, None, Filled),
        Dotted => (MessageStyle::Dotted, None, Filled),
        SolidOpen => (MessageStyle::Solid, None, Open),
        DottedOpen => (MessageStyle::Dotted, None, Open),
        SolidCross => (MessageStyle::Solid, None, Cross),
        DottedCross => (MessageStyle::Dotted, None, Cross),
        SolidPoint => (MessageStyle::Solid, None, None),
        DottedPoint => (MessageStyle::Dotted, None, None),
        // Bidirectional: arrowheads at both ends.
        BidirectionalSolid => (MessageStyle::Solid, Filled, Filled),
        BidirectionalDotted => (MessageStyle::Dotted, Filled, Filled),
        _ => (MessageStyle::Solid, None, Filled),
    }
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// Draw a sequence diagram. Shares the signature shape of the flowchart
/// renderer in `mermaid::render` so the dispatcher there can call either.
pub unsafe fn draw(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    graph: &SequenceGraph,
    ox: f32,
    oy: f32,
    scale: f32,
    mut brush: impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    mut fmt: impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let tx = |x: f32| ox + x * scale;
    let ty = |y: f32| oy + y * scale;
    let ts = |v: f32| v * scale;

    // Lifelines first so messages draw on top.
    let dash = crate::mermaid::render::sequence_dash_style(factory);
    for a in &graph.actors {
        let br = brush(theme::MERMAID_EDGE)?;
        target.DrawLine(
            Vector2 {
                X: tx(a.lifeline_x),
                Y: ty(a.lifeline_y0),
            },
            Vector2 {
                X: tx(a.lifeline_x),
                Y: ty(a.lifeline_y1),
            },
            &br,
            1.0,
            Some(dash),
        );
    }

    // Actor boxes
    let reg = crate::mermaid::shape_def::registry();
    for a in &graph.actors {
        let box_x = tx(a.box_x);
        let box_y = ty(a.box_y);
        let box_w = ts(a.box_w);
        let box_h = ts(a.box_h);
        let fill_br = brush(a.fill)?;
        let stroke_br = brush(a.stroke)?;

        // Default label rect — full actor box.
        let mut label_rect = D2D_RECT_F {
            left: box_x,
            top: box_y,
            right: box_x + box_w,
            bottom: box_y + box_h,
        };

        match a.shape {
            Some(crate::mermaid::ir::Shape::Custom(idx)) => {
                if let Some(def) = reg.get(idx) {
                    let geo = crate::mermaid::render::build_custom_geometry_pub(
                        factory, def, box_x, box_y, box_w, box_h,
                    )?;
                    target.FillGeometry(&geo, &fill_br, None);
                    target.DrawGeometry(
                        &geo,
                        &stroke_br,
                        1.5 * def.stroke_mult,
                        None::<&ID2D1StrokeStyle>,
                    );
                    // Tighter label box from text-area.
                    let (lx0, ly0, lx1, ly1) = def.label_rect();
                    label_rect = D2D_RECT_F {
                        left: box_x + lx0 * box_w,
                        top: box_y + ly0 * box_h,
                        right: box_x + lx1 * box_w,
                        bottom: box_y + ly1 * box_h,
                    };
                } else {
                    // Registry miss — fall back to a rounded rectangle.
                    let rr = D2D1_ROUNDED_RECT {
                        rect: label_rect,
                        radiusX: 6.0,
                        radiusY: 6.0,
                    };
                    target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
                    target.DrawRoundedRectangle(
                        std::ptr::addr_of!(rr),
                        &stroke_br,
                        1.5,
                        None::<&ID2D1StrokeStyle>,
                    );
                }
            }
            _ => {
                // Default participant style.
                let rr = D2D1_ROUNDED_RECT {
                    rect: label_rect,
                    radiusX: 6.0,
                    radiusY: 6.0,
                };
                target.FillRoundedRectangle(std::ptr::addr_of!(rr), &fill_br);
                target.DrawRoundedRectangle(
                    std::ptr::addr_of!(rr),
                    &stroke_br,
                    1.5,
                    None::<&ID2D1StrokeStyle>,
                );
            }
        }

        // Centered label inside whatever rect we computed above.
        let f = fmt(theme::BODY_FONT, a.font_size * scale, true, false)?;
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let buf: Vec<u16> = a.label.encode_utf16().collect();
        target.DrawText(
            &buf,
            &f,
            std::ptr::addr_of!(label_rect),
            &brush(a.text_color)?,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    }

    // Messages
    for m in &graph.messages {
        draw_message(target, factory, m, scale, &tx, &ty, &mut brush, &mut fmt)?;
    }
    for note in &graph.notes {
        draw_note(target, factory, note, &tx, &ty, &ts, &mut brush, &mut fmt)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_note(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    note: &SeqNote,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    ts: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let x = tx(note.x);
    let y = ty(note.y);
    let w = ts(note.w);
    let h = ts(note.h);
    let fold = (10.0 * ts(1.0)).max(6.0);
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    let fill = brush(note.fill)?;
    let stroke = brush(note.stroke)?;
    target.FillRectangle(std::ptr::addr_of!(rect), &fill);
    target.DrawRectangle(
        std::ptr::addr_of!(rect),
        &stroke,
        (1.0 * ts(1.0)).max(0.75),
        None::<&ID2D1StrokeStyle>,
    );
    let fold_geo = crate::mermaid::render::build_polygon_pub(
        factory,
        &[
            (x + w - fold, y),
            (x + w, y + fold),
            (x + w - fold, y + fold),
        ],
    )?;
    let fold_br = brush(0x51482C)?;
    target.FillGeometry(&fold_geo, &fold_br, None);
    target.DrawGeometry(
        &fold_geo,
        &stroke,
        (1.0 * ts(1.0)).max(0.75),
        None::<&ID2D1StrokeStyle>,
    );

    let text_rect = D2D_RECT_F {
        left: x + 8.0 * ts(1.0),
        top: y + 6.0 * ts(1.0),
        right: x + w - 8.0 * ts(1.0),
        bottom: y + h - 6.0 * ts(1.0),
    };
    let f = fmt(theme::BODY_FONT, note.font_size * ts(1.0), false, false)?;
    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
    let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
    let buf: Vec<u16> = note.text.encode_utf16().collect();
    target.DrawText(
        &buf,
        &f,
        std::ptr::addr_of!(text_rect),
        &brush(note.text_color)?,
        D2D1_DRAW_TEXT_OPTIONS_CLIP,
        DWRITE_MEASURING_MODE_NATURAL,
    );
    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    Ok(())
}

unsafe fn draw_message(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    m: &SeqMessage,
    scale: f32,
    tx: &impl Fn(f32) -> f32,
    ty: &impl Fn(f32) -> f32,
    brush: &mut impl FnMut(u32) -> Result<ID2D1SolidColorBrush>,
    fmt: &mut impl FnMut(&'static str, f32, bool, bool) -> Result<IDWriteTextFormat>,
) -> Result<()> {
    let line_br = brush(m.color)?;
    let dot_style = crate::mermaid::render::sequence_dot_style(factory);
    let style: Option<&ID2D1StrokeStyle> = match m.style {
        MessageStyle::Solid => None,
        MessageStyle::Dotted => Some(dot_style),
    };

    let (end_pt, b_pt) = if m.self_loop {
        // Self-loop: out from the lifeline, down, back in.
        let lx = tx(m.from_x);
        let y0 = ty(m.y);
        let y1 = ty(m.y + SELF_LOOP_V);
        let right = lx + SELF_LOOP_W;
        target.DrawLine(
            Vector2 { X: lx, Y: y0 },
            Vector2 { X: right, Y: y0 },
            &line_br,
            1.5,
            style,
        );
        target.DrawLine(
            Vector2 { X: right, Y: y0 },
            Vector2 { X: right, Y: y1 },
            &line_br,
            1.5,
            style,
        );
        target.DrawLine(
            Vector2 { X: right, Y: y1 },
            Vector2 { X: lx, Y: y1 },
            &line_br,
            1.5,
            style,
        );
        // Arrow tip lands on the lifeline coming from the right.
        ((lx, y1), (right, y1))
    } else {
        let x0 = tx(m.from_x);
        let x1 = tx(m.to_x);
        let yy = ty(m.y);
        target.DrawLine(
            Vector2 { X: x0, Y: yy },
            Vector2 { X: x1, Y: yy },
            &line_br,
            1.5,
            style,
        );
        ((x1, yy), (x0, yy))
    };

    // End arrowhead — at `end_pt`, oriented from `b_pt` → `end_pt`.
    draw_message_arrow(target, factory, b_pt, end_pt, m.end_arrow, &line_br)?;
    // Start arrowhead — at `b_pt`, oriented from `end_pt` → `b_pt` (i.e. the
    // arrow points back at the originating actor). Suppressed for self-loops
    // because `b_pt` and `end_pt` are degenerate as a back-pointer there.
    if !m.self_loop {
        draw_message_arrow(target, factory, end_pt, b_pt, m.start_arrow, &line_br)?;
    }

    // Label — above the line for forward messages, centered above the loop
    // top for self-messages.
    if !m.label.is_empty() {
        let font_size = m.font_size * scale;
        let label_w = m.label.chars().count() as f32 * font_size * LABEL_FONT_W_RATIO + 8.0 * scale;
        let label_h = font_size * 1.4 + 2.0 * scale;
        let (lx, ly) = if m.self_loop {
            let cx = end_pt.0 + (b_pt.0 - end_pt.0) / 2.0;
            (cx - label_w / 2.0, end_pt.1 - SELF_LOOP_V - label_h - 2.0)
        } else {
            let cx = (end_pt.0 + b_pt.0) / 2.0;
            (cx - label_w / 2.0, end_pt.1 - label_h - 2.0)
        };

        // Background pill so the label doesn't fight the lifeline dashes.
        let bg = brush(theme::BG)?;
        let r = D2D_RECT_F {
            left: lx,
            top: ly,
            right: lx + label_w,
            bottom: ly + label_h,
        };
        target.FillRectangle(std::ptr::addr_of!(r), &bg);

        let f = fmt(theme::BODY_FONT, font_size, false, false)?;
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
        let buf: Vec<u16> = m.label.encode_utf16().collect();
        target.DrawText(
            &buf,
            &f,
            std::ptr::addr_of!(r),
            &brush(m.label_color)?,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
        let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
        let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    }
    Ok(())
}

unsafe fn draw_message_arrow(
    target: &ID2D1RenderTarget,
    factory: &ID2D1Factory1,
    b: (f32, f32),
    a: (f32, f32),
    kind: MessageArrow,
    brush: &ID2D1SolidColorBrush,
) -> Result<()> {
    let dx = a.0 - b.0;
    let dy = a.1 - b.1;
    let len = (dx * dx + dy * dy).sqrt().max(0.0001);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let size = 10.0_f32;

    match kind {
        MessageArrow::None => {}
        MessageArrow::Filled => {
            let tip = (a.0, a.1);
            let back = (a.0 - ux * size, a.1 - uy * size);
            let half = size * 0.5;
            let l = (back.0 + px * half, back.1 + py * half);
            let r = (back.0 - px * half, back.1 - py * half);
            let geo = crate::mermaid::render::build_polygon_pub(factory, &[tip, l, r])?;
            target.FillGeometry(&geo, brush, None);
        }
        MessageArrow::Open => {
            // Two strokes forming an open "<" / ">" head pointing at `a`.
            let back = (a.0 - ux * size, a.1 - uy * size);
            let half = size * 0.6;
            let l = (back.0 + px * half, back.1 + py * half);
            let r = (back.0 - px * half, back.1 - py * half);
            target.DrawLine(
                Vector2 { X: a.0, Y: a.1 },
                Vector2 { X: l.0, Y: l.1 },
                brush,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
            target.DrawLine(
                Vector2 { X: a.0, Y: a.1 },
                Vector2 { X: r.0, Y: r.1 },
                brush,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
        }
        MessageArrow::Cross => {
            let half = size * 0.5;
            let v0 = (a.0 + px * half, a.1 + py * half);
            let v1 = (a.0 - px * half, a.1 - py * half);
            let h0 = (a.0 + ux * half, a.1 + uy * half);
            let h1 = (a.0 - ux * half, a.1 - uy * half);
            target.DrawLine(
                Vector2 { X: v0.0, Y: v0.1 },
                Vector2 { X: v1.0, Y: v1.1 },
                brush,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
            target.DrawLine(
                Vector2 { X: h0.0, Y: h0.1 },
                Vector2 { X: h1.0, Y: h1.1 },
                brush,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_notes_and_manual_overrides() {
        let source = r#"sequenceDiagram
    participant Client
    participant API
    participant DB
    Client->>API: request
    Note right of API: validate input
    API->>DB: lookup
    Note over API,DB: shared transaction
    DB-->>API: row
    %% @node Client x=40 w=110
    %% @node API x=230 w=110
    %% @node DB x=430 w=110
    %% @note note:0 x=285 y=90 w=150 h=46
    %% @note note:1 x=255 y=170 w=270 h=46
    %% @graph w=600 h=280
"#;
        let graph = match crate::mermaid::build(source).unwrap() {
            Graph::Sequence(graph) => graph,
            other => panic!("expected sequence graph, got {other:?}"),
        };

        assert_eq!(graph.actors.len(), 3);
        assert_eq!(graph.messages.len(), 3);
        assert_eq!(graph.notes.len(), 2);
        let client = graph
            .actors
            .iter()
            .find(|actor| actor.label == "Client")
            .unwrap();
        let api = graph
            .actors
            .iter()
            .find(|actor| actor.label == "API")
            .unwrap();
        assert_eq!((client.box_x, client.box_w), (40.0, 110.0));
        assert_eq!((api.box_x, api.box_w), (230.0, 110.0));
        assert_eq!((graph.notes[0].x, graph.notes[0].y), (285.0, 90.0));
        assert_eq!((graph.notes[1].w, graph.notes[1].h), (270.0, 46.0));
        assert_eq!((graph.width, graph.height), (600.0, 280.0));
    }
}
