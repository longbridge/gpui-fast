//! Rounded scopes use the same mask-scene protocol as Metal and WGPU. Colour
//! is drawn into the existing target; only mask coverage is rasterized into
//! separate textures. Siblings reuse storage after a scope finishes, while
//! active ancestors keep their masks at separate depths.

use std::{path::PathBuf, slice};

use anyhow::{Context as _, Result};
use gpui::{AtlasTextureId, LayerFrame, Scene};
use windows::{
    Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_SUBRESOURCE_DATA,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_IMMUTABLE, ID3D11Buffer, ID3D11Device,
        ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D,
    },
    core::Interface as _,
};

use crate::directx_renderer::{
    DirectXRenderPipelines, create_path_intermediate_msaa_texture_and_view,
    create_path_intermediate_texture,
};
use crate::fast::frame::{FrameState, PathTargets, Target, draw_scene_masked, upload};

pub(crate) fn shader_path(original: PathBuf, out_dir: &str) -> PathBuf {
    if original
        .file_name()
        .is_some_and(|name| name == "shaders.hlsl")
    {
        PathBuf::from(out_dir).join("rounded_clip_shaders.hlsl")
    } else {
        original
    }
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

#[derive(Default)]
pub(crate) struct Masks {
    device: Option<ID3D11Device>,
    flags: Option<ID3D11Buffer>,
    masks: Vec<MaskSlot>,
    paths: Vec<PathSlot>,
}

struct MaskSlot {
    size: (u32, u32),
    depth: usize,
    used: bool,
    image: MaskImage,
}

#[derive(Clone)]
struct MaskImage {
    target: Option<ID3D11RenderTargetView>,
    view: Option<ID3D11ShaderResourceView>,
}

struct PathSlot {
    size: (u32, u32),
    used: bool,
    image: PathImage,
}

#[derive(Clone)]
struct PathImage {
    intermediate: ID3D11Texture2D,
    view: Option<ID3D11ShaderResourceView>,
    msaa: ID3D11Texture2D,
    msaa_target: Option<ID3D11RenderTargetView>,
}

impl Masks {
    fn for_device(&mut self, device: &ID3D11Device) {
        if self.device.as_ref() != Some(device) {
            *self = Self {
                device: Some(device.clone()),
                ..Default::default()
            };
        }
    }

    fn image(
        &mut self,
        device: &ID3D11Device,
        size: (u32, u32),
        depth: usize,
    ) -> Result<MaskImage> {
        self.for_device(device);
        if let Some(slot) = self
            .masks
            .iter_mut()
            .find(|slot| slot.size == size && slot.depth == depth)
        {
            slot.used = true;
            return Ok(slot.image.clone());
        }
        let (texture, view) = create_path_intermediate_texture(device, size.0, size.1)?;
        let mut target = None;
        unsafe {
            device.CreateRenderTargetView(&texture, None, Some(&mut target))?;
        }
        let image = MaskImage { target, view };
        self.masks.push(MaskSlot {
            size,
            depth,
            used: true,
            image: image.clone(),
        });
        Ok(image)
    }

    fn paths(&mut self, device: &ID3D11Device, size: (u32, u32)) -> Result<PathImage> {
        if let Some(slot) = self.paths.iter_mut().find(|slot| slot.size == size) {
            slot.used = true;
            return Ok(slot.image.clone());
        }
        let (intermediate, view) = create_path_intermediate_texture(device, size.0, size.1)?;
        let (msaa, msaa_target) =
            create_path_intermediate_msaa_texture_and_view(device, size.0, size.1)?;
        let image = PathImage {
            intermediate,
            view,
            msaa,
            msaa_target,
        };
        self.paths.push(PathSlot {
            size,
            used: true,
            image: image.clone(),
        });
        Ok(image)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.masks.len()
    }
}

pub(crate) fn begin_frame(masks: &mut Masks) {
    masks.masks.retain(|slot| slot.used);
    masks.paths.retain(|slot| slot.used);
    for slot in &mut masks.masks {
        slot.used = false;
    }
    for slot in &mut masks.paths {
        slot.used = false;
    }
}

pub(crate) fn bind(
    target: &Target<'_>,
    masks: &mut Masks,
    view: Option<&ID3D11ShaderResourceView>,
) -> Result<()> {
    masks.for_device(target.device);
    if view.is_some() && masks.flags.is_none() {
        let values = [1u32, 0, 0, 0];
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: values.as_ptr().cast(),
            ..Default::default()
        };
        unsafe {
            target
                .device
                .CreateBuffer(&desc, Some(&data), Some(&mut masks.flags))?;
        }
    }
    let flags = if view.is_some() {
        masks.flags.clone()
    } else {
        None
    };
    unsafe {
        target
            .device_context
            .PSSetConstantBuffers(2, Some(slice::from_ref(&flags)));
        target
            .device_context
            .PSSetShaderResources(15, Some(slice::from_ref(&view.cloned())));
    }
    Ok(())
}

pub(crate) fn draw_group(
    target: &Target<'_>,
    pipelines: &mut DirectXRenderPipelines,
    frame: &mut FrameState,
    group: &LayerFrame,
    parent_mask: Option<&ID3D11ShaderResourceView>,
    depth: usize,
) -> Result<()> {
    let child = group.content.scene().context("rounded clip content")?;
    let mask = child
        .layers
        .frames
        .iter()
        .find(|frame| frame.key.0 == u32::MAX)
        .and_then(|frame| frame.content.scene())
        .context("rounded clip mask")?;
    let context = target.device_context;
    let mut parent = [None];
    unsafe {
        context.OMGetRenderTargets(Some(&mut parent), None);
    }
    let render_target = parent[0].clone().context("rounded clip render target")?;
    let resource = unsafe { render_target.GetResource()? };
    let texture: ID3D11Texture2D = resource.cast()?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }
    let size = (desc.Width, desc.Height);
    let image = frame.rounded_masks.image(target.device, size, depth)?;
    let fallback = if target.paths.is_none() {
        Some(frame.rounded_masks.paths(target.device, size)?)
    } else {
        None
    };
    let parent_view = Some(render_target);
    let paths = target.paths.unwrap_or_else(|| {
        let image = fallback.as_ref().unwrap();
        PathTargets {
            render_target_view: &parent_view,
            intermediate_texture: &image.intermediate,
            intermediate_srv: &image.view,
            msaa_texture: &image.msaa,
            msaa_view: &image.msaa_target,
        }
    });
    let result = (|| {
        unsafe {
            context.OMSetRenderTargets(Some(slice::from_ref(&image.target)), None);
            context.ClearRenderTargetView(
                image.target.as_ref().context("rounded clip mask target")?,
                &[0.; 4],
            );
        }
        let mask_target = Target {
            device: target.device,
            device_context: context,
            atlas: target.atlas,
            globals: target.globals,
            paths: Some(PathTargets {
                render_target_view: &image.target,
                ..paths
            }),
        };
        upload(&mask_target, pipelines, mask)?;
        draw_scene_masked(&mask_target, pipelines, frame, mask, parent_mask, depth + 1)?;
        unsafe {
            context.OMSetRenderTargets(Some(slice::from_ref(&parent_view)), None);
        }
        let content_target = Target {
            device: target.device,
            device_context: context,
            atlas: target.atlas,
            globals: target.globals,
            paths: Some(PathTargets {
                render_target_view: &parent_view,
                ..paths
            }),
        };
        upload(&content_target, pipelines, child)?;
        draw_scene_masked(
            &content_target,
            pipelines,
            frame,
            child,
            image.view.as_ref(),
            depth + 1,
        )
    })();
    unsafe {
        context.OMSetRenderTargets(Some(slice::from_ref(&parent_view)), None);
    }
    bind(target, &mut frame.rounded_masks, parent_mask)?;
    frame.state.forget();
    result
}
