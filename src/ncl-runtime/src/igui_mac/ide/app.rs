//! The IDE shell: a tabbed stack of editor buffers and a REPL pane in one
//! window, with focus switching and "eval from editor → REPL".
//!
//! Eval-agnostic, like `Repl`: `handle_event` returns `IdeAction::Eval`
//! with the source to evaluate; the driver runs it through the compiler
//! `Session` and calls `output`/`error`. The result always lands in the
//! REPL transcript so there is one log.

use crate::igui_events::{modifier, IGuiEvent};
use crate::igui_mac::events::vk;
use crate::igui_mac::ide::editor::{Editor, Theme};
use crate::igui_mac::ide::repl::Repl;
use crate::igui_paint::{
    FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Repl,
}

/// What the driver should do after an event.
pub enum IdeAction {
    None,
    /// Evaluate this source through the compiler, then call
    /// `output`/`error` with the result.
    Eval(String),
}

pub struct Ide {
    buffers: Vec<Editor>,
    active: usize,
    repl: Repl,
    focus: Focus,
    theme: Theme,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    /// Fraction of the height given to the editor (top). REPL gets the rest.
    split: f32,
    width: f32,
    height: f32,
}

impl Ide {
    pub fn new(theme: Theme) -> Self {
        let mut editor = Editor::with_text(
            ";; Scratch — edit Lisp here. Cmd-R runs the buffer; Cmd-Return\n\
             ;; evaluates the form at the cursor. Cmd-T new tab, Cmd-W close,\n\
             ;; Cmd-1..9 switch, Cmd-E/L focus editor/REPL.\n\n(defun square (x)\n  (* x x))\n",
        );
        editor.set_theme(theme.clone());
        let txt = editor.text();
        if let Some(pb) = txt.find("(defun") {
            editor.set_cursor(txt[..pb].chars().count() + 1);
        }
        let repl = Repl::new(theme.clone());
        Self {
            buffers: vec![editor],
            active: 0,
            repl,
            focus: Focus::Repl,
            cell_w: theme.cell_w,
            cell_h: theme.cell_h,
            ascent: theme.ascent,
            theme,
            split: 0.62,
            width: 900.0,
            height: 620.0,
        }
    }

    #[inline]
    fn ed(&self) -> &Editor {
        &self.buffers[self.active]
    }
    #[inline]
    fn ed_mut(&mut self) -> &mut Editor {
        &mut self.buffers[self.active]
    }

    pub fn set_metrics(&mut self, cell_w: f32, cell_h: f32, ascent: f32) {
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.ascent = ascent;
        for b in &mut self.buffers {
            b.set_metrics(cell_w, cell_h, ascent);
        }
        self.repl.set_metrics(cell_w, cell_h, ascent);
    }

    pub fn output(&mut self, text: &str) {
        self.repl.output(text);
    }
    pub fn error(&mut self, text: &str) {
        self.repl.error(text);
    }
    pub fn info(&mut self, text: &str) {
        self.repl.info(text);
    }

    // ─── buffers / tabs ───────────────────────────────────────────────

    fn new_buffer(&mut self) {
        let mut e = Editor::new();
        e.set_theme(self.theme.clone());
        e.set_metrics(self.cell_w, self.cell_h, self.ascent);
        self.buffers.push(e);
        self.active = self.buffers.len() - 1;
        self.focus = Focus::Editor;
    }

    fn close_buffer(&mut self) {
        if self.buffers.len() > 1 {
            self.buffers.remove(self.active);
            self.active = self.active.min(self.buffers.len() - 1);
        }
    }

    fn switch_to(&mut self, i: usize) {
        if i < self.buffers.len() {
            self.active = i;
            self.focus = Focus::Editor;
        }
    }

    /// Load a file: reuse the active buffer if it's an untitled, unmodified
    /// scratch buffer; otherwise open a new tab.
    pub fn load_file(&mut self, path: &str) {
        let reuse = self.ed().file_path().is_none() && !self.ed().is_dirty();
        if !reuse {
            self.new_buffer();
        }
        match self.ed_mut().load_file(path) {
            Ok(()) => {
                self.focus = Focus::Editor;
                self.repl.info(&format!("; loaded {path}"));
            }
            Err(e) => self.repl.error(&format!("open {path}: {e}")),
        }
    }

    // ─── layout ───────────────────────────────────────────────────────

    fn tab_h(&self) -> f32 {
        self.cell_h.max(12.0) + 6.0
    }
    fn status_h(&self) -> f32 {
        self.cell_h.max(12.0) + 4.0
    }
    fn editor_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: self.tab_h(), x1: self.width, y1: div - self.status_h() }
    }
    fn tab_area(&self) -> Rect {
        Rect { x0: 0.0, y0: 0.0, x1: self.width, y1: self.tab_h() }
    }
    fn status_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: div - self.status_h(), x1: self.width, y1: div - 1.0 }
    }
    fn repl_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: div + 1.0, x1: self.width, y1: self.height }
    }
    fn tab_width(&self) -> f32 {
        (self.width / self.buffers.len() as f32).min(200.0).max(60.0)
    }

    // ─── events ───────────────────────────────────────────────────────

    /// Route an event to the focused pane, handling IDE-global keys.
    /// Returns an action for the driver (an eval request) or `None`.
    pub fn handle_event(&mut self, ev: &IGuiEvent) -> IdeAction {
        match ev {
            IGuiEvent::Key { vkey, mods, down, .. } if *down => self.on_key(*vkey, *mods),
            IGuiEvent::Char { codepoint, .. } => {
                match self.focus {
                    Focus::Editor => {
                        self.ed_mut().on_char(*codepoint as u32);
                    }
                    Focus::Repl => {
                        self.repl.handle_event(ev);
                    }
                }
                IdeAction::None
            }
            IGuiEvent::Mouse { x, y, op, .. } => {
                use crate::igui_events::mouse_op;
                let (mx, my) = (*x as f32, *y as f32);
                let down = *op == mouse_op::LEFT_DOWN;
                let drag = *op == mouse_op::DRAG;
                // Tab-bar click switches buffers.
                if down && my < self.tab_h() {
                    let i = (mx / self.tab_width()) as usize;
                    self.switch_to(i);
                    return IdeAction::None;
                }
                if down {
                    self.focus = if my < self.editor_area().y1 {
                        Focus::Editor
                    } else {
                        Focus::Repl
                    };
                }
                match self.focus {
                    Focus::Repl => {
                        self.repl.handle_event(ev);
                    }
                    Focus::Editor => {
                        let area = self.editor_area();
                        if down {
                            self.ed_mut().on_click(mx, my, area, false);
                        } else if drag {
                            self.ed_mut().on_click(mx, my, area, true);
                        }
                    }
                }
                IdeAction::None
            }
            _ => IdeAction::None,
        }
    }

    fn on_key(&mut self, vkey: i64, mods: i64) -> IdeAction {
        // Command (WIN) drives IDE-global accelerators; Control is left for
        // the editor's paredit bindings.
        let cmd = mods & modifier::WIN != 0;
        if cmd {
            match vkey {
                0x45 => {
                    self.focus = Focus::Editor;
                    return IdeAction::None;
                } // Cmd-E
                0x4C => {
                    self.focus = Focus::Repl;
                    return IdeAction::None;
                } // Cmd-L
                0x54 => {
                    self.new_buffer();
                    return IdeAction::None;
                } // Cmd-T new tab
                0x57 => {
                    self.close_buffer();
                    return IdeAction::None;
                } // Cmd-W close tab
                0x31..=0x39 => {
                    self.switch_to((vkey - 0x31) as usize);
                    return IdeAction::None;
                } // Cmd-1..9
                0x52 => {
                    let src = self.ed().text();
                    if !src.trim().is_empty() {
                        self.repl.info("; run buffer");
                        return IdeAction::Eval(src);
                    }
                    return IdeAction::None;
                } // Cmd-R run buffer
                0x53 => {
                    match self.ed_mut().save() {
                        Ok(Some(p)) => self.repl.info(&format!("; saved {p}")),
                        Ok(None) => self
                            .repl
                            .info("; no file — launch with `ncl --windows <file.lisp>` to set one"),
                        Err(e) => self.repl.error(&format!("save failed: {e}")),
                    }
                    return IdeAction::None;
                } // Cmd-S
                _ => {}
            }
        }
        match self.focus {
            Focus::Editor => {
                if cmd && vkey == vk::RETURN {
                    if let Some(form) = self.ed().current_form() {
                        self.repl.info(&format!("; {form}"));
                        return IdeAction::Eval(form);
                    }
                    return IdeAction::None;
                }
                self.ed_mut().on_key(vkey, mods);
                IdeAction::None
            }
            Focus::Repl => {
                let ev = IGuiEvent::Key {
                    child_id: 1,
                    vkey,
                    scancode: 0,
                    mods,
                    repeat: 0,
                    down: true,
                    time_ms: 0,
                };
                match self.repl.handle_event(&ev) {
                    Some(src) => IdeAction::Eval(src),
                    None => IdeAction::None,
                }
            }
        }
    }

    // ─── render ───────────────────────────────────────────────────────

    fn tab_label(ed: &Editor) -> String {
        let base = ed
            .file_path()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
            .unwrap_or_else(|| "untitled".into());
        if ed.is_dirty() {
            format!("● {base}")
        } else {
            base
        }
    }

    fn run(&self, text: String, x: f32, y: f32, color: Rgba) -> SurfaceCmd {
        SurfaceCmd::DrawTextRun {
            run: TextRun {
                text,
                origin: Point { x, y },
                family: self.theme.family.clone(),
                size: self.theme.size - 1.0,
                weight: 400,
                style: FontStyle::Normal,
                stretch: FontStretch::Normal,
                locale: "en-us".into(),
                color,
                max_width: None,
                alignment: TextAlign::Leading,
                trimming: TextTrimming::None,
            },
        }
    }

    pub fn render(&mut self, area: Rect) -> Vec<SurfaceCmd> {
        self.width = area.x1 - area.x0;
        self.height = area.y1 - area.y0;
        let ea = self.editor_area();
        let ra = self.repl_area();
        let div_y = ea.y1.max((self.height * self.split).round() - self.status_h());
        let div_real = (self.height * self.split).round();

        let mut cmds = vec![SurfaceCmd::Clear { color: self.theme.bg }];

        // ── tab bar ──
        let ta = self.tab_area();
        cmds.push(SurfaceCmd::FillRect {
            rect: ta,
            corner_radius: 0.0,
            color: Rgba { r: 0.10, g: 0.11, b: 0.14, a: 1.0 },
        });
        let tw = self.tab_width();
        for (i, b) in self.buffers.iter().enumerate() {
            let x0 = i as f32 * tw;
            let active = i == self.active;
            cmds.push(SurfaceCmd::FillRect {
                rect: Rect { x0: x0 + 1.0, y0: 2.0, x1: x0 + tw - 1.0, y1: ta.y1 },
                corner_radius: 4.0,
                color: if active {
                    Rgba { r: 0.18, g: 0.20, b: 0.26, a: 1.0 }
                } else {
                    Rgba { r: 0.12, g: 0.13, b: 0.16, a: 1.0 }
                },
            });
            let col = if active { self.theme.fg } else { self.theme.gutter_fg };
            cmds.push(self.run(Self::tab_label(b), x0 + 8.0, 4.0, col));
        }

        // ── editor pane ──
        for c in self.ed_mut().render(ea) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }

        // ── status bar ──
        let sa = self.status_area();
        cmds.push(SurfaceCmd::FillRect {
            rect: sa,
            corner_radius: 0.0,
            color: Rgba { r: 0.16, g: 0.18, b: 0.22, a: 1.0 },
        });
        let status = self.ed().status();
        cmds.push(self.run(status, sa.x0 + 8.0, sa.y0 + 2.0, self.theme.gutter_fg));

        // ── REPL pane ──
        for c in self.repl.render(ra) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }

        // ── focus-indicating divider ──
        let _ = div_y;
        let (etop, ebot) = match self.focus {
            Focus::Editor => (focus_color(), Rgba { r: 0.3, g: 0.33, b: 0.4, a: 1.0 }),
            Focus::Repl => (Rgba { r: 0.3, g: 0.33, b: 0.4, a: 1.0 }, focus_color()),
        };
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: area.x0, y0: div_real - 2.0, x1: area.x1, y1: div_real },
            corner_radius: 0.0,
            color: etop,
        });
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: area.x0, y0: div_real, x1: area.x1, y1: div_real + 2.0 },
            corner_radius: 0.0,
            color: ebot,
        });
        cmds
    }
}

fn focus_color() -> Rgba {
    Rgba { r: 0.47, g: 0.78, b: 1.0, a: 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vkey: i64, mods: i64) -> IGuiEvent {
        IGuiEvent::Key { child_id: 1, vkey, scancode: 0, mods, repeat: 0, down: true, time_ms: 0 }
    }

    #[test]
    fn cmd_r_runs_the_buffer() {
        let mut ide = Ide::new(Theme::default());
        ide.focus = Focus::Editor;
        match ide.on_key(0x52, modifier::WIN) {
            IdeAction::Eval(src) => assert!(src.contains("defun square")),
            _ => panic!("Cmd-R should request an eval"),
        }
    }

    #[test]
    fn repl_return_evaluates() {
        let mut ide = Ide::new(Theme::default());
        ide.focus = Focus::Repl;
        for c in "(+ 2 3)".chars() {
            ide.handle_event(&IGuiEvent::Char {
                child_id: 1,
                codepoint: c as i64,
                mods: 0,
                time_ms: 0,
            });
        }
        match ide.handle_event(&key(vk::RETURN, 0)) {
            IdeAction::Eval(src) => assert_eq!(src, "(+ 2 3)"),
            _ => panic!("Return in REPL should eval"),
        }
    }

    #[test]
    fn cmd_enter_evals_form_at_point() {
        let mut ide = Ide::new(Theme::default());
        ide.focus = Focus::Editor;
        match ide.on_key(vk::RETURN, modifier::WIN) {
            IdeAction::Eval(src) => assert!(src.starts_with('(') && src.contains("defun")),
            _ => panic!("Cmd-Return should eval the form at point"),
        }
    }

    #[test]
    fn new_close_and_switch_buffers() {
        let mut ide = Ide::new(Theme::default());
        assert_eq!(ide.buffers.len(), 1);
        ide.on_key(0x54, modifier::WIN); // Cmd-T
        assert_eq!(ide.buffers.len(), 2);
        assert_eq!(ide.active, 1);
        ide.on_key(0x31, modifier::WIN); // Cmd-1 → first tab
        assert_eq!(ide.active, 0);
        ide.on_key(0x57, modifier::WIN); // Cmd-W close active
        assert_eq!(ide.buffers.len(), 1);
    }
}
