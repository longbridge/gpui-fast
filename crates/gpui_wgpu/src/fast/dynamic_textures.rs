//! Mutable textures sampled directly by the ordinary sprite pipeline.
//!
//! Two producers share [`TextureStore`]:
//! - CPU uploads arrive through [`gpui::DynamicTexturePlatform`] with tightly
//!   packed BGRA8 rows, update a per-texture pixel backing in place, and flush
//!   coalesced dirty regions into a standalone `wgpu::Texture` outside the
//!   sprite atlas in [`TextureStore::before_frame`] right before
//!   `Queue::submit` drains `PendingWrites`.
//! - GPU producers allocate owned render targets through
//!   [`WgpuContext::create_dynamic_texture`]; no GPU object enters GPUI's
//!   process-wide upload backend, so browser handles stay on their browser
//!   thread too.
//!
//! Each `WgpuContext` owns its lookup table, cloned by every renderer sharing
//! that device; a foreign context never resolves another device's sprite id.
//! Retaining the packed BGRA8 backing per live uploaded handle lets
//! `before_frame` avoid staging unsubmitted buffers while windows are occluded
//! and lets device-loss recovery restore both paused textures and untouched
//! regions outside a partial post-recovery update.

#[cfg(not(target_family = "wasm"))]
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::{collections::HashMap, sync::Arc};

#[cfg(not(target_family = "wasm"))]
use anyhow::Context as _;
use anyhow::Result;
use gpui::{Bounds, DevicePixels, DynamicTexture, DynamicTextureId, Pixels, Size, Window};
#[cfg(not(target_family = "wasm"))]
use gpui::{DynamicTexturePlatform, point, set_dynamic_texture_platform};
use parking_lot::Mutex;

#[cfg(not(target_family = "wasm"))]
use crate::DeviceErrorState;
use crate::WgpuContext;

/// BGRA8, matching the scene's polychrome sprite format.
#[cfg(not(target_family = "wasm"))]
const BYTES_PER_PIXEL: u32 = 4;

/// Maximum disjoint dirty rectangles tracked per texture between two rendered
/// frames; additional updates merge the pair with minimal bounding-area waste.
#[cfg(not(target_family = "wasm"))]
const MAX_DIRTY_RECTS: usize = 8;

#[cfg(not(target_family = "wasm"))]
const MIN_STAGING_BYTES: u64 = 6 << 20;
#[cfg(not(target_family = "wasm"))]
const MIN_WARM_STAGING_BYTES: u64 = 256 << 10;
#[cfg(not(target_family = "wasm"))]
const MAX_STAGING_BYTES: u64 = 128 << 20;
#[cfg(not(target_family = "wasm"))]
const STAGING_IDLE_UPLOADS: u8 = 4;

/// Monotonic generation of the process-wide CPU-upload table, checked lock-free
/// in [`TextureStore::before_frame`].
#[cfg(not(target_family = "wasm"))]
static GLOBAL_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default)]
pub(crate) struct TextureStore(Arc<TextureStoreInner>);

#[derive(Default)]
struct TextureStoreInner {
    views: Mutex<HashMap<DynamicTextureId, StoredTexture>>,
    #[cfg(not(target_family = "wasm"))]
    synced_epoch: AtomicU64,
    #[cfg(not(target_family = "wasm"))]
    upload_flush: Mutex<()>,
}

struct StoredTexture {
    view: wgpu::TextureView,
    #[cfg(not(target_family = "wasm"))]
    uploaded: bool,
}

impl TextureStore {
    pub(crate) fn view(&self, id: DynamicTextureId) -> Option<wgpu::TextureView> {
        self.0.views.lock().get(&id).map(|entry| entry.view.clone())
    }

    pub(crate) fn before_frame(&self) {
        #[cfg(not(target_family = "wasm"))]
        {
            let target_epoch = GLOBAL_EPOCH.load(Ordering::Acquire);
            if self.0.synced_epoch.load(Ordering::Acquire) == target_epoch {
                return;
            }
            if let Some(platform) = PLATFORM.get() {
                let _flush = self.0.upload_flush.lock();
                if self.0.synced_epoch.load(Ordering::Acquire)
                    != GLOBAL_EPOCH.load(Ordering::Acquire)
                {
                    platform.flush_store(self);
                }
            }
        }
    }

    fn insert_view(&self, id: DynamicTextureId, view: wgpu::TextureView) {
        self.0.views.lock().insert(
            id,
            StoredTexture {
                view,
                #[cfg(not(target_family = "wasm"))]
                uploaded: false,
            },
        );
    }

    fn remove_view(&self, id: DynamicTextureId) {
        let mut views = self.0.views.lock();
        views.remove(&id);
        if views.is_empty() {
            *views = HashMap::new();
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn insert_uploaded(&self, id: DynamicTextureId, view: wgpu::TextureView) {
        self.0.views.lock().insert(
            id,
            StoredTexture {
                view,
                uploaded: true,
            },
        );
    }

    #[cfg(not(target_family = "wasm"))]
    fn clear_uploaded(&self) {
        let mut views = self.0.views.lock();
        views.retain(|_, entry| !entry.uploaded);
        if views.is_empty() {
            *views = HashMap::new();
        }
        self.0.synced_epoch.store(0, Ordering::Release);
    }

    #[cfg(not(target_family = "wasm"))]
    fn advance_synced_epoch(&self, expected_prev: u64, next: u64) {
        let _ = self.0.synced_epoch.compare_exchange(
            expected_prev,
            next,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}

#[cfg(not(target_family = "wasm"))]
struct GpuState {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    errors: Arc<DeviceErrorState>,
    format: wgpu::TextureFormat,
    max_texture_size: u32,
    max_buffer_size: u64,
    store: TextureStore,
    staging: TextureStaging,
}

#[cfg(not(target_family = "wasm"))]
impl GpuState {
    fn from_context(context: &WgpuContext) -> Self {
        let limits = context.device.limits();
        Self {
            device: Arc::clone(&context.device),
            queue: Arc::clone(&context.queue),
            errors: Arc::clone(context.errors()),
            format: context.color_texture_format(),
            max_texture_size: limits.max_texture_dimension_2d,
            max_buffer_size: limits.max_buffer_size,
            store: context.fast_dynamic_textures.clone(),
            staging: TextureStaging::default(),
        }
    }
}

#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct StagingBuffers {
    ready: Vec<wgpu::Buffer>,
    allocated: u64,
    generation: u64,
}

#[cfg(not(target_family = "wasm"))]
impl StagingBuffers {
    fn take(&mut self, bytes: u64) -> Option<wgpu::Buffer> {
        let (index, _) = self
            .ready
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.size() >= bytes)
            .min_by_key(|(_, buffer)| buffer.size())?;
        Some(self.ready.swap_remove(index))
    }

    fn reserve(&mut self, bytes: u64) -> Option<()> {
        while self.allocated + bytes > MAX_STAGING_BYTES {
            self.allocated -= self.ready.pop()?.size();
        }
        self.allocated += bytes;
        Some(())
    }

    fn recycle(&mut self, buffer: wgpu::Buffer, generation: u64, mapped: bool) {
        if mapped && self.generation == generation {
            self.ready.push(buffer);
        } else {
            self.allocated -= buffer.size();
        }
    }

    fn release(&mut self) {
        self.allocated -= self
            .ready
            .drain(..)
            .map(|buffer| buffer.size())
            .sum::<u64>();
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(not(target_family = "wasm"))]
#[derive(Default)]
struct TextureStaging {
    buffers: Arc<Mutex<StagingBuffers>>,
    encoder: Option<wgpu::CommandEncoder>,
    uploads_until_release: u8,
}

#[cfg(not(target_family = "wasm"))]
impl TextureStaging {
    fn upload(
        &mut self,
        device: &wgpu::Device,
        destination: wgpu::TexelCopyTextureInfo<'_>,
        extent: wgpu::Extent3d,
        payload: &[u8],
        bytes_per_row: u32,
        max_buffer_size: u64,
    ) -> bool {
        let row_bytes = extent.width * BYTES_PER_PIXEL;
        let stage_stride = row_bytes.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let length = u64::from(stage_stride) * u64::from(extent.height);
        let large = length >= MIN_STAGING_BYTES;
        if length > MAX_STAGING_BYTES.min(max_buffer_size)
            || !(large || self.uploads_until_release > 0 && length >= MIN_WARM_STAGING_BYTES)
        {
            return false;
        }
        if large {
            self.uploads_until_release = STAGING_IDLE_UPLOADS + 1;
        }
        let (buffer, generation) = {
            let mut buffers = self.buffers.lock();
            let buffer = buffers.take(length);
            if buffer.is_none() && buffers.reserve(length).is_none() {
                return false;
            }
            (buffer, buffers.generation)
        };
        let buffer = buffer.unwrap_or_else(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("dynamic_texture_upload"),
                size: length,
                usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: true,
            })
        });
        let Ok(mut mapped) = buffer.slice(..length).get_mapped_range_mut() else {
            self.buffers.lock().recycle(buffer, generation, false);
            return false;
        };
        if row_bytes == bytes_per_row && bytes_per_row == stage_stride {
            mapped.copy_from_slice(payload);
        } else {
            let row_bytes = row_bytes as usize;
            for (row, source) in payload.chunks(bytes_per_row as usize).enumerate() {
                let offset = row * stage_stride as usize;
                mapped
                    .slice(offset..offset + row_bytes)
                    .copy_from_slice(&source[..row_bytes]);
            }
        }
        drop(mapped);
        buffer.unmap();

        let encoder = self.encoder.get_or_insert_with(|| {
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("dynamic_texture_upload"),
            })
        });
        encoder.copy_buffer_to_texture(
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stage_stride),
                    rows_per_image: None,
                },
            },
            destination,
            extent,
        );
        let buffers = Arc::downgrade(&self.buffers);
        let mapped_buffer = buffer.clone();
        encoder.map_buffer_on_submit(&buffer, wgpu::MapMode::Write, .., move |result| {
            if let Some(buffers) = buffers.upgrade() {
                buffers
                    .lock()
                    .recycle(mapped_buffer, generation, result.is_ok());
            }
        });
        true
    }

    fn finish_uploads(&mut self) -> Option<wgpu::CommandBuffer> {
        if self.uploads_until_release > 0 {
            self.uploads_until_release -= 1;
            if self.uploads_until_release == 0 {
                self.release();
            }
        }
        self.encoder.take().map(wgpu::CommandEncoder::finish)
    }

    fn release(&mut self) {
        self.buffers.lock().release();
        self.uploads_until_release = 0;
    }
}

#[cfg(not(target_family = "wasm"))]
struct DirtySet {
    rects: [Bounds<DevicePixels>; MAX_DIRTY_RECTS],
    len: usize,
}

#[cfg(not(target_family = "wasm"))]
impl DirtySet {
    fn new() -> Self {
        let empty = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: Size {
                width: DevicePixels(0),
                height: DevicePixels(0),
            },
        };
        Self {
            rects: [empty; MAX_DIRTY_RECTS],
            len: 0,
        }
    }

    /// Inserts `incoming`, absorbing any rects whose union adds no wasted area
    /// and merging the minimum-waste pair when full so stored rects always keep
    /// positive pairwise bounding-box waste.
    fn push(&mut self, incoming: Bounds<DevicePixels>) {
        let mut candidate = incoming;
        loop {
            let mut candidate_area = bounds_area(candidate);
            while let Some(idx) = (0..self.len).find(|&i| {
                bounds_area(bounds_union(self.rects[i], candidate))
                    <= bounds_area(self.rects[i]).saturating_add(candidate_area)
            }) {
                candidate = bounds_union(self.rects[idx], candidate);
                candidate_area = bounds_area(candidate);
                self.len -= 1;
                self.rects[idx] = self.rects[self.len];
            }

            if self.len < MAX_DIRTY_RECTS {
                self.rects[self.len] = candidate;
                self.len += 1;
                return;
            }

            let mut best_pair = (0usize, MAX_DIRTY_RECTS);
            let mut best_waste = u64::MAX;
            for i in 0..MAX_DIRTY_RECTS {
                let area_i = bounds_area(self.rects[i]);
                let waste_c = bounds_area(bounds_union(self.rects[i], candidate))
                    .saturating_sub(area_i.saturating_add(candidate_area));
                if waste_c < best_waste {
                    best_waste = waste_c;
                    best_pair = (i, MAX_DIRTY_RECTS);
                }
                for j in (i + 1)..MAX_DIRTY_RECTS {
                    let area_j = bounds_area(self.rects[j]);
                    let waste_ij = bounds_area(bounds_union(self.rects[i], self.rects[j]))
                        .saturating_sub(area_i.saturating_add(area_j));
                    if waste_ij < best_waste {
                        best_waste = waste_ij;
                        best_pair = (i, j);
                    }
                }
            }

            let (i, j) = best_pair;
            if j == MAX_DIRTY_RECTS {
                candidate = bounds_union(self.rects[i], candidate);
                self.len -= 1;
                self.rects[i] = self.rects[self.len];
            } else {
                let merged = bounds_union(self.rects[i], self.rects[j]);
                self.rects[i] = candidate;
                self.len -= 1;
                self.rects[j] = self.rects[self.len];
                candidate = merged;
            }
        }
    }

    /// Returns the coalesced dirty regions accumulated since the last flush and
    /// clears the set.
    fn take(&mut self) -> ([Bounds<DevicePixels>; MAX_DIRTY_RECTS], usize) {
        let count = self.len;
        self.len = 0;
        if count <= 1 {
            return (self.rects, count);
        }
        let mut union_all = self.rects[0];
        let mut sum_area = bounds_area(self.rects[0]);
        for &rect in &self.rects[1..count] {
            union_all = bounds_union(union_all, rect);
            sum_area = sum_area.saturating_add(bounds_area(rect));
        }
        if bounds_area(union_all) <= sum_area {
            let mut out = self.rects;
            out[0] = union_all;
            return (out, 1);
        }
        (self.rects, count)
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

#[cfg(not(target_family = "wasm"))]
struct CpuEntry {
    size: Size<DevicePixels>,
    /// Tightly packed BGRA8 texels (`width * height * 4` bytes). Allocated once
    /// on creation and updated in place so `before_frame` flushes only when a
    /// frame renders, and device-loss recovery preserves both static textures
    /// and untouched pixels outside partial updates.
    pixels: Box<[u8]>,
    dirty: DirtySet,
}

#[cfg(not(target_family = "wasm"))]
struct UploadState {
    /// `None` between [`release_lost_device`] and [`handle_device_lost`], so
    /// the dead `Arc<wgpu::Device>` and `Arc<wgpu::Queue>` are dropped before
    /// `WgpuRenderer::recover` asks the driver for a replacement device.
    gpu: Option<GpuState>,
    epoch: u64,
    entries: HashMap<DynamicTextureId, CpuEntry>,
    /// Reused across uploads when the adapter fell back to `Rgba8Unorm` and
    /// BGRA8 pixels need channel swizzling; stays empty on `Bgra8Unorm`.
    swizzle_scratch: Vec<u8>,
}

#[cfg(not(target_family = "wasm"))]
struct WgpuDynamicTextures {
    state: Mutex<UploadState>,
}

#[cfg(not(target_family = "wasm"))]
static PLATFORM: OnceLock<Arc<WgpuDynamicTextures>> = OnceLock::new();

/// Installs the dynamic texture upload backend for `context`.
///
/// Called when the shared windowed or headless renderer comes up. The first
/// live context wins: every window in the process shares one `WgpuContext`,
/// while independent secondary contexts keep their own [`TextureStore`] and
/// skip foreign sprites. If the installed context's device was lost or released
/// for recovery, a healthy context adopts the backend.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn init(context: &WgpuContext) {
    let platform = PLATFORM.get_or_init(|| {
        let textures = Arc::new(WgpuDynamicTextures::new(context));
        set_dynamic_texture_platform(textures.clone());
        textures
    });
    if platform.needs_recovery() && !context.device_lost() {
        platform.replace_context(context);
    }
}

/// Drops the upload backend's dead `Arc<wgpu::Device>`, `Arc<wgpu::Queue>` and
/// GPU textures before `WgpuRenderer::recover` recreates the GPU context.
///
/// CPU entries survive so [`handle_device_lost`] and the next rendered frame
/// restore every live texture with its last pixels intact.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn release_lost_device() {
    if let Some(platform) = PLATFORM.get() {
        platform.release_device();
    }
}

/// Points the upload backend at the recreated context after a device-lost
/// recovery.
///
/// GPU uploads are flushed lazily in [`TextureStore::before_frame`] on the next
/// draw; subsequent windows recovering onto the same replacement context are a
/// no-op.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn handle_device_lost(context: &WgpuContext) {
    if let Some(platform) = PLATFORM.get() {
        platform.replace_context(context);
    }
}

#[cfg(not(target_family = "wasm"))]
impl WgpuDynamicTextures {
    fn new(context: &WgpuContext) -> Self {
        let gpu = GpuState::from_context(context);
        let epoch = GLOBAL_EPOCH.load(Ordering::Acquire);
        gpu.store.0.synced_epoch.store(epoch, Ordering::Release);
        Self {
            state: Mutex::new(UploadState {
                gpu: Some(gpu),
                epoch,
                entries: HashMap::new(),
                swizzle_scratch: Vec::new(),
            }),
        }
    }

    fn needs_recovery(&self) -> bool {
        self.state
            .lock()
            .gpu
            .as_ref()
            .is_none_or(|gpu| gpu.errors.device_lost())
    }

    fn release_device(&self) {
        let mut state = self.state.lock();
        if let Some(old_gpu) = state.gpu.take() {
            old_gpu.store.clear_uploaded();
        }
    }

    fn replace_context(&self, context: &WgpuContext) {
        let mut state = self.state.lock();
        if state
            .gpu
            .as_ref()
            .is_some_and(|gpu| Arc::ptr_eq(&gpu.device, &context.device))
        {
            return;
        }
        if let Some(old_gpu) = state.gpu.take() {
            old_gpu.store.clear_uploaded();
        }
        let next_epoch = GLOBAL_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
        state.epoch = next_epoch;
        context
            .fast_dynamic_textures
            .0
            .synced_epoch
            .store(0, Ordering::Release);
        state.gpu = Some(GpuState::from_context(context));
    }

    fn flush_store(&self, store: &TextureStore) {
        let mut state = self.state.lock();
        let UploadState {
            gpu,
            epoch,
            entries,
            swizzle_scratch,
        } = &mut *state;
        let Some(gpu) = gpu.as_mut() else {
            return;
        };
        if !Arc::ptr_eq(&store.0, &gpu.store.0) {
            store.0.synced_epoch.store(*epoch, Ordering::Release);
            return;
        }
        if gpu.errors.device_lost() {
            return;
        }
        sync_store_locked(gpu, entries, swizzle_scratch);
        let epoch = *epoch;
        let submission = gpu
            .staging
            .finish_uploads()
            .map(|commands| (Arc::clone(&gpu.queue), commands));
        drop(state);
        if let Some((queue, commands)) = submission {
            queue.submit([commands]);
        }
        store.0.synced_epoch.store(epoch, Ordering::Release);
    }
}

#[cfg(not(target_family = "wasm"))]
impl DynamicTexturePlatform for WgpuDynamicTextures {
    fn create_texture(
        &self,
        id: DynamicTextureId,
        size: Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let mut state = self.state.lock();
        let UploadState {
            gpu,
            epoch,
            entries,
            ..
        } = &mut *state;
        let gpu = gpu
            .as_ref()
            .filter(|gpu| !gpu.errors.device_lost())
            .context("dynamic texture device is lost")?;
        ensure_within_device_limit(size, gpu.max_texture_size)?;
        let bounds = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size,
        };
        let region = check_upload_region(size, bounds, bytes)?;

        if entries
            .insert(
                id,
                CpuEntry {
                    size,
                    pixels: Box::from(&bytes[..region.byte_len]),
                    dirty: DirtySet::new(),
                },
            )
            .is_some()
        {
            gpu.store.remove_view(id);
        }
        let next_epoch = GLOBAL_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
        *epoch = next_epoch;
        Ok(())
    }

    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()> {
        let mut state = self.state.lock();
        let UploadState {
            gpu,
            epoch,
            entries,
            ..
        } = &mut *state;
        let gpu = gpu
            .as_ref()
            .filter(|gpu| !gpu.errors.device_lost())
            .context("dynamic texture device is lost")?;
        let entry = entries
            .get_mut(&id)
            .context("dynamic texture was not created")?;
        ensure_within_device_limit(entry.size, gpu.max_texture_size)?;
        let region = check_upload_region(entry.size, bounds, bytes)?;

        update_cpu_pixels(
            &mut entry.pixels,
            entry.size,
            &region,
            &bytes[..region.byte_len],
        );
        entry.dirty.push(bounds);

        let next_epoch = GLOBAL_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
        *epoch = next_epoch;
        Ok(())
    }

    fn remove_texture(&self, id: DynamicTextureId) {
        let mut state = self.state.lock();
        let UploadState {
            gpu,
            epoch,
            entries,
            swizzle_scratch,
        } = &mut *state;
        if entries.remove(&id).is_none() {
            return;
        }
        let prev_epoch = *epoch;
        let next_epoch = GLOBAL_EPOCH.fetch_add(1, Ordering::AcqRel) + 1;
        *epoch = next_epoch;
        if let Some(gpu) = gpu.as_ref() {
            gpu.store.remove_view(id);
            gpu.store.advance_synced_epoch(prev_epoch, next_epoch);
        }
        if entries.is_empty() {
            *entries = HashMap::new();
            if let Some(gpu) = gpu.as_mut() {
                gpu.staging.release();
            }
        }
        if swizzle_scratch.capacity() > 0 {
            let max_remaining = entries.values().map(|e| e.pixels.len()).max().unwrap_or(0);
            if swizzle_scratch.capacity() > max_remaining {
                swizzle_scratch.clear();
                swizzle_scratch.shrink_to(max_remaining);
            }
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn bounds_union(a: Bounds<DevicePixels>, b: Bounds<DevicePixels>) -> Bounds<DevicePixels> {
    let left = a.origin.x.0.min(b.origin.x.0);
    let top = a.origin.y.0.min(b.origin.y.0);
    let right = (a.origin.x.0 + a.size.width.0).max(b.origin.x.0 + b.size.width.0);
    let bottom = (a.origin.y.0 + a.size.height.0).max(b.origin.y.0 + b.size.height.0);
    Bounds {
        origin: point(DevicePixels(left), DevicePixels(top)),
        size: Size {
            width: DevicePixels(right - left),
            height: DevicePixels(bottom - top),
        },
    }
}

#[cfg(not(target_family = "wasm"))]
fn bounds_area(bounds: Bounds<DevicePixels>) -> u64 {
    (bounds.size.width.0.max(0) as u64) * (bounds.size.height.0.max(0) as u64)
}

#[cfg(not(target_family = "wasm"))]
fn sync_store_locked(
    gpu: &mut GpuState,
    entries: &mut HashMap<DynamicTextureId, CpuEntry>,
    swizzle_scratch: &mut Vec<u8>,
) {
    let format = gpu.format;
    let limit = gpu.max_texture_size;

    for (&id, entry) in entries.iter_mut() {
        let width = entry.size.width.0 as u32;
        let height = entry.size.height.0 as u32;
        if width == 0 || height == 0 || width > limit || height > limit {
            gpu.store.remove_view(id);
            entry.dirty.clear();
            continue;
        }
        match gpu.store.view(id) {
            Some(view) => {
                let (regions, count) = entry.dirty.take();
                for &bounds in &regions[..count] {
                    write_cpu_subregion(
                        gpu,
                        format,
                        view.texture(),
                        entry.size,
                        bounds,
                        &entry.pixels,
                        swizzle_scratch,
                        false,
                    );
                }
            }
            None => {
                let full_bounds = Bounds {
                    origin: point(DevicePixels(0), DevicePixels(0)),
                    size: entry.size,
                };
                let texture = create_upload_texture(&gpu.device, format, entry.size);
                write_cpu_subregion(
                    gpu,
                    format,
                    &texture,
                    entry.size,
                    full_bounds,
                    &entry.pixels,
                    swizzle_scratch,
                    true,
                );
                entry.dirty.clear();
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                gpu.store.insert_uploaded(id, view);
            }
        }
    }
}

#[cfg(not(target_family = "wasm"))]
struct CheckedRegion {
    origin_x: u32,
    origin_y: u32,
    width: u32,
    height: u32,
    row_bytes: u32,
    byte_len: usize,
}

#[cfg(not(target_family = "wasm"))]
fn update_cpu_pixels(
    pixels: &mut [u8],
    size: Size<DevicePixels>,
    region: &CheckedRegion,
    bytes: &[u8],
) {
    let tex_width = size.width.0 as usize;
    let tex_row_bytes = tex_width * BYTES_PER_PIXEL as usize;
    let origin_x = region.origin_x as usize;
    let origin_y = region.origin_y as usize;
    let width = region.width as usize;
    let height = region.height as usize;
    let row_bytes = region.row_bytes as usize;

    if origin_x == 0 && width == tex_width {
        let dst_start = origin_y * tex_row_bytes;
        pixels[dst_start..dst_start + region.byte_len].copy_from_slice(bytes);
    } else {
        let col_offset = origin_x * BYTES_PER_PIXEL as usize;
        for row in 0..height {
            let src_start = row * row_bytes;
            let dst_start = (origin_y + row) * tex_row_bytes + col_offset;
            pixels[dst_start..dst_start + row_bytes]
                .copy_from_slice(&bytes[src_start..src_start + row_bytes]);
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn ensure_within_device_limit(size: Size<DevicePixels>, limit: u32) -> Result<()> {
    anyhow::ensure!(
        size.width.0 > 0
            && size.height.0 > 0
            && size.width.0 as u32 <= limit
            && size.height.0 as u32 <= limit,
        "dynamic texture size must be positive and within the device's {limit} pixel limit"
    );
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
fn create_upload_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: Size<DevicePixels>,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("dynamic_texture"),
        size: wgpu::Extent3d {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

#[cfg(not(target_family = "wasm"))]
fn check_upload_region(
    size: Size<DevicePixels>,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
) -> Result<CheckedRegion> {
    let (origin_x, origin_y) = (i64::from(bounds.origin.x.0), i64::from(bounds.origin.y.0));
    let (width, height) = (
        i64::from(bounds.size.width.0),
        i64::from(bounds.size.height.0),
    );
    anyhow::ensure!(
        origin_x >= 0
            && origin_y >= 0
            && width > 0
            && height > 0
            && origin_x + width <= i64::from(size.width.0)
            && origin_y + height <= i64::from(size.height.0),
        "dynamic texture update is outside the texture"
    );
    let width = width as u32;
    let height = height as u32;
    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .context("dynamic texture row size overflow")?;
    let byte_len = (row_bytes as usize)
        .checked_mul(height as usize)
        .context("dynamic texture payload size overflow")?;
    anyhow::ensure!(
        bytes.len() >= byte_len,
        "dynamic texture update is shorter than the region"
    );
    Ok(CheckedRegion {
        origin_x: origin_x as u32,
        origin_y: origin_y as u32,
        width,
        height,
        row_bytes,
        byte_len,
    })
}

#[cfg(not(target_family = "wasm"))]
fn write_cpu_subregion(
    gpu: &mut GpuState,
    format: wgpu::TextureFormat,
    texture: &wgpu::Texture,
    tex_size: Size<DevicePixels>,
    bounds: Bounds<DevicePixels>,
    cpu_pixels: &[u8],
    swizzle_scratch: &mut Vec<u8>,
    initial: bool,
) {
    let tex_width = tex_size.width.0 as usize;
    let tex_row_bytes = tex_width * BYTES_PER_PIXEL as usize;
    let origin_x = bounds.origin.x.0 as u32;
    let origin_y = bounds.origin.y.0 as u32;
    let width = bounds.size.width.0 as u32;
    let height = bounds.size.height.0 as u32;
    let rect_row_bytes = width * BYTES_PER_PIXEL;

    let (payload, bytes_per_row) = if format == wgpu::TextureFormat::Rgba8Unorm {
        (
            swizzle_subregion_bgra_to_rgba(
                cpu_pixels,
                tex_row_bytes,
                origin_x as usize,
                origin_y as usize,
                rect_row_bytes as usize,
                height as usize,
                swizzle_scratch,
            ),
            rect_row_bytes,
        )
    } else {
        let start =
            origin_y as usize * tex_row_bytes + origin_x as usize * BYTES_PER_PIXEL as usize;
        let slice_len = (height as usize - 1) * tex_row_bytes + rect_row_bytes as usize;
        (&cpu_pixels[start..start + slice_len], tex_row_bytes as u32)
    };
    let destination = wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin: wgpu::Origin3d {
            x: origin_x,
            y: origin_y,
            z: 0,
        },
        aspect: wgpu::TextureAspect::All,
    };
    let extent = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    if !initial
        && gpu.staging.upload(
            &gpu.device,
            destination,
            extent,
            payload,
            bytes_per_row,
            gpu.max_buffer_size,
        )
    {
        return;
    }
    gpu.queue.write_texture(
        destination,
        payload,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: None,
        },
        extent,
    );
}

#[cfg(not(target_family = "wasm"))]
fn swizzle_subregion_bgra_to_rgba<'a>(
    cpu_pixels: &[u8],
    tex_row_bytes: usize,
    origin_x: usize,
    origin_y: usize,
    rect_row_bytes: usize,
    height: usize,
    scratch: &'a mut Vec<u8>,
) -> &'a [u8] {
    scratch.resize(rect_row_bytes * height, 0);
    let col_offset = origin_x * BYTES_PER_PIXEL as usize;
    for row in 0..height {
        let src_start = (origin_y + row) * tex_row_bytes + col_offset;
        let src_row = &cpu_pixels[src_start..src_start + rect_row_bytes];
        let dst_start = row * rect_row_bytes;
        let dst_row = &mut scratch[dst_start..dst_start + rect_row_bytes];
        for (&[b, g, r, a], dst) in src_row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst_row.as_chunks_mut::<4>().0)
        {
            *dst = [r, g, b, a];
        }
    }
    scratch.as_slice()
}

/// An owned render target that GPUI samples without an upload or transfer copy.
///
/// Allocate it from the context drawing the window. Dropping it frees its
/// entry and retires its sprite id, so later sprite lookups skip it.
/// Notify the view when removing content, as for other view state changes.
/// A different context cannot resolve its id, even on the same adapter.
///
/// Check the creating context's `device_lost()` when handling device loss,
/// even if contents change rarely. Drop its targets promptly, before replacing
/// the context: each owner retains a context clone and its device. Recreate
/// targets from the replacement context and render their contents again.
/// `render` refuses a lost device; `paint` can still succeed for a stale owner,
/// but the replacement context skips its sprite and displays no texture.
pub struct WgpuDynamicTexture {
    context: WgpuContext,
    texture: DynamicTexture,
}

impl WgpuContext {
    /// Allocates a GPU-written dynamic texture on this context's device.
    ///
    /// The view uses `color_texture_format()` and `RENDER_ATTACHMENT |
    /// TEXTURE_BINDING`. There is no initial pixel upload. Render new contents
    /// with `WgpuDynamicTexture::render`, then paint with its `paint` method.
    pub fn create_dynamic_texture(&self, size: Size<DevicePixels>) -> Result<WgpuDynamicTexture> {
        anyhow::ensure!(!self.device_lost(), "dynamic texture device is lost");
        let limit = self.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.width.0 > 0
                && size.height.0 > 0
                && size.width.0 as u32 <= limit
                && size.height.0 as u32 <= limit,
            "dynamic texture size must be positive and within the device's {limit} pixel limit"
        );
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dynamic_texture"),
            size: wgpu::Extent3d {
                width: size.width.0 as u32,
                height: size.height.0 as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.color_texture_format(),
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let texture = DynamicTexture::new_gpu(size)?;
        self.fast_dynamic_textures
            .insert_view(texture.id(), target.create_view(&Default::default()));
        Ok(WgpuDynamicTexture {
            context: self.clone(),
            texture,
        })
    }
}

impl WgpuDynamicTexture {
    /// Writes new contents and refreshes the window only after success.
    ///
    /// Submit the producer's commands on the creating context's queue before
    /// returning. GPUI's later submission samples the same texture; queue
    /// ordering makes a CPU wait or transfer copy unnecessary. Do not submit
    /// partial writes and then return an error. Call only when contents change.
    /// Outside paint, success requests a redraw. During paint it updates the
    /// current frame without scheduling another frame.
    /// Only this window is refreshed; notify other windows displaying the
    /// target so they redraw its new contents too.
    pub fn render<T>(
        &self,
        window: &mut Window,
        render: impl FnOnce(&wgpu::TextureView) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            !self.context.device_lost(),
            "dynamic texture device is lost"
        );
        let view = self
            .context
            .fast_dynamic_textures
            .view(self.texture.id())
            .ok_or_else(|| anyhow::anyhow!("dynamic texture was removed"))?;
        let result = render(&view)?;
        window.refresh_dynamic_texture(self.texture)?;
        Ok(result)
    }

    /// Paints with the window's current clipping, opacity and painter order.
    /// Call during paint in a window using the creating context.
    pub fn paint(&self, window: &mut Window, bounds: Bounds<Pixels>) -> Result<()> {
        window.paint_dynamic_texture(bounds, self.texture)
    }
}

impl Drop for WgpuDynamicTexture {
    fn drop(&mut self) {
        self.context
            .fast_dynamic_textures
            .remove_view(self.texture.id());
        self.texture.remove();
    }
}

#[cfg(all(test, feature = "test-support", not(target_family = "wasm")))]
mod tests;
