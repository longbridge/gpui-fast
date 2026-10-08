//! Quads, shadows, underlines and sprites: which pixels each one's
//! fragments land on, `fs_quad`, `fs_shadow`, `fs_underline`,
//! `fs_mono_sprite`, `fs_subpixel_sprite` and `fs_poly_sprite` ported, and
//! scroll layer tiles drawn from their content.

use gpui::{
    AtlasTile, BorderStyle, Bounds, Hsla, MonochromeSprite, PolychromeSprite, Quad, ScaledPixels,
    Shadow, SubpixelSprite, TransformationMatrix, Underline,
};

use super::plan::{TilePlan, draw_plan};
use super::shade::{
    Blend, FBounds, FragmentBits, GrayscaleCorrection, M_PI_F, Paint, Radii, SubpixelCorrection,
    UNORM8, V4, blend_color, blend_subpixel, blur_curve, clear_pixel, erf, fmod, gaussian,
    grayscale, hsla_to_rgba, over, pick_corner_radius, quad_sdf, quad_sdf_impl,
    quarter_ellipse_sdf, saturate, unpack,
};
use super::{Ctx, IRect, Target, TexturePixels};

// --- which pixels fragments land on --- //

/// The rasterizer's subpixel precision: vertices snap to 1/256 of a pixel.
pub(super) const SUBPIXEL: f64 = 256.;

/// Past this, coordinates are clamped: no frame is this large.
const FAR: f64 = 1_000_000.;

fn snap(v: f32) -> f64 {
    (v as f64 * SUBPIXEL).round() / SUBPIXEL
}

/// `v` snapped to the rasterizer's subpixel grid.
fn snap32(v: f32) -> f32 {
    snap(v) as f32
}

/// The pixels whose centers lie in `lo..hi`, edges snapped as the
/// rasterizer snaps them: an axis-aligned rectangle of two triangles covers
/// a center on its left or top edge but not on its right or bottom one.
fn rect_span(lo: f32, hi: f32) -> (i32, i32) {
    let (lo, hi) = (snap(lo), snap(hi));
    if lo.partial_cmp(&hi) != Some(std::cmp::Ordering::Less) {
        return (0, 0);
    }
    let first = (lo - 0.5).ceil().clamp(-FAR, FAR) as i32;
    let end = (hi - 0.5).ceil().clamp(-FAR, FAR) as i32;
    (first, end)
}

/// The pixels whose centers pass the shaders' clip distance test against
/// the mask `origin..origin + size` (both ends included).
fn mask_span(origin: f32, size: f32) -> (i32, i32) {
    let end = origin + size;
    if !matches!(
        origin.partial_cmp(&end),
        Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
    ) {
        return (0, 0);
    }
    let first = (origin as f64 - 0.5).ceil().clamp(-FAR, FAR) as i32;
    let end = ((end as f64 - 0.5).floor() + 1.).clamp(-FAR, FAR) as i32;
    (first, end)
}

/// The pixels of the rectangle `x0..x1` by `y0..y1` within `mask` and `clip`.
fn fragments(x0: f32, y0: f32, x1: f32, y1: f32, mask: &FBounds, clip: &IRect) -> IRect {
    let (fx0, fx1) = rect_span(x0, x1);
    let (fy0, fy1) = rect_span(y0, y1);
    let (mx0, mx1) = mask_span(mask.x, mask.w);
    let (my0, my1) = mask_span(mask.y, mask.h);
    IRect::new(fx0, fy0, fx1, fy1)
        .intersect(&IRect::new(mx0, my0, mx1, my1))
        .intersect(clip)
}

/// A triangle as the rasterizer covers sample points: vertices snapped to
/// the subpixel grid, and the top-left rule for points on its edges.
pub(super) struct Triangle {
    /// Each edge's function `a * x + b * y + c`, positive inside, in
    /// subpixel units, and whether points on the edge are covered.
    edges: [(i64, i64, i64, bool); 3],
    /// The pixels the triangle can cover.
    pub(super) pixels: IRect,
}

impl Triangle {
    /// The triangle of `points`, or `None` if it covers nothing.
    pub(super) fn new(points: [(f32, f32); 3]) -> Option<Self> {
        let mut p = [(0i64, 0i64); 3];
        for (snapped, (x, y)) in p.iter_mut().zip(points) {
            let x = (x as f64 * SUBPIXEL).round();
            let y = (y as f64 * SUBPIXEL).round();
            if !(x.abs() < FAR * SUBPIXEL && y.abs() < FAR * SUBPIXEL) {
                return None;
            }
            *snapped = (x as i64, y as i64);
        }
        let area = edge_function(p[0], p[1], p[2]);
        if area == 0 {
            return None;
        }
        if area < 0 {
            p.swap(1, 2);
        }
        let edge = |a: (i64, i64), b: (i64, i64)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let top = dy == 0 && dx > 0;
            let left = dy < 0;
            (-dy, dx, dy * a.0 - dx * a.1, top || left)
        };
        let min_x = p.iter().map(|v| v.0).min().unwrap_or(0);
        let max_x = p.iter().map(|v| v.0).max().unwrap_or(0);
        let min_y = p.iter().map(|v| v.1).min().unwrap_or(0);
        let max_y = p.iter().map(|v| v.1).max().unwrap_or(0);
        let unit = SUBPIXEL as i64;
        Some(Triangle {
            edges: [edge(p[0], p[1]), edge(p[1], p[2]), edge(p[2], p[0])],
            pixels: IRect::new(
                min_x.div_euclid(unit) as i32,
                min_y.div_euclid(unit) as i32,
                max_x.div_euclid(unit) as i32 + 1,
                max_y.div_euclid(unit) as i32 + 1,
            ),
        })
    }

    /// Whether the triangle covers the point (`x`, `y`), in subpixel units.
    #[inline]
    pub(super) fn covers(&self, x: i64, y: i64) -> bool {
        self.edges.iter().all(|&(a, b, c, inclusive)| {
            let w = a * x + b * y + c;
            w > 0 || (w == 0 && inclusive)
        })
    }

    /// Whether the triangle covers the center of pixel (`x`, `y`).
    #[inline]
    pub(super) fn covers_center(&self, x: i32, y: i32) -> bool {
        let half = SUBPIXEL as i64 / 2;
        let unit = SUBPIXEL as i64;
        self.covers(x as i64 * unit + half, y as i64 * unit + half)
    }
}

fn edge_function(a: (i64, i64), b: (i64, i64), p: (i64, i64)) -> i64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// How a constant fragment is blended over many pixels.
enum Fill {
    /// The fragment changes nothing.
    Nothing,
    /// The fragment replaces every pixel with this one.
    Replace(u32),
    Blend(Blend, [u32; 4]),
}

impl Fill {
    fn new(blend: Blend, src: V4, bits: FragmentBits) -> Self {
        let levels = bits.levels(src);
        if blend.is_noop_levels(levels) {
            Fill::Nothing
        } else if levels[3] == 255 && blend != Blend::Paths {
            // dst * (255 - 255) leaves the source alone.
            Fill::Replace(blend.apply_levels(0, levels))
        } else {
            Fill::Blend(blend, levels)
        }
    }

    #[inline]
    fn apply(&self, pixel: &mut u32) {
        match self {
            Fill::Nothing => {}
            Fill::Replace(value) => *pixel = *value,
            Fill::Blend(blend, levels) => *pixel = blend.apply_levels(*pixel, *levels),
        }
    }

    fn row(&self, row: &mut [u32]) {
        match self {
            Fill::Nothing => {}
            Fill::Replace(value) => row.fill(*value),
            _ => row.iter_mut().for_each(|pixel| self.apply(pixel)),
        }
    }
}

// --- quads --- //

/// What `fs_quad` gives at a point.
enum QuadFragment {
    /// `blend_color(background_color, 1.0)`: inside the borders and corners.
    Background,
    Color(V4),
}

struct QuadShader {
    bounds: FBounds,
    paint: Paint,
    border_color: V4,
    radii: Radii,
    top: f32,
    right: f32,
    bottom: f32,
    left: f32,
    dashed: bool,
    unrounded: bool,
    /// No border and no rounded corner: every fragment is the background.
    plain: bool,
    premultiplied: bool,
}

const ANTIALIAS_THRESHOLD: f32 = 0.5;

impl QuadShader {
    fn new(quad: &Quad, premultiplied: bool) -> Self {
        let radii = Radii::new(&quad.corner_radii);
        let widths = &quad.border_widths;
        let unrounded = radii.is_zero();
        let plain = widths.top.0 == 0.
            && widths.left.0 == 0.
            && widths.right.0 == 0.
            && widths.bottom.0 == 0.
            && unrounded;
        QuadShader {
            bounds: FBounds::new(&quad.bounds),
            paint: Paint::new(&quad.background),
            border_color: hsla_to_rgba(quad.border_color),
            radii,
            top: widths.top.0,
            right: widths.right.0,
            bottom: widths.bottom.0,
            left: widths.left.0,
            dashed: quad.border_style == BorderStyle::Dashed,
            unrounded,
            plain,
            premultiplied,
        }
    }

    #[inline]
    fn shade(&self, px: f32, py: f32) -> QuadFragment {
        if self.plain {
            return QuadFragment::Background;
        }
        let b = &self.bounds;
        let aa = ANTIALIAS_THRESHOLD;
        let half_w = b.w / 2.0;
        let half_h = b.h / 2.0;
        let point_x = px - b.x;
        let point_y = py - b.y;
        let cx = point_x - half_w;
        let cy = point_y - half_h;

        let corner_radius = pick_corner_radius(cx, cy, &self.radii);
        let border_x = if cx < 0.0 { self.left } else { self.right };
        let border_y = if cy < 0.0 { self.top } else { self.bottom };
        let reduced_x = if border_x == 0.0 { -aa } else { border_x };
        let reduced_y = if border_y == 0.0 { -aa } else { border_y };

        let corner_x = cx.abs() - half_w;
        let corner_y = cy.abs() - half_h;
        let ccx = corner_x + corner_radius;
        let ccy = corner_y + corner_radius;
        let is_near_rounded_corner = ccx >= 0.0 && ccy >= 0.0;

        let sx = corner_x + reduced_x;
        let sy = corner_y + reduced_y;
        let is_beyond_inner_straight_border = sx > 0.0 || sy > 0.0;
        let is_within_inner_straight_border = sx < -aa && sy < -aa;

        if is_within_inner_straight_border && !is_near_rounded_corner {
            return QuadFragment::Background;
        }

        let outer_sdf = quad_sdf_impl(ccx, ccy, corner_radius);
        let inner_sdf = if ccx <= 0.0 || ccy <= 0.0 {
            -sx.max(sy)
        } else if is_beyond_inner_straight_border {
            -1.0
        } else if reduced_x == reduced_y {
            -(outer_sdf + reduced_x)
        } else {
            let rx = (corner_radius - reduced_x).max(0.0);
            let ry = (corner_radius - reduced_y).max(0.0);
            quarter_ellipse_sdf(ccx, ccy, rx, ry)
        };

        let border_sdf = inner_sdf.max(outer_sdf);
        let background = self.paint.color_at(px, py, b);
        let mut color = background;
        if border_sdf < aa {
            let mut border_color = self.border_color;
            if self.dashed {
                border_color[3] *= self.dash_alpha(
                    point_x,
                    point_y,
                    cx,
                    cy,
                    ccx,
                    ccy,
                    corner_radius,
                    is_near_rounded_corner,
                );
            }
            let blended = over(background, border_color);
            let t = saturate(aa - inner_sdf);
            for i in 0..4 {
                color[i] = background[i] * (1.0 - t) + blended[i] * t;
            }
        }
        QuadFragment::Color(blend_color(
            color,
            saturate(aa - outer_sdf),
            self.premultiplied,
        ))
    }

    /// The dashed border's factor on the border color's alpha.
    #[allow(clippy::too_many_arguments)]
    fn dash_alpha(
        &self,
        point_x: f32,
        point_y: f32,
        cx: f32,
        cy: f32,
        ccx: f32,
        ccy: f32,
        corner_radius: f32,
        is_near_rounded_corner: bool,
    ) -> f32 {
        let size_x = self.bounds.w;
        let size_y = self.bounds.h;
        let dash_length_per_width = 2.0f32;
        let dash_gap_per_width = 1.0f32;
        let dash_period_per_width = dash_length_per_width + dash_gap_per_width;
        let dv_numerator = 1.0 / dash_period_per_width;

        let t;
        let mut max_t;
        let dash_velocity;
        if self.unrounded {
            let is_horizontal = ccx < ccy;
            let dashed_border_x = self.bottom.max(self.top);
            let dashed_border_y = self.right.max(self.left);
            let border_width = if is_horizontal {
                dashed_border_x
            } else {
                dashed_border_y
            };
            dash_velocity = dv_numerator / border_width;
            t = if is_horizontal { point_x } else { point_y } * dash_velocity;
            max_t = if is_horizontal { size_x } else { size_y } * dash_velocity;
        } else {
            let r = &self.radii;
            let (r_tr, r_br, r_bl, r_tl) = (r.top_right, r.bottom_right, r.bottom_left, r.top_left);
            let (w_t, w_r, w_b, w_l) = (self.top, self.right, self.bottom, self.left);
            let dv = |w: f32| if w <= 0.0 { 0.0 } else { dv_numerator / w };
            let (dv_t, dv_r, dv_b, dv_l) = (dv(w_t), dv(w_r), dv(w_b), dv(w_l));

            let s_t = (size_x - r_tl - r_tr) * dv_t;
            let s_r = (size_y - r_tr - r_br) * dv_r;
            let s_b = (size_x - r_br - r_bl) * dv_b;
            let s_l = (size_y - r_bl - r_tl) * dv_l;

            let cdv_tr = corner_dash_velocity(dv_t, dv_r);
            let cdv_br = corner_dash_velocity(dv_b, dv_r);
            let cdv_bl = corner_dash_velocity(dv_b, dv_l);
            let cdv_tl = corner_dash_velocity(dv_t, dv_l);

            let c_tr = r_tr * (M_PI_F / 2.0) * cdv_tr;
            let c_br = r_br * (M_PI_F / 2.0) * cdv_br;
            let c_bl = r_bl * (M_PI_F / 2.0) * cdv_bl;
            let c_tl = r_tl * (M_PI_F / 2.0) * cdv_tl;

            let upto_tr = s_t;
            let upto_r = upto_tr + c_tr;
            let upto_br = upto_r + s_r;
            let upto_b = upto_br + c_br;
            let upto_bl = upto_b + s_b;
            let upto_l = upto_bl + c_bl;
            let upto_tl = upto_l + s_l;
            max_t = upto_tl + c_tl;

            if is_near_rounded_corner {
                let radians = ccy.atan2(ccx);
                let corner_t = radians * corner_radius;
                if cx >= 0.0 {
                    if cy < 0.0 {
                        dash_velocity = cdv_tr;
                        t = upto_r - corner_t * dash_velocity;
                    } else {
                        dash_velocity = cdv_br;
                        t = upto_br + corner_t * dash_velocity;
                    }
                } else if cy >= 0.0 {
                    dash_velocity = cdv_bl;
                    t = upto_l - corner_t * dash_velocity;
                } else {
                    dash_velocity = cdv_tl;
                    t = upto_tl + corner_t * dash_velocity;
                }
            } else {
                let is_horizontal = ccx < ccy;
                if is_horizontal {
                    if cy < 0.0 {
                        dash_velocity = dv_t;
                        t = (point_x - r_tl) * dash_velocity;
                    } else {
                        dash_velocity = dv_b;
                        t = upto_bl - (point_x - r_bl) * dash_velocity;
                    }
                } else if cx < 0.0 {
                    dash_velocity = dv_l;
                    t = upto_tl - (point_y - r_tl) * dash_velocity;
                } else {
                    dash_velocity = dv_r;
                    t = upto_r + (point_y - r_tr) * dash_velocity;
                }
            }
        }

        let dash_length = dash_length_per_width / dash_period_per_width;
        if self.unrounded {
            max_t -= dash_length;
        }
        if max_t >= 1.0 {
            let dash_count = max_t.floor();
            let dash_period = max_t / dash_count;
            dash_alpha(
                t,
                dash_period,
                dash_length,
                dash_velocity,
                ANTIALIAS_THRESHOLD,
            )
        } else if self.unrounded {
            let dash_gap = max_t - dash_length;
            if dash_gap > 0.0 {
                let dash_period = dash_length + dash_gap;
                dash_alpha(
                    t,
                    dash_period,
                    dash_length,
                    dash_velocity,
                    ANTIALIAS_THRESHOLD,
                )
            } else {
                1.0
            }
        } else {
            1.0
        }
    }
}

fn corner_dash_velocity(dv1: f32, dv2: f32) -> f32 {
    if dv1 == 0.0 {
        dv2
    } else if dv2 == 0.0 {
        dv1
    } else {
        dv1.min(dv2)
    }
}

fn dash_alpha(
    t: f32,
    period: f32,
    length: f32,
    dash_velocity: f32,
    antialias_threshold: f32,
) -> f32 {
    let half_period = period / 2.0;
    let half_length = length / 2.0;
    let centered = fmod(t + half_period - half_length, period) - half_period;
    let signed_distance = centered.abs() - half_length;
    saturate(antialias_threshold - signed_distance / dash_velocity)
}

pub(super) fn quad(quad: &Quad, ctx: &Ctx, target: &mut Target) {
    let b = FBounds::new(&quad.bounds);
    let rect = fragments(
        b.x,
        b.y,
        b.x + b.w,
        b.y + b.h,
        &FBounds::new(&quad.content_mask.bounds),
        &target.clip,
    );
    if rect.is_empty() {
        return;
    }
    let premultiplied = ctx.params.premultiplied_alpha;
    let blend = Blend::for_target(premultiplied);
    let shader = QuadShader::new(quad, premultiplied);
    let solid = shader.paint.is_solid().then(|| {
        Fill::new(
            blend,
            blend_color(shader.paint.solid(), 1.0, premultiplied),
            ctx.bits,
        )
    });

    if shader.plain
        && let Some(fill) = &solid
    {
        for y in rect.y0..rect.y1 {
            fill.row(target.row(y, rect.x0, rect.x1));
        }
        return;
    }

    for y in rect.y0..rect.y1 {
        let py = y as f32 + 0.5;
        let row = target.row(y, rect.x0, rect.x1);
        for (x, pixel) in (rect.x0..).zip(row.iter_mut()) {
            let px = x as f32 + 0.5;
            match shader.shade(px, py) {
                QuadFragment::Background => match &solid {
                    Some(fill) => fill.apply(pixel),
                    None => {
                        let color = shader.paint.color_at(px, py, &shader.bounds);
                        let src = blend_color(color, 1.0, premultiplied);
                        blend.blend(pixel, src, ctx.bits);
                    }
                },
                QuadFragment::Color(src) => {
                    blend.blend(pixel, src, ctx.bits);
                }
            }
        }
    }
}

// --- shadows --- //

pub(super) fn shadow(shadow: &Shadow, ctx: &Ctx, target: &mut Target) {
    let inset = shadow.inset != 0;
    let blur = shadow.blur_radius.0;
    let geometry = if inset {
        FBounds::new(&shadow.element_bounds)
    } else {
        let margin = 3.0 * blur;
        let b = FBounds::new(&shadow.bounds);
        FBounds {
            x: b.x - margin,
            y: b.y - margin,
            w: b.w + 2.0 * margin,
            h: b.h + 2.0 * margin,
        }
    };
    let rect = fragments(
        geometry.x,
        geometry.y,
        geometry.x + geometry.w,
        geometry.y + geometry.h,
        &FBounds::new(&shadow.content_mask.bounds),
        &target.clip,
    );
    if rect.is_empty() {
        return;
    }

    let premultiplied = ctx.params.premultiplied_alpha;
    let blend = Blend::for_target(premultiplied);
    let color = hsla_to_rgba(shadow.color);
    let bounds = FBounds::new(&shadow.bounds);
    let radii = Radii::new(&shadow.corner_radii);
    let element_bounds = FBounds::new(&shadow.element_bounds);
    let element_radii = Radii::new(&shadow.element_corner_radii);
    let half_w = bounds.w / 2.0;
    let half_h = bounds.h / 2.0;
    let center_x = bounds.x + half_w;
    let center_y = bounds.y + half_h;
    // The row's blurred alpha, worked out sample by sample over the whole
    // row, which vectorizes.
    let mut alphas = vec![0f32; (rect.x1 - rect.x0) as usize];

    for y in rect.y0..rect.y1 {
        let py = y as f32 + 0.5;
        let cy = py - center_y;
        if blur == 0.0 {
            for (x, alpha) in (rect.x0..).zip(alphas.iter_mut()) {
                let distance = quad_sdf(x as f32 + 0.5, py, &bounds, &radii);
                *alpha = saturate(0.5 - distance);
            }
        } else {
            // `fs_shadow`'s four samples along y, and for each the curve
            // `blur_along_x` integrates to on either side of the center,
            // whose corners differ.
            let low = cy - half_h;
            let high = cy + half_h;
            let start = (-3.0 * blur).max(low).min(high);
            let end = (3.0 * blur).max(low).min(high);
            let step = (end - start) / 4.0;
            let k = 0.5f32.sqrt() / blur;
            alphas.fill(0.0);
            let mut sample_y = start + step * 0.5;
            for _ in 0..4 {
                let gaussian = gaussian(sample_y, blur);
                let dy = cy - sample_y;
                let curved_left =
                    blur_curve(dy, pick_corner_radius(-1.0, cy, &radii), half_w, half_h);
                let curved_right =
                    blur_curve(dy, pick_corner_radius(1.0, cy, &radii), half_w, half_h);
                for (x, alpha) in (rect.x0..).zip(alphas.iter_mut()) {
                    let cx = (x as f32 + 0.5) - center_x;
                    let curved = if cx < 0.0 { curved_left } else { curved_right };
                    let integral_low = 0.5 + 0.5 * erf((cx - curved) * k);
                    let integral_high = 0.5 + 0.5 * erf((cx + curved) * k);
                    *alpha += (integral_high - integral_low) * gaussian * step;
                }
                sample_y += step;
            }
        }

        let row = target.row(y, rect.x0, rect.x1);
        for ((x, pixel), &alpha) in (rect.x0..).zip(row.iter_mut()).zip(&alphas) {
            let mut alpha = alpha;
            if inset {
                alpha = 1.0 - alpha;
                let element_distance =
                    quad_sdf(x as f32 + 0.5, py, &element_bounds, &element_radii);
                alpha *= saturate(0.5 - element_distance);
            }
            let src = blend_color(color, alpha, premultiplied);
            blend.blend(pixel, src, ctx.bits);
        }
    }
}

// --- underlines --- //

pub(super) fn underline(underline: &Underline, ctx: &Ctx, target: &mut Target) {
    let b = FBounds::new(&underline.bounds);
    let rect = fragments(
        b.x,
        b.y,
        b.x + b.w,
        b.y + b.h,
        &FBounds::new(&underline.content_mask.bounds),
        &target.clip,
    );
    if rect.is_empty() {
        return;
    }
    let premultiplied = ctx.params.premultiplied_alpha;
    let blend = Blend::for_target(premultiplied);
    let color = hsla_to_rgba(underline.color);

    if underline.wavy == false.into() {
        let fill = Fill::new(blend, blend_color(color, color[3], premultiplied), ctx.bits);
        for y in rect.y0..rect.y1 {
            fill.row(target.row(y, rect.x0, rect.x1));
        }
        return;
    }

    const WAVE_FREQUENCY: f32 = 2.0;
    const WAVE_HEIGHT_RATIO: f32 = 0.8;
    let thickness = underline.thickness.0;
    let half_thickness = thickness * 0.5;
    let frequency = M_PI_F * WAVE_FREQUENCY * thickness / b.h;
    let amplitude = (thickness * WAVE_HEIGHT_RATIO) / b.h;
    for y in rect.y0..rect.y1 {
        let py = y as f32 + 0.5;
        let st_y = (py - b.y) / b.h - 0.5;
        let row = target.row(y, rect.x0, rect.x1);
        for (x, pixel) in (rect.x0..).zip(row.iter_mut()) {
            let px = x as f32 + 0.5;
            let st_x = (px - b.x) / b.h;
            let sine = (st_x * frequency).sin() * amplitude;
            let d_sine = (st_x * frequency).cos() * amplitude * frequency;
            let distance = (st_y - sine) / (1.0 + d_sine * d_sine).sqrt();
            let distance_in_pixels = distance * b.h;
            let distance_from_top_border = distance_in_pixels - half_thickness;
            let distance_from_bottom_border = distance_in_pixels + half_thickness;
            let alpha =
                saturate(0.5 - (-distance_from_bottom_border).max(distance_from_top_border));
            let src = blend_color(color, alpha * color[3], premultiplied);
            blend.blend(pixel, src, ctx.bits);
        }
    }
}

// --- sprites --- //

/// An atlas texture as uploaded.
pub(super) struct Texture<'a> {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    data: &'a [u8],
}

impl<'a> Texture<'a> {
    pub(super) fn new(pixels: Option<TexturePixels<'a>>) -> Option<Self> {
        let pixels = pixels?;
        let needed =
            pixels.width as usize * pixels.height as usize * pixels.bytes_per_pixel as usize;
        if pixels.width == 0
            || pixels.height == 0
            || !matches!(pixels.bytes_per_pixel, 1 | 4)
            || pixels.data.len() < needed
        {
            return None;
        }
        Some(Texture {
            width: pixels.width,
            height: pixels.height,
            bytes_per_pixel: pixels.bytes_per_pixel,
            data: pixels.data,
        })
    }

    /// The texel at (`x`, `y`) as the shaders sample it: an `R8` texel is
    /// (r, 0, 0, 1), a `BGRA8` one its channels.
    #[inline]
    fn texel(&self, x: u32, y: u32) -> V4 {
        let index = (y as usize * self.width as usize + x as usize) * self.bytes_per_pixel as usize;
        if self.bytes_per_pixel == 1 {
            [UNORM8[self.data[index] as usize], 0., 0., 1.]
        } else {
            let d = &self.data[index..index + 4];
            [
                UNORM8[d[2] as usize],
                UNORM8[d[1] as usize],
                UNORM8[d[0] as usize],
                UNORM8[d[3] as usize],
            ]
        }
    }
}

/// Where a polychrome sprite's texels come from.
pub(super) enum Sampler<'a> {
    Atlas(&'a Texture<'a>),
    /// A layer tile's texels drawn for the sprite: `pixels` holds rows of
    /// `stride` of them, the first at texel (`x0`, `y0`) of the tile's
    /// `size` × `size` texture.
    Tile {
        pixels: &'a [u32],
        stride: usize,
        /// The texels `pixels` holds; others are never read.
        texels: IRect,
        size: u32,
    },
}

impl Sampler<'_> {
    fn size(&self) -> (u32, u32) {
        match self {
            Sampler::Atlas(texture) => (texture.width, texture.height),
            Sampler::Tile { size, .. } => (*size, *size),
        }
    }

    #[inline]
    fn texel(&self, x: u32, y: u32) -> V4 {
        match self {
            Sampler::Atlas(texture) => texture.texel(x, y),
            Sampler::Tile {
                pixels,
                stride,
                texels,
                ..
            } => {
                let x = (x as i32).clamp(texels.x0, texels.x1 - 1) - texels.x0;
                let y = (y as i32).clamp(texels.y0, texels.y1 - 1) - texels.y0;
                unpack(pixels[y as usize * stride + x as usize])
            }
        }
    }

    /// The texture sampled at texel coordinates (`u`, `v`) (not normalized)
    /// with the atlas sampler: bilinear, clamped to the edges.
    #[inline]
    fn bilinear(&self, u: f32, v: f32) -> V4 {
        let (width, height) = self.size();
        let (x0, x1, fx) = filter_taps(u, width);
        let (y0, y1, fy) = filter_taps(v, height);
        // Each texel's weight in 1/256ths, rounded half to even.
        let weight = |a: u32, b: u32| {
            let product = a * b;
            let (quotient, remainder) = (product >> 8, product & 0xff);
            let up = remainder > 128 || (remainder == 128 && quotient & 1 == 1);
            (quotient + up as u32) as f32
        };
        let w00 = weight(256 - fx, 256 - fy);
        let w10 = weight(fx, 256 - fy);
        let w01 = weight(256 - fx, fy);
        let w11 = weight(fx, fy);
        let t00 = self.texel(x0, y0);
        let t10 = self.texel(x1, y0);
        let t01 = self.texel(x0, y1);
        let t11 = self.texel(x1, y1);
        let mut out = [0.; 4];
        for i in 0..4 {
            out[i] = (t00[i] * w00 + t10[i] * w10 + t01[i] * w01 + t11[i] * w11) / 256.0;
        }
        out
    }
}

/// The two texels linear filtering reads along an axis of `size` texels at
/// coordinate `u`, clamped to the edge, and the weight of the second in
/// 1/256ths: GPUs filter with 8 bits of fraction, rounded, and weigh each
/// texel by the product of its fractions, rounded to 8 bits too (which
/// reproduces 98% of NVIDIA's filtered levels exactly, the rest a level
/// apart).
#[inline]
fn filter_taps(u: f32, size: u32) -> (u32, u32, u32) {
    let x = u - 0.5;
    let floor = x.floor();
    let fraction = ((x - floor) * 256.0 + 0.5) as u32;
    let last = size as i64 - 1;
    let first = (floor as i64).clamp(0, last);
    let second = (floor as i64 + 1).clamp(0, last);
    (first as u32, second as u32, fraction.min(256))
}

/// The corners of a sprite's quad after its transformation, as
/// `to_device_position_transformed` places them: (0, 0), (1, 0), (0, 1) and
/// (1, 1) of the unit square.
pub(super) fn transformed_corners(
    bounds: &Bounds<ScaledPixels>,
    transformation: &TransformationMatrix,
) -> [(f32, f32); 4] {
    let b = FBounds::new(bounds);
    let m = &transformation.rotation_scale;
    let t = &transformation.translation;
    let corner = |ux: f32, uy: f32| {
        let px = ux * b.w + b.x;
        let py = uy * b.h + b.y;
        (
            m[0][0] * px + m[0][1] * py + t[0],
            m[1][0] * px + m[1][1] * py + t[1],
        )
    };
    [
        corner(0., 0.),
        corner(1., 0.),
        corner(0., 1.),
        corner(1., 1.),
    ]
}

/// Where a sprite's fragments land and what texels they read.
struct SpriteGeometry {
    /// The pixels fragments can land on.
    rect: IRect,
    shape: SpriteShape,
    /// The tile's texels: `tile.origin + unit * tile.size` maps the unit
    /// square onto them.
    tile_x: f32,
    tile_y: f32,
    tile_w: f32,
    tile_h: f32,
}

enum SpriteShape {
    /// Every pixel of `rect` is a fragment, at `(center - origin) / size` of
    /// the unit square.
    Rect {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// When each fragment reads exactly one texel: the offset from
        /// pixel to texel.
        nearest: Option<(i32, i32)>,
    },
    /// The two triangles of a transformed quad, the mask tested per pixel,
    /// and the map from a point back to the unit square.
    Transformed {
        triangles: [Option<Triangle>; 2],
        mask: FBounds,
        origin: (f32, f32),
        /// The inverse of the matrix whose columns are the unit square's
        /// edges after transformation.
        inverse: [[f32; 2]; 2],
    },
}

impl SpriteGeometry {
    fn new(
        bounds: &Bounds<ScaledPixels>,
        transformation: &TransformationMatrix,
        mask: &Bounds<ScaledPixels>,
        tile: &AtlasTile,
        texture_size: (u32, u32),
        clip: &IRect,
    ) -> Option<Self> {
        let mask = FBounds::new(mask);
        let tile_x = tile.bounds.origin.x.0 as f32;
        let tile_y = tile.bounds.origin.y.0 as f32;
        let tile_w = tile.bounds.size.width.0 as f32;
        let tile_h = tile.bounds.size.height.0 as f32;
        let (rect, shape) = if *transformation == TransformationMatrix::unit() {
            let b = FBounds::new(bounds);
            let rect = fragments(b.x, b.y, b.x + b.w, b.y + b.h, &mask, clip);
            let tile_in_texture = tile.bounds.origin.x.0 >= 0
                && tile.bounds.origin.y.0 >= 0
                && tile.bounds.origin.x.0 + tile.bounds.size.width.0 <= texture_size.0 as i32
                && tile.bounds.origin.y.0 + tile.bounds.size.height.0 <= texture_size.1 as i32;
            let nearest = (b.w == tile_w
                && b.h == tile_h
                && b.x.fract() == 0.
                && b.y.fract() == 0.
                && b.x.abs() < 1e6
                && b.y.abs() < 1e6
                && tile_in_texture)
                .then(|| {
                    (
                        tile.bounds.origin.x.0 - b.x as i32,
                        tile.bounds.origin.y.0 - b.y as i32,
                    )
                });
            (
                rect,
                // Attributes are interpolated between the vertices as the
                // rasterizer snapped them.
                SpriteShape::Rect {
                    x: snap32(b.x),
                    y: snap32(b.y),
                    w: snap32(b.x + b.w) - snap32(b.x),
                    h: snap32(b.y + b.h) - snap32(b.y),
                    nearest,
                },
            )
        } else {
            let [v0, v1, v2, v3] = transformed_corners(bounds, transformation);
            let triangles = [Triangle::new([v0, v1, v2]), Triangle::new([v1, v2, v3])];
            let snapped = |(x, y): (f32, f32)| (snap32(x), snap32(y));
            let (v0, v1, v2) = (snapped(v0), snapped(v1), snapped(v2));
            let (mx0, mx1) = mask_span(mask.x, mask.w);
            let (my0, my1) = mask_span(mask.y, mask.h);
            let mut rect = IRect::EMPTY;
            for triangle in triangles.iter().flatten() {
                rect = rect.union(&triangle.pixels);
            }
            let rect = rect
                .intersect(&IRect::new(mx0, my0, mx1, my1))
                .intersect(clip);
            let (ax, ay) = (v1.0 - v0.0, v1.1 - v0.1);
            let (bx, by) = (v2.0 - v0.0, v2.1 - v0.1);
            let det = ax * by - bx * ay;
            if det == 0.0 || !det.is_finite() {
                return None;
            }
            let inverse = [[by / det, -bx / det], [-ay / det, ax / det]];
            (
                rect,
                SpriteShape::Transformed {
                    triangles,
                    mask,
                    origin: v0,
                    inverse,
                },
            )
        };
        if rect.is_empty() {
            return None;
        }
        Some(SpriteGeometry {
            rect,
            shape,
            tile_x,
            tile_y,
            tile_w,
            tile_h,
        })
    }

    fn nearest(&self) -> Option<(i32, i32)> {
        match self.shape {
            SpriteShape::Rect { nearest, .. } => nearest,
            SpriteShape::Transformed { .. } => None,
        }
    }

    /// Calls `f` with each fragment's pixel, its center, and the texel
    /// coordinates it samples at.
    #[inline]
    fn for_each(&self, target: &mut Target, mut f: impl FnMut(&mut u32, f32, f32, f32, f32)) {
        let rect = self.rect;
        match &self.shape {
            SpriteShape::Rect { x, y, w, h, .. } => {
                for py_index in rect.y0..rect.y1 {
                    let py = py_index as f32 + 0.5;
                    let v = self.tile_y + ((py - y) / h) * self.tile_h;
                    let row = target.row(py_index, rect.x0, rect.x1);
                    for (px_index, pixel) in (rect.x0..).zip(row.iter_mut()) {
                        let px = px_index as f32 + 0.5;
                        let u = self.tile_x + ((px - x) / w) * self.tile_w;
                        f(pixel, px, py, u, v);
                    }
                }
            }
            SpriteShape::Transformed {
                triangles,
                mask,
                origin,
                inverse,
            } => {
                for py_index in rect.y0..rect.y1 {
                    let py = py_index as f32 + 0.5;
                    let row = target.row(py_index, rect.x0, rect.x1);
                    for (px_index, pixel) in (rect.x0..).zip(row.iter_mut()) {
                        let covered = triangles
                            .iter()
                            .flatten()
                            .any(|triangle| triangle.covers_center(px_index, py_index));
                        let px = px_index as f32 + 0.5;
                        if !covered || !mask.clip_contains(px, py) {
                            continue;
                        }
                        let dx = px - origin.0;
                        let dy = py - origin.1;
                        let unit_x = inverse[0][0] * dx + inverse[0][1] * dy;
                        let unit_y = inverse[1][0] * dx + inverse[1][1] * dy;
                        let u = self.tile_x + unit_x * self.tile_w;
                        let v = self.tile_y + unit_y * self.tile_h;
                        f(pixel, px, py, u, v);
                    }
                }
            }
        }
    }
}

/// A monochrome sprite, or a subpixel sprite drawn as one.
pub(super) struct MonochromeSource<'a> {
    bounds: &'a Bounds<ScaledPixels>,
    content_mask: &'a Bounds<ScaledPixels>,
    color: Hsla,
    tile: &'a AtlasTile,
    transformation: &'a TransformationMatrix,
}

impl<'a> MonochromeSource<'a> {
    pub(super) fn new(sprite: &'a MonochromeSprite) -> Self {
        MonochromeSource {
            bounds: &sprite.bounds,
            content_mask: &sprite.content_mask.bounds,
            color: sprite.color,
            tile: &sprite.tile,
            transformation: &sprite.transformation,
        }
    }

    pub(super) fn from_subpixel(sprite: &'a SubpixelSprite) -> Self {
        MonochromeSource {
            bounds: &sprite.bounds,
            content_mask: &sprite.content_mask.bounds,
            color: sprite.color,
            tile: &sprite.tile,
            transformation: &sprite.transformation,
        }
    }
}

/// The fragments of glyphs of one color by the level they sample, worked
/// out as they are needed: text runs share their color.
pub(super) struct GlyphCache {
    color: Option<[u32; 4]>,
    known: [bool; 256],
    /// The fragment's levels, alpha 0 when it blends nothing.
    fragments: [[u32; 4]; 256],
}

impl Default for GlyphCache {
    fn default() -> Self {
        GlyphCache {
            color: None,
            known: [false; 256],
            fragments: [[0; 4]; 256],
        }
    }
}

impl GlyphCache {
    fn select(&mut self, color: Hsla) {
        let key = [
            color.h.to_bits(),
            color.s.to_bits(),
            color.l.to_bits(),
            color.a.to_bits(),
        ];
        if self.color != Some(key) {
            self.color = Some(key);
            self.known = [false; 256];
        }
    }
}

/// `fs_mono_sprite`.
pub(super) fn monochrome(
    sprite: MonochromeSource,
    texture: &Texture,
    ctx: &Ctx,
    target: &mut Target,
    glyphs: &mut GlyphCache,
) {
    let Some(geometry) = SpriteGeometry::new(
        sprite.bounds,
        sprite.transformation,
        sprite.content_mask,
        sprite.tile,
        (texture.width, texture.height),
        &target.clip,
    ) else {
        return;
    };
    let params = ctx.params;
    let premultiplied = params.premultiplied_alpha;
    let blend = Blend::for_target(premultiplied);
    let color = hsla_to_rgba(sprite.color);
    let correction = GrayscaleCorrection::new(
        [color[0], color[1], color[2]],
        params.grayscale_enhanced_contrast,
        params.gamma_ratios,
    );
    let fragment = |sample: f32| blend_color(color, correction.apply(sample), premultiplied);

    if let Some((dx, dy)) = geometry.nearest() {
        glyphs.select(sprite.color);
        let rect = geometry.rect;
        let bytes_per_pixel = texture.bytes_per_pixel as usize;
        let red = if bytes_per_pixel == 1 { 0 } else { 2 };
        let width = (rect.x1 - rect.x0) as usize;
        for y in rect.y0..rect.y1 {
            let start = ((y + dy) as usize * texture.width as usize + (rect.x0 + dx) as usize)
                * bytes_per_pixel
                + red;
            let texels = &texture.data[start..start + (width - 1) * bytes_per_pixel + 1];
            let row = target.row(y, rect.x0, rect.x1);
            for (pixel, &level) in row.iter_mut().zip(texels.iter().step_by(bytes_per_pixel)) {
                let level = level as usize;
                // A zero sample corrects to zero alpha, which blends nothing.
                if level == 0 {
                    continue;
                }
                if !glyphs.known[level] {
                    let levels = ctx.bits.levels(fragment(UNORM8[level]));
                    glyphs.fragments[level] = if blend.is_noop_levels(levels) {
                        [0; 4]
                    } else {
                        levels
                    };
                    glyphs.known[level] = true;
                }
                let levels = glyphs.fragments[level];
                if levels[3] != 0 || levels[..3] != [0, 0, 0] {
                    *pixel = blend.apply_levels(*pixel, levels);
                }
            }
        }
        return;
    }

    let sampler = Sampler::Atlas(texture);
    geometry.for_each(target, |pixel, _, _, u, v| {
        let sample = sampler.bilinear(u, v)[0];
        let src = fragment(sample);
        blend.blend(pixel, src, ctx.bits);
    });
}

/// `fs_subpixel_sprite`, blended as its dual-source pipeline blends.
pub(super) fn subpixel(sprite: &SubpixelSprite, texture: &Texture, ctx: &Ctx, target: &mut Target) {
    let Some(geometry) = SpriteGeometry::new(
        &sprite.bounds,
        &sprite.transformation,
        &sprite.content_mask.bounds,
        &sprite.tile,
        (texture.width, texture.height),
        &target.clip,
    ) else {
        return;
    };
    let params = ctx.params;
    let color = hsla_to_rgba(sprite.color);
    let foreground = [color[0], color[1], color[2]];
    let correction = SubpixelCorrection::new(
        foreground,
        params.subpixel_enhanced_contrast,
        params.gamma_ratios,
    );
    let is_bgr = params.is_bgr;
    let shade = |sample: V4| {
        let mut rgb = [sample[0], sample[1], sample[2]];
        if is_bgr {
            rgb.swap(0, 2);
        }
        let corrected = correction.apply(rgb);
        corrected.map(|alpha| color[3] * alpha)
    };

    if let Some((dx, dy)) = geometry.nearest() {
        let rect = geometry.rect;
        for y in rect.y0..rect.y1 {
            let ty = (y + dy) as u32;
            let row = target.row(y, rect.x0, rect.x1);
            for (x, pixel) in (rect.x0..).zip(row.iter_mut()) {
                let sample = texture.texel((x + dx) as u32, ty);
                *pixel = blend_subpixel(*pixel, foreground, shade(sample), ctx.bits);
            }
        }
        return;
    }

    let sampler = Sampler::Atlas(texture);
    geometry.for_each(target, |pixel, _, _, u, v| {
        let sample = sampler.bilinear(u, v);
        *pixel = blend_subpixel(*pixel, foreground, shade(sample), ctx.bits);
    });
}

/// `fs_poly_sprite`, sampling `sampler`.
pub(super) fn polychrome(
    sprite: &PolychromeSprite,
    sampler: &Sampler,
    ctx: &Ctx,
    target: &mut Target,
) {
    let Some(geometry) = SpriteGeometry::new(
        &sprite.bounds,
        &TransformationMatrix::unit(),
        &sprite.content_mask.bounds,
        &sprite.tile,
        sampler.size(),
        &target.clip,
    ) else {
        return;
    };
    let premultiplied = ctx.params.premultiplied_alpha;
    let blend = Blend::for_target(premultiplied);
    let bounds = FBounds::new(&sprite.bounds);
    let radii = Radii::new(&sprite.corner_radii);
    let is_grayscale = sprite.grayscale == true.into();
    let opacity = sprite.opacity;
    let shade = |sample: V4, px: f32, py: f32| {
        let distance = quad_sdf(px, py, &bounds, &radii);
        let mut color = sample;
        if is_grayscale {
            let gray = grayscale(color);
            color = [gray, gray, gray, sample[3]];
        }
        blend_color(color, opacity * saturate(0.5 - distance), premultiplied)
    };

    if let Some((dx, dy)) = geometry.nearest() {
        let rect = geometry.rect;
        for y in rect.y0..rect.y1 {
            let py = y as f32 + 0.5;
            let ty = (y + dy) as u32;
            let row = target.row(y, rect.x0, rect.x1);
            for (x, pixel) in (rect.x0..).zip(row.iter_mut()) {
                let sample = sampler.texel((x + dx) as u32, ty);
                let src = shade(sample, x as f32 + 0.5, py);
                blend.blend(pixel, src, ctx.bits);
            }
        }
        return;
    }

    geometry.for_each(target, |pixel, px, py, u, v| {
        let src = shade(sampler.bilinear(u, v), px, py);
        blend.blend(pixel, src, ctx.bits);
    });
}

// --- scroll layer tiles --- //

/// A scroll layer tile sprite: the texels its fragments sample are drawn
/// from the tile's content over the layer's background, as the GPU renderer
/// rasterizes the tile's texture, and then sampled as `fs_poly_sprite`
/// samples that texture.
pub(super) fn layer_tile(
    sprite: &PolychromeSprite,
    tile: &TilePlan,
    ctx: &Ctx,
    target: &mut Target,
) {
    let size = tile.size;
    if size == 0 {
        return;
    }
    let Some(geometry) = SpriteGeometry::new(
        &sprite.bounds,
        &TransformationMatrix::unit(),
        &sprite.content_mask.bounds,
        &sprite.tile,
        (size, size),
        &target.clip,
    ) else {
        return;
    };

    // The texels the fragments read, one more on every side.
    let rect = geometry.rect;
    let bounds = FBounds::new(&sprite.bounds);
    let (x0, x1) = match geometry.nearest() {
        Some((dx, _)) => (rect.x0 + dx, rect.x1 + dx),
        None => {
            let u = |x: i32| {
                geometry.tile_x + ((x as f32 + 0.5 - bounds.x) / bounds.w) * geometry.tile_w
            };
            let (a, b) = (u(rect.x0), u(rect.x1 - 1));
            (
                (a.min(b) - 0.5).floor() as i32 - 1,
                (a.max(b) - 0.5).floor() as i32 + 3,
            )
        }
    };
    let (y0, y1) = match geometry.nearest() {
        Some((_, dy)) => (rect.y0 + dy, rect.y1 + dy),
        None => {
            let v = |y: i32| {
                geometry.tile_y + ((y as f32 + 0.5 - bounds.y) / bounds.h) * geometry.tile_h
            };
            let (a, b) = (v(rect.y0), v(rect.y1 - 1));
            (
                (a.min(b) - 0.5).floor() as i32 - 1,
                (a.max(b) - 0.5).floor() as i32 + 3,
            )
        }
    };
    let texels = IRect::new(x0, y0, x1, y1).intersect(&IRect::new(0, 0, size as i32, size as i32));
    if texels.is_empty() {
        return;
    }

    let stride = (texels.x1 - texels.x0) as usize;
    let background = tile.background;
    let mut pixels = vec![
        clear_pixel([background.r, background.g, background.b, background.a]);
        stride * (texels.y1 - texels.y0) as usize
    ];
    {
        let mut tile_target = Target {
            pixels: &mut pixels,
            stride,
            origin_x: texels.x0,
            origin_y: texels.y0,
            clip: texels,
        };
        let tile_ctx = Ctx {
            params: ctx.params,
            bits: ctx.bits,
            sprites: ctx.sprites,
            tiles: &[],
        };
        draw_plan(&tile.plan, &tile_ctx, &mut tile_target);
    }
    polychrome(
        sprite,
        &Sampler::Tile {
            pixels: &pixels,
            stride,
            texels,
            size,
        },
        ctx,
        target,
    );
}
