//! The context that draws a web window, for applications that render with
//! wgpu themselves.
//!
//! A page has no window composition, so an element whose content wgpu draws
//! can only reach the screen as a texture GPUI samples: the element draws
//! during `paint` into a `WgpuDynamicTexture`, and GPUI paints it as a sprite
//! like any other. Such a texture has to be allocated from the context
//! drawing the window that shows it, and `WebPlatform` is what holds that
//! context, so reaching it is the whole of this module.

use gpui_wgpu::WgpuContext;

use crate::WebPlatform;

impl WebPlatform {
    /// The context drawing this platform's window, once graphics have been
    /// initialized: from the moment the application finishes launching. An
    /// application that renders with wgpu allocates its dynamic textures from
    /// here, so GPUI can sample what it drew.
    pub fn wgpu_context(&self) -> Option<WgpuContext> {
        self.wgpu_context.borrow().clone()
    }
}
