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
    /// the frame that samples them (see [`encode_uploads`]). Bounded per
    /// texture by bytes; see the budget in [`update_texture`].
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
        // `newTextureWithDescriptor:` answers nil when the device cannot back
        // the descriptor — a texture past its size limit, or a storage mode it
        // does not support. Every property of that object reads as zero, so a
        // sprite sampling it would draw nothing and the caller would have no
        // idea why; refuse here instead.
        anyhow::ensure!(
            texture.width() == size.width.0.max(0) as u64
                && texture.height() == size.height.0.max(0) as u64,
            "the device could not allocate a {}x{} dynamic texture",
            size.width.0,
            size.height.0
        );

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
        // A buffer the device refuses comes back empty, and writing the rows
        // through its null `contents()` would crash instead of reporting it.
        anyhow::ensure!(
            buffer.length() >= bytes.len() as u64,
            "the device could not stage a {} byte dynamic texture update",
            bytes.len()
        );
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
        // Bound the staging queue per texture by bytes, not by update
        // count: a producer that splits a frame into many small dirty
        // regions pushes a legitimate same-tick burst, which a fixed
        // count would truncate. What needs bounding is a stream into a
        // window that stopped drawing, where nothing drains the queue.
        // At the budget's edge the update reports failure instead of
        // dropping an older staged region: the caller learns the pixels
        // have not reached the GPU, and the core does not advance the
        // texture's version, so no cached tile re-renders against
        // contents it will not get.
        const MAX_STAGED_FULL_FRAMES: u64 = 4;
        let frame_bytes = texture_size.width.0 as u64 * texture_size.height.0 as u64 * 4;
        let mut pending = self.pending_uploads.lock();
        let staged_bytes = pending
            .iter()
            .filter(|upload| upload.id == id)
            .map(|upload| upload.buffer.length())
            .sum::<u64>();
        anyhow::ensure!(
            staged_bytes + bytes.len() as u64 <= frame_bytes.saturating_mul(MAX_STAGED_FULL_FRAMES),
            "dynamic texture {id:?}'s staging queue is full; draw the window to drain it"
        );
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

#[cfg(test)]
mod tests {
    use super::{MetalDynamicTextures, PendingUpload};
    use gpui::{
        AtlasTextureId, AtlasTextureKind, Bounds, DevicePixels, DynamicTextureId,
        DynamicTexturePlatform, Size, point,
    };
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// A texture id for tests, named through the public mapping: an atlas
    /// texture id inside the reserved range maps back to the texture of the
    /// same offset. `BASE` mirrors the core's reserved range base, which is
    /// private to the `fast` module.
    fn test_id(offset: u32) -> DynamicTextureId {
        const BASE: u32 = 0xE000_0000;
        DynamicTextureId::from_atlas_texture_id(AtlasTextureId {
            index: BASE + offset,
            kind: AtlasTextureKind::Polychrome,
        })
        .expect("the reserved dynamic range maps to texture ids")
    }

    /// A backend over the host's Metal device, in managed storage so the
    /// textures allocate on every Mac the tests can run on. Staging is
    /// pure CPU work, so these tests exercise it without a GPU frame.
    fn managed_backend() -> Option<MetalDynamicTextures> {
        let device = metal::Device::system_default()?;
        Some(MetalDynamicTextures {
            device,
            supports_shared_storage: false,
            textures: Mutex::new(HashMap::new()),
            pending_uploads: Mutex::new(Vec::new()),
        })
    }

    fn size_of(width: i32, height: i32) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(width),
            height: DevicePixels(height),
        }
    }

    fn bounds_at(origin: (i32, i32), width: i32, height: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(origin.0), DevicePixels(origin.1)),
            size: size_of(width, height),
        }
    }

    /// Distinct bytes per pixel, so a staged region is recognisable.
    fn packed_bytes(width: i32, height: i32, seed: u8) -> Vec<u8> {
        (0..width * height * 4)
            .map(|i| seed.wrapping_add(i as u8))
            .collect()
    }

    fn staged_bytes(upload: &PendingUpload) -> Vec<u8> {
        let length = upload.buffer.length() as usize;
        unsafe { std::slice::from_raw_parts(upload.buffer.contents() as *const u8, length) }
            .to_vec()
    }

    #[test]
    fn stages_more_than_four_disjoint_updates_without_loss() {
        let Some(backend) = managed_backend() else {
            return;
        };
        let id = test_id(1);
        backend
            .create_texture(id, size_of(32, 32), &packed_bytes(32, 32, 0))
            .expect("created");

        // Six disjoint 2x2 updates — one past the per-texture count a
        // count cap would have kept, all distinct in place and content.
        let mut expected = Vec::new();
        for i in 0..6u32 {
            let bounds = bounds_at(((i % 3) as i32 * 4, (i / 3) as i32 * 4), 2, 2);
            let bytes = packed_bytes(2, 2, (i + 1) as u8);
            backend
                .update_texture(id, bounds, &bytes)
                .expect("every disjoint update is staged");
            expected.push((bounds, bytes));
        }

        let pending = backend.pending_uploads.lock();
        assert_eq!(
            pending.len(),
            expected.len(),
            "every disjoint update stays staged"
        );
        for (upload, (bounds, bytes)) in pending.iter().zip(&expected) {
            assert_eq!(upload.id, id);
            assert_eq!(upload.region.origin.x, bounds.origin.x.0 as u64);
            assert_eq!(upload.region.origin.y, bounds.origin.y.0 as u64);
            assert_eq!(upload.region.size.width, bounds.size.width.0 as u64);
            assert_eq!(upload.region.size.height, bounds.size.height.0 as u64);
            assert_eq!(&staged_bytes(upload)[..bytes.len()], bytes);
        }
    }

    #[test]
    fn refuses_updates_past_the_staging_budget() {
        let Some(backend) = managed_backend() else {
            return;
        };
        let id = test_id(2);
        // A 16x16 texture stages 1024 bytes per full frame; four fit.
        backend
            .create_texture(id, size_of(16, 16), &packed_bytes(16, 16, 0))
            .expect("created");

        let frame = packed_bytes(16, 16, 1);
        for _ in 0..4 {
            backend
                .update_texture(id, bounds_at((0, 0), 16, 16), &frame)
                .expect("four full frames fit the budget");
        }
        assert!(
            backend
                .update_texture(id, bounds_at((0, 0), 16, 16), &frame)
                .is_err()
        );
        assert_eq!(
            backend.pending_uploads.lock().len(),
            4,
            "the refusal leaves the staged updates untouched"
        );

        backend.remove_texture(id);
        assert!(
            backend.pending_uploads.lock().is_empty(),
            "removing the texture drops its staged updates"
        );
    }
}
