//! D3D11 device + DXGI swap chain creation for iGui surfaces.
//!
//! Phase 1: one D3D11 device for the process, plus a swap chain bound to
//! the MDI client HWND. The swap chain is the rendering target for now;
//! once child windows arrive in Phase 3, each child gets its own swap
//! chain and this module grows a per-HWND helper.

#![cfg(windows)]

use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_CREATE_FACTORY_FLAGS,
    DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use super::IGuiError;

/// Process-wide D3D11 device. Created lazily by `D3dContext::new`. Owned
/// by the GUI thread; iGui guarantees no other thread touches it.
#[allow(dead_code)] // immediate context + factory are held for Phase 3+
pub struct D3dContext {
    pub device: ID3D11Device,
    pub immediate: ID3D11DeviceContext,
    pub dxgi_factory: IDXGIFactory2,
}

impl D3dContext {
    pub fn new() -> Result<Self, IGuiError> {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1];

        let mut device: Option<ID3D11Device> = None;
        let mut immediate: Option<ID3D11DeviceContext> = None;
        let mut chosen_level = D3D_FEATURE_LEVEL_11_0;

        // Try hardware first; fall back to WARP for headless / CI.
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut chosen_level),
                Some(&mut immediate),
            )
        };
        if hr.is_err() {
            let hr2 = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    HMODULE::default(),
                    flags,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut chosen_level),
                    Some(&mut immediate),
                )
            };
            hr2.map_err(|e| IGuiError::D3D(format!("D3D11CreateDevice (HW+WARP) failed: {e}")))?;
        }

        let device = device.ok_or_else(|| {
            IGuiError::D3D("D3D11CreateDevice returned success but no device".into())
        })?;
        let immediate = immediate.ok_or_else(|| {
            IGuiError::D3D("D3D11CreateDevice returned success but no immediate context".into())
        })?;

        let dxgi_factory: IDXGIFactory2 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
                .map_err(|e| IGuiError::D3D(format!("CreateDXGIFactory2 failed: {e}")))?;

        Ok(Self {
            device,
            immediate,
            dxgi_factory,
        })
    }

    pub fn dxgi_device(&self) -> Result<IDXGIDevice, IGuiError> {
        self.device
            .cast::<IDXGIDevice>()
            .map_err(|e| IGuiError::D3D(format!("ID3D11Device → IDXGIDevice cast failed: {e}")))
    }

    /// Create a flip-model swap chain bound to the given HWND. BGRA8 to
    /// keep Direct2D interop straightforward.
    pub fn create_swap_chain_for_hwnd(
        &self,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<IDXGISwapChain1, IGuiError> {
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };

        unsafe {
            self.dxgi_factory
                .CreateSwapChainForHwnd(&self.device, hwnd, &desc, None, None)
        }
        .map_err(|e| IGuiError::D3D(format!("CreateSwapChainForHwnd failed: {e}")))
    }
}

/// Convenience: present with default flags. Phase 1 calls this once per
/// `WM_PAINT`.
pub fn present(swap_chain: &IDXGISwapChain1) -> Result<(), IGuiError> {
    unsafe { swap_chain.Present(1, DXGI_PRESENT(0)) }
        .ok()
        .map_err(|e| IGuiError::D3D(format!("Present failed: {e}")))?;
    Ok(())
}
