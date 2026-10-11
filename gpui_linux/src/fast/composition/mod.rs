// Modified by Longbridge for gpui-fast.
//! Window composition (zed-industries/zed#62379): native views between
//! GPUI's base content and its overlays, on Wayland and X11.

#[cfg(feature = "wayland")]
pub(crate) mod wayland;
#[cfg(feature = "x11")]
pub(crate) mod x11;
