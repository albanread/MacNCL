//! Cocoa windowing for iGui (feature `mac-gui`) — **multi-window**.
//!
//! macOS requires all AppKit work on the main thread, while NCL runs Lisp
//! on a worker thread. So the worker never touches `NSWindow` directly: it
//! posts typed [`UiCmd`]s (open / close / set-title) onto a queue and calls
//! [`present`] with a `SurfaceCmd` batch per window id. A main-thread timer
//! drains the queue (creating/closing real windows via [`WindowManager`])
//! and repaints any window whose batch changed. A local `NSEvent` monitor
//! tags each event with the id of the window it came from and pushes it
//! into the shared mailbox (`crate::igui_events`).
//!
//! This is the side-window model: every `open-child` from Lisp becomes its
//! own `NSWindow`. The IDE is just window id 1.
//!
//! Build/run with `--features mac-gui`. Needs a logged-in GUI session.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use block2::RcBlock;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSEventMask,
    NSEventType, NSImage, NSImageView, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::igui_events;
use crate::igui_mac::events as ev;
use crate::igui_mac::render::CgCanvas;
use crate::igui_paint::SurfaceCmd;

/// The IDE / main window's id.
pub const MAIN_ID: i64 = 1;

// ── Worker → main-thread command queue ────────────────────────────────

enum UiCmd {
    Open { id: i64, w: f64, h: f64, title: String },
    Close { id: i64 },
    Title { id: i64, title: String },
}

fn cmd_queue() -> &'static Mutex<VecDeque<UiCmd>> {
    static Q: OnceLock<Mutex<VecDeque<UiCmd>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn post(cmd: UiCmd) {
    cmd_queue().lock().unwrap_or_else(|e| e.into_inner()).push_back(cmd);
}

/// Open a side window (worker-callable). The window appears on the next
/// main-thread tick. Idempotent per id.
pub fn open_window(id: i64, w: f64, h: f64, title: &str) {
    post(UiCmd::Open { id, w, h, title: title.to_string() });
}
pub fn close_window(id: i64) {
    post(UiCmd::Close { id });
}
pub fn set_window_title(id: i64, title: &str) {
    post(UiCmd::Title { id, title: title.to_string() });
}

// ── Per-window batch store ────────────────────────────────────────────

fn batches() -> &'static Mutex<HashMap<i64, Arc<Vec<SurfaceCmd>>>> {
    static B: OnceLock<Mutex<HashMap<i64, Arc<Vec<SurfaceCmd>>>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}
fn dirty() -> &'static Mutex<HashSet<i64>> {
    static D: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Present a `SurfaceCmd` batch to window `id` (worker-callable). The
/// main-thread timer renders it on the next tick.
pub fn present(id: i64, cmds: Vec<SurfaceCmd>) {
    batches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, Arc::new(cmds));
    dirty().lock().unwrap_or_else(|e| e.into_inner()).insert(id);
}

/// Present to the main (IDE) window.
pub fn present_main(cmds: Vec<SurfaceCmd>) {
    present(MAIN_ID, cmds);
}

fn take_batch(id: i64) -> Option<Arc<Vec<SurfaceCmd>>> {
    batches().lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
}

// ── Main-thread window registry ───────────────────────────────────────

struct WinEntry {
    window: Retained<NSWindow>,
    view: Retained<NSImageView>,
    w: f64,
    h: f64,
    /// `NSWindow.windowNumber` — a stable per-window integer used to route
    /// `NSEvent`s to the right child (robust integer compare).
    num: isize,
}

struct WindowManager {
    wins: HashMap<i64, WinEntry>,
    mtm: MainThreadMarker,
}

impl WindowManager {
    fn new(mtm: MainThreadMarker) -> Self {
        Self { wins: HashMap::new(), mtm }
    }

    fn open(&mut self, id: i64, w: f64, h: f64, title: &str) {
        if self.wins.contains_key(&id) {
            return;
        }
        let mtm = self.mtm;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str(title));
        window.center();
        let view = NSImageView::new(mtm);
        window.setContentView(Some(&view));
        window.makeKeyAndOrderFront(None);
        let num = window.windowNumber();
        self.wins.insert(id, WinEntry { window, view, w, h, num });
        // Show any batch already presented for this id.
        dirty().lock().unwrap_or_else(|e| e.into_inner()).insert(id);
    }

    fn close(&mut self, id: i64) {
        if let Some(e) = self.wins.remove(&id) {
            e.window.close();
        }
    }

    fn set_title(&mut self, id: i64, title: &str) {
        if let Some(e) = self.wins.get(&id) {
            e.window.setTitle(&NSString::from_str(title));
        }
    }

    /// id of the window an event belongs to, by `NSEvent.windowNumber`.
    /// Falls back to the key window, then MAIN_ID.
    fn id_for_event(&self, e: &NSEvent) -> i64 {
        let num = e.windowNumber();
        for (id, entry) in &self.wins {
            if entry.num == num {
                return *id;
            }
        }
        // Fallback: the key (focused) window — for a click that's the one
        // just clicked.
        for (id, entry) in &self.wins {
            if entry.window.isKeyWindow() {
                return *id;
            }
        }
        MAIN_ID
    }

    fn height_of(&self, id: i64) -> f64 {
        self.wins.get(&id).map(|e| e.h).unwrap_or(0.0)
    }

    fn repaint(&self, id: i64) {
        let Some(entry) = self.wins.get(&id) else { return };
        let Some(cmds) = take_batch(id) else { return };
        let scale = entry
            .view
            .window()
            .map(|win| win.backingScaleFactor())
            .filter(|s| *s >= 1.0)
            .unwrap_or(2.0);
        let mut canvas = CgCanvas::new_scaled(entry.w as usize, entry.h as usize, scale);
        canvas.execute(&cmds);
        // Debug frame dumps: main window → NCL_IGUI_DUMP, any other → NCL_IGUI_DUMP2.
        let dump_var = if id == MAIN_ID { "NCL_IGUI_DUMP" } else { "NCL_IGUI_DUMP2" };
        if let Some(path) = std::env::var_os(dump_var) {
            let _ = std::fs::write(path, canvas.to_ppm());
        }
        let Some(img) = canvas.cg_image() else { return };
        let cg_ref = img.as_ptr();
        // SAFETY: live CGImageRef; initWithCGImage_size retains it.
        let objc_img: &CGImage = unsafe { &*(cg_ref as *const CGImage) };
        let ns_image = NSImage::initWithCGImage_size(
            self.mtm.alloc::<NSImage>(),
            objc_img,
            NSSize::new(entry.w, entry.h),
        );
        entry.view.setImage(Some(&ns_image));
    }

    fn drain_commands(&mut self) {
        let cmds: Vec<UiCmd> =
            cmd_queue().lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        for c in cmds {
            match c {
                UiCmd::Open { id, w, h, title } => self.open(id, w, h, &title),
                UiCmd::Close { id } => self.close(id),
                UiCmd::Title { id, title } => self.set_title(id, &title),
            }
        }
    }

    fn repaint_dirty(&self) {
        let ids: Vec<i64> = {
            let mut d = dirty().lock().unwrap_or_else(|e| e.into_inner());
            d.drain().collect()
        };
        for id in ids {
            self.repaint(id);
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────

/// Open the main (IDE) window and run the AppKit event loop on the calling
/// (main) thread, with `worker` running the Lisp side on a background
/// thread. Returns when the app terminates.
pub fn run<F>(title: &str, width: f64, height: f64, worker: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "igui_mac::run must be called on the main thread".to_string())?;

    igui_events::install();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let manager = Rc::new(RefCell::new(WindowManager::new(mtm)));
    manager.borrow_mut().open(MAIN_ID, width, height, title);

    // Repaint + command-drain timer (~60 Hz) on the main thread.
    let mgr_t = Rc::clone(&manager);
    let tick = RcBlock::new(move |_t: NonNull<NSTimer>| {
        // Drain commands (creates/closes windows), then repaint dirty ones.
        mgr_t.borrow_mut().drain_commands();
        mgr_t.borrow().repaint_dirty();
    });
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &tick)
    };

    // Event monitor: tag each event with its window's id.
    let mgr_e = Rc::clone(&manager);
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let e = unsafe { event.as_ref() };
        let (id, h) = {
            let m = mgr_e.borrow();
            let id = m.id_for_event(e);
            (id, m.height_of(id))
        };
        dispatch_event(e, id, h);
        event.as_ptr()
    });
    let mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDragged
        | NSEventMask::ScrollWheel;
    let _monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) };

    app.activate();

    std::thread::Builder::new()
        .name("ncl-lisp-worker".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(worker)
        .map_err(|e| format!("failed to spawn Lisp worker: {e}"))?;

    app.run();
    Ok(())
}

/// Translate one `NSEvent` for window `child_id` and push the resulting
/// `IGuiEvent`(s). `view_height` is that window's content height (for the
/// y-flip).
fn dispatch_event(e: &NSEvent, child_id: i64, view_height: f64) {
    let flags = e.modifierFlags().0 as u64;
    let t = e.r#type();

    if t == NSEventType::KeyDown || t == NSEventType::KeyUp {
        let down = t == NSEventType::KeyDown;
        let keycode = e.keyCode();
        let ch = e
            .charactersIgnoringModifiers()
            .and_then(|s| s.to_string().chars().next());
        igui_events::push(ev::key_event(child_id, keycode, ch, flags, e.isARepeat(), down, 0));
        if down {
            if let Some(c) = ch {
                if !c.is_control() {
                    igui_events::push(ev::char_event(child_id, c as u32, flags, 0));
                }
            }
        }
        return;
    }

    let mouse = |op: i64, button: i64| {
        let p: NSPoint = e.locationInWindow();
        let y = ev::to_top_left_y(p.y, view_height);
        if std::env::var_os("NCL_GUI_DEBUG").is_some() {
            eprintln!("[mac-evt] mouse child={child_id} op={op} x={} y={}", p.x as i64, y as i64);
        }
        igui_events::push(ev::mouse_event(child_id, p.x, y, op, button, flags, 0, 0, 0));
    };

    if t == NSEventType::LeftMouseDown {
        mouse(ev::ns_mouse::left_down(), 0);
    } else if t == NSEventType::LeftMouseUp {
        mouse(ev::ns_mouse::left_up(), 0);
    } else if t == NSEventType::RightMouseDown {
        mouse(ev::ns_mouse::right_down(), 1);
    } else if t == NSEventType::RightMouseUp {
        mouse(ev::ns_mouse::right_up(), 1);
    } else if t == NSEventType::MouseMoved {
        mouse(ev::ns_mouse::moved(), 0);
    } else if t == NSEventType::LeftMouseDragged {
        mouse(crate::igui_events::mouse_op::DRAG, 0);
    } else if t == NSEventType::ScrollWheel {
        let p: NSPoint = e.locationInWindow();
        let y = ev::to_top_left_y(p.y, view_height);
        let dy = e.scrollingDeltaY();
        igui_events::push(ev::mouse_event(
            child_id,
            p.x,
            y,
            ev::ns_mouse::wheel(),
            0,
            flags,
            dy as i64,
            dy as i64,
            0,
        ));
    }
}
