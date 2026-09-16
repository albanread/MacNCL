//! The IDE shell: a tabbed stack of editor buffers and a REPL pane in one
//! window, with focus switching and "eval from editor → REPL".
//!
//! Eval-agnostic, like `Repl`: `handle_event` returns `IdeAction::Eval`
//! with the source to evaluate; the driver runs it through the compiler
//! `Session` and calls `output`/`error`. The result always lands in the
//! REPL transcript so there is one log.
//!
//! Menus live in the **system menu bar** (`igui_mac::menu`); picks arrive
//! here as `IGuiEvent::Menu` events and route through `run_menu_cmd`, the
//! same dispatcher the keyboard shortcuts use.

use crate::igui_events::{menu_cmd, modifier, IGuiEvent};
use crate::igui_mac::events::vk;
use crate::igui_mac::ide::editor::{Editor, Theme};
use crate::igui_mac::ide::repl::Repl;
use crate::igui_mac::menu;
use crate::igui_mac::theme::{fixed_dark, fixed_light, SystemTheme};
use crate::igui_paint::{
    FontStretch, FontStyle, Point, Rect, Rgba, SurfaceCmd, TextAlign, TextRun, TextTrimming,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Repl,
}

/// What the driver should do after an event.
#[derive(Debug)]
pub enum IdeAction {
    None,
    /// Evaluate this source through the compiler, then call
    /// `output`/`error` with the result.
    Eval(String),
    /// The code font size changed — re-measure cell metrics via Core Text
    /// and call `set_metrics` before the next render.
    Remeasure,
}

/// A menu command. The system menu bar's dispatcher, the key-equivalent
/// swallow in the event monitor, and `NCL_GUI_MENU` all arrive as
/// `IGuiEvent::Menu`; this is their shared endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MenuCmd {
    New,
    CloseTab,
    Save,
    Settings,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Find,
    FindNext,
    Comment,
    RunBuffer,
    EvalForm,
    ClearRepl,
    FocusEditor,
    FocusRepl,
    FontUp,
    FontDown,
    Help,
}

impl MenuCmd {
    /// Map a `menu_cmd::*` opcode to its command. Unknown ids no-op.
    fn from_opcode(op: i64) -> Option<Self> {
        Some(match op {
            menu_cmd::NEW => Self::New,
            menu_cmd::CLOSE_TAB => Self::CloseTab,
            menu_cmd::SAVE => Self::Save,
            menu_cmd::SETTINGS => Self::Settings,
            menu_cmd::UNDO => Self::Undo,
            menu_cmd::REDO => Self::Redo,
            menu_cmd::CUT => Self::Cut,
            menu_cmd::COPY => Self::Copy,
            menu_cmd::PASTE => Self::Paste,
            menu_cmd::SELECT_ALL => Self::SelectAll,
            menu_cmd::FIND => Self::Find,
            menu_cmd::FIND_NEXT => Self::FindNext,
            menu_cmd::COMMENT => Self::Comment,
            menu_cmd::RUN_BUFFER => Self::RunBuffer,
            menu_cmd::EVAL_FORM => Self::EvalForm,
            menu_cmd::CLEAR_REPL => Self::ClearRepl,
            menu_cmd::FOCUS_EDITOR => Self::FocusEditor,
            menu_cmd::FOCUS_REPL => Self::FocusRepl,
            menu_cmd::FONT_UP => Self::FontUp,
            menu_cmd::FONT_DOWN => Self::FontDown,
            menu_cmd::HELP => Self::Help,
            _ => return None,
        })
    }

    /// The editor/REPL accelerator equivalent for editing commands: menu
    /// picks must behave exactly like pressing the key. Returned as a
    /// (vkey, mods) pair fed to `on_key`, which routes by focus.
    fn accelerator(self) -> Option<(i64, i64)> {
        let w = modifier::WIN;
        let s = modifier::SHIFT;
        Some(match self {
            Self::Undo => (0x5A, w),          // Cmd-Z
            Self::Redo => (0x5A, w | s),      // Cmd-Shift-Z
            Self::Cut => (0x58, w),           // Cmd-X
            Self::Copy => (0x43, w),          // Cmd-C
            Self::Paste => (0x56, w),         // Cmd-V
            Self::SelectAll => (0x41, w),     // Cmd-A
            Self::Find => (0x46, w),          // Cmd-F
            Self::FindNext => (0x47, w),      // Cmd-G
            Self::Comment => (vk::OEM_2, w),  // Cmd-/
            _ => return None,
        })
    }
}

pub struct Ide {
    buffers: Vec<Editor>,
    active: usize,
    repl: Repl,
    focus: Focus,
    theme: Theme,
    /// The semantic tokens the chrome is painted from (see `igui_mac::theme`).
    sys: SystemTheme,
    /// Whether the IDE window is the key window — inactive windows dim
    /// their labels and hide the caret, like native Mac apps.
    window_active: bool,
    /// Code font size in points (⌘+/⌘−, clamped 12–20). Survives
    /// re-theming (appearance changes keep the user's zoom).
    font_size: f32,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    /// Fraction of the height given to the editor (top). REPL gets the rest.
    split: f32,
    /// True while the user is dragging the editor/REPL divider.
    dragging_split: bool,
    width: f32,
    height: f32,
}

/// How close (px) to the divider a click must land to start a drag.
const SPLIT_GRAB: f32 = 5.0;
/// Clamp the split so neither pane can be dragged shut.
const SPLIT_MIN: f32 = 0.12;
const SPLIT_MAX: f32 = 0.90;
/// Left inset of the tab strip: the traffic lights float over the strip
/// (FullSizeContentView + transparent title bar), so the first tab must
/// start clear of them. ~78pt covers the three buttons at standard width.
pub const TRAFFIC_INSET: f32 = 78.0;
/// Width of the `+` (new buffer) button that follows the last tab.
const PLUS_W: f32 = 28.0;

impl Ide {
    pub fn new(sys: SystemTheme) -> Self {
        let theme = sys.to_editor_theme();
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
            sys,
            window_active: true,
            font_size: 15.0,
            split: 0.62,
            dragging_split: false,
            width: 900.0,
            height: 620.0,
        }
    }

    /// Re-theme from a new snapshot (appearance/accent change). Carries the
    /// measured cell metrics and the user's font size across — only
    /// colours move.
    pub fn set_theme(&mut self, sys: SystemTheme) {
        self.theme = sys.to_editor_theme();
        self.theme.size = self.font_size;
        self.theme.cell_w = self.cell_w;
        self.theme.cell_h = self.cell_h;
        self.theme.ascent = self.ascent;
        self.sys = sys;
        let t = self.theme.clone();
        for b in &mut self.buffers {
            b.set_theme(t.clone());
            b.set_metrics(self.cell_w, self.cell_h, self.ascent);
        }
        self.repl.set_theme(t);
    }

    /// Step the code font size (⌘+/⌘−), clamped to 12–20pt. Returns
    /// `Remeasure` when it moved so the driver re-measures cell metrics.
    fn step_font_size(&mut self, d: f32) -> IdeAction {
        let new = (self.font_size + d).clamp(12.0, 20.0);
        if (new - self.font_size).abs() < 0.01 {
            return IdeAction::None;
        }
        self.font_size = new;
        self.theme.size = new;
        let t = self.theme.clone();
        for b in &mut self.buffers {
            b.set_theme(t.clone());
            b.set_metrics(self.cell_w, self.cell_h, self.ascent);
        }
        self.repl.set_theme(t);
        IdeAction::Remeasure
    }

    /// The code font family/size the driver should (re-)measure.
    pub fn font_family(&self) -> &str {
        &self.theme.family
    }
    pub fn font_size(&self) -> f32 {
        self.theme.size
    }

    /// Set whether the IDE window is the key window. Inactive windows dim
    /// labels one tier and hide carets (native Mac behaviour).
    pub fn set_active(&mut self, active: bool) {
        self.window_active = active;
        let show = active;
        for b in &mut self.buffers {
            b.set_show_caret(show);
        }
        self.repl.set_show_caret(show);
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
    /// scratch buffer; otherwise open a new tab. Records the file in the
    /// Open Recent menu.
    pub fn load_file(&mut self, path: &str) {
        menu::record_recent(path);
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
    /// Top of the editor stack: below the tab bar (menus live in the
    /// system menu bar, not in-window).
    fn header_h(&self) -> f32 {
        self.tab_h()
    }
    fn editor_area(&self) -> Rect {
        let div = (self.height * self.split).round();
        Rect { x0: 0.0, y0: self.header_h(), x1: self.width, y1: div - self.status_h() }
    }
    fn tab_area(&self) -> Rect {
        Rect { x0: 0.0, y0: 0.0, x1: self.width, y1: self.header_h() }
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
        ((self.width - TRAFFIC_INSET) / self.buffers.len() as f32).min(200.0).max(60.0)
    }

    /// Rect of the `+` (new buffer) button that follows the last tab.
    fn plus_rect(&self) -> Rect {
        let x0 = TRAFFIC_INSET + self.buffers.len() as f32 * self.tab_width() + 4.0;
        Rect { x0, y0: 0.0, x1: x0 + PLUS_W, y1: self.tab_h() }
    }

    /// What the NSWindow title should show: the active buffer's file name,
    /// or "untitled". (The visible identity is the active tab; this feeds
    /// the Window menu / Mission Control / ⌘-switcher.)
    pub fn window_title(&self) -> String {
        self.ed()
            .file_path()
            .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
            .unwrap_or_else(|| "untitled".into())
    }

    /// What the NSWindow subtitle should show: the active buffer's
    /// directory, or "" for untitled buffers.
    pub fn window_subtitle(&self) -> String {
        self.ed()
            .file_path()
            .and_then(|p| {
                let dir = p.rsplit_once('/').map(|(d, _)| d)?;
                (!dir.is_empty()).then(|| dir.to_string())
            })
            .unwrap_or_default()
    }

    /// Whether the menu's Save item should be enabled (active buffer dirty).
    pub fn can_save(&self) -> bool {
        self.ed().is_dirty()
    }

    fn run_menu_cmd(&mut self, cmd: MenuCmd) -> IdeAction {
        // Editing commands behave exactly like their keyboard equivalent:
        // run the accelerator through on_key, which routes by focus.
        if let Some((vkey, mods)) = cmd.accelerator() {
            return self.on_key(vkey, mods);
        }
        match cmd {
            MenuCmd::New => {
                self.new_buffer();
                IdeAction::None
            }
            MenuCmd::CloseTab => {
                self.close_buffer();
                IdeAction::None
            }
            MenuCmd::Save => {
                self.save_buffer();
                IdeAction::None
            }
            MenuCmd::Settings => {
                self.repl.info("; no settings yet — watch this space");
                IdeAction::None
            }
            MenuCmd::RunBuffer => self.run_buffer(),
            MenuCmd::EvalForm => {
                self.focus = Focus::Editor;
                self.eval_form_at_point()
            }
            MenuCmd::ClearRepl => {
                self.clear_repl();
                IdeAction::None
            }
            MenuCmd::FocusEditor => {
                self.focus = Focus::Editor;
                IdeAction::None
            }
            MenuCmd::FocusRepl => {
                self.focus = Focus::Repl;
                IdeAction::None
            }
            MenuCmd::FontUp => self.step_font_size(1.0),
            MenuCmd::FontDown => self.step_font_size(-1.0),
            MenuCmd::Help => {
                self.show_shortcuts();
                IdeAction::None
            }
            // Handled by the accelerator fast path above.
            MenuCmd::Undo
            | MenuCmd::Redo
            | MenuCmd::Cut
            | MenuCmd::Copy
            | MenuCmd::Paste
            | MenuCmd::SelectAll
            | MenuCmd::Find
            | MenuCmd::FindNext
            | MenuCmd::Comment => unreachable!("editing commands have accelerators"),
        }
    }

    // ─── command actions (shared by keyboard shortcuts and the menu) ───

    fn run_buffer(&mut self) -> IdeAction {
        let src = self.ed().text();
        if !src.trim().is_empty() {
            self.repl.info("; run buffer");
            return IdeAction::Eval(src);
        }
        IdeAction::None
    }

    fn eval_form_at_point(&mut self) -> IdeAction {
        // Eval the selection if there is one, else the form at point.
        let sel = self.ed().selected_text();
        let form = if !sel.trim().is_empty() {
            Some(sel)
        } else {
            self.ed().current_form()
        };
        if let Some(form) = form {
            self.repl.info(&format!("; {}", form.replace('\n', " ")));
            return IdeAction::Eval(form);
        }
        IdeAction::None
    }

    fn save_buffer(&mut self) {
        match self.ed_mut().save() {
            Ok(Some(p)) => self.repl.info(&format!("; saved {p}")),
            Ok(None) => self
                .repl
                .info("; no file — launch with `ncl --windows <file.lisp>` to set one"),
            Err(e) => self.repl.error(&format!("save failed: {e}")),
        }
    }

    fn clear_repl(&mut self) {
        self.repl.clear();
    }

    fn show_shortcuts(&mut self) {
        for line in [
            "Keyboard shortcuts:",
            "  Cmd-R  run buffer          Cmd-Return  eval form at point",
            "  Cmd-T  new tab             Cmd-W       close tab",
            "  Cmd-1..9  switch tab       Cmd-S       save buffer",
            "  Cmd-E  focus editor        Cmd-L       focus REPL",
            "  Cmd-K  clear REPL          drag the divider to resize panes",
        ] {
            self.repl.info(line);
        }
    }

    // ─── events ───────────────────────────────────────────────────────

    /// Route an event to the focused pane, handling IDE-global keys.
    /// Returns an action for the driver (an eval request) or `None`.
    pub fn handle_event(&mut self, ev: &IGuiEvent) -> IdeAction {
        match ev {
            // System-menu-bar picks (mouse clicks on items, swallowed key
            // equivalents, NCL_GUI_MENU injection) — same dispatcher the
            // keyboard shortcuts use.
            IGuiEvent::Menu { menu_id, item_id } if *menu_id == menu_cmd::IDE => {
                match MenuCmd::from_opcode(*item_id) {
                    Some(cmd) => self.run_menu_cmd(cmd),
                    None => IdeAction::None,
                }
            }
            // Open a file: from the open panel, a recents pick, or a
            // Finder drop onto the window.
            IGuiEvent::Open { path } => {
                self.load_file(path);
                IdeAction::None
            }
            // Save As…: write the buffer to the chosen path and adopt it.
            IGuiEvent::SaveAs { path } => {
                match self.ed_mut().save_to(path) {
                    Ok(()) => {
                        self.focus = Focus::Editor;
                        menu::record_recent(path);
                        self.repl.info(&format!("; saved {path}"));
                    }
                    Err(e) => self.repl.error(&format!("save as {path}: {e}")),
                }
                IdeAction::None
            }
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
                let up = *op == mouse_op::LEFT_UP;

                // ── splitter drag ──
                // Grab the editor/REPL divider on a down within SPLIT_GRAB px,
                // track it through DRAG, release on UP. While dragging, the
                // event never reaches a pane.
                let div = (self.height * self.split).round();
                if up {
                    self.dragging_split = false;
                    return IdeAction::None;
                }
                if down && (my - div).abs() <= SPLIT_GRAB && my > self.header_h() {
                    self.dragging_split = true;
                    return IdeAction::None;
                }
                if self.dragging_split {
                    if drag && self.height > 0.0 {
                        self.split = (my / self.height).clamp(SPLIT_MIN, SPLIT_MAX);
                    }
                    return IdeAction::None;
                }

                // Tab-bar clicks: the `+` button opens a buffer; a tab
                // switches to it. (Tabs start right of the traffic lights.)
                if down && my < self.header_h() {
                    let pr = self.plus_rect();
                    if mx >= pr.x0 && mx < pr.x1 {
                        self.new_buffer();
                        return IdeAction::None;
                    }
                    if mx >= TRAFFIC_INSET {
                        let i = ((mx - TRAFFIC_INSET) / self.tab_width()) as usize;
                        self.switch_to(i);
                    }
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
                0x52 => return self.run_buffer(), // Cmd-R run buffer
                0x4B if mods & modifier::SHIFT == 0 => {
                    self.clear_repl();
                    return IdeAction::None;
                } // Cmd-K clear REPL (Cmd-Shift-K falls through to editor delete-line)
                0x53 => {
                    self.save_buffer();
                    return IdeAction::None;
                } // Cmd-S
                _ => {}
            }
        }
        match self.focus {
            Focus::Editor => {
                if cmd && vkey == vk::RETURN {
                    return self.eval_form_at_point();
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

    /// A label tier, dimmed one step while the window is inactive (native
    /// Mac windows drop label opacity in the background).
    fn tier(&self, primary: Rgba, dimmed: Rgba) -> Rgba {
        if self.window_active {
            primary
        } else {
            dimmed
        }
    }

    /// A chrome text run: SF Pro (`__system`) at an explicit size — 13pt
    /// for tab labels, 11pt for the status bar (the design's type scale;
    /// ⌘+/⌘− zoom code, not chrome).
    fn run(&self, text: String, x: f32, y: f32, color: Rgba, size: f32) -> SurfaceCmd {
        SurfaceCmd::DrawTextRun {
            run: TextRun {
                text,
                origin: Point { x, y },
                family: "__system".into(),
                size,
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

        // ── tab bar (menus live in the system menu bar) ──
        let ta = self.tab_area();
        let s = &self.sys;
        cmds.push(SurfaceCmd::FillRect {
            rect: ta,
            corner_radius: 0.0,
            color: s.chrome_bg,
        });
        let tw = self.tab_width();
        for (i, b) in self.buffers.iter().enumerate() {
            let x0 = TRAFFIC_INSET + i as f32 * tw;
            let active = i == self.active;
            cmds.push(SurfaceCmd::FillRect {
                rect: Rect { x0: x0 + 1.0, y0: ta.y0 + 2.0, x1: x0 + tw - 1.0, y1: ta.y1 },
                corner_radius: 4.0,
                color: if active { s.raised_bg } else { s.sunken_bg },
            });
            let col = if active {
                self.tier(s.text, s.text_secondary)
            } else {
                self.tier(s.text_secondary, s.text_tertiary)
            };
            cmds.push(self.run(Self::tab_label(b), x0 + 8.0, ta.y0 + 4.0, col, 13.0));
        }
        // `+` button after the last tab — ⌘N/⌘T discoverability.
        let pr = self.plus_rect();
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: pr.x0, y0: ta.y0 + 2.0, x1: pr.x1, y1: ta.y1 },
            corner_radius: 4.0,
            color: s.sunken_bg,
        });
        cmds.push(
            self.run(
                "+".into(),
                pr.x0 + 9.0,
                ta.y0 + 4.0,
                self.tier(s.text_secondary, s.text_tertiary),
                13.0,
            ),
        );

        // ── editor pane ──
        for c in self.ed_mut().render(ea) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }

        // ── status bar ──
        let s = &self.sys;
        let sa = self.status_area();
        cmds.push(SurfaceCmd::FillRect {
            rect: sa,
            corner_radius: 0.0,
            color: s.status_bg,
        });
        let status = self.ed().status();
        cmds.push(self.run(
            status,
            sa.x0 + 8.0,
            sa.y0 + 2.0,
            self.tier(s.text_secondary, s.text_tertiary),
            11.0,
        ));

        // ── REPL pane ──
        for c in self.repl.render(ra) {
            if matches!(c, SurfaceCmd::Clear { .. }) {
                continue;
            }
            cmds.push(c);
        }

        // ── draggable divider ──
        // Doubles as the focus indicator (lit on the focused side). While the
        // user is dragging it, the whole bar lights up to signal it's live,
        // and a short grab handle is drawn centred so it's discoverable.
        // Inactive windows lose the accent glow entirely.
        let _ = div_y;
        let s = &self.sys;
        let dim = s.separator;
        let glow = if self.window_active { s.accent } else { s.separator };
        let (etop, ebot) = if self.dragging_split {
            (glow, glow)
        } else {
            match self.focus {
                Focus::Editor => (glow, dim),
                Focus::Repl => (dim, glow),
            }
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
        // Centred grab handle — a short, brighter pill so the divider reads
        // as draggable rather than a plain rule.
        let hw = 18.0_f32.min(self.width * 0.25);
        let cx = (area.x0 + area.x1) * 0.5;
        cmds.push(SurfaceCmd::FillRect {
            rect: Rect { x0: cx - hw, y0: div_real - 2.0, x1: cx + hw, y1: div_real + 2.0 },
            corner_radius: 2.0,
            color: glow,
        });
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vkey: i64, mods: i64) -> IGuiEvent {
        IGuiEvent::Key { child_id: 1, vkey, scancode: 0, mods, repeat: 0, down: true, time_ms: 0 }
    }

    fn mouse(op: i64, x: f32, y: f32) -> IGuiEvent {
        IGuiEvent::Mouse {
            child_id: 1, x: x as i64, y: y as i64, op, button: 0,
            mods: 0, wheel_delta: 0, wheel_lines: 0, time_ms: 0,
        }
    }

    #[test]
    fn dragging_the_divider_moves_the_split() {
        use crate::igui_events::mouse_op;
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        // A render establishes width/height (900×620).
        ide.render(Rect { x0: 0.0, y0: 0.0, x1: 900.0, y1: 620.0 });
        let div = (ide.height * ide.split).round();

        // Grab the divider, drag it up to ~30% of the height, release.
        ide.handle_event(&mouse(mouse_op::LEFT_DOWN, 450.0, div));
        assert!(ide.dragging_split, "down on the divider should start a drag");
        ide.handle_event(&mouse(mouse_op::DRAG, 450.0, 0.30 * 620.0));
        ide.handle_event(&mouse(mouse_op::LEFT_UP, 450.0, 0.30 * 620.0));
        assert!(!ide.dragging_split, "up should end the drag");
        assert!((ide.split - 0.30).abs() < 0.02, "split should track the cursor, got {}", ide.split);

        // A click far from the divider must NOT start a drag.
        ide.handle_event(&mouse(mouse_op::LEFT_DOWN, 450.0, 50.0));
        assert!(!ide.dragging_split);
    }

    /// A system-menu-bar pick, as it arrives from the dispatcher / the
    /// key-equivalent swallow / NCL_GUI_MENU.
    fn menu(item_id: i64) -> IGuiEvent {
        IGuiEvent::Menu { menu_id: menu_cmd::IDE, item_id }
    }

    /// Sprint 1 AC: a Menu Run-Buffer pick behaves exactly like the ⌘R
    /// shortcut — same `IdeAction::Eval`, same source.
    #[test]
    fn menu_run_buffer_matches_the_shortcut() {
        let mut ide = Ide::new(fixed_dark());
        ide.focus = Focus::Editor;
        let via_key = ide.on_key(0x52, modifier::WIN); // Cmd-R
        let via_menu = ide.handle_event(&menu(menu_cmd::RUN_BUFFER));
        match (via_key, via_menu) {
            (IdeAction::Eval(k), IdeAction::Eval(m)) => assert_eq!(k, m),
            _ => panic!("both paths should request an eval"),
        }
    }

    /// Sprint 1 AC: every opcode the menu registry can emit routes to a
    /// known command and produces no action (or an eval) — never a panic.
    #[test]
    fn every_menu_opcode_routes() {
        for op in [
            menu_cmd::NEW, menu_cmd::CLOSE_TAB, menu_cmd::SAVE, menu_cmd::SETTINGS,
            menu_cmd::UNDO, menu_cmd::REDO, menu_cmd::CUT, menu_cmd::COPY,
            menu_cmd::PASTE, menu_cmd::SELECT_ALL, menu_cmd::FIND, menu_cmd::FIND_NEXT,
            menu_cmd::COMMENT, menu_cmd::RUN_BUFFER, menu_cmd::EVAL_FORM,
            menu_cmd::CLEAR_REPL, menu_cmd::FOCUS_EDITOR, menu_cmd::FOCUS_REPL,
            menu_cmd::HELP, menu_cmd::FONT_UP, menu_cmd::FONT_DOWN,
        ] {
            let mut ide = Ide::new(fixed_dark());
            let _ = ide.handle_event(&menu(op));
        }
        // Unknown ids no-op instead of misrouting.
        let mut ide = Ide::new(fixed_dark());
        assert!(matches!(ide.handle_event(&menu(9999)), IdeAction::None));
        // Foreign menus (not menu_cmd::IDE) are ignored.
        let foreign = IGuiEvent::Menu { menu_id: 42, item_id: menu_cmd::SAVE };
        assert!(matches!(ide.handle_event(&foreign), IdeAction::None));
    }

    #[test]
    fn menu_new_opens_a_buffer() {
        let mut ide = Ide::new(fixed_dark());
        let before = ide.buffers.len();
        ide.handle_event(&menu(menu_cmd::NEW));
        assert_eq!(ide.buffers.len(), before + 1);
    }

    /// Menu Edit commands run the focused pane's accelerator: inserting a
    /// char and then picking Undo must leave the buffer as it started.
    #[test]
    fn menu_undo_runs_the_editor_accelerator() {
        let mut ide = Ide::new(fixed_dark());
        ide.focus = Focus::Editor;
        let before = ide.ed().text();
        ide.ed_mut().on_char('Z' as u32);
        assert_ne!(ide.ed().text(), before, "sanity: the char was inserted");
        ide.handle_event(&menu(menu_cmd::UNDO));
        assert_eq!(ide.ed().text(), before, "menu Undo should undo the insert");
    }

    #[test]
    fn split_is_clamped_so_panes_never_vanish() {
        use crate::igui_events::mouse_op;
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        ide.render(Rect { x0: 0.0, y0: 0.0, x1: 900.0, y1: 620.0 });
        let div = (ide.height * ide.split).round();
        ide.handle_event(&mouse(mouse_op::LEFT_DOWN, 450.0, div));
        // Drag way past the bottom — split clamps, REPL stays visible.
        ide.handle_event(&mouse(mouse_op::DRAG, 450.0, 5000.0));
        assert!(ide.split <= SPLIT_MAX + 0.001 && ide.split >= SPLIT_MIN);
    }

    #[test]
    fn cmd_r_runs_the_buffer() {
        let mut ide = Ide::new(fixed_dark());
        ide.focus = Focus::Editor;
        match ide.on_key(0x52, modifier::WIN) {
            IdeAction::Eval(src) => assert!(src.contains("defun square")),
            _ => panic!("Cmd-R should request an eval"),
        }
    }

    #[test]
    fn repl_return_evaluates() {
        let mut ide = Ide::new(fixed_dark());
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
        let mut ide = Ide::new(fixed_dark());
        ide.focus = Focus::Editor;
        match ide.on_key(vk::RETURN, modifier::WIN) {
            IdeAction::Eval(src) => assert!(src.starts_with('(') && src.contains("defun")),
            _ => panic!("Cmd-Return should eval the form at point"),
        }
    }

    #[test]
    fn new_close_and_switch_buffers() {
        let mut ide = Ide::new(fixed_dark());
        assert_eq!(ide.buffers.len(), 1);
        ide.on_key(0x54, modifier::WIN); // Cmd-T
        assert_eq!(ide.buffers.len(), 2);
        assert_eq!(ide.active, 1);
        ide.on_key(0x31, modifier::WIN); // Cmd-1 → first tab
        assert_eq!(ide.active, 0);
        ide.on_key(0x57, modifier::WIN); // Cmd-W close active
        assert_eq!(ide.buffers.len(), 1);
    }

    // ── Sprint 2: window polish ─────────────────────────────────────────

    /// The first tab and the `+` button must start clear of the traffic
    /// lights, which float over the strip's left end.
    #[test]
    fn tab_strip_clears_traffic_lights() {
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        ide.on_key(0x54, modifier::WIN); // a second buffer → two chips + plus
        let cmds = ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });

        // Every tab chip / `+` chip lives in the strip (y within tab_h) and
        // starts at or right of the inset. Collect the chip rects from the
        // rendered FillRects in the strip band.
        let strip_bottom = ide.tab_h();
        let mut chips = Vec::new();
        for c in &cmds {
            if let SurfaceCmd::FillRect { rect, .. } = c {
                if rect.y0 < strip_bottom && rect.y1 <= strip_bottom && rect.x1 - rect.x0 < 300.0
                {
                    chips.push((rect.x0, rect.x1));
                }
            }
        }
        assert!(chips.len() >= 3, "expected two tabs + plus chip, got {chips:?}");
        for (x0, x1) in &chips {
            assert!(
                *x0 >= TRAFFIC_INSET,
                "chip at x0={x0} overlaps the traffic lights (< {TRAFFIC_INSET})"
            );
            assert!(*x1 <= 1000.0);
        }
        // The strip background itself spans the full width (drawn under the
        // lights so the chrome looks continuous).
        assert!(cmds.iter().any(|c| matches!(
            c,
            SurfaceCmd::FillRect { rect, .. }
                if rect.y0 == 0.0 && rect.x0 == 0.0 && rect.x1 == 1000.0
        )));
    }

    /// Clicking the `+` chip opens a buffer; clicks left of the inset (where
    /// the traffic lights are) never hit a tab.
    #[test]
    fn plus_button_opens_a_buffer() {
        use crate::igui_events::mouse_op;
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        let pr = ide.plus_rect();
        let before = ide.buffers.len();
        ide.handle_event(&mouse(mouse_op::LEFT_DOWN, (pr.x0 + pr.x1) / 2.0, 5.0));
        assert_eq!(ide.buffers.len(), before + 1, "+ click should open a buffer");
        // A click in the traffic-light zone does nothing pane-visible.
        ide.handle_event(&mouse(mouse_op::LEFT_DOWN, 20.0, 5.0));
        assert_eq!(ide.buffers.len(), before + 1);
    }

    /// The window title/subtitle track the active buffer: filename and
    /// directory once a file is loaded, "untitled"/"" before.
    #[test]
    fn title_tracks_active_buffer() {
        let mut ide = Ide::new(fixed_dark());
        assert_eq!(ide.window_title(), "untitled");
        assert_eq!(ide.window_subtitle(), "");

        // Load a file into the scratch buffer (untitled + clean → reused).
        let dir = std::env::temp_dir().join("ncl_title_test.lisp");
        std::fs::write(&dir, "(+ 1 2)\n").unwrap();
        ide.load_file(dir.to_str().unwrap());
        assert_eq!(ide.window_title(), "ncl_title_test.lisp");
        assert_eq!(
            ide.window_subtitle(),
            dir.parent().unwrap().to_str().unwrap(),
            "subtitle should be the file's directory"
        );

        // A second (new) buffer takes over the identity; switching back
        // restores it.
        ide.on_key(0x54, modifier::WIN); // Cmd-T
        assert_eq!(ide.window_title(), "untitled");
        ide.on_key(0x31, modifier::WIN); // Cmd-1 → file buffer
        assert_eq!(ide.window_title(), "ncl_title_test.lisp");
        let _ = std::fs::remove_file(&dir);
    }

    /// Full-size content means the batch starts at the very top of the
    /// window: the strip's background rect must begin at y = 0 (no
    /// title-bar gap above the IDE's chrome).
    #[test]
    fn tab_strip_starts_at_window_top() {
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        let cmds = ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        let strip = cmds.iter().find_map(|c| match c {
            SurfaceCmd::FillRect { rect, .. }
                if rect.x0 == 0.0 && rect.x1 == 1000.0 && rect.y1 == ide.tab_h() =>
            {
                Some(*rect)
            }
            _ => None,
        });
        let rect = strip.expect("tab strip background rect");
        assert_eq!(rect.y0, 0.0, "strip must start at the window top edge");
    }

    // ── Sprint 3: system theme ─────────────────────────────────────────

    /// A forced light vs dark snapshot must change the painted chrome —
    /// sample the tab strip and the status bar in rendered pixels.
    #[test]
    fn forced_theme_changes_frame() {
        use crate::igui_mac::render::CgCanvas;
        let paint = |sys: SystemTheme| -> (Vec<u8>, Vec<u8>) {
            let mut ide = Ide::new(sys);
            ide.set_metrics(8.0, 16.0, 12.0);
            let cmds = ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
            let mut canvas = CgCanvas::new(1000, 680);
            canvas.execute(&cmds);
            let strip = canvas.pixel(500, 5);
            let status = canvas.pixel(500, (ide.status_area().y0 as usize
                + ide.status_area().y1 as usize)
                / 2);
            (strip.to_vec(), status.to_vec())
        };
        let (dark_strip, dark_status) = paint(fixed_dark());
        let (light_strip, light_status) = paint(fixed_light());
        assert_ne!(dark_strip, light_strip, "tab strip must follow appearance");
        assert_ne!(dark_status, light_status, "status bar must follow appearance");
    }

    /// Inactive window: labels drop one tier and every accent-coloured
    /// element (caret, divider glow, grab handle) disappears from the batch.
    #[test]
    fn inactive_window_dims_labels_and_hides_accent() {
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        let sys = ide.sys.clone();
        ide.set_active(false);

        // Active-tab label colour steps down one tier.
        let cmds = ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        let tab_label = cmds.iter().find_map(|c| match c {
            SurfaceCmd::DrawTextRun { run } if run.text == "untitled" => Some(run.color),
            _ => None,
        });
        assert_eq!(tab_label, Some(sys.text_secondary), "active tab dims one tier");

        // No accent anywhere: caret, divider glow, grab handle.
        for c in &cmds {
            let color = match c {
                SurfaceCmd::FillRect { color, .. }
                | SurfaceCmd::StrokeRect { color, .. }
                | SurfaceCmd::SelectionRange { color, .. }
                | SurfaceCmd::Caret { color, .. } => *color,
                _ => continue,
            };
            assert_ne!(
                color, sys.accent,
                "accent-coloured element must vanish while inactive: {c:?}"
            );
        }
    }

    /// Text selection follows the accent at the snapshot's alpha — build
    /// with two accents and the rendered selection tracks each.
    #[test]
    fn ide_selection_follows_accent() {
        let mut a = fixed_dark();
        a.accent = crate::igui_paint::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
        a.selection = crate::igui_paint::Rgba { r: 1.0, g: 0.0, b: 0.0, a: 0.28 };
        let mut b = fixed_dark();
        b.accent = crate::igui_paint::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
        b.selection = crate::igui_paint::Rgba { r: 0.0, g: 1.0, b: 0.0, a: 0.28 };

        let sel = |sys: SystemTheme| -> Vec<crate::igui_paint::Rgba> {
            let mut ide = Ide::new(sys);
            ide.set_metrics(8.0, 16.0, 12.0);
            ide.focus = Focus::Editor;
            ide.on_key(0x41, modifier::WIN); // Cmd-A: select all
            ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 })
                .into_iter()
                .filter_map(|c| match c {
                    SurfaceCmd::SelectionRange { color, .. } => Some(color),
                    _ => None,
                })
                .collect()
        };
        let red = sel(a);
        let green = sel(b);
        assert!(!red.is_empty(), "selection rects should render");
        assert!(red.iter().all(|c| c.r > 0.9 && c.g < 0.1), "red accent: {red:?}");
        assert!(green.iter().all(|c| c.g > 0.9 && c.r < 0.1), "green accent: {green:?}");
    }

    /// Chrome colours come from the token snapshot — no ad-hoc color
    /// literals may appear in this file's render path (tests aside).
    /// Matches struct literals (`Rgba { r: …`), not `-> Rgba {` signatures.
    #[test]
    fn no_chrome_rgba_literals() {
        let src = include_str!("app.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            !code.contains("Rgba { r") && !code.contains("rgb("),
            "hardcoded chrome color crept back into app.rs"
        );
    }

    // ── Sprint 4: typography ───────────────────────────────────────────

    /// ⌘+ eleven times from 15pt clamps at 20; ⌘− underflows to 12; a
    /// step that can't move returns None (nothing to re-measure).
    #[test]
    fn font_size_commands_clamp() {
        let mut ide = Ide::new(fixed_dark());
        assert_eq!(ide.font_size(), 15.0);
        for _ in 0..5 {
            match ide.handle_event(&menu(menu_cmd::FONT_UP)) {
                IdeAction::Remeasure => {}
                other => panic!("font-up should request a remeasure, got {other:?}"),
            }
        }
        assert_eq!(ide.font_size(), 20.0, "upper clamp");
        assert!(matches!(ide.handle_event(&menu(menu_cmd::FONT_UP)), IdeAction::None));

        for _ in 0..15 {
            ide.handle_event(&menu(menu_cmd::FONT_DOWN));
        }
        assert_eq!(ide.font_size(), 12.0, "lower clamp");
        assert!(matches!(ide.handle_event(&menu(menu_cmd::FONT_DOWN)), IdeAction::None));

        // The zoom survives a re-theme (appearance change).
        ide.set_theme(fixed_light());
        assert_eq!(ide.font_size(), 12.0, "font size survives re-theme");
    }

    /// Chrome text uses the system font at the design's type scale
    /// (13pt tabs, 11pt status); code text uses the mono family.
    #[test]
    fn chrome_uses_system_font_and_type_scale() {
        let mut ide = Ide::new(fixed_dark());
        ide.set_metrics(8.0, 16.0, 12.0);
        let cmds = ide.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        let mut chrome_sizes = Vec::new();
        let mut code_families = Vec::new();
        for c in &cmds {
            if let SurfaceCmd::DrawTextRun { run } = c {
                match run.family.as_str() {
                    "__system" => chrome_sizes.push(run.size),
                    f => code_families.push(f.to_string()),
                }
            }
        }
        assert!(!chrome_sizes.is_empty());
        assert!(
            chrome_sizes.iter().all(|s| (s - 13.0).abs() < 0.01 || (s - 11.0).abs() < 0.01),
            "chrome sizes must be 13/11pt, got {chrome_sizes:?}"
        );
        assert!(
            code_families.iter().all(|f| f == "__mono"),
            "code runs must use __mono, got {code_families:?}"
        );
    }

    /// Rendering is deterministic for a fixed state — guards the Menlo
    /// fallback metric baseline (two renders must agree byte-for-byte at
    /// the command level).
    #[test]
    fn render_is_deterministic_for_fixed_state() {
        let mut a = Ide::new(fixed_dark());
        a.set_metrics(8.0, 16.0, 12.0);
        let mut b = Ide::new(fixed_dark());
        b.set_metrics(8.0, 16.0, 12.0);
        let ca = a.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        let cb = b.render(Rect { x0: 0.0, y0: 0.0, x1: 1000.0, y1: 680.0 });
        assert_eq!(format!("{ca:?}"), format!("{cb:?}"));
    }

    // ── Sprint 5: native files ─────────────────────────────────────────

    fn isolate_recents() {
        let p = std::env::temp_dir()
            .join(format!("ncl_recents_ide_test_{}.json", std::process::id()));
        // SAFETY: test-only env mutation; suite is single-threaded here.
        unsafe { std::env::set_var("NCL_RECENTS_FILE", &p) };
    }

    /// An Open event (panel pick / recents / Finder drop) loads the file:
    /// tab retitled, `; loaded` notice, recents recorded.
    #[test]
    fn open_event_loads_file() {
        isolate_recents();
        let dir = std::env::temp_dir().join("ncl_open_event.lisp");
        std::fs::write(&dir, "(* 6 7)\n").unwrap();
        let mut ide = Ide::new(fixed_dark());
        ide.handle_event(&IGuiEvent::Open { path: dir.to_str().unwrap().into() });
        assert_eq!(ide.window_title(), "ncl_open_event.lisp");
        assert!(matches!(ide.focus, Focus::Editor), "open focuses the editor");
        assert!(menu::recent_paths().iter().any(|p| p.ends_with("ncl_open_event.lisp")));
        let _ = std::fs::remove_file(&dir);
    }

    /// A SaveAs event writes the buffer, clears dirty, adopts the name.
    #[test]
    fn save_as_writes_file_and_retitles() {
        isolate_recents();
        let path = std::env::temp_dir().join("ncl_save_as.lisp");
        let path = path.to_str().unwrap().to_string();
        let _ = std::fs::remove_file(&path);
        let mut ide = Ide::new(fixed_dark());
        ide.focus = Focus::Editor;
        ide.ed_mut().set_text("");
        for c in "(save-me 42)".chars() {
            ide.ed_mut().on_char(c as u32);
        }
        assert!(ide.can_save(), "sanity: buffer dirty before save");
        ide.handle_event(&IGuiEvent::SaveAs { path: path.clone() });
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written.trim(), "(save-me 42)");
        assert!(!ide.can_save(), "save-as clears dirty");
        assert_eq!(ide.window_title(), "ncl_save_as.lisp");
        assert_eq!(
            ide.window_subtitle().trim_end_matches('/'),
            std::env::temp_dir().to_str().unwrap().trim_end_matches('/'),
            "subtitle should be the file's directory"
        );
        let _ = std::fs::remove_file(&path);
    }
}
