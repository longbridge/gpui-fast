//! Mutable BGRA8 textures for content that changes every frame.
//!
//! A texture allocated through [`crate::Window::create_dynamic_texture`] is
//! owned by the platform renderer, not by the sprite atlas: it lives outside
//! the atlas allocator, so repainting a frame into it never invalidates the
//! glyphs and icons sharing the atlas. Callers paint the current contents
//! with [`crate::Window::paint_dynamic_texture`] and refresh sub-regions with
//! [`crate::Window::update_dynamic_texture`]; the upload goes straight into
//! the existing GPU texture instead of rebuilding an image.
//!
//! The GPU work itself is platform-specific: a backend implements
//! [`DynamicTexturePlatform`] and installs itself with
//! [`set_dynamic_texture_platform`]. Platforms without a backend report an
//! error from `create_dynamic_texture` instead of failing later, and the
//! sprite atlas keeps its meaning as a cache of immutable content.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Context as _, Result};

use crate::fast::layers::scene::LAYER_TILE_TEXTURE_BASE;
use crate::{
    AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, DevicePixels, Pixels, PolychromeSprite,
    TileId, Window, point,
};

/// Bytes per pixel of a dynamic texture: BGRA8, matching the scene's
/// polychrome sprite format.
const BYTES_PER_PIXEL: usize = 4;

/// Atlas texture indices at or above this base belong to dynamic textures.
///
/// The sprite atlas numbers its textures from small, densely reused indices,
/// and layer tiles reserve the range at [`LAYER_TILE_TEXTURE_BASE`], so a
/// reserved range below that keeps the three id spaces disjoint without
/// touching `AtlasTextureKind`, which is upstream's type.
///
/// [`LAYER_TILE_TEXTURE_BASE`]: crate::fast::layers::scene::LAYER_TILE_TEXTURE_BASE
const DYNAMIC_INDEX_BASE: u32 = 0xE000_0000;

/// How many dynamic texture ids the reserved atlas index range holds.
const DYNAMIC_ID_LIMIT: u32 = LAYER_TILE_TEXTURE_BASE - DYNAMIC_INDEX_BASE;

/// Identifies one dynamic texture for its lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DynamicTextureId(u32);

impl DynamicTextureId {
    /// The atlas texture id a sprite of this dynamic texture carries. The
    /// renderer resolves these ids through the dynamic texture backend before
    /// it falls back to the sprite atlas.
    pub fn atlas_texture_id(self) -> AtlasTextureId {
        AtlasTextureId {
            index: DYNAMIC_INDEX_BASE.wrapping_add(self.0),
            kind: AtlasTextureKind::Polychrome,
        }
    }

    /// The dynamic texture id behind an atlas texture id, if it is one.
    pub fn from_atlas_texture_id(id: AtlasTextureId) -> Option<Self> {
        (id.kind == AtlasTextureKind::Polychrome
            && id.index >= DYNAMIC_INDEX_BASE
            && id.index - DYNAMIC_INDEX_BASE < DYNAMIC_ID_LIMIT)
            .then(|| Self(id.index - DYNAMIC_INDEX_BASE))
    }
}

/// A handle to a mutable texture created with
/// [`crate::Window::create_dynamic_texture`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicTexture {
    /// The identifier to pass back to the update, paint and remove methods.
    pub id: DynamicTextureId,
    /// The size the texture was created with, in device pixels.
    pub size: crate::Size<DevicePixels>,
}

/// The GPU side of dynamic textures, implemented by the platform crates that
/// can upload into an existing texture.
///
/// Every method receives tightly packed BGRA8 rows — the caller repacks
/// strided input, so the backend never sees row padding.
pub trait DynamicTexturePlatform: Send + Sync {
    /// Allocates a texture of `size` for `id` and uploads `bytes` into it.
    fn create_texture(
        &self,
        id: DynamicTextureId,
        size: crate::Size<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()>;

    /// Uploads `bytes` for `bounds` — relative to the texture's own origin —
    /// into the texture allocated for `id`.
    fn update_texture(
        &self,
        id: DynamicTextureId,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
    ) -> Result<()>;

    /// Releases the texture allocated for `id`.
    fn remove_texture(&self, id: DynamicTextureId);
}

static PLATFORM: parking_lot::RwLock<Option<Arc<dyn DynamicTexturePlatform>>> =
    parking_lot::RwLock::new(None);

/// Installs the platform backend for dynamic textures.
///
/// Called once by the platform's renderer while it is coming up. The first
/// backend wins: every renderer in the process selects the same Metal device,
/// and textures must stay on the device that draws them.
pub fn set_dynamic_texture_platform(backend: Arc<dyn DynamicTexturePlatform>) {
    let mut platform = PLATFORM.write();
    if platform.is_none() {
        *platform = Some(backend);
    }
}

fn platform() -> Result<Arc<dyn DynamicTexturePlatform>> {
    PLATFORM
        .read()
        .clone()
        .context("dynamic textures are not supported on this platform")
}

static NEXT_TEXTURE_ID: AtomicU32 = AtomicU32::new(0);

/// The live dynamic textures, so update and paint can validate their
/// arguments against the size the texture was created with.
static LIVE_TEXTURES: parking_lot::Mutex<
    Option<HashMap<DynamicTextureId, crate::Size<DevicePixels>>>,
> = parking_lot::Mutex::new(None);

fn live_textures()
-> parking_lot::MappedMutexGuard<'static, HashMap<DynamicTextureId, crate::Size<DevicePixels>>> {
    parking_lot::MutexGuard::map(LIVE_TEXTURES.lock(), |textures| {
        textures.get_or_insert_with(HashMap::new)
    })
}

/// The paint epoch, bumped on every sub-region update so a cached frame
/// holding a sprite of a refreshed texture is told apart from the new one.
static PAINT_EPOCH: AtomicU32 = AtomicU32::new(0);

fn bump_paint_epoch() -> u32 {
    PAINT_EPOCH
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1)
}

/// Allocates a mutable BGRA8 texture.
///
/// `bytes` holds the initial contents in `bytes_per_row`-strided rows; it is
/// repacked into tight rows before the upload. The payload must match the
/// dimensions exactly.
pub fn create(
    size: crate::Size<DevicePixels>,
    bytes: &[u8],
    bytes_per_row: u32,
) -> Result<DynamicTexture> {
    let width = usize::try_from(size.width.0).context("negative texture width")?;
    let height = usize::try_from(size.height.0).context("negative texture height")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "dynamic texture dimensions must be positive"
    );

    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .context("dynamic texture row size overflow")?;
    let stride = usize::try_from(bytes_per_row).context("negative bytes per row")?;
    anyhow::ensure!(
        stride >= row_bytes,
        "dynamic texture stride is smaller than one row"
    );

    let required = checked_payload_len(height, stride, row_bytes)?;
    anyhow::ensure!(
        bytes.len() == required,
        "dynamic texture payload length does not match its dimensions"
    );

    let packed = pack_rows(bytes, stride, row_bytes, height);
    let backend = platform()?;
    let id = DynamicTextureId(NEXT_TEXTURE_ID.fetch_add(1, Ordering::Relaxed));
    backend
        .create_texture(id, size, &packed)
        .with_context(|| format!("failed to create dynamic texture {id:?}"))?;
    live_textures().insert(id, size);

    Ok(DynamicTexture { id, size })
}

/// Uploads a BGRA8 sub-region into a mutable texture.
///
/// The region is relative to the texture's own origin and must lie inside it,
/// so a stale texture size cannot scribble over other content. The rows are
/// repacked before the upload, and the paint epoch advances so cached frames
/// learn about the new contents.
pub fn update(
    texture: DynamicTexture,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
    bytes_per_row: u32,
) -> Result<()> {
    let texture_size = live_size(texture.id)?;
    let packed = validate_region(texture_size, bounds, bytes, bytes_per_row)?;

    platform()?
        .update_texture(texture.id, bounds, &packed)
        .with_context(|| format!("failed to update dynamic texture {texture:?}"))?;
    bump_paint_epoch();

    Ok(())
}

/// Paints the current contents of a mutable texture into the scene.
///
/// This should only be called as part of the paint phase of element drawing.
pub fn paint(window: &mut Window, bounds: Bounds<Pixels>, texture: DynamicTexture) -> Result<()> {
    window.invalidator.debug_assert_paint();

    let texture_size = live_size(texture.id)?;

    let tile = AtlasTile {
        texture_id: texture.id.atlas_texture_id(),
        tile_id: TileId(texture.id.0),
        padding: 0,
        bounds: Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: texture_size,
        },
    };
    window.next_frame.scene.insert_primitive(PolychromeSprite {
        order: 0,
        pad: PAINT_EPOCH.load(Ordering::Relaxed).max(1),
        grayscale: false.into(),
        bounds: window.snap_bounds(bounds),
        content_mask: window.snapped_content_mask(),
        corner_radii: Default::default(),
        tile,
        opacity: window.element_opacity(),
    });

    Ok(())
}

/// Releases a mutable texture and its GPU memory.
pub fn remove(texture: DynamicTexture) {
    if live_textures().remove(&texture.id).is_some() {
        if let Some(backend) = PLATFORM.read().clone() {
            backend.remove_texture(texture.id);
        }
    }
}

impl Window {
    /// Allocates a mutable BGRA8 texture for content that changes every
    /// frame, such as a stream of video or remote desktop frames.
    ///
    /// `bytes_per_row` may include padding past the visible width, which is
    /// what surfaces backed by a native framebuffer hand over. The texture
    /// lives outside the sprite atlas, so refreshing it never reallocates the
    /// tiles glyphs and icons share. Subsequent sub-region writes go through
    /// [`Window::update_dynamic_texture`]; platforms without a dynamic
    /// texture backend report an error here.
    pub fn create_dynamic_texture(
        &mut self,
        size: crate::Size<DevicePixels>,
        bytes: &[u8],
        bytes_per_row: u32,
    ) -> Result<DynamicTexture> {
        create(size, bytes, bytes_per_row)
    }

    /// Uploads a BGRA8 sub-region into a mutable texture.
    ///
    /// The region is relative to the texture's own origin and must lie inside
    /// it; both requirements are checked, so a stale texture size cannot
    /// scribble over other content.
    pub fn update_dynamic_texture(
        &mut self,
        texture: DynamicTexture,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
        bytes_per_row: u32,
    ) -> Result<()> {
        update(texture, bounds, bytes, bytes_per_row)
    }

    /// Paints the current contents of a mutable texture into the scene.
    ///
    /// This method should only be called as part of the paint phase of
    /// element drawing.
    pub fn paint_dynamic_texture(
        &mut self,
        bounds: Bounds<Pixels>,
        texture: DynamicTexture,
    ) -> Result<()> {
        paint(self, bounds, texture)
    }

    /// Releases a mutable texture and its GPU memory.
    pub fn remove_dynamic_texture(&mut self, texture: DynamicTexture) {
        remove(texture);
    }
}

/// The size a live dynamic texture was created with.
fn live_size(id: DynamicTextureId) -> Result<crate::Size<DevicePixels>> {
    live_textures()
        .get(&id)
        .copied()
        .context("unknown dynamic texture")
}

/// Validates a sub-region against the texture's size and returns its rows
/// tightly packed.
fn validate_region(
    texture_size: crate::Size<DevicePixels>,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
    bytes_per_row: u32,
) -> Result<Vec<u8>> {
    // i64 arithmetic: the caller's bounds are untrusted, and the i32 sums of
    // origin and size must not wrap before they are compared.
    let (origin_x, origin_y) = (bounds.origin.x.0 as i64, bounds.origin.y.0 as i64);
    let (width, height) = (bounds.size.width.0 as i64, bounds.size.height.0 as i64);
    anyhow::ensure!(
        origin_x >= 0
            && origin_y >= 0
            && width > 0
            && height > 0
            && origin_x + width <= texture_size.width.0 as i64
            && origin_y + height <= texture_size.height.0 as i64,
        "dynamic texture update is outside the texture"
    );

    let row_bytes = width as usize * BYTES_PER_PIXEL;
    let stride = usize::try_from(bytes_per_row).context("negative bytes per row")?;
    anyhow::ensure!(
        stride >= row_bytes,
        "dynamic texture stride is smaller than one row"
    );

    let required = checked_payload_len(height as usize, stride, row_bytes)?;
    anyhow::ensure!(
        bytes.len() >= required,
        "dynamic texture update is shorter than the region"
    );

    Ok(pack_rows(bytes, stride, row_bytes, height as usize))
}

/// The exact byte length a `height`-row payload needs with `stride` bytes per
/// row and `row_bytes` visible per row.
fn checked_payload_len(height: usize, stride: usize, row_bytes: usize) -> Result<usize> {
    (height - 1)
        .checked_mul(stride)
        .and_then(|length| length.checked_add(row_bytes))
        .context("dynamic texture payload size overflow")
}

/// Repacks strided rows into tightly packed rows.
fn pack_rows(bytes: &[u8], stride: usize, row_bytes: usize, height: usize) -> Vec<u8> {
    let mut packed = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        let start = row * stride;
        packed.extend_from_slice(&bytes[start..start + row_bytes]);
    }
    packed
}

#[cfg(test)]
mod tests {
    use super::{
        BYTES_PER_PIXEL, DYNAMIC_INDEX_BASE, DynamicTexture, DynamicTextureId,
        DynamicTexturePlatform, PLATFORM, create, live_size, remove, set_dynamic_texture_platform,
        update, validate_region,
    };
    use crate::{AtlasTextureId, AtlasTextureKind, Bounds, DevicePixels, point, size};
    use anyhow::Result;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    /// Records the platform calls a test makes, standing in for a GPU.
    ///
    /// The backend global is process-wide and installed once, so every test
    /// shares this one instance and resets the call records it cares about.
    struct MockPlatform {
        created: parking_lot::Mutex<Vec<(DynamicTextureId, crate::Size<DevicePixels>, usize)>>,
        updated: parking_lot::Mutex<Vec<(DynamicTextureId, Bounds<DevicePixels>, usize)>>,
        removed: parking_lot::Mutex<Vec<DynamicTextureId>>,
        fail_updates: AtomicBool,
    }

    impl DynamicTexturePlatform for MockPlatform {
        fn create_texture(
            &self,
            id: DynamicTextureId,
            size: crate::Size<DevicePixels>,
            bytes: &[u8],
        ) -> Result<()> {
            self.created.lock().push((id, size, bytes.len()));
            Ok(())
        }

        fn update_texture(
            &self,
            id: DynamicTextureId,
            bounds: Bounds<DevicePixels>,
            bytes: &[u8],
        ) -> Result<()> {
            if self.fail_updates.load(Ordering::SeqCst) {
                anyhow::bail!("backend refused the update");
            }
            self.updated.lock().push((id, bounds, bytes.len()));
            Ok(())
        }

        fn remove_texture(&self, id: DynamicTextureId) {
            self.removed.lock().push(id);
        }
    }

    /// Serializes the tests that share the process-wide backend.
    static MOCK_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// The process-wide mock backend, installed on first use.
    fn shared_mock() -> &'static Arc<MockPlatform> {
        static MOCK: std::sync::OnceLock<Arc<MockPlatform>> = std::sync::OnceLock::new();
        MOCK.get_or_init(|| {
            let mock = Arc::new(MockPlatform {
                created: parking_lot::Mutex::new(Vec::new()),
                updated: parking_lot::Mutex::new(Vec::new()),
                removed: parking_lot::Mutex::new(Vec::new()),
                fail_updates: AtomicBool::new(false),
            });
            set_dynamic_texture_platform(mock.clone());
            mock
        })
    }

    /// Takes the test lock and resets the mock's records and flags.
    fn fresh_mock() -> (parking_lot::MutexGuard<'static, ()>, Arc<MockPlatform>) {
        let lock = MOCK_LOCK.lock();
        let mock = shared_mock();
        mock.created.lock().clear();
        mock.updated.lock().clear();
        mock.removed.lock().clear();
        mock.fail_updates.store(false, Ordering::SeqCst);
        (lock, mock.clone())
    }

    fn size_of(width: i32, height: i32) -> crate::Size<DevicePixels> {
        size(DevicePixels(width), DevicePixels(height))
    }

    fn bounds_at(origin: (i32, i32), width: i32, height: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(origin.0), DevicePixels(origin.1)),
            size: size(DevicePixels(width), DevicePixels(height)),
        }
    }

    /// Enough zeroed bytes for a tightly packed texture of `width x height`.
    fn packed_bytes(width: i32, height: i32) -> Vec<u8> {
        vec![0u8; width as usize * height as usize * BYTES_PER_PIXEL]
    }

    #[test]
    fn create_reports_missing_platform() {
        // Another test may have installed a mock already; the expectation
        // only holds while no backend is installed.
        if PLATFORM.read().is_none() {
            let result = create(size_of(4, 4), &packed_bytes(4, 4), 16);
            assert_eq!(
                result.unwrap_err().to_string(),
                "dynamic textures are not supported on this platform"
            );
        }
    }

    #[test]
    fn create_rejects_a_payload_that_does_not_match() {
        let (_lock, _mock) = fresh_mock();

        let short = create(size_of(4, 4), &packed_bytes(4, 4)[..63], 16);
        assert!(short.is_err(), "one byte short must be refused");

        let long = create(size_of(4, 4), &packed_bytes(4, 5), 16);
        assert!(long.is_err(), "extra bytes must be refused");

        let empty = create(size_of(4, 4), &[], 16);
        assert!(empty.is_err());

        let zero = create(size_of(0, 4), &[], 0);
        assert!(zero.is_err(), "zero width must be refused");
    }

    #[test]
    fn create_rejects_a_stride_smaller_than_a_row() {
        let (_lock, _mock) = fresh_mock();

        let result = create(
            size_of(4, 4),
            &packed_bytes(4, 4),
            3 * BYTES_PER_PIXEL as u32,
        );
        assert!(result.is_err());
    }

    #[test]
    fn create_packs_rows_and_allocates_distinct_ids() {
        let (_lock, mock) = fresh_mock();

        // Two bytes of padding per row that the backend must not receive.
        // The payload is (height - 1) strided rows plus the final row's
        // visible bytes only.
        let stride = (4 * BYTES_PER_PIXEL + 2) as u32;
        let mut strided = vec![0u8; (2 - 1) * stride as usize + 4 * BYTES_PER_PIXEL];
        for byte in strided.iter_mut().step_by(stride as usize) {
            *byte = 0xFF;
        }

        let texture = create(size_of(4, 2), &strided, stride).expect("created");
        {
            let created = mock.created.lock();
            let (created_id, created_size, packed_len) =
                created.last().expect("create reached the platform");
            assert_eq!(*created_id, texture.id);
            assert_eq!(*created_size, texture.size);
            assert_eq!(
                *packed_len,
                4 * 2 * BYTES_PER_PIXEL,
                "rows must arrive tightly packed"
            );
        }

        // The second create runs with the records released: the mock's
        // mutexes are not reentrant.
        let other = create(size_of(4, 4), &packed_bytes(4, 4), 16).expect("created again");
        assert_ne!(texture.id, other.id, "ids must not repeat");
    }

    #[test]
    fn update_validates_the_region_against_the_created_size() {
        let (_lock, mock) = fresh_mock();

        let texture_size = size_of(8, 8);
        let texture = create(texture_size, &packed_bytes(8, 8), 32).expect("created");

        let inside = bounds_at((0, 0), 4, 4);
        let packed = validate_region(texture_size, inside, &packed_bytes(4, 4), 16)
            .expect("a region inside the texture is accepted");
        assert_eq!(packed.len(), 4 * 4 * BYTES_PER_PIXEL);

        let offset = bounds_at((4, 4), 4, 4);
        assert!(validate_region(texture_size, offset, &packed_bytes(4, 4), 16).is_ok());

        let too_wide = bounds_at((0, 0), 9, 4);
        assert!(
            validate_region(texture_size, too_wide, &packed_bytes(9, 4), 36).is_err(),
            "a region past the texture must be refused"
        );

        let negative = bounds_at((-1, 0), 4, 4);
        assert!(validate_region(texture_size, negative, &packed_bytes(4, 4), 16).is_err());

        let empty = bounds_at((0, 0), 0, 4);
        assert!(validate_region(texture_size, empty, &[], 0).is_err());

        let unknown = DynamicTexture {
            id: DynamicTextureId(u32::MAX),
            size: texture_size,
        };
        assert!(
            live_size(unknown.id).is_err(),
            "an unknown id must be refused"
        );

        remove(texture);
        assert!(
            live_size(texture.id).is_err(),
            "a removed texture must be refused"
        );
        assert_eq!(mock.created.lock().len(), 1);
        assert_eq!(mock.updated.lock().len(), 0);
        assert_eq!(mock.removed.lock().len(), 1);
    }

    #[test]
    fn update_requires_a_full_final_row() {
        let (_lock, _mock) = fresh_mock();

        let texture_size = size_of(8, 8);
        let bounds = bounds_at((0, 0), 4, 4);
        let row_bytes = 4 * BYTES_PER_PIXEL;

        let short_by_one = &packed_bytes(4, 4)[..4 * row_bytes - 1];
        assert!(
            validate_region(texture_size, bounds, short_by_one, row_bytes as u32).is_err(),
            "a final row missing bytes must be refused"
        );
    }

    #[test]
    fn remove_releases_the_texture_once() {
        let (_lock, mock) = fresh_mock();

        let texture = create(size_of(4, 4), &packed_bytes(4, 4), 16).expect("created");
        remove(texture);
        remove(texture);

        assert_eq!(
            mock.removed.lock().len(),
            1,
            "the platform sees one removal"
        );
    }

    #[test]
    fn a_failing_backend_leaves_no_stale_state() {
        let (_lock, mock) = fresh_mock();
        mock.fail_updates.store(true, Ordering::SeqCst);

        let texture = create(size_of(4, 4), &packed_bytes(4, 4), 16).expect("created");
        let result = update(texture, bounds_at((0, 0), 2, 2), &packed_bytes(2, 2), 8);
        assert!(result.is_err(), "the backend's error must propagate");
        assert!(
            live_size(texture.id).is_ok(),
            "the texture stays live for the next attempt"
        );
    }

    #[test]
    fn atlas_ids_stay_out_of_the_atlas_range() {
        let id = DynamicTextureId(0);
        let atlas_id = id.atlas_texture_id();
        assert_eq!(
            DynamicTextureId::from_atlas_texture_id(atlas_id),
            Some(id),
            "the mapping must round-trip"
        );

        let atlas_id = AtlasTextureId {
            index: 7,
            kind: AtlasTextureKind::Polychrome,
        };
        assert_eq!(DynamicTextureId::from_atlas_texture_id(atlas_id), None);

        let atlas_id = AtlasTextureId {
            index: DYNAMIC_INDEX_BASE,
            kind: AtlasTextureKind::Monochrome,
        };
        assert_eq!(
            DynamicTextureId::from_atlas_texture_id(atlas_id),
            None,
            "a dynamic range index in another kind must not match"
        );
    }
}
