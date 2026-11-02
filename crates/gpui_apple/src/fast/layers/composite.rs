//! Drawing scroll layer tiles where the scene composites them: polychrome
//! sprites whose texture id lies in the range no atlas allocates stand for
//! tiles, and are drawn with the polychrome sprite pipeline from the tiles'
//! own textures instead of an atlas texture.
//!
//! A tile sprite covers the tile's whole texture at whole device pixels, so
//! every fragment samples a texel center and copies the tile's pixel.

use std::{mem, ops::Range, ptr};

use gpui::{AtlasTextureId, DevicePixels, Size, TileId, decode_layer_tile, size};
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLPrimitiveType, MTLRenderCommandEncoder, MTLTexture};

use crate::metal_renderer::{InstanceBindings, MetalRenderer, SpriteInputIndex};

/// Forwarded to by `MetalRenderer::draw_polychrome_sprites`. Draws the batch
/// `sprites` if its texture is a layer's tiles, and returns whether it was:
/// otherwise upstream draws it from the atlas. A tile the cache does not hold
/// draws nothing; rasterizing every composited tile it lacks before the
/// frame is drawn prevents that.
pub(crate) fn draw_tiles(
    renderer: &MetalRenderer,
    texture_id: AtlasTextureId,
    sprites: &Range<usize>,
    instance_bindings: &InstanceBindings,
    viewport_size: Size<DevicePixels>,
    command_encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
) -> bool {
    let Some((layer, _)) = decode_layer_tile(texture_id, TileId(0)) else {
        return false;
    };
    let cache = &renderer.fast_layers;
    let polychrome_sprites = &instance_bindings.polychrome_sprites;
    let mut pipeline_set = false;
    for (tile, run) in cache.tile_runs(sprites.clone()) {
        let Some(texture) = cache.texture(layer, tile) else {
            continue;
        };
        if !pipeline_set {
            pipeline_set = true;
            command_encoder.setRenderPipelineState(&renderer.polychrome_sprites_pipeline_state);
            // SAFETY: Six unit vertices and the scene's sprites match the shader
            // layouts and slots. Metal retains the buffers, `writer.finish`
            // publishes their writes, and the viewport bytes are copied.
            unsafe {
                command_encoder.setVertexBuffer_offset_atIndex(
                    Some(&renderer.unit_vertices),
                    0,
                    SpriteInputIndex::Vertices as usize,
                );
                command_encoder.setVertexBuffer_offset_atIndex(
                    Some(&polychrome_sprites.buffer),
                    polychrome_sprites.offset,
                    SpriteInputIndex::Sprites as usize,
                );
                command_encoder.setVertexBytes_length_atIndex(
                    ptr::NonNull::from(&viewport_size).cast(),
                    mem::size_of_val(&viewport_size),
                    SpriteInputIndex::ViewportSize as usize,
                );
                command_encoder.setFragmentBuffer_offset_atIndex(
                    Some(&polychrome_sprites.buffer),
                    polychrome_sprites.offset,
                    SpriteInputIndex::Sprites as usize,
                );
            }
        }
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        // SAFETY: The size is copied from a live value; the tile's texture
        // matches the sprite texture slot and Metal retains it; the run lies
        // within the scene's polychrome sprites.
        unsafe {
            command_encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&texture_size).cast(),
                mem::size_of_val(&texture_size),
                SpriteInputIndex::AtlasTextureSize as usize,
            );
            command_encoder
                .setFragmentTexture_atIndex(Some(texture), SpriteInputIndex::AtlasTexture as usize);
            command_encoder.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(
                MTLPrimitiveType::Triangle,
                0,
                6,
                run.len(),
                run.start,
            );
        }
    }
    true
}
