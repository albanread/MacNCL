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

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use block2::RcBlock;
use foreign_types::ForeignType;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSEvent, NSEventMask,
    NSEventType, NSImage, NSImageScaling, NSImageView, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::igui_events::{self, menu_cmd, modifier, IGuiEvent};
use crate::igui_mac::events as ev;
use crate::igui_mac::menu;
use crate::igui_mac::render::CgCanvas;
use crate::igui_mac::theme;
use crate::igui_paint::SurfaceCmd;

/// ── System font resolution (AppKit side) ────────────────────────────────
///
/// SF Mono (and the system font's named weights) aren't reachable by
/// CoreText name lookup, so this resolver goes through `NSFont` and hands
/// the toll-free-bridged `CTFont` pointer to the renderer. Falls back to
/// `None` (→ render.rs's own Menlo/Helvetica chain) for unknown families.
#[cfg(feature = "mac-gui")]
fn system_font_resolver(
    family: &str,
    size: f32,
) -> Option<core_text::font::CTFont> {
    use core_foundation::base::TCFType;
    use objc2::rc::Retained;
    use objc2_app_kit::{NSFont, NSFontWeightRegular};

    let ns: Retained<NSFont> = match family {
        "__system" => NSFont::systemFontOfSize(size as f64),
        "__system-bold" => NSFont::boldSystemFontOfSize(size as f64),
        "__mono" => {
            // SAFETY: reading the extern weight constant (a plain CGFloat).
            let w = unsafe { NSFontWeightRegular };
            NSFont::monospacedSystemFontOfSize_weight(size as f64, w)
        }
        _ => return None,
    };
    // NSFont and CTFont are the same object; `wrap_under_get_rule`
    // retains it for the core-text wrapper.
    let ptr = Retained::as_ptr(&ns) as core_text::font::CTFontRef;
    unsafe { Some(core_text::font::CTFont::wrap_under_get_rule(ptr)) }
}

/// The IDE / main window's id.
pub const MAIN_ID: i64 = 1;

// ── Worker → main-thread command queue ────────────────────────────────

enum UiCmd {
    Open { id: i64, w: f64, h: f64, title: String },
    Close { id: i64 },
    Title { id: i64, title: String },
    Subtitle { id: i64, subtitle: String },
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

/// Set the window's subtitle (shown in window lists; the IDE uses it for
/// the active buffer's directory). Worker-callable.
pub fn set_window_subtitle(id: i64, subtitle: &str) {
    post(UiCmd::Subtitle { id, subtitle: subtitle.to_string() });
}

// ── Per-window redraw timers (`set-redraw-rate`) ───────────────────────
//
// `(set-redraw-rate id ms)` runs a thread that posts a :TICK for `id` every
// `ms`. Each thread is governed by an `AtomicBool` flag so it stops when the
// window closes — otherwise it leaks (spins forever posting ticks nothing
// consumes). Lifecycle is owned here, beside open/close, so every close path
// (the `close-child` command AND the title-bar close box detected by
// `sweep_closed`) tears the timer down.

fn redraw_flags() -> &'static Mutex<HashMap<i64, Arc<AtomicBool>>> {
    static R: OnceLock<Mutex<HashMap<i64, Arc<AtomicBool>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start (or restart) a `ms`-period redraw timer for window `id`. `ms <= 0`
/// stops any existing timer. Worker-callable.
pub fn set_redraw_rate(id: i64, ms: i64) {
    if ms <= 0 {
        stop_redraw(id);
        return;
    }
    let flag = Arc::new(AtomicBool::new(true));
    {
        let mut m = redraw_flags().lock().unwrap_or_else(|e| e.into_inner());
        // Replace any prior timer for this id, signalling the old one to exit.
        if let Some(old) = m.insert(id, Arc::clone(&flag)) {
            old.store(false, Ordering::Relaxed);
        }
    }
    let dur = Duration::from_millis(ms as u64);
    std::thread::Builder::new()
        .name(format!("igui-tick-{id}"))
        .spawn(move || {
            while flag.load(Ordering::Relaxed) {
                std::thread::sleep(dur);
                if !flag.load(Ordering::Relaxed) {
                    break;
                }
                igui_events::push(igui_events::IGuiEvent::Tick { child_id: id, time_ms: 0 });
            }
        })
        .ok();
}

/// Stop window `id`'s redraw timer, if any. Called on every window-close path.
fn stop_redraw(id: i64) {
    if let Some(flag) = redraw_flags()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    {
        flag.store(false, Ordering::Relaxed);
    }
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
        let is_main = id == MAIN_ID;
        let mut style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // The IDE draws its own chrome right up to the top of the window
        // (tab strip under the traffic lights, Xcode-style). Child windows
        // keep the standard title bar — they draw arbitrary content that
        // must not slide under the lights.
        if is_main {
            style |= NSWindowStyleMask::FullSizeContentView;
        }
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
        if is_main {
            // Transparent title bar: the full-size content shows through
            // where the (now invisible) title bar would be; only the
            // traffic lights remain, floating over the tab strip.
            unsafe { window.setTitlebarAppearsTransparent(true) };
            // Remember position/size across launches. Setting the name
            // restores a previously saved frame and returns false when
            // there is none — only then do we centre a first launch.
            let restored =
                unsafe { window.setFrameAutosaveName(&NSString::from_str("MacNCL-IDE")) };
            if !restored {
                window.center();
            }
            window.setMinSize(NSSize::new(720.0, 480.0));
        }
        // CRITICAL: programmatically created NSWindows default to
        // `releasedWhenClosed = YES`, so AppKit releases the window when it
        // closes. We also hold a `Retained` to it in `WinEntry`, so that
        // default is an over-release: closing the window (close box OR
        // `close()`) frees it out from under our pointer, and the next
        // `id_for_event` sweep calls `isKeyWindow` on freed memory →
        // EXC_BREAKPOINT in object_getClass. Owning the lifetime ourselves
        // (release only when we drop the entry) fixes it.
        unsafe { window.setReleasedWhenClosed(false) };
        let view = NSImageView::new(mtm);
        // Stretch the current frame to fill the view during a live resize, so
        // the window content tracks the drag smoothly until the owner
        // re-renders at the new size (see `sync_window_sizes`). In the steady
        // state the image is exactly the view size, so this is a 1:1 blit.
        view.setImageScaling(NSImageScaling::ScaleAxesIndependently);
        window.setContentView(Some(&view));
        window.makeKeyAndOrderFront(None);
        let num = window.windowNumber();
        self.wins.insert(id, WinEntry { window, view, w, h, num });
        // Show any batch already presented for this id.
        dirty().lock().unwrap_or_else(|e| e.into_inner()).insert(id);
    }

    /// Sweep windows the user closed via the title-bar close box (which, with
    /// `releasedWhenClosed = NO`, just orders the window out — it stays in
    /// `wins`). Returns the ids removed so the caller can emit `:close`
    /// events. macOS has no NSEvent for "window closed", so we detect it
    /// here: a window that is neither visible nor miniaturized has been
    /// closed. Skipped entirely while the whole app is hidden (Cmd-H), since
    /// then every window reports not-visible without being closed.
    fn sweep_closed(&mut self, app_hidden: bool) -> Vec<i64> {
        if app_hidden {
            return Vec::new();
        }
        let mut closed = Vec::new();
        self.wins.retain(|id, entry| {
            let alive = entry.window.isVisible() || entry.window.isMiniaturized();
            if !alive {
                stop_redraw(*id);
                closed.push(*id);
            }
            alive
        });
        closed
    }

    fn close(&mut self, id: i64) {
        if let Some(e) = self.wins.remove(&id) {
            stop_redraw(id);
            e.window.close();
        }
    }

    fn set_title(&mut self, id: i64, title: &str) {
        if let Some(e) = self.wins.get(&id) {
            e.window.setTitle(&NSString::from_str(title));
        }
    }

    fn set_subtitle(&mut self, id: i64, subtitle: &str) {
        if let Some(e) = self.wins.get(&id) {
            e.window.setSubtitle(&NSString::from_str(subtitle));
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

    fn main_size(&self) -> Option<(f64, f64)> {
        self.wins.get(&MAIN_ID).map(|e| (e.w, e.h))
    }

    /// Whether the IDE window currently exists and is the key window
    /// (None if the window isn't open yet).
    fn main_is_key(&self) -> Option<bool> {
        self.wins.get(&MAIN_ID).map(|e| e.window.isKeyWindow())
    }

    /// Detect window resizes and react. Our NSEvent monitor only sees
    /// key/mouse, so a window-content resize never reaches the mailbox on its
    /// own; we poll the content-view size each main-thread tick instead. When
    /// it changes we update the cached size (so the next repaint rasterises at
    /// the new resolution) and push a `Resize` event so the owner — the IDE
    /// (window 1) or a Lisp `(on-window …)` handler — re-lays-out and repaints.
    fn sync_window_sizes(&mut self) {
        let mut resized: Vec<(i64, f64, f64)> = Vec::new();
        for (id, entry) in self.wins.iter_mut() {
            let sz = entry.view.frame().size;
            if sz.width >= 1.0
                && sz.height >= 1.0
                && ((sz.width - entry.w).abs() > 0.5 || (sz.height - entry.h).abs() > 0.5)
            {
                entry.w = sz.width;
                entry.h = sz.height;
                resized.push((*id, sz.width, sz.height));
            }
        }
        for (id, w, h) in resized {
            igui_events::push(igui_events::IGuiEvent::Resize {
                child_id: id,
                width: w as i64,
                height: h as i64,
            });
        }
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
                UiCmd::Subtitle { id, subtitle } => self.set_subtitle(id, &subtitle),
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

/// Build the startup loading-screen batch (a centred progress bar over a dark
/// field). Painted by the main-thread timer while the Lisp worker is busy
/// JIT-compiling the stdlib and can't repaint itself. See `crate::load_progress`.
fn loading_batch(w: f32, h: f32, done: u32, total: u32, phase: &str) -> Vec<SurfaceCmd> {
    use crate::igui_paint::{
        FontStretch, FontStyle, Point, Rect, Rgba, TextAlign, TextRun, TextTrimming,
    };
    let bg = Rgba { r: 0.08, g: 0.09, b: 0.12, a: 1.0 };
    let track = Rgba { r: 0.18, g: 0.20, b: 0.26, a: 1.0 };
    let fill = Rgba { r: 0.47, g: 0.78, b: 1.0, a: 1.0 };
    let fg = Rgba { r: 0.85, g: 0.88, b: 0.95, a: 1.0 };
    let dim = Rgba { r: 0.50, g: 0.55, b: 0.62, a: 1.0 };
    let frac = (done as f32 / total.max(1) as f32).clamp(0.0, 1.0);
    let bw = (w * 0.6).min(420.0);
    let bx = (w - bw) * 0.5;
    let by = (h * 0.5).round();
    let bh = 12.0;
    let text = |s: String, x: f32, y: f32, size: f32, color: Rgba| SurfaceCmd::DrawTextRun {
        run: TextRun {
            text: s,
            origin: Point { x, y },
            family: "Menlo".into(),
            size,
            weight: 500,
            style: FontStyle::Normal,
            stretch: FontStretch::Normal,
            locale: "en-us".into(),
            color,
            max_width: None,
            alignment: TextAlign::Leading,
            trimming: TextTrimming::None,
        },
    };
    let mut cmds = vec![
        SurfaceCmd::Clear { color: bg },
        text("Loading NCL…".into(), bx, by - 40.0, 20.0, fg),
        SurfaceCmd::FillRect {
            rect: Rect { x0: bx, y0: by, x1: bx + bw, y1: by + bh },
            corner_radius: 6.0,
            color: track,
        },
        SurfaceCmd::FillRect {
            rect: Rect { x0: bx, y0: by, x1: bx + bw * frac, y1: by + bh },
            corner_radius: 6.0,
            color: fill,
        },
    ];
    let label = if phase.is_empty() {
        format!("{done} / {total}")
    } else {
        format!("{phase}   {done} / {total}")
    };
    cmds.push(text(label, bx, by + bh + 10.0, 13.0, dim));
    cmds
}

/// Open the main (IDE) window and run the AppKit event loop on the calling
/// (main) thread, with `worker` running the Lisp side on a background
/// thread. Returns when the app terminates.
pub fn run<F>(title: &str, width: f64, height: f64, worker: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    run_inner(Some((title.to_string(), width, height)), false, worker)
}

/// Headless app mode (`ncl --run-window`): run the AppKit event loop WITHOUT
/// the IDE main window. The Lisp worker opens its own window(s) via
/// `open-child`; the process quits when the last window closes. Lets Lisp ship
/// standalone GUI apps with no REPL/editor chrome.
pub fn run_app<F>(worker: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    run_inner(None, true, worker)
}

/// Shared AppKit driver. `main_window` = Some((title, w, h)) opens the IDE
/// window (id 1); None runs headless. `quit_on_last_close` terminates the app
/// once every window has closed (used by the headless `--run-window` mode so a
/// standalone app exits when the user closes its window).
fn run_inner<F>(
    main_window: Option<(String, f64, f64)>,
    quit_on_last_close: bool,
    worker: F,
) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "igui_mac::run must be called on the main thread".to_string())?;

    igui_events::install();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // The system menu bar replaces the old in-window menu: real ⌘-glyph
    // items whose picks arrive as `IGuiEvent::Menu` (see `igui_mac::menu`).
    menu::install(&app, mtm);
    // Resolve the semantic theme once up front (appearance + accent) so
    // the worker's very first batch already uses system colors.
    theme::refresh(&app);
    // Give the renderer access to the private system faces (SF Mono has no
    // CoreText name — see `render::register_font_resolver`). NSFont and
    // CTFont are toll-free bridged: retain + hand the pointer across.
    crate::igui_mac::render::register_font_resolver(system_font_resolver);

    let manager = Rc::new(RefCell::new(WindowManager::new(mtm)));
    if let Some((title, w, h)) = &main_window {
        manager.borrow_mut().open(MAIN_ID, *w, *h, title);
    }

    // Tracks whether any window has ever been opened — so `quit_on_last_close`
    // doesn't terminate during the worker's (windowless) startup, only after a
    // real window has come and gone.
    let had_window = Rc::new(Cell::new(false));

    // Repaint + command-drain timer (~60 Hz) on the main thread.
    // Also polls the `NCL_GUI_QUIT_AFTER_MS` flag so lifecycle tests can
    // quit the app cleanly without a human pressing ⌘Q.
    static QUIT_REQUESTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    if let Some(ms) = std::env::var_os("NCL_GUI_QUIT_AFTER_MS") {
        if let Ok(ms) = ms.to_string_lossy().parse::<u64>() {
            std::thread::Builder::new()
                .name("ncl-gui-quit-timer".into())
                .spawn(move || {
                    std::thread::sleep(Duration::from_millis(ms));
                    QUIT_REQUESTED.store(true, Ordering::Relaxed);
                })
                .ok();
        }
    }
    let mgr_t = Rc::clone(&manager);
    let app_t = app.clone();
    let had_t = Rc::clone(&had_window);
    // Last progress value painted, so the loading bar only re-renders on a real
    // advance (not 60×/s). u32::MAX forces the first frame.
    let last_done = Rc::new(Cell::new(u32::MAX));
    // Last-seen key state of the IDE window, so a change (the user focused
    // another app/window) reaches the worker as a Focus event.
    let last_main_key = Rc::new(Cell::new(None::<bool>));
    // Handle to the Lisp worker, so the quit path can join it before
    // terminating. Exiting while the worker is mid-JIT-compile segfaults
    // on Apple Silicon (the W^X write-protect toggle is per-thread), so
    // ⌘Q/`NCL_GUI_QUIT_AFTER_MS` must let the worker reach a quiet point
    // first. It stops when its central loop sees a `FrameClose`.
    let worker_handle: Rc<RefCell<Option<std::thread::JoinHandle<()>>>> =
        Rc::new(RefCell::new(None));
    let worker_h_t = Rc::clone(&worker_handle);
    let tick = RcBlock::new(move |_t: NonNull<NSTimer>| {
        // System theme: re-resolve on appearance/accent change and wake the
        // worker with a ThemeChange so it re-templates and repaints.
        if theme::refresh(&app_t) {
            igui_events::push(IGuiEvent::ThemeChange);
        }

        // IDE key-window state → Focus events (dimming while inactive).
        {
            let key = mgr_t.borrow().main_is_key();
            if let Some(k) = key {
                if last_main_key.get() != Some(k) {
                    last_main_key.set(Some(k));
                    igui_events::push(IGuiEvent::Focus { child_id: MAIN_ID, gained: k });
                }
            }
        }

        // Drain commands (creates/closes windows), then repaint dirty ones.
        mgr_t.borrow_mut().drain_commands();

        // Detect window resizes (no NSEvent for them reaches our monitor) and
        // emit Resize events so the IDE / Lisp panes re-render at the new size.
        mgr_t.borrow_mut().sync_window_sizes();

        // Startup loading bar: the worker is blocked JIT-compiling the stdlib
        // and can't repaint, so we paint progress here on the main thread.
        if crate::load_progress::active() {
            let (done, total, phase) = crate::load_progress::snapshot();
            if done != last_done.get() {
                last_done.set(done);
                if let Some((w, h)) = mgr_t.borrow().main_size() {
                    present(MAIN_ID, loading_batch(w as f32, h as f32, done, total, &phase));
                }
            }
        }

        if quit_on_last_close {
            // Detect title-bar close-box closes (no NSEvent for them) and
            // emit a Lisp `:close` so app handlers can react.
            let app_hidden = app_t.isHidden();
            let closed = mgr_t.borrow_mut().sweep_closed(app_hidden);
            for id in closed {
                igui_events::push(igui_events::IGuiEvent::Close { child_id: id });
            }
            let empty = mgr_t.borrow().wins.is_empty();
            if !empty {
                had_t.set(true);
            } else if had_t.get() {
                // The last window has closed — quit the standalone app.
                unsafe { app_t.terminate(None) };
            }
        }
        if QUIT_REQUESTED.swap(false, Ordering::Relaxed) {
            // Graceful quit (NCL_GUI_QUIT_AFTER_MS): stop the worker, wait
            // for it to leave the JIT, then terminate. See `worker_handle`.
            igui_events::push(IGuiEvent::FrameClose);
            if let Some(h) = worker_h_t.borrow_mut().take() {
                let _ = h.join();
            }
            unsafe { app_t.terminate(None) };
        }
        mgr_t.borrow().repaint_dirty();
    });
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &tick)
    };

    // Event monitor: tag each event with its window's id.
    let mgr_e = Rc::clone(&manager);
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let e = unsafe { event.as_ref() };
        // Menu-owned key equivalents (⌘S, ⌘N, ⇧⌘Z, …): post the menu
        // command and swallow the event. Swallowing is what keeps the
        // dispatch single — otherwise AppKit's own menu matching would
        // fire the item AND our key path would see the event. Pure-AppKit
        // combos (⌘Q, ⌘H, ⌘M) are not registered and flow on untouched.
        if e.r#type() == NSEventType::KeyDown {
            let mods = ev::mods_from_flags(e.modifierFlags().0 as u64)
                & (modifier::SHIFT | modifier::CONTROL | modifier::ALT | modifier::WIN);
            if let Some(op) = menu::key_equivalent_cmd(e.keyCode() as u16, mods) {
                igui_events::push(IGuiEvent::Menu {
                    menu_id: menu_cmd::IDE,
                    item_id: op,
                });
                return std::ptr::null_mut();
            }
        }
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

    let spawned = std::thread::Builder::new()
        .name("ncl-lisp-worker".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(worker)
        .map_err(|e| format!("failed to spawn Lisp worker: {e}"))?;
    *worker_handle.borrow_mut() = Some(spawned);

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
                // Command combos are menu accelerators, not text: real Mac
                // apps never insert a character for them. (⌘Q/⌘H etc. are
                // not in our registry and still reach AppKit's menu.)
                if !c.is_control() && flags & ev::nsflags::COMMAND == 0 {
                    igui_events::push(ev::char_event(child_id, c as u32, flags, 0));
                }
            }
        }
        return;
    }

    let mouse = |op: i64, button: i64| {
        let p: NSPoint = e.locationInWindow();
        let y = ev::to_top_left_y(p.y, view_height);
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
