//! Rounded scopes draw into the current colour target with independent mask
//! textures. A child's mask is evaluated under its parent's mask. A mask
//! stays unchanged while its scope is active; siblings reuse its storage in
//! later render passes. Paths are masked on composition,
//! and all the other primitive pipelines apply the same coverage in their
//! fragment shader.

use anyhow::{Context as _, Result};
use gpui::{AtlasTextureId, DevicePixels, LayerFrame, PrimitiveBatch, Scene, Size};

use crate::metal_renderer::{
    InstanceBindings, InstanceBufferWriter, MetalRenderer, new_command_encoder_for_texture,
    write_instances,
};

const CLIP_TEXTURE_BASE: u32 = 0xF100_0000;

#[derive(Default)]
pub(crate) struct MaskPool(Vec<MaskSlot>);

struct MaskSlot {
    depth: usize,
    size: Size<DevicePixels>,
    texture: metal::Texture,
    used: bool,
}

impl MaskPool {
    fn get(
        &mut self,
        device: &metal::DeviceRef,
        size: Size<DevicePixels>,
        depth: usize,
    ) -> metal::Texture {
        let index = self
            .0
            .iter()
            .position(|slot| slot.depth == depth && slot.size == size)
            .unwrap_or_else(|| {
                let descriptor = metal::TextureDescriptor::new();
                descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
                descriptor.set_texture_type(metal::MTLTextureType::D2);
                descriptor.set_width(size.width.0 as u64);
                descriptor.set_height(size.height.0 as u64);
                descriptor.set_storage_mode(metal::MTLStorageMode::Private);
                descriptor.set_usage(
                    metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead,
                );
                let texture = device.new_texture(&descriptor);
                texture.set_label("rounded_overflow_mask");
                self.0.push(MaskSlot {
                    depth,
                    size,
                    texture,
                    used: true,
                });
                self.0.len() - 1
            });
        self.0[index].used = true;
        self.0[index].texture.clone()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

pub(crate) fn begin_frame(renderer: &mut MetalRenderer) {
    renderer.fast_rounded_clip.0.retain(|slot| slot.used);
    for slot in &mut renderer.fast_rounded_clip.0 {
        slot.used = false;
    }
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
    bind(encoder, None);
}

fn bind(encoder: &metal::RenderCommandEncoderRef, mask: Option<&metal::TextureRef>) {
    let enabled = u32::from(mask.is_some());
    encoder.set_fragment_bytes(
        29,
        size_of_val(&enabled) as u64,
        &enabled as *const u32 as *const _,
    );
    encoder.set_fragment_texture(15, mask);
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
    encode(
        renderer,
        scene,
        bindings,
        writer,
        texture,
        viewport,
        command_buffer,
        None,
        Some(metal::MTLClearColor::new(0., 0., 0., alpha)),
        0,
    )?;
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
        encode(
            renderer,
            scene,
            &bindings,
            writer,
            texture,
            viewport,
            commands,
            None,
            Some(background),
            0,
        )
    })();
    renderer.path_intermediate_texture = old_texture;
    renderer.path_intermediate_msaa_texture = old_msaa;
    result
}

#[allow(clippy::too_many_arguments)]
fn encode(
    renderer: &mut MetalRenderer,
    scene: &Scene,
    bindings: &InstanceBindings,
    writer: &mut InstanceBufferWriter,
    texture: &metal::TextureRef,
    viewport: Size<DevicePixels>,
    commands: &metal::CommandBufferRef,
    mask: Option<&metal::TextureRef>,
    clear: Option<metal::MTLClearColor>,
    depth: usize,
) -> Result<()> {
    renderer.fast_layers.note_composited(scene);
    let mut encoder = new_command_encoder_for_texture(commands, texture, viewport, clear);
    for batch in scene.batches() {
        bind(encoder, mask);
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
                    let mask_scene = child
                        .layers
                        .frames
                        .iter()
                        .find(|frame| frame.key.0 == u32::MAX)
                        .and_then(|frame| frame.content.scene())
                        .context("rounded clip mask")?;
                    for _ in range {
                        encoder.end_encoding();
                        let child_mask =
                            renderer
                                .fast_rounded_clip
                                .get(&renderer.device, viewport, depth);
                        let mask_bindings = write_instances(mask_scene, writer)?;
                        encode(
                            renderer,
                            mask_scene,
                            &mask_bindings,
                            writer,
                            &child_mask,
                            viewport,
                            commands,
                            mask,
                            Some(metal::MTLClearColor::new(0., 0., 0., 0.)),
                            depth + 1,
                        )?;
                        let child_bindings = write_instances(child, writer)?;
                        encode(
                            renderer,
                            child,
                            &child_bindings,
                            writer,
                            texture,
                            viewport,
                            commands,
                            Some(&child_mask),
                            None,
                            depth + 1,
                        )?;
                        renderer.fast_layers.note_composited(scene);
                        encoder =
                            new_command_encoder_for_texture(commands, texture, viewport, None);
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
                bind(encoder, mask);
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
    encoder.end_encoding();
    Ok(())
}
