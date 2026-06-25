//! macOS iGui backend.
//!
//! Reimplements the iGui rendering surface on Apple frameworks,
//! replacing the Windows Direct3D11 / Direct2D / DirectWrite stack:
//!
//!   - **render** — a Core Graphics rasteriser for the platform-neutral
//!     `SurfaceCmd` IR (`crate::igui_paint`). This is the macOS analogue
//!     of `igui::child::execute_d2d_batch`. It can render headlessly to
//!     a `CGBitmapContext`, so the whole drawing path is unit-testable
//!     without a window or display — the same render-to-bitmap approach
//!     the `doc-crate` snapshot harness uses on Windows.
//!   - *(later)* **window** — Cocoa `NSApplication`/`NSWindow` on the
//!     main thread + `CAMetalLayer` surface + `NSEvent`→`IGuiEvent`
//!     mailbox (Phase 2.2), and Core Text measurement for the three
//!     synchronous text queries.
//!
//! See docs/PORTING_DESIGN.md §4.5–4.6.

pub mod events;
pub mod ide;
pub mod render;
/// Native Lisp shims that let Lisp drive the Cocoa side windows (open-child,
/// the batch/`%emit-*` drawing primitives, next-event). Opt-in (`mac-gui`).
#[cfg(feature = "mac-gui")]
pub mod shims;
/// Cocoa windowing + AppKit event loop. Opt-in (`mac-gui`) because it
/// pulls the objc2 AppKit bindings and needs a GUI session to run; the
/// renderer and event translation below it are always built and tested.
#[cfg(feature = "mac-gui")]
pub mod window;

pub use render::CgCanvas;
