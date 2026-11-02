//! Dynamic texture support for the Metal renderer.
//!
//! The core's [`gpui::DynamicTexturePlatform`] contract arrives here with
//! tightly packed BGRA8 rows. Each texture is its own `MTLTexture` — outside
//! the sprite atlas — so an in-place sub-region upload can never disturb the
//! glyphs and icons sharing an atlas texture, and the renderer resolves a
//! dynamic texture id through [`metal_texture`] before it consults the atlas.
//!
//! Updates never overwrite a texture on the CPU: [`update_texture`] stages
//! the rows in a buffer, and [`encode_uploads`] copies them into the texture
//! with a blit on the sampling frame's own command buffer. Encoders on a
//! command buffer run in order, and command buffers on one queue run in the
//! order committed, so a frame never samples a region an in-flight
//! predecessor is still overwriting.

use std::{collections::HashMap, sync::Arc};

use gpui::{
    Bounds, DevicePixels, DynamicTextureId, DynamicTexturePlatform, Size,
    set_dynamic_texture_platform,
};
use metal::{
    MTLBlitOption, MTLOrigin, MTLPixelFormat, MTLRegion, MTLResourceOptions, MTLSize,
    MTLTextureUsage, NSRange,
};
use parking_lot::{Mutex, RwLock};

/// The textures one renderer's device owns, looked up by the draw path.
static METAL_TEXTURES: RwLock<Option<Arc<MetalDynamicTextures>>> = RwLock::new(None);

/// Installs the dynamic texture backend for `device`.
///
/// Called from the renderer's constructor. The first call wins: every
/// renderer in the process selects the same Metal device, and textures must
/// stay on the device that draws them.
///
/// `supports_shared_storage` is the renderer's own answer, not a guess from
/// the device: only Apple GPUs can create shared textures on macOS, and an
/// Intel Mac reports unified memory it cannot back a shared texture with.
pub(crate) fn init(device: &metal::DeviceRef, supports_shared_storage: bool) {
    let mut textures = METAL_TEXTURES.write();
    if textures.is_none() {
        *textures = Some(Arc::new(MetalDynamicTextures {
            device: device.to_owned(),
            supports_shared_storage,
            textures: Mutex::new(HashMap::new()),
            pending_uploads: Mutex::new(Vec::new()),
        }));
        set_dynamic_texture_platform(textures.as_ref().unwrap().clone());
    }
}

/// The command queue every renderer in the process shares.
///
/// Each renderer used to own a queue, which left an upload staged for one
/// window unordered against another window's committed frames sampling the
/// same texture: queues only order their own command buffers. One process
/// queue restores the order — buffers run in the order committed, and a
/// frame's blits precede its passes in the same buffer — so no frame samples
/// a region an in-flight upload is overwriting, whatever window encodes it.
pub(crate) fn command_queue(device: &metal::DeviceRef) -> metal::CommandQueue {
    static SHARED_QUEUE: RwLock<Option<(u64, metal::CommandQueue)>> = RwLock::new(None);
    let registry_id = device.registry_id();
    let mut shared = SHARED_QUEUE.write();
    if let Some((installed_for, queue)) = shared.as_ref() {
        if *installed_for == registry_id {
            return queue.clone();
        }
    }
    let queue = device.new_command_queue();
    *shared = Some((registry_id, queue.clone()));
    queue
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

/// Copies the staged texture updates into their textures on `command_buffer`.
///
/// Called with each frame's command buffer, before the frame encodes any
/// pass that samples the textures. A staged update whose texture is gone is
/// dropped with its buffer; the command buffer retains the rest until it
/// completes, so the buffers may be released here.
pub(crate) fn encode_uploads(command_buffer: &metal::CommandBufferRef) {
    let Some(textures) = METAL_TEXTURES.read().clone() else {
        return;
    };
    let pending = textures
        .pending_uploads
        .lock()
        .drain(..)
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return;
    }

    let encoder = command_buffer.new_blit_command_encoder();
    for upload in pending {
        let Some((texture, _)) = textures.textures.lock().get(&upload.id).cloned() else {
            continue;
        };
        encoder.copy_from_buffer_to_texture(
            &upload.buffer,
            0,
            upload.bytes_per_row,
            0,
            MTLSize::new(upload.region.size.width, upload.region.size.height, 1),
            &texture,
            0,
            0,
            MTLOrigin {
                x: upload.region.origin.x,
                y: upload.region.origin.y,
                z: 0,
            },
            MTLBlitOption::empty(),
        );
    }
    encoder.end_encoding();
}

struct MetalDynamicTextures {
    device: metal::Device,
    supports_shared_storage: bool,
    textures: Mutex<HashMap<DynamicTextureId, (metal::Texture, Size<DevicePixels>)>>,
    /// Updates staged by [`update_texture`], copied into their textures by
    /// the frame that samples them (see [`encode_uploads`]).
    pending_uploads: Mutex<Vec<PendingUpload>>,
}

/// One staged update: tightly packed rows for a region of a texture, in a
/// buffer the CPU wrote and a frame's blit copies from.
struct PendingUpload {
    id: DynamicTextureId,
    buffer: metal::Buffer,
    bytes_per_row: u64,
    region: MTLRegion,
}

impl MetalDynamicTextures {
    /// A standalone BGRA8 texture, shared storage when the device allows it.
    fn new_texture(&self, size: Size<DevicePixels>) -> metal::Texture {
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_width(size.width.0 as u64);
        descriptor.set_height(size.height.0 as u64);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        // A blit copy into the texture writes it on the GPU, which needs the
        // write usage alongside the shader reads the draw path performs.
        descriptor.set_usage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
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
        // The initial upload happens before any frame can reference the
        // texture's never-reused id, so no in-flight frame samples it.
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
        let (_, texture_size) = entry;

        // The core validates the region against the created size; re-check
        // the region against this texture so a stale id cannot write past it.
        anyhow::ensure!(
            bounds.origin.x.0 + bounds.size.width.0 <= texture_size.width.0
                && bounds.origin.y.0 + bounds.size.height.0 <= texture_size.height.0,
            "dynamic texture update is outside the texture"
        );

        // Stage the rows in a buffer instead of `replace_region`, which
        // would overwrite a texture a committed frame may still be sampling.
        let options = if self.supports_shared_storage {
            MTLResourceOptions::StorageModeShared
        } else {
            MTLResourceOptions::StorageModeManaged
        };
        let buffer = self.device.new_buffer(bytes.len() as u64, options);
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                buffer.contents() as *mut u8,
                bytes.len(),
            );
        }
        if !self.supports_shared_storage {
            // Managed storage: tell Metal the CPU touched the whole buffer
            // before it is read.
            buffer.did_modify_range(NSRange {
                location: 0,
                length: bytes.len() as u64,
            });
        }
        // Bound the staging queue per texture: a window that stopped
        // drawing — hidden or minimized — while its producer keeps updating
        // would otherwise pile buffers up without limit. The oldest staged
        // updates go first; for a live stream the newest contents are the
        // ones worth drawing when the window comes back, and the regions an
        // older staged update covered arrive again with the producer's next
        // frames.
        const MAX_PENDING_PER_TEXTURE: usize = 4;
        let mut pending = self.pending_uploads.lock();
        let mut staged = pending.iter().filter(|upload| upload.id == id).count();
        while staged >= MAX_PENDING_PER_TEXTURE {
            let oldest = pending.iter().position(|upload| upload.id == id);
            pending.remove(oldest.expect("counted a staged update above"));
            staged -= 1;
        }
        pending.push(PendingUpload {
            id,
            buffer,
            bytes_per_row: bounds.size.width.0 as u64 * 4,
            region: MTLRegion::new_2d(
                bounds.origin.x.0 as u64,
                bounds.origin.y.0 as u64,
                bounds.size.width.0 as u64,
                bounds.size.height.0 as u64,
            ),
        });
        Ok(())
    }

    fn remove_texture(&self, id: DynamicTextureId) {
        self.textures.lock().remove(&id);
        // Drop the staged updates of a texture that is gone; their buffers
        // would otherwise wait for a frame that will never sample them.
        self.pending_uploads.lock().retain(|upload| upload.id != id);
    }
}
