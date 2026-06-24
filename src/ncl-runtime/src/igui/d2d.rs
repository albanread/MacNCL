//! Direct2D factory, device, and per-swap-chain render target.
//!
//! Phase 1: one D2D device on top of the D3D11 device, one device
//! context, and a bitmap render target wrapped around the swap chain's
//! back buffer. Recreated on resize.

#![cfg(windows)]

use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_PIXEL_FORMAT, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;

use super::d3d::D3dContext;
use super::IGuiError;

/// Process-wide Direct2D plumbing. The factory and device are created
/// once; the device context is reusable across surfaces.
#[allow(dead_code)] // factory + device are held for Phase 3+ (per-pane contexts)
pub struct D2dContext {
    pub factory: ID2D1Factory1,
    pub device: ID2D1Device,
    pub context: ID2D1DeviceContext,
}

impl D2dContext {
    pub fn new(d3d: &D3dContext) -> Result<Self, IGuiError> {
        let options = D2D1_FACTORY_OPTIONS::default();
        let factory: ID2D1Factory1 = unsafe {
            D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))
        }
        .map_err(|e| IGuiError::D2D(format!("D2D1CreateFactory failed: {e}")))?;

        let dxgi_device = d3d.dxgi_device()?;
        let device = unsafe { factory.CreateDevice(&dxgi_device) }
            .map_err(|e| IGuiError::D2D(format!("ID2D1Factory1::CreateDevice failed: {e}")))?;

        let context = unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
            .map_err(|e| IGuiError::D2D(format!("CreateDeviceContext failed: {e}")))?;

        Ok(Self {
            factory,
            device,
            context,
        })
    }
}

/// Render target wrapping the back buffer of a swap chain. Recreated on
/// resize via `recreate`.
pub struct SwapChainTarget {
    pub bitmap: ID2D1Bitmap1,
}

impl SwapChainTarget {
    pub fn new(d2d: &D2dContext, swap_chain: &IDXGISwapChain1) -> Result<Self, IGuiError> {
        let back_buffer: windows::Win32::Graphics::Dxgi::IDXGISurface =
            unsafe { swap_chain.GetBuffer(0) }.map_err(|e| {
                IGuiError::D2D(format!("IDXGISwapChain1::GetBuffer failed: {e}"))
            })?;

        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };

        let bitmap =
            unsafe { d2d.context.CreateBitmapFromDxgiSurface(&back_buffer, Some(&props)) }
                .map_err(|e| {
                    IGuiError::D2D(format!("CreateBitmapFromDxgiSurface failed: {e}"))
                })?;

        Ok(Self { bitmap })
    }

    /// Bind this bitmap as the active render target on the D2D context.
    pub fn bind(&self, d2d: &D2dContext) {
        unsafe { d2d.context.SetTarget(&self.bitmap) };
    }

    /// Detach the active render target (call before resizing the swap
    /// chain).
    pub fn unbind(d2d: &D2dContext) {
        unsafe { d2d.context.SetTarget(None) };
    }

    #[allow(dead_code)] // used in Phase 3 for resize math
    pub fn pixel_size(&self) -> D2D_SIZE_U {
        unsafe { self.bitmap.GetPixelSize() }
    }
}
