//! Rasterizing a scroll layer tile: the layer's content over the tile,
//! translated into the tile's space, drawn into the tile's texture cleared
//! to the layer's baked background, with the frame's pipelines and a
//! viewport the size of the tile.
//!
//! All position-dependent shading is relative to each primitive's own bounds,
//! and the tile is a whole number of device pixels from the content's
//! origin, so a tile holds exactly the pixels the content would have drawn
//! over the same background in the window.

use gpui::{Rgba, Scene, SceneLayers, TileCoord};

use crate::fast::frame::{DrawGraph, FrameState, FrameTarget, SceneDraw, draw_scene};
use crate::wgpu_renderer::GlobalParams;

/// A tile to rasterize this frame, with the part of its layer's content
/// that covers it.
pub(crate) struct TileRaster {
    /// The index of the tile's layer in the frame's `SceneLayers::frames`.
    pub(crate) layer: usize,
    pub(crate) tile: TileCoord,
    pub(crate) scene: Scene,
}

/// The scenes of the tiles `planned` by `TileCache::begin_frame`.
pub(crate) fn plan(layers: &SceneLayers, planned: &[(usize, TileCoord)]) -> Vec<TileRaster> {
    planned
        .iter()
        .map(|&(layer, tile)| TileRaster {
            layer,
            tile,
            scene: layers.frames[layer].tile_scene(tile),
        })
        .collect()
}

/// The clear colour a tile over `background` starts from: the bytes an
/// opaque quad of that colour writes to the frame's non-sRGB texture.
pub(crate) fn clear_color(background: Rgba) -> wgpu::Color {
    wgpu::Color {
        r: background.r as f64,
        g: background.g as f64,
        b: background.b as f64,
        a: background.a as f64,
    }
}

/// Records the pass that rasterizes `tile` into its texture: the frame's
/// planned scene `scene_index`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize(
    target: &FrameTarget,
    state: &FrameState,
    globals: &[GlobalParams],
    encoder: &mut wgpu::CommandEncoder,
    layers: &SceneLayers,
    tile: &TileRaster,
    scene_index: usize,
    graph: &DrawGraph,
) {
    let layer = &layers.frames[tile.layer];
    let Some(view) = state.layers.texture(layer.key, tile.tile) else {
        debug_assert!(false, "a planned tile was not prepared");
        return;
    };
    draw_scene(
        target,
        state,
        globals,
        encoder,
        &SceneDraw {
            scene: scene_index,
            view,
            clear: clear_color(layer.background),
            label: "layer_tile_pass",
            graph,
        },
    );
}
