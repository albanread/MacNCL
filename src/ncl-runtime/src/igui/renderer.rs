//! Process-wide D3D11 / Direct2D / DirectWrite context, lazily
//! initialised at iGui startup and shared across the frame and every
//! MDI child. Lives behind a `OnceLock` rather than per-window state
//! because the GPU device and DWrite factory are cheap to share and
//! expensive to duplicate.

#![cfg(windows)]

use std::sync::OnceLock;

use super::d2d::D2dContext;
use super::d3d::D3dContext;
use super::dwrite::DWriteContext;
use super::IGuiError;

pub struct RendererCtx {
    pub d3d: D3dContext,
    pub d2d: D2dContext,
    #[allow(dead_code)] // surfaced in Phase 4 for text format/layout caches
    pub dwrite: DWriteContext,
}

static RENDERER: OnceLock<RendererCtx> = OnceLock::new();

pub fn install() -> Result<(), IGuiError> {
    if RENDERER.get().is_some() {
        return Ok(());
    }
    let d3d = D3dContext::new()?;
    let d2d = D2dContext::new(&d3d)?;
    let dwrite = DWriteContext::new()?;
    let _ = RENDERER.set(RendererCtx { d3d, d2d, dwrite });
    Ok(())
}

/// Panics if called before `install`. Internal use only — callers
/// inside iGui can assume the renderer is up because `window::run`
/// installs it before any window exists.
pub fn ctx() -> &'static RendererCtx {
    RENDERER
        .get()
        .expect("iGui renderer accessed before install")
}
