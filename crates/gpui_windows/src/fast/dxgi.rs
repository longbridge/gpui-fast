//! The DXGI factory interface the renderer asks for.
//!
//! Upstream requests `IDXGIFactory6`, which only exists from Windows 10 1803
//! (build 17134). On older systems such as 1709 (build 16299),
//! `CreateDXGIFactory2` then fails with `E_NOINTERFACE` (0x80004002) and the app
//! panics at startup before showing a window. The renderer only calls methods
//! that `IDXGIFactory2` already has (`EnumAdapters`,
//! `CreateSwapChainForHwnd`, `CreateSwapChainForComposition`,
//! `MakeWindowAssociation`), so we ask for that instead.

pub(crate) type DxgiFactory = windows::Win32::Graphics::Dxgi::IDXGIFactory2;
