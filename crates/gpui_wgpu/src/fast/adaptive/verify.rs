//! `GPUI_CPU_VERIFY=1`: checks every CPU frame against the whole scene.
//!
//! A CPU frame redraws the canvas only inside its region; outside it, the
//! canvas keeps what earlier frames drew. That is right only when the region
//! holds every pixel that changed: the scene's damage, the stale pixels of GPU
//! frames, the sprites over written atlas tiles. Here, after each CPU frame,
//! the scene is drawn again whole into a second canvas and the two are
//! compared pixel by pixel. The raster's pixels do not depend on how a region
//! is split, so any difference is a change the region missed. Mismatches are
//! logged with their bounds and counted in the render stats (`verify_*`).
//!
//! It costs a whole CPU frame per frame: for debugging and checking real
//! applications, not for use.

use std::sync::OnceLock;

use gpui::{Bounds, DevicePixels, Scene};

use crate::fast::adaptive::region;
use crate::fast::cpu::raster::{self, Canvas, RasterParams, SpritePixels};

/// Whether `GPUI_CPU_VERIFY=1`.
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("GPUI_CPU_VERIFY").is_ok_and(|value| value == "1"))
}

/// Pixels where an incrementally drawn canvas differs from the scene drawn
/// whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mismatch {
    pub(crate) pixels: u64,
    /// The bounds of the differing pixels.
    pub(crate) bounds: Bounds<DevicePixels>,
}

/// Compares `canvas`, just drawn inside a region, with `scene` drawn whole
/// into `whole` (kept between frames to reuse its memory).
pub(crate) fn check(
    canvas: &Canvas,
    whole: &mut Canvas,
    scene: &Scene,
    sprites: &dyn SpritePixels,
    params: &RasterParams,
    threads: usize,
) -> Option<Mismatch> {
    let (width, height) = (canvas.width(), canvas.height());
    if whole.width() != width || whole.height() != height {
        *whole = Canvas::new(width, height);
    }
    raster::draw(
        whole,
        scene,
        &[region::whole(width, height)],
        sprites,
        params,
        threads,
    );
    compare(canvas.pixels(), whole.pixels(), width)
}

/// Where two frames of `width` pixels per row differ.
pub(crate) fn compare(drawn: &[u32], expected: &[u32], width: u32) -> Option<Mismatch> {
    let width = width.max(1) as usize;
    let mut pixels = 0u64;
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for (y, (row, expected_row)) in drawn
        .chunks_exact(width)
        .zip(expected.chunks_exact(width))
        .enumerate()
    {
        if row == expected_row {
            continue;
        }
        for (x, _) in row
            .iter()
            .zip(expected_row)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
        {
            pixels += 1;
            x0 = x0.min(x);
            x1 = x1.max(x + 1);
            y0 = y0.min(y);
            y1 = y1.max(y + 1);
        }
    }
    (pixels > 0).then(|| Mismatch {
        pixels,
        bounds: region::rect(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32),
    })
}

#[cfg(test)]
mod tests {
    use super::compare;
    use crate::fast::adaptive::region;

    #[test]
    fn compare_finds_the_bounds_of_differences() {
        let expected = vec![0u32; 6 * 4];
        assert_eq!(compare(&expected, &expected, 6), None);
        let mut drawn = expected.clone();
        drawn[6 + 2] = 1;
        drawn[2 * 6 + 4] = 1;
        let mismatch = compare(&drawn, &expected, 6).unwrap();
        assert_eq!(mismatch.pixels, 2);
        assert_eq!(mismatch.bounds, region::rect(2, 1, 3, 2));
    }
}
