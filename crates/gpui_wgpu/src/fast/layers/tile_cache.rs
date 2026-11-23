//! The textures scroll layer tiles are rasterized into, kept from frame to
//! frame so that a frame that only scrolled draws them again without
//! rasterizing anything.
//!
//! A tile's texture holds the layer's content of one generation. The core
//! bumps a layer's generation whenever it repaints the content and lists the
//! tiles whose pixels changed (`LayerFrame::dirty_tiles`); every other tile
//! of the previous generation stays as it is. A tile the cache has missed a
//! generation of could have changed in it, so it is rasterized again.

use collections::{FxHashMap, FxHashSet};
use gpui::{LayerKey, SceneLayers, TileCoord};

use crate::fast::frame::FrameTarget;

/// How much tile texture memory a window keeps, by default (spec §5.6).
pub(crate) const DEFAULT_BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Frames a layer that no frame composites keeps its tiles for; the core
/// drops such a layer after as many.
const LAYER_KEEP_FRAMES: u64 = 120;

/// Textures of released tiles kept for new tiles to reuse.
const POOL_LIMIT: usize = 8;

/// A window's scroll layer tiles, and what rasterizing them takes.
pub(crate) struct TileCache {
    tiles: FxHashMap<(LayerKey, TileCoord), Tile>,
    layers: FxHashMap<LayerKey, SeenLayer>,
    pool: Vec<TileTexture>,
    frame: u64,
    budget_bytes: u64,
}

impl Default for TileCache {
    fn default() -> Self {
        Self {
            tiles: FxHashMap::default(),
            layers: FxHashMap::default(),
            pool: Vec::new(),
            frame: 0,
            budget_bytes: DEFAULT_BUDGET_BYTES,
        }
    }
}

struct Tile {
    /// The generation of the content the texture holds, or will hold once
    /// this frame's rasterization runs.
    generation: u64,
    tile_size: u32,
    valid: bool,
    /// The last frame that composited or rasterized the tile.
    last_used: u64,
    texture: Option<TileTexture>,
}

struct SeenLayer {
    generation: u64,
    last_frame: u64,
}

pub(crate) struct TileTexture {
    size: u32,
    /// Read back by the pixel tests; the view keeps it alive otherwise.
    #[cfg_attr(not(test), allow(dead_code))]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

fn tile_bytes(tile_size: u32) -> u64 {
    tile_size as u64 * tile_size as u64 * 4
}

impl TileCache {
    /// The texture of `tile` of `layer`, if the cache holds its content.
    pub(crate) fn texture(&self, layer: LayerKey, tile: TileCoord) -> Option<&wgpu::TextureView> {
        self.tiles
            .get(&(layer, tile))
            .filter(|tile| tile.valid)
            .and_then(|tile| tile.texture.as_ref())
            .map(|texture| &texture.view)
    }

    #[cfg(test)]
    pub(crate) fn tile_texture(&self, layer: LayerKey, tile: TileCoord) -> Option<&wgpu::Texture> {
        self.tiles
            .get(&(layer, tile))
            .filter(|tile| tile.valid)
            .and_then(|tile| tile.texture.as_ref())
            .map(|texture| &texture.texture)
    }

    #[cfg(test)]
    pub(crate) fn holds(&self, layer: LayerKey, tile: TileCoord) -> bool {
        self.tiles
            .get(&(layer, tile))
            .is_some_and(|tile| tile.valid)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.layers.is_empty() && self.pool.is_empty()
    }

    /// Starts a frame that composites the tiles `composited` of the layers in
    /// `layers`. Returns the tiles to rasterize before they are drawn, each
    /// with the index of its layer in `layers.frames`: every composited tile
    /// the cache does not hold, or holds from an older generation. A dirty
    /// tile the frame does not composite is only marked stale, and waits
    /// until a frame shows it: a scrolling list dirties the tiles at the edge
    /// of its overscan on most frames. They count as held from here on.
    pub(crate) fn begin_frame(
        &mut self,
        layers: &SceneLayers,
        composited: impl Iterator<Item = (LayerKey, TileCoord)>,
    ) -> Vec<(usize, TileCoord)> {
        self.frame += 1;
        let frame = self.frame;
        let mut planned = FxHashSet::default();

        for layer in &layers.frames {
            let seen = self.layers.entry(layer.key).or_insert(SeenLayer {
                generation: layer.generation.wrapping_sub(2),
                last_frame: frame,
            });
            seen.last_frame = frame;
            if seen.generation == layer.generation {
                continue;
            }
            let previous = seen.generation;
            let consecutive = previous.wrapping_add(1) == layer.generation;
            seen.generation = layer.generation;
            let dirty: FxHashSet<TileCoord> = layer.dirty_tiles.iter().copied().collect();
            for ((key, coord), tile) in &mut self.tiles {
                if *key != layer.key {
                    continue;
                }
                if consecutive
                    && tile.valid
                    && tile.generation == previous
                    && tile.tile_size == layer.tile_size
                    && !dirty.contains(coord)
                {
                    tile.generation = layer.generation;
                } else {
                    tile.valid = false;
                }
            }
        }

        let mut indices = FxHashMap::default();
        for (index, layer) in layers.frames.iter().enumerate() {
            indices.insert(layer.key, index);
        }
        for (key, coord) in composited {
            let Some(&index) = indices.get(&key) else {
                debug_assert!(false, "a composited layer tile has no layer frame");
                continue;
            };
            let layer = &layers.frames[index];
            match self.tiles.get_mut(&(key, coord)) {
                Some(tile)
                    if tile.valid
                        && tile.generation == layer.generation
                        && tile.tile_size == layer.tile_size =>
                {
                    tile.last_used = frame;
                }
                _ => {
                    planned.insert((index, coord));
                }
            }
        }

        let mut planned: Vec<(usize, TileCoord)> = planned.into_iter().collect();
        planned.sort();
        for &(index, coord) in &planned {
            let layer = &layers.frames[index];
            let tile = self.tiles.entry((layer.key, coord)).or_insert(Tile {
                generation: layer.generation,
                tile_size: layer.tile_size,
                valid: false,
                last_used: frame,
                texture: None,
            });
            tile.generation = layer.generation;
            tile.tile_size = layer.tile_size;
            tile.valid = true;
            tile.last_used = frame;
        }

        self.drop_unseen_layers();
        planned
    }

    /// Releases the tiles of layers no frame has composited for a while.
    fn drop_unseen_layers(&mut self) {
        let frame = self.frame;
        let gone: Vec<LayerKey> = self
            .layers
            .iter()
            .filter(|(_, seen)| seen.last_frame + LAYER_KEEP_FRAMES < frame)
            .map(|(key, _)| *key)
            .collect();
        if gone.is_empty() {
            return;
        }
        for key in &gone {
            self.layers.remove(key);
        }
        let released: Vec<(LayerKey, TileCoord)> = self
            .tiles
            .keys()
            .filter(|(key, _)| gone.contains(key))
            .copied()
            .collect();
        for key in released {
            if let Some(texture) = self.tiles.remove(&key).and_then(|tile| tile.texture)
                && self.pool.len() < POOL_LIMIT
            {
                self.pool.push(texture);
            }
        }
    }

    /// Releases tiles, least recently used first, until the tiles and the
    /// pool fit in `budget_bytes`. Tiles this frame composites or
    /// rasterizes are kept even when they alone exceed the budget.
    pub(crate) fn evict_to_budget(&mut self, budget_bytes: u64) {
        let mut bytes: u64 = self
            .tiles
            .values()
            .filter(|tile| tile.valid || tile.texture.is_some())
            .map(|tile| tile_bytes(tile.tile_size))
            .sum::<u64>()
            + self
                .pool
                .iter()
                .map(|texture| tile_bytes(texture.size))
                .sum::<u64>();
        while bytes > budget_bytes
            && let Some(texture) = self.pool.pop()
        {
            bytes -= tile_bytes(texture.size);
        }
        if bytes <= budget_bytes {
            return;
        }
        let mut candidates: Vec<(u64, LayerKey, TileCoord)> = self
            .tiles
            .iter()
            .filter(|(_, tile)| tile.last_used != self.frame)
            .map(|((key, coord), tile)| (tile.last_used, *key, *coord))
            .collect();
        candidates.sort_by_key(|&(last_used, key, coord)| (last_used, key.0, coord));
        for (_, key, coord) in candidates {
            if bytes <= budget_bytes {
                break;
            }
            if let Some(tile) = self.tiles.remove(&(key, coord))
                && (tile.valid || tile.texture.is_some())
            {
                bytes -= tile_bytes(tile.tile_size);
            }
        }
    }

    /// Releases every tile and texture, as when the window's size or scale,
    /// or its GPU device, changes.
    pub(crate) fn clear(&mut self) {
        self.tiles.clear();
        self.layers.clear();
        self.pool.clear();
    }

    /// Gives every tile in `planned` a texture, fits the cache in its
    /// budget, and makes the globals and path textures of the frame's tile
    /// sizes.
    pub(crate) fn prepare(
        &mut self,
        target: &FrameTarget,
        layers: &SceneLayers,
        planned: &[(usize, TileCoord)],
    ) {
        self.evict_to_budget(self.budget_bytes);
        for &(index, coord) in planned {
            let layer = &layers.frames[index];
            let Some(tile) = self.tiles.get_mut(&(layer.key, coord)) else {
                continue;
            };
            if tile
                .texture
                .as_ref()
                .is_some_and(|texture| texture.size == tile.tile_size)
            {
                continue;
            }
            let size = tile.tile_size;
            let texture = match self.pool.iter().position(|texture| texture.size == size) {
                Some(position) => self.pool.swap_remove(position),
                None => create_tile_texture(target.device, target.format, size),
            };
            tile.texture = Some(texture);
        }
    }
}

fn create_tile_texture(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: u32,
) -> TileTexture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("scroll_layer_tile"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    TileTexture {
        size,
        texture,
        view,
    }
}
