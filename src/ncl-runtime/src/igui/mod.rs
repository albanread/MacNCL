//! iGui — integrated GUI for NewCormanLisp.
//!
//! Direct-rendered MDI frame using Direct2D and DirectWrite. Borrowed
//! from the sister NewCP repo (E:/NewCP/NewCP/src/newcp-runtime/src/
//! igui) — the renderer architecture and event-mailbox design carry
//! over verbatim. The integration layer (`cp_exports.rs`) is what
//! changed: NewCP routed these via its CP module system, here we
//! install them as native Lisp functions through the same
//! `install_native` mechanism format and the file primitives use.
//!
//! Architecture:
//!   - `window` runs an MDI frame on a dedicated GUI thread.
//!   - `child` manages MDI children; each child owns a render
//!     surface (D3D swap chain wrapped by D2D).
//!   - `channels` is a bounded MPSC mailbox carrying typed
//!     `IGuiEvent` values from the GUI thread to the language
//!     thread.
//!   - `batch` queues `SurfaceCmd` draw operations for execution by
//!     `executor` on the GUI thread.
//!   - `ledit` and `log_view` are built-in MDI children for an
//!     editor and a log overlay; `tools_menu` wires their menu and
//!     accelerators into the frame.
//!
//! Phase 1 scope: open an MDI frame + MDI client, initialize D3D11 /
//! Direct2D / DirectWrite, paint a solid color into the MDI client
//! area during `WM_PAINT`, and exit cleanly on `WM_CLOSE` /
//! `WM_DESTROY`. The Lisp-side bindings (NEXT-EVENT / OPEN-CHILD /
//! ...) ride on top in a follow-up commit.

#![cfg(windows)]

pub mod batch;
pub mod canvas;
pub mod channels;
mod child;
pub mod crash_handler;
pub mod crash_view;
pub mod cp_exports;
mod cursor;
pub mod splash;
mod d2d;
mod d3d;
pub mod doc_pane;
mod dwrite;
pub mod help_pane;
mod executor;
mod font_metrics;
pub mod lisp_shims;
pub mod log_view;
mod menu;
pub(crate) mod ledit;
pub mod repl_child;
pub(crate) mod rope_buffer;
pub(crate) mod sexp_nav;
mod registry;
mod renderer;
mod replies;
mod tools_menu;
pub mod system_colors;
pub(crate) mod text_view;
mod window;

pub use ledit::{install_checker, Diagnostic};
pub use window::run;

/// Errors surfaced from iGui startup. Phase 1 keeps this lossy on purpose;
/// every variant carries enough text to diagnose without a debugger.
#[derive(Debug)]
pub enum IGuiError {
    Win32(String),
    D3D(String),
    D2D(String),
    DWrite(String),
}

impl std::fmt::Display for IGuiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IGuiError::Win32(msg) => write!(f, "iGui: Win32: {msg}"),
            IGuiError::D3D(msg) => write!(f, "iGui: D3D: {msg}"),
            IGuiError::D2D(msg) => write!(f, "iGui: D2D: {msg}"),
            IGuiError::DWrite(msg) => write!(f, "iGui: DirectWrite: {msg}"),
        }
    }
}

impl std::error::Error for IGuiError {}

/// Phase 1 paints this slate gray into the MDI client area so we can see
/// the renderer is actually running. Will be replaced once the surface
/// executor lands and children own their own colors.
pub(crate) const PHASE1_BACKGROUND: [f32; 4] = [0.18, 0.20, 0.23, 1.0];
