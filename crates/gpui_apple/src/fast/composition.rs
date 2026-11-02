//! Renderers for the extra GPUI surfaces of a composed window.
//!
//! Ported from zed-industries/zed#62379. A window that composes native views
//! between GPUI's base content and its overlays draws each GPUI surface on a
//! `CAMetalLayer` of its own. Every surface's renderer shares the window
//! renderer's device and sprite atlas, so glyphs and images rasterized for
//! one surface are there for the others, and the tiles a frame's scene refers
//! to are valid whichever surface draws them.

use std::sync::Arc;

use anyhow::Result;
use objc2_quartz_core::CAMetalLayer;
use parking_lot::Mutex;

use crate::metal_renderer::{Context, InstanceBufferPool, MetalRenderer, Renderer};

/// A renderer for an overlay surface above `base`: transparent, on its own
/// layer, sharing `base`'s device and sprite atlas.
pub(crate) fn new_overlay_renderer(context: Context, base: &Renderer) -> Result<Renderer> {
    new_sharing_atlas(base, context, true)
}

/// A renderer on a new `CAMetalLayer` that draws with `base`'s device and
/// sprite atlas.
fn new_sharing_atlas(
    base: &MetalRenderer,
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    transparent: bool,
) -> Result<MetalRenderer> {
    let device = base.device.clone();
    let layer = CAMetalLayer::new();
    MetalRenderer::configure_layer(&layer, &device, transparent, false);
    copy_colorspace(base, &layer);
    let mut renderer =
        MetalRenderer::new_internal(device, Some(layer), !transparent, instance_buffer_pool)?;
    renderer.sprite_atlas = base.sprite_atlas().clone();
    Ok(renderer)
}

/// Gives `layer` the colorspace of `base`'s layer, so that overlays above a
/// window drawn in Display P3 are drawn in it too.
fn copy_colorspace(base: &MetalRenderer, layer: &CAMetalLayer) {
    let Some(base_layer) = base.layer.as_ref() else {
        return;
    };
    layer.setColorspace(base_layer.colorspace().as_deref());
}
