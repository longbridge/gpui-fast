//! The private rounded-clip layer protocol is shared with the Metal renderer:
//! reserved sprite ids identify drawable groups, each carrying a mask scene.
//! Every colour pipeline samples its group's mask. A one-texel, disabled
//! binding keeps ordinary frames on the same pipelines without mask passes.

use gpui::{AtlasTextureId, LayerFrame, Scene};
use std::cell::RefCell;

/// Sibling scopes execute in separate ordered render passes, so they can
/// reuse a texture after the previous scope ends. Ancestors remain live at
/// other depths. Resource use depends on nesting depth, not item count.
#[derive(Default)]
pub(crate) struct MaskPool(RefCell<Vec<MaskSlot>>);

struct MaskSlot {
    depth: usize,
    size: wgpu::Extent3d,
    format: wgpu::TextureFormat,
    view: wgpu::TextureView,
    binding: wgpu::BindGroup,
    used: bool,
}

impl MaskPool {
    pub(crate) fn begin_frame(&self) {
        let mut slots = self.0.borrow_mut();
        slots.retain(|slot| slot.used);
        for slot in &mut *slots {
            slot.used = false;
        }
    }

    pub(crate) fn get(
        &self,
        device: &wgpu::Device,
        bindings: &Bindings,
        format: wgpu::TextureFormat,
        size: wgpu::Extent3d,
        depth: usize,
    ) -> (wgpu::TextureView, wgpu::BindGroup) {
        let mut slots = self.0.borrow_mut();
        let index = slots
            .iter()
            .position(|slot| slot.depth == depth && slot.size == size && slot.format == format)
            .unwrap_or_else(|| {
                let texture = texture(device, format, size.width, size.height);
                let view = texture.create_view(&Default::default());
                let binding = bindings.mask(device, &view);
                slots.push(MaskSlot {
                    depth,
                    size,
                    format,
                    view,
                    binding,
                    used: true,
                });
                slots.len() - 1
            });
        let slot = &mut slots[index];
        slot.used = true;
        (slot.view.clone(), slot.binding.clone())
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.borrow().len()
    }
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
pub(crate) fn headless_instance() -> wgpu::Instance {
    if cfg!(target_os = "macos") {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        })
    } else {
        crate::WgpuContext::instance(None)
    }
}

pub(crate) struct Bindings {
    pub(crate) layout: wgpu::BindGroupLayout,
    pub(crate) unmasked: wgpu::BindGroup,
}

impl Bindings {
    pub(crate) fn new(device: &wgpu::Device) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rounded_clip_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
            ],
        });
        let dummy = texture(device, wgpu::TextureFormat::Rgba8Unorm, 1, 1);
        let unmasked = bind(
            device,
            &layout,
            &dummy.create_view(&Default::default()),
            false,
        );
        Self { layout, unmasked }
    }

    pub(crate) fn mask(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> wgpu::BindGroup {
        bind(device, &self.layout, view, true)
    }
}

fn bind(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view: &wgpu::TextureView,
    enabled: bool,
) -> wgpu::BindGroup {
    let flags = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rounded_clip_flags"),
        size: 16,
        usage: wgpu::BufferUsages::UNIFORM,
        mapped_at_creation: true,
    });
    flags
        .slice(..)
        .get_mapped_range_mut()
        .copy_from_slice(bytemuck::bytes_of(&[u32::from(enabled), 0, 0, 0]));
    flags.unmap();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("rounded_clip_bind_group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: flags.as_entire_binding(),
            },
        ],
    })
}

pub(crate) fn texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rounded_clip_mask"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

pub(crate) fn pipeline_layouts<'a>(
    layouts: &mut Vec<Option<&'a wgpu::BindGroupLayout>>,
    all: &'a crate::wgpu_renderer::WgpuBindGroupLayouts,
) {
    layouts.resize(3, None);
    layouts.push(Some(&all.fast_rounded_clip.layout));
}

pub(crate) fn bind_unmasked(
    pass: &mut wgpu::RenderPass<'_>,
    resources: &crate::wgpu_renderer::WgpuResources,
) {
    pass.set_bind_group(
        3,
        &resources.bind_group_layouts.fast_rounded_clip.unmasked,
        &[],
    );
}

pub(crate) fn shader_source(source: &str) -> wgpu::ShaderSource<'static> {
    wgpu::ShaderSource::Wgsl(shader(source).into())
}

pub(crate) fn group(scene: &Scene, texture: AtlasTextureId) -> Option<&LayerFrame> {
    if texture.index < 0xF100_0000 {
        return None;
    }
    let key = texture.index - 0xF000_0000;
    scene
        .layers
        .frames
        .iter()
        .find(|frame| frame.key.0 == key && frame.tile_size == 0)
}

/// Extend the unchanged upstream WGSL entry points. Path rasterization is
/// unmasked; paths are masked once, when their premultiplied sprite is drawn.
pub(crate) fn shader(source: &str) -> String {
    let mut source = source.to_owned();
    for name in [
        "fs_quad",
        "fs_shadow",
        "fs_path",
        "fs_underline",
        "fs_mono_sprite",
        "fs_poly_sprite",
        "fs_surface",
        "fs_subpixel_sprite",
    ] {
        let Some(start) = source.find(&format!("fn {name}(")) else {
            continue;
        };
        let body_start = start + source[start..].find('{').expect("fragment body");
        let mut depth = 1;
        let mut body_end = body_start + 1;
        for (offset, ch) in source[body_start + 1..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                body_end = body_start + 1 + offset;
                break;
            }
        }
        let helper = if name == "fs_subpixel_sprite" {
            "fast_clip_subpixel"
        } else {
            "fast_clip_color"
        };
        let premultiplied = if name == "fs_path" {
            "true"
        } else {
            "globals.premultiplied_alpha != 0u"
        };
        let mut remaining = &source[body_start + 1..body_end];
        let mut body = String::new();
        while let Some(start) = remaining.find("return ") {
            let end = start + remaining[start..].find(';').expect("fragment return");
            body.push_str(&remaining[..start]);
            let expression = &remaining[start + 7..end];
            if name == "fs_subpixel_sprite" {
                body.push_str(&format!(
                    "return {helper}({expression}, input.position.xy);"
                ));
            } else {
                body.push_str(&format!(
                    "return {helper}({expression}, input.position.xy, {premultiplied});"
                ));
            }
            remaining = &remaining[end + 1..];
        }
        body.push_str(remaining);
        source.replace_range(body_start + 1..body_end, &body);
    }
    source.push_str(
        r#"
@group(3) @binding(0) var fast_clip_texture: texture_2d<f32>;
@group(3) @binding(1) var<uniform> fast_clip_flags: vec4<u32>;

fn fast_clip_coverage(position: vec2<f32>) -> f32 {
    if (fast_clip_flags.x == 0u) { return 1.0; }
    return textureLoad(fast_clip_texture, vec2<i32>(position), 0).a;
}

fn fast_clip_color(color: vec4<f32>, position: vec2<f32>, premultiplied: bool) -> vec4<f32> {
    let coverage = fast_clip_coverage(position);
    return vec4<f32>(color.rgb * select(1.0, coverage, premultiplied), color.a * coverage);
}
"#,
    );
    if source.contains("fn fs_subpixel_sprite(") {
        source.push_str(r#"
fn fast_clip_subpixel(color: SubpixelSpriteFragmentOutput, position: vec2<f32>) -> SubpixelSpriteFragmentOutput {
    var out = color;
    out.alpha *= fast_clip_coverage(position);
    return out;
}
"#);
    }
    source
}

#[cfg(test)]
mod tests {
    use super::shader;

    fn validate(source: &str, capabilities: naga::valid::Capabilities) {
        let module = naga::front::wgsl::parse_str(source).expect("rounded clip shader parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), capabilities)
            .validate(&module)
            .expect("rounded clip shader validates");
    }

    #[test]
    fn rounded_clip_storage_and_texture_shaders_validate() {
        for transport in [
            include_str!("../shaders_storage.wgsl"),
            include_str!("../shaders_webgl.wgsl"),
        ] {
            let source = format!("{}\n{transport}", include_str!("../shaders.wgsl"));
            let extended = shader(&source);
            validate(&extended, naga::valid::Capabilities::empty());
            assert_eq!(
                source.contains("var<storage"),
                extended.contains("var<storage")
            );
        }
    }

    #[test]
    fn rounded_clip_dual_source_shader_validates() {
        let source = format!(
            "enable dual_source_blending;\n{}\n{}\n{}",
            include_str!("../shaders.wgsl"),
            include_str!("../shaders_storage.wgsl"),
            include_str!("../shaders_subpixel.wgsl")
        );
        validate(
            &shader(&source),
            naga::valid::Capabilities::DUAL_SOURCE_BLENDING,
        );
    }
}
