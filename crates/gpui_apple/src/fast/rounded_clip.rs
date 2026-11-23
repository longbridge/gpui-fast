//! Evaluate independent rounded padding edges directly in colour fragments.
//! Nested scopes append geometry and siblings restore the enclosing stack.
//! Children draw at the enclosing sprite's depth and inherit its clip stack.

use std::{mem, ops::Range, ptr::NonNull};

use anyhow::{Context as _, Result};
use gpui::{AtlasTextureId, DevicePixels, LayerFrame, Scene, Size};
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVMetalTexture;
use objc2_metal::{MTLDevice, MTLRenderCommandEncoder, MTLResourceOptions};

use crate::metal_renderer::{InstanceBufferWriter, MetalRenderer, write_instances};

const CLIP_TEXTURE_BASE: u32 = 0xF100_0000;

/// Geometry is copied into Metal's command stream, never retained in a shared
/// mutable texture. A stack remains valid when sibling scopes overlap.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Clip {
    bounds: [f32; 4],
    horizontal: [f32; 4],
    vertical: [f32; 4],
}

fn group(scene: &Scene, texture: AtlasTextureId) -> Option<&LayerFrame> {
    let key = texture.index.checked_sub(0xF000_0000)?;
    if texture.index < CLIP_TEXTURE_BASE {
        return None;
    }
    scene
        .layers
        .frames
        .iter()
        .find(|frame| frame.key.0 == key && frame.tile_size == 0)
}

pub(crate) fn default_mask(encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>) {
    let count = 0u32;
    let empty = [0f32; 12];
    // SAFETY: Both values are live for the call, which copies their bytes into
    // the fragment slots the injected clip parameters read.
    unsafe {
        encoder.setFragmentBytes_length_atIndex(NonNull::from(&count).cast(), 4, 29);
        encoder.setFragmentBytes_length_atIndex(NonNull::from(&empty).cast(), 48, 28);
    }
}

fn bind(
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    clips: &[Clip],
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<()> {
    let count = clips.len() as u32;
    // SAFETY: `count` is live for the call, which copies its bytes.
    unsafe {
        encoder.setFragmentBytes_length_atIndex(NonNull::from(&count).cast(), 4, 29);
    }
    if !clips.is_empty() {
        let length = mem::size_of_val(clips);
        if length <= 4096 {
            // SAFETY: `clips` is live for the call, which copies its `length` bytes.
            unsafe {
                encoder.setFragmentBytes_length_atIndex(NonNull::from(clips).cast(), length, 28);
            }
        } else {
            // SAFETY: Metal copies `length` bytes of the live `clips` slice.
            let buffer = unsafe {
                device.newBufferWithBytes_length_options(
                    NonNull::from(clips).cast(),
                    length,
                    MTLResourceOptions::StorageModeShared,
                )
            };
            let buffer =
                buffer.with_context(|| format!("allocating {length} bytes for rounded clips"))?;
            // SAFETY: The buffer holds `clips.len()` clips from offset zero, and
            // the encoder retains it until the command buffer completes.
            unsafe {
                encoder.setFragmentBuffer_offset_atIndex(Some(&buffer), 0, 28);
            }
        }
    }
    Ok(())
}

pub(crate) fn raster_nested_tiles(renderer: &mut MetalRenderer, scene: &Scene) {
    for sprite in &scene.polychrome_sprites {
        if let Some(frame) = group(scene, sprite.tile.texture_id)
            && let Some(child) = frame.content.scene()
        {
            crate::fast::layers::raster::rasterize_tiles(renderer, child);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_group(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    texture_id: AtlasTextureId,
    range: Range<usize>,
    writer: &mut InstanceBufferWriter,
    viewport: Size<DevicePixels>,
    encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    clips: &mut Vec<Clip>,
    surface_textures: &mut Vec<CFRetained<CVMetalTexture>>,
    parent_depth: f64,
) -> Result<bool> {
    let Some(frame) = group(scene, texture_id) else {
        return Ok(false);
    };
    let child = frame.content.scene().context("rounded clip content")?;
    let radii = child
        .layers
        .frames
        .iter()
        .find(|frame| frame.key.0 == u32::MAX)
        .context("rounded clip geometry")?
        .background;
    let child_bindings = write_instances(child, writer)?;
    for sprite in &scene.polychrome_sprites[range] {
        let b = sprite.bounds;
        let r = sprite.corner_radii;
        clips.push(Clip {
            bounds: [b.origin.x.0, b.origin.y.0, b.right().0, b.bottom().0],
            horizontal: [
                r.top_left.0,
                r.top_right.0,
                r.bottom_right.0,
                r.bottom_left.0,
            ],
            vertical: [radii.r, radii.g, radii.b, radii.a],
        });
        bind(encoder, clips, &renderer.device)?;
        let drawn = renderer.encode_fast_batches(
            child,
            &child_bindings,
            writer,
            viewport,
            encoder,
            clips,
            surface_textures,
            Some(parent_depth),
        );
        clips.pop();
        drawn?;
    }
    bind(encoder, clips, &renderer.device)?;
    Ok(true)
}
