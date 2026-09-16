//! The system menu bar for the macOS IDE.
//!
//! Replaces the old in-window, custom-drawn menu bar: a real `NSMenu`
//! hierarchy in the system menu bar gives us ⌘-glyph rendering, keyboard
//! navigation, ⌘Q/⌘H/⌘M behaviour, and menu-item validation for free.
//!
//! ## How commands reach the IDE
//!
//! Every IDE item carries its [`menu_cmd`] opcode as the `NSMenuItem` tag
//! and targets a singleton dispatcher object whose single action posts
//! `IGuiEvent::Menu { menu_id: menu_cmd::IDE, item_id }` into the shared
//! mailbox — the same mailbox every other input flows through, so the
//! worker-side `Ide::handle_event` routes it exactly like a key press.
//!
//! ## Keyboard equivalents: one owner per combo
//!
//! AppKit would *also* fire a menu item when its key equivalent is pressed,
//! and our local `NSEvent` monitor delivers the same key to the mailbox —
//! a double dispatch. So the monitor owns registered combos instead: it
//! matches key-downs against the registry here ([`key_equivalent_cmd`]),
//! posts the `Menu` event itself, and swallows the `NSEvent` before AppKit
//! ever sees it. Mouse picks go through the dispatcher. Both paths converge
//! on one `Menu` event per activation. Pure-AppKit items (Quit, Hide,
//! Minimize, About) are *not* registered: their key events flow on to
//! AppKit's own menu matching untouched.
//!
//! The registry below is pure data so every invariant (coverage, unique
//! key equivalents, name lookups) is unit-testable headlessly; the `NSMenu`
//! build is a direct translation of it.

use crate::igui_events::menu_cmd;
use crate::igui_events::modifier;
use crate::igui_mac::events::kvk;

#[cfg(feature = "mac-gui")]
use objc2_app_kit::NSApplication;

// ── Registry (pure data) ───────────────────────────────────────────────────

/// A key equivalent: macOS hardware keycode + igui modifier bits
/// (`modifier::WIN` is Command). Exact match — shift is significant
/// (`⌘/` and `⌘?` differ only in shift).
pub struct KeyEq {
    pub kvk: u16,
    pub mods: i64,
}

/// One IDE menu item: registry name, display label, optional key
/// equivalent, and the opcode posted when picked.
pub struct ItemSpec {
    /// Canonical dash-separated name for `NCL_GUI_MENU=<name>` injection.
    pub name: &'static str,
    pub label: &'static str,
    pub key: Option<KeyEq>,
    pub opcode: i64,
}

pub struct MenuSpec {
    pub title: &'static str,
    pub items: &'static [ItemSpec],
}

/// The IDE menus that carry opcodes (the app menu's About/Hide/Quit and the
/// Window menu's Minimize/Zoom are pure-AppKit actions and are built
/// procedurally — only the Settings item below is registry-listed).
pub static MENUS: &[MenuSpec] = &[
    MenuSpec {
        title: "File",
        items: &[
            ItemSpec { name: "new", label: "New", opcode: menu_cmd::NEW,
                key: Some(KeyEq { kvk: kvk::N, mods: modifier::WIN }) },
            ItemSpec { name: "close-tab", label: "Close Tab", opcode: menu_cmd::CLOSE_TAB,
                key: Some(KeyEq { kvk: kvk::W, mods: modifier::WIN }) },
            ItemSpec { name: "save", label: "Save", opcode: menu_cmd::SAVE,
                key: Some(KeyEq { kvk: kvk::S, mods: modifier::WIN }) },
        ],
    },
    MenuSpec {
        title: "Edit",
        items: &[
            ItemSpec { name: "undo", label: "Undo", opcode: menu_cmd::UNDO,
                key: Some(KeyEq { kvk: kvk::Z, mods: modifier::WIN }) },
            ItemSpec { name: "redo", label: "Redo", opcode: menu_cmd::REDO,
                key: Some(KeyEq { kvk: kvk::Z, mods: modifier::WIN | modifier::SHIFT }) },
            ItemSpec { name: "cut", label: "Cut", opcode: menu_cmd::CUT,
                key: Some(KeyEq { kvk: kvk::X, mods: modifier::WIN }) },
            ItemSpec { name: "copy", label: "Copy", opcode: menu_cmd::COPY,
                key: Some(KeyEq { kvk: kvk::C, mods: modifier::WIN }) },
            ItemSpec { name: "paste", label: "Paste", opcode: menu_cmd::PASTE,
                key: Some(KeyEq { kvk: kvk::V, mods: modifier::WIN }) },
            ItemSpec { name: "select-all", label: "Select All", opcode: menu_cmd::SELECT_ALL,
                key: Some(KeyEq { kvk: kvk::A, mods: modifier::WIN }) },
            ItemSpec { name: "find", label: "Find…", opcode: menu_cmd::FIND,
                key: Some(KeyEq { kvk: kvk::F, mods: modifier::WIN }) },
            ItemSpec { name: "find-next", label: "Find Next", opcode: menu_cmd::FIND_NEXT,
                key: Some(KeyEq { kvk: kvk::G, mods: modifier::WIN }) },
            ItemSpec { name: "comment", label: "Comment Selection", opcode: menu_cmd::COMMENT,
                key: Some(KeyEq { kvk: kvk::SLASH, mods: modifier::WIN }) },
        ],
    },
    MenuSpec {
        title: "Eval",
        items: &[
            ItemSpec { name: "run-buffer", label: "Run Buffer", opcode: menu_cmd::RUN_BUFFER,
                key: Some(KeyEq { kvk: kvk::R, mods: modifier::WIN }) },
            ItemSpec { name: "eval-form", label: "Eval Form at Point", opcode: menu_cmd::EVAL_FORM,
                key: Some(KeyEq { kvk: kvk::RETURN, mods: modifier::WIN }) },
            ItemSpec { name: "clear-repl", label: "Clear REPL", opcode: menu_cmd::CLEAR_REPL,
                key: Some(KeyEq { kvk: kvk::K, mods: modifier::WIN }) },
        ],
    },
    MenuSpec {
        title: "View",
        items: &[
            ItemSpec { name: "focus-editor", label: "Focus Editor", opcode: menu_cmd::FOCUS_EDITOR,
                key: Some(KeyEq { kvk: kvk::E, mods: modifier::WIN }) },
            ItemSpec { name: "focus-repl", label: "Focus REPL", opcode: menu_cmd::FOCUS_REPL,
                key: Some(KeyEq { kvk: kvk::L, mods: modifier::WIN }) },
            ItemSpec { name: "font-up", label: "Bigger", opcode: menu_cmd::FONT_UP,
                key: Some(KeyEq { kvk: kvk::EQUALS, mods: modifier::WIN }) },
            ItemSpec { name: "font-down", label: "Smaller", opcode: menu_cmd::FONT_DOWN,
                key: Some(KeyEq { kvk: kvk::MINUS, mods: modifier::WIN }) },
        ],
    },
    MenuSpec {
        title: "Help",
        items: &[
            ItemSpec { name: "help", label: "Keyboard Shortcuts", opcode: menu_cmd::HELP,
                key: Some(KeyEq { kvk: kvk::SLASH, mods: modifier::WIN | modifier::SHIFT }) },
        ],
    },
];

/// Keyed items in the app (first) menu. About/Hide/Quit are AppKit actions;
/// Settings is ours.
pub static APP_KEYED: &[ItemSpec] = &[ItemSpec {
    name: "settings",
    label: "Settings…",
    opcode: menu_cmd::SETTINGS,
    key: Some(KeyEq { kvk: kvk::COMMA, mods: modifier::WIN }),
}];

/// Every registry item, in menu order.
fn all_items() -> impl Iterator<Item = &'static ItemSpec> {
    APP_KEYED.iter().chain(MENUS.iter().flat_map(|m| m.items.iter()))
}

/// The opcode for a registered key equivalent, if (kvk, mods) matches one.
/// `mods` must be pre-masked to SHIFT|CONTROL|ALT|WIN by the caller.
///
/// Used by the `NSEvent` monitor in `window.rs` to decide whether to swallow
/// a key-down (the menu owns that combo) and post the `Menu` event instead.
pub fn key_equivalent_cmd(kvk_: u16, mods: i64) -> Option<i64> {
    all_items().find_map(|it| {
        let k = it.key.as_ref()?;
        (k.kvk == kvk_ && k.mods == mods).then_some(it.opcode)
    })
}

/// Resolve an `NCL_GUI_MENU=<name>` injection name (case-insensitive;
/// `_`, spaces, and `-` all separate words) to an opcode.
pub fn opcode_for_name(name: &str) -> Option<i64> {
    let norm: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c { '_' | ' ' => '-', other => other })
        .collect();
    all_items().find(|it| it.name == norm).map(|it| it.opcode)
}

// ── Item enablement (Save) ────────────────────────────────────────────────

static SAVE_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Publish whether Save should be enabled (the active buffer is dirty).
/// Called from the worker; read by `validateMenuItem:` on the main thread
/// when the menu opens.
pub fn set_save_enabled(on: bool) {
    SAVE_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn save_enabled() -> bool {
    SAVE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

// ── NSMenu build + dispatcher (AppKit) ────────────────────────────────────

/// Build and install the main menu. Must run on the main thread before
/// `app.run()`.
#[cfg(feature = "mac-gui")]
pub fn install(app: &NSApplication, mtm: objc2::MainThreadMarker) {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSMenu, NSMenuItem, NSEventModifierFlags as MF};
    use objc2_foundation::NSString;

    let dispatcher = dispatcher_instance();
    // The typed setter wants &AnyObject; upcast the shared instance once.
    // SAFETY: MenuDispatcher is an NSObject subclass — same object pointer.
    let target: &objc2::runtime::AnyObject = unsafe {
        &*(objc2::rc::Retained::as_ptr(dispatcher)
            as *const objc2::runtime::AnyObject)
    };

    let cmd_item = |label: &str, key: &KeyEq, opcode: i64| -> Retained<NSMenuItem> {
        let it = NSMenuItem::new(mtm);
        it.setTitle(&NSString::from_str(label));
        let (eq, mask) = ns_key_equivalent(key);
        it.setKeyEquivalent(&NSString::from_str(&eq));
        it.setKeyEquivalentModifierMask(mask);
        it.setTag(opcode as isize);
        // SAFETY: setters with plain values on a live item; the dispatcher
        // target outlives the menu (process lifetime).
        unsafe {
            it.setTarget(Some(target));
            it.setAction(Some(objc2::sel!(nclDispatch:)));
        }
        it
    };

    // App (first) menu: the system displays the app name, not this title.
    let app_menu = NSMenu::new(mtm);
    app_menu.setTitle(&NSString::from_str("MacNCL"));
    let about = NSMenuItem::new(mtm);
    about.setTitle(&NSString::from_str("About MacNCL"));
    // SAFETY: standard AppKit action selector.
    unsafe { about.setAction(Some(objc2::sel!(orderFrontStandardAboutPanel:))) };
    app_menu.addItem(&about);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    for spec in APP_KEYED {
        let it = cmd_item(spec.label, spec.key.as_ref().unwrap(), spec.opcode);
        app_menu.addItem(&it);
    }
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let appkit_item = |label: &str, key: &str, mask: MF, action: objc2::runtime::Sel| {
        let it = NSMenuItem::new(mtm);
        it.setTitle(&NSString::from_str(label));
        if !key.is_empty() {
            it.setKeyEquivalent(&NSString::from_str(key));
            it.setKeyEquivalentModifierMask(mask);
        }
        // SAFETY: setting a known AppKit action selector.
        unsafe { it.setAction(Some(action)) };
        it
    };
    app_menu.addItem(&appkit_item("Hide MacNCL", "h", MF::Command, objc2::sel!(hide:)));
    app_menu.addItem(&appkit_item(
        "Hide Others", "h", MF::Command | MF::Option, objc2::sel!(hideOtherApplications:),
    ));
    app_menu.addItem(&appkit_item("Show All", "", MF::empty(), objc2::sel!(unhideAllApplications:)));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&appkit_item("Quit MacNCL", "q", MF::Command, objc2::sel!(terminate:)));
    let app_root = NSMenuItem::new(mtm); // title unused for the app menu
    app_root.setSubmenu(Some(&app_menu));

    let main = NSMenu::new(mtm);
    main.addItem(&app_root);
    for spec in MENUS {
        let m = NSMenu::new(mtm);
        m.setTitle(&NSString::from_str(spec.title));
        for it in spec.items {
            let item = cmd_item(it.label, it.key.as_ref().unwrap(), it.opcode);
            m.addItem(&item);
        }
        let root = NSMenuItem::new(mtm);
        root.setTitle(&NSString::from_str(spec.title));
        root.setSubmenu(Some(&m));
        main.addItem(&root);
    }

    // Window menu: standard selectors with nil targets (they ride the
    // responder chain to the key window / NSApp). Registering it as the
    // windows menu makes AppKit maintain the window list.
    let win = NSMenu::new(mtm);
    win.setTitle(&NSString::from_str("Window"));
    win.addItem(&appkit_item("Minimize", "m", MF::Command, objc2::sel!(miniaturize:)));
    win.addItem(&appkit_item("Zoom", "", MF::empty(), objc2::sel!(performZoom:)));
    win.addItem(&appkit_item(
        "Bring All to Front", "", MF::empty(), objc2::sel!(arrangeInFront:),
    ));
    let win_root = NSMenuItem::new(mtm);
    win_root.setTitle(&NSString::from_str("Window"));
    win_root.setSubmenu(Some(&win));
    main.addItem(&win_root);
    app.setWindowsMenu(Some(&win));

    app.setMainMenu(Some(&main));
}

/// Translate a registry key equivalent into NSMenuItem terms:
/// (keyEquivalent string, modifier mask). `⌘?` is expressed as "?" with a
/// plain Command mask (the conventional display; the physical ⇧⌘/ event
/// never reaches AppKit because the monitor swallows it).
#[cfg(feature = "mac-gui")]
fn ns_key_equivalent(k: &KeyEq) -> (String, objc2_app_kit::NSEventModifierFlags) {
    use objc2_app_kit::NSEventModifierFlags as MF;

    let mut mask = MF::empty();
    if k.mods & modifier::WIN != 0 {
        mask |= MF::Command;
    }
    if k.mods & modifier::SHIFT != 0 {
        mask |= MF::Shift;
    }
    if k.mods & modifier::ALT != 0 {
        mask |= MF::Option;
    }
    if k.mods & modifier::CONTROL != 0 {
        mask |= MF::Control;
    }
    let ch = if k.mods & modifier::SHIFT != 0 && k.kvk == kvk::SLASH {
        "?".to_string()
    } else {
        match k.kvk {
            kvk::A => "a", kvk::C => "c", kvk::E => "e", kvk::F => "f",
            kvk::G => "g", kvk::K => "k", kvk::L => "l", kvk::N => "n",
            kvk::R => "r", kvk::S => "s", kvk::T => "t", kvk::V => "v",
            kvk::W => "w", kvk::X => "x", kvk::Z => "z",
            kvk::SLASH => "/", kvk::COMMA => ",", kvk::RETURN => "\r",
            kvk::EQUALS => "=", kvk::MINUS => "-",
            _ => "",
        }
        .to_string()
    };
    (ch, mask)
}

// ── Dispatcher object ─────────────────────────────────────────────────────

/// The singleton target for every IDE menu item. One action (`nclDispatch:`)
/// reads the item's tag (the opcode) and posts the `Menu` event;
/// `validatesMenuItem:` keeps Save in step with the published dirty flag.
///
/// Menu items do NOT retain their target, so a process-lifetime static
/// `Retained` keeps it alive.
#[cfg(feature = "mac-gui")]
objc2::define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "NCLMenuDispatcher"]
    struct MenuDispatcher;

    impl MenuDispatcher {
        #[unsafe(method(nclDispatch:))]
        fn dispatch(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(item) = sender else { return };
            let tag: isize = unsafe { objc2::msg_send![item, tag] };
            crate::igui_events::push(crate::igui_events::IGuiEvent::Menu {
                menu_id: menu_cmd::IDE,
                item_id: tag as i64,
            });
        }

        #[unsafe(method(validatesMenuItem:))]
        fn validate(&self, sender: Option<&objc2::runtime::AnyObject>) -> objc2::runtime::Bool {
            let Some(item) = sender else { return true.into() };
            let tag: isize = unsafe { objc2::msg_send![item, tag] };
            let enabled = tag as i64 != menu_cmd::SAVE || save_enabled();
            enabled.into()
        }
    }
);

/// The shared dispatcher instance (created lazily on first menu install).
/// `define_class!` types with `NSObject` + `()` ivars are `Sync`, so the
/// `Retained` can live in a static directly.
#[cfg(feature = "mac-gui")]
fn dispatcher_instance() -> &'static objc2::rc::Retained<MenuDispatcher> {
    use objc2::ClassType;
    use std::sync::OnceLock;

    static INSTANCE: OnceLock<objc2::rc::Retained<MenuDispatcher>> = OnceLock::new();
    INSTANCE.get_or_init(|| unsafe { objc2::msg_send![MenuDispatcher::class(), new] })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::igui_events::modifier;

    fn all_opcodes() -> Vec<i64> {
        vec![
            menu_cmd::NEW, menu_cmd::CLOSE_TAB, menu_cmd::SAVE, menu_cmd::SETTINGS,
            menu_cmd::UNDO, menu_cmd::REDO, menu_cmd::CUT, menu_cmd::COPY,
            menu_cmd::PASTE, menu_cmd::SELECT_ALL, menu_cmd::FIND, menu_cmd::FIND_NEXT,
            menu_cmd::COMMENT, menu_cmd::RUN_BUFFER, menu_cmd::EVAL_FORM,
            menu_cmd::CLEAR_REPL, menu_cmd::FOCUS_EDITOR, menu_cmd::FOCUS_REPL,
            menu_cmd::HELP, menu_cmd::FONT_UP, menu_cmd::FONT_DOWN,
        ]
    }

    /// Sprint 1 AC: every registry item maps to a known opcode, every known
    /// opcode is reachable from some item — no orphans in either direction.
    #[test]
    fn registry_and_opcodes_cover_each_other() {
        let registered: Vec<i64> = all_items().map(|it| it.opcode).collect();
        for op in all_opcodes() {
            assert!(
                registered.contains(&op),
                "menu_cmd {op} has no menu item"
            );
        }
        for it in all_items() {
            assert!(
                all_opcodes().contains(&it.opcode),
                "item {} maps to unknown opcode {}",
                it.name, it.opcode
            );
        }
        // And each opcode appears exactly once (no duplicated commands).
        let mut sorted = registered.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), registered.len(), "duplicate opcodes: {registered:?}");
    }

    /// No two items may claim the same (keycode, modifiers) — the monitor
    /// swallows on that pair and would shadow one of them.
    #[test]
    fn key_equivalents_are_unambiguous() {
        let mut seen: Vec<(u16, i64)> = Vec::new();
        for it in all_items() {
            if let Some(k) = &it.key {
                let pair = (k.kvk, k.mods);
                assert!(
                    !seen.contains(&pair),
                    "duplicate key equivalent {pair:?} on {}",
                    it.name
                );
                seen.push(pair);
            }
        }
    }

    #[test]
    fn key_equivalent_matching() {
        use crate::igui_mac::events::kvk;
        let w = modifier::WIN;
        assert_eq!(key_equivalent_cmd(kvk::N, w), Some(menu_cmd::NEW));
        assert_eq!(key_equivalent_cmd(kvk::W, w), Some(menu_cmd::CLOSE_TAB));
        assert_eq!(key_equivalent_cmd(kvk::S, w), Some(menu_cmd::SAVE));
        assert_eq!(key_equivalent_cmd(kvk::Z, w), Some(menu_cmd::UNDO));
        assert_eq!(
            key_equivalent_cmd(kvk::Z, w | modifier::SHIFT),
            Some(menu_cmd::REDO)
        );
        assert_eq!(key_equivalent_cmd(kvk::X, w), Some(menu_cmd::CUT));
        assert_eq!(key_equivalent_cmd(kvk::SLASH, w), Some(menu_cmd::COMMENT));
        assert_eq!(
            key_equivalent_cmd(kvk::SLASH, w | modifier::SHIFT),
            Some(menu_cmd::HELP)
        );
        assert_eq!(
            key_equivalent_cmd(kvk::RETURN, w),
            Some(menu_cmd::EVAL_FORM)
        );
        assert_eq!(key_equivalent_cmd(kvk::COMMA, w), Some(menu_cmd::SETTINGS));
        // Modifiers are exact: bare keys, wrong modifiers → no match.
        assert_eq!(key_equivalent_cmd(kvk::N, 0), None);
        assert_eq!(key_equivalent_cmd(kvk::N, w | modifier::SHIFT), None);
        assert_eq!(key_equivalent_cmd(kvk::RETURN, 0), None);
    }

    #[test]
    fn opcode_for_name_resolves_every_item_and_is_forgiving() {
        for it in all_items() {
            assert_eq!(opcode_for_name(it.name), Some(it.opcode), "name {}", it.name);
        }
        assert_eq!(opcode_for_name("Run Buffer"), Some(menu_cmd::RUN_BUFFER));
        assert_eq!(opcode_for_name("RUN_BUFFER"), Some(menu_cmd::RUN_BUFFER));
        assert_eq!(opcode_for_name("run-buffer"), Some(menu_cmd::RUN_BUFFER));
        assert_eq!(opcode_for_name("Eval-Form"), Some(menu_cmd::EVAL_FORM));
        assert_eq!(opcode_for_name("bogus"), None);
    }

    #[test]
    fn save_enabled_flag_flips() {
        let before = save_enabled();
        set_save_enabled(!before);
        assert_eq!(save_enabled(), !before);
        set_save_enabled(before);
    }
}
