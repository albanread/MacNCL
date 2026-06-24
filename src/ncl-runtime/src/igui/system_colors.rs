//! Process-wide system color cache + theme-change wiring.
//!
//! The CP side never embeds raw OS palette indices in `SurfaceCmd`s
//! (every command carries explicit `Rgba`). System colors are read
//! through the `iGui.SystemColor` query which returns the current
//! theme's RGBA for a logical role. The cache refreshes when the
//! frame WndProc receives `WM_SYSCOLORCHANGE` or `WM_THEMECHANGED`,
//! at which point we push an `EvThemeChange` event onto the mailbox
//! so language-thread views can repaint anything theme-dependent.

#![cfg(windows)]

use std::sync::Mutex;

use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_3DFACE, COLOR_BTNTEXT, COLOR_CAPTIONTEXT, COLOR_GRAYTEXT,
    COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_INFOBK, COLOR_INFOTEXT, COLOR_MENU,
    COLOR_MENUTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT,
};

use super::batch::Rgba;
use super::channels::{self, IGuiEvent};

/// Stable enum constants exposed to CP via `iGui.Sc*`. Append-only.
pub mod kind {
    pub const WINDOW_BG: i32 = 0;
    pub const WINDOW_FG: i32 = 1;
    pub const CONTROL_BG: i32 = 2;
    pub const CONTROL_FG: i32 = 3;
    pub const SELECTION_BG: i32 = 4;
    pub const SELECTION_FG: i32 = 5;
    pub const HIGHLIGHT_BG: i32 = 6;
    pub const HIGHLIGHT_FG: i32 = 7;
    pub const DISABLED_FG: i32 = 8;
    pub const CARET: i32 = 9;
    pub const DIALOG_BG: i32 = 10;
    pub const DIALOG_FG: i32 = 11;
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub window_bg: Rgba,
    pub window_fg: Rgba,
    pub control_bg: Rgba,
    pub control_fg: Rgba,
    pub selection_bg: Rgba,
    pub selection_fg: Rgba,
    pub highlight_bg: Rgba,
    pub highlight_fg: Rgba,
    pub disabled_fg: Rgba,
    pub caret: Rgba,
    pub dialog_bg: Rgba,
    pub dialog_fg: Rgba,
}

impl Default for Palette {
    fn default() -> Self {
        // Sensible mid-light defaults until the first sample().
        const LIGHT: Rgba = Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 };
        const DARK: Rgba = Rgba { r: 0.10, g: 0.10, b: 0.10, a: 1.0 };
        const ACCENT: Rgba = Rgba { r: 0.20, g: 0.50, b: 0.85, a: 1.0 };
        Self {
            window_bg: LIGHT,
            window_fg: DARK,
            control_bg: LIGHT,
            control_fg: DARK,
            selection_bg: ACCENT,
            selection_fg: LIGHT,
            highlight_bg: ACCENT,
            highlight_fg: LIGHT,
            disabled_fg: Rgba { r: 0.55, g: 0.55, b: 0.55, a: 1.0 },
            caret: DARK,
            dialog_bg: LIGHT,
            dialog_fg: DARK,
        }
    }
}

static PALETTE: Mutex<Palette> = Mutex::new(Palette {
    // Same as Default, but spelled out so this can be a const Mutex.
    window_bg: Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 },
    window_fg: Rgba { r: 0.10, g: 0.10, b: 0.10, a: 1.0 },
    control_bg: Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 },
    control_fg: Rgba { r: 0.10, g: 0.10, b: 0.10, a: 1.0 },
    selection_bg: Rgba { r: 0.20, g: 0.50, b: 0.85, a: 1.0 },
    selection_fg: Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 },
    highlight_bg: Rgba { r: 0.20, g: 0.50, b: 0.85, a: 1.0 },
    highlight_fg: Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 },
    disabled_fg: Rgba { r: 0.55, g: 0.55, b: 0.55, a: 1.0 },
    caret: Rgba { r: 0.10, g: 0.10, b: 0.10, a: 1.0 },
    dialog_bg: Rgba { r: 0.95, g: 0.95, b: 0.95, a: 1.0 },
    dialog_fg: Rgba { r: 0.10, g: 0.10, b: 0.10, a: 1.0 },
});

fn cref_to_rgba(cref: u32) -> Rgba {
    let r = (cref & 0xFF) as f32 / 255.0;
    let g = ((cref >> 8) & 0xFF) as f32 / 255.0;
    let b = ((cref >> 16) & 0xFF) as f32 / 255.0;
    Rgba { r, g, b, a: 1.0 }
}

/// Sample the current theme palette via `GetSysColor`. Idempotent;
/// safe to call repeatedly.
pub fn sample() {
    let win = cref_to_rgba(unsafe { GetSysColor(COLOR_WINDOW) });
    let win_text = cref_to_rgba(unsafe { GetSysColor(COLOR_WINDOWTEXT) });
    let face = cref_to_rgba(unsafe { GetSysColor(COLOR_3DFACE) });
    let face_text = cref_to_rgba(unsafe { GetSysColor(COLOR_BTNTEXT) });
    let sel = cref_to_rgba(unsafe { GetSysColor(COLOR_HIGHLIGHT) });
    let sel_text = cref_to_rgba(unsafe { GetSysColor(COLOR_HIGHLIGHTTEXT) });
    let info_bg = cref_to_rgba(unsafe { GetSysColor(COLOR_INFOBK) });
    let info_fg = cref_to_rgba(unsafe { GetSysColor(COLOR_INFOTEXT) });
    let disabled = cref_to_rgba(unsafe { GetSysColor(COLOR_GRAYTEXT) });
    let caret = cref_to_rgba(unsafe { GetSysColor(COLOR_CAPTIONTEXT) });
    let menu = cref_to_rgba(unsafe { GetSysColor(COLOR_MENU) });
    let menu_text = cref_to_rgba(unsafe { GetSysColor(COLOR_MENUTEXT) });

    let new_palette = Palette {
        window_bg: win,
        window_fg: win_text,
        control_bg: face,
        control_fg: face_text,
        selection_bg: sel,
        selection_fg: sel_text,
        highlight_bg: info_bg,
        highlight_fg: info_fg,
        disabled_fg: disabled,
        caret,
        dialog_bg: menu,
        dialog_fg: menu_text,
    };

    let mut guard = PALETTE.lock().expect("PALETTE poisoned");
    *guard = new_palette;
}

/// Refresh the palette and emit an `EvThemeChange` event. Called by
/// the frame WndProc on `WM_SYSCOLORCHANGE` / `WM_THEMECHANGED`.
pub fn refresh_and_notify() {
    sample();
    channels::push(IGuiEvent::ThemeChange);
}

pub fn lookup(k: i32) -> Rgba {
    let p = *PALETTE.lock().expect("PALETTE poisoned");
    match k {
        x if x == kind::WINDOW_BG => p.window_bg,
        x if x == kind::WINDOW_FG => p.window_fg,
        x if x == kind::CONTROL_BG => p.control_bg,
        x if x == kind::CONTROL_FG => p.control_fg,
        x if x == kind::SELECTION_BG => p.selection_bg,
        x if x == kind::SELECTION_FG => p.selection_fg,
        x if x == kind::HIGHLIGHT_BG => p.highlight_bg,
        x if x == kind::HIGHLIGHT_FG => p.highlight_fg,
        x if x == kind::DISABLED_FG => p.disabled_fg,
        x if x == kind::CARET => p.caret,
        x if x == kind::DIALOG_BG => p.dialog_bg,
        x if x == kind::DIALOG_FG => p.dialog_fg,
        _ => p.window_fg,
    }
}
