//! The functions `shaders.wgsl` shares between its fragment shaders, ported
//! operation for operation in `f32`, and the blend states of the pipelines,
//! applied to 8-bit pixels as the GPU applies them to its `Bgra8Unorm`
//! target.

use gpui::{Background, Bounds, Corners, Hsla, LinearColorStop, ScaledPixels};

/// A color or a premultiplied color: red, green, blue, alpha.
pub(super) type V4 = [f32; 4];

/// The shaders' π, which is not `f32::consts::PI`.
#[allow(clippy::approx_constant)]
pub(super) const M_PI_F: f32 = 3.1415926;
const GRAYSCALE_FACTORS: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// A rectangle in `f32`, as the shaders' `Bounds`.
#[derive(Clone, Copy, Debug)]
pub(super) struct FBounds {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
}

impl FBounds {
    pub(super) fn new(bounds: &Bounds<ScaledPixels>) -> Self {
        FBounds {
            x: bounds.origin.x.0,
            y: bounds.origin.y.0,
            w: bounds.size.width.0,
            h: bounds.size.height.0,
        }
    }

    /// Whether the shaders' clip distances to `self` are all non-negative
    /// at (`x`, `y`): `distance_from_clip_rect_impl`.
    #[inline]
    pub(super) fn clip_contains(&self, x: f32, y: f32) -> bool {
        x - self.x >= 0.
            && (self.x + self.w) - x >= 0.
            && y - self.y >= 0.
            && (self.y + self.h) - y >= 0.
    }
}

/// The corner radii in the shaders' order.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Radii {
    pub(super) top_left: f32,
    pub(super) top_right: f32,
    pub(super) bottom_right: f32,
    pub(super) bottom_left: f32,
}

impl Radii {
    pub(super) fn new(corners: &Corners<ScaledPixels>) -> Self {
        Radii {
            top_left: corners.top_left.0,
            top_right: corners.top_right.0,
            bottom_right: corners.bottom_right.0,
            bottom_left: corners.bottom_left.0,
        }
    }

    pub(super) fn is_zero(&self) -> bool {
        self.top_left == 0.
            && self.top_right == 0.
            && self.bottom_right == 0.
            && self.bottom_left == 0.
    }
}

/// WGSL's `%` on floats, `a - b * trunc(a / b)`: the remainder with the
/// sign of `a`. The GPU fuses the multiply and the subtraction, which `f64`
/// reproduces for `f32` operands.
#[inline]
pub(super) fn fmod(a: f32, b: f32) -> f32 {
    (a as f64 - b as f64 * (a / b).trunc() as f64) as f32
}

/// WGSL's `saturate`, which takes NaN to 0 where `f32::clamp` keeps it.
#[inline]
#[allow(clippy::manual_clamp)]
pub(super) fn saturate(x: f32) -> f32 {
    x.max(0.).min(1.)
}

/// WGSL's `clamp`, which unlike `f32::clamp` accepts `low > high`.
#[inline]
pub(super) fn clamp(x: f32, low: f32, high: f32) -> f32 {
    x.max(low).min(high)
}

#[inline]
fn length(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

/// WGSL's `mix`.
#[inline]
fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1. - t) + b * t
}

#[inline]
fn mix4(a: V4, b: V4, t: f32) -> V4 {
    [
        mix(a[0], b[0], t),
        mix(a[1], b[1], t),
        mix(a[2], b[2], t),
        mix(a[3], b[3], t),
    ]
}

/// `hsla_to_rgba`.
pub(super) fn hsla_to_rgba(hsla: Hsla) -> V4 {
    let h = hsla.h * 6.0;
    let s = hsla.s;
    let l = hsla.l;
    let a = hsla.a;

    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (fmod(h, 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let mut color = [m, m, m];

    if h >= 0.0 && h < 1.0 {
        color[0] += c;
        color[1] += x;
    } else if h >= 1.0 && h < 2.0 {
        color[0] += x;
        color[1] += c;
    } else if h >= 2.0 && h < 3.0 {
        color[1] += c;
        color[2] += x;
    } else if h >= 3.0 && h < 4.0 {
        color[1] += x;
        color[2] += c;
    } else if h >= 4.0 && h < 5.0 {
        color[0] += x;
        color[2] += c;
    } else {
        color[0] += c;
        color[2] += x;
    }

    [color[0], color[1], color[2], a]
}

fn srgb_to_linear(srgb: f32) -> f32 {
    if srgb < 0.04045 {
        srgb / 12.92
    } else {
        ((srgb + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(linear: f32) -> f32 {
    if linear < 0.0031308 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn linear_to_srgba(color: V4) -> V4 {
    [
        linear_to_srgb(color[0]),
        linear_to_srgb(color[1]),
        linear_to_srgb(color[2]),
        color[3],
    ]
}

fn srgba_to_linear(color: V4) -> V4 {
    [
        srgb_to_linear(color[0]),
        srgb_to_linear(color[1]),
        srgb_to_linear(color[2]),
        color[3],
    ]
}

fn linear_srgb_to_oklab(color: V4) -> V4 {
    let [r, g, b, a] = color;
    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

    let l_ = l.powf(1.0 / 3.0);
    let m_ = m.powf(1.0 / 3.0);
    let s_ = s.powf(1.0 / 3.0);

    [
        0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
        1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
        0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
        a,
    ]
}

fn oklab_to_linear_srgb(color: V4) -> V4 {
    let [r, g, b, a] = color;
    let l_ = r + 0.3963377774 * g + 0.2158037573 * b;
    let m_ = r - 0.1055613458 * g - 0.0638541728 * b;
    let s_ = r - 0.0894841775 * g - 1.2914855480 * b;

    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;

    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
        a,
    ]
}

/// `over`.
pub(super) fn over(below: V4, above: V4) -> V4 {
    let alpha = above[3] + below[3] * (1.0 - above[3]);
    let channel = |i: usize| (above[i] * above[3] + below[i] * below[3] * (1.0 - above[3])) / alpha;
    [channel(0), channel(1), channel(2), alpha]
}

/// `blend_color`: the color a fragment shader returns for the frame's
/// alpha mode.
#[inline]
pub(super) fn blend_color(color: V4, alpha_factor: f32, premultiplied: bool) -> V4 {
    let alpha = color[3] * alpha_factor;
    let multiplier = if premultiplied { alpha } else { 1.0 };
    [
        color[0] * multiplier,
        color[1] * multiplier,
        color[2] * multiplier,
        alpha,
    ]
}

/// `pick_corner_radius`.
#[inline]
pub(super) fn pick_corner_radius(cx: f32, cy: f32, radii: &Radii) -> f32 {
    if cx < 0.0 {
        if cy < 0.0 {
            radii.top_left
        } else {
            radii.bottom_left
        }
    } else if cy < 0.0 {
        radii.top_right
    } else {
        radii.bottom_right
    }
}

/// `quad_sdf_impl`.
#[inline]
pub(super) fn quad_sdf_impl(x: f32, y: f32, corner_radius: f32) -> f32 {
    if corner_radius == 0.0 {
        x.max(y)
    } else {
        let signed_distance_to_inset_quad = length(x.max(0.), y.max(0.)) + x.max(y).min(0.);
        signed_distance_to_inset_quad - corner_radius
    }
}

/// `quad_sdf`.
#[inline]
pub(super) fn quad_sdf(px: f32, py: f32, bounds: &FBounds, radii: &Radii) -> f32 {
    let half_w = bounds.w / 2.0;
    let half_h = bounds.h / 2.0;
    let cx = px - (bounds.x + half_w);
    let cy = py - (bounds.y + half_h);
    let corner_radius = pick_corner_radius(cx, cy, radii);
    let corner_x = cx.abs() - half_w;
    let corner_y = cy.abs() - half_h;
    quad_sdf_impl(
        corner_x + corner_radius,
        corner_y + corner_radius,
        corner_radius,
    )
}

/// `quarter_ellipse_sdf`.
pub(super) fn quarter_ellipse_sdf(px: f32, py: f32, rx: f32, ry: f32) -> f32 {
    let unit_circle_sdf = length(px / rx, py / ry) - 1.0;
    unit_circle_sdf * (rx + ry) * -0.5
}

/// `gaussian`.
pub(super) fn gaussian(x: f32, sigma: f32) -> f32 {
    (-(x * x) / (2.0 * sigma * sigma)).exp() / ((2.0 * M_PI_F).sqrt() * sigma)
}

/// `erf`, one component.
#[inline]
pub(super) fn erf(v: f32) -> f32 {
    // WGSL's `sign` is 0 at 0, where this is 1 or -1, but at 0 both give
    // `s - s / 1`, 0: without the branch, the shadow loop vectorizes.
    let s = 1f32.copysign(v);
    let a = v.abs();
    let r1 = 1.0 + (0.278393 + (0.230389 + (0.000972 + 0.078108 * a) * a) * a) * a;
    let r2 = r1 * r1;
    s - s / (r2 * r2)
}

/// The half width `blur_along_x` integrates over at `y` from a shadow's
/// center: its `curved`.
#[inline]
pub(super) fn blur_curve(y: f32, corner: f32, half_w: f32, half_h: f32) -> f32 {
    let delta = (half_h - corner - y.abs()).min(0.0);
    half_w - corner + (corner * corner - delta * delta).max(0.0).sqrt()
}

fn color_brightness(color: [f32; 3]) -> f32 {
    color[0] * 0.30 + color[1] * 0.59 + color[2] * 0.11
}

fn light_on_dark_contrast(enhanced_contrast: f32, color: [f32; 3]) -> f32 {
    let brightness = color_brightness(color);
    let multiplier = saturate(4.0 * (0.75 - brightness));
    enhanced_contrast * multiplier
}

#[inline]
fn enhance_contrast(alpha: f32, k: f32) -> f32 {
    alpha * (k + 1.0) / (alpha * k + 1.0)
}

#[inline]
fn apply_alpha_correction(a: f32, b: f32, g: &[f32; 4]) -> f32 {
    let brightness_adjustment = g[0] * b + g[1];
    let correction = brightness_adjustment * a + (g[2] * b + g[3]);
    a + a * (1.0 - a) * correction
}

/// `apply_contrast_and_gamma_correction`, with the parts that depend only
/// on the color worked out once.
#[derive(Clone, Copy)]
pub(super) struct GrayscaleCorrection {
    contrast: f32,
    brightness: f32,
    gamma_ratios: [f32; 4],
}

impl GrayscaleCorrection {
    pub(super) fn new(
        color: [f32; 3],
        enhanced_contrast_factor: f32,
        gamma_ratios: [f32; 4],
    ) -> Self {
        GrayscaleCorrection {
            contrast: light_on_dark_contrast(enhanced_contrast_factor, color),
            brightness: color_brightness(color),
            gamma_ratios,
        }
    }

    #[inline]
    pub(super) fn apply(&self, sample: f32) -> f32 {
        let contrasted = enhance_contrast(sample, self.contrast);
        apply_alpha_correction(contrasted, self.brightness, &self.gamma_ratios)
    }
}

/// `apply_contrast_and_gamma_correction3`, with the parts that depend only
/// on the color worked out once.
#[derive(Clone, Copy)]
pub(super) struct SubpixelCorrection {
    contrast: f32,
    color: [f32; 3],
    gamma_ratios: [f32; 4],
}

impl SubpixelCorrection {
    pub(super) fn new(
        color: [f32; 3],
        enhanced_contrast_factor: f32,
        gamma_ratios: [f32; 4],
    ) -> Self {
        SubpixelCorrection {
            contrast: light_on_dark_contrast(enhanced_contrast_factor, color),
            color,
            gamma_ratios,
        }
    }

    #[inline]
    pub(super) fn apply(&self, sample: [f32; 3]) -> [f32; 3] {
        let mut out = [0.; 3];
        for i in 0..3 {
            let contrasted = enhance_contrast(sample[i], self.contrast);
            out[i] = apply_alpha_correction(contrasted, self.color[i], &self.gamma_ratios);
        }
        out
    }
}

/// `dot(color.rgb, GRAYSCALE_FACTORS)`.
#[inline]
pub(super) fn grayscale(color: V4) -> f32 {
    color[0] * GRAYSCALE_FACTORS[0]
        + color[1] * GRAYSCALE_FACTORS[1]
        + color[2] * GRAYSCALE_FACTORS[2]
}

/// `Background` as the shaders read it: its fields are private to gpui, and
/// it reaches the GPU as these bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawBackground {
    tag: u32,
    color_space: u32,
    solid: Hsla,
    gradient_angle_or_pattern_height: f32,
    colors: [LinearColorStop; 2],
    pad: u32,
}

const _: () = assert!(size_of::<RawBackground>() == size_of::<Background>());
const _: () = assert!(align_of::<RawBackground>() == align_of::<Background>());

fn raw_background(background: &Background) -> RawBackground {
    // SAFETY: `Background` is `#[repr(C)]` with exactly these fields, two
    // `#[repr(C)]` fieldless enums (C ints) among them, which the renderer
    // uploads as bytes for the shaders to read as `RawBackground`.
    unsafe { std::mem::transmute_copy::<Background, RawBackground>(background) }
}

/// The color of a checkerboard background, for tests.
#[cfg(test)]
pub(super) fn checkerboard_color(background: &Background) -> Option<Hsla> {
    let raw = raw_background(background);
    (raw.tag == 3).then_some(raw.solid)
}

/// A background ready to shade: `prepare_gradient_color`'s results, and
/// what `gradient_color` works out from the background alone.
#[derive(Clone, Copy)]
pub(super) struct Paint {
    tag: u32,
    color_space: u32,
    solid: V4,
    color0: V4,
    color1: V4,
    angle_or_height: f32,
    stop0: f32,
    stop1: f32,
}

impl Paint {
    /// `prepare_gradient_color`.
    pub(super) fn new(background: &Background) -> Self {
        let raw = raw_background(background);
        let mut paint = Paint {
            tag: raw.tag,
            color_space: raw.color_space,
            solid: [0.; 4],
            color0: [0.; 4],
            color1: [0.; 4],
            angle_or_height: raw.gradient_angle_or_pattern_height,
            stop0: raw.colors[0].percentage,
            stop1: raw.colors[1].percentage,
        };
        if raw.tag == 0 || raw.tag == 2 || raw.tag == 3 {
            paint.solid = hsla_to_rgba(raw.solid);
        } else if raw.tag == 1 {
            paint.color0 = hsla_to_rgba(raw.colors[0].color);
            paint.color1 = hsla_to_rgba(raw.colors[1].color);
            if raw.color_space == 0 {
                paint.color0 = linear_to_srgba(paint.color0);
                paint.color1 = linear_to_srgba(paint.color1);
            } else if raw.color_space == 1 {
                paint.color0 = linear_srgb_to_oklab(paint.color0);
                paint.color1 = linear_srgb_to_oklab(paint.color1);
            }
        }
        paint
    }

    /// Whether the color is the same at every position.
    pub(super) fn is_solid(&self) -> bool {
        !matches!(self.tag, 1..=3)
    }

    pub(super) fn solid(&self) -> V4 {
        self.solid
    }

    /// `gradient_color` at (`x`, `y`) of `bounds`.
    pub(super) fn color_at(&self, x: f32, y: f32, bounds: &FBounds) -> V4 {
        match self.tag {
            1 => {
                let angle = self.angle_or_height;
                let radians = (fmod(angle, 360.0) - 90.0) * M_PI_F / 180.0;
                let mut dx = radians.cos();
                let mut dy = radians.sin();
                if bounds.w > bounds.h {
                    dy *= bounds.h / bounds.w;
                } else {
                    dx *= bounds.w / bounds.h;
                }
                let half_w = bounds.w / 2.0;
                let half_h = bounds.h / 2.0;
                let cx = x - (bounds.x + half_w);
                let cy = y - (bounds.y + half_h);
                let mut t = (cx * dx + cy * dy) / length(dx, dy);
                if dx.abs() > dy.abs() {
                    t = (t + half_w) / bounds.w;
                } else {
                    t = (t + half_h) / bounds.h;
                }
                t = (t - self.stop0) / (self.stop1 - self.stop0);
                t = clamp(t, 0.0, 1.0);
                if self.color_space == 1 {
                    oklab_to_linear_srgb(mix4(self.color0, self.color1, t))
                } else {
                    srgba_to_linear(mix4(self.color0, self.color1, t))
                }
            }
            2 => {
                let height = self.angle_or_height;
                let pattern_width = (height / 65535.0) / 255.0;
                let pattern_interval = fmod(height, 65535.0) / 255.0;
                let pattern_height = pattern_width + pattern_interval;
                let stripe_angle = M_PI_F / 4.0;
                let (sin, cos) = stripe_angle.sin_cos();
                let pattern_period = pattern_height * sin;
                let rx = x - bounds.x;
                let ry = y - bounds.y;
                let rotated_x = cos * rx + sin * ry;
                let pattern = fmod(rotated_x, pattern_period);
                let distance = pattern.min(pattern_period - pattern)
                    - pattern_period * (pattern_width / pattern_height) / 2.0;
                let mut color = self.solid;
                color[3] *= saturate(0.5 - distance);
                color
            }
            3 => {
                let size = self.angle_or_height;
                let x_index = ((x - bounds.x) / size).floor();
                let y_index = ((y - bounds.y) / size).floor();
                let should_be_colored = fmod(x_index + y_index, 2.0);
                let mut color = self.solid;
                color[3] *= saturate(should_be_colored);
                color
            }
            _ => self.solid,
        }
    }
}

/// `unorm8` to `f32`, as the GPU reads the target and textures.
pub(super) static UNORM8: [f32; 256] = {
    let mut table = [0f32; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = i as f32 / 255.0;
        i += 1;
    }
    table
};

/// The bits of fixed point the GPU truncates a fragment's channels to
/// (`RasterParams::fragment_bits`), and so the level of the 8-bit target a
/// fragment's channel is blended at.
///
/// The GPU does not blend into an 8-bit target in `f32`: it converts each
/// channel of the fragment to fixed point, truncating, and that to an 8-bit
/// level, rounding; it then blends levels as integers, rounding the sum to
/// the nearest level. NVIDIA truncates to 12 bits: this reproduces 16382 of
/// 16384 blended channels exactly (and the other two a level apart), where
/// blending in `f32` misses one in nine. Intel (Mesa) truncates to 16 bits,
/// reproduced exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FragmentBits {
    Twelve,
    Sixteen,
}

impl FragmentBits {
    pub(super) fn new(bits: u32) -> Self {
        if bits >= 16 {
            FragmentBits::Sixteen
        } else {
            FragmentBits::Twelve
        }
    }

    /// The level of a fragment's channel.
    #[inline]
    pub(super) fn level(self, x: f32) -> u32 {
        match self {
            FragmentBits::Twelve => level_of::<false>(x),
            FragmentBits::Sixteen => level_of::<true>(x),
        }
    }

    /// A fragment's channels as levels: red, green, blue, alpha.
    #[inline]
    pub(super) fn levels(self, src: V4) -> [u32; 4] {
        match self {
            FragmentBits::Twelve => src.map(level_of::<false>),
            FragmentBits::Sixteen => src.map(level_of::<true>),
        }
    }
}

#[inline]
#[allow(clippy::manual_clamp)]
fn level_of<const BITS_16: bool>(x: f32) -> u32 {
    // NaN, which `max` drops, ends as 0.
    let x = x.max(0.).min(1.);
    if BITS_16 {
        let fixed = (x * 65536.0) as u32;
        ((fixed * 255 + 32768) >> 16).min(255)
    } else {
        let fixed = (x * 4096.0) as u32;
        ((fixed * 255 + 2048) >> 12).min(255)
    }
}

/// A pixel's channels as levels: red, green, blue, alpha.
#[inline]
pub(super) fn levels(pixel: u32) -> [u32; 4] {
    [
        pixel >> 16 & 0xff,
        pixel >> 8 & 0xff,
        pixel & 0xff,
        pixel >> 24,
    ]
}

/// A pixel of levels: red, green, blue, alpha.
#[inline]
pub(super) fn pack_levels(levels: [u32; 4]) -> u32 {
    levels[3] << 24 | levels[0] << 16 | levels[1] << 8 | levels[2]
}

/// A pixel's channels as the shaders see them: red, green, blue, alpha.
#[inline]
pub(super) fn unpack(pixel: u32) -> V4 {
    [
        UNORM8[(pixel >> 16 & 0xff) as usize],
        UNORM8[(pixel >> 8 & 0xff) as usize],
        UNORM8[(pixel & 0xff) as usize],
        UNORM8[(pixel >> 24) as usize],
    ]
}

/// A clear color as the target holds it: each channel rounded to the
/// nearest level.
pub(super) fn clear_pixel(color: V4) -> u32 {
    let level = |x: f32| (saturate(x) * 255.0 + 0.5) as u32;
    pack_levels([
        level(color[0]),
        level(color[1]),
        level(color[2]),
        level(color[3]),
    ])
}

/// `a / 255` rounded to the nearest integer, for `a` up to `255 * 510`.
#[inline]
fn div255(a: u32) -> u32 {
    (a + 127) / 255
}

/// The blend states of the pipelines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Blend {
    /// `BlendState::ALPHA_BLENDING`, the opaque window's.
    Alpha,
    /// `BlendState::PREMULTIPLIED_ALPHA_BLENDING`, the transparent window's
    /// and path rasterization's.
    Premultiplied,
    /// The paths pipeline's: color `One`/`OneMinusSrcAlpha`, alpha `One`/`One`.
    Paths,
}

impl Blend {
    pub(super) fn for_target(premultiplied_alpha: bool) -> Self {
        if premultiplied_alpha {
            Blend::Premultiplied
        } else {
            Blend::Alpha
        }
    }

    /// Blends the fragment `src` over the pixel `dst`.
    #[inline]
    pub(super) fn apply(self, dst: u32, src: V4, bits: FragmentBits) -> u32 {
        self.apply_levels(dst, bits.levels(src))
    }

    /// Blends a fragment of levels `s` over the pixel `dst`.
    #[inline]
    pub(super) fn apply_levels(self, dst: u32, s: [u32; 4]) -> u32 {
        let d = levels(dst);
        let a = s[3];
        let inv = 255 - a;
        let out = match self {
            Blend::Alpha => [
                div255(s[0] * a + d[0] * inv),
                div255(s[1] * a + d[1] * inv),
                div255(s[2] * a + d[2] * inv),
                div255(a * 255 + d[3] * inv),
            ],
            Blend::Premultiplied => [
                div255(s[0] * 255 + d[0] * inv).min(255),
                div255(s[1] * 255 + d[1] * inv).min(255),
                div255(s[2] * 255 + d[2] * inv).min(255),
                div255(a * 255 + d[3] * inv),
            ],
            Blend::Paths => [
                div255(s[0] * 255 + d[0] * inv).min(255),
                div255(s[1] * 255 + d[1] * inv).min(255),
                div255(s[2] * 255 + d[2] * inv).min(255),
                (a + d[3]).min(255),
            ],
        };
        pack_levels(out)
    }

    /// Blends the fragment `src` into `pixel`.
    #[inline]
    pub(super) fn blend(self, pixel: &mut u32, src: V4, bits: FragmentBits) {
        let levels = bits.levels(src);
        if !self.is_noop_levels(levels) {
            *pixel = self.apply_levels(*pixel, levels);
        }
    }

    /// Whether blending `src` leaves every pixel as it was.
    #[inline]
    pub(super) fn is_noop(self, src: V4, bits: FragmentBits) -> bool {
        self.is_noop_levels(bits.levels(src))
    }

    #[inline]
    pub(super) fn is_noop_levels(self, s: [u32; 4]) -> bool {
        s[3] == 0
            && match self {
                Blend::Alpha => true,
                Blend::Premultiplied | Blend::Paths => s[0] == 0 && s[1] == 0 && s[2] == 0,
            }
    }
}

/// Blends a subpixel sprite's fragment as its dual-source pipeline does:
/// color `Src1`/`OneMinusSrc1` per channel, and alpha not at all: the
/// pipeline writes color only.
#[inline]
pub(super) fn blend_subpixel(
    dst: u32,
    foreground: [f32; 3],
    alpha: [f32; 3],
    bits: FragmentBits,
) -> u32 {
    let mut out = levels(dst);
    for i in 0..3 {
        let f = bits.level(foreground[i]);
        let a = bits.level(alpha[i]);
        out[i] = div255(f * a + out[i] * (255 - a));
    }
    pack_levels(out)
}
