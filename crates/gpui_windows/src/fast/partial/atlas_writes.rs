//! Atlas tiles written between frames, and the sprites they change.
//!
//! Atlas content is outside the scene: a tile freed and allocated again to
//! another image can keep its place, so a sprite the scene draws unchanged
//! can show different pixels. Every atlas write of the process is logged
//! here (the atlases are few and their texture ids may repeat; a write to
//! another atlas's texture of the same id only damages more), and a frame
//! adds to its damage the sprites whose tile overlaps a write since the
//! canvas was drawn. Past [`MAX_LOGGED`] writes a frame is drawn whole.

use std::collections::VecDeque;

use gpui::{AtlasTextureId, AtlasTile, Bounds, DevicePixels, Scene, ScaledPixels, point, size};
use parking_lot::Mutex;

/// Writes the log keeps; a window more writes behind draws whole.
const MAX_LOGGED: usize = 256;

struct Log {
    /// The number of writes ever logged.
    count: u64,
    /// The last writes, at most [`MAX_LOGGED`]: the texture and the tile's
    /// bounds in it.
    writes: VecDeque<(AtlasTextureId, Bounds<DevicePixels>)>,
}

static LOG: Mutex<Log> = Mutex::new(Log {
    count: 0,
    writes: VecDeque::new(),
});

/// Logs that `tile` was written.
pub(crate) fn note(tile: &AtlasTile) {
    let mut log = LOG.lock();
    log.count += 1;
    if log.writes.len() == MAX_LOGGED {
        log.writes.pop_front();
    }
    log.writes.push_back((tile.texture_id, tile.bounds));
}

/// The number of writes logged so far.
pub(crate) fn count() -> u64 {
    LOG.lock().count
}

/// The writes logged after the first `seen`, or `None` when the log no
/// longer holds them all.
pub(crate) fn since(seen: u64) -> Option<Vec<(AtlasTextureId, Bounds<DevicePixels>)>> {
    let log = LOG.lock();
    let new = log.count.checked_sub(seen)?;
    if new > log.writes.len() as u64 {
        return None;
    }
    Some(
        log.writes
            .iter()
            .skip(log.writes.len() - new as usize)
            .copied()
            .collect(),
    )
}

/// The pixels the sprites of `scene` whose tile overlaps one of `writes` can
/// touch, one rectangle a sprite.
pub(crate) fn written_sprites(
    scene: &Scene,
    writes: &[(AtlasTextureId, Bounds<DevicePixels>)],
) -> Vec<Bounds<DevicePixels>> {
    let mut rects = Vec::new();
    if writes.is_empty() {
        return rects;
    }
    let written = |tile: &AtlasTile| {
        writes
            .iter()
            .any(|(id, bounds)| *id == tile.texture_id && bounds.intersects(&tile.bounds))
    };
    for sprite in &scene.monochrome_sprites {
        if written(&sprite.tile) {
            rects.push(sprite_extent(
                &sprite.bounds,
                &sprite.content_mask.bounds,
                Some(&sprite.transformation.rotation_scale),
                sprite.transformation.translation,
            ));
        }
    }
    for sprite in &scene.subpixel_sprites {
        if written(&sprite.tile) {
            rects.push(sprite_extent(
                &sprite.bounds,
                &sprite.content_mask.bounds,
                Some(&sprite.transformation.rotation_scale),
                sprite.transformation.translation,
            ));
        }
    }
    for sprite in &scene.polychrome_sprites {
        if written(&sprite.tile) {
            rects.push(sprite_extent(
                &sprite.bounds,
                &sprite.content_mask.bounds,
                None,
                [0.; 2],
            ));
        }
    }
    rects.retain(|rect| rect.size.width.0 > 0 && rect.size.height.0 > 0);
    rects
}

/// The device pixels a sprite of `bounds` can touch: its bounds transformed
/// as the shaders transform them (`rotation_scale` row-major, then
/// `translation`), clipped to the mask, rounded out and widened by a pixel
/// for the edges' antialiasing.
fn sprite_extent(
    bounds: &Bounds<ScaledPixels>,
    mask: &Bounds<ScaledPixels>,
    rotation_scale: Option<&[[f32; 2]; 2]>,
    translation: [f32; 2],
) -> Bounds<DevicePixels> {
    let x0 = bounds.origin.x.0;
    let y0 = bounds.origin.y.0;
    let x1 = x0 + bounds.size.width.0;
    let y1 = y0 + bounds.size.height.0;
    let (mut left, mut top, mut right, mut bottom) = (x0, y0, x1, y1);
    if let Some(m) = rotation_scale {
        left = f32::INFINITY;
        top = f32::INFINITY;
        right = f32::NEG_INFINITY;
        bottom = f32::NEG_INFINITY;
        for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
            let tx = m[0][0] * x + m[0][1] * y + translation[0];
            let ty = m[1][0] * x + m[1][1] * y + translation[1];
            left = left.min(tx);
            top = top.min(ty);
            right = right.max(tx);
            bottom = bottom.max(ty);
        }
    }
    left = left.max(mask.origin.x.0);
    top = top.max(mask.origin.y.0);
    right = right.min(mask.origin.x.0 + mask.size.width.0);
    bottom = bottom.min(mask.origin.y.0 + mask.size.height.0);
    if !(left < right && top < bottom) {
        return Bounds::default();
    }
    let clamp = |v: f32| v.clamp(i32::MIN as f32 / 2., i32::MAX as f32 / 2.) as i32;
    let (left, top) = (clamp(left.floor()) - 1, clamp(top.floor()) - 1);
    let (right, bottom) = (clamp(right.ceil()) + 1, clamp(bottom.ceil()) + 1);
    Bounds {
        origin: point(DevicePixels(left), DevicePixels(top)),
        size: size(DevicePixels(right - left), DevicePixels(bottom - top)),
    }
}
