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
/// Settings is ours. `Key Clicks` toggles the keypress sound (handled on
/// the main thread — the state is the static below, no worker round-trip).
pub static APP_KEYED: &[ItemSpec] = &[
    ItemSpec {
        name: "settings",
        label: "Settings…",
        opcode: menu_cmd::SETTINGS,
        key: Some(KeyEq { kvk: kvk::COMMA, mods: modifier::WIN }),
    },
    ItemSpec {
        name: "key-clicks",
        label: "Key Clicks",
        opcode: menu_cmd::KEY_CLICKS,
        key: None,
    },
];

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

/// Whether keypresses play a click (opt-in; off by default — quiet like a
/// native Mac editor until asked otherwise).
static KEY_CLICKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn key_clicks_enabled() -> bool {
    KEY_CLICKS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_key_clicks(on: bool) {
    KEY_CLICKS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Publish whether Save should be enabled (the active buffer is dirty).
/// Called from the worker; read by `validateMenuItem:` on the main thread
/// when the menu opens.
pub fn set_save_enabled(on: bool) {
    SAVE_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn save_enabled() -> bool {
    SAVE_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

// ── Open Recent store ─────────────────────────────────────────────────────

/// How many recents are kept.
pub const RECENTS_MAX: usize = 10;

/// The recents file: `~/Library/Application Support/MacNCL/recents.json`
/// (a plain JSON array of paths). `NCL_RECENTS_FILE` overrides the
/// location so tests stay hermetic.
fn recents_file() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("NCL_RECENTS_FILE") {
        return std::path::PathBuf::from(p);
    }
    let mut p = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
    );
    p.push("Library/Application Support/MacNCL/recents.json");
    p
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Load the recents list (missing/corrupt file → empty).
pub fn recent_paths() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(recents_file()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = text.trim().trim_start_matches('[');
    // Hand-parsed JSON string array — the file is ours and tiny; no
    // parser dependency needed.
    while let Some(q) = rest.strip_prefix('"') {
        let Some(end) = q.find('"') else { break };
        let mut path = String::new();
        let mut chars = q[..end].chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    path.push(n);
                }
            } else {
                path.push(c);
            }
        }
        out.push(path);
        rest = q[end + 1..].trim_start().trim_start_matches(',');
    }
    out
}

fn persist_recents(paths: &[String]) {
    let file = recents_file();
    let _ = std::fs::create_dir_all(file.parent().unwrap_or(std::path::Path::new("/")));
    let body: Vec<String> = paths.iter().map(|p| format!("\"{}\"", escape_json(p))).collect();
    let _ = std::fs::write(file, format!("[{}]\n", body.join(",")).as_bytes());
}

/// Record an opened file: front of the list, deduped, capped at
/// [`RECENTS_MAX`], persisted immediately.
pub fn record_recent(path: &str) {
    let mut paths = recent_paths();
    paths.retain(|p| p != path);
    paths.insert(0, path.to_string());
    paths.truncate(RECENTS_MAX);
    persist_recents(&paths);
    // The main thread rebuilds the submenu so the menu shows fresh data.
    #[cfg(feature = "mac-gui")]
    crate::igui_mac::window::request_recents_rebuild();
}

// ── Drag type filter ──────────────────────────────────────────────────────

/// File extensions the IDE accepts as drops/opens. Lowercase, no dot.
fn allowed_extensions() -> &'static [&'static str] {
    &["lisp", "lsp", "cl", "txt"]
}

/// Whether a path's extension makes it droppable/openable (AC: .lisp in,
/// .png out).
pub fn accepts_drop_path(path: &str) -> bool {
    let Some(ext) = path.rsplit('.').next() else { return false };
    ext.eq_ignore_ascii_case("lisp")
        || ext.eq_ignore_ascii_case("lsp")
        || ext.eq_ignore_ascii_case("cl")
        || ext.eq_ignore_ascii_case("txt")
}

/// Keep only the droppable paths from a drag, preserving order.
pub fn filter_drop_paths(paths: &[String]) -> Vec<String> {
    paths.iter().filter(|p| accepts_drop_path(p)).cloned().collect()
}

// ── Open/save panels (main thread) ────────────────────────────────────────

/// Suggested name for the save panel's name field (the active buffer's
/// file name). Published by the driver alongside the window title.
static SAVE_SUGGESTED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[cfg(feature = "mac-gui")]
pub fn set_save_suggested_name(name: &str) {
    *SAVE_SUGGESTED.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
}

#[cfg(feature = "mac-gui")]
fn save_suggested_name() -> String {
    let s = SAVE_SUGGESTED.lock().unwrap_or_else(|e| e.into_inner());
    if s.is_empty() { "untitled.lisp".into() } else { s.clone() }
}

/// The file-type restriction shared by both panels.
#[cfg(feature = "mac-gui")]
fn allowed_types_array(
    mtm: objc2::MainThreadMarker,
) -> objc2::rc::Retained<objc2_foundation::NSArray<objc2_foundation::NSString>> {
    use objc2_foundation::{NSArray, NSString};
    let items: Vec<objc2::rc::Retained<NSString>> =
        allowed_extensions().iter().map(|e| NSString::from_str(e)).collect();
    NSArray::from_retained_slice(&items)
}

/// Run the open panel (⌘O / File ▸ Open…). On OK, posts `IGuiEvent::Open`
/// with the chosen path. Main thread only — called from the menu
/// dispatcher's selector, which is where ⌘O lands via AppKit's own menu
/// matching.
#[cfg(feature = "mac-gui")]
pub fn run_open_panel() {
    use objc2_app_kit::{NSModalResponse, NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::NSString;

    let mtm = objc2::MainThreadMarker::new().expect("open panel on main thread");
    let panel = NSOpenPanel::new(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    #[allow(deprecated)]
    panel.setAllowedFileTypes(Some(&allowed_types_array(mtm)));
    if panel.runModal() == NSModalResponseOK {
        if let Some(path) = panel.URL().and_then(|u| u.path()) {
            crate::igui_events::push(crate::igui_events::IGuiEvent::Open {
                path: path.to_string(),
            });
        }
    }
}

/// Run the save panel (⇧⌘S / File ▸ Save As…). On OK, posts
/// `IGuiEvent::SaveAs` with the chosen path.
#[cfg(feature = "mac-gui")]
pub fn run_save_panel() {
    use objc2_app_kit::{NSModalResponse, NSModalResponseOK, NSSavePanel};
    use objc2_foundation::NSString;

    let mtm = objc2::MainThreadMarker::new().expect("save panel on main thread");
    let panel = NSSavePanel::new(mtm);
    panel.setNameFieldStringValue(&NSString::from_str(&save_suggested_name()));
    #[allow(deprecated)]
    panel.setAllowedFileTypes(Some(&allowed_types_array(mtm)));
    if panel.runModal() == NSModalResponseOK {
        if let Some(path) = panel.URL().and_then(|u| u.path()) {
            crate::igui_events::push(crate::igui_events::IGuiEvent::SaveAs {
                path: path.to_string(),
            });
        }
    }
}

// ── Open Recent submenu ───────────────────────────────────────────────────

/// The recents submenu, held for main-thread rebuilds. NSMenu is not
/// Send/Sync; the wrapper just makes it storable — it is only touched on
/// the main thread.
#[cfg(feature = "mac-gui")]
struct SyncMenuPtr(*mut objc2_app_kit::NSMenu);
#[cfg(feature = "mac-gui")]
unsafe impl Send for SyncMenuPtr {}
#[cfg(feature = "mac-gui")]
unsafe impl Sync for SyncMenuPtr {}

#[cfg(feature = "mac-gui")]
static RECENTS_MENU: std::sync::OnceLock<SyncMenuPtr> = std::sync::OnceLock::new();

/// Rebuild the Open Recent submenu from the store. Main thread; the
/// worker asks for this via `window::request_recents_rebuild` whenever
/// `record_recent` changes the list.
#[cfg(feature = "mac-gui")]
pub(crate) fn apply_recents_rebuild() {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSMenu, NSMenuItem};
    use objc2_foundation::NSString;

    let mtm = objc2::MainThreadMarker::new().expect("recents rebuild on main thread");
    let Some(&SyncMenuPtr(ptr)) = RECENTS_MENU.get() else { return };
    // SAFETY: the submenu is alive for the process lifetime (held by the
    // menu bar) and confined to the main thread.
    let menu: &NSMenu = unsafe { &*ptr };
    unsafe { menu.removeAllItems() };
    let dispatcher = dispatcher_instance();
    let target: &objc2::runtime::AnyObject = unsafe {
        &*(objc2::rc::Retained::as_ptr(dispatcher)
            as *const objc2::runtime::AnyObject)
    };
    let paths = recent_paths();
    if paths.is_empty() {
        let it = NSMenuItem::new(mtm);
        it.setTitle(&NSString::from_str("No Recents"));
        it.setEnabled(false);
        unsafe { menu.addItem(&it) };
        return;
    }
    for p in &paths {
        let it = NSMenuItem::new(mtm);
        let name = p.rsplit('/').next().unwrap_or(p);
        it.setTitle(&NSString::from_str(&format!("{name} — {}", parent_of(p))));
        // SAFETY: setter with a plain string value.
        unsafe { it.setRepresentedObject(Some(&NSString::from_str(p))) };
        // SAFETY: dispatcher outlives the menu; known selector.
        unsafe {
            it.setTarget(Some(target));
            it.setAction(Some(objc2::sel!(nclOpenRecent:)));
        }
        unsafe { menu.addItem(&it) };
    }
}

#[cfg(feature = "mac-gui")]
fn parent_of(p: &str) -> &str {
    match p.rsplit_once('/') {
        Some((d, _)) if !d.is_empty() => d,
        _ => "/",
    }
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

    let cmd_item = |label: &str, key: Option<&KeyEq>, opcode: i64| -> Retained<NSMenuItem> {
        let it = NSMenuItem::new(mtm);
        it.setTitle(&NSString::from_str(label));
        if let Some(k) = key {
            let (eq, mask) = ns_key_equivalent(k);
            it.setKeyEquivalent(&NSString::from_str(&eq));
            it.setKeyEquivalentModifierMask(mask);
        }
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
        let it = cmd_item(spec.label, spec.key.as_ref(), spec.opcode);
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
        if spec.title == "File" {
            // File is built manually: it interleaves the registry items
            // with the panel-driven Open… / Save As… and the recents
            // submenu (those act on the main thread, not via opcodes).
            let dispatcher_item = |label: &str, key: &str, mask: MF, sel: objc2::runtime::Sel| {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str(label));
                if !key.is_empty() {
                    it.setKeyEquivalent(&NSString::from_str(key));
                    it.setKeyEquivalentModifierMask(mask);
                }
                // SAFETY: dispatcher target outlives the menu.
                unsafe {
                    it.setTarget(Some(target));
                    it.setAction(Some(sel));
                }
                it
            };
            // New (⌘N)
            let it = cmd_item(spec.items[0].label, spec.items[0].key.as_ref(), spec.items[0].opcode);
            m.addItem(&it);
            m.addItem(&dispatcher_item("Open…", "o", MF::Command, objc2::sel!(nclOpenDocument:)));
            // Open Recent ▸ (rebuilt from the store; see apply_recents_rebuild)
            let recents = NSMenu::new(mtm);
            let _ = RECENTS_MENU.set(SyncMenuPtr(Retained::as_ptr(&recents) as *mut NSMenu));
            let recents_root = NSMenuItem::new(mtm);
            recents_root.setTitle(&NSString::from_str("Open Recent"));
            recents_root.setSubmenu(Some(&recents));
            m.addItem(&recents_root);
            // Examples ▸ — the shipped demos (bundle Resources/Lisp/demos,
            // or the repo's in dev), listed name-sorted. Picking one opens
            // it as a buffer (⌘R runs it).
            let examples = NSMenu::new(mtm);
            let ex_files = crate::igui_mac::paths::example_files(
                &crate::igui_mac::paths::lisp_dir().unwrap_or_default(),
            );
            if ex_files.is_empty() {
                let it = NSMenuItem::new(mtm);
                it.setTitle(&NSString::from_str("No Examples Found"));
                it.setEnabled(false);
                examples.addItem(&it);
            } else {
                for (name, path) in ex_files {
                    let it = NSMenuItem::new(mtm);
                    it.setTitle(&NSString::from_str(&name));
                    // SAFETY: plain string value.
                    unsafe { it.setRepresentedObject(Some(&NSString::from_str(&path))) };
                    // SAFETY: dispatcher outlives the menu; known selector.
                    unsafe {
                        it.setTarget(Some(target));
                        it.setAction(Some(objc2::sel!(nclOpenExample:)));
                    }
                    examples.addItem(&it);
                }
            }
            let examples_root = NSMenuItem::new(mtm);
            examples_root.setTitle(&NSString::from_str("Examples"));
            examples_root.setSubmenu(Some(&examples));
            m.addItem(&examples_root);
            // Close Tab (⌘W), Save (⌘S) from the registry.
            let it = cmd_item(spec.items[1].label, spec.items[1].key.as_ref(), spec.items[1].opcode);
            m.addItem(&it);
            let it = cmd_item(spec.items[2].label, spec.items[2].key.as_ref(), spec.items[2].opcode);
            m.addItem(&it);
            m.addItem(&dispatcher_item(
                "Save As…", "S", MF::Command | MF::Shift, objc2::sel!(nclSaveDocumentAs:),
            ));
        } else {
            for it in spec.items {
                let item = cmd_item(it.label, it.key.as_ref(), it.opcode);
                m.addItem(&item);
            }
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
    apply_recents_rebuild();
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
            if tag as i64 == menu_cmd::KEY_CLICKS {
                // Handled entirely on the main thread (state + checkmark).
                let on = !key_clicks_enabled();
                set_key_clicks(on);
                // SAFETY: setState with a NSControlStateValue on the item.
                let state: isize = if on { 1 } else { 0 };
                let _: () = unsafe { objc2::msg_send![item, setState: state] };
                return;
            }
            crate::igui_events::push(crate::igui_events::IGuiEvent::Menu {
                menu_id: menu_cmd::IDE,
                item_id: tag as i64,
            });
        }

        /// File ▸ Open… — the panel must run on this (main) thread, so it
        /// is driven by the selector rather than an opcode round-trip.
        #[unsafe(method(nclOpenDocument:))]
        fn open_document(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            run_open_panel();
        }

        /// File ▸ Save As… (⇧⌘S).
        #[unsafe(method(nclSaveDocumentAs:))]
        fn save_document_as(&self, _sender: Option<&objc2::runtime::AnyObject>) {
            run_save_panel();
        }

        /// Open Recent ▸ / Examples ▸ — the item carries the full path in
        /// its representedObject.
        #[unsafe(method(nclOpenRecent:))]
        fn open_recent(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(item) = sender else { return };
            let path: Option<objc2::rc::Retained<objc2_foundation::NSString>> =
                unsafe { objc2::msg_send![item, representedObject] };
            if let Some(p) = path {
                crate::igui_events::push(crate::igui_events::IGuiEvent::Open {
                    path: p.to_string(),
                });
            }
        }

        /// Examples ▸ pick (same shape as recents).
        #[unsafe(method(nclOpenExample:))]
        fn open_example(&self, sender: Option<&objc2::runtime::AnyObject>) {
            let Some(item) = sender else { return };
            let path: Option<objc2::rc::Retained<objc2_foundation::NSString>> =
                unsafe { objc2::msg_send![item, representedObject] };
            if let Some(p) = path {
                crate::igui_events::push(crate::igui_events::IGuiEvent::Open {
                    path: p.to_string(),
                });
            }
        }

        #[unsafe(method(validatesMenuItem:))]
        fn validate(&self, sender: Option<&objc2::runtime::AnyObject>) -> objc2::runtime::Bool {
            let Some(item) = sender else { return true.into() };
            let tag: isize = unsafe { objc2::msg_send![item, tag] };
            if tag as i64 == menu_cmd::KEY_CLICKS {
                // Keep the checkmark in sync with the shared state.
                // SAFETY: setState with a NSControlStateValue.
                let state: isize = if key_clicks_enabled() { 1 } else { 0 };
                let _: () = unsafe { objc2::msg_send![item, setState: state] };
                return true.into();
            }
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
            menu_cmd::KEY_CLICKS,
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
    fn key_clicks_flag_flips() {
        let before = key_clicks_enabled();
        set_key_clicks(!before);
        assert_eq!(key_clicks_enabled(), !before);
        set_key_clicks(before);
    }

    #[test]
    fn save_enabled_flag_flips() {
        let before = save_enabled();
        set_save_enabled(!before);
        assert_eq!(save_enabled(), !before);
        set_save_enabled(before);
    }

    // ── Sprint 5: recents + drag filter ────────────────────────────────

    /// Point the recents store at a fresh temp file (tests stay hermetic;
    /// the suite runs single-threaded on this target).
    fn isolate_recents() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ncl_recents_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        // SAFETY: test-only env mutation; suite is single-threaded here.
        unsafe { std::env::set_var("NCL_RECENTS_FILE", &p) };
        p
    }

    /// Ten distinct opens fill the list most-recent-first; an eleventh
    /// evicts the oldest; the list persists across a "relaunch" (re-read).
    #[test]
    fn recents_update_and_cap() {
        let file = isolate_recents();
        for i in 0..11 {
            record_recent(&format!("/tmp/f{i}.lisp"));
        }
        let paths = recent_paths();
        assert_eq!(paths.len(), RECENTS_MAX, "capped at {}", RECENTS_MAX);
        assert_eq!(paths[0], "/tmp/f10.lisp", "most recent first");
        assert!(
            !paths.contains(&"/tmp/f0.lisp".to_string()),
            "oldest evicted: {paths:?}"
        );
        // Re-opening an existing path moves it to the front, no dupes.
        record_recent("/tmp/f3.lisp");
        let paths = recent_paths();
        assert_eq!(paths[0], "/tmp/f3.lisp");
        assert_eq!(paths.iter().filter(|p| p.as_str() == "/tmp/f3.lisp").count(), 1);
        // Persists: a fresh read (new "launch") sees the same list.
        assert_eq!(recent_paths(), paths);
        let _ = std::fs::remove_file(file);
    }

    /// The drag/open type filter: Lisp sources in, anything else out.
    #[test]
    fn drag_types_filter() {
        assert!(accepts_drop_path("/a/b/code.lisp"));
        assert!(accepts_drop_path("/a/b/CODE.LSP"));
        assert!(accepts_drop_path("/a/b/notes.cl"));
        assert!(accepts_drop_path("/a/b/notes.txt"));
        assert!(!accepts_drop_path("/a/pic.png"));
        assert!(!accepts_drop_path("/a/binary"));
        let kept = filter_drop_paths(&[
            "/x/a.lisp".into(),
            "/x/b.png".into(),
            "/x/c.lsp".into(),
        ]);
        assert_eq!(kept, vec!["/x/a.lisp".to_string(), "/x/c.lsp".to_string()]);
    }
}
