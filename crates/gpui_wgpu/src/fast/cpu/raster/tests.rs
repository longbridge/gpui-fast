//! The CPU rasterizer against the GPU renderer: scenes of every primitive
//! kind and shader branch, and seeded random scenes, drawn by both and
//! compared pixel by pixel; and region and threaded drawing against
//! whole-frame drawing.
//!
//! [`Harness`] draws scenes without a window, through the renderer's own
//! pipelines and `fast::frame` recording, as the layer pixel tests do, and
//! keeps a copy of every sprite it uploads for the CPU to sample.
//!
//! The GPU's own interpolation, filtering, transcendental functions and
//! blending differ from `f32` on the CPU in the last bits, so a pixel may
//! come out a level or two apart; antialiased path edge pixels, whose
//! coverage is resolved from 4 samples each holding an 8-bit value, up to 8.

use std::borrow::Cow;
use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::Arc;

use anyhow::Result;
use gpui::{
    AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Background, BorderStyle, Bounds,
    ContentMask, Corners, DevicePixels, Edges, FontId, GlyphId, Hsla, ImageId, LayerFrame,
    LayerKey, MonochromeSprite, Path, PlatformAtlas, PolychromeSprite, Quad, Radians,
    RenderGlyphParams, RenderImageParams, Rgba, ScaledPixels, Scene, Shadow, Size, SubpixelSprite,
    TileCoord, TransformationMatrix, Underline, checkerboard, layer_tile_id, layer_tile_texture_id,
    linear_color_stop, linear_gradient, pattern_slash, point, px, rgba, size,
};

use super::{Canvas, RasterParams, SpritePixels, TexturePixels, draw};
use crate::WgpuAtlas;
use crate::fast::frame::{FrameHost, FrameState, FrameTarget};
use crate::wgpu_renderer::{
    GammaParams, GlobalParams, RenderingParameters, WgpuBindGroupLayouts, WgpuPipelines,
    WgpuRendererCore,
};

// --- comparisons --- //

/// How two frames differ.
#[derive(Debug, Default, Clone, Copy)]
struct Difference {
    /// The largest difference of each channel: red, green, blue, alpha.
    max: [u8; 4],
    /// The pixels that differ at all.
    differing: usize,
    /// The pixels that differ by more than 2 in some channel.
    beyond_two: usize,
    pixels: usize,
    /// Where the largest difference is.
    worst: (u32, u32),
    worst_pixels: (u32, u32),
}

impl Difference {
    fn largest(&self) -> u8 {
        *self.max.iter().max().unwrap()
    }

    fn merge(&mut self, other: &Difference) {
        if other.largest() > self.largest() {
            self.worst = other.worst;
            self.worst_pixels = other.worst_pixels;
        }
        for i in 0..4 {
            self.max[i] = self.max[i].max(other.max[i]);
        }
        self.differing += other.differing;
        self.beyond_two += other.beyond_two;
        self.pixels += other.pixels;
    }

    fn report(&self, label: &str) -> String {
        format!(
            "{label}: max diff r{} g{} b{} a{}, {} of {} pixels differ ({:.4}%), {} by more than 2; worst at {:?}: cpu {:08x} gpu {:08x}",
            self.max[0],
            self.max[1],
            self.max[2],
            self.max[3],
            self.differing,
            self.pixels,
            100. * self.differing as f64 / self.pixels.max(1) as f64,
            self.beyond_two,
            self.worst,
            self.worst_pixels.0,
            self.worst_pixels.1,
        )
    }
}

fn compare(cpu: &[u32], gpu: &[u32], width: u32) -> Difference {
    assert_eq!(cpu.len(), gpu.len());
    let mut difference = Difference {
        pixels: cpu.len(),
        ..Default::default()
    };
    let mut worst = 0;
    for (index, (&c, &g)) in cpu.iter().zip(gpu).enumerate() {
        if c == g {
            continue;
        }
        difference.differing += 1;
        let mut largest = 0;
        for (channel, shift) in [16, 8, 0, 24].into_iter().enumerate() {
            let d = ((c >> shift & 0xff) as i32 - (g >> shift & 0xff) as i32).unsigned_abs() as u8;
            difference.max[channel] = difference.max[channel].max(d);
            largest = largest.max(d);
        }
        if largest > 2 {
            difference.beyond_two += 1;
        }
        if largest > worst {
            worst = largest;
            difference.worst = (index as u32 % width, index as u32 / width);
            difference.worst_pixels = (c, g);
        }
    }
    difference
}

// --- the harness --- //

const INITIAL_INSTANCE_CAPACITY: u64 = 2 * 1024 * 1024;

/// The pipelines of one target alpha mode and subpixel blending.
struct Pipelines {
    premultiplied: bool,
    dual_source_blending: bool,
    pipelines: WgpuPipelines,
}

/// Draws scenes into textures without a surface, through the renderer's
/// pipelines and frame recording, and keeps the bytes of every sprite it
/// uploads, where it uploads them.
struct Harness {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    format: wgpu::TextureFormat,
    atlas: WgpuAtlas,
    mirror: Mirror,
    pipelines: Vec<Pipelines>,
    current: usize,
    bind_group_layouts: WgpuBindGroupLayouts,
    atlas_sampler: wgpu::Sampler,
    rendering_params: RenderingParameters,
    /// The adapter's PCI vendor.
    vendor: u32,
    globals_buffer: wgpu::Buffer,
    path_globals_offset: u64,
    gamma_offset: u64,
    globals_bind_group: wgpu::BindGroup,
    path_globals_bind_group: wgpu::BindGroup,
    instance_buffer: wgpu::Buffer,
    instance_capacity: u64,
    instance_alignment: u64,
    path_targets: Option<(
        Size<DevicePixels>,
        wgpu::Texture,
        wgpu::TextureView,
        Option<(wgpu::Texture, wgpu::TextureView)>,
    )>,
    state: FrameState,
    device_dual_source_blending: bool,
    next_key: u32,
}

/// The atlas textures as uploaded, for the CPU to sample.
#[derive(Default)]
struct Mirror {
    textures: HashMap<AtlasTextureId, MirrorTexture>,
}

struct MirrorTexture {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    data: Vec<u8>,
}

impl SpritePixels for Mirror {
    fn texture(&self, id: AtlasTextureId) -> Option<TexturePixels<'_>> {
        let texture = self.textures.get(&id)?;
        Some(TexturePixels {
            width: texture.width,
            height: texture.height,
            bytes_per_pixel: texture.bytes_per_pixel,
            data: &texture.data,
        })
    }
}

/// How a frame is drawn.
#[derive(Clone, Copy, Debug)]
struct Mode {
    /// A transparent window's: premultiplied blending.
    premultiplied: bool,
    dual_source_blending: bool,
    is_bgr: bool,
}

const OPAQUE: Mode = Mode {
    premultiplied: false,
    dual_source_blending: true,
    is_bgr: false,
};

const PREMULTIPLIED: Mode = Mode {
    premultiplied: true,
    dual_source_blending: true,
    is_bgr: false,
};

impl Harness {
    fn new() -> Option<Harness> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });
        let adapter = gpui::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .ok()?;
        let (device, queue, dual_source_blending, color_texture_format) =
            gpui::block_on(crate::WgpuContext::create_device(&adapter)).ok()?;
        let (device, queue) = (Arc::new(device), Arc::new(queue));

        let usages = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC;
        let format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|format| {
            adapter
                .get_texture_format_features(*format)
                .allowed_usages
                .contains(usages)
        })?;

        let rendering_params = RenderingParameters::new(&adapter, format);
        let bind_group_layouts = WgpuRendererCore::create_bind_group_layouts(&device, false);
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_alignment = device.limits().min_uniform_buffer_offset_alignment as u64;
        let globals_size = size_of::<GlobalParams>() as u64;
        let gamma_size = size_of::<GammaParams>() as u64;
        let path_globals_offset = globals_size.next_multiple_of(uniform_alignment);
        let gamma_offset = (path_globals_offset + globals_size).next_multiple_of(uniform_alignment);
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals_buffer"),
            size: gamma_offset + gamma_size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = |label: &str, offset: u64| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &bind_group_layouts.globals,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &globals_buffer,
                            offset,
                            size: NonZeroU64::new(globals_size),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &globals_buffer,
                            offset: gamma_offset,
                            size: NonZeroU64::new(gamma_size),
                        }),
                    },
                ],
            })
        };
        let path_globals_bind_group =
            globals_bind_group("path_globals_bind_group", path_globals_offset);
        let globals_bind_group = globals_bind_group("globals_bind_group", 0);

        let instance_alignment = device.limits().min_storage_buffer_offset_alignment as u64;
        let instance_buffer = create_instance_buffer(&device, INITIAL_INSTANCE_CAPACITY);
        let atlas = WgpuAtlas::new(device.clone(), queue.clone(), color_texture_format);

        Some(Harness {
            device,
            queue,
            format,
            atlas,
            mirror: Mirror::default(),
            pipelines: Vec::new(),
            current: 0,
            bind_group_layouts,
            atlas_sampler,
            rendering_params,
            vendor: adapter.get_info().vendor,
            globals_buffer,
            path_globals_offset,
            gamma_offset,
            globals_bind_group,
            path_globals_bind_group,
            instance_buffer,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instance_alignment,
            path_targets: None,
            state: FrameState::default(),
            device_dual_source_blending: dual_source_blending,
            next_key: 1,
        })
    }

    /// Whether the adapter is NVIDIA's, which the raster was measured
    /// against to the last bit. Elsewhere (measured on Intel with Mesa) it
    /// matches but for two rounding edges: texture filtering under rotation
    /// can land a level further apart, and a division landing exactly on a
    /// whole number (a checkerboard cell's edge on a pixel center) can come
    /// out just below it, moving the edge a pixel.
    fn is_nvidia(&self) -> bool {
        self.vendor == 0x10de
    }

    /// The levels sampled sprites may differ by: see [`Harness::is_nvidia`].
    fn sampling_tolerance(&self) -> u8 {
        if self.is_nvidia() { 2 } else { 3 }
    }

    /// The parameters the CPU draws with to match the GPU in `mode`.
    fn params(&self, mode: Mode) -> RasterParams {
        RasterParams {
            gamma_ratios: self.rendering_params.gamma_ratios,
            grayscale_enhanced_contrast: self.rendering_params.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: self.rendering_params.subpixel_enhanced_contrast,
            is_bgr: mode.is_bgr,
            premultiplied_alpha: mode.premultiplied,
            dual_source_blending: mode.dual_source_blending && self.device_dual_source_blending,
            path_sample_count: 4,
            fragment_bits: super::fragment_bits(self.vendor),
        }
    }

    fn use_pipelines(&mut self, premultiplied: bool, dual_source_blending: bool) {
        let dual_source_blending = dual_source_blending && self.device_dual_source_blending;
        if let Some(index) = self.pipelines.iter().position(|p| {
            p.premultiplied == premultiplied && p.dual_source_blending == dual_source_blending
        }) {
            self.current = index;
            return;
        }
        let pipelines = WgpuRendererCore::create_pipelines(
            &self.device,
            &self.bind_group_layouts,
            self.format,
            if premultiplied {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                wgpu::CompositeAlphaMode::Opaque
            },
            self.rendering_params.path_sample_count,
            dual_source_blending,
            false,
        );
        self.pipelines.push(Pipelines {
            premultiplied,
            dual_source_blending,
            pipelines,
        });
        self.current = self.pipelines.len() - 1;
    }

    /// Draws `scene` on the GPU into a `width` × `height` texture cleared to
    /// transparent, and returns its pixels as `0xAARRGGBB`.
    fn gpu(&mut self, scene: &Scene, width: u32, height: u32, mode: Mode) -> Vec<u32> {
        self.use_pipelines(mode.premultiplied, mode.dual_source_blending);
        self.write_globals(width, height, mode);
        self.ensure_path_targets(size(
            DevicePixels(width as i32),
            DevicePixels(height as i32),
        ));
        self.atlas.before_frame();
        // Tiles are rasterized anew for every frame: a cached tile could
        // have been drawn in another mode.
        self.state = FrameState::default();

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("harness_target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        crate::fast::frame::record_into(self, scene, &view, wgpu::Color::TRANSPARENT)
            .expect("frame recorded");
        let rgba = read_back(&self.device, &self.queue, &texture, self.format);
        rgba.chunks_exact(4)
            .map(|p| (p[3] as u32) << 24 | (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32)
            .collect()
    }

    /// Draws `scene` on the CPU, whole.
    fn cpu(&self, scene: &Scene, width: u32, height: u32, mode: Mode, threads: usize) -> Vec<u32> {
        let mut canvas = Canvas::new(width, height);
        draw(
            &mut canvas,
            scene,
            &[device_bounds(0, 0, width as i32, height as i32)],
            &self.mirror,
            &self.params(mode),
            threads,
        );
        canvas.pixels().to_vec()
    }

    /// Draws `scene` both ways and compares.
    fn compare(&mut self, scene: &Scene, width: u32, height: u32, mode: Mode) -> Difference {
        let gpu = self.gpu(scene, width, height, mode);
        let cpu = self.cpu(scene, width, height, mode, 4);
        compare(&cpu, &gpu, width)
    }

    fn write_globals(&self, width: u32, height: u32, mode: Mode) {
        let globals = GlobalParams {
            viewport_size: [width as f32, height as f32],
            premultiplied_alpha: mode.premultiplied as u32,
            pad: 0,
        };
        let path_globals = GlobalParams {
            premultiplied_alpha: 0,
            ..globals
        };
        let gamma = GammaParams {
            gamma_ratios: self.rendering_params.gamma_ratios,
            grayscale_enhanced_contrast: self.rendering_params.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: self.rendering_params.subpixel_enhanced_contrast,
            is_bgr: mode.is_bgr as u32,
            _pad: 0,
        };
        self.queue
            .write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));
        self.queue.write_buffer(
            &self.globals_buffer,
            self.path_globals_offset,
            bytemuck::bytes_of(&path_globals),
        );
        self.queue.write_buffer(
            &self.globals_buffer,
            self.gamma_offset,
            bytemuck::bytes_of(&gamma),
        );
    }

    fn ensure_path_targets(&mut self, size: Size<DevicePixels>) {
        if self
            .path_targets
            .as_ref()
            .is_some_and(|targets| targets.0 == size)
        {
            return;
        }
        let (width, height) = (size.width.0 as u32, size.height.0 as u32);
        let (intermediate, intermediate_view) =
            WgpuRendererCore::create_path_intermediate(&self.device, self.format, width, height);
        let msaa = WgpuRendererCore::create_msaa_if_needed(
            &self.device,
            self.format,
            width,
            height,
            self.rendering_params.path_sample_count,
        );
        self.path_targets = Some((size, intermediate, intermediate_view, msaa));
    }

    /// Uploads a sprite of `width` × `height` `bytes` to the atlas, where
    /// `key` puts it, and keeps a copy for the CPU.
    fn upload(&mut self, key: AtlasKey, width: i32, height: i32, bytes: Vec<u8>) -> AtlasTile {
        let kind = key.texture_kind();
        let tile = self
            .atlas
            .get_or_insert_with(key, &mut || {
                Ok(Some((
                    size(DevicePixels(width), DevicePixels(height)),
                    Cow::Owned(bytes.clone()),
                )))
            })
            .expect("sprite uploaded")
            .expect("sprite tile");
        let bytes_per_pixel = match kind {
            AtlasTextureKind::Monochrome => 1,
            _ => 4,
        };
        // Atlas textures are 1024 × 1024 unless a sprite is larger.
        const SIDE: u32 = 1024;
        let texture = self
            .mirror
            .textures
            .entry(tile.texture_id)
            .or_insert_with(|| MirrorTexture {
                width: SIDE,
                height: SIDE,
                bytes_per_pixel,
                data: vec![0; (SIDE * SIDE * bytes_per_pixel) as usize],
            });
        let origin = tile.bounds.origin;
        assert!(origin.x.0 + width <= SIDE as i32 && origin.y.0 + height <= SIDE as i32);
        let row = (width as u32 * bytes_per_pixel) as usize;
        for y in 0..height as usize {
            let to = ((origin.y.0 as usize + y) * SIDE as usize + origin.x.0 as usize)
                * bytes_per_pixel as usize;
            texture.data[to..to + row].copy_from_slice(&bytes[y * row..(y + 1) * row]);
        }
        tile
    }

    fn glyph(&mut self, width: i32, height: i32, subpixel: bool, bytes: Vec<u8>) -> AtlasTile {
        self.next_key += 1;
        let key = AtlasKey::Glyph(RenderGlyphParams {
            font_id: FontId(9_000),
            glyph_id: GlyphId(self.next_key),
            font_size: px(12.),
            subpixel_variant: point(0, 0),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering: subpixel,
            dilation: 0,
        });
        self.upload(key, width, height, bytes)
    }

    fn image(&mut self, width: i32, height: i32, bytes: Vec<u8>) -> AtlasTile {
        self.next_key += 1;
        let key = AtlasKey::Image(RenderImageParams {
            image_id: ImageId(self.next_key as usize),
            frame_index: 0,
        });
        self.upload(key, width, height, bytes)
    }
}

impl FrameHost for Harness {
    fn frame_state(&mut self) -> &mut FrameState {
        &mut self.state
    }

    fn instance_data_alignment(&self) -> u64 {
        self.instance_alignment.max(1)
    }

    fn reserve_instance_data(&mut self, size: u64) -> Result<()> {
        if size > self.instance_capacity {
            self.instance_capacity = size.next_power_of_two();
            self.instance_buffer = create_instance_buffer(&self.device, self.instance_capacity);
        }
        Ok(())
    }

    fn target(&self) -> Result<FrameTarget<'_>> {
        let path_targets = self.path_targets.as_ref();
        let pipelines = &self.pipelines[self.current];
        Ok(FrameTarget {
            device: &self.device,
            queue: &self.queue,
            pipelines: &pipelines.pipelines,
            bind_group_layouts: &self.bind_group_layouts,
            atlas: &self.atlas,
            atlas_sampler: &self.atlas_sampler,
            globals_bind_group: &self.globals_bind_group,
            path_globals_bind_group: &self.path_globals_bind_group,
            path_intermediate_view: path_targets.map(|targets| &targets.2),
            path_msaa_view: path_targets.and_then(|targets| targets.3.as_ref().map(|m| &m.1)),
            instance_buffer: &self.instance_buffer,
            globals_buffer: &self.globals_buffer,
            gamma_offset: self.gamma_offset,
            gamma_size: size_of::<GammaParams>() as u64,
            format: self.format,
            path_sample_count: self.rendering_params.path_sample_count,
            premultiplied_alpha: pipelines.premultiplied,
        })
    }
}

fn create_instance_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instance_buffer"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The pixels of `texture` as RGBA bytes, row by row.
fn read_back(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
) -> Vec<u8> {
    let (width, height) = (texture.width(), texture.height());
    let row = width * 4;
    let padded_row = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("harness_readback"),
        size: (padded_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("harness_readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |result| {
        result.expect("readback mapped")
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .expect("device polled");
    let mapped = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((row * height) as usize);
    for y in 0..height {
        let start = (y * padded_row) as usize;
        pixels.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    if format == wgpu::TextureFormat::Bgra8Unorm {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    pixels
}

// --- scene helpers --- //

fn sp(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(x), ScaledPixels(y)),
        size: size(ScaledPixels(w), ScaledPixels(h)),
    }
}

fn device_bounds(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
    Bounds {
        origin: point(DevicePixels(x), DevicePixels(y)),
        size: size(DevicePixels(w), DevicePixels(h)),
    }
}

fn no_mask() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: sp(-10_000., -10_000., 20_000., 20_000.),
    }
}

fn mask(x: f32, y: f32, w: f32, h: f32) -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: sp(x, y, w, h),
    }
}

fn color(hex: u32) -> Hsla {
    Hsla::from(rgba(hex))
}

fn quad(bounds: Bounds<ScaledPixels>, background: impl Into<Background>) -> Quad {
    Quad {
        bounds,
        content_mask: no_mask(),
        background: background.into(),
        ..Default::default()
    }
}

fn shadow(bounds: Bounds<ScaledPixels>, blur: f32, radius: f32, hex: u32) -> Shadow {
    Shadow {
        order: 0,
        blur_radius: ScaledPixels(blur),
        bounds,
        corner_radii: Corners::all(ScaledPixels(radius)),
        content_mask: no_mask(),
        color: color(hex),
        element_bounds: bounds,
        element_corner_radii: Corners::all(ScaledPixels(radius)),
        inset: 0,
        pad: 0,
    }
}

/// A deterministic pseudo-random sequence (xorshift64*).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `lo..hi`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() >> 40) as f32 / (1u64 << 24) as f32 * (hi - lo)
    }

    fn int(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo) as u64) as i32
    }

    fn chance(&mut self, p: f32) -> bool {
        self.range(0., 1.) < p
    }

    /// A coordinate: whole, half or arbitrary.
    fn coord(&mut self, lo: f32, hi: f32) -> f32 {
        let v = self.range(lo, hi);
        match self.int(0, 4) {
            0 | 1 => v.round(),
            2 => (v * 2.).round() / 2.,
            _ => v,
        }
    }

    fn color(&mut self) -> Hsla {
        Hsla {
            h: self.range(0., 1.),
            s: self.range(0., 1.),
            l: self.range(0., 1.),
            a: if self.chance(0.5) {
                1.
            } else {
                self.range(0.05, 1.)
            },
        }
    }

    fn background(&mut self) -> Background {
        match self.int(0, 10) {
            0..=4 => self.color().into(),
            5 => linear_gradient(
                self.range(0., 720.),
                linear_color_stop(self.color(), self.range(0., 0.4)),
                linear_color_stop(self.color(), self.range(0.6, 1.)),
            ),
            6 => linear_gradient(
                self.range(0., 360.),
                linear_color_stop(self.color(), self.range(0., 0.4)),
                linear_color_stop(self.color(), self.range(0.6, 1.)),
            )
            .color_space(gpui::ColorSpace::Oklab),
            7 => pattern_slash(self.color(), self.range(1., 6.), self.range(1., 6.)),
            8 => checkerboard(self.color(), self.int(2, 12) as f32),
            _ => self.color().into(),
        }
    }
}

/// Draws `scene`'s checkerboard backgrounds solid, for GPUs whose division
/// moves cells' edges (see [`Harness::is_nvidia`]).
fn solid_checkerboards(scene: &mut Scene) {
    for quad in &mut scene.quads {
        if let Some(color) = super::shade::checkerboard_color(&quad.background) {
            quad.background = color.into();
        }
    }
}

/// A sprite's size scaled from its tile's by up to 3 either way. Sampling
/// texels minified much further hangs on the last bits of the GPU's
/// interpolated coordinates.
fn scaled(rng: &mut Rng, width: i32, height: i32) -> (f32, f32) {
    let scale = |rng: &mut Rng, side: i32| rng.coord(side as f32 / 3., side as f32 * 3.).max(1.);
    (scale(rng, width), scale(rng, height))
}

fn random_glyph_bytes(rng: &mut Rng, width: i32, height: i32, channels: i32) -> Vec<u8> {
    (0..width * height * channels)
        .map(|_| match rng.int(0, 4) {
            0 => 0,
            1 => 255,
            _ => rng.int(0, 256) as u8,
        })
        .collect()
}

fn random_image_bytes(rng: &mut Rng, width: i32, height: i32) -> Vec<u8> {
    (0..width * height)
        .flat_map(|_| {
            let a = if rng.chance(0.3) {
                255
            } else {
                rng.int(0, 256) as u32
            };
            // Premultiplied BGRA, as images reach the atlas.
            let channel = |rng: &mut Rng| (rng.int(0, 256) as u32 * a / 255) as u8;
            [channel(rng), channel(rng), channel(rng), a as u8]
        })
        .collect()
}

fn random_quad(rng: &mut Rng, w: f32, h: f32) -> Quad {
    let x = rng.coord(-20., w);
    let y = rng.coord(-20., h);
    let qw = rng.coord(1., w / 2.);
    let qh = rng.coord(1., h / 2.);
    let max_radius = qw.min(qh) / 2.;
    let radius = |rng: &mut Rng| {
        if rng.chance(0.5) {
            0.
        } else {
            rng.coord(0., max_radius)
        }
    };
    let width = |rng: &mut Rng| {
        if rng.chance(0.4) {
            0.
        } else {
            rng.coord(0., 6.)
        }
    };
    let uniform_radius = rng.chance(0.4);
    let r = radius(rng);
    let corner_radii = if uniform_radius {
        Corners::all(ScaledPixels(r))
    } else {
        Corners {
            top_left: ScaledPixels(radius(rng)),
            top_right: ScaledPixels(radius(rng)),
            bottom_right: ScaledPixels(radius(rng)),
            bottom_left: ScaledPixels(radius(rng)),
        }
    };
    let has_border = rng.chance(0.6);
    let uniform_border = rng.chance(0.5);
    let bw = width(rng);
    let border_widths = if !has_border {
        Edges::default()
    } else if uniform_border {
        Edges::all(ScaledPixels(bw))
    } else {
        Edges {
            top: ScaledPixels(width(rng)),
            right: ScaledPixels(width(rng)),
            bottom: ScaledPixels(width(rng)),
            left: ScaledPixels(width(rng)),
        }
    };
    let mut border_style = if rng.chance(0.3) {
        BorderStyle::Dashed
    } else {
        BorderStyle::Solid
    };
    // A dashed border around rounded corners with a side of zero width
    // divides by a zero dash velocity along that side, where the pixels sit
    // exactly on a dash's end: whether the GPU draws them is rounding noise.
    let widths = [
        border_widths.top,
        border_widths.right,
        border_widths.bottom,
        border_widths.left,
    ];
    let rounded = corner_radii != Corners::default();
    if rounded && widths.iter().any(|w| w.0 == 0.) {
        border_style = BorderStyle::Solid;
    }
    Quad {
        order: 0,
        border_style,
        bounds: sp(x, y, qw, qh),
        content_mask: random_mask(rng, w, h),
        background: rng.background(),
        border_color: rng.color(),
        corner_radii,
        border_widths,
    }
}

/// A content mask, or none. Its edges never fall on pixel centers, where
/// whether the GPU's interpolated clip distances come out a hair above or
/// below zero is noise.
fn random_mask(rng: &mut Rng, w: f32, h: f32) -> ContentMask<ScaledPixels> {
    if rng.chance(0.6) {
        return no_mask();
    }
    let mut edge = |lo: f32, hi: f32| {
        let v = rng.coord(lo, hi);
        if v.fract().abs() == 0.5 { v + 0.25 } else { v }
    };
    let (x, y) = (edge(-10., w * 0.7), edge(-10., h * 0.7));
    let (right, bottom) = (edge(x + 10., x + w), edge(y + 10., y + h));
    mask(x, y, right - x, bottom - y)
}

fn random_shadow(rng: &mut Rng, w: f32, h: f32) -> Shadow {
    let bounds = sp(
        rng.coord(0., w * 0.8),
        rng.coord(0., h * 0.8),
        rng.coord(4., w / 3.),
        rng.coord(4., h / 3.),
    );
    let radius = if rng.chance(0.3) {
        0.
    } else {
        rng.coord(0., 12.)
    };
    let inset = rng.chance(0.4);
    let blur = if rng.chance(0.25) {
        0.
    } else {
        rng.coord(0.5, 16.)
    };
    let element_bounds = if inset {
        bounds.dilate(ScaledPixels(rng.coord(0., 10.)))
    } else {
        bounds
    };
    let mut hole = bounds;
    if inset {
        hole.origin.x.0 += rng.coord(-6., 6.);
        hole.origin.y.0 += rng.coord(-6., 6.);
    }
    Shadow {
        order: 0,
        blur_radius: ScaledPixels(blur),
        bounds: hole,
        corner_radii: Corners::all(ScaledPixels(radius)),
        content_mask: random_mask(rng, w, h),
        color: rng.color(),
        element_bounds,
        element_corner_radii: Corners::all(ScaledPixels(radius + rng.coord(0., 4.))),
        inset: inset as u32,
        pad: 0,
    }
}

fn random_underline(rng: &mut Rng, w: f32, h: f32) -> Underline {
    let thickness = rng.coord(1., 4.);
    Underline {
        order: 0,
        pad: 0,
        bounds: sp(
            rng.coord(0., w * 0.8),
            rng.coord(0., h * 0.9),
            rng.coord(10., w / 2.),
            if rng.chance(0.5) {
                thickness
            } else {
                thickness * 3.
            },
        ),
        content_mask: random_mask(rng, w, h),
        color: rng.color(),
        thickness: ScaledPixels(thickness),
        wavy: rng.chance(0.5).into(),
    }
}

fn random_transformation(rng: &mut Rng, x: f32, y: f32) -> TransformationMatrix {
    match rng.int(0, 6) {
        0 => TransformationMatrix::unit()
            .translate(point(ScaledPixels(x), ScaledPixels(y)))
            .rotate(Radians(rng.range(-3.1, 3.1)))
            .translate(point(ScaledPixels(-x), ScaledPixels(-y))),
        1 => TransformationMatrix::unit()
            .translate(point(ScaledPixels(x), ScaledPixels(y)))
            .scale(size(rng.range(0.5, 2.), rng.range(0.5, 2.)))
            .translate(point(ScaledPixels(-x), ScaledPixels(-y))),
        _ => TransformationMatrix::unit(),
    }
}

fn random_path(rng: &mut Rng, w: f32, h: f32) -> Path<ScaledPixels> {
    let cx = rng.range(0., w);
    let cy = rng.range(0., h);
    let r = rng.range(5., w.min(h) / 3.);
    let mut at = || point(px(cx + rng.range(-r, r)), px(cy + rng.range(-r, r)));
    let mut path = Path::new(at());
    let segments = 3 + (r as usize % 4);
    for i in 0..segments {
        if i % 2 == 0 {
            path.line_to(at());
        } else {
            let to = at();
            path.curve_to(to, at());
        }
    }
    let start = path.vertices.first().map(|v| v.xy_position);
    if let Some(start) = start {
        path.line_to(start);
    }
    path.content_mask = if rng.chance(0.3) {
        ContentMask {
            bounds: Bounds {
                origin: point(px(rng.range(0., w / 2.)), px(rng.range(0., h / 2.))),
                size: size(px(rng.range(20., w)), px(rng.range(20., h))),
            },
        }
    } else {
        ContentMask {
            bounds: Bounds {
                origin: point(px(-10_000.), px(-10_000.)),
                size: size(px(20_000.), px(20_000.)),
            },
        }
    };
    path.color = rng.background();
    path.scale(1.)
}

/// What a random scene is made of.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kinds {
    Quads,
    Shadows,
    Underlines,
    Monochrome,
    Subpixel,
    Polychrome,
    Paths,
    Mixed,
}

fn random_scene(harness: &mut Harness, rng: &mut Rng, kinds: Kinds, w: f32, h: f32) -> Scene {
    let mut scene = Scene::default();
    // An opaque backdrop, as windows have.
    if rng.chance(0.7) {
        scene.insert_primitive(quad(sp(0., 0., w, h), rng.color()));
    }
    let count = if kinds == Kinds::Mixed { 120 } else { 40 };
    for _ in 0..count {
        let kind = if kinds == Kinds::Mixed {
            [
                Kinds::Quads,
                Kinds::Quads,
                Kinds::Quads,
                Kinds::Shadows,
                Kinds::Underlines,
                Kinds::Monochrome,
                Kinds::Monochrome,
                Kinds::Subpixel,
                Kinds::Polychrome,
                Kinds::Paths,
            ][rng.int(0, 10) as usize]
        } else {
            kinds
        };
        let layered = rng.chance(0.15);
        if layered {
            scene.push_layer(sp(
                rng.coord(0., w / 2.),
                rng.coord(0., h / 2.),
                rng.coord(10., w),
                rng.coord(10., h),
            ));
        }
        match kind {
            Kinds::Quads => scene.insert_primitive(random_quad(rng, w, h)),
            Kinds::Shadows => scene.insert_primitive(random_shadow(rng, w, h)),
            Kinds::Underlines => scene.insert_primitive(random_underline(rng, w, h)),
            Kinds::Monochrome => {
                let (gw, gh) = (rng.int(3, 24), rng.int(3, 24));
                let bytes = random_glyph_bytes(rng, gw, gh, 1);
                let tile = harness.glyph(gw, gh, false, bytes);
                let (x, y) = (rng.coord(0., w - 10.), rng.coord(0., h - 10.));
                let (bw, bh) = if rng.chance(0.8) {
                    (gw as f32, gh as f32)
                } else {
                    scaled(rng, gw, gh)
                };
                scene.insert_primitive(MonochromeSprite {
                    order: 0,
                    pad: 0,
                    bounds: sp(x, y, bw, bh),
                    content_mask: random_mask(rng, w, h),
                    color: rng.color(),
                    tile,
                    transformation: random_transformation(rng, x + bw / 2., y + bh / 2.),
                });
            }
            Kinds::Subpixel => {
                let (gw, gh) = (rng.int(3, 24), rng.int(3, 24));
                let bytes = random_glyph_bytes(rng, gw, gh, 4);
                let tile = harness.glyph(gw, gh, true, bytes);
                let (x, y) = (rng.coord(0., w - 10.), rng.coord(0., h - 10.));
                let (bw, bh) = if rng.chance(0.8) {
                    (gw as f32, gh as f32)
                } else {
                    scaled(rng, gw, gh)
                };
                scene.insert_primitive(SubpixelSprite {
                    order: 0,
                    pad: 0,
                    bounds: sp(x, y, bw, bh),
                    content_mask: random_mask(rng, w, h),
                    color: rng.color(),
                    tile,
                    transformation: random_transformation(rng, x + bw / 2., y + bh / 2.),
                });
            }
            Kinds::Polychrome => {
                let (iw, ih) = (rng.int(2, 40), rng.int(2, 40));
                let bytes = random_image_bytes(rng, iw, ih);
                let tile = harness.image(iw, ih, bytes);
                let (bw, bh) = if rng.chance(0.6) {
                    (iw as f32, ih as f32)
                } else {
                    scaled(rng, iw, ih)
                };
                scene.insert_primitive(PolychromeSprite {
                    order: 0,
                    pad: 0,
                    grayscale: rng.chance(0.3).into(),
                    opacity: if rng.chance(0.5) {
                        1.
                    } else {
                        rng.range(0., 1.)
                    },
                    bounds: sp(rng.coord(0., w - 10.), rng.coord(0., h - 10.), bw, bh),
                    content_mask: random_mask(rng, w, h),
                    corner_radii: if rng.chance(0.5) {
                        Corners::default()
                    } else {
                        Corners::all(ScaledPixels(rng.coord(0., bw.min(bh) / 2.)))
                    },
                    tile,
                });
            }
            Kinds::Paths => scene.insert_primitive(random_path(rng, w, h)),
            Kinds::Mixed => unreachable!(),
        }
        if layered {
            scene.pop_layer();
        }
    }
    scene.finish();
    scene
}

// --- tests --- //

macro_rules! harness {
    () => {
        match Harness::new() {
            Some(harness) => harness,
            None => {
                eprintln!("skipped: no wgpu adapter");
                return;
            }
        }
    };
}

/// Asserts that a difference is within `tolerance` levels.
fn check(difference: &Difference, label: &str, tolerance: u8) {
    eprintln!("{}", difference.report(label));
    assert!(
        difference.largest() <= tolerance,
        "{}",
        difference.report(label)
    );
}

#[test]
fn quads_match_the_gpu() {
    let mut harness = harness!();
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0x203040ff)));
    // Solid fills: opaque, translucent, at fractional positions.
    scene.insert_primitive(quad(sp(10., 10., 50., 30.), color(0xff0000ff)));
    scene.insert_primitive(quad(sp(20.5, 20.25, 50.3, 30.7), color(0x00ff0080)));
    scene.insert_primitive(quad(sp(30.75, 45.5, 0.4, 20.), color(0xffffffff)));
    // Gradients in sRGB and Oklab, at several angles and stops.
    for (i, angle) in [0., 45., 90., 135., 200., 300., 400.]
        .into_iter()
        .enumerate()
    {
        let x = 70. + i as f32 * 45.;
        scene.insert_primitive(quad(
            sp(x, 10., 40., 60.),
            linear_gradient(
                angle,
                linear_color_stop(rgba(0xff0000ff), 0.1),
                linear_color_stop(rgba(0x0000ff80), 0.9),
            ),
        ));
        scene.insert_primitive(quad(
            sp(x, 75., 40., 30.),
            linear_gradient(
                angle,
                linear_color_stop(rgba(0xffcc00ff), 0.),
                linear_color_stop(rgba(0x00ccffff), 1.),
            )
            .color_space(gpui::ColorSpace::Oklab),
        ));
    }
    // Patterns.
    scene.insert_primitive(quad(
        sp(10., 110., 80., 60.),
        pattern_slash(color(0xffffffff), 2., 3.),
    ));
    scene.insert_primitive(quad(
        sp(100., 110., 80., 60.),
        pattern_slash(color(0x00ff00c0), 1., 1.5),
    ));
    scene.insert_primitive(quad(
        sp(190., 110., 80., 60.),
        checkerboard(color(0xff00ffff), 7.),
    ));
    scene.insert_primitive(quad(
        sp(280.5, 110.5, 80., 60.),
        checkerboard(color(0xffff0080), 4.),
    ));
    // Rounded corners, borders, partial borders, elliptic inner corners.
    scene.insert_primitive(Quad {
        bounds: sp(10., 180., 90., 60.),
        content_mask: no_mask(),
        background: color(0xeeeeeeff).into(),
        border_color: color(0x222222ff),
        corner_radii: Corners::all(ScaledPixels(12.)),
        border_widths: Edges::all(ScaledPixels(3.)),
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        bounds: sp(110.5, 180.25, 90., 60.),
        content_mask: no_mask(),
        background: color(0x3366ff80).into(),
        border_color: color(0xff8800c0),
        corner_radii: Corners {
            top_left: ScaledPixels(0.),
            top_right: ScaledPixels(20.),
            bottom_right: ScaledPixels(5.),
            bottom_left: ScaledPixels(14.5),
        },
        border_widths: Edges {
            top: ScaledPixels(1.),
            right: ScaledPixels(6.),
            bottom: ScaledPixels(0.),
            left: ScaledPixels(2.5),
        },
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        bounds: sp(210., 180., 80., 60.),
        content_mask: mask(220., 190., 50., 100.),
        background: color(0x00000000).into(),
        border_color: color(0xffffffff),
        border_widths: Edges {
            top: ScaledPixels(0.),
            right: ScaledPixels(0.),
            bottom: ScaledPixels(4.),
            left: ScaledPixels(0.),
        },
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        bounds: sp(300., 180., 90., 100.),
        content_mask: no_mask(),
        background: linear_gradient(
            30.,
            linear_color_stop(rgba(0x112233ff), 0.),
            linear_color_stop(rgba(0x445566ff), 1.),
        ),
        border_color: color(0xccddeeff),
        corner_radii: Corners::all(ScaledPixels(45.)),
        border_widths: Edges::all(ScaledPixels(10.)),
        ..Default::default()
    });
    // Dashed borders, unrounded and rounded, roomy and cramped.
    scene.insert_primitive(Quad {
        border_style: BorderStyle::Dashed,
        bounds: sp(10., 250., 120., 40.),
        content_mask: no_mask(),
        background: color(0x00000000).into(),
        border_color: color(0xffffffff),
        border_widths: Edges::all(ScaledPixels(2.)),
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        border_style: BorderStyle::Dashed,
        bounds: sp(140., 250., 60., 40.),
        content_mask: no_mask(),
        background: color(0x404040ff).into(),
        border_color: color(0xff4444ff),
        corner_radii: Corners::all(ScaledPixels(10.)),
        border_widths: Edges {
            top: ScaledPixels(3.),
            right: ScaledPixels(1.),
            bottom: ScaledPixels(2.),
            left: ScaledPixels(0.),
        },
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        border_style: BorderStyle::Dashed,
        bounds: sp(210., 250., 5., 6.),
        content_mask: no_mask(),
        border_color: color(0xffffffff),
        border_widths: Edges::all(ScaledPixels(2.)),
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        border_style: BorderStyle::Dashed,
        bounds: sp(220., 250., 8., 8.),
        content_mask: no_mask(),
        border_color: color(0x00ff00ff),
        corner_radii: Corners::all(ScaledPixels(3.)),
        border_widths: Edges::all(ScaledPixels(3.)),
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        border_style: BorderStyle::Dashed,
        bounds: sp(240., 250., 9., 40.),
        content_mask: no_mask(),
        border_color: color(0x00ffffff),
        border_widths: Edges::all(ScaledPixels(3.)),
        ..Default::default()
    });
    scene.finish();

    for mode in [OPAQUE, PREMULTIPLIED] {
        let difference = harness.compare(&scene, 400, 300, mode);
        check(&difference, &format!("quads {mode:?}"), 2);
    }
}

#[test]
fn shadows_and_underlines_match_the_gpu() {
    let mut harness = harness!();
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0xf0f0f0ff)));
    scene.insert_primitive(shadow(sp(20., 20., 100., 60.), 0., 0., 0x00000080));
    scene.insert_primitive(shadow(sp(150., 20., 100., 60.), 0., 10., 0x0000ff80));
    scene.insert_primitive(shadow(sp(280., 20., 80., 60.), 8., 6., 0x000000a0));
    scene.insert_primitive(shadow(sp(30.5, 120.25, 120., 70.), 16., 0., 0xff000060));
    scene.insert_primitive(Shadow {
        content_mask: mask(180., 110., 90., 50.),
        ..shadow(sp(180., 120., 100., 60.), 4., 12., 0x00000080)
    });
    for (i, (blur, radius)) in [(0., 0.), (0., 8.), (6., 0.), (10., 12.)]
        .into_iter()
        .enumerate()
    {
        let element = sp(20. + i as f32 * 95., 210., 80., 60.);
        scene.insert_primitive(quad(element, color(0xffffffff)));
        scene.insert_primitive(Shadow {
            bounds: sp(element.origin.x.0 + 4., element.origin.y.0 + 6., 80., 60.),
            element_bounds: element,
            element_corner_radii: Corners::all(ScaledPixels(radius)),
            inset: 1,
            ..shadow(element, blur, radius, 0x000000c0)
        });
    }
    for (i, wavy) in [false, true].into_iter().enumerate() {
        for (j, thickness) in [1., 2., 3.5].into_iter().enumerate() {
            scene.insert_primitive(Underline {
                order: 0,
                pad: 0,
                bounds: sp(
                    300. + i as f32 * 0.5,
                    100. + j as f32 * 30. + i as f32 * 12.,
                    90.,
                    if wavy { thickness * 3. } else { thickness },
                ),
                content_mask: no_mask(),
                color: color(if wavy { 0xff0000ff } else { 0x0000ffc0 }),
                thickness: ScaledPixels(thickness),
                wavy: wavy.into(),
            });
        }
    }
    scene.finish();

    for mode in [OPAQUE, PREMULTIPLIED] {
        let difference = harness.compare(&scene, 400, 300, mode);
        check(&difference, &format!("shadows and underlines {mode:?}"), 2);
    }
}

#[test]
fn sprites_match_the_gpu() {
    let mut harness = harness!();
    let mut rng = Rng::new(7);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0x102030ff)));
    scene.insert_primitive(quad(sp(200., 0., 200., 300.), color(0xf0f0e0ff)));
    let colors = [0xffffffff, 0x000000ff, 0xff8800ff, 0x33cc6680, 0x8080ffff];
    for (i, hex) in colors.into_iter().enumerate() {
        for (j, (bx, by, scale)) in [
            (0., 0., 1.),
            (0.5, 0.25, 1.),
            (0., 0., 1.7),
            (0.3, 0.6, 0.6),
        ]
        .into_iter()
        .enumerate()
        {
            let bytes = random_glyph_bytes(&mut rng, 12, 16, 1);
            let tile = harness.glyph(12, 16, false, bytes);
            let x = 10. + i as f32 * 78. + j as f32 * 18. + bx;
            scene.insert_primitive(MonochromeSprite {
                order: 0,
                pad: 0,
                bounds: sp(x, 10. + by, 12. * scale, 16. * scale),
                content_mask: no_mask(),
                color: color(hex),
                tile,
                transformation: TransformationMatrix::unit(),
            });
            let bytes = random_glyph_bytes(&mut rng, 12, 16, 4);
            let tile = harness.glyph(12, 16, true, bytes);
            scene.insert_primitive(SubpixelSprite {
                order: 0,
                pad: 0,
                bounds: sp(x, 50. + by, 12. * scale, 16. * scale),
                content_mask: mask(0., 52., 400., 300.),
                color: color(hex),
                tile,
                transformation: TransformationMatrix::unit(),
            });
        }
    }
    // Rotated and scaled glyphs.
    for (i, angle) in [0.3f32, 1.2, -2.5].into_iter().enumerate() {
        let (x, y) = (40. + i as f32 * 120., 100.);
        let transformation = TransformationMatrix::unit()
            .translate(point(ScaledPixels(x + 10.), ScaledPixels(y + 10.)))
            .rotate(Radians(angle))
            .scale(size(1.5, 1.2))
            .translate(point(ScaledPixels(-x - 10.), ScaledPixels(-y - 10.)));
        let bytes = random_glyph_bytes(&mut rng, 20, 20, 1);
        let tile = harness.glyph(20, 20, false, bytes);
        scene.insert_primitive(MonochromeSprite {
            order: 0,
            pad: 0,
            bounds: sp(x, y, 20., 20.),
            content_mask: no_mask(),
            color: color(0xffcc00ff),
            tile,
            transformation,
        });
        let bytes = random_glyph_bytes(&mut rng, 20, 20, 4);
        let tile = harness.glyph(20, 20, true, bytes);
        scene.insert_primitive(SubpixelSprite {
            order: 0,
            pad: 0,
            bounds: sp(x + 50., y, 20., 20.),
            content_mask: no_mask(),
            color: color(0x00ccffff),
            tile,
            transformation,
        });
    }
    // Images: plain, grayscale, translucent, rounded, scaled.
    for (i, (grayscale, opacity, radius, scale)) in [
        (false, 1., 0., 1.),
        (true, 1., 0., 1.),
        (false, 0.5, 6., 1.),
        (true, 0.8, 12., 1.),
        (false, 1., 4., 2.3),
        (false, 1., 0., 0.7),
    ]
    .into_iter()
    .enumerate()
    {
        let bytes = random_image_bytes(&mut rng, 30, 24);
        let tile = harness.image(30, 24, bytes);
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: grayscale.into(),
            opacity,
            bounds: sp(
                10. + i as f32 * 65.,
                180. + (i % 2) as f32 * 0.5,
                30. * scale,
                24. * scale,
            ),
            content_mask: no_mask(),
            corner_radii: Corners::all(ScaledPixels(radius)),
            tile,
        });
    }
    scene.finish();

    for mode in [
        OPAQUE,
        PREMULTIPLIED,
        Mode {
            is_bgr: true,
            ..OPAQUE
        },
        Mode {
            dual_source_blending: false,
            ..OPAQUE
        },
        Mode {
            dual_source_blending: false,
            ..PREMULTIPLIED
        },
    ] {
        let difference = harness.compare(&scene, 400, 300, mode);
        check(
            &difference,
            &format!("sprites {mode:?}"),
            harness.sampling_tolerance(),
        );
    }
}

/// Paths: lines and curves, solid and gradient, in one batch of one order
/// (composited path by path) and across orders (composited once).
fn path_scene(harness: &mut Harness) -> Scene {
    let _ = harness;
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0x202020ff)));
    let mut path = Path::new(point(px(20.), px(20.)));
    path.line_to(point(px(180.5), px(40.)));
    path.curve_to(point(px(100.), px(200.)), point(px(220.), px(150.)));
    path.line_to(point(px(20.), px(20.)));
    path.color = color(0x8800ffcc).into();
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(-1000.), px(-1000.)),
            size: size(px(3000.), px(3000.)),
        },
    };
    scene.insert_primitive(path.scale(1.));

    // A sparkline-like stroke of thin triangles with a gradient.
    let mut line = Path::new(point(px(200.), px(250.)));
    for i in 0..20 {
        let x = 200. + i as f32 * 9.5;
        let y = 250. - ((i * 37) % 50) as f32;
        line.line_to(point(px(x), px(y)));
        line.line_to(point(px(x + 4.), px(y + 1.5)));
    }
    line.line_to(point(px(200.), px(250.)));
    line.color = linear_gradient(
        90.,
        linear_color_stop(rgba(0xff0000ff), 0.),
        linear_color_stop(rgba(0x00ff00ff), 1.),
    );
    line.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(210.), px(190.)),
            size: size(px(150.), px(80.)),
        },
    };
    scene.insert_primitive(line.scale(1.));

    // Overlapping paths sharing a layer, so an order: each composited
    // through its own sprite.
    scene.paint_layer_for_tests(sp(0., 0., 400., 300.), |scene| {
        for i in 0..3 {
            let x = 40. + i as f32 * 25.;
            let mut blob = Path::new(point(px(x), px(220.)));
            blob.curve_to(point(px(x + 60.), px(220.)), point(px(x + 30.), px(160.)));
            blob.curve_to(point(px(x), px(220.)), point(px(x + 30.), px(280.)));
            blob.color = color([0xff000080, 0x00ff0080, 0x0000ff80][i]).into();
            blob.content_mask = ContentMask {
                bounds: Bounds {
                    origin: point(px(-1000.), px(-1000.)),
                    size: size(px(3000.), px(3000.)),
                },
            };
            scene.insert_primitive(blob.scale(1.));
        }
    });
    scene.finish();
    scene
}

trait PaintLayer {
    fn paint_layer_for_tests(&mut self, bounds: Bounds<ScaledPixels>, f: impl FnOnce(&mut Self));
}

impl PaintLayer for Scene {
    fn paint_layer_for_tests(&mut self, bounds: Bounds<ScaledPixels>, f: impl FnOnce(&mut Self)) {
        self.push_layer(bounds);
        f(self);
        self.pop_layer();
    }
}

#[test]
fn paths_match_the_gpu() {
    let mut harness = harness!();
    let scene = path_scene(&mut harness);
    for mode in [OPAQUE, PREMULTIPLIED] {
        let difference = harness.compare(&scene, 400, 300, mode);
        check(&difference, &format!("paths {mode:?}"), 8);
    }
}

const TILE: u32 = 256;

/// A window with a viewport over a scroll layer, composited from its tiles.
fn layer_scene(harness: &mut Harness, translation: (f32, f32), background: Rgba) -> Scene {
    let mut rng = Rng::new(99);
    let mut content = random_scene(harness, &mut rng, Kinds::Mixed, 512., 512.);
    // Tile content has no backdrop of its own: the tiles' background is it.
    content.quads.retain(|quad| quad.bounds.size.width.0 < 512.);
    let key = LayerKey(5);
    let tiles = [(0, 0), (1, 0), (0, 1), (1, 1)].map(|(x, y)| TileCoord { x, y });
    let layer = LayerFrame {
        key,
        generation: 1,
        background,
        tile_size: TILE,
        content: content.into(),
        dirty_tiles: tiles.to_vec(),
    };
    let viewport = sp(40., 30., 300., 220.);
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0x445566ff)));
    scene.push_layer(viewport);
    for tile in tiles {
        let bounds = layer.tile_bounds(tile);
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.,
            bounds: sp(
                bounds.origin.x.0 + translation.0,
                bounds.origin.y.0 + translation.1,
                TILE as f32,
                TILE as f32,
            ),
            content_mask: ContentMask { bounds: viewport },
            corner_radii: Corners::default(),
            tile: AtlasTile {
                texture_id: layer_tile_texture_id(key),
                tile_id: layer_tile_id(tile),
                padding: 0,
                bounds: Bounds {
                    origin: point(DevicePixels(0), DevicePixels(0)),
                    size: size(DevicePixels(TILE as i32), DevicePixels(TILE as i32)),
                },
            },
        });
    }
    scene.pop_layer();
    scene.insert_primitive(quad(sp(350., 40., 12., 120.), color(0x888888ff)));
    scene.layers.frames.push(layer);
    scene.finish();
    scene
}

#[test]
fn layer_tiles_match_the_gpu() {
    let mut harness = harness!();
    for (translation, background) in [
        ((-37., -91.), rgba(0x336699ff)),
        ((40., 30.), rgba(0xffffffff)),
        ((12.5, -20.25), rgba(0x336699ff)),
    ] {
        let scene = layer_scene(&mut harness, translation, background);
        for mode in [OPAQUE, PREMULTIPLIED] {
            let difference = harness.compare(&scene, 400, 300, mode);
            check(
                &difference,
                &format!("layer tiles at {translation:?} {mode:?}"),
                8,
            );
        }
    }
}

#[test]
fn random_scenes_match_the_gpu() {
    let mut harness = harness!();
    let kinds = [
        Kinds::Quads,
        Kinds::Shadows,
        Kinds::Underlines,
        Kinds::Monochrome,
        Kinds::Subpixel,
        Kinds::Polychrome,
        Kinds::Paths,
        Kinds::Mixed,
    ];
    let mut failures = Vec::new();
    for kind in kinds {
        for mode in [OPAQUE, PREMULTIPLIED] {
            let mut total = Difference::default();
            for seed in 0..8 {
                let mut rng = Rng::new(seed * 31 + kind as u64);
                let mut scene = random_scene(&mut harness, &mut rng, kind, 320., 240.);
                if !harness.is_nvidia() {
                    solid_checkerboards(&mut scene);
                }
                let difference = harness.compare(&scene, 320, 240, mode);
                total.merge(&difference);
                let tolerance = if matches!(kind, Kinds::Paths | Kinds::Mixed) {
                    8
                } else {
                    2
                };
                if difference.largest() > tolerance {
                    failures.push(difference.report(&format!("{kind:?} seed {seed} {mode:?}")));
                }
            }
            eprintln!("{}", total.report(&format!("random {kind:?} {mode:?}")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Drawing a scene into regions of a frame of another scene gives the new
/// scene's pixels inside them, and leaves the old ones outside.
#[test]
fn regions_draw_like_whole_frames() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("skipped: no wgpu adapter");
        return;
    };
    let (w, h) = (700u32, 500u32);
    let mut rng = Rng::new(5);
    let pairs = [
        (
            random_scene(&mut harness, &mut rng, Kinds::Mixed, w as f32, h as f32),
            random_scene(&mut harness, &mut rng, Kinds::Mixed, w as f32, h as f32),
        ),
        (
            layer_scene(&mut harness, (-37., -91.), rgba(0x336699ff)),
            layer_scene(&mut harness, (-37., -60.), rgba(0x336699ff)),
        ),
    ];
    // Small and large (drawn in bands) regions, overlapping and crossing
    // the frame's edges.
    let regions = [
        device_bounds(10, 10, 50, 30),
        device_bounds(100, 50, 500, 300),
        device_bounds(650, -10, 100, 40),
        device_bounds(40, 20, 30, 30),
        device_bounds(0, 480, 700, 30),
    ];
    for mode in [OPAQUE, PREMULTIPLIED] {
        let params = harness.params(mode);
        for (before, after) in &pairs {
            let mut canvas = Canvas::new(w, h);
            let whole_frame = [device_bounds(0, 0, w as i32, h as i32)];
            draw(
                &mut canvas,
                before,
                &whole_frame,
                &harness.mirror,
                &params,
                1,
            );
            let old = canvas.pixels().to_vec();
            draw(&mut canvas, after, &regions, &harness.mirror, &params, 4);
            let whole = harness.cpu(after, w, h, mode, 1);

            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let inside = regions.iter().any(|r| {
                        x >= r.origin.x.0
                            && x < r.origin.x.0 + r.size.width.0
                            && y >= r.origin.y.0
                            && y < r.origin.y.0 + r.size.height.0
                    });
                    let index = (y as u32 * w + x as u32) as usize;
                    let expected = if inside { whole[index] } else { old[index] };
                    assert_eq!(
                        canvas.pixels()[index],
                        expected,
                        "pixel ({x}, {y}) {}",
                        if inside { "inside" } else { "outside" }
                    );
                }
            }
        }
    }
}

/// A region beside some of a path batch's sprites, level with them, draws
/// as the whole frame does there (their rectangles clipped to it are empty).
#[test]
fn regions_beside_path_sprites_draw_like_whole_frames() {
    let Some(harness) = Harness::new() else {
        eprintln!("skipped: no wgpu adapter");
        return;
    };
    // Side by side, apart: one order, so one batch composited through a
    // sprite per path.
    let mut scene = Scene::default();
    scene.insert_primitive(quad(sp(0., 0., 400., 300.), color(0x202020ff)));
    for x in [40., 220.] {
        let mut path = Path::new(point(px(x), px(60.)));
        path.line_to(point(px(x + 120.), px(80.)));
        path.curve_to(point(px(x + 20.), px(220.)), point(px(x + 100.), px(150.)));
        path.line_to(point(px(x), px(60.)));
        path.color = color(0x8800ffcc).into();
        path.content_mask = ContentMask {
            bounds: Bounds {
                origin: point(px(-1000.), px(-1000.)),
                size: size(px(3000.), px(3000.)),
            },
        };
        scene.insert_primitive(path.scale(1.));
    }
    scene.finish();
    assert_eq!(scene.paths[0].order, scene.paths[1].order);

    let (w, h) = (400u32, 300u32);
    // Over the right path only, level with the left one.
    let region = device_bounds(230, 90, 60, 80);
    for mode in [OPAQUE, PREMULTIPLIED] {
        let params = harness.params(mode);
        let whole = harness.cpu(&scene, w, h, mode, 1);
        let mut canvas = Canvas::new(w, h);
        draw(&mut canvas, &scene, &[region], &harness.mirror, &params, 1);
        for y in 90..170 {
            for x in 230..290 {
                let index = (y * w + x) as usize;
                assert_eq!(canvas.pixels()[index], whole[index], "pixel ({x}, {y})");
            }
        }
    }
}

/// Threads draw the same pixels as one thread does.
#[test]
fn threaded_drawing_is_exact() {
    let Some(mut harness) = Harness::new() else {
        eprintln!("skipped: no wgpu adapter");
        return;
    };
    let mut rng = Rng::new(11);
    let scene = random_scene(&mut harness, &mut rng, Kinds::Mixed, 700., 500.);
    let layers = layer_scene(&mut harness, (12., -7.), rgba(0x336699ff));
    for mode in [OPAQUE, PREMULTIPLIED] {
        for (scene, w, h) in [(&scene, 700, 500), (&layers, 400, 300)] {
            let one = harness.cpu(scene, w, h, mode, 1);
            for threads in [2, 3, 8] {
                assert!(
                    one == harness.cpu(scene, w, h, mode, threads),
                    "{threads} threads"
                );
            }
        }
    }
}

/// A window of a code editor: panels, a file list, tabs, buttons, a popover
/// with a shadow, icons and ~5000 glyphs of text, with the atlas it samples.
fn editor_scene(width: f32, height: f32) -> (Scene, Mirror) {
    let mut rng = Rng::new(42);
    let mut mirror = Mirror::default();
    const SIDE: u32 = 1024;
    let glyphs_texture = AtlasTextureId {
        index: 0,
        kind: AtlasTextureKind::Monochrome,
    };
    let icons_texture = AtlasTextureId {
        index: 0,
        kind: AtlasTextureKind::Polychrome,
    };
    let mut glyph_data = vec![0u8; (SIDE * SIDE) as usize];
    let mut glyph_tiles = Vec::new();
    for glyph in 0..96u32 {
        let (gx, gy) = ((glyph % 64) * 10, (glyph / 64) * 18);
        for y in 0..16 {
            for x in 0..8 {
                // A blob with antialiased edges.
                let d = ((x as f32 - 3.5).powi(2) + (y as f32 - 8.).powi(2)).sqrt()
                    - (3. + (glyph % 5) as f32);
                let coverage =
                    (0.5 - d).clamp(0., 1.) * if (x + y + glyph) % 3 == 0 { 1. } else { 0.7 };
                glyph_data[((gy + y) * SIDE + gx + x) as usize] = (coverage * 255.) as u8;
            }
        }
        glyph_tiles.push(AtlasTile {
            texture_id: glyphs_texture,
            tile_id: gpui::TileId(glyph),
            padding: 0,
            bounds: Bounds {
                origin: point(DevicePixels(gx as i32), DevicePixels(gy as i32)),
                size: size(DevicePixels(8), DevicePixels(16)),
            },
        });
    }
    mirror.textures.insert(
        glyphs_texture,
        MirrorTexture {
            width: SIDE,
            height: SIDE,
            bytes_per_pixel: 1,
            data: glyph_data,
        },
    );
    let icon_bytes = random_image_bytes(&mut rng, 16 * 8, 16);
    let mut icon_data = vec![0u8; (SIDE * SIDE * 4) as usize];
    for y in 0..16usize {
        icon_data[y * SIDE as usize * 4..y * SIDE as usize * 4 + 16 * 8 * 4]
            .copy_from_slice(&icon_bytes[y * 16 * 8 * 4..(y + 1) * 16 * 8 * 4]);
    }
    mirror.textures.insert(
        icons_texture,
        MirrorTexture {
            width: SIDE,
            height: SIDE,
            bytes_per_pixel: 4,
            data: icon_data,
        },
    );

    let mut scene = Scene::default();
    let text = |scene: &mut Scene, rng: &mut Rng, x: f32, y: f32, chars: usize, color: Hsla| {
        for i in 0..chars {
            if rng.chance(0.15) {
                continue;
            }
            scene.insert_primitive(MonochromeSprite {
                order: 0,
                pad: 0,
                bounds: sp(x + i as f32 * 8., y, 8., 16.),
                content_mask: no_mask(),
                color,
                tile: glyph_tiles[rng.int(0, 96) as usize],
                transformation: TransformationMatrix::unit(),
            });
        }
    };
    scene.insert_primitive(quad(sp(0., 0., width, height), color(0x1e1e1eff)));
    // Title bar and status bar.
    scene.insert_primitive(quad(sp(0., 0., width, 36.), color(0x2b2b2bff)));
    scene.insert_primitive(quad(sp(0., height - 24., width, 24.), color(0x007accff)));
    text(
        &mut scene,
        &mut rng,
        12.,
        height - 20.,
        60,
        color(0xffffffff),
    );
    // Sidebar with a file list.
    scene.insert_primitive(Quad {
        border_color: color(0x3c3c3cff),
        border_widths: Edges {
            right: ScaledPixels(1.),
            ..Default::default()
        },
        ..quad(sp(0., 36., 300., height - 60.), color(0x252526ff))
    });
    for row in 0..45 {
        let y = 44. + row as f32 * 22.;
        if row == 7 {
            scene.insert_primitive(Quad {
                corner_radii: Corners::all(ScaledPixels(4.)),
                ..quad(sp(6., y, 288., 22.), color(0x37373dff))
            });
        }
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.,
            bounds: sp(16. + (row % 3) as f32 * 12., y + 3., 16., 16.),
            content_mask: no_mask(),
            corner_radii: Corners::default(),
            tile: AtlasTile {
                texture_id: icons_texture,
                tile_id: gpui::TileId(1000 + row),
                padding: 0,
                bounds: Bounds {
                    origin: point(DevicePixels((row as i32 % 8) * 16), DevicePixels(0)),
                    size: size(DevicePixels(16), DevicePixels(16)),
                },
            },
        });
        text(
            &mut scene,
            &mut rng,
            40. + (row % 3) as f32 * 12.,
            y + 3.,
            18 + row as usize % 10,
            color(0xccccccff),
        );
    }
    // Tabs.
    for tab in 0..8 {
        let x = 300. + tab as f32 * 160.;
        scene.insert_primitive(Quad {
            border_color: color(0x3c3c3cff),
            border_widths: Edges {
                right: ScaledPixels(1.),
                bottom: ScaledPixels(if tab == 2 { 0. } else { 1. }),
                ..Default::default()
            },
            ..quad(
                sp(x, 36., 160., 36.),
                color(if tab == 2 { 0x1e1e1eff } else { 0x2d2d2dff }),
            )
        });
        text(&mut scene, &mut rng, x + 14., 46., 14, color(0xffffffcc));
        scene.insert_primitive(Quad {
            corner_radii: Corners::all(ScaledPixels(3.)),
            ..quad(sp(x + 136., 46., 16., 16.), color(0xffffff10))
        });
    }
    // Editor: line numbers, text, a current line, a selection, squiggles.
    scene.insert_primitive(quad(sp(300., 96., width - 300., 20.), color(0xffffff0a)));
    for line in 0..58 {
        let y = 80. + line as f32 * 18.;
        if y > height - 50. {
            break;
        }
        text(&mut scene, &mut rng, 310., y, 4, color(0x858585ff));
        let indent = (line % 7) as f32 * 32.;
        let chars = 30 + (line * 37) % 70;
        for (i, hex) in [0x569cd6ffu32, 0xd4d4d4ff, 0xce9178ff, 0x9cdcfeff]
            .into_iter()
            .enumerate()
        {
            text(
                &mut scene,
                &mut rng,
                360. + indent + i as f32 * (chars as f32 * 2.),
                y,
                chars / 4,
                color(hex),
            );
        }
        if line % 13 == 5 {
            scene.insert_primitive(Underline {
                order: 0,
                pad: 0,
                bounds: sp(400. + indent, y + 15., 120., 3.),
                content_mask: no_mask(),
                color: color(0xf14c4cff),
                thickness: ScaledPixels(1.),
                wavy: true.into(),
            });
        }
    }
    scene.insert_primitive(quad(sp(420., 260., 380., 90.), color(0x264f78aa)));
    // Buttons.
    for button in 0..12 {
        let x = 320. + button as f32 * 110.;
        scene.insert_primitive(Quad {
            border_color: color(0x5a5a5aff),
            border_widths: Edges::all(ScaledPixels(1.)),
            corner_radii: Corners::all(ScaledPixels(6.)),
            ..quad(sp(x, height - 64., 100., 30.), color(0x3a3d41ff))
        });
        text(
            &mut scene,
            &mut rng,
            x + 14.,
            height - 57.,
            9,
            color(0xffffffff),
        );
    }
    // A panel of tags: rounded, bordered chips in a grid.
    for chip in 0..240 {
        let (column, row) = ((chip % 12) as f32, (chip / 12) as f32);
        let bounds = sp(1380. + column * 44., 90. + row * 30., 40., 24.);
        scene.insert_primitive(Quad {
            border_color: color(0x4a4a4aff),
            border_widths: Edges::all(ScaledPixels(1.)),
            corner_radii: Corners::all(ScaledPixels(12.)),
            ..quad(
                bounds,
                color(if chip % 7 == 0 {
                    0x0e639cff
                } else {
                    0x333333ff
                }),
            )
        });
        text(
            &mut scene,
            &mut rng,
            bounds.origin.x.0 + 8.,
            bounds.origin.y.0 + 4.,
            3,
            color(0xffffffff),
        );
    }
    // A popover with a shadow.
    let popover = sp(900., 300., 420., 320.);
    scene.insert_primitive(Shadow {
        order: 0,
        blur_radius: ScaledPixels(16.),
        bounds: popover,
        corner_radii: Corners::all(ScaledPixels(8.)),
        content_mask: no_mask(),
        color: color(0x00000080),
        element_bounds: popover,
        element_corner_radii: Corners::all(ScaledPixels(8.)),
        inset: 0,
        pad: 0,
    });
    scene.insert_primitive(Quad {
        border_color: color(0x454545ff),
        border_widths: Edges::all(ScaledPixels(1.)),
        corner_radii: Corners::all(ScaledPixels(8.)),
        ..quad(popover, color(0x252526ff))
    });
    for row in 0..14 {
        let y = 310. + row as f32 * 22.;
        if row == 3 {
            scene.insert_primitive(Quad {
                corner_radii: Corners::all(ScaledPixels(4.)),
                ..quad(sp(906., y, 408., 22.), color(0x04395eff))
            });
        }
        text(&mut scene, &mut rng, 920., y + 3., 40, color(0xccccccff));
    }
    scene.finish();
    (scene, mirror)
}

/// Prints how long frames and regions of [`editor_scene`] take to draw.
/// Run with `--release --ignored --nocapture`.
#[test]
#[ignore]
fn timings() {
    let (width, height) = (1920u32, 1080u32);
    let (scene, mirror) = editor_scene(width as f32, height as f32);
    let params = RasterParams {
        gamma_ratios: gpui::get_gamma_correction_ratios(1.8),
        grayscale_enhanced_contrast: 1.,
        subpixel_enhanced_contrast: 0.5,
        is_bgr: false,
        premultiplied_alpha: false,
        dual_source_blending: true,
        path_sample_count: 4,
        fragment_bits: 12,
    };
    eprintln!(
        "scene: {} quads, {} shadows, {} underlines, {} monochrome sprites, {} polychrome sprites",
        scene.quads.len(),
        scene.shadows.len(),
        scene.underlines.len(),
        scene.monochrome_sprites.len(),
        scene.polychrome_sprites.len()
    );
    let start = std::time::Instant::now();
    for _ in 0..100 {
        std::hint::black_box(super::plan::Plan::new(&scene, &[]));
    }
    eprintln!("planning the scene: {:?}", start.elapsed() / 100);
    let mut canvas = Canvas::new(width, height);
    let mut time = |label: &str, regions: &[Bounds<DevicePixels>], threads: usize, runs: usize| {
        let mut samples = Vec::new();
        for _ in 0..runs {
            let start = std::time::Instant::now();
            draw(&mut canvas, &scene, regions, &mirror, &params, threads);
            samples.push(start.elapsed());
        }
        samples.sort();
        eprintln!(
            "{label}: median {:?}, best {:?}",
            samples[samples.len() / 2],
            samples[0]
        );
    };
    let whole = [device_bounds(0, 0, width as i32, height as i32)];
    time("1920x1080, 1 thread", &whole, 1, 15);
    time("1920x1080, 2 threads", &whole, 2, 15);
    time("1920x1080, 4 threads", &whole, 4, 15);
    time("1920x1080, 8 threads", &whole, 8, 15);
    let caret = [device_bounds(700, 300, 200, 40)];
    time("200x40 region of text", &caret, 8, 200);
    let popover = [device_bounds(1000, 400, 200, 40)];
    time("200x40 region over the popover", &popover, 8, 200);
    let tiny = [device_bounds(520, 300, 2, 18)];
    time("2x18 caret", &tiny, 8, 500);

    // What the text region's time goes to.
    let mut quads_only = Scene::default();
    for quad in &scene.quads {
        quads_only.insert_primitive(*quad);
    }
    quads_only.finish();
    let mut glyphs_only = Scene::default();
    for sprite in &scene.monochrome_sprites {
        glyphs_only.insert_primitive(*sprite);
    }
    glyphs_only.finish();
    let mut shadow_only = Scene::default();
    for shadow in &scene.shadows {
        shadow_only.insert_primitive(*shadow);
    }
    shadow_only.finish();
    for (label, scene, region) in [
        ("quads", &quads_only, &caret),
        ("glyphs", &glyphs_only, &caret),
        ("shadow over the popover", &shadow_only, &popover),
    ] {
        let mut samples = Vec::new();
        for _ in 0..200 {
            let start = std::time::Instant::now();
            draw(&mut canvas, scene, region, &mirror, &params, 1);
            samples.push(start.elapsed());
        }
        samples.sort();
        eprintln!(
            "200x40 region of text, {label} only: median {:?}",
            samples[100]
        );
    }
}

/// Only surfaces need the GPU.
#[test]
fn scenes_without_surfaces_can_be_drawn() {
    let scene = Scene::default();
    assert_eq!(super::can_draw(&scene, &[], &PARAMS_FOR_CAN_DRAW), Ok(()));
}

/// The GPU converts a fragment to the target's levels by truncating it to 12
/// bits, and blends levels: every gray level and hundredth of a level in
/// between, and random translucent grays over random pixels.
#[test]
fn fragments_blend_at_the_gpus_levels() {
    let mut harness = harness!();
    for mode in [OPAQUE, PREMULTIPLIED] {
        let mut levels = Scene::default();
        for k in 0..256 {
            for f in 0..100 {
                let l = ((k as f32 + f as f32 / 100.) / 255.).min(1.);
                levels.insert_primitive(quad(
                    sp(f as f32, k as f32, 1., 1.),
                    Hsla {
                        h: 0.,
                        s: 0.,
                        l,
                        a: 1.,
                    },
                ));
            }
        }
        levels.finish();
        let difference = harness.compare(&levels, 100, 256, mode);
        check(&difference, &format!("opaque levels {mode:?}"), 1);
        assert!(
            difference.differing <= 10,
            "{}",
            difference.report("opaque levels")
        );

        let mut rng = Rng::new(17);
        let (w, h) = (64u32, 64u32);
        let mut blended = Scene::default();
        let gray = |rng: &mut Rng| Hsla {
            h: 0.,
            s: 0.,
            l: rng.range(0., 1.),
            a: if rng.chance(0.5) {
                rng.range(0., 1.)
            } else {
                1.
            },
        };
        for i in 0..w * h {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            blended.insert_primitive(quad(sp(x, y, 1., 1.), gray(&mut rng)));
        }
        for i in 0..w * h {
            let (x, y) = ((i % w) as f32, (i / w) as f32);
            blended.insert_primitive(quad(sp(x, y, 1., 1.), gray(&mut rng)));
        }
        blended.finish();
        let difference = harness.compare(&blended, w, h, mode);
        check(&difference, &format!("blended levels {mode:?}"), 1);
        assert!(
            difference.differing <= 10,
            "{}",
            difference.report("blended levels")
        );
    }
}

/// Scaled images sample their texels as the GPU's bilinear filter does,
/// magnified and minified, at whole and fractional positions.
#[test]
fn scaled_sprites_sample_like_the_gpu() {
    let mut harness = harness!();
    let mut rng = Rng::new(23);
    let (tw, th) = (21, 22);
    let texels: Vec<u8> = (0..tw * th).map(|_| rng.int(0, 256) as u8).collect();
    let bytes: Vec<u8> = texels.iter().flat_map(|&v| [v, v, v, 255]).collect();
    let tile = harness.image(tw, th, bytes);
    let mut total = Difference::default();
    for (bx, by, bw, bh) in [
        (3.3, 5.7, 77.7, 51.3),
        (14., 13., 45., 11.),
        (10., 7., 77., 51.),
        (5., 5., 13., 40.),
        (2.5, 1.25, 30., 20.),
    ] {
        let mut scene = Scene::default();
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.,
            bounds: sp(bx, by, bw, bh),
            content_mask: no_mask(),
            corner_radii: Corners::default(),
            tile,
        });
        scene.finish();
        for mode in [OPAQUE, PREMULTIPLIED] {
            total.merge(&harness.compare(&scene, 96, 64, mode));
        }
    }
    check(&total, "scaled sprites", 2);
}

/// Draws each primitive of a random scene alone over its backdrop and
/// reports the ones the CPU draws most differently, for investigating.
/// `RASTER_KIND` (a `Kinds` name), `RASTER_SEED` and `RASTER_PREMULTIPLIED`
/// pick the scene.
#[test]
#[ignore]
fn isolate_differences() {
    let mut harness = harness!();
    let kind = match std::env::var("RASTER_KIND").as_deref() {
        Ok("Shadows") => Kinds::Shadows,
        Ok("Underlines") => Kinds::Underlines,
        Ok("Monochrome") => Kinds::Monochrome,
        Ok("Subpixel") => Kinds::Subpixel,
        Ok("Polychrome") => Kinds::Polychrome,
        Ok("Paths") => Kinds::Paths,
        Ok("Mixed") => Kinds::Mixed,
        _ => Kinds::Quads,
    };
    let seed: u64 = std::env::var("RASTER_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mode = if std::env::var("RASTER_PREMULTIPLIED").is_ok() {
        PREMULTIPLIED
    } else {
        OPAQUE
    };
    let mut rng = Rng::new(seed * 31 + kind as u64);
    let mut scene = random_scene(&mut harness, &mut rng, kind, 320., 240.);
    if !harness.is_nvidia() {
        solid_checkerboards(&mut scene);
    }
    let whole = harness.compare(&scene, 320, 240, mode);
    eprintln!("{}", whole.report("whole"));
    let backdrop: Vec<Quad> = scene
        .quads
        .iter()
        .filter(|q| q.bounds.size.width.0 == 320.)
        .cloned()
        .collect();
    let alone = |f: &dyn Fn(&mut Scene)| {
        let mut single = Scene::default();
        for quad in &backdrop {
            single.insert_primitive(*quad);
        }
        f(&mut single);
        single.finish();
        single
    };
    let mut results = Vec::new();
    for quad in scene.quads.iter().filter(|q| q.bounds.size.width.0 != 320.) {
        let single = alone(&|s| s.insert_primitive(*quad));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{quad:?}"),
        ));
    }
    for shadow in &scene.shadows {
        let single = alone(&|s| s.insert_primitive(*shadow));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{shadow:?}"),
        ));
    }
    for underline in &scene.underlines {
        let single = alone(&|s| s.insert_primitive(*underline));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{underline:?}"),
        ));
    }
    for sprite in &scene.monochrome_sprites {
        let single = alone(&|s| s.insert_primitive(*sprite));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{sprite:?}"),
        ));
    }
    for sprite in &scene.subpixel_sprites {
        let single = alone(&|s| s.insert_primitive(*sprite));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{sprite:?}"),
        ));
    }
    for sprite in &scene.polychrome_sprites {
        let single = alone(&|s| s.insert_primitive(*sprite));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{sprite:?}"),
        ));
    }
    for path in &scene.paths {
        let single = alone(&|s| s.insert_primitive(path.clone()));
        results.push((
            harness.compare(&single, 320, 240, mode),
            format!("{path:?}"),
        ));
    }
    if std::env::var("RASTER_DUMP").is_ok() {
        for sprite in &scene.subpixel_sprites {
            let single = alone(&|s| s.insert_primitive(*sprite));
            let gpu = harness.gpu(&single, 320, 240, mode);
            let cpu = harness.cpu(&single, 320, 240, mode, 1);
            if compare(&cpu, &gpu, 320).largest() > 2 {
                for i in 0..gpu.len() {
                    if gpu[i] != cpu[i] {
                        eprintln!(
                            "DUMP {} {} cpu {:08x} gpu {:08x}",
                            i % 320,
                            i / 320,
                            cpu[i],
                            gpu[i]
                        );
                    }
                }
            }
        }
    }
    results.sort_by_key(|(d, _)| std::cmp::Reverse(d.largest()));
    for (difference, primitive) in results.iter().take(6) {
        eprintln!("{}\n  {primitive}", difference.report("alone"));
    }
}

#[cfg(test)]
const PARAMS_FOR_CAN_DRAW: RasterParams = RasterParams {
    gamma_ratios: [0.; 4],
    grayscale_enhanced_contrast: 1.,
    subpixel_enhanced_contrast: 1.,
    is_bgr: false,
    premultiplied_alpha: false,
    dual_source_blending: true,
    path_sample_count: 4,
    fragment_bits: 12,
};
