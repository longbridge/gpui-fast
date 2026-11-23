//! Metal frame encoding for retained tiles and nested rounded clips.
use super::{
    CFRetained, CVMetalTexture, DevicePixels, GlobalInputIndex, InstanceBindings,
    InstanceBufferWriter, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLRenderCommandEncoder, MTLTexture, MTLViewport, MetalRenderer, PaintedPath, PrimitiveBatch,
    ProtocolObject, Result, Retained, Scene, Size, mem, new_command_encoder_for_texture, ptr,
    quad_depth, quad_depth_scale,
};
use anyhow::Context as _;

impl MetalRenderer {
    pub(super) fn fast_frame(
        &mut self,
        scene: &Scene,
        instance_bindings: &InstanceBindings,
        writer: &mut InstanceBufferWriter,
        texture: &ProtocolObject<dyn MTLTexture>,
        viewport_size: Size<DevicePixels>,
        surface_textures: &mut Vec<CFRetained<CVMetalTexture>>,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>> {
        if scene.layers.frames.is_empty() {
            return self.draw_primitives_to_texture(
                scene,
                instance_bindings,
                texture,
                viewport_size,
                surface_textures,
            );
        }
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue
            .commandBuffer()
            .context("creating command buffer")?;
        let alpha = if self.opaque { 1. } else { 0. };
        crate::fast::dynamic_textures::encode_uploads(&command_buffer);
        self.encode_fast_scene(
            scene,
            instance_bindings,
            writer,
            texture,
            viewport_size,
            &command_buffer,
            MTLClearColor {
                red: 0.,
                green: 0.,
                blue: 0.,
                alpha,
            },
            surface_textures,
        )?;
        Ok(command_buffer)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_fast_scene(
        &mut self,
        scene: &Scene,
        instance_bindings: &InstanceBindings,
        writer: &mut InstanceBufferWriter,
        texture: &ProtocolObject<dyn MTLTexture>,
        viewport_size: Size<DevicePixels>,
        command_buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        clear: MTLClearColor,
        surface_textures: &mut Vec<CFRetained<CVMetalTexture>>,
    ) -> Result<()> {
        let depth_texture = self.depth_texture_matching(texture)?;
        let encoder = new_command_encoder_for_texture(
            command_buffer,
            texture,
            &depth_texture,
            viewport_size,
            clear,
        )?;
        crate::fast::rounded_clip::default_mask(&encoder);
        let result = self.encode_fast_batches(
            scene,
            instance_bindings,
            writer,
            viewport_size,
            &encoder,
            &mut Vec::new(),
            surface_textures,
            None,
        );
        encoder.endEncoding();
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn encode_fast_batches(
        &mut self,
        scene: &Scene,
        instance_bindings: &InstanceBindings,
        writer: &mut InstanceBufferWriter,
        viewport_size: Size<DevicePixels>,
        command_encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
        clips: &mut Vec<crate::fast::rounded_clip::Clip>,
        surface_textures: &mut Vec<CFRetained<CVMetalTexture>>,
        parent_depth: Option<f64>,
    ) -> Result<()> {
        self.fast_layers.note_composited(scene);
        let depth_scale = if parent_depth.is_some() {
            0.
        } else {
            quad_depth_scale(scene.quads.len())
        };
        // SAFETY: Metal copies both values, which match every vertex shader's
        // `GlobalInputIndex_ViewportSize` and `GlobalInputIndex_DepthScale` arguments.
        unsafe {
            command_encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&viewport_size).cast(),
                mem::size_of_val(&viewport_size),
                GlobalInputIndex::ViewportSize as usize,
            );
            command_encoder.setVertexBytes_length_atIndex(
                ptr::NonNull::from(&depth_scale).cast(),
                mem::size_of_val(&depth_scale),
                GlobalInputIndex::DepthScale as usize,
            );
        }

        if parent_depth.is_none() {
            self.draw_opaque_quads(scene.quads.len(), instance_bindings, command_encoder);
        }
        command_encoder.setDepthStencilState(Some(&self.depth_test_state));

        let mut quad_cursor: u32 = 0;
        let mut path_tile_cursor = 0;
        for batch in scene.batches() {
            // Quad shaders assign each quad its own depth.
            // In every other batch type, the depth is fixed to the position of the quad cursor.
            let (znear, zfar) = if let Some(depth) = parent_depth {
                (depth, depth)
            } else if matches!(&batch, PrimitiveBatch::Quads(_)) {
                (0.0, 1.0)
            } else {
                let depth = quad_depth(quad_cursor, depth_scale) as f64;
                (depth, depth)
            };
            command_encoder.setViewport(MTLViewport {
                originX: 0.0,
                originY: 0.0,
                width: i32::from(viewport_size.width) as f64,
                height: i32::from(viewport_size.height) as f64,
                znear,
                zfar,
            });

            match batch {
                PrimitiveBatch::Shadows(range) => {
                    self.draw_shadows(range, instance_bindings, command_encoder)
                }
                PrimitiveBatch::Quads(range) => {
                    quad_cursor += range.len() as u32;
                    self.draw_quads(range, instance_bindings, command_encoder)
                }
                PrimitiveBatch::Paths(range) => {
                    let tiles_start = path_tile_cursor;
                    path_tile_cursor += scene.paths[range]
                        .iter()
                        .map(PaintedPath::tile_count)
                        .sum::<usize>();
                    self.draw_path_tiles(
                        tiles_start..path_tile_cursor,
                        instance_bindings,
                        command_encoder,
                    )
                }
                PrimitiveBatch::Underlines(range) => {
                    self.draw_underlines(range, instance_bindings, command_encoder)
                }
                PrimitiveBatch::MonochromeSprites { texture_id, range } => self
                    .draw_monochrome_sprites(texture_id, range, instance_bindings, command_encoder),
                PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                    if crate::fast::rounded_clip::draw_group(
                        self,
                        scene,
                        texture_id,
                        range.clone(),
                        writer,
                        viewport_size,
                        command_encoder,
                        clips,
                        surface_textures,
                        znear,
                    )? {
                        // The nested scene used its own depth constants and tile runs.
                        unsafe {
                            command_encoder.setVertexBytes_length_atIndex(
                                ptr::NonNull::from(&depth_scale).cast(),
                                mem::size_of_val(&depth_scale),
                                GlobalInputIndex::DepthScale as usize,
                            );
                        }
                        self.fast_layers.note_composited(scene);
                    } else {
                        self.draw_polychrome_sprites(
                            texture_id,
                            range,
                            instance_bindings,
                            command_encoder,
                        );
                    }
                }
                PrimitiveBatch::Surfaces(range) => {
                    self.draw_surfaces(
                        &scene.surfaces[range.clone()],
                        range.start,
                        instance_bindings,
                        command_encoder,
                        surface_textures,
                    )?;
                }
                PrimitiveBatch::NativeViews(range) => {
                    self.draw_native_views(range, instance_bindings, command_encoder)
                }
                PrimitiveBatch::SubpixelSprites { .. } => unreachable!(),
            }
        }

        Ok(())
    }
}
