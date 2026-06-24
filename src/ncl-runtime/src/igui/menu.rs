//! Frame-menu spec parsing + Win32 HMENU build + MDI verb dispatch.
//!
//! Spec format (line-oriented):
//!
//! ```text
//! MENU File
//! ITEM 1001 New
//! ITEM 1002 Open
//! SEP
//! ITEM 1099 Exit
//! MENU Window
//! MDI cascade
//! MDI tile-h
//! MDI tile-v
//! MDI close-all
//! MDI arrange-icons
//! ```
//!
//! - Lines starting with `MENU <title>` open a top-level submenu.
//!   Title text supports `&` mnemonics (Win32 native).
//! - Lines starting with `ITEM <id> <title>` add a command item.
//!   Caller-chosen id, must be in 0x1000..=0x1FFF.
//! - Lines starting with `SEP` insert a separator into the current
//!   submenu.
//! - Lines starting with `MDI <kind>` add a standard MDI verb item
//!   to the current submenu. Recognized kinds: `cascade`, `tile-h`,
//!   `tile-v`, `close-all`, `arrange-icons`. The id is auto-assigned
//!   from a reserved range (0x2000..=0x2010) and dispatched directly
//!   to the MDI client; no `EvMenu` event fires for these items.
//!
//! Comment lines (starting with `#`) and blank lines are ignored.

#![cfg(windows)]

use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateMenu, CreatePopupMenu, DrawMenuBar, GetClientRect, SendMessageW, SetMenu,
    HMENU, MF_POPUP, MF_SEPARATOR, MF_STRING, WM_MDICASCADE, WM_MDIICONARRANGE, WM_MDITILE,
    WM_SIZE, MDITILE_HORIZONTAL, MDITILE_VERTICAL,
};

/// Auto-allocated id range for `MDI` items. None of these reach the
/// language thread as `EvMenu`; they're dispatched directly to the
/// MDI client by the frame WndProc.
pub const MDI_VERB_BASE: u16 = 0x2000;

#[derive(Debug, Clone, Copy)]
pub enum MdiVerb {
    Cascade,
    TileH,
    TileV,
    CloseAll,
    ArrangeIcons,
}

#[derive(Debug, Clone)]
enum Item {
    Command { id: u16, title: String },
    Separator,
    Mdi { id: u16, verb: MdiVerb, title: String },
}

#[derive(Debug, Clone)]
struct Submenu {
    title: String,
    items: Vec<Item>,
}

#[derive(Debug, Clone, Default)]
struct ParsedMenu {
    submenus: Vec<Submenu>,
}

fn parse(spec: &str) -> ParsedMenu {
    let mut out = ParsedMenu::default();
    let mut current: Option<Submenu> = None;
    let mut next_mdi_id: u16 = MDI_VERB_BASE;

    // Accept both newline and semicolon as line separators so CP
    // callers can write a single-line spec.
    let normalized: String = spec.replace(';', "\n");
    for raw in normalized.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("MENU ") {
            if let Some(menu) = current.take() {
                out.submenus.push(menu);
            }
            current = Some(Submenu {
                title: rest.trim().to_string(),
                items: Vec::new(),
            });
        } else if line == "SEP" {
            if let Some(m) = current.as_mut() {
                m.items.push(Item::Separator);
            }
        } else if let Some(rest) = line.strip_prefix("ITEM ") {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let Some(id_str) = parts.next() else { continue };
            let Some(title) = parts.next() else { continue };
            let Ok(id) = id_str.parse::<u16>() else { continue };
            if let Some(m) = current.as_mut() {
                m.items.push(Item::Command {
                    id,
                    title: title.trim().to_string(),
                });
            }
        } else if let Some(rest) = line.strip_prefix("MDI ") {
            let kind = rest.trim();
            let (verb, title) = match kind {
                "cascade" => (MdiVerb::Cascade, "Cascade"),
                "tile-h" => (MdiVerb::TileH, "Tile Horizontally"),
                "tile-v" => (MdiVerb::TileV, "Tile Vertically"),
                "close-all" => (MdiVerb::CloseAll, "Close All"),
                "arrange-icons" => (MdiVerb::ArrangeIcons, "Arrange Icons"),
                _ => continue,
            };
            if let Some(m) = current.as_mut() {
                let id = next_mdi_id;
                next_mdi_id += 1;
                m.items.push(Item::Mdi {
                    id,
                    verb,
                    title: title.to_string(),
                });
            }
        }
        // Anything else is silently dropped.
    }
    if let Some(menu) = current.take() {
        out.submenus.push(menu);
    }
    out
}

// Track id -> MdiVerb so the frame WndProc can dispatch on WM_COMMAND.
static MDI_DISPATCH: Mutex<Option<Vec<(u16, MdiVerb)>>> = Mutex::new(None);

pub fn lookup_mdi_verb(id: u16) -> Option<MdiVerb> {
    let guard = MDI_DISPATCH.lock().expect("MDI_DISPATCH poisoned");
    let table = guard.as_ref()?;
    table.iter().find_map(|(i, v)| (*i == id).then_some(*v))
}

fn record_mdi_dispatch(parsed: &ParsedMenu) {
    let mut table = Vec::new();
    for sub in &parsed.submenus {
        for item in &sub.items {
            if let Item::Mdi { id, verb, .. } = item {
                table.push((*id, *verb));
            }
        }
    }
    let mut guard = MDI_DISPATCH.lock().expect("MDI_DISPATCH poisoned");
    *guard = Some(table);
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn build_hmenu(parsed: &ParsedMenu) -> Result<HMENU, String> {
    let bar = unsafe { CreateMenu() }.map_err(|e| format!("CreateMenu: {e}"))?;
    for sub in &parsed.submenus {
        let popup = unsafe { CreatePopupMenu() }.map_err(|e| format!("CreatePopupMenu: {e}"))?;
        for item in &sub.items {
            match item {
                Item::Separator => {
                    unsafe { AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null()) }
                        .map_err(|e| format!("AppendMenuW(SEP): {e}"))?;
                }
                Item::Command { id, title } => {
                    let title_w = utf16(title);
                    unsafe {
                        AppendMenuW(
                            popup,
                            MF_STRING,
                            *id as usize,
                            PCWSTR(title_w.as_ptr()),
                        )
                    }
                    .map_err(|e| format!("AppendMenuW(item): {e}"))?;
                }
                Item::Mdi { id, title, .. } => {
                    let title_w = utf16(title);
                    unsafe {
                        AppendMenuW(
                            popup,
                            MF_STRING,
                            *id as usize,
                            PCWSTR(title_w.as_ptr()),
                        )
                    }
                    .map_err(|e| format!("AppendMenuW(mdi): {e}"))?;
                }
            }
        }
        let title_w = utf16(&sub.title);
        unsafe {
            AppendMenuW(
                bar,
                MF_POPUP,
                popup.0 as usize,
                PCWSTR(title_w.as_ptr()),
            )
        }
        .map_err(|e| format!("AppendMenuW(popup): {e}"))?;
    }
    Ok(bar)
}

/// Build the menu bar from `spec` and install it on `frame`. Returns
/// `true` on success.
pub fn install_for_frame(frame: HWND, mdi_client: HWND, spec: &str) -> bool {
    eprintln!("[igui-menu] spec received ({} bytes): {:?}", spec.len(), spec);
    let parsed = parse(spec);
    eprintln!(
        "[igui-menu] parsed {} submenus: {:?}",
        parsed.submenus.len(),
        parsed.submenus.iter().map(|s| &s.title).collect::<Vec<_>>()
    );
    record_mdi_dispatch(&parsed);
    let menu = match build_hmenu(&parsed) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[igui-menu] build failed: {e}");
            return false;
        }
    };
    // Always re-append the built-in editor menus (Edit, Lisp, Tools)
    // so they stay reachable regardless of what the language thread
    // set. Edit/Lisp items dispatch to whichever ledit child is active.
    super::tools_menu::append_edit_menu(menu);
    super::tools_menu::append_lisp_menu(menu);
    super::tools_menu::append_tools_menu(menu);
    let set_result = unsafe { SetMenu(frame, Some(menu)) };
    let draw_result = unsafe { DrawMenuBar(frame) };
    eprintln!(
        "[igui-menu] installed: {} submenus, SetMenu={:?} DrawMenuBar={:?}",
        parsed.submenus.len(),
        set_result,
        draw_result
    );
    // Setting the menu bar shrinks the frame's client area; fire a
    // WM_SIZE so DefFrameProc repositions the MDI client to fit.
    let mut rect = windows::Win32::Foundation::RECT::default();
    let _ = unsafe { GetClientRect(frame, &mut rect) };
    let w = (rect.right - rect.left) as usize;
    let h = (rect.bottom - rect.top) as usize;
    unsafe {
        SendMessageW(
            frame,
            WM_SIZE,
            Some(windows::Win32::Foundation::WPARAM(0)),
            Some(windows::Win32::Foundation::LPARAM(
                ((h << 16) | w) as isize,
            )),
        )
    };
    let _ = mdi_client;
    true
}

/// Dispatch one of the standard MDI verbs to the given MDI client.
pub fn dispatch_mdi(mdi_client: HWND, verb: MdiVerb) {
    let (msg, wparam) = match verb {
        MdiVerb::Cascade => (WM_MDICASCADE, 0usize),
        MdiVerb::TileH => (WM_MDITILE, MDITILE_HORIZONTAL.0 as usize),
        MdiVerb::TileV => (WM_MDITILE, MDITILE_VERTICAL.0 as usize),
        MdiVerb::CloseAll => {
            // Win32 has no single-message Close All; iterate via the
            // registry snapshot. Done by frame_wnd_proc which has
            // direct access.
            return;
        }
        MdiVerb::ArrangeIcons => (WM_MDIICONARRANGE, 0usize),
    };
    unsafe {
        SendMessageW(
            mdi_client,
            msg,
            Some(windows::Win32::Foundation::WPARAM(wparam)),
            Some(windows::Win32::Foundation::LPARAM(0)),
        )
    };
}
