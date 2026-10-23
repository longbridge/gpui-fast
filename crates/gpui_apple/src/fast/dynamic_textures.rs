//! Dynamic texture support for the Metal renderer.
//!
//! The core's [`gpui::DynamicTexturePlatform`] contract arrives here with
//! tightly packed BGRA8 rows. Each texture is its own `MTLTexture` — outside
//! the sprite atlas — so an in-place sub-region upload can never disturb the
//! glyphs and icons sharing an atlas texture, and the renderer resolves a
//! dynamic texture id through [`metal_texture`] before it consults the atlas.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    Bounds, DevicePixels, DynamicTextureId, DynamicTexturePlatform, Size,
    set_dynamic_texture_platform,
};
use metal::{MTLPixelFormat, MTLRegion, MTLTextureUsage};
use parking_lot::{Mutex, RwLock};

/// The textures one renderer's device owns, looked up by the draw path.
static METAL_TEXTURES: RwLock<Option<Arc<MetalDynamicTextures>>> = RwLock::new(None);

/// Installs the dynamic texture backend for `device`.
///
/// Called from the renderer's constructor. The first call wins: every
/// renderer in the process selects the same Metal device, and textures must
/// stay on the device that draws them.
pub(crate) fn init(device: &metal::DeviceRef) {
    let mut textures = METAL_TEXTURES.write();
    if textures.is_none() {
        *textures = Some(Arc::new(MetalDynamicTextures {
            device: device.to_owned(),
            // iOS does not support managed resources; see the renderer's own
            // reasoning around `has_unified_memory`.
            supports_shared_storage: cfg!(target_os = "ios") || device.has_unified_memory(),
            textures: Mutex::new(HashMap::new()),
        }));
        set_dynamic_texture_platform(textures.as_ref().unwrap().clone());
    }
}

/// The Metal texture behind an atlas texture id the scene carries, if it is
/// one of this process's dynamic textures and it belongs to `device`.
pub(crate) fn metal_texture(
    device: &metal::DeviceRef,
    id: gpui::AtlasTextureId,
) -> Option<metal::Texture> {
    let dynamic_id = DynamicTextureId::from_atlas_texture_id(id)?;
    let textures = METAL_TEXTURES.read().clone()?;
    if textures.device.registry_id() != device.registry_id() {
        return None;
    }
    textures
        .textures
        .lock()
        .get(&dynamic_id)
        .map(|(texture, _)| texture.clone())
}

struct MetalDynamicTextures {
    device: metal::Device,
    supports_shared_storage: bool,
    textures: Mutex<HashMap<DynamicTextureId, (metal::Texture, Size<DevicePixels>)>>,
}

impl MetalDynamicTextures {
    /// A standalone BGRA8 texture, shared storage when the device allows it.
    fn new_texture(&self, size: Size<DevicePixels>) -> metal::Texture {
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_width(size.width.0 as u64);
        descriptor.set_height(size.height.0 as u64);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor.set_usage(MTLTextureUsage::ShaderRead);
        descriptor.set_storage_mode(if self.supports_shared_storage {
            metal::MTLStorageMode::Shared
        } else {
            metal::MTLStorageMode::Managed
        });
        self.device.new_texture(&descriptor)
    }
}

impl DynamicTexturePlatform for MetalDynamicTextures {
    fn create_texture(
        &self,
        id: DynamicTextureId,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let expected = size.width.0 as usize * size.height.0 as usize * 4;
        anyhow::ensure!(
            bytes.len() == expected,
            "dynamic texture payload does not match its dimensions"
        );

        let texture = self.new_texture(size);
        texture.replace_region(
            MTLRegion::new_2d(0, 0, size.width.0 as u64, size.height.0 as u64),
            0,
            bytes.as_ptr() as *const _,
            size.width.0 as u64 * 4,
        );
        self.textures.lock().insert(id, (texture, size));
        Ok(())
    }

    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        let entry = self
            .textures
            .lock()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown dynamic texture {id:?}"))?;
        let (texture, texture_size) = entry;

        // The core validates the region against the created size; re-check
        // the region against this texture so a stale id cannot write past it.
        anyhow::ensure!(
            bounds.origin.x.0 + bounds.size.width.0 <= texture_size.width.0
                && bounds.origin.y.0 + bounds.size.height.0 <= texture_size.height.0,
            "dynamic texture update is outside the texture"
        );

        texture.replace_region(
            MTLRegion::new_2d(
                bounds.origin.x.0 as u64,
                bounds.origin.y.0 as u64,
                bounds.size.width.0 as u64,
                bounds.size.height.0 as u64,
            ),
            0,
            bytes.as_ptr() as *const _,
            bounds.size.width.0 as u64 * 4,
        );
        Ok(())
    }

    fn remove_texture(&self, id: DynamicTextureId) {
        self.textures.lock().remove(&id);
    }
}
