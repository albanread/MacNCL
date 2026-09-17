//! Apple Events: scriptability and lifecycle, on the main thread.
//!
//! Registered at GUI startup:
//! - an **NSApplication delegate** — Dock/Finder re-open brings the IDE
//!   front, `open file.lisp` (odoc) opens tabs, and quitting (⌘Q *and* the
//!   Apple-event quit) now goes through `applicationShouldTerminate:`,
//!   which joins the Lisp worker before terminating (exiting mid-JIT
//!   segfaults on Apple Silicon — see the quit-path comment in window.rs).
//! - **NSAppleEventManager** handlers for our scripting class `MNCL`
//!   (`eval`, `transcript`, `clear repl` — see resources/MacNCL.sdef), so
//!   `osascript -e 'tell application id "com.albanread.macncl" to eval
//!   "(* 6 7)"'` drives the real REPL. Handlers forward through
//!   [`crate::igui_mac::scripting`] to the worker and block for the reply
//!   (bounded) — synchronous Apple Events are the point.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2_app_kit::NSApplicationDelegate;
use objc2_core_services::{AEEventClass, AEEventID};
use objc2_foundation::NSString;

use crate::igui_events::{self, IGuiEvent};
use crate::igui_mac::scripting::{self, ScriptKind};

/// Our scripting event class ('MNCL').
const CLASS: AEEventClass = fourcc(b"MNCL");
const ID_EVAL: AEEventID = fourcc(b"eval");
const ID_TRANSCRIPT: AEEventID = fourcc(b"trns");
const ID_CLEAR: AEEventID = fourcc(b"clr ");

/// How long a scripted eval may run before the Apple Event reply gives up
/// (the worker still finishes it; the transcript keeps the result).
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

/// Install the delegate + handlers. Must run on the main thread before
/// `app.run()`.
pub fn install(app: &objc2_app_kit::NSApplication, mtm: objc2::MainThreadMarker) {
    use objc2::ClassType;
    scripting::install();

    // Delegate (kept alive for the process lifetime).
    let delegate: Retained<AppDelegate> = unsafe { objc2::msg_send![AppDelegate::class(), new] };
    let d_raw = Retained::into_raw(delegate.clone());
    DELEGATE.store(d_raw, Ordering::Relaxed);
    // SAFETY: AppDelegate implements NSApplicationDelegate below.
    let po: Retained<objc2::runtime::ProtocolObject<dyn objc2_app_kit::NSApplicationDelegate>> =
        unsafe { objc2::runtime::ProtocolObject::from_retained(delegate.clone()) };
    app.setDelegate(Some(&po));

    // Scripting handlers: one selector, dispatched by event id.
    let handler: Retained<AppleEventHandler> =
        unsafe { objc2::msg_send![AppleEventHandler::class(), new] };
    let h_raw = Retained::into_raw(handler.clone());
    HANDLER.store(h_raw, Ordering::Relaxed);
    let _ = mtm; // registration is main-thread by construction
    let mgr = objc2_foundation::NSAppleEventManager::sharedAppleEventManager();
    for id in [ID_EVAL, ID_TRANSCRIPT, ID_CLEAR] {
        // SAFETY: the handler lives in a static; the selector exists.
        unsafe {
            mgr.setEventHandler_andSelector_forEventClass_andEventID(
                &handler,
                objc2::sel!(handleAppleEvent:withReplyEvent:),
                CLASS,
                id,
            );
        }
    }
}

use std::sync::atomic::{AtomicPtr, Ordering};
/// Delegate + handler are main-thread-only objects; the raw pointers just
/// make them storable in statics (never touched off the main thread).
static DELEGATE: AtomicPtr<AppDelegate> = AtomicPtr::new(std::ptr::null_mut());
static HANDLER: AtomicPtr<AppleEventHandler> = AtomicPtr::new(std::ptr::null_mut());

/// The IDE window, for re-open ordering (set from `window::open`).
static MAIN_WINDOW: AtomicPtr<objc2_app_kit::NSWindow> =
    AtomicPtr::new(std::ptr::null_mut::<objc2_app_kit::NSWindow>());

pub(crate) fn note_main_window(w: &objc2_app_kit::NSWindow) {
    // SAFETY: retained for the static pointer; the WindowManager holds
    // the window for the process lifetime anyway.
    let p: *mut objc2_app_kit::NSWindow = unsafe { objc2::msg_send![w, retain] };
    MAIN_WINDOW.store(p, Ordering::Relaxed);
}

// ── Application delegate ─────────────────────────────────────────────────

objc2::define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "NCLAppDelegate"]
    #[thread_kind = objc2::MainThreadOnly]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Dock/Finder click with no window visible → bring the IDE back.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(
            &self,
            _app: Option<&objc2_app_kit::NSApplication>,
            _has_visible: objc2::runtime::Bool,
        ) -> bool {
            let p = MAIN_WINDOW.load(Ordering::Relaxed);
            if !p.is_null() {
                // SAFETY: the window is alive for the process lifetime.
                let w: &objc2_app_kit::NSWindow = unsafe { &*p };
                w.makeKeyAndOrderFront(None);
            }
            true
        }

        /// odoc: files opened via Finder, `open file.lisp`, or Dock drops.
        #[unsafe(method(application:openFiles:))]
        fn open_files(
            &self,
            _app: Option<&objc2_app_kit::NSApplication>,
            files: Option<&AnyObject>,
        ) {
            let Some(files) = files else { return };
            // NSArray<NSString> of paths (the deprecated API, still what
            // LaunchServices delivers for document types).
            let count: usize = unsafe { objc2::msg_send![files, count] };
            for i in 0..count {
                // objectAtIndex: takes NSUInteger — usize, not isize.
                let path: Option<Retained<NSString>> = unsafe {
                    objc2::msg_send![files, objectAtIndex: i]
                };
                if let Some(p) = path {
                    igui_events::push(IGuiEvent::Open { path: p.to_string() });
                }
            }
        }

        /// Every quit path lands here (⌘Q, Apple-event quit). Join the
        /// Lisp worker before terminating — exit(3) during a JIT compile
        /// segfaults on Apple Silicon.
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(
            &self,
            _app: Option<&objc2_app_kit::NSApplication>,
        ) -> objc2_app_kit::NSApplicationTerminateReply {
            igui_events::push(IGuiEvent::FrameClose);
            crate::igui_mac::window::join_worker_for_quit();
            objc2_app_kit::NSApplicationTerminateReply::TerminateNow
        }
    }
);

// ── Scripting handlers ───────────────────────────────────────────────────

objc2::define_class!(
    #[unsafe(super(objc2::runtime::NSObject))]
    #[name = "NCLAppleEventHandler"]
    struct AppleEventHandler;

    impl AppleEventHandler {
        #[unsafe(method(handleAppleEvent:withReplyEvent:))]
        fn handle(
            &self,
            event: Option<&objc2_foundation::NSAppleEventDescriptor>,
            reply: Option<&objc2_foundation::NSAppleEventDescriptor>,
        ) {
            use objc2_core_services::keyDirectObject;
            let (Some(event), Some(reply)) = (event, reply) else { return };
            let id: AEEventID = unsafe { objc2::msg_send![event, eventID] };
            let answer = match id {
                ID_EVAL => {
                    let src: String = event
                        .descriptorForKeyword(keyDirectObject)
                        .and_then(|d| d.stringValue())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    if src.is_empty() {
                        "; eval: no source".to_string()
                    } else {
                        match scripting::submit(ScriptKind::Eval(src)) {
                            Some(rx) => rx
                                .recv_timeout(REPLY_TIMEOUT)
                                .unwrap_or_else(|_| "; eval: timed out".into()),
                            None => "; eval: worker unavailable".into(),
                        }
                    }
                }
                ID_TRANSCRIPT => match scripting::submit(ScriptKind::Transcript) {
                    Some(rx) => rx
                        .recv_timeout(REPLY_TIMEOUT)
                        .unwrap_or_else(|_| "; transcript: timed out".into()),
                    None => String::new(),
                },
                _ => {
                    // clear repl
                    match scripting::submit(ScriptKind::ClearRepl) {
                        Some(rx) => rx.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| "; timed out".into()),
                        None => "OK".into(),
                    }
                }
            };
            // Reply with the answer string as the direct object.
            let desc = objc2_foundation::NSAppleEventDescriptor::descriptorWithString(
                &NSString::from_str(&answer),
            );
            reply.setDescriptor_forKeyword(&desc, keyDirectObject);
        }
    }
);
