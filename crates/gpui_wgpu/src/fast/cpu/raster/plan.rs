//! A scene as the drawing threads share it: its primitives, its batches as
//! `fast::frame` draws them, the pixels each primitive can reach, and the
//! content of the scroll layer tiles it composites.
//!
//! `Scene` itself cannot be shared between threads (its layers hold `Rc`s),
//! but its primitive arrays can.

use std::ops::Range;

use gpui::{
    AtlasTextureId, Bounds, LayerKey, MonochromeSprite, Path, PolychromeSprite, PrimitiveBatch,
    Quad, Rgba, ScaledPixels, Scene, Shadow, SubpixelSprite, TileCoord, TransformationMatrix,
    Underline, decode_layer_tile,
};

use super::{Ctx, IRect, Target, paths, prims};

/// A batch of `Scene::batches`, as the GPU renderer draws it.
enum Batch {
    Quads(Range<usize>),
    Shadows(Range<usize>),
    /// Paths rasterized together, then composited through `sprites`.
    Paths {
        range: Range<usize>,
        sprites: Vec<Bounds<ScaledPixels>>,
        reach: IRect,
    },
    Underlines(Range<usize>),
    Monochrome {
        texture: AtlasTextureId,
        range: Range<usize>,
    },
    Subpixel {
        texture: AtlasTextureId,
        range: Range<usize>,
    },
    Polychrome {
        texture: AtlasTextureId,
        range: Range<usize>,
    },
    /// Polychrome sprites that stand for scroll layer tiles.
    LayerTiles(Range<usize>),
}

/// A scene ready to draw on any thread.
pub(super) struct Plan<'a> {
    pub(super) quads: &'a [Quad],
    pub(super) shadows: &'a [Shadow],
    pub(super) paths: &'a [Path<ScaledPixels>],
    pub(super) underlines: &'a [Underline],
    pub(super) monochrome_sprites: &'a [MonochromeSprite],
    pub(super) subpixel_sprites: &'a [SubpixelSprite],
    pub(super) polychrome_sprites: &'a [PolychromeSprite],
    batches: Vec<Batch>,
    /// For each polychrome sprite that is a layer tile, its tile among the
    /// frame's tile plans.
    tile_index: Vec<Option<usize>>,
}

/// A scroll layer tile the scene composites, and what it holds.
pub(super) struct TilePlan<'a> {
    pub(super) layer: LayerKey,
    pub(super) tile: TileCoord,
    /// The opaque color the tile is cleared with before its content is drawn.
    pub(super) background: Rgba,
    /// The side of the tile's texture.
    pub(super) size: u32,
    /// The tile's content, in the tile's space.
    pub(super) plan: Plan<'a>,
}

/// A tile's content scene, built where the tile is composited.
pub(super) struct TileScene {
    layer: usize,
    tile: TileCoord,
    scene: Scene,
}

impl TileScene {
    pub(super) fn scene(this: &Self) -> &Scene {
        &this.scene
    }
}

/// The content scenes of the layer tiles `scene` composites within
/// `regions`, each tile once.
pub(super) fn tile_scenes(scene: &Scene, regions: &[IRect]) -> Vec<TileScene> {
    let mut tiles: Vec<TileScene> = Vec::new();
    if scene.layers.frames.is_empty() {
        return tiles;
    }
    for sprite in &scene.polychrome_sprites {
        let Some((key, tile)) = decode_layer_tile(sprite.tile.texture_id, sprite.tile.tile_id)
        else {
            continue;
        };
        let reach = reach(&sprite.bounds, &sprite.content_mask.bounds);
        if !regions.iter().any(|region| region.intersects(&reach)) {
            continue;
        }
        let Some(layer) = scene
            .layers
            .frames
            .iter()
            .position(|frame| frame.key == key)
        else {
            continue;
        };
        if tiles
            .iter()
            .any(|planned| planned.layer == layer && planned.tile == tile)
        {
            continue;
        }
        tiles.push(TileScene {
            layer,
            tile,
            scene: scene.layers.frames[layer].tile_scene(tile),
        });
    }
    tiles
}

/// The plans of the `tiles` of `scene`'s layers.
pub(super) fn tile_plans<'a>(scene: &Scene, tiles: &'a [TileScene]) -> Vec<TilePlan<'a>> {
    tiles
        .iter()
        .map(|tile| {
            let frame = &scene.layers.frames[tile.layer];
            TilePlan {
                layer: frame.key,
                tile: tile.tile,
                background: frame.background,
                size: frame.tile_size,
                plan: Plan::new(&tile.scene, &[]),
            }
        })
        .collect()
}

impl<'a> Plan<'a> {
    /// The plan of `scene`, whose layer tiles are among `tiles`.
    pub(super) fn new(scene: &'a Scene, tiles: &[TilePlan]) -> Self {
        let mut batches = Vec::new();
        for batch in scene.batches() {
            batches.push(match batch {
                PrimitiveBatch::Quads(range) => Batch::Quads(range),
                PrimitiveBatch::Shadows(range) => Batch::Shadows(range),
                PrimitiveBatch::Paths(range) => {
                    let Some(sprites) = path_sprites(&scene.paths[range.clone()]) else {
                        continue;
                    };
                    let reach = sprites.iter().fold(IRect::EMPTY, |union, sprite| {
                        union.union(&reach(sprite, sprite))
                    });
                    Batch::Paths {
                        range,
                        sprites,
                        reach,
                    }
                }
                PrimitiveBatch::Underlines(range) => Batch::Underlines(range),
                PrimitiveBatch::MonochromeSprites { texture_id, range } => Batch::Monochrome {
                    texture: texture_id,
                    range,
                },
                PrimitiveBatch::SubpixelSprites { texture_id, range } => Batch::Subpixel {
                    texture: texture_id,
                    range,
                },
                PrimitiveBatch::PolychromeSprites { texture_id, range }
                    if decode_layer_tile(texture_id, gpui::TileId(0)).is_some() =>
                {
                    Batch::LayerTiles(range)
                }
                PrimitiveBatch::PolychromeSprites { texture_id, range } => Batch::Polychrome {
                    texture: texture_id,
                    range,
                },
                PrimitiveBatch::Surfaces(_) => continue,
            });
        }

        let tile_index = if tiles.is_empty() {
            Vec::new()
        } else {
            scene
                .polychrome_sprites
                .iter()
                .map(|sprite| {
                    let (layer, tile) =
                        decode_layer_tile(sprite.tile.texture_id, sprite.tile.tile_id)?;
                    tiles
                        .iter()
                        .position(|plan| plan.layer == layer && plan.tile == tile)
                })
                .collect()
        };

        Plan {
            quads: &scene.quads,
            shadows: &scene.shadows,
            paths: &scene.paths,
            underlines: &scene.underlines,
            monochrome_sprites: &scene.monochrome_sprites,
            subpixel_sprites: &scene.subpixel_sprites,
            polychrome_sprites: &scene.polychrome_sprites,
            batches,
            tile_index,
        }
    }
}

/// The sprites a path batch is composited through, as `fast::frame` plans
/// them: one per path when they all share an order, else one over all of
/// them. `None` for an empty batch, which draws nothing.
fn path_sprites(paths: &[Path<ScaledPixels>]) -> Option<Vec<Bounds<ScaledPixels>>> {
    let first = paths.first()?;
    if paths.last().map(|path| path.order) == Some(first.order) {
        Some(paths.iter().map(|path| path.clipped_bounds()).collect())
    } else {
        let mut union = first.clipped_bounds();
        for path in paths {
            union = union.union(&path.clipped_bounds());
        }
        Some(vec![union])
    }
}

/// Past this, coordinates are clamped: no frame is this large.
const FAR: f32 = 1_000_000.;

/// The whole pixels `bounds` and `mask` overlap, rounded out.
fn reach(bounds: &Bounds<ScaledPixels>, mask: &Bounds<ScaledPixels>) -> IRect {
    let x0 = bounds.origin.x.0.max(mask.origin.x.0);
    let y0 = bounds.origin.y.0.max(mask.origin.y.0);
    let x1 = (bounds.origin.x.0 + bounds.size.width.0).min(mask.origin.x.0 + mask.size.width.0);
    let y1 = (bounds.origin.y.0 + bounds.size.height.0).min(mask.origin.y.0 + mask.size.height.0);
    rounded_out(x0, y0, x1, y1)
}

fn rounded_out(x0: f32, y0: f32, x1: f32, y1: f32) -> IRect {
    // NaN bounds reach nothing.
    if !(x0 <= x1 && y0 <= y1) {
        return IRect::EMPTY;
    }
    let lo = |v: f32| (v.clamp(-FAR, FAR).floor() as i32) - 1;
    let hi = |v: f32| (v.clamp(-FAR, FAR).ceil() as i32) + 1;
    IRect::new(lo(x0), lo(y0), hi(x1), hi(y1))
}

/// The pixels being drawn, a pixel wider on every side, to tell cheaply
/// which primitives cannot reach them.
struct Reach {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    clip: IRect,
}

impl Reach {
    fn new(clip: IRect) -> Self {
        Reach {
            x0: clip.x0 as f32 - 1.,
            y0: clip.y0 as f32 - 1.,
            x1: clip.x1 as f32 + 1.,
            y1: clip.y1 as f32 + 1.,
            clip,
        }
    }

    /// Whether the overlap of `bounds` and `mask` can reach the pixels.
    #[inline]
    fn bounds(&self, bounds: &Bounds<ScaledPixels>, mask: &Bounds<ScaledPixels>) -> bool {
        let (bx, by) = (bounds.origin.x.0, bounds.origin.y.0);
        let (mx, my) = (mask.origin.x.0, mask.origin.y.0);
        bx.max(mx) < self.x1
            && by.max(my) < self.y1
            && (bx + bounds.size.width.0).min(mx + mask.size.width.0) > self.x0
            && (by + bounds.size.height.0).min(my + mask.size.height.0) > self.y0
    }

    /// Whether a transformed sprite can reach the pixels.
    #[inline]
    fn transformed(
        &self,
        bounds: &Bounds<ScaledPixels>,
        transformation: &TransformationMatrix,
        mask: &Bounds<ScaledPixels>,
    ) -> bool {
        if *transformation == TransformationMatrix::unit() {
            return self.bounds(bounds, mask);
        }
        let corners = prims::transformed_corners(bounds, transformation);
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in corners {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        rounded_out(x0, y0, x1, y1)
            .intersect(&reach(mask, mask))
            .intersects(&self.clip)
    }

    fn shadow(&self, shadow: &Shadow) -> bool {
        let geometry = if shadow.inset != 0 {
            shadow.element_bounds
        } else {
            shadow
                .bounds
                .dilate(ScaledPixels(3. * shadow.blur_radius.0))
        };
        self.bounds(&geometry, &shadow.content_mask.bounds)
    }
}

/// Draws `plan` into `target`, batch by batch, as the GPU renderer does.
pub(super) fn draw_plan(plan: &Plan, ctx: &Ctx, target: &mut Target) {
    let reach = Reach::new(target.clip);
    let mut glyphs = prims::GlyphCache::default();
    for batch in &plan.batches {
        match batch {
            Batch::Quads(range) => {
                for index in range.clone() {
                    let quad = &plan.quads[index];
                    if reach.bounds(&quad.bounds, &quad.content_mask.bounds) {
                        prims::quad(quad, ctx, target);
                    }
                }
            }
            Batch::Shadows(range) => {
                for index in range.clone() {
                    let shadow = &plan.shadows[index];
                    if reach.shadow(shadow) {
                        prims::shadow(shadow, ctx, target);
                    }
                }
            }
            Batch::Paths {
                range,
                sprites,
                reach: batch_reach,
            } => {
                if batch_reach.intersects(&reach.clip) {
                    paths::draw_batch(&plan.paths[range.clone()], sprites, ctx, target);
                }
            }
            Batch::Underlines(range) => {
                for index in range.clone() {
                    let underline = &plan.underlines[index];
                    if reach.bounds(&underline.bounds, &underline.content_mask.bounds) {
                        prims::underline(underline, ctx, target);
                    }
                }
            }
            Batch::Monochrome { texture, range } => {
                let reaches = |index: usize| {
                    let sprite = &plan.monochrome_sprites[index];
                    reach.transformed(
                        &sprite.bounds,
                        &sprite.transformation,
                        &sprite.content_mask.bounds,
                    )
                };
                if !range.clone().any(reaches) {
                    continue;
                }
                let Some(texture) = prims::Texture::new(ctx.sprites.texture(*texture)) else {
                    continue;
                };
                for index in range.clone() {
                    if reaches(index) {
                        let sprite = &plan.monochrome_sprites[index];
                        prims::monochrome(
                            prims::MonochromeSource::new(sprite),
                            &texture,
                            ctx,
                            target,
                            &mut glyphs,
                        );
                    }
                }
            }
            Batch::Subpixel { texture, range } => {
                let reaches = |index: usize| {
                    let sprite = &plan.subpixel_sprites[index];
                    reach.transformed(
                        &sprite.bounds,
                        &sprite.transformation,
                        &sprite.content_mask.bounds,
                    )
                };
                if !range.clone().any(reaches) {
                    continue;
                }
                let Some(texture) = prims::Texture::new(ctx.sprites.texture(*texture)) else {
                    continue;
                };
                for index in range.clone() {
                    if !reaches(index) {
                        continue;
                    }
                    let sprite = &plan.subpixel_sprites[index];
                    if ctx.params.dual_source_blending {
                        prims::subpixel(sprite, &texture, ctx, target);
                    } else {
                        // Without dual-source blending the renderer draws
                        // them with the monochrome pipeline, which samples
                        // the texture's red channel.
                        prims::monochrome(
                            prims::MonochromeSource::from_subpixel(sprite),
                            &texture,
                            ctx,
                            target,
                            &mut glyphs,
                        );
                    }
                }
            }
            Batch::Polychrome { texture, range } => {
                let reaches = |index: usize| {
                    let sprite = &plan.polychrome_sprites[index];
                    reach.bounds(&sprite.bounds, &sprite.content_mask.bounds)
                };
                if !range.clone().any(reaches) {
                    continue;
                }
                let Some(texture) = prims::Texture::new(ctx.sprites.texture(*texture)) else {
                    continue;
                };
                for index in range.clone() {
                    if reaches(index) {
                        prims::polychrome(
                            &plan.polychrome_sprites[index],
                            &prims::Sampler::Atlas(&texture),
                            ctx,
                            target,
                        );
                    }
                }
            }
            Batch::LayerTiles(range) => {
                for index in range.clone() {
                    let sprite = &plan.polychrome_sprites[index];
                    if !reach.bounds(&sprite.bounds, &sprite.content_mask.bounds) {
                        continue;
                    }
                    let Some(tile) = plan.tile_index.get(index).copied().flatten() else {
                        continue;
                    };
                    prims::layer_tile(
                        &plan.polychrome_sprites[index],
                        &ctx.tiles[tile],
                        ctx,
                        target,
                    );
                }
            }
        }
    }
}
