//! The IDE shell: an editor pane and a REPL pane stacked in one window,
//! with focus switching and "eval from editor → REPL".
//!
//! Eval-agnostic, like `Repl`: `handle_event` returns `IdeAction::Eval`
//! with the source to evaluate; the driver runs it through the compiler
//! `Session` and calls `output`/`error`. The result always lands in the
//! REPL transcript so there is one log.

use crate::igui_events::{modifier, IGuiEvent};
use crate::igui_mac::events::vk;
use crate::igui_mac::ide::editor::{Editor, Theme};
use crate::igui_mac::ide::repl::Repl;
use crate::igui_paint::{Rect, Rgba, SurfaceCmd};

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
    editor: Editor,
    repl: Repl,
    focus: Focus,
    theme: Theme,
    /// Fraction of the height given to the editor (top). REPL gets the rest.
    split: f32,
    width: f32,
    height: f32,
}

impl Ide {
    pub fn new(theme: Theme) -> Self {
        let mut editor = Editor::with_text(
            ";; Editor — edit Lisp here. Cmd-R runs the buffer; Cmd-Return\n\
             ;; evaluates the top-level form at the cursor. Cmd-L focuses the\n\
             ;; REPL, Cmd-E the editor.\n\n(defun square (x)\n  (* x x))\n",
        );
        editor.set_theme(theme.clone());
        // Park the cursor inside the sample form so Cmd-Return works out of
        // the box. `find` gives a byte offset; convert to a code-point offset.
        let txt = editor.text();
        if let Some(pb) = txt.find("(defun") {
            editor.set_cursor(txt[..pb].chars().count() + 1);
        }
        let repl = Repl::new(theme.clone());
        Self {
            editor,
            repl,
            focus: Focus::Repl,
            theme,
            split: 0.62,
            width: 900.0,
            height: 620.0,
        }
    }

    pub fn set_metrics(&mut self, cell_w: f32, cell_h: f32, ascent: f32) {
        self.editor.set_metrics(cell_w, cell_h, ascent);
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

    /// Load a file into the editor pane and focus it.
    pub fn load_file(&mut self, path: &str) {
        match self.editor.load_file(path) {
            Ok(()) => {
                self.focus = Focus::Editor;
                self.repl.info(&format!("; loaded {path}"));
            }
            Err(e) => self.repl.error(&format!("open {path}: {e}")),
        }
    }

    fn status_h(&self) -> f32 {
        self.theme.cell_h.max(12.0) + 4.0
    }
    fn editor_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: 0.0, x1: self.width, y1: div - self.status_h() }
    }
    fn status_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: div - self.status_h(), x1: self.width, y1: div - 1.0 }
    }
    fn repl_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: div + 1.0, x1: self.width, y1: self.height }
    }

    /// Route an event to the focused pane, handling IDE-global keys
    /// (focus switch, run-buffer, eval-form). Returns an action for the
    /// driver (an eval request) or `None`.
    pub fn handle_event(&mut self, ev: &IGuiEvent) -> IdeAction {
        match ev {
            IGuiEvent::Key { vkey, mods, down, .. } if *down => self.on_key(*vkey, *mods),
            IGuiEvent::Char { codepoint, .. } => {
                match self.focus {
                    Focus::Editor => {
                        self.editor.on_char(*codepoint as u32);
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
                            self.editor.on_click(mx, my, area, false);
                        } else if drag {
                            self.editor.on_click(mx, my, area, true); // extend selection
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
        // IDE-global accelerators.
        if cmd {
            match vkey {
                0x45 => {
                    self.focus = Focus::Editor;
                    return IdeAction::None;
                } // Cmd-E → editor
                0x4C => {
                    self.focus = Focus::Repl;
                    return IdeAction::None;
                } // Cmd-L → listener
                0x52 => {
                    // Cmd-R → run whole editor buffer.
                    let src = self.editor.text();
                    if !src.trim().is_empty() {
                        self.repl.info("; run buffer");
                        return IdeAction::Eval(src);
                    }
                    return IdeAction::None;
                }
                0x53 => {
                    // Cmd-S → save the editor's backing file.
                    match self.editor.save() {
                        Ok(Some(p)) => self.repl.info(&format!("; saved {p}")),
                        Ok(None) => self
                            .repl
                            .info("; no file — launch with `ncl --windows <file.lisp>` to set one"),
                        Err(e) => self.repl.error(&format!("save failed: {e}")),
                    }
                    return IdeAction::None;
                }
                _ => {}
            }
        }
        match self.focus {
            Focus::Editor => {
                // Cmd-Return → eval the top-level form at the cursor.
                if cmd && vkey == vk::RETURN {
                    if let Some(form) = self.editor.current_form() {
                        self.repl.info(&format!("; {form}"));
                        return IdeAction::Eval(form);
                    }
                    return IdeAction::None;
                }
                self.editor.on_key(vkey, mods);
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

    /// Render both panes plus a focus-indicating divider.
    pub fn render(&mut self, area: Rect) -> Vec<SurfaceCmd> {
        self.width = area.x1 - area.x0;
        self.height = area.y1 - area.y0;
        let ea = self.editor_area();
        let ra = self.repl_area();
        let div_y = ea.y1;

        let mut cmds = vec![SurfaceCmd::Clear { color: self.theme.bg }];

        // Editor pane (drop its own Clear so it composes within its area).
        for c in self.editor.render(ea) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }
        // Status bar (editor file • L:C • modified).
        let sa = self.status_area();
        cmds.push(SurfaceCmd::FillRect {
            rect: sa,
            corner_radius: 0.0,
            color: Rgba { r: 0.16, g: 0.18, b: 0.22, a: 1.0 },
        });
        cmds.push(SurfaceCmd::DrawTextRun {
            run: crate::igui_paint::TextRun {
                text: self.editor.status(),
                origin: crate::igui_paint::Point { x: sa.x0 + 8.0, y: sa.y0 + 2.0 },
                family: self.theme.family.clone(),
                size: self.theme.size - 1.0,
                weight: 400,
                style: crate::igui_paint::FontStyle::Normal,
                stretch: crate::igui_paint::FontStretch::Normal,
                locale: "en-us".into(),
                color: self.theme.gutter_fg,
                max_width: None,
                alignment: crate::igui_paint::TextAlign::Leading,
                trimming: crate::igui_paint::TextTrimming::None,
            },
        });

        // REPL pane.
        for c in self.repl.render(ra) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }

        // Divider — brighter on the focused side.
        let (etop, ebot) = match self.focus {
            Focus::Editor => (focus_color(), Rgba { r: 0.3, g: 0.33, b: 0.4, a: 1.0 }),
            Focus::Repl => (Rgba { r: 0.3, g: 0.33, b: 0.4, a: 1.0 }, focus_color()),
        };
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: area.x0, y0: div_y - 2.0, x1: area.x1, y1: div_y },
            corner_radius: 0.0,
            color: etop,
        });
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: area.x0, y0: div_y, x1: area.x1, y1: div_y + 2.0 },
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
}
