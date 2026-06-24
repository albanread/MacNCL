//! Frame-level menus and keyboard accelerator table.
//!
//! Menu structure (default bar, before any language-thread spec):
//!
//!   File  — New, Open, Save, Save As, ─, Exit
//!   Edit  — Undo, Redo, ─, Cut, Copy, Paste, Select All
//!   Lisp  — Run Buffer (F5), Run Form at Point (Ctrl+Enter), ─,
//!            Forward S-expr, Backward S-expr, ─,
//!            Slurp Forward, Barf Forward, Wrap, Splice, Raise
//!   Demos — one entry per *.lisp discovered in Lisp/demos/
//!   Tools — ledit (Ctrl+Shift+E), Log (Ctrl+Shift+L), REPL (Ctrl+Shift+R)
//!
//! When the language thread installs its own menu bar via
//! `iGui.SetMenu`, the Lisp and Tools menus are re-appended to
//! whatever the language spec provided (so Ctrl+Shift+E is always
//! available regardless of the Lisp code's menu spec).

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateAcceleratorTableW, CreateMenu, CreatePopupMenu, ACCEL,
    FCONTROL, FSHIFT, FVIRTKEY, HACCEL, HMENU, MF_POPUP, MF_SEPARATOR, MF_STRING,
};

use super::crash_view;
use super::ledit;
use super::log_view;
use super::repl_child;

/// Help → Documentation: open the bundled `docs/user/index.md` in an
/// in-window doc-pane MDI child (routed via `window::open_docs` →
/// `igui::doc_pane::open` + `set_markdown`). No external process.
pub const HELP_CMD_DOCS: u16 = 0x5000;

// ── Internal helpers ──────────────────────────────────────────────────

fn append_item(popup: HMENU, id: u16, label: &str) {
    let mut w: Vec<u16> = label.encode_utf16().collect();
    w.push(0);
    let _ = unsafe { AppendMenuW(popup, MF_STRING, id as usize, PCWSTR(w.as_ptr())) };
}

fn append_sep(popup: HMENU) {
    let _ = unsafe { AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null()) };
}

fn attach_popup(bar: HMENU, popup: HMENU, title: &str) {
    let mut w: Vec<u16> = title.encode_utf16().collect();
    w.push(0);
    let _ = unsafe { AppendMenuW(bar, MF_POPUP, popup.0 as usize, PCWSTR(w.as_ptr())) };
}

// ── Public menu-building functions ────────────────────────────────────

/// File menu: New, Open, Save, Save As, separator, Exit.
pub fn append_file_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    append_item(popup, ledit::FILE_CMD_NEW,     "&New\tCtrl+N");
    append_item(popup, ledit::FILE_CMD_OPEN,    "&Open…\tCtrl+O");
    append_item(popup, ledit::FILE_CMD_SAVE,    "&Save\tCtrl+S");
    append_item(popup, ledit::FILE_CMD_SAVE_AS, "Save &As…\tCtrl+Shift+S");
    append_sep(popup);
    append_item(popup, ledit::FILE_CMD_EXIT,    "E&xit\tAlt+F4");
    attach_popup(bar, popup, "&File");
}

/// Edit menu: undo/redo and clipboard only.
/// Structural editing has moved to the Lisp menu so this stays
/// familiar to users who have never heard of paredit.
pub fn append_edit_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    append_item(popup, ledit::EDIT_CMD_UNDO,       "&Undo\tCtrl+Z");
    append_item(popup, ledit::EDIT_CMD_REDO,       "&Redo\tCtrl+Y");
    append_sep(popup);
    append_item(popup, ledit::EDIT_CMD_CUT,        "Cu&t\tCtrl+X");
    append_item(popup, ledit::EDIT_CMD_COPY,       "&Copy\tCtrl+C");
    append_item(popup, ledit::EDIT_CMD_PASTE,      "&Paste\tCtrl+V");
    append_item(popup, ledit::EDIT_CMD_SELECT_ALL, "Select &All\tCtrl+A");
    attach_popup(bar, popup, "&Edit");
}

/// Lisp menu: evaluation shortcuts and structural editing ops.
pub fn append_lisp_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    append_item(popup, ledit::EDIT_CMD_RUN_BUFFER, "R&un Buffer\tF5");
    append_item(popup, ledit::EDIT_CMD_RUN_FORM,   "Run Form at &Point\tCtrl+Enter");
    append_sep(popup);
    append_item(popup, ledit::EDIT_CMD_FORWARD_SEXP,  "Forward S-expression\tCtrl+\u{2192}");
    append_item(popup, ledit::EDIT_CMD_BACKWARD_SEXP, "Backward S-expression\tCtrl+\u{2190}");
    append_sep(popup);
    append_item(popup, ledit::EDIT_CMD_SLURP_FORWARD, "&Slurp Forward\tCtrl+Shift+\u{2192}");
    append_item(popup, ledit::EDIT_CMD_BARF_FORWARD,  "&Barf Forward\tCtrl+Shift+\u{2190}");
    append_item(popup, ledit::EDIT_CMD_WRAP,          "&Wrap with ( )\tAlt+W");
    append_item(popup, ledit::EDIT_CMD_SPLICE,        "Spli&ce / Unwrap\tAlt+S");
    append_item(popup, ledit::EDIT_CMD_RAISE,         "&Raise\tAlt+R");
    attach_popup(bar, popup, "&Lisp");
}

/// Demos menu: one entry per discovered demo file.
/// `demos` is a slice of (menu_id, display_name) pairs.
/// Silently omitted if the slice is empty.
pub fn append_demos_menu(bar: HMENU, demos: &[(u16, String)]) {
    if demos.is_empty() {
        return;
    }
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    for (id, name) in demos {
        append_item(popup, *id, name);
    }
    attach_popup(bar, popup, "&Demos");
}

/// Tools menu: ledit editor, log overlay, REPL, and crash dump.
pub fn append_tools_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    append_item(popup, ledit::MENU_CMD_ID,       "ledit\tCtrl+Shift+E");
    append_item(popup, log_view::MENU_CMD_ID,    "Log\tCtrl+Shift+L");
    append_sep(popup);
    append_item(popup, repl_child::MENU_CMD_ID,  "REPL\tCtrl+Shift+R");
    append_sep(popup);
    append_item(popup, crash_view::MENU_CMD_ID,  "\u{03BB} Crash dump\tCtrl+Shift+X");
    attach_popup(bar, popup, "&Tools");
}

/// Help menu — Documentation (opens an in-window doc-pane child
/// loaded with `docs/user/index.md`; see `window::open_docs`).
pub fn append_help_menu(bar: HMENU) {
    let Ok(popup) = (unsafe { CreatePopupMenu() }) else { return };
    let mut w: Vec<u16> = "&Documentation\tF1".encode_utf16().collect();
    w.push(0);
    let _ = unsafe { AppendMenuW(popup, MF_STRING, HELP_CMD_DOCS as usize, PCWSTR(w.as_ptr())) };
    let mut t: Vec<u16> = "&Help".encode_utf16().collect();
    t.push(0);
    let _ = unsafe { AppendMenuW(bar, MF_POPUP, popup.0 as usize, PCWSTR(t.as_ptr())) };
}

/// Build the default menu bar: File | Edit | Lisp | [Demos] | Tools | Help.
/// `demos` carries (id, display_name) pairs produced by the frame's
/// demo-discovery pass.
pub fn build_default_menu_bar(demos: &[(u16, String)]) -> Option<HMENU> {
    let bar = unsafe { CreateMenu() }.ok()?;
    append_file_menu(bar);
    append_edit_menu(bar);
    append_lisp_menu(bar);
    append_demos_menu(bar, demos);
    append_tools_menu(bar);
    append_help_menu(bar);
    Some(bar)
}

/// Frame-level accelerator table.
///
/// | Key              | Command           |
/// |------------------|-------------------|
/// | Ctrl+N           | New               |
/// | Ctrl+O           | Open              |
/// | Ctrl+S           | Save              |
/// | Ctrl+Shift+S     | Save As           |
/// | F5               | Run Buffer        |
/// | Ctrl+Shift+E     | Open ledit        |
/// | Ctrl+Shift+L     | Open log view     |
/// | Ctrl+Shift+R     | Open REPL         |
/// | Ctrl+Shift+X     | Open crash dump   |
pub fn build_accelerator_table() -> Option<HACCEL> {
    // VK_F5 = 0x74
    let entries = [
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'N' as u16, cmd: ledit::FILE_CMD_NEW },
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'O' as u16, cmd: ledit::FILE_CMD_OPEN },
        ACCEL { fVirt: FCONTROL | FVIRTKEY,          key: b'S' as u16, cmd: ledit::FILE_CMD_SAVE },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'S' as u16, cmd: ledit::FILE_CMD_SAVE_AS },
        ACCEL { fVirt: FVIRTKEY,                     key: 0x74_u16,    cmd: ledit::EDIT_CMD_RUN_BUFFER },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'E' as u16, cmd: ledit::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'L' as u16, cmd: log_view::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'R' as u16, cmd: repl_child::MENU_CMD_ID },
        ACCEL { fVirt: FCONTROL | FSHIFT | FVIRTKEY, key: b'X' as u16, cmd: crash_view::MENU_CMD_ID },
        // Help
        ACCEL { fVirt: FVIRTKEY,                     key: 0x70_u16,    cmd: HELP_CMD_DOCS },
    ];
    unsafe { CreateAcceleratorTableW(&entries) }
        .ok()
        .filter(|h| !h.is_invalid())
}
