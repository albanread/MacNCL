//! Cocoa windowing for iGui (feature `mac-gui`).
//!
//! The macOS analogue of `igui::window::run`: it takes the calling thread
//! as the **UI thread** (macOS *requires* AppKit on the main thread),
//! brings up an `NSApplication` + `NSWindow`, spawns the Lisp worker on a
//! background thread, installs a local `NSEvent` monitor that translates
//! input into `IGuiEvent`s and pushes them into the shared mailbox
//! (`crate::igui_events`), and runs the AppKit event loop.
//!
//! This is the windowing + event-boundary half of Phase 2.2. Live
//! presentation of a `CgCanvas` into the window (via `NSImageView` + a
//! `CGImage` bridge) is the next mechanical step — the renderer itself is
//! already complete and unit-tested in `render.rs`. See PORTING_DESIGN.md
//! §4.6.
//!
//! Build/run with `--features mac-gui`. Requires a logged-in GUI session
//! (WindowServer); it cannot run fully headless.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use block2::RcBlock;
use foreign_types::ForeignType;
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

// ── Frame presentation: worker thread sets a frame, the main-thread
// timer renders and blits it into the window's NSImageView ──────────────

static FRAME: OnceLock<Mutex<Option<Arc<Vec<SurfaceCmd>>>>> = OnceLock::new();
static DIRTY: AtomicBool = AtomicBool::new(false);

fn frame_slot() -> &'static Mutex<Option<Arc<Vec<SurfaceCmd>>>> {
    FRAME.get_or_init(|| Mutex::new(None))
}

/// Present a frame: store the `SurfaceCmd` list and mark the window dirty.
/// Safe to call from the Lisp worker thread — the main-thread timer reads
/// the slot and repaints. This is the macOS analogue of `batch::submit`.
pub fn present(cmds: Vec<SurfaceCmd>) {
    *frame_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(cmds));
    DIRTY.store(true, Ordering::Release);
}

/// Render the current frame (if dirty) into `image_view` at `w`×`h`
/// points. Runs on the main thread from the repaint timer.
fn repaint(image_view: &NSImageView, w: f64, h: f64, mtm: MainThreadMarker) {
    if !DIRTY.swap(false, Ordering::AcqRel) {
        return;
    }
    let cmds = {
        let guard = frame_slot().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(c) => Arc::clone(c),
            None => return,
        }
    };
    let mut canvas = CgCanvas::new(w as usize, h as usize);
    canvas.execute(&cmds);
    // Debug: dump the exact frame being presented to a PPM once, so the
    // composed window content is inspectable without a screen grab.
    if let Some(path) = std::env::var_os("NCL_IGUI_DUMP") {
        static DUMPED: AtomicBool = AtomicBool::new(false);
        if !DUMPED.swap(true, Ordering::AcqRel) {
            let _ = std::fs::write(path, canvas.to_ppm());
        }
    }
    let Some(img) = canvas.cg_image() else { return };
    // Bridge the servo `core_graphics` CGImage to objc2's `&CGImage`:
    // both are opaque `CGImageRef` handles to the same object.
    let cg_ref = img.as_ptr();
    // SAFETY: `cg_ref` is a live CGImageRef for the duration of this call;
    // `initWithCGImage_size` retains it before we drop `img`.
    let objc_img: &CGImage = unsafe { &*(cg_ref as *const CGImage) };
    let ns_image = NSImage::initWithCGImage_size(mtm.alloc::<NSImage>(), objc_img, NSSize::new(w, h));
    image_view.setImage(Some(&ns_image));
}

/// The single content child's id for this first single-window bring-up.
/// The full child registry (one id per pane) arrives with the pane port.
const CONTENT_CHILD_ID: i64 = 1;

/// Open the iGui main window and run the AppKit event loop on the calling
/// (main) thread, with `worker` running the Lisp side on a background
/// thread. Returns when the app terminates. `Err` if not on the main
/// thread.
pub fn run<F>(title: &str, width: f64, height: f64, worker: F) -> Result<(), String>
where
    F: FnOnce() + Send + 'static,
{
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| "igui_mac::run must be called on the main thread".to_string())?;

    // Make sure the GUI→Lisp dispatcher is running before any event fires.
    igui_events::install();

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // Build the main window.
    let content_rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str(title));
    window.center();

    // Content view: an NSImageView we blit the rendered CgCanvas into.
    let image_view = NSImageView::new(mtm);
    window.setContentView(Some(&image_view));

    // Repaint timer (~60 Hz): on the main thread, render the latest frame
    // and show it. Cheap when not dirty (early-out).
    let iv_for_timer = image_view.clone();
    let repaint_block = RcBlock::new(move |_t: NonNull<NSTimer>| {
        repaint(&iv_for_timer, width, height, mtm);
    });
    let _timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_repeats_block(1.0 / 60.0, true, &repaint_block)
    };

    // Translate every key/mouse event into an IGuiEvent and push it to the
    // shared mailbox. The closure returns the event pointer unchanged so
    // normal processing (menus, the close button) still happens.
    let view_height = height;
    let handler = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands us a live, non-null NSEvent for the
        // duration of the call.
        let e = unsafe { event.as_ref() };
        dispatch_event(e, view_height);
        event.as_ptr()
    });
    let mask = NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::LeftMouseDown
        | NSEventMask::LeftMouseUp
        | NSEventMask::RightMouseDown
        | NSEventMask::RightMouseUp
        | NSEventMask::MouseMoved
        | NSEventMask::ScrollWheel;
    // Keep the monitor alive for the life of the app.
    let _monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &handler) };

    window.makeKeyAndOrderFront(None);
    app.activate();

    // Spawn the Lisp worker on a background thread (mirrors the Windows
    // model: UI on thread 0, Lisp on a worker).
    std::thread::Builder::new()
        .name("ncl-lisp-worker".into())
        .spawn(worker)
        .map_err(|e| format!("failed to spawn Lisp worker: {e}"))?;

    app.run();
    Ok(())
}

/// Translate one `NSEvent` and push the resulting `IGuiEvent`(s).
fn dispatch_event(e: &NSEvent, view_height: f64) {
    let flags = e.modifierFlags().0 as u64;
    let t = e.r#type();

    if t == NSEventType::KeyDown || t == NSEventType::KeyUp {
        let down = t == NSEventType::KeyDown;
        let keycode = e.keyCode();
        let ch = e
            .charactersIgnoringModifiers()
            .and_then(|s| s.to_string().chars().next());
        igui_events::push(ev::key_event(
            CONTENT_CHILD_ID,
            keycode,
            ch,
            flags,
            e.isARepeat(),
            down,
            0,
        ));
        // On key-down, also emit a Char event for printable input, mirroring
        // the Win32 WM_KEYDOWN + WM_CHAR pairing the panes expect.
        if down {
            if let Some(c) = ch {
                if !c.is_control() {
                    igui_events::push(ev::char_event(CONTENT_CHILD_ID, c as u32, flags, 0));
                }
            }
        }
        return;
    }

    // Mouse events: convert window-space (bottom-left) to top-left view space.
    let mouse = |op: i64, button: i64| {
        let p: NSPoint = e.locationInWindow();
        let y = ev::to_top_left_y(p.y, view_height);
        igui_events::push(ev::mouse_event(
            CONTENT_CHILD_ID,
            p.x,
            y,
            op,
            button,
            flags,
            0,
            0,
            0,
        ));
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
    } else if t == NSEventType::ScrollWheel {
        let p: NSPoint = e.locationInWindow();
        let y = ev::to_top_left_y(p.y, view_height);
        let dy = e.scrollingDeltaY();
        igui_events::push(ev::mouse_event(
            CONTENT_CHILD_ID,
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
