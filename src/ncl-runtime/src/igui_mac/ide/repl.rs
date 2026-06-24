//! Mac-native REPL pane.
//!
//! A scrollback transcript plus an input line built on the shared
//! `Editor`. Ports the shape of the Windows `igui::repl_child` onto the
//! portable stack: the pane is eval-agnostic — it does not depend on the
//! compiler. `handle_event` returns `Some(source)` when the user submits a
//! complete form; the worker thread evaluates it (via the compiler) and
//! calls `output` / `error` with the result. This keeps the REPL in
//! `ncl-runtime` with no upward dependency on the compiler.

use crate::igui_events::{modifier, IGuiEvent};
use crate::igui_mac::events::vk;
use crate::igui_mac::ide::editor::{Editor, Theme};
use crate::igui_paint::{
    FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Input,
    Output,
    Error,
    Info,
}

struct Line {
    kind: LineKind,
    text: String,
}

pub struct Repl {
    transcript: Vec<Line>,
    input: Editor,
    history: Vec<String>,
    hist_idx: Option<usize>,
    /// Scroll offset from the bottom of the transcript, in lines.
    scroll_from_bottom: usize,
    theme: Theme,
    prompt: String,
    input_color: Rgba,
    output_color: Rgba,
    error_color: Rgba,
    info_color: Rgba,
    visible_transcript_rows: usize,
}

#[inline]
fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
}

/// True if every bracket in `src` is matched, ignoring brackets inside
/// `"…"` strings, `#\x` char literals, and `;` line comments. Used to
/// decide whether Return submits the form or inserts a newline.
pub fn parens_balanced(src: &str) -> bool {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut in_comment = false;
    let mut escape = false;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if in_comment {
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            ';' => in_comment = true,
            '"' => in_string = true,
            '#' if chars.peek() == Some(&'\\') => {
                chars.next(); // backslash
                chars.next(); // the literal char (skip; may be a paren)
            }
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false; // unbalanced close
        }
    }
    depth == 0 && !in_string
}

impl Repl {
    pub fn new(theme: Theme) -> Self {
        let mut input = Editor::new();
        input.show_gutter = false;
        input.set_theme(theme.clone());
        let mut r = Self {
            transcript: Vec::new(),
            input,
            history: Vec::new(),
            hist_idx: None,
            scroll_from_bottom: 0,
            theme,
            prompt: "λ> ".into(),
            input_color: rgb(168, 218, 255),
            output_color: rgb(220, 223, 228),
            error_color: rgb(231, 111, 81),
            info_color: rgb(148, 210, 189),
            visible_transcript_rows: 1,
        };
        r.print(LineKind::Info, "NCL REPL — type a form and press Return.");
        r
    }

    pub fn set_metrics(&mut self, cell_w: f32, cell_h: f32, ascent: f32) {
        self.theme.cell_w = cell_w;
        self.theme.cell_h = cell_h;
        self.theme.ascent = ascent;
        self.input.set_metrics(cell_w, cell_h, ascent);
    }

    /// Append text (split on newlines) to the transcript.
    pub fn print(&mut self, kind: LineKind, text: &str) {
        for line in text.split('\n') {
            self.transcript.push(Line { kind, text: line.to_string() });
        }
        self.scroll_from_bottom = 0; // jump to bottom on new output
    }
    pub fn output(&mut self, text: &str) {
        self.print(LineKind::Output, text);
    }
    pub fn error(&mut self, text: &str) {
        self.print(LineKind::Error, text);
    }
    pub fn info(&mut self, text: &str) {
        self.print(LineKind::Info, text);
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(idx);
        self.input.set_text(&self.history[idx]);
    }
    fn history_next(&mut self) {
        match self.hist_idx {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.hist_idx = Some(i + 1);
                self.input.set_text(&self.history[i + 1]);
            }
            Some(_) => {
                self.hist_idx = None;
                self.input.clear();
            }
        }
    }

    /// Route an event. Returns `Some(source)` when a complete form is
    /// submitted (Return on balanced input) — the caller evaluates it and
    /// calls `output`/`error`. Otherwise the input editor handles it.
    pub fn handle_event(&mut self, ev: &IGuiEvent) -> Option<String> {
        match ev {
            IGuiEvent::Char { codepoint, .. } => {
                self.input.on_char(*codepoint as u32);
                None
            }
            IGuiEvent::Key { vkey, mods, down, .. } if *down => self.on_key(*vkey, *mods),
            IGuiEvent::Mouse { op, wheel_lines, .. } if *op == crate::igui_events::mouse_op::WHEEL => {
                let new = self.scroll_from_bottom as i64 - *wheel_lines;
                self.scroll_from_bottom = new.max(0) as usize;
                None
            }
            _ => None,
        }
    }

    fn on_key(&mut self, vkey: i64, mods: i64) -> Option<String> {
        let shift = mods & modifier::SHIFT != 0;
        if vkey == vk::RETURN && !shift {
            let src = self.input.text();
            if !src.trim().is_empty() && parens_balanced(&src) {
                self.input.clear();
                self.hist_idx = None;
                self.history.push(src.clone());
                // Echo the submission into the transcript with the prompt.
                let echo = format!("{}{}", self.prompt, src.replace('\n', "\n   "));
                self.print(LineKind::Input, &echo);
                return Some(src);
            }
            // Incomplete form → newline (continue editing).
            self.input.on_key(vk::RETURN, mods);
            return None;
        }
        // History recall at the input boundaries.
        let (row, _) = self.input.cursor_rc();
        if vkey == vk::UP && row == 0 {
            self.history_prev();
            return None;
        }
        if vkey == vk::DOWN && row + 1 >= self.input.line_count() {
            self.history_next();
            return None;
        }
        self.input.on_key(vkey, mods);
        None
    }

    fn color_for(&self, kind: LineKind) -> Rgba {
        match kind {
            LineKind::Input => self.input_color,
            LineKind::Output => self.output_color,
            LineKind::Error => self.error_color,
            LineKind::Info => self.info_color,
        }
    }

    /// Render the REPL into `area`: a scrollback transcript on top and the
    /// input line (with prompt) on the bottom.
    pub fn render(&mut self, area: Rect) -> Vec<SurfaceCmd> {
        let t = self.theme.clone();
        let cell_h = t.cell_h.max(1.0);
        let cell_w = t.cell_w.max(1.0);

        // Input area: 1–6 lines tall depending on input content.
        let input_lines = self.input.line_count().clamp(1, 6) as f32;
        let input_h = input_lines * cell_h + 8.0;
        let split_y = (area.y1 - input_h).max(area.y0 + cell_h);

        let transcript_area = Rect { x0: area.x0, y0: area.y0, x1: area.x1, y1: split_y };
        let input_area = Rect { x0: area.x0, y0: split_y, x1: area.x1, y1: area.y1 };

        let mut cmds = vec![SurfaceCmd::Clear { color: t.bg }];

        // ── transcript ──
        let avail = ((transcript_area.y1 - transcript_area.y0) / cell_h).floor().max(1.0) as usize;
        self.visible_transcript_rows = avail;
        let total = self.transcript.len();
        let max_scroll = total.saturating_sub(avail);
        let scroll = self.scroll_from_bottom.min(max_scroll);
        let end = total - scroll;
        let start = end.saturating_sub(avail);
        let pad_x = transcript_area.x0 + 6.0;
        for (i, line) in self.transcript[start..end].iter().enumerate() {
            let y = transcript_area.y0 + i as f32 * cell_h;
            if line.text.is_empty() {
                continue;
            }
            cmds.push(SurfaceCmd::DrawTextRun {
                run: TextRun {
                    text: line.text.clone(),
                    origin: Point { x: pad_x, y },
                    family: t.family.clone(),
                    size: t.size,
                    weight: 400,
                    style: FontStyle::Normal,
                    stretch: FontStretch::Normal,
                    locale: "en-us".into(),
                    color: self.color_for(line.kind),
                    max_width: None,
                    alignment: TextAlign::Leading,
                    trimming: TextTrimming::None,
                },
            });
        }

        // ── separator ──
        cmds.push(SurfaceCmd::DrawLine {
            p0: Point { x: area.x0, y: split_y },
            p1: Point { x: area.x1, y: split_y },
            half_thickness: 0.5,
            color: t.gutter_fg,
        });

        // ── input line: prompt + editor ──
        let prompt_w = self.prompt.chars().count() as f32 * cell_w;
        cmds.push(SurfaceCmd::DrawTextRun {
            run: TextRun {
                text: self.prompt.clone(),
                origin: Point { x: input_area.x0 + 6.0, y: input_area.y0 + 4.0 },
                family: t.family.clone(),
                size: t.size,
                weight: 600,
                style: FontStyle::Normal,
                stretch: FontStretch::Normal,
                locale: "en-us".into(),
                color: self.info_color,
                max_width: None,
                alignment: TextAlign::Leading,
                trimming: TextTrimming::None,
            },
        });
        // Render the input editor to the right of the prompt. Suppress its
        // own background clear so it composes over the REPL background.
        let editor_area = Rect {
            x0: input_area.x0 + 6.0 + prompt_w,
            y0: input_area.y0 + 4.0,
            x1: input_area.x1 - 4.0,
            y1: input_area.y1 - 4.0,
        };
        for c in self.input.render(editor_area) {
            // Drop the editor's full-surface Clear so it doesn't paint over
            // the transcript; keep everything else (text, caret, selection).
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_detects_complete_and_incomplete() {
        assert!(parens_balanced("(+ 1 2)"));
        assert!(!parens_balanced("(+ 1 2"));
        assert!(parens_balanced("(list \"a)b\" 1)")); // paren in string ignored
        assert!(parens_balanced("(foo) ; (bar"));     // paren in comment ignored
        assert!(parens_balanced("#\\(")); // char literal paren ignored
        assert!(!parens_balanced(")"));
        assert!(parens_balanced(""));
    }

    fn key(vkey: i64) -> IGuiEvent {
        IGuiEvent::Key { child_id: 1, vkey, scancode: 0, mods: 0, repeat: 0, down: true, time_ms: 0 }
    }
    fn ch(c: char) -> IGuiEvent {
        IGuiEvent::Char { child_id: 1, codepoint: c as i64, mods: 0, time_ms: 0 }
    }

    #[test]
    fn submitting_a_complete_form_returns_source() {
        let mut r = Repl::new(Theme::default());
        for c in "(+ 1 2)".chars() {
            assert!(r.handle_event(&ch(c)).is_none());
        }
        let submitted = r.handle_event(&key(vk::RETURN));
        assert_eq!(submitted.as_deref(), Some("(+ 1 2)"));
        // Input cleared, history recorded.
        assert_eq!(r.input.text(), "");
        assert_eq!(r.history, vec!["(+ 1 2)".to_string()]);
    }

    #[test]
    fn incomplete_form_inserts_newline_not_submit() {
        let mut r = Repl::new(Theme::default());
        for c in "(+ 1".chars() {
            r.handle_event(&ch(c));
        }
        assert!(r.handle_event(&key(vk::RETURN)).is_none());
        assert!(r.input.text().contains('\n'), "incomplete form should newline");
    }

    #[test]
    fn history_recall_with_up_arrow() {
        let mut r = Repl::new(Theme::default());
        for c in "(foo)".chars() {
            r.handle_event(&ch(c));
        }
        r.handle_event(&key(vk::RETURN));
        assert_eq!(r.input.text(), "");
        r.handle_event(&key(vk::UP));
        assert_eq!(r.input.text(), "(foo)");
    }

    #[test]
    fn output_lands_in_transcript() {
        let mut r = Repl::new(Theme::default());
        r.output("3");
        assert!(r.transcript.iter().any(|l| l.text == "3" && l.kind == LineKind::Output));
    }
}
