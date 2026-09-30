//! Renderers for the extra GPUI surfaces of a composed window.
//!
//! Ported from zed-industries/zed#62379. A window that composes native views
//! between GPUI's base content and its overlays draws each GPUI surface on a
//! `CAMetalLayer` of its own. Every surface's renderer shares the window
//! renderer's device and sprite atlas, so glyphs and images rasterized for
//! one surface are there for the others, and the tiles a frame's scene refers
//! to are valid whichever surface draws them.

use std::sync::Arc;

use cocoa::{
    base::{NO, YES},
    quartzcore::AutoresizingMask,
};
use metal::MTLPixelFormat;
use objc::{msg_send, sel, sel_impl};
use parking_lot::Mutex;

use crate::metal_renderer::{Context, InstanceBufferPool, MetalRenderer, Renderer};

/// A renderer for an overlay surface above `base`: transparent, on its own
/// layer, sharing `base`'s device and sprite atlas.
pub(crate) fn new_overlay_renderer(context: Context, base: &Renderer) -> Renderer {
    new_sharing_atlas(base, context, true)
}

/// A renderer on a new `CAMetalLayer` that draws with `base`'s device and
/// sprite atlas.
fn new_sharing_atlas(
    base: &MetalRenderer,
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    transparent: bool,
) -> MetalRenderer {
    let device = base.device.clone();
    let layer = new_layer(&device, transparent);
    let mut renderer =
        MetalRenderer::new_internal(device, Some(layer), !transparent, instance_buffer_pool);
    renderer.sprite_atlas = base.sprite_atlas().clone();
    renderer
}

/// A window's `CAMetalLayer`, set up as `MetalRenderer::new` sets up its own.
fn new_layer(device: &metal::Device, transparent: bool) -> metal::MetalLayer {
    let layer = metal::MetalLayer::new();
    layer.set_device(device);
    layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
    // Support direct-to-display rendering if the window is not transparent
    // https://developer.apple.com/documentation/metal/managing-your-game-window-for-metal-in-macos
    layer.set_opaque(!transparent);
    layer.set_maximum_drawable_count(3);
    // Allow texture reading for visual tests (captures screenshots without ScreenCaptureKit)
    #[cfg(any(test, feature = "test-support"))]
    layer.set_framebuffer_only(false);
    unsafe {
        let _: () = msg_send![&*layer, setAllowsNextDrawableTimeout: NO];
        let _: () = msg_send![&*layer, setNeedsDisplayOnBoundsChange: YES];
        let _: () = msg_send![
            &*layer,
            setAutoresizingMask: AutoresizingMask::WIDTH_SIZABLE
                | AutoresizingMask::HEIGHT_SIZABLE
        ];
    }
    layer
}
