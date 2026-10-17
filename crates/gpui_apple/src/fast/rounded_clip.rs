//! Evaluate independent rounded padding edges directly in colour fragments.
//! Nested scopes append geometry and siblings restore the enclosing stack.
//! Only actual path batches restart the colour pass; rounded containers do
//! not allocate textures or encode mask render passes.

use anyhow::{Context as _, Result};
use gpui::{AtlasTextureId, DevicePixels, LayerFrame, PrimitiveBatch, Scene, Size};

use crate::metal_renderer::{
    InstanceBindings, InstanceBufferWriter, MetalRenderer, new_command_encoder_for_texture,
    write_instances,
};

const CLIP_TEXTURE_BASE: u32 = 0xF100_0000;

/// Geometry is copied into Metal's command stream, never retained in a shared
/// mutable texture. A stack remains valid when sibling scopes overlap.
#[repr(C)]
#[derive(Clone, Copy)]
struct Clip {
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

pub(crate) fn has_groups(scene: &Scene) -> bool {
    scene
        .polychrome_sprites
        .iter()
        .any(|sprite| group(scene, sprite.tile.texture_id).is_some())
}

pub(crate) fn default_mask(encoder: &metal::RenderCommandEncoderRef) {
    let count = 0u32;
    encoder.set_fragment_bytes(29, 4, &count as *const u32 as *const _);
    let empty = [0f32; 12];
    encoder.set_fragment_bytes(28, 48, empty.as_ptr().cast());
}

fn bind(encoder: &metal::RenderCommandEncoderRef, clips: &[Clip], device: &metal::DeviceRef) {
    let count = clips.len() as u32;
    encoder.set_fragment_bytes(29, 4, &count as *const u32 as *const _);
    if !clips.is_empty() {
        let length = std::mem::size_of_val(clips) as u64;
        if length <= 4096 {
            encoder.set_fragment_bytes(28, length, clips.as_ptr().cast());
        } else {
            let buffer = device.new_buffer_with_data(
                clips.as_ptr().cast(),
                length,
                metal::MTLResourceOptions::StorageModeShared,
            );
            encoder.set_fragment_buffer(28, Some(&buffer), 0);
        }
    }
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

pub(crate) fn draw(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    bindings: &InstanceBindings,
    writer: &mut InstanceBufferWriter,
    texture: &metal::TextureRef,
    viewport: Size<DevicePixels>,
) -> Result<metal::CommandBuffer> {
    let queue = renderer.command_queue.clone();
    let command_buffer = queue.new_command_buffer();
    let alpha = if renderer.opaque { 1. } else { 0. };
    let encoder = new_command_encoder_for_texture(
        command_buffer,
        texture,
        viewport,
        Some(metal::MTLClearColor::new(0., 0., 0., alpha)),
    );
    encode(
        renderer,
        scene,
        bindings,
        writer,
        texture,
        viewport,
        command_buffer,
        &mut Vec::new(),
        encoder,
    )?
    .end_encoding();
    Ok(command_buffer.to_owned())
}

pub(crate) fn draw_tile(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    writer: &mut InstanceBufferWriter,
    texture: &metal::TextureRef,
    viewport: Size<DevicePixels>,
    commands: &metal::CommandBufferRef,
    background: metal::MTLClearColor,
) -> Result<()> {
    let old_texture = renderer.path_intermediate_texture.take();
    let old_msaa = renderer.path_intermediate_msaa_texture.take();
    renderer.update_path_intermediate_textures(viewport);
    let result = (|| {
        let bindings = write_instances(scene, writer)?;
        let encoder =
            new_command_encoder_for_texture(commands, texture, viewport, Some(background));
        encode(
            renderer,
            scene,
            &bindings,
            writer,
            texture,
            viewport,
            commands,
            &mut Vec::new(),
            encoder,
        )?
        .end_encoding();
        Ok(())
    })();
    renderer.path_intermediate_texture = old_texture;
    renderer.path_intermediate_msaa_texture = old_msaa;
    result
}

#[allow(clippy::too_many_arguments)]
fn encode<'a>(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    bindings: &InstanceBindings,
    writer: &mut InstanceBufferWriter,
    texture: &'a metal::TextureRef,
    viewport: Size<DevicePixels>,
    commands: &'a metal::CommandBufferRef,
    clips: &mut Vec<Clip>,
    mut encoder: &'a metal::RenderCommandEncoderRef,
) -> Result<&'a metal::RenderCommandEncoderRef> {
    renderer.fast_layers.note_composited(scene);
    for batch in scene.batches() {
        bind(encoder, clips, &renderer.device);
        match batch {
            PrimitiveBatch::Shadows(range) => {
                renderer.draw_shadows(range, bindings, viewport, encoder)
            }
            PrimitiveBatch::Quads(range) => renderer.draw_quads(range, bindings, viewport, encoder),
            PrimitiveBatch::Underlines(range) => {
                renderer.draw_underlines(range, bindings, viewport, encoder)
            }
            PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                renderer.draw_monochrome_sprites(texture_id, range, bindings, viewport, encoder)
            }
            PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                if let Some(frame) = group(scene, texture_id) {
                    let child = frame.content.scene().context("rounded clip content")?;
                    let radii = child
                        .layers
                        .frames
                        .iter()
                        .find(|frame| frame.key.0 == u32::MAX)
                        .context("rounded clip geometry")?
                        .background;
                    for index in range {
                        let sprite = &scene.polychrome_sprites[index];
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
                        let child_bindings = write_instances(child, writer)?;
                        encoder = encode(
                            renderer,
                            child,
                            &child_bindings,
                            writer,
                            texture,
                            viewport,
                            commands,
                            clips,
                            encoder,
                        )?;
                        clips.pop();
                        renderer.fast_layers.note_composited(scene);
                    }
                } else {
                    renderer
                        .draw_polychrome_sprites(texture_id, range, bindings, viewport, encoder);
                }
            }
            PrimitiveBatch::Paths(range) => {
                encoder.end_encoding();
                let paths = &scene.paths[range];
                let drawn =
                    renderer.draw_paths_to_intermediate(paths, writer, viewport, commands)?;
                encoder = new_command_encoder_for_texture(commands, texture, viewport, None);
                bind(encoder, clips, &renderer.device);
                if drawn {
                    renderer.draw_paths_from_intermediate(paths, writer, viewport, encoder)?;
                }
            }
            PrimitiveBatch::Surfaces(range) => renderer.draw_surfaces(
                &scene.surfaces[range.clone()],
                range.start,
                bindings,
                viewport,
                encoder,
            ),
            PrimitiveBatch::SubpixelSprites { .. } => unreachable!("Metal uses grayscale glyphs"),
        }
    }
    Ok(encoder)
}
