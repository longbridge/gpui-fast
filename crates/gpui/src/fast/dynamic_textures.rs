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
//! Renderer crates can also register a GPU-written texture through
//! [`DynamicTexture::new_gpu`], retaining its GPU memory on their own context.
//! GPUI tracks only the size and content version; no GPU object enters the
//! upload platform or the process-wide metadata table.
//!
//! CPU uploads use [`DynamicTexturePlatform`] and
//! [`set_dynamic_texture_platform`]. Platforms without an upload backend
//! report an error from `create_dynamic_texture`. GPU-written handles refuse
//! CPU updates; their renderer records writes through
//! [`crate::Window::refresh_dynamic_texture`].

use anyhow::{Context as _, Result};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

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

/// A copyable handle to a mutable texture, not an owner of its GPU memory.
///
/// Applications obtain uploaded textures from
/// [`crate::Window::create_dynamic_texture`]. Renderer crates can register
/// GPU-written textures with [`Self::new_gpu`] and own their GPU memory.
/// Removing a texture retires every copy of its handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicTexture {
    id: DynamicTextureId,
    size: crate::Size<DevicePixels>,
}

impl DynamicTexture {
    /// Identifies the texture in its renderer's lookup table.
    /// This is not a GPU resource or an ownership token.
    pub fn id(&self) -> DynamicTextureId {
        self.id
    }

    /// Registers the size and version of a GPU-written texture.
    ///
    /// Renderer crates allocate and own its GPU memory on their own context.
    /// Applications should use their renderer's constructor. This method never
    /// selects or calls the process's upload platform.
    pub fn new_gpu(size: crate::Size<DevicePixels>) -> Result<Self> {
        anyhow::ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "dynamic texture dimensions must be positive"
        );
        allocate(size, TextureSource::Gpu, |_| Ok(()))
    }

    /// Releases the texture without requiring a live window, for example
    /// when the producer is dropped. Other copies of the handle become stale.
    /// Releasing an already removed texture has no effect.
    /// Renderer owners must also release GPU memory held outside the upload
    /// platform; this method retires only the metadata of GPU-written textures.
    pub fn remove(self) {
        remove(self);
    }
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
    ///
    /// May fail while the platform still holds earlier staged uploads, for
    /// example when the window has not drawn to drain them. A refused
    /// upload leaves the texture's previous contents in place.
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
    require_platform(PLATFORM.read().clone())
}

/// The backend to use, or the error that says why there is none.
///
/// Split out of [`platform`] so the missing-backend branch can be tested:
/// the global is installed once per process, and a test binary that has
/// already installed a mock can never see it empty again.
fn require_platform(
    platform: Option<Arc<dyn DynamicTexturePlatform>>,
) -> Result<Arc<dyn DynamicTexturePlatform>> {
    platform.context("dynamic textures are not supported on this platform")
}

static NEXT_TEXTURE_ID: AtomicU32 = AtomicU32::new(0);

/// Set the first time the id counter passes the reserved atlas range. The
/// counter is not allowed to wrap: past `u32::MAX` it would count back into
/// ids a live texture may still hold, so once the range runs out no further
/// id is granted for the life of the process.
static IDS_EXHAUSTED: AtomicBool = AtomicBool::new(false);

/// The live dynamic textures, so update and paint can validate their
/// arguments against the size the texture was created with.
static LIVE_TEXTURES: parking_lot::Mutex<Option<HashMap<DynamicTextureId, TextureState>>> =
    parking_lot::Mutex::new(None);

/// A live texture's created size and its content version.
#[derive(Clone, Copy)]
struct TextureState {
    size: crate::Size<DevicePixels>,
    source: TextureSource,
    /// Bumped on every sub-region update. Painted sprites carry it in
    /// [`PolychromeSprite::pad`], which the scroll-layer tile hashes cover,
    /// so tiles showing refreshed contents are re-rendered instead of
    /// kept from the cache.
    version: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextureSource {
    Upload,
    Gpu,
}

fn live_textures() -> parking_lot::MappedMutexGuard<'static, HashMap<DynamicTextureId, TextureState>>
{
    parking_lot::MutexGuard::map(LIVE_TEXTURES.lock(), |textures| {
        textures.get_or_insert_with(HashMap::new)
    })
}

/// The version a sprite painted now must carry: a changing texture version,
/// so a cached frame holding a sprite of a refreshed texture is told apart
/// from the new one.
fn painted_version(id: DynamicTextureId) -> u32 {
    live_textures()
        .get(&id)
        .map_or(1, |state| state.version.max(1))
}

/// Allocates a mutable BGRA8 texture.
///
/// `bytes` holds the initial contents in `bytes_per_row`-strided rows; it is
/// repacked into tight rows before the upload. The payload must hold every
/// row's visible bytes; padding on the final row — what a native framebuffer
/// allocation naturally has — is fine, and anything past the rows the
/// texture needs is ignored.
pub(crate) fn create(
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
        bytes.len() >= required,
        "dynamic texture payload is shorter than its dimensions"
    );

    let packed = pack_rows(bytes, stride, row_bytes, height);
    let backend = platform()?;
    allocate(size, TextureSource::Upload, |id| {
        backend.create_texture(id, size, &packed)
    })
}

/// Grants a texture id, has the backend allocate the texture under it with
/// `create`, and tracks it as live.
fn allocate(
    size: crate::Size<DevicePixels>,
    source: TextureSource,
    create: impl FnOnce(DynamicTextureId) -> Result<()>,
) -> Result<DynamicTexture> {
    let id = DynamicTextureId(NEXT_TEXTURE_ID.fetch_add(1, Ordering::Relaxed));
    // Ids count up for the life of the process into the reserved atlas
    // range, so an id past the range would name a layer tile's texture.
    // Refuse — and keep refusing for good: the counter would eventually
    // wrap around and hand out an id a live texture already holds.
    if IDS_EXHAUSTED.load(Ordering::Relaxed) || id.0 >= DYNAMIC_ID_LIMIT {
        IDS_EXHAUSTED.store(true, Ordering::Relaxed);
        anyhow::bail!("dynamic texture ids are exhausted");
    }
    create(id).with_context(|| format!("failed to create dynamic texture {id:?}"))?;
    live_textures().insert(
        id,
        TextureState {
            size,
            source,
            version: 1,
        },
    );

    Ok(DynamicTexture { id, size })
}

/// Uploads a BGRA8 sub-region into a mutable texture.
///
/// The region is relative to the texture's own origin and must lie inside it,
/// so a stale texture size cannot scribble over other content. The rows are
/// repacked before the upload, and the texture's version advances so cached
/// frames learn about the new contents.
pub(crate) fn update(
    texture: DynamicTexture,
    bounds: Bounds<DevicePixels>,
    bytes: &[u8],
    bytes_per_row: u32,
) -> Result<()> {
    let state = live_textures()
        .get(&texture.id)
        .copied()
        .context("unknown dynamic texture")?;
    anyhow::ensure!(
        state.source == TextureSource::Upload,
        "GPU-written textures must be updated by their producer"
    );
    let packed = validate_region(state.size, bounds, bytes, bytes_per_row)?;

    platform()?
        .update_texture(texture.id, bounds, &packed)
        .with_context(|| format!("failed to update dynamic texture {texture:?}"))?;
    if let Some(state) = live_textures().get_mut(&texture.id) {
        state.version = state.version.wrapping_add(1).max(1);
    }

    Ok(())
}

/// Records that a GPU producer wrote new contents into `texture`: the version
/// the next paint carries advances, as an update's does.
pub(crate) fn mark_written(texture: DynamicTexture) -> Result<()> {
    let mut textures = live_textures();
    let state = textures
        .get_mut(&texture.id)
        .context("unknown dynamic texture")?;
    state.version = state.version.wrapping_add(1).max(1);
    Ok(())
}

/// Paints the current contents of a mutable texture into the scene.
///
/// This should only be called as part of the paint phase of element drawing.
pub(crate) fn paint(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    texture: DynamicTexture,
) -> Result<()> {
    window.invalidator.debug_assert_paint();

    let texture_size = live_size(texture.id)?;

    // Every consumer resolves a sprite of this texture through its atlas
    // texture id — the dynamic range the renderer checks first — and never
    // looks the tile up by id in the atlas, so the tile id here only needs
    // to be stable within the scene; it must never be used to reach the
    // sprite atlas.
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
        pad: painted_version(texture.id),
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
pub(crate) fn remove(texture: DynamicTexture) {
    let state = live_textures().remove(&texture.id);
    if state.is_some_and(|state| state.source == TextureSource::Upload)
        && let Ok(backend) = platform()
    {
        backend.remove_texture(texture.id);
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
    /// scribble over other content. The platform may also refuse the update
    /// while it still holds earlier staged uploads — a window that has not
    /// drawn to drain them — in which case the error is returned, the
    /// texture keeps its previous contents, and the version the next paint
    /// carries does not advance.
    ///
    /// Called outside a frame, this marks the window for a full redraw, so
    /// cached content showing the texture — a retained view that would draw
    /// nothing again, or a scroll-layer tile — re-renders with the new
    /// contents even though no element state changed. Called while a frame is
    /// drawing, the upload lands on the next frame and the window is not
    /// marked: painting cannot schedule a redraw, so an element that streams
    /// from its own paint should ask the window to draw again itself.
    pub fn update_dynamic_texture(
        &mut self,
        texture: DynamicTexture,
        bounds: Bounds<DevicePixels>,
        bytes: &[u8],
        bytes_per_row: u32,
    ) -> Result<()> {
        update(texture, bounds, bytes, bytes_per_row)?;
        self.refresh();
        Ok(())
    }

    /// Records that a GPU producer wrote new contents into `texture`.
    ///
    /// Like [`Window::update_dynamic_texture`], this advances the version the
    /// next paint carries and, called outside a frame, marks the window for a
    /// full redraw, so cached views and scroll-layer tiles showing the
    /// texture render again. A removed texture is refused.
    pub fn refresh_dynamic_texture(&mut self, texture: DynamicTexture) -> Result<()> {
        mark_written(texture)?;
        self.refresh();
        Ok(())
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
        .map(|state| state.size)
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

    // Positive and bounded by the texture's i32 size, so the usize casts
    // cannot truncate or go negative.
    let (width, height) = (width as usize, height as usize);
    let row_bytes = width
        .checked_mul(BYTES_PER_PIXEL)
        .context("dynamic texture region row size overflow")?;
    let stride = usize::try_from(bytes_per_row).context("negative bytes per row")?;
    anyhow::ensure!(
        stride >= row_bytes,
        "dynamic texture stride is smaller than one row"
    );

    let required = checked_payload_len(height, stride, row_bytes)?;
    anyhow::ensure!(
        bytes.len() >= required,
        "dynamic texture update is shorter than the region"
    );

    Ok(pack_rows(bytes, stride, row_bytes, height))
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
        DynamicTexturePlatform, create, live_size, mark_written, painted_version, remove,
        set_dynamic_texture_platform, update, validate_region,
    };
    use crate::{
        AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask, DevicePixels,
        PolychromeSprite, ScaledPixels, Scene, TileId, point, size,
    };
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
        // The backend global is installed once per process, so it cannot be
        // observed empty from a test binary that already installed a mock.
        // Assert the branch directly instead of skipping when a mock is up.
        let error = super::require_platform(None)
            .err()
            .expect("a missing backend is an error");
        assert_eq!(
            error.to_string(),
            "dynamic textures are not supported on this platform"
        );
    }

    #[test]
    fn create_requires_every_row_and_ignores_trailing_bytes() {
        let (_lock, mock) = fresh_mock();

        let short = create(size_of(4, 4), &packed_bytes(4, 4)[..63], 16);
        assert!(short.is_err(), "one byte short must be refused");

        // A native framebuffer allocation pads the final row too: a 4x4
        // BGRA surface with a 16-byte stride is 64 bytes, not 60.
        let _padded = create(size_of(4, 4), &[0u8; 4 * 16], 16).expect("created");
        {
            let created = mock.created.lock();
            assert_eq!(created.last().expect("created").2, 4 * 4 * BYTES_PER_PIXEL);
        }

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
        // The payload is one strided row plus the final row's visible bytes
        // only — a 4x2 texture.
        let stride = (4 * BYTES_PER_PIXEL + 2) as u32;
        let mut strided = vec![0u8; stride as usize + 4 * BYTES_PER_PIXEL];
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
        let version = painted_version(texture.id);
        let result = update(texture, bounds_at((0, 0), 2, 2), &packed_bytes(2, 2), 8);
        assert!(result.is_err(), "the backend's error must propagate");
        assert!(
            live_size(texture.id).is_ok(),
            "the texture stays live for the next attempt"
        );
        assert_eq!(
            painted_version(texture.id),
            version,
            "a refused update must not advance the version the next paint carries"
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

    /// A scene holding the texture painted over `region`'s tiles, the way
    /// [`paint`] would build it: the version the texture carries at paint
    /// time rides in the sprite's pad.
    fn scene_with_texture(texture: DynamicTexture, pad: u32) -> Scene {
        let mut scene = Scene::default();
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad,
            tile: AtlasTile {
                texture_id: texture.id.atlas_texture_id(),
                tile_id: TileId(texture.id.0),
                padding: 0,
                bounds: Bounds {
                    origin: point(DevicePixels(0), DevicePixels(0)),
                    size: texture.size,
                },
            },
            grayscale: false.into(),
            opacity: 1.,
            bounds: Bounds {
                origin: point(ScaledPixels(0.), ScaledPixels(0.)),
                size: size(ScaledPixels(64.), ScaledPixels(64.)),
            },
            content_mask: ContentMask {
                bounds: Bounds {
                    origin: point(ScaledPixels(0.), ScaledPixels(0.)),
                    size: size(ScaledPixels(128.), ScaledPixels(128.)),
                },
            },
            corner_radii: Default::default(),
        });
        scene
    }

    #[test]
    fn repainting_an_updated_texture_dirties_its_tiles() {
        let (_lock, _mock) = fresh_mock();

        let texture = create(size_of(4, 4), &packed_bytes(4, 4), 16).expect("created");
        let region = Bounds {
            origin: point(ScaledPixels(0.), ScaledPixels(0.)),
            size: size(ScaledPixels(128.), ScaledPixels(128.)),
        };

        let before = crate::fast::layers::tiles::tile_hashes(
            &scene_with_texture(texture, painted_version(texture.id)),
            64,
            region,
        );

        // An in-place update at unchanged sprite bounds changes no hashed
        // field of the sprite but the version the next paint carries.
        update(texture, bounds_at((0, 0), 2, 2), &packed_bytes(2, 2), 8).expect("updated");

        let after = crate::fast::layers::tiles::tile_hashes(
            &scene_with_texture(texture, painted_version(texture.id)),
            64,
            region,
        );
        assert_ne!(before, after, "tiles showing the texture must be dirtied");

        // A texture that was never updated repaints with the same hash, so
        // its tiles are still kept.
        let other = create(size_of(4, 4), &packed_bytes(4, 4), 16).expect("created");
        let unchanged = crate::fast::layers::tiles::tile_hashes(
            &scene_with_texture(other, painted_version(other.id)),
            64,
            region,
        );
        assert_eq!(
            crate::fast::layers::tiles::tile_hashes(
                &scene_with_texture(other, painted_version(other.id)),
                64,
                region,
            ),
            unchanged,
        );
    }

    #[test]
    fn gpu_metadata_never_calls_the_upload_platform() {
        let (_lock, mock) = fresh_mock();
        assert!(DynamicTexture::new_gpu(size_of(0, 4)).is_err());
        let texture = DynamicTexture::new_gpu(size_of(4, 4)).expect("GPU metadata");
        assert!(mock.created.lock().is_empty());
        let version = painted_version(texture.id);
        mark_written(texture).expect("recorded GPU write");
        assert_ne!(painted_version(texture.id), version);
        assert!(update(texture, bounds_at((0, 0), 4, 4), &packed_bytes(4, 4), 16).is_err());
        texture.remove();
        assert!(mock.updated.lock().is_empty());
        assert!(mock.removed.lock().is_empty());
        assert!(mark_written(texture).is_err());
    }
}
