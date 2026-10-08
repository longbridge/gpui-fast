//! Path batches, drawn as the GPU renderer draws them: every path of the
//! batch rasterized, triangle by triangle, into an intermediate texture
//! cleared to transparent, with 4× multisampling (`fs_path_rasterization`
//! run once per pixel at its center, its result blended premultiplied into
//! each sample the triangle covers), the samples averaged, and the result
//! composited through the batch's sprites with the paths blend.

use gpui::{Bounds, Path, ScaledPixels};

use super::prims::{SUBPIXEL, Triangle};
use super::shade::{Blend, FBounds, Paint, levels, saturate};
use super::{Ctx, IRect, Target};

/// The standard positions of 4 samples within a pixel, in subpixel units
/// from its top-left corner.
const SAMPLES: [(i64, i64); 4] = [(96, 32), (224, 96), (32, 160), (160, 224)];

/// Draws a batch of `paths` composited through `sprites`.
pub(super) fn draw_batch(
    paths: &[Path<ScaledPixels>],
    sprites: &[Bounds<ScaledPixels>],
    ctx: &Ctx,
    target: &mut Target,
) {
    // The pixels each sprite covers: the only ones the intermediate texture
    // is read at.
    let sprite_rects: Vec<IRect> = sprites
        .iter()
        .map(|sprite| sprite_pixels(sprite).intersect(&target.clip))
        .filter(|rect| !rect.is_empty())
        .collect();
    let area = sprite_rects
        .iter()
        .fold(IRect::EMPTY, |union, rect| union.union(rect));
    if area.is_empty() {
        return;
    }
    let width = (area.x1 - area.x0) as usize;
    let height = (area.y1 - area.y0) as usize;
    let mut samples = vec![[0u32; 4]; width * height];
    let mut touched = false;

    let unit = SUBPIXEL as i64;
    for path in paths {
        let clip = FBounds::new(&path.clipped_bounds());
        let paint = Paint::new(&path.color);
        for triangle in path.vertices.chunks_exact(3) {
            let points =
                [0, 1, 2].map(|i| (triangle[i].xy_position.x.0, triangle[i].xy_position.y.0));
            let Some(coverage) = Triangle::new(points) else {
                continue;
            };
            let pixels = coverage.pixels.intersect(&area);
            if pixels.is_empty() {
                continue;
            }
            let st = [0, 1, 2].map(|i| (triangle[i].st_position.x, triangle[i].st_position.y));
            let gradient = StGradient::new(points, st);

            for y in pixels.y0..pixels.y1 {
                let py = y as f32 + 0.5;
                let row = (y - area.y0) as usize * width;
                for x in pixels.x0..pixels.x1 {
                    let mut covered = [false; 4];
                    let mut any = false;
                    for (sample, &(sx, sy)) in covered.iter_mut().zip(&SAMPLES) {
                        *sample = coverage.covers(x as i64 * unit + sx, y as i64 * unit + sy);
                        any |= *sample;
                    }
                    let px = x as f32 + 0.5;
                    if !any || !clip.clip_contains(px, py) {
                        continue;
                    }
                    let alpha = gradient.alpha(px, py);
                    let color = paint.color_at(px, py, &clip);
                    let src = [
                        color[0] * color[3] * alpha,
                        color[1] * color[3] * alpha,
                        color[2] * color[3] * alpha,
                        color[3] * alpha,
                    ];
                    if Blend::Premultiplied.is_noop(src, ctx.bits) {
                        continue;
                    }
                    touched = true;
                    let pixel = &mut samples[row + (x - area.x0) as usize];
                    for (value, covered) in pixel.iter_mut().zip(covered) {
                        if covered {
                            *value = Blend::Premultiplied.apply(*value, src, ctx.bits);
                        }
                    }
                }
            }
        }
    }
    if !touched {
        return;
    }

    // Resolve: each channel the average of its samples.
    let intermediate: Vec<u32> = samples.iter().map(|pixel| resolve(pixel)).collect();

    for rect in &sprite_rects {
        for y in rect.y0..rect.y1 {
            let from = (y - area.y0) as usize * width + (rect.x0 - area.x0) as usize;
            let source = &intermediate[from..from + (rect.x1 - rect.x0) as usize];
            for (pixel, &texel) in target.row(y, rect.x0, rect.x1).iter_mut().zip(source) {
                if texel != 0 {
                    *pixel = Blend::Paths.apply_levels(*pixel, levels(texel));
                }
            }
        }
    }
}

/// The pixels a path sprite's quad covers.
fn sprite_pixels(bounds: &Bounds<ScaledPixels>) -> IRect {
    let b = FBounds::new(bounds);
    let snap = |v: f32| (v as f64 * SUBPIXEL).round() / SUBPIXEL;
    let span = |lo: f32, hi: f32| {
        let (lo, hi) = (snap(lo), snap(hi));
        if lo.partial_cmp(&hi) != Some(std::cmp::Ordering::Less) {
            return (0, 0);
        }
        let far = 1_000_000.;
        (
            (lo - 0.5).ceil().clamp(-far, far) as i32,
            (hi - 0.5).ceil().clamp(-far, far) as i32,
        )
    };
    let (x0, x1) = span(b.x, b.x + b.w);
    let (y0, y1) = span(b.y, b.y + b.h);
    IRect::new(x0, y0, x1, y1)
}

/// The average of 4 samples, channel by channel, rounded to the nearest level.
fn resolve(samples: &[u32; 4]) -> u32 {
    let mut out = 0;
    for shift in [0, 8, 16, 24] {
        let sum: u32 = samples.iter().map(|sample| sample >> shift & 0xff).sum();
        out |= ((sum + 2) / 4) << shift;
    }
    out
}

/// A triangle's `st_position` as the GPU interpolates it, with `dpdx` and
/// `dpdy` of it, which are constant over the triangle.
struct StGradient {
    origin: (f32, f32),
    st0: (f32, f32),
    ds_dx: f32,
    ds_dy: f32,
    dt_dx: f32,
    dt_dy: f32,
}

impl StGradient {
    fn new(points: [(f32, f32); 3], st: [(f32, f32); 3]) -> Self {
        let (p0, p1, p2) = (points[0], points[1], points[2]);
        let (e1x, e1y) = (p1.0 - p0.0, p1.1 - p0.1);
        let (e2x, e2y) = (p2.0 - p0.0, p2.1 - p0.1);
        let det = e1x * e2y - e2x * e1y;
        let gradient = |a0: f32, a1: f32, a2: f32| {
            let (d1, d2) = (a1 - a0, a2 - a0);
            ((d1 * e2y - d2 * e1y) / det, (d2 * e1x - d1 * e2x) / det)
        };
        let (ds_dx, ds_dy) = gradient(st[0].0, st[1].0, st[2].0);
        let (dt_dx, dt_dy) = gradient(st[0].1, st[1].1, st[2].1);
        StGradient {
            origin: p0,
            st0: st[0],
            ds_dx,
            ds_dy,
            dt_dx,
            dt_dy,
        }
    }

    /// `fs_path_rasterization`'s coverage at (`px`, `py`).
    #[inline]
    fn alpha(&self, px: f32, py: f32) -> f32 {
        if (self.ds_dx * self.ds_dx + self.ds_dy * self.ds_dy).sqrt() < 0.001 {
            return 1.0;
        }
        let dx = px - self.origin.0;
        let dy = py - self.origin.1;
        let s = self.st0.0 + self.ds_dx * dx + self.ds_dy * dy;
        let t = self.st0.1 + self.dt_dx * dx + self.dt_dy * dy;
        let gradient_x = 2.0 * s * self.ds_dx - self.dt_dx;
        let gradient_y = 2.0 * s * self.ds_dy - self.dt_dy;
        let f = s * s - t;
        let distance = f / (gradient_x * gradient_x + gradient_y * gradient_y).sqrt();
        saturate(0.5 - distance)
    }
}
