//! `NSEvent` → `IGuiEvent` translation.
//!
//! The Windows iGui synthesises `IGuiEvent`s from Win32 messages
//! (`WM_KEYDOWN`/`WM_CHAR`/`WM_*BUTTON*`) inside `window.rs`. On macOS the
//! same events come from `NSEvent`s delivered to the content `NSView`.
//!
//! To keep the Lisp side identical across platforms, the `IGuiEvent`
//! fields carry the **same meanings** as on Windows: `mods` uses the
//! `igui_events::modifier::*` bits, and `vkey` uses **Win32 virtual-key
//! codes** so Lisp event code (`Library/events.lisp`) matches the same
//! constants on both platforms. We translate macOS hardware keycodes and
//! modifier flags into those.
//!
//! The mapping functions here are intentionally **pure** — they take raw
//! integers, not `NSEvent` objects — so the whole translation table is
//! unit-testable headlessly. `window.rs` reads the values off the real
//! `NSEvent` and calls these.

use crate::igui_events::{modifier, mouse_op, IGuiEvent};

/// `NSEventModifierFlags` bit positions (from `<AppKit/NSEvent.h>`).
pub mod nsflags {
    pub const CAPS_LOCK: u64 = 1 << 16;
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const OPTION: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
}

/// macOS hardware virtual keycodes (`<Carbon/HIToolbox/Events.h>`, `kVK_*`).
pub mod kvk {
    pub const RETURN: u16 = 0x24;
    pub const TAB: u16 = 0x30;
    pub const SPACE: u16 = 0x31;
    pub const DELETE: u16 = 0x33; // Backspace
    pub const ESCAPE: u16 = 0x35;
    pub const FORWARD_DELETE: u16 = 0x75;
    pub const HOME: u16 = 0x73;
    pub const END: u16 = 0x77;
    pub const PAGE_UP: u16 = 0x74;
    pub const PAGE_DOWN: u16 = 0x79;
    pub const LEFT: u16 = 0x7B;
    pub const RIGHT: u16 = 0x7C;
    pub const DOWN: u16 = 0x7D;
    pub const UP: u16 = 0x7E;
    pub const F1: u16 = 0x7A;
    pub const F2: u16 = 0x78;
    pub const F3: u16 = 0x63;
    pub const F4: u16 = 0x76;
    pub const F5: u16 = 0x60;
    pub const F6: u16 = 0x61;
    pub const F7: u16 = 0x62;
    pub const F8: u16 = 0x64;
    pub const F9: u16 = 0x65;
    pub const F10: u16 = 0x6D;
    pub const F11: u16 = 0x67;
    pub const F12: u16 = 0x6F;
    pub const SLASH: u16 = 0x2C; // kVK_ANSI_Slash
    // Letters needed for menu key-equivalent matching (see igui_mac::menu).
    pub const A: u16 = 0x00;
    pub const C: u16 = 0x08;
    pub const COMMA: u16 = 0x2B;
    pub const E: u16 = 0x0E;
    pub const F: u16 = 0x03;
    pub const G: u16 = 0x05;
    pub const K: u16 = 0x28;
    pub const L: u16 = 0x25;
    pub const N: u16 = 0x2D;
    pub const R: u16 = 0x0F;
    pub const S: u16 = 0x01;
    pub const T: u16 = 0x11;
    pub const V: u16 = 0x09;
    pub const W: u16 = 0x0D;
    pub const X: u16 = 0x07;
    pub const Z: u16 = 0x06;
}

/// Win32 virtual-key codes we map onto (the values the Lisp side expects).
pub mod vk {
    pub const BACK: i64 = 0x08;
    pub const TAB: i64 = 0x09;
    pub const RETURN: i64 = 0x0D;
    pub const ESCAPE: i64 = 0x1B;
    pub const SPACE: i64 = 0x20;
    pub const PRIOR: i64 = 0x21; // Page Up
    pub const NEXT: i64 = 0x22; // Page Down
    pub const END: i64 = 0x23;
    pub const HOME: i64 = 0x24;
    pub const LEFT: i64 = 0x25;
    pub const UP: i64 = 0x26;
    pub const RIGHT: i64 = 0x27;
    pub const DOWN: i64 = 0x28;
    pub const DELETE: i64 = 0x2E;
    pub const F1: i64 = 0x70;
    pub const OEM_2: i64 = 0xBF; // '/' '?'
}

/// Translate `NSEventModifierFlags` into the `igui_events::modifier::*`
/// bitmask. Command maps to the `WIN` bit (it is the macOS analogue of
/// the Windows/Super key for accelerator purposes); Option maps to ALT.
pub fn mods_from_flags(flags: u64) -> i64 {
    let mut m = 0;
    if flags & nsflags::SHIFT != 0 {
        m |= modifier::SHIFT;
    }
    if flags & nsflags::CONTROL != 0 {
        m |= modifier::CONTROL;
    }
    if flags & nsflags::OPTION != 0 {
        m |= modifier::ALT;
    }
    if flags & nsflags::COMMAND != 0 {
        m |= modifier::WIN;
    }
    if flags & nsflags::CAPS_LOCK != 0 {
        m |= modifier::CAPS;
    }
    m
}

/// Map a macOS hardware keycode to a Win32 VK code for the non-printable
/// keys the editor cares about. Returns `0` for keys best identified by
/// their character (letters/digits/punctuation) — `vkey_from_char`
/// covers those.
pub fn vkey_from_keycode(keycode: u16) -> i64 {
    match keycode {
        kvk::RETURN => vk::RETURN,
        kvk::TAB => vk::TAB,
        kvk::SPACE => vk::SPACE,
        kvk::DELETE => vk::BACK,
        kvk::FORWARD_DELETE => vk::DELETE,
        kvk::ESCAPE => vk::ESCAPE,
        kvk::HOME => vk::HOME,
        kvk::END => vk::END,
        kvk::PAGE_UP => vk::PRIOR,
        kvk::PAGE_DOWN => vk::NEXT,
        kvk::LEFT => vk::LEFT,
        kvk::RIGHT => vk::RIGHT,
        kvk::UP => vk::UP,
        kvk::DOWN => vk::DOWN,
        kvk::F1 => vk::F1,
        kvk::F2 => vk::F1 + 1,
        kvk::F3 => vk::F1 + 2,
        kvk::F4 => vk::F1 + 3,
        kvk::F5 => vk::F1 + 4,
        kvk::F6 => vk::F1 + 5,
        kvk::F7 => vk::F1 + 6,
        kvk::F8 => vk::F1 + 7,
        kvk::F9 => vk::F1 + 8,
        kvk::F10 => vk::F1 + 9,
        kvk::F11 => vk::F1 + 10,
        kvk::F12 => vk::F1 + 11,
        kvk::SLASH => vk::OEM_2,
        _ => 0,
    }
}

/// Win32 VK for a printable character: letters → 'A'..'Z' (0x41..0x5A),
/// digits → '0'..'9' (0x30..0x39). Other characters return 0; the Lisp
/// side uses the `Char` event's codepoint for those.
pub fn vkey_from_char(c: char) -> i64 {
    match c {
        'a'..='z' => (c as i64 - 'a' as i64) + 0x41,
        'A'..='Z' => (c as i64 - 'A' as i64) + 0x41,
        '0'..='9' => c as i64,
        _ => 0,
    }
}

/// Resolve a key event's `vkey`: prefer the hardware-keycode mapping (for
/// named keys), fall back to the character mapping (for letters/digits).
pub fn resolve_vkey(keycode: u16, chars_ignoring_mods: Option<char>) -> i64 {
    let vk = vkey_from_keycode(keycode);
    if vk != 0 {
        return vk;
    }
    chars_ignoring_mods.map(vkey_from_char).unwrap_or(0)
}

/// Convert a y coordinate from `NSEvent`'s window space (bottom-left
/// origin, y-up) to top-left, y-down view space matching the `SurfaceCmd`
/// IR. `view_height` is the content view's height in points.
#[inline]
pub fn to_top_left_y(y_window: f64, view_height: f64) -> f64 {
    view_height - y_window
}

// ── IGuiEvent builders (used by window.rs once it has the NSEvent) ──────

pub fn key_event(
    child_id: i64,
    keycode: u16,
    chars_ignoring_mods: Option<char>,
    flags: u64,
    repeat: bool,
    down: bool,
    time_ms: i64,
) -> IGuiEvent {
    IGuiEvent::Key {
        child_id,
        vkey: resolve_vkey(keycode, chars_ignoring_mods),
        scancode: keycode as i64,
        mods: mods_from_flags(flags),
        repeat: repeat as i64,
        down,
        time_ms,
    }
}

pub fn char_event(child_id: i64, codepoint: u32, flags: u64, time_ms: i64) -> IGuiEvent {
    IGuiEvent::Char {
        child_id,
        codepoint: codepoint as i64,
        mods: mods_from_flags(flags),
        time_ms,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn mouse_event(
    child_id: i64,
    x: f64,
    y_top_left: f64,
    op: i64,
    button: i64,
    flags: u64,
    wheel_delta: i64,
    wheel_lines: i64,
    time_ms: i64,
) -> IGuiEvent {
    IGuiEvent::Mouse {
        child_id,
        x: x as i64,
        y: y_top_left as i64,
        op,
        button,
        mods: mods_from_flags(flags),
        wheel_delta,
        wheel_lines,
        time_ms,
    }
}

/// Mouse sub-kind from the AppKit event selector, expressed as a small
/// tag `window.rs` passes in (it knows which `NSView` method fired).
pub mod ns_mouse {
    use crate::igui_events::mouse_op;
    pub fn left_down() -> i64 {
        mouse_op::LEFT_DOWN
    }
    pub fn left_up() -> i64 {
        mouse_op::LEFT_UP
    }
    pub fn right_down() -> i64 {
        mouse_op::RIGHT_DOWN
    }
    pub fn right_up() -> i64 {
        mouse_op::RIGHT_UP
    }
    pub fn moved() -> i64 {
        mouse_op::MOVE
    }
    pub fn wheel() -> i64 {
        mouse_op::WHEEL
    }
}

// Keep `mouse_op` referenced even if `ns_mouse` is the only user.
#[allow(unused_imports)]
use mouse_op as _mouse_op;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::igui_events::{modifier, mouse_op};

    #[test]
    fn modifier_flags_map_to_bits() {
        assert_eq!(mods_from_flags(0), 0);
        assert_eq!(mods_from_flags(nsflags::SHIFT), modifier::SHIFT);
        assert_eq!(mods_from_flags(nsflags::CONTROL), modifier::CONTROL);
        assert_eq!(mods_from_flags(nsflags::OPTION), modifier::ALT);
        assert_eq!(mods_from_flags(nsflags::COMMAND), modifier::WIN);
        assert_eq!(mods_from_flags(nsflags::CAPS_LOCK), modifier::CAPS);
        // Combined: Ctrl+Shift.
        assert_eq!(
            mods_from_flags(nsflags::CONTROL | nsflags::SHIFT),
            modifier::CONTROL | modifier::SHIFT
        );
    }

    #[test]
    fn named_keys_map_to_win32_vks() {
        assert_eq!(vkey_from_keycode(kvk::RETURN), vk::RETURN);
        assert_eq!(vkey_from_keycode(kvk::DELETE), vk::BACK); // Mac Delete = Backspace
        assert_eq!(vkey_from_keycode(kvk::FORWARD_DELETE), vk::DELETE);
        assert_eq!(vkey_from_keycode(kvk::ESCAPE), vk::ESCAPE);
        assert_eq!(vkey_from_keycode(kvk::LEFT), vk::LEFT);
        assert_eq!(vkey_from_keycode(kvk::UP), vk::UP);
        assert_eq!(vkey_from_keycode(kvk::F1), vk::F1);
        assert_eq!(vkey_from_keycode(kvk::F12), vk::F1 + 11);
        // Unknown / printable keycode → 0 (resolved via character).
        assert_eq!(vkey_from_keycode(0x00), 0);
    }

    #[test]
    fn printable_chars_map_to_vks() {
        assert_eq!(vkey_from_char('a'), 0x41);
        assert_eq!(vkey_from_char('A'), 0x41);
        assert_eq!(vkey_from_char('z'), 0x5A);
        assert_eq!(vkey_from_char('0'), 0x30);
        assert_eq!(vkey_from_char('9'), 0x39);
        assert_eq!(vkey_from_char('!'), 0);
    }

    #[test]
    fn resolve_prefers_keycode_then_char() {
        // Named key: keycode wins, character ignored.
        assert_eq!(resolve_vkey(kvk::RETURN, Some('\r')), vk::RETURN);
        // Letter: keycode unknown (0), falls back to the character.
        assert_eq!(resolve_vkey(0x00, Some('k')), 0x4B);
        // Nothing usable.
        assert_eq!(resolve_vkey(0x00, None), 0);
    }

    #[test]
    fn y_flips_to_top_left() {
        assert_eq!(to_top_left_y(0.0, 300.0), 300.0); // bottom in NS → bottom row
        assert_eq!(to_top_left_y(300.0, 300.0), 0.0); // top in NS → row 0
    }

    #[test]
    fn builders_produce_expected_events() {
        // Ctrl+Left key-down on child 7.
        let ev = key_event(7, kvk::LEFT, None, nsflags::CONTROL, false, true, 1234);
        match ev {
            IGuiEvent::Key { child_id, vkey, mods, down, scancode, .. } => {
                assert_eq!(child_id, 7);
                assert_eq!(vkey, vk::LEFT);
                assert_eq!(mods, modifier::CONTROL);
                assert!(down);
                assert_eq!(scancode, kvk::LEFT as i64);
            }
            _ => panic!("expected Key"),
        }
        // Left mouse down at (10, 20) top-left.
        let ev = mouse_event(7, 10.0, 20.0, ns_mouse::left_down(), 0, 0, 0, 0, 99);
        match ev {
            IGuiEvent::Mouse { x, y, op, .. } => {
                assert_eq!((x, y), (10, 20));
                assert_eq!(op, mouse_op::LEFT_DOWN);
            }
            _ => panic!("expected Mouse"),
        }
    }
}
