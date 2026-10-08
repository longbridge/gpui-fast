//! A CPU rasterizer for finished scenes that reproduces `shaders.wgsl`.
//!
//! [`draw`] draws a scene the way `fast::frame` records it for the GPU: the
//! batches of `Scene::batches` in order, each primitive's fragments being the
//! pixels whose centers its geometry covers, shaded by a port of the
//! primitive's fragment shader in `f32`, and blended into the 8-bit frame as
//! the pipeline's blend state blends them, rounding after every primitive.
//!
//! - `shade`: the shaders' shared functions (colors, gradients, distance
//!   fields, contrast and gamma correction) and the blend states.
//! - `plan`: the scene in a form the drawing threads can share, with the
//!   scroll layer tiles it composites.
//! - `prims`: quads, shadows, underlines and sprites.
//! - `paths`: path batches, rasterized with 4× multisampling into an
//!   intermediate and composited, as the GPU renderer draws them.
//!
//! Pixels are drawn inside regions only, each region cleared first; large
//! regions are drawn in horizontal bands on several threads, each band on
//! its own: the pixels drawn do not depend on how a region is split.

use gpui::{AtlasTextureId, Bounds, DevicePixels, Scene};

mod paths;
mod plan;
mod prims;
mod shade;

#[cfg(test)]
mod tests;

/// A frame in memory: premultiplied `0xAARRGGBB` pixels, row by row.
#[derive(Default)]
pub(crate) struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Canvas {
    /// A canvas of `width` × `height` transparent pixels.
    pub(crate) fn new(width: u32, height: u32) -> Self {
        Canvas {
            width,
            height,
            pixels: vec![0; width as usize * height as usize],
        }
    }

    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn pixels(&self) -> &[u32] {
        &self.pixels
    }
}

/// What the GPU renderer draws with that the shaders read besides the scene.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RasterParams {
    pub(crate) gamma_ratios: [f32; 4],
    pub(crate) grayscale_enhanced_contrast: f32,
    pub(crate) subpixel_enhanced_contrast: f32,
    pub(crate) is_bgr: bool,
    /// Whether the target composites premultiplied (`GlobalParams::premultiplied_alpha`).
    pub(crate) premultiplied_alpha: bool,
    /// Whether subpixel sprites are drawn with dual-source blending, as the
    /// GPU renderer does when the adapter supports it.
    pub(crate) dual_source_blending: bool,
    /// The samples per pixel paths are rasterized with. Only 4 is
    /// reproduced; other counts make scenes with paths need the GPU.
    pub(crate) path_sample_count: u32,
    /// The bits of fixed point the GPU truncates a fragment's channels to
    /// before rounding them to the target's levels ([`fragment_bits`]).
    pub(crate) fragment_bits: u32,
}

/// The bits of fixed point a GPU of PCI vendor `vendor` truncates a
/// fragment's channels to before rounding them to 8-bit levels: 12 on
/// NVIDIA, 16 on Intel (both measured, see `shade::fragment_level`). Others
/// are taken as 16, the nearer of the two to rounding exactly.
// The web never draws on the CPU.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub(crate) fn fragment_bits(vendor: u32) -> u32 {
    const NVIDIA: u32 = 0x10de;
    if vendor == NVIDIA { 12 } else { 16 }
}

/// The pixels of an atlas texture, as they were uploaded.
pub(crate) struct TexturePixels<'a> {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// 1 for monochrome textures (`R8`), 4 for color textures (`BGRA8`).
    pub(crate) bytes_per_pixel: u32,
    pub(crate) data: &'a [u8],
}

/// Where sprites' pixels come from.
pub(crate) trait SpritePixels: Sync {
    fn texture(&self, id: AtlasTextureId) -> Option<TexturePixels<'_>>;
}

/// Why a scene must be drawn on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeedsGpu {
    /// The scene has surfaces (video frames), which only the GPU samples.
    Surfaces,
    /// The scene has paths, and the GPU rasterizes them with another sample
    /// count than the 4 the CPU reproduces.
    PathSampling,
}

/// Whether the CPU can draw `scene` inside `regions` as the GPU would with
/// `params`.
pub(crate) fn can_draw(
    scene: &Scene,
    _regions: &[Bounds<DevicePixels>],
    params: &RasterParams,
) -> Result<(), NeedsGpu> {
    if !scene.surfaces.is_empty() {
        Err(NeedsGpu::Surfaces)
    } else if !scene.paths.is_empty() && params.path_sample_count != 4 {
        Err(NeedsGpu::PathSampling)
    } else {
        Ok(())
    }
}

/// Regions with fewer pixels than this are drawn on the calling thread.
const MIN_THREADED_PIXELS: i64 = 96 * 1024;

/// The fewest rows a band drawn on a thread of its own gets.
const MIN_BAND_ROWS: i32 = 16;

/// Bands per thread: more bands than threads even out the work when the
/// scene is busier in some bands than in others.
const BANDS_PER_THREAD: i32 = 3;

/// Draws `scene` into `canvas` inside `regions` only, as the GPU renderer
/// draws it into a target cleared to transparent black: each region is
/// cleared, then every primitive that reaches it is drawn, clipped to it.
/// Large regions are drawn on up to `threads` threads.
pub(crate) fn draw(
    canvas: &mut Canvas,
    scene: &Scene,
    regions: &[Bounds<DevicePixels>],
    sprites: &dyn SpritePixels,
    params: &RasterParams,
    threads: usize,
) {
    let canvas_rect = IRect::new(0, 0, canvas.width as i32, canvas.height as i32);
    let regions: Vec<IRect> = regions
        .iter()
        .map(|region| IRect::from_bounds(region).intersect(&canvas_rect))
        .filter(|region| !region.is_empty())
        .collect();
    if regions.is_empty() {
        return;
    }
    shade::set_fragment_bits(params.fragment_bits);

    let tile_scenes = plan::tile_scenes(scene, &regions);
    let tiles = plan::tile_plans(scene, &tile_scenes);
    let plan = plan::Plan::new(scene, &tiles);
    let ctx = Ctx {
        params,
        sprites,
        tiles: &tiles,
    };

    let stride = canvas.width as usize;
    for region in &regions {
        draw_region(&mut canvas.pixels, stride, *region, &plan, &ctx, threads);
    }
}

/// Clears `region` of the `stride`-wide `pixels` and draws `plan` in it,
/// in bands on up to `threads` threads when it is large.
fn draw_region(
    pixels: &mut [u32],
    stride: usize,
    region: IRect,
    plan: &plan::Plan,
    ctx: &Ctx,
    threads: usize,
) {
    let rows = region.y1 - region.y0;
    let band_count = if threads <= 1 || region.area() < MIN_THREADED_PIXELS {
        1
    } else {
        (threads as i32 * BANDS_PER_THREAD)
            .min(rows / MIN_BAND_ROWS)
            .max(1)
    };

    let region_rows = &mut pixels[region.y0 as usize * stride..region.y1 as usize * stride];
    if band_count == 1 {
        let mut target = Target {
            pixels: region_rows,
            stride,
            origin_x: 0,
            origin_y: region.y0,
            clip: region,
        };
        target.fill(0);
        plan::draw_plan(plan, ctx, &mut target);
        return;
    }

    // Split the rows into bands of nearly equal height, dealt to the threads
    // in turn.
    let threads = threads.min(band_count as usize);
    let mut per_thread: Vec<Vec<Target>> = (0..threads).map(|_| Vec::new()).collect();
    let mut rest = region_rows;
    let mut y = region.y0;
    for band in 0..band_count {
        let band_rows = (rows * (band + 1)) / band_count - (rows * band) / band_count;
        let (band_pixels, tail) = rest.split_at_mut(band_rows as usize * stride);
        rest = tail;
        per_thread[band as usize % threads].push(Target {
            pixels: band_pixels,
            stride,
            origin_x: 0,
            origin_y: y,
            clip: IRect::new(region.x0, y, region.x1, y + band_rows),
        });
        y += band_rows;
    }

    std::thread::scope(|scope| {
        let mut per_thread = per_thread.into_iter();
        let own = per_thread.next();
        for bands in per_thread {
            scope.spawn(move || {
                for mut target in bands {
                    target.fill(0);
                    plan::draw_plan(plan, ctx, &mut target);
                }
            });
        }
        for mut target in own.into_iter().flatten() {
            target.fill(0);
            plan::draw_plan(plan, ctx, &mut target);
        }
    });
}

/// What every primitive is drawn with besides the scene.
struct Ctx<'a> {
    params: &'a RasterParams,
    sprites: &'a dyn SpritePixels,
    /// The scroll layer tiles the scene composites that reach the regions.
    tiles: &'a [plan::TilePlan<'a>],
}

/// A rectangle of whole pixels, `x0..x1` by `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IRect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl IRect {
    const EMPTY: IRect = IRect {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
    };

    fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        IRect { x0, y0, x1, y1 }
    }

    fn from_bounds(bounds: &Bounds<DevicePixels>) -> Self {
        let x0 = bounds.origin.x.0;
        let y0 = bounds.origin.y.0;
        IRect::new(
            x0,
            y0,
            x0.saturating_add(bounds.size.width.0),
            y0.saturating_add(bounds.size.height.0),
        )
    }

    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
        }
    }

    fn intersect(&self, other: &IRect) -> IRect {
        IRect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    fn union(&self, other: &IRect) -> IRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        IRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn intersects(&self, other: &IRect) -> bool {
        !self.intersect(other).is_empty()
    }
}

/// Pixels being drawn: `pixels` holds rows of `stride` pixels, the first
/// pixel at (`origin_x`, `origin_y`), and only `clip` of them is drawn.
pub(super) struct Target<'a> {
    pixels: &'a mut [u32],
    stride: usize,
    origin_x: i32,
    origin_y: i32,
    clip: IRect,
}

impl Target<'_> {
    fn index(&self, x: i32, y: i32) -> usize {
        (y - self.origin_y) as usize * self.stride + (x - self.origin_x) as usize
    }

    /// The pixels `x0..x1` of row `y`.
    fn row(&mut self, y: i32, x0: i32, x1: i32) -> &mut [u32] {
        let start = self.index(x0, y);
        &mut self.pixels[start..start + (x1 - x0) as usize]
    }

    fn fill(&mut self, value: u32) {
        let clip = self.clip;
        for y in clip.y0..clip.y1 {
            self.row(y, clip.x0, clip.x1).fill(value);
        }
    }
}
