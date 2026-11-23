//! The global uniforms of every scene a frame draws, written only when they
//! change.
//!
//! Upstream's renderer draws one scene per frame and writes its globals with a
//! `Queue::write_buffer` every frame. A fast frame also draws the scenes of
//! scroll layer tiles and of rounded clip scopes, each with its own quad count,
//! depth scale and viewport, so each gets a slot of one uniform buffer and a
//! bind group kept from frame to frame. A frame whose scenes' globals are the
//! last frame's writes nothing.

use std::num::NonZeroU64;

use crate::fast::frame::FrameTarget;
use crate::wgpu_renderer::GlobalParams;

const GLOBALS: u64 = size_of::<GlobalParams>() as u64;

#[derive(Default)]
pub(crate) struct SceneGlobals {
    buffer: Option<wgpu::Buffer>,
    /// The gamma buffer the bind groups were created with.
    gamma: Option<wgpu::Buffer>,
    stride: u64,
    bind_groups: Vec<wgpu::BindGroup>,
    uploaded: Vec<u8>,
    bytes: Vec<u8>,
}

impl SceneGlobals {
    /// Makes slot `i` of the buffer hold `globals[i]`, for every scene.
    pub(crate) fn write(&mut self, target: &FrameTarget, globals: &[GlobalParams]) {
        let device = target.device;
        let stride =
            GLOBALS.next_multiple_of(device.limits().min_uniform_buffer_offset_alignment as u64);
        let slots = globals.len().max(1);
        let stale = self.stride != stride
            || self.gamma.as_ref() != Some(target.globals_buffer)
            || self.bind_groups.len() < slots;
        if stale {
            let capacity = slots.next_power_of_two().max(self.bind_groups.len());
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("scene_globals_buffer"),
                size: stride * capacity as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.bind_groups = (0..capacity)
                .map(|slot| {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("scene_globals_bind_group"),
                        layout: &target.bind_group_layouts.globals,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: &buffer,
                                    offset: slot as u64 * stride,
                                    size: NonZeroU64::new(GLOBALS),
                                }),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: target.globals_buffer,
                                    offset: target.gamma_offset,
                                    size: NonZeroU64::new(target.gamma_size),
                                }),
                            },
                        ],
                    })
                })
                .collect();
            self.buffer = Some(buffer);
            self.gamma = Some(target.globals_buffer.clone());
            self.stride = stride;
            self.uploaded.clear();
        }

        self.bytes.clear();
        self.bytes.resize(stride as usize * globals.len(), 0);
        for (index, globals) in globals.iter().enumerate() {
            let offset = index * stride as usize;
            self.bytes[offset..offset + GLOBALS as usize]
                .copy_from_slice(bytemuck::bytes_of(globals));
        }
        if self.bytes == self.uploaded || self.bytes.is_empty() {
            return;
        }
        if let Some(buffer) = &self.buffer {
            target.queue.write_buffer(buffer, 0, &self.bytes);
        }
        std::mem::swap(&mut self.uploaded, &mut self.bytes);
    }

    /// The bind group of scene `index`'s globals, as last written.
    pub(crate) fn bind_group(&self, index: usize) -> &wgpu::BindGroup {
        &self.bind_groups[index]
    }
}

/// Depth attachments by size, kept while frames use them.
#[derive(Default)]
pub(crate) struct DepthTargets {
    targets: Vec<(u32, u32, wgpu::Texture, wgpu::TextureView, bool)>,
}

impl DepthTargets {
    /// Starts a frame: forgets which targets the last frame used.
    pub(crate) fn begin_frame(&mut self) {
        self.targets.retain(|target| target.4);
        for target in &mut self.targets {
            target.4 = false;
        }
    }

    /// Makes a depth target of `width` by `height` ready for the frame.
    pub(crate) fn ensure(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if let Some(target) = self
            .targets
            .iter_mut()
            .find(|target| target.0 == width && target.1 == height)
        {
            target.4 = true;
            return;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fast_depth_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: crate::wgpu_renderer::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.targets.push((width, height, texture, view, true));
    }

    pub(crate) fn view(&self, width: u32, height: u32) -> Option<&wgpu::TextureView> {
        self.targets
            .iter()
            .find(|target| target.0 == width && target.1 == height)
            .map(|target| &target.3)
    }
}
